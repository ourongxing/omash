use crate::{
    api::MihomoClient,
    config::Config,
    profiles::{CoreKind, Profiles},
    tun::{
        process::{Process, Spec, private_write},
        protocol::{Action, Client, HelperUnavailable},
    },
};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::process::Stdio;
use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime},
};
use tokio::process::Command;

pub enum ConfigApply {
    Reloaded,
    Restarted,
}

enum Backend {
    Direct(Process),
    Service(Client),
}
#[derive(Clone)]
struct Applied {
    spec: Spec,
    // Built text before the live-mode override; revisions are compared by this.
    source_text: String,
    config: Config,
    service: bool,
    profile_id: Option<String>,
    source_mode: String,
}
impl Applied {
    fn candidate(
        mut spec: Spec,
        config: &Config,
        profile_id: Option<String>,
        previous: Option<&Self>,
        live_mode: Option<&str>,
    ) -> Result<Self> {
        let source_text = spec.text.clone();
        let source_mode = spec.mode()?;
        // An API-only mode choice survives a restart unless the profile's own mode changed.
        if let Some(mode) = live_mode
            && previous.is_some_and(|a| {
                a.spec.core == spec.core
                    && a.profile_id == profile_id
                    && a.source_mode == source_mode
            })
        {
            spec.set_mode(mode)?;
        }
        Ok(Self {
            service: config.tun_enabled && spec.core == CoreKind::Mihomo,
            spec,
            source_text,
            config: config.clone(),
            profile_id,
            source_mode,
        })
    }
    // The live mode is deliberately not compared: only the source text decides.
    fn reloads(&self, previous: &Self, running: bool, tun_active: bool) -> bool {
        running
            && (!self.service || tun_active)
            && previous.spec.core == self.spec.core
            && previous.service == self.service
            && previous.source_text == self.source_text
    }
}
pub struct CoreManager {
    backend: Option<Backend>,
    applied: Option<Applied>,
}

impl CoreManager {
    pub const fn new() -> Self {
        Self {
            backend: None,
            applied: None,
        }
    }

    fn prepare(config: &Config, profiles: &Profiles) -> Result<Spec> {
        let core = profiles.current_core();
        if core == CoreKind::Mihomo {
            ensure_core_resources()?;
        }
        let staged = staged_runtime_path(core);
        profiles.build_runtime_at(config, &staged)?;
        let text = fs::read_to_string(&staged);
        let _ = fs::remove_file(staged);
        Ok(Spec {
            core,
            data_dir: Config::data_dir(),
            text: text?,
        })
    }
    pub async fn validate_only(&self, config: &Config, profiles: &Profiles) -> Result<()> {
        let spec = Self::prepare(config, profiles)?;
        Self::validate_spec(&spec).await
    }
    async fn validate_spec(spec: &Spec) -> Result<()> {
        let staged = staged_runtime_path(spec.core);
        let result = spec.validate(&staged).await;
        let _ = fs::remove_file(staged);
        result
    }
    pub async fn refresh(&mut self) -> Result<()> {
        if let Some(Backend::Service(client)) = &mut self.backend
            && let Err(e) = client.request(Action::Status).await
        {
            client.status.running = false;
            client.status.tun_active = false;
            return Err(e);
        }
        Ok(())
    }
    pub fn is_running(&mut self) -> bool {
        match &mut self.backend {
            Some(Backend::Direct(p)) => p.running(),
            Some(Backend::Service(c)) => c.status.running,
            None => false,
        }
    }
    pub fn pid(&self) -> Option<u32> {
        match &self.backend {
            Some(Backend::Direct(p)) => p.pid(),
            Some(Backend::Service(c)) => c.status.pid,
            None => None,
        }
    }
    fn backend_name(&self) -> Option<String> {
        self.backend.as_ref().map(|b| {
            match b {
                Backend::Direct(_) => "direct",
                Backend::Service(_) => "service",
            }
            .into()
        })
    }
    fn tun_active(&self) -> bool {
        matches!(&self.backend, Some(Backend::Service(c)) if c.status.tun_active)
    }
    fn tun_health_error(&self) -> Option<String> {
        match &self.backend {
            Some(Backend::Service(c)) if c.status.running && !c.status.tun_active => Some(
                c.status.firewall_error.as_ref().map_or_else(
                    || "Mihomo is running but TUN runtime/device could not be confirmed; run `omash tun doctor`".into(),
                    |error| format!("TUN firewall: {error}"),
                ),
            ),
            _ => None,
        }
    }
    fn helper_version(&self) -> Option<String> {
        match &self.backend {
            Some(Backend::Service(c)) => Some(c.status.helper_version.clone()),
            _ => None,
        }
    }
    fn applied_config(&self) -> Option<&Config> {
        self.applied.as_ref().map(|a| &a.config)
    }

    pub async fn start(&mut self, config: &Config, profiles: &Profiles) -> Result<()> {
        self.restart(config, profiles).await.map(|_| ())
    }
    pub async fn stop(&mut self) -> Result<()> {
        if let Some(backend) = &mut self.backend {
            match backend {
                Backend::Direct(p) => p.stop().await?,
                Backend::Service(c) => {
                    c.request(Action::Stop {
                        generation: c.status.generation,
                    })
                    .await?;
                }
            }
        }
        self.backend = None;
        Ok(())
    }
    async fn launch(applied: &Applied, service: Option<Client>) -> Result<Backend> {
        if applied.service {
            let mut client = match service {
                Some(c) => c,
                None => Client::connect().await?,
            };
            client
                .request(Action::Start {
                    yaml: applied.spec.text.clone(),
                    validate_only: false,
                })
                .await?;
            Ok(Backend::Service(client))
        } else {
            let path =
                Config::data_dir().join(format!("core-active.{}", applied.spec.core.extension()));
            Ok(Backend::Direct(
                Process::start(applied.spec.clone(), &path).await?,
            ))
        }
    }
    fn commit_mirror(spec: &Spec) -> Result<()> {
        private_write(&Config::runtime_path_for(spec.core), spec.text.as_bytes())?;
        let opposite = match spec.core {
            CoreKind::Mihomo => CoreKind::Singbox,
            CoreKind::Singbox => CoreKind::Mihomo,
        };
        match fs::remove_file(Config::runtime_path_for(opposite)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
            _ => {}
        }
        Ok(())
    }
    pub async fn restart(&mut self, config: &Config, profiles: &Profiles) -> Result<ConfigApply> {
        let mut live_mode = None;
        if self.is_running()
            && let Some(applied) = &self.applied
            && let Ok(api) = applied.spec.api()
            && let Ok(runtime) = api.runtime_config().await
        {
            live_mode = Some(runtime.mode.to_ascii_lowercase());
        }
        let result = self.apply(config, profiles, live_mode.as_deref()).await;
        if let Err(error) = result {
            // Rollback may restart the old core; restore an API-only mode selection too.
            if self.is_running()
                && let Some(mode) = live_mode
                && let Some(applied) = &self.applied
                && let Ok(api) = applied.spec.api()
                && let Err(rollback) = api.set_mode(&mode).await
            {
                // Keep the original error as the source so callers can still classify it.
                return Err(error.context(format!("routing-mode rollback failed: {rollback:#}")));
            }
            return Err(error);
        }
        result
    }

    async fn apply(
        &mut self,
        config: &Config,
        profiles: &Profiles,
        live_mode: Option<&str>,
    ) -> Result<ConfigApply> {
        let spec = Self::prepare(config, profiles)?;
        let candidate = Applied::candidate(
            spec,
            config,
            profiles.current.clone(),
            self.applied.as_ref(),
            live_mode,
        )?;
        let (running, tun_active) = (self.is_running(), self.tun_active());
        if self
            .applied
            .as_ref()
            .is_some_and(|a| candidate.reloads(a, running, tun_active))
        {
            let _ = restore_selected_nodes(config, profiles).await;
            // System proxy preferences may change without changing the core's YAML.
            let applied = self.applied.as_mut().unwrap();
            applied.config = candidate.config;
            applied.profile_id = candidate.profile_id;
            applied.source_mode = candidate.source_mode;
            return Ok(ConfigApply::Reloaded);
        }
        // The helper validates Service candidates itself (preflight or Apply).
        let service = candidate.service;
        if !service {
            Self::validate_spec(&candidate.spec).await?;
        }
        // Preflight with the current lease when already using the helper; a second
        // connection is a different owner and must never replace an active core.
        if let Some(Backend::Service(client)) = &mut self.backend
            && service
        {
            client
                .request(Action::Apply {
                    yaml: candidate.spec.text.clone(),
                    generation: client.status.generation,
                })
                .await?;
            if let Err(error) = Self::commit_mirror(&candidate.spec) {
                if let Some(previous) = &self.applied {
                    client
                        .request(Action::Apply {
                            yaml: previous.spec.text.clone(),
                            generation: client.status.generation,
                        })
                        .await
                        .context("runtime mirror failed and rollback failed")?;
                }
                return Err(error);
            }
        } else {
            let preflight = if service {
                let mut c = Client::connect().await?;
                c.request(Action::Start {
                    yaml: candidate.spec.text.clone(),
                    validate_only: true,
                })
                .await?;
                Some(c)
            } else {
                None
            };
            let previous = self.applied.clone().filter(|_| self.is_running());
            self.stop().await?;
            let attempt = match Self::launch(&candidate, preflight).await {
                Ok(backend) => {
                    self.backend = Some(backend);
                    Self::commit_mirror(&candidate.spec)
                }
                Err(error) => Err(error),
            };
            if let Err(error) = attempt {
                self.stop().await.context("failed to stop rejected core")?;
                let restored = previous.is_some();
                if let Some(previous) = previous {
                    self.backend =
                        Some(Self::launch(&previous, None).await.with_context(|| {
                            format!("new configuration failed ({error:#}); rollback also failed")
                        })?);
                    Self::commit_mirror(&previous.spec)?;
                }
                return Err(error).context(if restored {
                    "new configuration failed; previous core restored"
                } else {
                    "new configuration failed; no previous core to restore"
                });
            }
        }
        self.applied = Some(candidate);
        let _ = restore_selected_nodes(config, profiles).await;
        Ok(ConfigApply::Restarted)
    }

    pub fn recent_logs(limit: usize) -> Result<Vec<String>> {
        let Some(path) = fs::read_dir(Config::logs_dir())?
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| {
                p.extension().is_some_and(|ext| ext == "log")
                    && p.file_name().is_none_or(|n| n != "core-validation.log")
            })
            .max_by_key(|p| fs::metadata(p).and_then(|m| m.modified()).ok())
        else {
            return Ok(vec![]);
        };
        let text = fs::read_to_string(path)?;
        let mut lines: Vec<_> = text.lines().rev().take(limit).map(str::to_owned).collect();
        lines.reverse();
        Ok(lines)
    }
}
fn staged_runtime_path(core: CoreKind) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    Config::runtime_path_for(core).with_file_name(format!(
        "runtime.pending-{}-{nonce}.{}",
        std::process::id(),
        core.extension()
    ))
}

pub fn ensure_system_core() -> Result<()> {
    let profiles = Profiles::load().unwrap_or_default();
    if profiles.items.is_empty() {
        return Ok(());
    }
    let core = profiles.current_core();
    let path = Config::core_path(core);
    if !path.is_file() {
        bail!(
            "system {} not found at {}; install the Arch {} package",
            core.as_str(),
            path.display(),
            core.as_str()
        );
    }
    Ok(())
}

async fn restore_selected_nodes(config: &Config, profiles: &Profiles) -> Result<()> {
    let selected = profiles.current_selections();
    if selected.is_empty() {
        return Ok(());
    }
    let api = MihomoClient::new(&config.controller, config.secret.clone())?;
    let proxies = api.proxies().await?;
    for selection in selected {
        let Some(group) = proxies.proxies.get(&selection.name) else {
            continue;
        };
        if group.all.iter().any(|node| node == &selection.now) && group.now != selection.now {
            api.select_proxy(&selection.name, &selection.now).await?;
        }
    }
    Ok(())
}

fn ensure_core_resources() -> Result<()> {
    let destination = Config::data_dir().join("Country.mmdb");
    if destination.exists() {
        return Ok(());
    }
    let source = ["/etc/mihomo/Country.mmdb", "/etc/clash/Country.mmdb"]
        .into_iter()
        .map(Path::new)
        .find(|path| path.is_file())
        .context("system Country.mmdb not found; install the Arch clash-geoip package")?;
    link_or_copy(source, &destination).with_context(|| {
        format!(
            "failed to link system Country.mmdb from {} to {}",
            source.display(),
            destination.display()
        )
    })
}

#[cfg(unix)]
fn link_or_copy(source: &Path, destination: &Path) -> Result<()> {
    std::os::unix::fs::symlink(source, destination)
        .or_else(|_| fs::copy(source, destination).map(|_| ()))?;
    Ok(())
}

#[cfg(not(unix))]
fn link_or_copy(source: &Path, destination: &Path) -> Result<()> {
    fs::copy(source, destination)?;
    Ok(())
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct SupervisorState {
    pub running: bool,
    pub pid: Option<u32>,
    pub restarts: u64,
    pub reloads: u64,
    pub error: Option<String>,
    pub desired_revision: String,
    pub attempted_revision: String,
    pub applied_revision: String,
    pub backend: Option<String>,
    pub tun_desired: bool,
    pub tun_active: bool,
    pub tun_state: TunState,
    pub helper_version: Option<String>,
}

impl SupervisorState {
    // The manager's present view: a core that died since the previous pass is not "running".
    fn observe(&mut self, manager: &mut CoreManager) {
        self.running = manager.is_running();
        self.pid = manager.pid();
        self.backend = manager.backend_name();
        self.tun_active = manager.tun_active();
        self.helper_version = manager.helper_version();
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TunState {
    #[default]
    Disabled,
    Starting,
    Active,
    Unavailable,
    Failed,
}
impl std::fmt::Display for TunState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Disabled => "disabled",
            Self::Starting => "starting",
            Self::Active => "active",
            Self::Unavailable => "unavailable",
            Self::Failed => "failed",
        })
    }
}

const SUPERVISOR_SERVICE: &str = "omash-supervisor.service";
const PACKAGED_SUPERVISOR_UNIT: &str = "/usr/lib/systemd/user/omash-supervisor.service";

pub async fn ensure_supervisor(auto_start: bool) -> Result<()> {
    let migrated = migrate_legacy_supervisor_unit()?;
    user_systemctl(&["daemon-reload"]).await?;
    if migrated && auto_start {
        user_systemctl(&["reenable", "--now", SUPERVISOR_SERVICE]).await?;
    } else {
        set_supervisor_autostart(auto_start).await?;
    }
    user_systemctl(&["start", SUPERVISOR_SERVICE]).await
}

pub async fn set_supervisor_autostart(enabled: bool) -> Result<()> {
    if enabled {
        user_systemctl(&["enable", "--now", SUPERVISOR_SERVICE]).await
    } else {
        // Disabling login startup must not interrupt the currently running proxy.
        user_systemctl(&["disable", SUPERVISOR_SERVICE]).await
    }
}

/// An upgrade replaces the omash binary under a running supervisor, which keeps the previous
/// build, and with it the previous helper protocol, until it is restarted.
pub async fn supervisor_runs_replaced_binary() -> Result<bool> {
    let pid = user_systemctl_output(&["show", "--property=MainPID", "--value", SUPERVISOR_SERVICE])
        .await?;
    main_pid_runs_replaced_binary(&pid)
}

/// `main_pid` as `systemctl show` prints it; 0 is a unit that is not running.
fn main_pid_runs_replaced_binary(main_pid: &str) -> Result<bool> {
    let pid: u32 = main_pid
        .trim()
        .parse()
        .with_context(|| format!("unexpected supervisor MainPID {main_pid:?}"))?;
    if pid == 0 {
        return Ok(false);
    }
    runs_replaced_binary(pid)
}

/// Installers unlink the old file before writing the new one (a running executable cannot be
/// rewritten in place), and the kernel then names the process's executable `<path> (deleted)`.
fn runs_replaced_binary(pid: u32) -> Result<bool> {
    let exe = fs::read_link(format!("/proc/{pid}/exe"))
        .with_context(|| format!("cannot read the executable of process {pid}"))?;
    Ok(exe.as_os_str().as_encoded_bytes().ends_with(b" (deleted)"))
}

/// Restarts the supervisor if it is running.
pub async fn restart_supervisor() -> Result<()> {
    user_systemctl(&["try-restart", SUPERVISOR_SERVICE]).await
}

fn migrate_legacy_supervisor_unit() -> Result<bool> {
    if !Path::new(PACKAGED_SUPERVISOR_UNIT).is_file() {
        return Ok(false);
    }
    let unit_path = dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("systemd/user")
        .join(SUPERVISOR_SERVICE);
    let Some(unit) = fs::read_to_string(&unit_path).ok() else {
        return Ok(false);
    };
    if unit.starts_with("# BinaryModified=") && unit.contains("Description=Omash Mihomo Supervisor")
    {
        fs::remove_file(unit_path)?;
        return Ok(true);
    }
    Ok(false)
}

const RETRY_MIN: Duration = Duration::from_secs(5);
const RETRY_MAX: Duration = Duration::from_secs(30);
// Bounds a probe, including the Hello a stalled helper never answers (RPC_TIMEOUT is 90 s).
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

// Spacing between automatic attempts after `failures` consecutive failures:
// 5 s, 5 s, 10 s, 20 s, then capped at 30 s. Zero failures still keep 5 s apart.
fn retry_backoff(failures: u32) -> Duration {
    RETRY_MIN
        .saturating_mul(1 << failures.saturating_sub(1).min(3))
        .min(RETRY_MAX)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Attempt {
    Skip,
    Apply,
    // Check that the helper answers before touching the Direct core that is running.
    Probe,
}

struct Pending {
    new_request: bool,
    running: bool,
    // TUN is wanted, the helper backend is not in use and the last failure was a missing helper.
    helper_retry: bool,
    failures: u32,
    since_last: Option<Duration>,
}

// A new revision is attempted at once. Otherwise only a core that is not running is
// retried; a failed change next to a running core stays failed, except a missing helper.
fn attempt_decision(p: &Pending) -> Attempt {
    if p.new_request {
        Attempt::Apply
    } else if p
        .since_last
        .is_some_and(|elapsed| elapsed < retry_backoff(p.failures))
    {
        Attempt::Skip
    } else if !p.running {
        Attempt::Apply
    } else if p.helper_retry {
        Attempt::Probe
    } else {
        Attempt::Skip
    }
}

struct TunView {
    active: bool,
    wanted: bool,
    enabled: bool,
    // The current profile's core can use the managed helper (Mihomo).
    capable: bool,
    helper_unavailable: bool,
    attempting: bool,
}

fn tun_state(v: &TunView) -> TunState {
    if v.active {
        TunState::Active
    } else if !v.wanted || !v.enabled {
        TunState::Disabled
    } else if !v.capable || v.helper_unavailable {
        TunState::Unavailable
    } else if v.attempting {
        TunState::Starting
    } else {
        TunState::Failed
    }
}

// A timed-out probe is a failed probe. The connection, if any, is dropped at once: a second
// connection never owns the lease, so closing it stops nothing.
async fn probe<T>(connect: impl Future<Output = Result<T>>, limit: Duration) -> bool {
    matches!(tokio::time::timeout(limit, connect).await, Ok(Ok(_)))
}

// The original failure stays first: `omash tun on` reports it verbatim.
fn fallback_error(original: &str, fallback: Result<()>) -> String {
    match fallback {
        Ok(()) => format!("{original}; proxy running without TUN"),
        Err(error) => format!("{original}; proxy fallback without TUN also failed: {error:#}"),
    }
}

pub async fn run_supervisor(mut config: Config) -> Result<()> {
    let mut manager = CoreManager::new();
    let mut state = SupervisorState::default();
    let mut proxy_config: Option<Config> = None;
    let mut revisions = RevisionCache::default();
    let mut state_file = StateWriter::new(Config::supervisor_state_path());
    // Consecutive failed attempts (a failed helper probe counts), the end of the last
    // attempt, and whether the failure being retried was a missing helper.
    let mut failures = 0u32;
    let mut last_attempt: Option<Instant> = None;
    let mut helper_unavailable = false;
    let mut was_running = false;
    let mut health_error = None;
    // Register once, before a potentially slow apply: a SIGTERM during validation
    // must remain pending until we can gracefully stop the owned generation.
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    loop {
        // Hash first: a file that changes after this point is a new revision on the next
        // pass instead of being recorded as applied without having been read.
        let revision = revisions.current();
        let enabled = core_desired_enabled();
        let mut read_error = match load_daemon_config() {
            Ok(latest) => { config = latest; None }
            Err(_) => Some("invalid omash configuration; check config.toml syntax (details withheld to protect credentials)".into()),
        };
        let profiles = match Profiles::load() {
            Ok(profiles) => profiles,
            Err(_) => {
                read_error = Some("invalid profile index; check profiles.yaml syntax (details withheld to protect subscription URLs)".into());
                Profiles::default()
            }
        };
        state.desired_revision = revision.clone();
        state.tun_desired = config.tun_enabled;
        if let Err(e) = manager.refresh().await {
            state.error = Some(format!("TUN lease lost: {e:#}"));
            manager.backend = None;
            // The helper connection is gone: until an attempt says otherwise, so is the helper.
            // Nothing runs now, so the next attempt is still a plain Apply, never a probe.
            helper_unavailable = true;
        }
        let running = manager.is_running();
        if was_running && !running {
            // A core that vanished is a new situation, not one more failed attempt: only
            // the minimum spacing applies, not a backoff grown by failed helper probes.
            failures = 0;
        }
        let new_request = state.attempted_revision != revision;
        let capable = profiles.current_core() == CoreKind::Mihomo;
        let wants_tun = config.tun_enabled && capable;
        if !enabled || (read_error.is_none() && profiles.items.is_empty()) {
            if let Some(previous) = proxy_config.take() {
                let _ = apply_system_proxy(&previous, false).await;
            }
            manager.stop().await?;
            failures = 0;
            helper_unavailable = false;
            state.attempted_revision = revision.clone();
            state.error = (enabled && profiles.items.is_empty())
                .then(|| "Core not started: no profile imported".into());
            if state.error.is_none() {
                state.applied_revision = revision.clone();
            }
        } else if let Some(error) = read_error {
            state.attempted_revision = revision.clone();
            state.error = Some(error);
        } else {
            let mut attempt = attempt_decision(&Pending {
                new_request,
                running,
                helper_retry: wants_tun
                    && helper_unavailable
                    && !matches!(manager.backend, Some(Backend::Service(_))),
                failures,
                since_last: last_attempt.map(|t| t.elapsed()),
            });
            if attempt == Attempt::Probe {
                let reachable = probe(Client::connect(), PROBE_TIMEOUT).await;
                last_attempt = Some(Instant::now());
                if reachable {
                    attempt = Attempt::Apply;
                } else {
                    failures = failures.saturating_add(1);
                }
            }
            if attempt == Attempt::Apply {
                state.attempted_revision = revision.clone();
                // The helper is tried again: the stale verdict must not label this attempt.
                helper_unavailable = false;
                if new_request {
                    failures = 0;
                    state.error = None;
                }
                // Before the (possibly slow) attempt: a core that died since the last pass must
                // read as starting, while an Apply on a live TUN still reads as active.
                state.observe(&mut manager);
                state.tun_state = tun_state(&TunView {
                    active: state.tun_active,
                    wanted: config.tun_enabled,
                    enabled,
                    capable,
                    helper_unavailable,
                    attempting: true,
                });
                state_file.write(&state)?;
                if let Some(previous) = proxy_config.take() {
                    let _ = apply_system_proxy(&previous, false).await;
                }
                let outcome = if running {
                    manager.restart(&config, &profiles).await
                } else {
                    manager
                        .start(&config, &profiles)
                        .await
                        .map(|_| ConfigApply::Restarted)
                };
                match outcome {
                    Ok(outcome) => {
                        match outcome {
                            ConfigApply::Reloaded => state.reloads += 1,
                            ConfigApply::Restarted => state.restarts += 1,
                        }
                        state.applied_revision = revision.clone();
                        state.error = None;
                        failures = 0;
                        helper_unavailable = false;
                    }
                    Err(error) => {
                        failures = failures.saturating_add(1);
                        helper_unavailable = error.downcast_ref::<HelperUnavailable>().is_some();
                        let mut message = format!("{error:#}");
                        if wants_tun && !manager.is_running() {
                            // Nothing runs: keep the ordinary proxy up while TUN is unavailable.
                            // This precedes the system-proxy step so its environment is applied.
                            let mut plain = config.clone();
                            plain.tun_enabled = false;
                            message =
                                fallback_error(&message, manager.start(&plain, &profiles).await);
                        }
                        state.error = Some(message);
                    }
                }
                last_attempt = Some(Instant::now());
                // A failed switch may have restored a different controller/proxy port.
                if manager.is_running()
                    && let Some(applied) = manager.applied_config().filter(|c| c.system_proxy)
                {
                    match apply_system_proxy(applied, true).await {
                        Ok(()) => proxy_config = Some(applied.clone()),
                        Err(error) => {
                            state.error = Some(format!(
                                "{}system proxy failed: {error}",
                                state
                                    .error
                                    .as_ref()
                                    .map(|e| format!("{e}; "))
                                    .unwrap_or_default()
                            ))
                        }
                    }
                }
            }
        }
        state.observe(&mut manager);
        state.tun_state = tun_state(&TunView {
            active: state.tun_active,
            wanted: config.tun_enabled,
            enabled,
            capable,
            helper_unavailable,
            attempting: false,
        });
        // A repaired firewall must clear its transient error without hiding a
        // failed Apply or other independent error from the current generation.
        if health_error.is_some() && state.error == health_error {
            state.error = None;
        }
        health_error = manager.tun_health_error();
        if state.error.is_none() {
            state.error = health_error.clone();
        }
        state_file.write(&state)?;
        was_running = state.running;
        let shutdown = tokio::select! {
            _ = terminate.recv() => true,
            _ = tokio::signal::ctrl_c() => true,
            _ = tokio::time::sleep(Duration::from_secs(1)) => false,
        };
        if shutdown {
            break;
        }
    }
    if let Some(previous) = proxy_config {
        let _ = apply_system_proxy(&previous, false).await;
    }
    // The final state is written even when the core could not be stopped cleanly.
    let stopped = manager.stop().await;
    state.running = false;
    state.pid = None;
    state.tun_active = false;
    state.tun_state = TunState::Disabled;
    state_file.write(&state)?;
    stopped
}

pub fn supervisor_state() -> SupervisorState {
    fs::read_to_string(Config::supervisor_state_path())
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

pub fn request_core_enabled(enabled: bool) -> Result<()> {
    let path = Config::disabled_state_path();
    if enabled {
        if path.exists() {
            fs::remove_file(path)?;
        }
    } else {
        private_write(&path, b"disabled by user\n")?;
    }
    request_restart()
}

pub fn core_desired_enabled() -> bool {
    !Config::disabled_state_path().exists()
}

pub fn request_restart() -> Result<()> {
    private_write(
        &Config::restart_request_path(),
        uuid::Uuid::new_v4().to_string().as_bytes(),
    )
}

pub async fn cli_stop() -> Result<()> {
    if !core_desired_enabled() && !supervisor_state().running {
        println!("omash core is already stopped");
        return Ok(());
    }
    request_core_enabled(false)?;
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut last = supervisor_state();
    while Instant::now() < deadline {
        last = supervisor_state();
        if !last.running {
            println!("omash core stopped; system proxy cleared");
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    match last.pid {
        Some(pid) => println!(
            "stop requested, but the core is still running (pid {pid}); see {}",
            Config::logs_dir().display()
        ),
        None => println!(
            "stop requested, but the supervisor did not apply it within 10 seconds; see {}",
            Config::logs_dir().display()
        ),
    }
    Ok(())
}

pub async fn cli_start() -> Result<()> {
    let before = supervisor_state();
    if core_desired_enabled() && before.running {
        println!("omash core is already running");
        return Ok(());
    }
    request_core_enabled(true)?;
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut last = before.clone();
    while Instant::now() < deadline {
        last = supervisor_state();
        if last.running {
            match last.pid {
                Some(pid) => println!("omash core started (pid {pid})"),
                None => println!("omash core started"),
            }
            return Ok(());
        }
        if let Some(error) = startup_failure(&before, &last) {
            println!("omash core failed to start: {error}");
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    println!(
        "start requested, but the core is not running yet; last state: {}",
        last.error
            .unwrap_or_else(|| "no response from supervisor".into())
    );
    Ok(())
}

pub async fn cli_restart() -> Result<()> {
    if !core_desired_enabled() {
        println!("omash core is disabled; run `omash start` first");
        return Ok(());
    }
    let before = supervisor_state();
    request_restart()?;
    let applied = before.restarts.saturating_add(before.reloads);
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let last = supervisor_state();
        if last.restarts.saturating_add(last.reloads) > applied {
            match last.pid.filter(|_| last.running) {
                Some(pid) => println!("omash core restarted (pid {pid})"),
                None => println!("omash core restarted"),
            }
            return Ok(());
        }
        if let Some(error) = startup_failure(&before, &last) {
            println!("omash core restart failed: {error}");
            return Ok(());
        }
        if Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    println!("restart requested; supervisor did not apply it within 15 seconds");
    Ok(())
}

// The supervisor reports "starting <core>" while a start attempt is in flight;
// only a new, non-transient error compared to the pre-request state means the
// attempt actually failed.
fn startup_failure(before: &SupervisorState, last: &SupervisorState) -> Option<String> {
    let error = last.error.as_ref()?;
    if error.starts_with("starting ") || last.error == before.error {
        return None;
    }
    Some(error.clone())
}

fn load_daemon_config() -> Result<Config> {
    Config::load(&crate::config::Cli {
        command: None,
        daemon: true,
        refresh_ms: None,
        config: None,
    })
}

fn revision_paths() -> Vec<PathBuf> {
    let mut paths = vec![
        Config::default_path(),
        Config::profiles_path(),
        Config::restart_request_path(),
        Config::disabled_state_path(),
    ];
    if let Ok(entries) = fs::read_dir(Config::profiles_dir()) {
        paths.extend(entries.filter_map(Result::ok).map(|e| e.path()));
    }
    paths.sort();
    paths
}

// The wire format is shared with the CLI and with older supervisors: do not change it.
fn hash_paths(paths: &[PathBuf]) -> String {
    use sha2::{Digest, Sha256};
    let mut digest = Sha256::new();
    for path in paths {
        digest.update(path.as_os_str().as_encoded_bytes());
        if let Ok(bytes) = fs::read(path) {
            digest.update((bytes.len() as u64).to_be_bytes());
            digest.update(bytes);
        }
    }
    format!("{:x}", digest.finalize())
}

pub fn desired_revision() -> String {
    hash_paths(&revision_paths())
}

const REVISION_MAX_AGE: Duration = Duration::from_secs(30);

// No ctime: it moves on chmod/chown/link changes that leave the content alone (`Config::load`
// used to chmod config.toml on every pass), which would make the cache miss for nothing.
// Size, mtime and inode reveal edits and rename-over replacements; the periodic rehash
// backs them up.
#[derive(Debug, PartialEq, Eq)]
struct Fingerprint {
    len: u64,
    mtime: (i64, i64),
    inode: u64,
}

fn fingerprint(path: &Path) -> Option<Fingerprint> {
    use std::os::unix::fs::MetadataExt;
    let meta = fs::metadata(path).ok()?;
    Some(Fingerprint {
        len: meta.len(),
        mtime: (meta.mtime(), meta.mtime_nsec()),
        inode: meta.ino(),
    })
}

struct CachedRevision {
    files: Vec<(PathBuf, Option<Fingerprint>)>,
    revision: String,
    computed: Instant,
}

// `desired_revision` for a loop that runs every second: stat the same files instead of
// reading them while nothing changed, and rehash everything at least every 30 s.
#[derive(Default)]
struct RevisionCache {
    cached: Option<CachedRevision>,
}

impl RevisionCache {
    fn current(&mut self) -> String {
        self.of(revision_paths())
    }

    fn of(&mut self, paths: Vec<PathBuf>) -> String {
        self.of_at(paths, Instant::now())
    }

    fn of_at(&mut self, paths: Vec<PathBuf>, now: Instant) -> String {
        // Fingerprint before hashing: a change racing the hash is seen on the next call.
        let files: Vec<_> = paths
            .into_iter()
            .map(|path| {
                let print = fingerprint(&path);
                (path, print)
            })
            .collect();
        if let Some(cached) = &self.cached
            && cached.files == files
            && now.saturating_duration_since(cached.computed) < REVISION_MAX_AGE
        {
            return cached.revision.clone();
        }
        let paths: Vec<_> = files.iter().map(|(path, _)| path.clone()).collect();
        let revision = hash_paths(&paths);
        self.cached = Some(CachedRevision {
            files,
            revision: revision.clone(),
            computed: now,
        });
        revision
    }
}

// Rewrites the state file (fsync) only when its content changed or the file vanished.
struct StateWriter {
    path: PathBuf,
    last: Option<Vec<u8>>,
}

impl StateWriter {
    fn new(path: PathBuf) -> Self {
        Self { path, last: None }
    }

    fn write(&mut self, state: &SupervisorState) -> Result<bool> {
        let bytes = serde_json::to_vec(state)?;
        if self.last.as_deref() == Some(bytes.as_slice()) && self.path.exists() {
            return Ok(false);
        }
        private_write(&self.path, &bytes)?;
        self.last = Some(bytes);
        Ok(true)
    }
}

async fn user_systemctl(arguments: &[&str]) -> Result<()> {
    user_systemctl_output(arguments).await.map(drop)
}

async fn user_systemctl_output(arguments: &[&str]) -> Result<String> {
    let output = Command::new("systemctl")
        .arg("--user")
        .args(arguments)
        .output()
        .await?;
    if !output.status.success() {
        bail!(
            "systemctl --user failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

pub async fn apply_system_proxy(config: &Config, enabled: bool) -> Result<()> {
    let mut supported = false;
    if command_exists("gsettings").await {
        supported = true;
        if enabled {
            let port = config.mixed_port.to_string();
            for protocol in ["http", "https", "socks"] {
                let schema = format!("org.gnome.system.proxy.{protocol}");
                run("gsettings", &["set", &schema, "host", "127.0.0.1"]).await?;
                run("gsettings", &["set", &schema, "port", &port]).await?;
            }
            let bypass = gsettings_bypass(&config.proxy_bypass);
            run(
                "gsettings",
                &["set", "org.gnome.system.proxy", "ignore-hosts", &bypass],
            )
            .await?;
            run(
                "gsettings",
                &["set", "org.gnome.system.proxy", "use-same-proxy", "true"],
            )
            .await?;
        }
        run(
            "gsettings",
            &[
                "set",
                "org.gnome.system.proxy.http",
                "enabled",
                if enabled { "true" } else { "false" },
            ],
        )
        .await?;
        let mode = if enabled { "manual" } else { "none" };
        run(
            "gsettings",
            &["set", "org.gnome.system.proxy", "mode", mode],
        )
        .await?;
    }

    // Omarchy launches desktop applications as UWSM/systemd user units. Such
    // applications do not consistently consume GNOME's gsettings proxy, but
    // inherit the user manager environment. Keep both backends in sync.
    if command_exists("systemctl").await {
        supported = true;
        let keys = [
            "http_proxy",
            "https_proxy",
            "all_proxy",
            "no_proxy",
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "NO_PROXY",
        ];
        if enabled {
            let http = format!("http://127.0.0.1:{}", config.mixed_port);
            let socks = format!("socks5://127.0.0.1:{}", config.mixed_port);
            let values = [
                format!("http_proxy={http}"),
                format!("https_proxy={http}"),
                format!("all_proxy={socks}"),
                format!("no_proxy={}", config.proxy_bypass),
                format!("HTTP_PROXY={http}"),
                format!("HTTPS_PROXY={http}"),
                format!("ALL_PROXY={socks}"),
                format!("NO_PROXY={}", config.proxy_bypass),
            ];
            let mut arguments = vec!["--user", "set-environment"];
            arguments.extend(values.iter().map(String::as_str));
            run("systemctl", &arguments).await?;
        } else {
            let mut arguments = vec!["--user", "unset-environment"];
            arguments.extend(keys);
            run("systemctl", &arguments).await?;
        }

        // UWSM scopes inherit the (possibly stale) environment of the menu or
        // compositor that launched them. Services inherit the current systemd
        // user-manager environment, allowing proxy changes to reach Chrome and
        // other newly launched Omarchy applications without a new login.
        if command_exists("uwsm-app").await {
            let unit_type = if enabled { "service" } else { "scope" };
            let setting = format!("UWSM_APP_UNIT_TYPE={unit_type}");
            run("systemctl", &["--user", "set-environment", &setting]).await?;
            let daemon_active = Command::new("systemctl")
                .args([
                    "--user",
                    "is-active",
                    "--quiet",
                    "wayland-wm-app-daemon.service",
                ])
                .status()
                .await
                .is_ok_and(|status| status.success());
            if daemon_active {
                run(
                    "systemctl",
                    &["--user", "restart", "wayland-wm-app-daemon.service"],
                )
                .await?;
            }
        }
    }

    if supported {
        Ok(())
    } else {
        bail!("this session has no supported system-proxy backend")
    }
}

fn gsettings_bypass(value: &str) -> String {
    let items = value
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(|item| format!("'{}'", item.replace('\\', "\\\\").replace('\'', "\\'")))
        .collect::<Vec<_>>()
        .join(", ");
    format!("[{items}]")
}

async fn command_exists(name: &str) -> bool {
    Command::new("sh")
        .args(["-c", &format!("command -v {name}")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .is_ok_and(|s| s.success())
}

async fn run(program: &str, args: &[&str]) -> Result<()> {
    let output = Command::new(program).args(args).output().await?;
    if !output.status.success() {
        bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    fn secs(n: u64) -> Option<Duration> {
        Some(Duration::from_secs(n))
    }

    fn retry() -> Pending {
        Pending {
            new_request: false,
            running: false,
            helper_retry: false,
            failures: 1,
            since_last: secs(0),
        }
    }

    #[test]
    fn backoff_starts_at_five_seconds_doubles_and_caps_at_thirty() {
        let seconds = |failures| retry_backoff(failures).as_secs();
        assert_eq!(
            [0, 1, 2, 3, 4, 5, 6, 100, u32::MAX].map(seconds),
            [5, 5, 10, 20, 30, 30, 30, 30, 30]
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_process_whose_binary_was_replaced_is_detected() {
        let dir = tempfile::tempdir().unwrap();
        let program = dir.path().join("omash");
        let sleep = ["/usr/bin/sleep", "/bin/sleep"]
            .into_iter()
            .find(|path| Path::new(path).is_file())
            .unwrap();
        fs::copy(sleep, &program).unwrap();
        // A test thread that forks while the copy is still open for writing makes the exec fail
        // with ETXTBSY until that child execs in turn.
        let mut child = loop {
            match std::process::Command::new(&program).arg("30").spawn() {
                Err(error) if error.raw_os_error() == Some(libc::ETXTBSY) => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                spawned => break spawned.unwrap(),
            }
        };
        // posix_spawn returns as soon as the child lets go of our memory, which exec does
        // before it switches the executable that /proc shows.
        let running = fs::canonicalize(&program).unwrap();
        let exe = format!("/proc/{}/exe", child.id());
        let deadline = Instant::now() + Duration::from_secs(5);
        while fs::read_link(&exe).ok().as_deref() != Some(running.as_path()) {
            assert!(Instant::now() < deadline, "the child never ran {running:?}");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!runs_replaced_binary(child.id()).unwrap());
        // An upgrade unlinks the running file and writes a new one at the same path.
        fs::remove_file(&program).unwrap();
        fs::copy(sleep, &program).unwrap();
        let replaced = runs_replaced_binary(child.id());
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(replaced.unwrap());
    }

    #[test]
    fn a_supervisor_that_cannot_be_inspected_is_an_error() {
        // systemd prints MainPID 0 for a unit that is not running.
        assert!(!main_pid_runs_replaced_binary("0\n").unwrap());
        assert!(main_pid_runs_replaced_binary("").is_err());
        // No process has this PID, so its executable cannot be read.
        assert!(main_pid_runs_replaced_binary(&u32::MAX.to_string()).is_err());
    }

    #[test]
    fn first_start_failure_is_retried_after_the_backoff() {
        // Nothing ever ran: the failed first attempt must not be sticky.
        for (failures, wait) in [(0, 5), (1, 5), (2, 10), (3, 20), (4, 30), (9, 30)] {
            let decide = |since| {
                attempt_decision(&Pending {
                    failures,
                    since_last: since,
                    ..retry()
                })
            };
            assert_eq!(decide(secs(wait - 1)), Attempt::Skip, "{failures}");
            assert_eq!(decide(secs(wait)), Attempt::Apply, "{failures}");
        }
        assert_eq!(
            attempt_decision(&Pending {
                since_last: None,
                ..retry()
            }),
            Attempt::Apply
        );
    }

    #[test]
    fn new_requests_are_attempted_immediately() {
        for running in [false, true] {
            let decision = attempt_decision(&Pending {
                new_request: true,
                running,
                failures: 9,
                since_last: secs(0),
                ..retry()
            });
            assert_eq!(decision, Attempt::Apply);
        }
    }

    #[test]
    fn failed_change_next_to_a_running_core_is_not_retried() {
        let decision = attempt_decision(&Pending {
            running: true,
            since_last: secs(3600),
            ..retry()
        });
        assert_eq!(decision, Attempt::Skip);
    }

    #[test]
    fn missing_helper_next_to_a_direct_core_is_probed_after_the_backoff() {
        let decide = |failures, since| {
            attempt_decision(&Pending {
                running: true,
                helper_retry: true,
                failures,
                since_last: secs(since),
                ..retry()
            })
        };
        assert_eq!(decide(2, 9), Attempt::Skip);
        assert_eq!(decide(2, 10), Attempt::Probe);
        // With nothing running the attempt itself reaches the helper; no probe is needed.
        let stopped = Pending {
            helper_retry: true,
            since_last: secs(5),
            ..retry()
        };
        assert_eq!(attempt_decision(&stopped), Attempt::Apply);
    }

    #[test]
    fn lease_loss_is_retried_as_a_plain_attempt_after_the_minimum_spacing() {
        // Lease loss drops the backend and sets the helper verdict (for tun_state only).
        let lost = |since| {
            attempt_decision(&Pending {
                helper_retry: true,
                failures: 0,
                since_last: secs(since),
                ..retry()
            })
        };
        assert_eq!(lost(4), Attempt::Skip);
        assert_eq!(lost(5), Attempt::Apply);
        // Until that attempt begins the state reads unavailable, not failed.
        let waiting = TunView {
            helper_unavailable: true,
            ..view()
        };
        assert_eq!(tun_state(&waiting), TunState::Unavailable);
    }

    #[tokio::test]
    async fn in_flight_attempt_reports_the_present_core_state() {
        use crate::tun::protocol::Status;
        let (stream, _peer) = tokio::net::UnixStream::pair().unwrap();
        let status = Status {
            running: true,
            tun_active: true,
            pid: Some(7),
            helper_version: "1.2.3".into(),
            ..Default::default()
        };
        let mut manager = CoreManager {
            backend: Some(Backend::Service(Client::from_parts(stream, status))),
            applied: None,
        };
        let attempting = |state: &SupervisorState| {
            tun_state(&TunView {
                active: state.tun_active,
                attempting: true,
                ..view()
            })
        };
        let mut state = SupervisorState::default();
        // An Apply on a live TUN keeps reading as active.
        state.observe(&mut manager);
        assert!(state.running && state.tun_active);
        assert_eq!(state.pid, Some(7));
        assert_eq!(state.backend.as_deref(), Some("service"));
        assert_eq!(state.helper_version.as_deref(), Some("1.2.3"));
        assert_eq!(attempting(&state), TunState::Active);
        // kill -9 of the helper's core: its next Status reply says so, and the recovery
        // attempt must not keep the previous pass's tun_active.
        let Some(Backend::Service(client)) = &mut manager.backend else {
            unreachable!()
        };
        client.status.running = false;
        client.status.tun_active = false;
        client.status.pid = None;
        state.observe(&mut manager);
        assert!(!state.running && !state.tun_active && state.pid.is_none());
        assert_eq!(state.backend.as_deref(), Some("service"));
        assert_eq!(attempting(&state), TunState::Starting);
        // Lease loss drops the backend altogether.
        manager.backend = None;
        state.observe(&mut manager);
        assert!(!state.running && state.backend.is_none() && state.helper_version.is_none());
    }

    #[test]
    fn probes_back_off_while_the_helper_stays_down() {
        // The failed attempt happened at 0 s; the loop ticks once per second.
        let (mut failures, mut last) = (1, 0);
        let mut probes = vec![];
        for now in 1..=100 {
            let decision = attempt_decision(&Pending {
                running: true,
                helper_retry: true,
                failures,
                since_last: secs(now - last),
                ..retry()
            });
            if decision == Attempt::Probe {
                probes.push(now);
                (failures, last) = (failures + 1, now);
            }
        }
        assert_eq!(probes, [5, 15, 35, 65, 95]);
    }

    #[tokio::test]
    async fn probe_succeeds_only_for_a_prompt_connection() {
        let limit = Duration::from_millis(100);
        assert!(probe(async { Ok(()) }, limit).await);
        let refused = async { Err::<(), _>(anyhow::anyhow!("refused")) };
        assert!(!probe(refused, limit).await);
        // A helper that accepts but never answers is a failed probe, not a stalled loop.
        let started = Instant::now();
        assert!(!probe(std::future::pending::<Result<()>>(), limit).await);
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn fallback_error_keeps_the_original_failure_first() {
        let original = "TUN helper unavailable; run `omash tun setup`: socket is gone";
        assert_eq!(
            fallback_error(original, Ok(())),
            format!("{original}; proxy running without TUN")
        );
        let failed = fallback_error(
            original,
            Err(anyhow::anyhow!("mixed port is already in use")),
        );
        assert_eq!(
            failed,
            format!(
                "{original}; proxy fallback without TUN also failed: mixed port is already in use"
            )
        );
    }

    fn view() -> TunView {
        TunView {
            active: false,
            wanted: true,
            enabled: true,
            capable: true,
            helper_unavailable: false,
            attempting: false,
        }
    }

    #[test]
    fn tun_state_follows_the_contract() {
        assert_eq!(tun_state(&view()), TunState::Failed);
        let unavailable = TunView {
            helper_unavailable: true,
            ..view()
        };
        assert_eq!(tun_state(&unavailable), TunState::Unavailable);
        let singbox = TunView {
            capable: false,
            ..view()
        };
        assert_eq!(tun_state(&singbox), TunState::Unavailable);
        let attempting = TunView {
            attempting: true,
            ..view()
        };
        assert_eq!(tun_state(&attempting), TunState::Starting);
        // Unavailable precedes starting; the loop clears a stale helper verdict when a
        // real attempt begins, which is why that attempt reports starting.
        let stale = TunView {
            helper_unavailable: true,
            ..attempting
        };
        assert_eq!(tun_state(&stale), TunState::Unavailable);
        let singbox = TunView {
            capable: false,
            attempting: true,
            ..view()
        };
        assert_eq!(tun_state(&singbox), TunState::Unavailable);
    }

    #[test]
    fn tun_state_is_disabled_when_tun_is_not_wanted_whatever_else_failed() {
        for view in [
            TunView {
                wanted: false,
                helper_unavailable: true,
                ..view()
            },
            TunView {
                wanted: false,
                capable: false,
                attempting: true,
                ..view()
            },
            TunView {
                enabled: false,
                helper_unavailable: true,
                ..view()
            },
        ] {
            assert_eq!(tun_state(&view), TunState::Disabled);
        }
    }

    #[test]
    fn tun_state_is_active_even_if_a_later_request_failed() {
        let later_failure = TunView {
            active: true,
            helper_unavailable: true,
            attempting: true,
            ..view()
        };
        assert_eq!(tun_state(&later_failure), TunState::Active);
        assert_eq!(
            serde_json::to_string(&TunState::Unavailable).unwrap(),
            "\"unavailable\""
        );
    }

    fn spec(core: CoreKind, text: &str) -> Spec {
        Spec {
            core,
            data_dir: PathBuf::new(),
            text: text.into(),
        }
    }

    fn candidate(text: &str, tun: bool, previous: Option<&Applied>, live: Option<&str>) -> Applied {
        let config = Config {
            tun_enabled: tun,
            ..Config::default()
        };
        let spec = spec(CoreKind::Mihomo, text);
        Applied::candidate(spec, &config, Some("profile".into()), previous, live).unwrap()
    }

    const TEXT: &str = "mode: rule\nrules: [MATCH,DIRECT]\n";

    #[test]
    fn selection_change_after_an_api_mode_switch_reloads() {
        let running = candidate(TEXT, false, None, None);
        // `PATCH /configs mode=global` only changes the live core; a selection-only
        // profile change then yields the same built text.
        let next = candidate(TEXT, false, Some(&running), Some("global"));
        assert_eq!(next.spec.mode().unwrap(), "global");
        assert_ne!(next.spec.text, running.spec.text);
        assert_eq!(next.source_text, running.source_text);
        assert!(next.reloads(&running, true, false));
        // The same holds once a real restart carried the live mode into the applied spec.
        let changed = "mode: rule\nrules: []\n";
        let restarted = candidate(changed, false, Some(&next), Some("global"));
        assert!(!restarted.reloads(&next, true, false));
        assert_eq!(restarted.spec.mode().unwrap(), "global");
        let again = candidate(changed, false, Some(&restarted), Some("global"));
        assert!(again.reloads(&restarted, true, false));
        assert_eq!(again.spec.mode().unwrap(), "global");
    }

    #[test]
    fn real_text_change_restarts() {
        let running = candidate(TEXT, false, None, None);
        let edited = candidate("mode: rule\nrules: []\n", false, Some(&running), None);
        assert!(!edited.reloads(&running, true, false));
        // A mode written in the profile wins over the live mode and restarts.
        let direct = candidate("mode: direct\n", false, Some(&running), Some("global"));
        assert_eq!(direct.spec.mode().unwrap(), "direct");
        assert!(!direct.reloads(&running, true, false));
        // A candidate for a different profile does not take over the live mode.
        let config = Config::default();
        let spec = spec(CoreKind::Mihomo, TEXT);
        let other = Applied::candidate(
            spec,
            &config,
            Some("other".into()),
            Some(&running),
            Some("global"),
        );
        assert_eq!(other.unwrap().spec.mode().unwrap(), "rule");
    }

    #[test]
    fn backend_or_core_change_restarts() {
        let direct = candidate(TEXT, false, None, None);
        let service = candidate(TEXT, true, Some(&direct), None);
        assert!(service.service && !direct.service);
        assert!(!service.reloads(&direct, true, true));
        assert!(!direct.reloads(&service, true, true));
        // JSON is valid for both cores: only the core differs.
        let json = r#"{"experimental":{"clash_api":{"default_mode":"rule"}}}"#;
        let mihomo = candidate(json, false, None, None);
        let singbox = Applied::candidate(
            spec(CoreKind::Singbox, json),
            &Config::default(),
            Some("profile".into()),
            None,
            None,
        )
        .unwrap();
        assert_eq!(singbox.source_text, mihomo.source_text);
        assert!(!singbox.reloads(&mihomo, true, false));
    }

    #[test]
    fn service_reload_needs_a_running_core_with_confirmed_tun() {
        let service = candidate(TEXT, true, None, None);
        let same = candidate(TEXT, true, Some(&service), None);
        assert!(same.reloads(&service, true, true));
        assert!(!same.reloads(&service, true, false));
        assert!(!same.reloads(&service, false, true));
        let direct = candidate(TEXT, false, None, None);
        assert!(!direct.reloads(&direct, false, false));
    }

    #[test]
    fn state_file_is_rewritten_only_when_its_content_changes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("supervisor-state.json");
        let mut writer = StateWriter::new(path.clone());
        let mut state = SupervisorState {
            running: true,
            pid: Some(7),
            ..Default::default()
        };
        assert!(writer.write(&state).unwrap());
        let inode = fs::metadata(&path).unwrap().ino();
        assert!(!writer.write(&state).unwrap());
        assert_eq!(fs::metadata(&path).unwrap().ino(), inode);
        // The final shutdown state differs and therefore always lands.
        state.running = false;
        state.pid = None;
        assert!(writer.write(&state).unwrap());
        assert_ne!(fs::metadata(&path).unwrap().ino(), inode);
        let text = fs::read_to_string(&path).unwrap();
        assert!(!text.contains("updated_at"));
        assert!(
            !serde_json::from_str::<SupervisorState>(&text)
                .unwrap()
                .running
        );
        // A deleted file is restored even though the content is unchanged.
        fs::remove_file(&path).unwrap();
        assert!(writer.write(&state).unwrap());
        assert!(path.exists());
    }

    #[test]
    fn state_files_written_with_updated_at_stay_readable() {
        let old = r#"{"running":true,"pid":42,"restarts":3,"updated_at":1700000000}"#;
        let state: SupervisorState = serde_json::from_str(old).unwrap();
        assert!(state.running && state.pid == Some(42) && state.restarts == 3);
        let text = serde_json::to_string(&state).unwrap();
        assert!(!text.contains("updated_at"));
    }

    // Independent statement of the revision format shared with older supervisors.
    fn reference_revision(paths: &[PathBuf]) -> String {
        use sha2::{Digest, Sha256};
        let mut bytes = vec![];
        for path in paths {
            bytes.extend_from_slice(path.as_os_str().as_encoded_bytes());
            if let Ok(content) = fs::read(path) {
                bytes.extend_from_slice(&(content.len() as u64).to_be_bytes());
                bytes.extend_from_slice(&content);
            }
        }
        format!("{:x}", Sha256::digest(&bytes))
    }

    #[test]
    fn revision_format_is_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b, c) = (
            dir.path().join("a"),
            dir.path().join("b"),
            dir.path().join("c"),
        );
        fs::write(&a, "one").unwrap();
        fs::write(&c, "").unwrap();
        let paths = [a, b, c];
        assert_eq!(hash_paths(&paths), reference_revision(&paths));
        // sha256("/nonexistent/omash-revision"): a missing file contributes only its path.
        assert_eq!(
            hash_paths(&[PathBuf::from("/nonexistent/omash-revision")]),
            "6b57f56f9ab58666d9ac02bc223e06d263a18021c71c805328dbfa7608871261"
        );
    }

    struct Tree {
        _dir: tempfile::TempDir,
        root: PathBuf,
    }
    impl Tree {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().to_path_buf();
            for (name, text) in [("config.toml", "aaaa"), ("profiles.yaml", "bbbb")] {
                fs::write(root.join(name), text).unwrap();
            }
            fs::create_dir(root.join("profiles")).unwrap();
            fs::write(root.join("profiles/one.yaml"), "cccc").unwrap();
            Self { _dir: dir, root }
        }
        // Mirrors `revision_paths`: fixed files (some missing) plus the listed directory.
        fn paths(&self) -> Vec<PathBuf> {
            let mut paths = vec![
                self.root.join("config.toml"),
                self.root.join("profiles.yaml"),
                self.root.join("restart-request"),
                self.root.join("core-disabled"),
            ];
            let entries = fs::read_dir(self.root.join("profiles")).unwrap();
            paths.extend(entries.map(|e| e.unwrap().path()));
            paths.sort();
            paths
        }
        fn rename_over(&self, name: &str, text: &str) {
            let temporary = self.root.join(".rewrite");
            fs::write(&temporary, text).unwrap();
            fs::rename(temporary, self.root.join(name)).unwrap();
        }
    }

    // True when the unchanged tree is answered from the cache rather than rehashed.
    fn served_from_cache(cache: &mut RevisionCache, tree: &Tree) -> bool {
        let genuine = cache.cached.as_ref().unwrap().revision.clone();
        cache.cached.as_mut().unwrap().revision = "poisoned".into();
        let served = cache.of(tree.paths()) == "poisoned";
        if served {
            cache.cached.as_mut().unwrap().revision = genuine;
        }
        served
    }

    #[test]
    fn cached_revision_equals_the_uncached_one() {
        let tree = Tree::new();
        let mut cache = RevisionCache::default();
        let first = cache.of(tree.paths());
        assert_eq!(first, hash_paths(&tree.paths()));
        assert_eq!(first, reference_revision(&tree.paths()));
        assert!(served_from_cache(&mut cache, &tree));
        assert_eq!(cache.of(tree.paths()), first);
    }

    #[test]
    fn metadata_only_changes_keep_the_cache_valid() {
        use std::os::unix::fs::PermissionsExt;
        let tree = Tree::new();
        let mut cache = RevisionCache::default();
        let first = cache.of(tree.paths());
        // A chmod (`Config::load` used to do one on every pass) bumps the ctime but not
        // the content, and must not defeat the cache.
        std::thread::sleep(Duration::from_millis(30));
        let config = tree.root.join("config.toml");
        fs::set_permissions(&config, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(served_from_cache(&mut cache, &tree));
        assert_eq!(cache.of(tree.paths()), first);
    }

    #[test]
    fn rewriting_a_file_changes_the_revision_even_with_the_same_size() {
        let tree = Tree::new();
        let mut cache = RevisionCache::default();
        let first = cache.of(tree.paths());
        tree.rename_over("profiles/one.yaml", "dddd");
        let second = cache.of(tree.paths());
        assert_ne!(second, first);
        assert_eq!(second, hash_paths(&tree.paths()));
        assert!(served_from_cache(&mut cache, &tree));
    }

    #[test]
    fn modifying_a_file_in_place_changes_the_revision() {
        use std::io::Write;
        let tree = Tree::new();
        let mut cache = RevisionCache::default();
        let first = cache.of(tree.paths());
        let path = tree.root.join("profiles/one.yaml");
        let file = fs::OpenOptions::new().write(true).open(&path).unwrap();
        (&file).write_all(b"eeee").unwrap();
        // Timestamps can tie on a coarse clock; make the modification time distinct.
        file.set_modified(SystemTime::now() + Duration::from_secs(10))
            .unwrap();
        let second = cache.of(tree.paths());
        assert_ne!(second, first);
        assert_eq!(second, hash_paths(&tree.paths()));
    }

    #[test]
    fn created_removed_and_added_files_change_the_revision() {
        let tree = Tree::new();
        let mut cache = RevisionCache::default();
        let mut seen = vec![cache.of(tree.paths())];
        fs::write(tree.root.join("restart-request"), "x").unwrap();
        seen.push(cache.of(tree.paths()));
        fs::write(tree.root.join("profiles/two.yaml"), "cccc").unwrap();
        seen.push(cache.of(tree.paths()));
        fs::remove_file(tree.root.join("profiles/one.yaml")).unwrap();
        seen.push(cache.of(tree.paths()));
        fs::remove_file(tree.root.join("restart-request")).unwrap();
        seen.push(cache.of(tree.paths()));
        for window in seen.windows(2) {
            assert_ne!(window[0], window[1]);
        }
        assert_eq!(seen[4], hash_paths(&tree.paths()));
    }

    #[test]
    fn cache_is_rehashed_after_thirty_seconds() {
        let tree = Tree::new();
        let mut cache = RevisionCache::default();
        let start = Instant::now();
        let first = cache.of_at(tree.paths(), start);
        cache.cached.as_mut().unwrap().revision = "poisoned".into();
        let almost = start + REVISION_MAX_AGE - Duration::from_secs(1);
        assert_eq!(cache.of_at(tree.paths(), almost), "poisoned");
        let expired = start + REVISION_MAX_AGE;
        assert_eq!(cache.of_at(tree.paths(), expired), first);
    }
}
