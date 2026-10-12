use crate::{
    config::{Config, TunCommand},
    core,
};
use anyhow::{Result, bail};
use std::{
    fs,
    time::{Duration, Instant},
};

pub async fn run(mut config: Config, command: &TunCommand) -> Result<()> {
    match command {
        TunCommand::Setup => super::install::setup().await,
        TunCommand::Uninstall => super::install::uninstall(&mut config).await,
        TunCommand::On | TunCommand::Off => {
            if !cfg!(target_os = "linux") {
                bail!(super::toggle::LINUX_ONLY);
            }
            let enabled = matches!(command, TunCommand::On);
            if enabled
                && let Some(reason) = super::toggle::refusal(
                    true,
                    crate::profiles::Profiles::load()?.current_core(),
                    enabled,
                )
            {
                bail!(reason);
            }
            config.tun_enabled = enabled;
            config.save()?;
            if enabled {
                core::request_core_enabled(true)?;
            } else {
                core::request_restart()?;
            }
            let request = super::toggle::Request::new(core::desired_revision(), enabled);
            core::ensure_supervisor(config.auto_start).await?;
            let revision = wait_for_revision(request).await?;
            println!(
                "TUN {} (applied revision {revision})",
                if enabled { "active" } else { "disabled" }
            );
            Ok(())
        }
        TunCommand::Status { check_helper } => {
            let helper = super::protocol::Client::connect().await;
            if *check_helper {
                helper?;
                return Ok(());
            }
            let state = core::supervisor_state();
            let value = match helper {
                Ok(c) => {
                    serde_json::json!({"desired": config.tun_enabled, "active": c.status.tun_active, "supervisor": state, "helper": c.status})
                }
                Err(e) => {
                    serde_json::json!({"desired": config.tun_enabled, "active": false, "supervisor": state, "helper_error": format!("{e:#}")})
                }
            };
            println!("{}", serde_json::to_string_pretty(&value)?);
            Ok(())
        }
        TunCommand::Doctor => doctor(&config).await,
    }
}

pub(crate) fn revision_result(
    state: &core::SupervisorState,
    revision: &str,
    enabled: bool,
) -> Result<bool> {
    if state.applied_revision == revision
        && state.tun_active == enabled
        && (state.running || !enabled)
    {
        return Ok(true);
    }
    if state.attempted_revision == revision
        && let Some(error) = &state.error
    {
        bail!("TUN request failed: {error}");
    }
    Ok(false)
}
/// Waits until the supervisor applied `request`, following the revisions of later changes; returns
/// the revision it applied.
async fn wait_for_revision(request: super::toggle::Request) -> Result<String> {
    let enabled = request.enabled;
    follow_request(
        request,
        look_now,
        |state, revision| revision_result(state, revision, enabled),
        SUPERSEDED,
        REQUEST_INTERVAL,
        super::toggle::REQUEST_TIMEOUT,
    )
    .await
}

/// Waits until the supervisor turned TUN off for `request` and released the helper, following the
/// revisions of later changes that keep TUN off.
pub(super) async fn wait_for_uninstall(
    request: super::toggle::Request,
    has_profiles: bool,
) -> Result<()> {
    follow_request(
        request,
        look_now,
        |state, revision| uninstall_result(state, revision, has_profiles),
        UNINSTALL_SUPERSEDED,
        REQUEST_INTERVAL,
        super::toggle::REQUEST_TIMEOUT,
    )
    .await
    .map(drop)
}

const REQUEST_INTERVAL: Duration = Duration::from_millis(200);
const SUPERSEDED: &str =
    "TUN request superseded: TUN was switched again elsewhere; see `omash tun status`";
const UNINSTALL_SUPERSEDED: &str =
    "TUN helper retained: TUN was switched on again during uninstall";

/// What a waiting command looks at: the revision of the files the supervisor watches, the TUN
/// setting in config.toml, and the supervisor's state, in this order.
struct Look {
    revision: String,
    tun_enabled: Option<bool>,
    state: core::SupervisorState,
}

fn look_now() -> Look {
    Look {
        revision: core::desired_revision(),
        tun_enabled: Config::saved_tun_enabled(),
        state: core::supervisor_state(),
    }
}

/// Any later change of the watched files (a node selection, a profile update) gives the supervisor
/// a newer revision than the request's, which it applies instead; while config.toml keeps the
/// request's TUN setting, the request follows that revision (see `Request::follow`). `judge` says
/// whether the supervisor completed the request at a revision. An unreadable config.toml leaves the
/// setting as requested: the supervisor reports that configuration as the request's failure.
async fn follow_request(
    mut request: super::toggle::Request,
    mut look: impl FnMut() -> Look,
    judge: impl Fn(&core::SupervisorState, &str) -> Result<bool>,
    superseded: &str,
    interval: Duration,
    timeout: Duration,
) -> Result<String> {
    loop {
        let seen = look();
        let previous = request.revision.clone();
        if !request.follow(&seen.revision, seen.tun_enabled.unwrap_or(request.enabled)) {
            bail!("{superseded}");
        }
        if request.revision != previous {
            eprintln!(
                "revision {previous} was superseded by a later change; waiting for {}",
                request.revision
            );
        }
        if judge(&seen.state, &request.revision)? {
            return Ok(request.revision);
        }
        if request.started.elapsed() >= timeout {
            bail!(super::toggle::TIMED_OUT);
        }
        tokio::time::sleep(interval).await;
    }
}

fn uninstall_result(
    state: &core::SupervisorState,
    revision: &str,
    has_profiles: bool,
) -> Result<bool> {
    // An installation without any profiles can be removed after the supervisor
    // acknowledges the request and confirms that it owns no running backend.
    let idle = !has_profiles
        && state.attempted_revision == revision
        && !state.running
        && state.backend.is_none();
    if (state.applied_revision == revision || idle)
        && !state.tun_active
        && state.backend.as_deref() != Some("service")
    {
        return Ok(true);
    }
    if state.attempted_revision == revision
        && let Some(error) = &state.error
    {
        bail!("TUN helper retained: supervisor could not disable TUN: {error}");
    }
    Ok(false)
}

/// The first helper protocol whose helper adds firewall exceptions.
const FIREWALL_PROTOCOL: u32 = 2;
/// The helper retries a failed removal every two seconds.
const FIREWALL_CLEANUP_WAIT: Duration = Duration::from_secs(10);
const CLEANUP_INTERVAL: Duration = Duration::from_millis(500);
/// Bounds one look at the helper, including a Hello that a busy helper answers late and the
/// unit-state query.
const CLEANUP_LOOK: Duration = Duration::from_secs(5);

/// What one look at the helper showed about its firewall exceptions.
#[derive(Clone, Debug)]
enum Cleanup {
    /// The helper answered without a firewall error: none of its exceptions is left.
    Done,
    /// The helper answered that removing its exceptions failed.
    Failed(String),
    /// A helper older than protocol 2, which adds no exceptions.
    Legacy,
    /// The helper unit is inactive or failed: nothing runs that could retry.
    Stopped,
    /// No answer for now: the helper may be busy or restarting.
    Unknown(String),
}

async fn look_at_cleanup() -> Cleanup {
    let error = match super::protocol::Client::helper_status().await {
        Ok(status) => return status.firewall_error.map_or(Cleanup::Done, Cleanup::Failed),
        Err(error) => error,
    };
    if error
        .downcast_ref::<super::protocol::ProtocolMismatch>()
        .is_some_and(|mismatch| mismatch.peer < FIREWALL_PROTOCOL)
    {
        return Cleanup::Legacy;
    }
    if helper_unit_state()
        .await
        .as_deref()
        .is_some_and(stopped_unit)
    {
        return Cleanup::Stopped;
    }
    Cleanup::Unknown(format!("{error:#}"))
}

async fn helper_unit_state() -> Option<String> {
    let output = tokio::process::Command::new("systemctl")
        .args([
            "show",
            "--property=ActiveState",
            "--value",
            "omash-tun.service",
        ])
        // A look that runs out of time is dropped; so is this query.
        .kill_on_drop(true)
        .output()
        .await
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// Only these states mean that no helper process runs and systemd will not start one by itself.
fn stopped_unit(state: &str) -> bool {
    matches!(state, "inactive" | "failed")
}

/// Only the helper retries a failed removal of its firewall exceptions, so uninstall keeps it
/// until it confirms that none is left.
pub(super) async fn wait_for_firewall_cleanup() -> Result<()> {
    wait_for_cleanup(
        look_at_cleanup,
        FIREWALL_CLEANUP_WAIT,
        CLEANUP_INTERVAL,
        CLEANUP_LOOK,
    )
    .await
}

async fn wait_for_cleanup<F, R>(
    mut look: F,
    wait: Duration,
    interval: Duration,
    look_limit: Duration,
) -> Result<()>
where
    F: FnMut() -> R,
    R: std::future::Future<Output = Cleanup>,
{
    let deadline = Instant::now() + wait;
    let mut failure = None;
    let mut unknown = String::new();
    loop {
        // Each whole look ends by its own limit and by the deadline.
        let limit = deadline
            .saturating_duration_since(Instant::now())
            .min(look_limit);
        let observed = tokio::time::timeout(limit, look())
            .await
            .unwrap_or_else(|_| Cleanup::Unknown("the TUN helper did not answer in time".into()));
        match observed {
            Cleanup::Done => return Ok(()),
            // Neither can retry anything; a helper that reported a failure may have left exceptions.
            Cleanup::Legacy | Cleanup::Stopped if failure.is_none() => return Ok(()),
            Cleanup::Legacy | Cleanup::Stopped => {}
            Cleanup::Failed(error) => failure = Some(error),
            Cleanup::Unknown(error) => unknown = error,
        }
        if deadline.saturating_duration_since(Instant::now()) <= interval {
            break;
        }
        tokio::time::sleep(interval).await;
    }
    let kept = "TUN is disabled and the helper is kept; fix the cause (see `omash tun doctor` and `systemctl status omash-tun`), then run `omash tun uninstall` again";
    match failure {
        Some(error) => bail!(
            "TUN helper retained: it could not remove its firewall exceptions: {error}. {kept}"
        ),
        None => bail!(
            "TUN helper retained: cannot confirm that it removed its firewall exceptions: {unknown}. {kept}"
        ),
    }
}

async fn doctor(config: &Config) -> Result<()> {
    println!(
        "omash {} / TUN protocol {}",
        env!("CARGO_PKG_VERSION"),
        super::protocol::VERSION
    );
    println!(
        "desired TUN: {}; IPv6 enabled: {}",
        config.tun_enabled, config.ipv6
    );
    println!("IPv6 off does not block native IPv6 traffic at the OS level.");
    println!(
        "/dev/net/tun: {}",
        std::path::Path::new("/dev/net/tun").exists()
    );
    let state = core::supervisor_state();
    println!(
        "supervisor: running={}, backend={:?}, TUN={}, applied_current={}",
        state.running,
        state.backend,
        state.tun_state,
        state.applied_revision == state.desired_revision
    );
    if let Some(error) = state.error {
        println!("last apply error: {error}");
    }
    match super::protocol::Client::connect().await {
        Ok(c) => {
            println!(
                "helper: version={}, uid={}, running={}, TUN={}, device={:?}",
                c.status.helper_version,
                c.status.uid,
                c.status.running,
                c.status.tun_active,
                c.status.device
            );
            if let Some(error) = &c.status.firewall_error {
                println!("TUN firewall: {error}");
            }
            if let Some(pid) = c.status.pid
                && let Ok(text) = fs::read_to_string(format!("/proc/{pid}/status"))
            {
                for line in text.lines().filter(|line| {
                    ["Uid:", "CapEff:", "CapAmb:", "NoNewPrivs:"]
                        .iter()
                        .any(|p| line.starts_with(p))
                }) {
                    println!("{line}");
                }
            }
        }
        Err(e) => println!("helper: {e:#}"),
    }
    if let Some(hint) = super::firewall::legacy_filter_warning() {
        println!("TUN firewall: {hint}");
    }
    match super::install::trusted_core() {
        Ok(()) => println!("core trust: verified (root-owned, no SUID/file capabilities)"),
        Err(e) => println!("core trust: {e:#}"),
    }
    // Only structural runtime information is printed: never the full config or secret.
    if let Ok(api) = crate::api::MihomoClient::new(&config.controller, config.secret.clone()) {
        if let Ok(version) = api.version().await {
            println!("core version: {}", version.version);
        }
        match api.runtime_config().await {
            Ok(runtime) => println!("controller TUN: {:?}", runtime.tun),
            Err(_) => println!("controller: unavailable"),
        }
    }
    if cfg!(target_os = "linux") {
        for args in [
            vec!["-brief", "link"],
            vec!["-4", "rule", "show"],
            vec!["-6", "rule", "show"],
            vec!["-4", "route", "show", "table", "all"],
            vec!["-6", "route", "show", "table", "all"],
        ] {
            let output = tokio::process::Command::new("/usr/bin/ip")
                .args(&args)
                .output()
                .await?;
            println!(
                "ip {}:\n{}",
                args.join(" "),
                String::from_utf8_lossy(&output.stdout)
            );
        }
        println!(
            "An inactive helper with a remaining TUN device or policy table may indicate a forced-stop residue. No routes were modified by doctor; inspect ownership before removing anything."
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn look(revision: &str, tun_enabled: bool, state: core::SupervisorState) -> Look {
        Look {
            revision: revision.into(),
            tun_enabled: Some(tun_enabled),
            state,
        }
    }

    fn supervisor(applied: &str, attempted: &str, tun: bool) -> core::SupervisorState {
        core::SupervisorState {
            running: true,
            tun_active: tun,
            backend: Some(if tun { "service" } else { "direct" }.into()),
            applied_revision: applied.into(),
            attempted_revision: attempted.into(),
            ..Default::default()
        }
    }

    /// Feeds the looks in order, repeating the last one.
    fn looks(mut seen: Vec<Look>) -> impl FnMut() -> Look {
        seen.reverse();
        move || {
            if seen.len() > 1 {
                seen.pop().unwrap()
            } else {
                let last = &seen[0];
                look(
                    &last.revision,
                    last.tun_enabled.unwrap(),
                    last.state.clone(),
                )
            }
        }
    }

    async fn follow_on(seen: Vec<Look>, enabled: bool) -> Result<String> {
        follow_request(
            crate::tun::toggle::Request::new("r1".into(), enabled),
            looks(seen),
            |state, revision| revision_result(state, revision, enabled),
            SUPERSEDED,
            Duration::from_millis(1),
            Duration::from_millis(50),
        )
        .await
    }

    #[tokio::test]
    async fn tun_on_and_off_follow_a_revision_that_superseded_theirs() {
        // A profile update after `omash tun on`: the supervisor applies r2, never r1.
        let followed = follow_on(
            vec![
                look("r1", true, supervisor("r0", "r0", false)),
                look("r2", true, supervisor("r0", "r0", false)),
                look("r2", true, supervisor("r2", "r2", true)),
            ],
            true,
        )
        .await;
        assert_eq!(followed.unwrap(), "r2");
        // The same for `omash tun off`.
        let off = follow_on(
            vec![look("r2", false, supervisor("r2", "r2", false))],
            false,
        )
        .await;
        assert_eq!(off.unwrap(), "r2");
        // A failure of the followed revision is the request's failure.
        let mut rejected = supervisor("r1", "r2", false);
        rejected.error = Some("core rejected".into());
        let failed = follow_on(vec![look("r2", true, rejected)], true).await;
        assert_eq!(
            failed.unwrap_err().to_string(),
            "TUN request failed: core rejected"
        );
        // Without a result the request times out, the full time after its last revision.
        let pending = follow_on(vec![look("r2", true, supervisor("r0", "r0", false))], true).await;
        assert_eq!(
            pending.unwrap_err().to_string(),
            crate::tun::toggle::TIMED_OUT
        );
    }

    #[tokio::test]
    async fn a_request_switched_again_elsewhere_is_superseded() {
        let switched = follow_on(
            vec![
                look("r1", true, supervisor("r0", "r0", false)),
                look("r2", false, supervisor("r0", "r0", false)),
            ],
            true,
        )
        .await;
        assert_eq!(switched.unwrap_err().to_string(), SUPERSEDED);
        // r2 adopted with a setting read before `omash tun off` ran; the next look ends it.
        let raced = follow_on(
            vec![
                look("r2", true, supervisor("r0", "r0", false)),
                look("r2", false, supervisor("r2", "r2", false)),
            ],
            true,
        )
        .await;
        assert_eq!(raced.unwrap_err().to_string(), SUPERSEDED);
    }

    #[tokio::test]
    async fn uninstall_follows_a_later_revision_but_still_waits_for_the_release() {
        let wait = |seen| {
            follow_request(
                crate::tun::toggle::Request::new("r1".into(), false),
                looks(seen),
                |state, revision| uninstall_result(state, revision, true),
                UNINSTALL_SUPERSEDED,
                Duration::from_millis(1),
                Duration::from_millis(50),
            )
        };
        let mut serving = supervisor("r2", "r2", false);
        serving.backend = Some("service".into());
        // Applied r2, but the helper's backend is still in use: not done yet.
        let released = wait(vec![
            look("r2", false, serving.clone()),
            look("r2", false, supervisor("r2", "r2", false)),
        ])
        .await;
        assert_eq!(released.unwrap(), "r2");
        let never = wait(vec![look("r2", false, serving)]).await;
        assert_eq!(
            never.unwrap_err().to_string(),
            crate::tun::toggle::TIMED_OUT
        );
        let switched_on = wait(vec![look("r2", true, supervisor("r2", "r2", true))]).await;
        assert_eq!(switched_on.unwrap_err().to_string(), UNINSTALL_SUPERSEDED);
        // A failure of the followed revision keeps the helper and reports that failure.
        let mut rejected = supervisor("r0", "r2", true);
        rejected.error = Some("core rejected".into());
        let failed = wait(vec![
            look("r1", false, supervisor("r0", "r0", true)),
            look("r2", false, rejected),
        ])
        .await;
        assert_eq!(
            failed.unwrap_err().to_string(),
            "TUN helper retained: supervisor could not disable TUN: core rejected"
        );
    }

    #[test]
    fn tun_on_reports_applied_revision() {
        let mut state = core::SupervisorState {
            applied_revision: "old".into(),
            running: true,
            tun_active: true,
            ..Default::default()
        };
        assert!(!revision_result(&state, "new", true).unwrap());
        state.applied_revision = "new".into();
        assert!(revision_result(&state, "new", true).unwrap());
    }
    #[test]
    fn failed_revision_is_not_marked_applied() {
        let state = core::SupervisorState {
            attempted_revision: "new".into(),
            applied_revision: "old".into(),
            error: Some("rejected".into()),
            ..Default::default()
        };
        assert!(revision_result(&state, "new", true).is_err());
    }

    #[test]
    fn uninstall_waits_for_the_requested_revision_and_service_release() {
        let mut state = core::SupervisorState {
            attempted_revision: "off".into(),
            applied_revision: "on".into(),
            backend: Some("service".into()),
            running: true,
            tun_active: false,
            ..Default::default()
        };
        assert!(!uninstall_result(&state, "off", true).unwrap());
        // A failed TUN probe is not evidence that the helper has released its core.
        state.applied_revision = "off".into();
        assert!(!uninstall_result(&state, "off", true).unwrap());
        state.backend = Some("direct".into());
        assert!(uninstall_result(&state, "off", true).unwrap());
    }

    #[test]
    fn uninstall_keeps_helper_when_off_rolls_back() {
        let state = core::SupervisorState {
            attempted_revision: "off".into(),
            applied_revision: "on".into(),
            backend: Some("service".into()),
            running: true,
            tun_active: true,
            error: Some("direct core failed; old TUN restored".into()),
            ..Default::default()
        };
        assert!(uninstall_result(&state, "off", true).is_err());
    }

    #[test]
    fn uninstall_allows_acknowledged_idle_installation_without_profiles() {
        let mut state = core::SupervisorState {
            attempted_revision: "old".into(),
            error: Some("Core not started: no profile imported".into()),
            ..Default::default()
        };
        assert!(!uninstall_result(&state, "off", false).unwrap());
        state.attempted_revision = "off".into();
        assert!(uninstall_result(&state, "off", false).unwrap());
        assert!(uninstall_result(&state, "off", true).is_err());
    }

    fn failed() -> Cleanup {
        Cleanup::Failed("cannot start nft".into())
    }
    fn unknown() -> Cleanup {
        Cleanup::Unknown("connection refused".into())
    }
    /// Runs `wait_for_cleanup` over `script`, whose last look repeats; returns the result and
    /// the number of looks.
    async fn run_cleanup(script: &[Cleanup]) -> (Result<()>, usize) {
        let mut looks = 0;
        let result = wait_for_cleanup(
            || {
                let next = script[looks.min(script.len() - 1)].clone();
                looks += 1;
                async move { next }
            },
            Duration::from_millis(30),
            Duration::from_millis(1),
            Duration::from_secs(5),
        )
        .await;
        (result, looks)
    }

    #[tokio::test]
    async fn uninstall_waits_until_the_helper_removed_its_exceptions() {
        let (result, looks) = run_cleanup(&[failed(), failed(), Cleanup::Done]).await;
        result.unwrap();
        assert_eq!(looks, 3);
    }

    #[tokio::test]
    async fn uninstall_keeps_a_helper_that_cannot_remove_its_exceptions() {
        let error = run_cleanup(&[failed()]).await.0.unwrap_err().to_string();
        assert!(
            error.starts_with(
                "TUN helper retained: it could not remove its firewall exceptions: cannot start nft"
            ),
            "{error}"
        );
    }

    #[tokio::test]
    async fn a_briefly_unreachable_helper_is_asked_again() {
        for script in [
            vec![unknown(), Cleanup::Done],
            vec![failed(), unknown(), Cleanup::Done],
        ] {
            let (result, looks) = run_cleanup(&script).await;
            result.unwrap();
            assert_eq!(looks, script.len());
        }
    }

    #[tokio::test]
    async fn uninstall_keeps_a_helper_that_stays_unreachable() {
        let error = run_cleanup(&[unknown()]).await.0.unwrap_err().to_string();
        assert!(
            error.starts_with(
                "TUN helper retained: cannot confirm that it removed its firewall exceptions: connection refused"
            ),
            "{error}"
        );
    }

    #[tokio::test]
    async fn a_failing_helper_that_goes_away_is_kept() {
        for gone in [unknown(), Cleanup::Stopped, Cleanup::Legacy] {
            let error = run_cleanup(&[failed(), gone])
                .await
                .0
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("could not remove its firewall exceptions: cannot start nft"),
                "{error}"
            );
        }
    }

    #[tokio::test]
    async fn older_and_stopped_helpers_are_not_waited_for() {
        for first in [Cleanup::Legacy, Cleanup::Stopped] {
            let (result, looks) = run_cleanup(&[first]).await;
            result.unwrap();
            assert_eq!(looks, 1);
        }
    }

    #[tokio::test]
    async fn a_look_that_hangs_ends_by_the_deadline() {
        let wait = wait_for_cleanup(
            std::future::pending::<Cleanup>,
            Duration::from_millis(50),
            Duration::from_millis(1),
            Duration::from_secs(30),
        );
        let error = tokio::time::timeout(Duration::from_secs(5), wait)
            .await
            .expect("the wait overran its deadline")
            .unwrap_err()
            .to_string();
        assert!(
            error.starts_with(
                "TUN helper retained: cannot confirm that it removed its firewall exceptions: the TUN helper did not answer in time"
            ),
            "{error}"
        );
    }

    #[tokio::test]
    async fn a_slow_retry_is_cut_off_and_the_reported_failure_kept() {
        let mut looks = 0;
        let wait = wait_for_cleanup(
            || {
                looks += 1;
                let first = looks == 1;
                async move {
                    if first {
                        failed()
                    } else {
                        std::future::pending::<Cleanup>().await
                    }
                }
            },
            Duration::from_millis(80),
            Duration::from_millis(1),
            Duration::from_millis(20),
        );
        let error = tokio::time::timeout(Duration::from_secs(5), wait)
            .await
            .expect("the wait overran its deadline")
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("could not remove its firewall exceptions: cannot start nft"),
            "{error}"
        );
        assert!(looks >= 2, "{looks}");
    }

    #[test]
    fn only_inactive_or_failed_units_count_as_stopped() {
        for state in ["inactive", "failed"] {
            assert!(stopped_unit(state), "{state}");
        }
        for state in ["active", "activating", "deactivating", "reloading", ""] {
            assert!(!stopped_unit(state), "{state}");
        }
    }
}
