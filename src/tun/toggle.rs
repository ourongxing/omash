//! The Mihomo TUN switch in the TUI: the rules of `omash tun on/off`, an offer to install a missing
//! or outdated helper first, and the result of a request, watched without blocking the screen.

use crate::{core::SupervisorState, profiles::CoreKind};
use std::time::{Duration, Instant};

pub const LINUX_ONLY: &str = "Mihomo TUN requires Linux/systemd";
pub const MIHOMO_ONLY: &str =
    "select a Mihomo profile before enabling managed TUN (sing-box is not managed by this helper)";
pub const APPLYING: &str = "Applying TUN…";
pub const SUPERSEDED: &str =
    "Mihomo TUN was switched again elsewhere; its row in Settings shows the current state";
/// How long `omash tun on/off` and the TUI wait for the supervisor to apply a request.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(100);
pub const TIMED_OUT: &str =
    "TUN request timed out; inspect `omash tun doctor` (desired setting is preserved)";
/// Bounds the look at the helper before TUN is turned on (its Hello alone may take 90 s).
pub const HELPER_LOOK: Duration = Duration::from_secs(2);

/// What one look at the helper showed before TUN is turned on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Helper {
    /// No helper was installed: `/etc/omash-tun.json` does not exist.
    Missing,
    /// The helper answered with an older or newer protocol.
    Outdated,
    /// The helper answered, or is installed but did not answer; the supervisor falls back and
    /// retries it on its own.
    Installed,
}

/// What the TUI offers to run through sudo before turning TUN on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Setup {
    Install,
    Update,
}

/// Why TUN cannot be turned on at all; turning it off is never refused.
pub fn refusal(linux: bool, core: CoreKind, enable: bool) -> Option<&'static str> {
    if !enable {
        None
    } else if !linux {
        Some(LINUX_ONLY)
    } else if core != CoreKind::Mihomo {
        Some(MIHOMO_ONLY)
    } else {
        None
    }
}

/// `installed` is whether `/etc/omash-tun.json` exists; `error` is how connecting failed, if it did
/// (a look that ran out of time carries no error).
pub fn classify(installed: bool, error: Option<&anyhow::Error>) -> Helper {
    if !installed {
        Helper::Missing
    } else if error.is_some_and(|error| {
        error
            .downcast_ref::<super::protocol::ProtocolMismatch>()
            .is_some()
    }) {
        Helper::Outdated
    } else {
        Helper::Installed
    }
}

/// Whether the helper has to be installed or updated before TUN is turned on.
pub fn setup_needed(helper: Helper) -> Option<Setup> {
    match helper {
        Helper::Missing => Some(Setup::Install),
        Helper::Outdated => Some(Setup::Update),
        Helper::Installed => None,
    }
}

/// What a switch of Mihomo TUN does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    Refuse(&'static str),
    Offer(Setup),
    Apply,
}

/// Turning TUN off is applied at once, without looking at the helper. Turning it on is refused
/// where `omash tun on` refuses it, and first offers to install a missing or outdated helper.
pub async fn decide<F>(
    linux: bool,
    core: CoreKind,
    enable: bool,
    look: impl FnOnce() -> F,
) -> Decision
where
    F: std::future::Future<Output = Helper>,
{
    if let Some(reason) = refusal(linux, core, enable) {
        return Decision::Refuse(reason);
    }
    if !enable {
        return Decision::Apply;
    }
    match setup_needed(look().await) {
        Some(setup) => Decision::Offer(setup),
        None => Decision::Apply,
    }
}

/// The notice after the installation was declined in the confirmation.
pub fn declined(setup: Setup) -> &'static str {
    match setup {
        Setup::Install => {
            "TUN stays off: the TUN helper is not installed. Press Enter on Mihomo TUN to install it, or run `omash tun setup` in a terminal."
        }
        Setup::Update => {
            "TUN stays off: the TUN helper is outdated. Press Enter on Mihomo TUN to update it, or run `omash tun setup` in a terminal."
        }
    }
}

/// The notice after sudo or the installation itself failed.
pub fn setup_failed(error: &str) -> String {
    format!(
        "TUN stays off: TUN helper setup failed: {error}. Press Enter on Mihomo TUN to try again, or run `omash tun setup` in a terminal."
    )
}

/// A TUN switch whose result the TUI is waiting for.
#[derive(Clone, Debug)]
pub struct Request {
    pub revision: String,
    pub enabled: bool,
    pub started: Instant,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Progress {
    Pending,
    Done(String),
    Failed(String),
}

impl Request {
    pub fn new(revision: String, enabled: bool) -> Self {
        Self {
            revision,
            enabled,
            started: Instant::now(),
        }
    }

    /// The files the supervisor watches changed after this request (a node selection, a profile
    /// update, another setting): the supervisor applies their newer revision, never this one.
    /// While `tun_enabled` still matches the request, that revision carries it, so the request
    /// follows it and waits its full time again. Returns false when TUN was switched again
    /// elsewhere and the request no longer applies; `tun_enabled` is checked even when the
    /// revision did not change, because a revision adopted with a stale `tun_enabled` (read before
    /// `omash tun off` ran) is the current one from then on.
    pub fn follow(&mut self, current_revision: &str, tun_enabled: bool) -> bool {
        if tun_enabled != self.enabled {
            return false;
        }
        if current_revision == self.revision {
            return true;
        }
        current_revision.clone_into(&mut self.revision);
        self.started = Instant::now();
        true
    }

    pub fn progress(&self, state: &SupervisorState) -> Progress {
        progress(state, &self.revision, self.enabled, self.started.elapsed())
    }
}

/// The same verdict as `omash tun on/off`, after `elapsed` of waiting.
pub fn progress(
    state: &SupervisorState,
    revision: &str,
    enabled: bool,
    elapsed: Duration,
) -> Progress {
    match super::cli::revision_result(state, revision, enabled) {
        Ok(true) if enabled => Progress::Done("TUN is active".into()),
        Ok(true) => Progress::Done("TUN is off".into()),
        Err(error) => Progress::Failed(format!("{error:#}")),
        Ok(false) if elapsed >= REQUEST_TIMEOUT => Progress::Failed(TIMED_OUT.into()),
        Ok(false) => Progress::Pending,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tun::protocol::{HelperUnavailable, ProtocolMismatch};

    #[test]
    fn turning_tun_on_is_refused_where_the_cli_refuses_it() {
        assert_eq!(refusal(false, CoreKind::Mihomo, true), Some(LINUX_ONLY));
        assert_eq!(refusal(true, CoreKind::Singbox, true), Some(MIHOMO_ONLY));
        assert_eq!(refusal(true, CoreKind::Mihomo, true), None);
        // Turning it off is always possible, whatever the profile or platform.
        assert_eq!(refusal(false, CoreKind::Singbox, false), None);
        assert_eq!(refusal(true, CoreKind::Singbox, false), None);
    }

    #[test]
    fn a_missing_or_outdated_helper_is_offered_for_setup() {
        let mismatch = anyhow::Error::from(ProtocolMismatch { peer: 1 }).context(HelperUnavailable);
        let unreachable = anyhow::anyhow!("failed to connect").context(HelperUnavailable);
        // Without the settings file nothing was installed, whatever connecting showed.
        assert_eq!(classify(false, None), Helper::Missing);
        assert_eq!(classify(false, Some(&unreachable)), Helper::Missing);
        assert_eq!(classify(true, Some(&mismatch)), Helper::Outdated);
        // Installed but silent or unreachable: the supervisor falls back and retries by itself.
        assert_eq!(classify(true, Some(&unreachable)), Helper::Installed);
        assert_eq!(classify(true, None), Helper::Installed);
        assert_eq!(setup_needed(Helper::Missing), Some(Setup::Install));
        assert_eq!(setup_needed(Helper::Outdated), Some(Setup::Update));
        assert_eq!(setup_needed(Helper::Installed), None);
    }

    #[tokio::test]
    async fn turning_tun_off_never_looks_at_the_helper() {
        let looked = std::cell::Cell::new(0);
        let look = || {
            looked.set(looked.get() + 1);
            async { Helper::Missing }
        };
        assert_eq!(
            decide(true, CoreKind::Mihomo, false, look).await,
            Decision::Apply
        );
        assert_eq!(
            decide(false, CoreKind::Singbox, false, look).await,
            Decision::Apply
        );
        assert_eq!(looked.get(), 0, "turning TUN off looked at the helper");
        // A refusal comes before the look, too.
        assert_eq!(
            decide(true, CoreKind::Singbox, true, look).await,
            Decision::Refuse(MIHOMO_ONLY)
        );
        assert_eq!(
            decide(false, CoreKind::Mihomo, true, look).await,
            Decision::Refuse(LINUX_ONLY)
        );
        assert_eq!(looked.get(), 0, "a refused switch looked at the helper");
        assert_eq!(
            decide(true, CoreKind::Mihomo, true, look).await,
            Decision::Offer(Setup::Install)
        );
        assert_eq!(
            decide(true, CoreKind::Mihomo, true, || async { Helper::Outdated }).await,
            Decision::Offer(Setup::Update)
        );
        assert_eq!(
            decide(true, CoreKind::Mihomo, true, || async { Helper::Installed }).await,
            Decision::Apply
        );
        assert_eq!(looked.get(), 1);
    }

    #[test]
    fn every_stays_off_notice_says_how_to_install_later() {
        for notice in [
            declined(Setup::Install).to_owned(),
            declined(Setup::Update).to_owned(),
            setup_failed("TUN setup failed (exit status: 1)"),
        ] {
            assert!(notice.starts_with("TUN stays off: "), "{notice}");
            assert!(notice.contains("Press Enter on Mihomo TUN to "), "{notice}");
            assert!(
                notice.ends_with("or run `omash tun setup` in a terminal."),
                "{notice}"
            );
        }
        assert!(setup_failed("boom").contains("TUN helper setup failed: boom."));
    }

    fn state(applied: &str, attempted: &str, running: bool, tun: bool) -> SupervisorState {
        SupervisorState {
            running,
            tun_active: tun,
            applied_revision: applied.into(),
            attempted_revision: attempted.into(),
            ..Default::default()
        }
    }

    #[test]
    fn a_request_follows_later_changes_that_keep_its_tun_wish() {
        // TUN was turned on (revision r1), then a node was selected (r2) before the supervisor
        // applied r1; it applies r2, with TUN.
        let applied = state("r2", "r2", true, true);
        // Judged against r1, the request could only ever time out.
        assert_eq!(
            progress(&applied, "r1", true, REQUEST_TIMEOUT),
            Progress::Failed(TIMED_OUT.into())
        );
        let mut request = Request::new("r1".into(), true);
        assert!(request.follow("r2", true));
        assert_eq!(request.revision, "r2");
        assert_eq!(
            request.progress(&applied),
            Progress::Done("TUN is active".into())
        );
        // A failure of the newer revision is the request's failure.
        let mut failed = state("r1", "r2", true, false);
        failed.error = Some("core rejected".into());
        assert_eq!(
            request.progress(&failed),
            Progress::Failed("TUN request failed: core rejected".into())
        );
        // Unchanged files change nothing.
        let mut same = Request::new("r1".into(), false);
        assert!(same.follow("r1", false));
        assert_eq!(same.revision, "r1");
        // TUN switched again elsewhere (`omash tun off`): the request no longer applies.
        let mut switched = Request::new("r1".into(), true);
        assert!(!switched.follow("r2", false));
        // The same, when the revision of `omash tun off` was adopted with a `tun_enabled` read
        // before it ran: the next look, with the current value, still ends the request.
        let mut raced = Request::new("r1".into(), true);
        assert!(raced.follow("r2", true));
        assert!(!raced.follow("r2", false));
    }

    #[test]
    fn a_request_reports_its_result_like_the_cli() {
        let soon = Duration::from_secs(1);
        // Not attempted yet, or attempted and still applying.
        assert_eq!(
            progress(&state("old", "old", true, false), "new", true, soon),
            Progress::Pending
        );
        assert_eq!(
            progress(&state("old", "new", true, false), "new", true, soon),
            Progress::Pending
        );
        assert_eq!(
            progress(&state("new", "new", true, true), "new", true, soon),
            Progress::Done("TUN is active".into())
        );
        assert_eq!(
            progress(&state("new", "new", true, false), "new", false, soon),
            Progress::Done("TUN is off".into())
        );
        // Applied without TUN is not a success for a request to turn it on.
        assert_eq!(
            progress(&state("new", "new", true, false), "new", true, soon),
            Progress::Pending
        );
        let mut failed = state("old", "new", true, false);
        failed.error = Some("TUN helper unavailable; proxy running without TUN".into());
        assert_eq!(
            progress(&failed, "new", true, soon),
            Progress::Failed(
                "TUN request failed: TUN helper unavailable; proxy running without TUN".into()
            )
        );
        assert_eq!(
            progress(
                &state("old", "old", true, false),
                "new",
                true,
                REQUEST_TIMEOUT
            ),
            Progress::Failed(TIMED_OUT.into())
        );
    }
}
