use super::{
    process::{Process, Spec},
    protocol::*,
    routes::{self, Identity, Rules, Snapshot},
};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_yaml_ng::Mapping;
use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::{
    net::{UnixListener, UnixStream},
    sync::Mutex,
};

pub const SETTINGS: &str = "/etc/omash-tun.json";
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    pub uid: u32,
    pub gid: u32,
    pub data_dir: PathBuf,
}

pub fn load_settings() -> Result<Settings> {
    super::install::trusted_path(Path::new(SETTINGS), false)?;
    Ok(serde_json::from_slice(&fs::read(SETTINGS)?)?)
}

struct State {
    settings: Settings,
    process: Option<Process>,
    owner: Option<String>,
    generation: u64,
    revision: String,
    /// Rules and routes the running core added.
    routes: Snapshot,
    /// Rules of cores that ended; removed before a conflict check once they are provably ours.
    residue: Rules,
    /// What the ledger file last held (owned, residue); `None` while unknown.
    persisted: Option<(Rules, Rules)>,
    /// Owner and revision of the last successful validate-only request.
    validated: Option<(String, String)>,
    firewall_error: Option<String>,
    /// Creates the firewall manager; tests replace it to simulate nft failures.
    new_firewall: fn(u32) -> Result<super::firewall::Firewall>,
}
impl State {
    fn new(settings: Settings, residue: Rules) -> Self {
        Self {
            settings,
            process: None,
            owner: None,
            generation: 0,
            revision: String::new(),
            routes: Snapshot::default(),
            residue,
            persisted: None,
            validated: None,
            firewall_error: None,
            new_firewall: super::firewall::Firewall::new,
        }
    }
    async fn status(&mut self) -> Status {
        let running = self.process.as_mut().is_some_and(Process::running);
        let device = self.process.as_ref().and_then(|p| p.device.clone());
        let mut tun_active = false;
        if running
            && self.firewall_error.is_none()
            && let Some(p) = &self.process
            && let Ok(Ok(runtime)) = tokio::time::timeout(
                std::time::Duration::from_millis(600),
                p.api.runtime_config(),
            )
            .await
        {
            tun_active = super::process::confirms_tun(&runtime, device.as_deref())
                && super::process::device_exists(device.as_deref()).await;
        }
        Status {
            protocol: VERSION,
            helper_version: env!("CARGO_PKG_VERSION").into(),
            uid: self.settings.uid,
            data_dir: self.settings.data_dir.clone(),
            running,
            tun_active,
            pid: self.process.as_ref().and_then(Process::pid),
            generation: self.generation,
            revision: self.revision.clone(),
            device,
            firewall_error: self.firewall_error.clone(),
            max_frame: Some(MAX_FRAME),
        }
    }
    async fn stop(&mut self) -> Result<()> {
        if let Some(p) = &mut self.process {
            p.stop().await?;
        }
        self.process = None;
        self.revision.clear();
        self.retire_rules(true).await;
        // Best effort: the core and its device are gone. A failure stays in
        // `firewall_error`, which housekeeping retries every two seconds.
        let _ = self.update_firewall(None).await;
        Ok(())
    }

    async fn update_firewall(&mut self, device: Option<&str>) -> Result<()> {
        let (new_firewall, uid) = (self.new_firewall, self.settings.uid);
        let result = async { new_firewall(uid)?.reconcile(device).await }.await;
        let error = result.as_ref().err().map(|e| format!("{e:#}"));
        if error != self.firewall_error
            && let Some(error) = &error
        {
            eprintln!("omash-tun: firewall: {error}");
        }
        self.firewall_error = error;
        result
    }

    /// Runs independently of status clients, so a UFW reload is healed even while
    /// the supervisor is idle. Dead cores lose their exceptions promptly.
    async fn maintain_firewall(&mut self) -> Result<()> {
        if self.process.as_mut().is_some_and(|p| !p.running()) {
            self.process = None;
            self.revision.clear();
            self.retire_rules(false).await;
        }
        let device = self
            .process
            .as_ref()
            .filter(|p| p.kernel_stack)
            .and_then(|p| p.device.clone());
        self.update_firewall(device.as_deref()).await
    }
    /// The core ended: its rules are remembered as leftovers until a conflict check removes them.
    async fn retire_rules(&mut self, graceful: bool) {
        if self.routes.has_rules() {
            // A core that was stopped normally has usually removed its rules already.
            let present = if graceful {
                Snapshot::read().await.ok()
            } else {
                None
            };
            self.routes.retire_into(&mut self.residue, present.as_ref());
            self.persist();
        }
        self.routes = Snapshot::default();
    }
    /// Best effort: losing the ledger only delays the cleanup of a crash residue.
    fn persist(&mut self) {
        let key = (self.routes.rules().clone(), self.residue.clone());
        if self.persisted.as_ref() == Some(&key) {
            return;
        }
        let path = ledger_path(&self.settings.data_dir);
        match Identity::current().and_then(|id| routes::remember(&path, &id, &key.0, &key.1)) {
            Ok(()) => self.persisted = Some(key),
            Err(error) => eprintln!("omash-tun: cannot record leftover policy rules: {error:#}"),
        }
    }
    /// Removes leftovers of ended cores, then returns the snapshot for the conflict check.
    async fn heal_rules(&mut self) -> Result<Snapshot> {
        let snapshot =
            routes::heal(Snapshot::read().await?, &self.routes, &mut self.residue).await?;
        self.persist();
        Ok(snapshot)
    }
    fn check_owner(&self, owner: &str, generation: Option<u64>) -> Result<()> {
        check_owner(self.owner.as_deref(), owner, self.generation, generation)
    }
    async fn apply(&mut self, yaml: String, owner: &str, validate_only: bool) -> Result<()> {
        self.check_owner(owner, None)?;
        super::install::trusted_core()?;
        check_capabilities()?;
        let spec = Spec {
            core: crate::profiles::CoreKind::Mihomo,
            data_dir: self.settings.data_dir.clone(),
            text: yaml,
        };
        let device = spec
            .tun_device()?
            .context("service only accepts enabled Mihomo TUN configs")?;
        let mut root: serde_yaml_ng::Mapping = serde_yaml_ng::from_str(&spec.text)?;
        let ipv6 = root
            .get("ipv6")
            .and_then(serde_yaml_ng::Value::as_bool)
            .unwrap_or(false);
        super::config::apply(&mut root, true, ipv6)?;
        if super::firewall::kernel_stack(&root) {
            (self.new_firewall)(self.settings.uid)?.check().await?;
        }
        let (controller, secret) = spec.controller()?;
        let address: std::net::SocketAddr = controller
            .parse()
            .context("TUN controller must be a loopback IP:port")?;
        if !address.ip().is_loopback() || secret.is_empty() {
            bail!("TUN controller requires a loopback address and a nonempty secret");
        }
        if self.process.as_mut().is_some_and(|p| !p.running()) {
            self.process = None;
            self.revision.clear();
            self.retire_rules(false).await;
        }
        let old_device = self.process.as_ref().and_then(|p| p.device.clone());
        if old_device.as_deref() != Some(&device)
            && super::process::device_exists(Some(&device)).await
        {
            bail!("TUN device already exists; inspect `omash tun doctor` before retrying");
        }
        self.heal_rules().await?.check(&root, &self.routes)?;
        let dir = self.settings.data_dir.join("tun-service");
        fs::create_dir_all(&dir)?;
        let digest = revision(spec.text.as_bytes());
        // The preflight on this lease already validated these exact bytes.
        let validated = take_validation(&mut self.validated, owner, &digest) && !validate_only;
        if !validated {
            let pending = dir.join("pending.yaml");
            let validation = spec.validate(&pending).await;
            let _ = fs::remove_file(&pending);
            validation?;
        }
        if validate_only {
            self.validated = Some((owner.to_owned(), digest));
            return Ok(());
        }
        self.owner = Some(owner.to_owned());
        let previous = self.process.as_ref().map(|p| p.spec.clone());
        self.stop().await?;
        let active = dir.join("active.yaml");
        match self.start_process(spec, &active).await {
            Ok(()) => {
                self.revision = digest;
                self.generation += 1;
                Ok(())
            }
            Err(error) => {
                // Startup and route capture form a single transaction.
                self.stop()
                    .await
                    .with_context(|| format!("TUN apply failed: {error:#}; cleanup failed"))?;
                let restored = match previous {
                    Some(previous) => match self.start_process(previous.clone(), &active).await {
                        Ok(()) => {
                            self.revision = revision(previous.text.as_bytes());
                            true
                        }
                        Err(rollback) => {
                            self.stop().await.with_context(|| {
                                format!("TUN rollback failed: {rollback:#}; cleanup failed")
                            })?;
                            bail!("TUN apply failed: {error:#}; rollback failed: {rollback:#}")
                        }
                    },
                    None => false,
                };
                Err(error).context(if restored {
                    "TUN apply failed; previous generation restored"
                } else {
                    "TUN apply failed; no previous generation to restore"
                })
            }
        }
    }

    async fn start_process(&mut self, spec: Spec, active: &Path) -> Result<()> {
        let root = serde_yaml_ng::from_str(&spec.text)?;
        let device = spec.tun_device()?.context("missing TUN device")?;
        // Recheck after the old core exits; rules may have changed during validation.
        let before = self.heal_rules().await?;
        before.check(&root, &Snapshot::default())?;
        self.update_firewall(super::firewall::kernel_stack(&root).then_some(device.as_str()))
            .await?;
        match Process::start(spec, active).await {
            Ok(process) => self.process = Some(process),
            Err(error) => {
                self.remember_unclean_exit(&before, &root, &device).await;
                return Err(error);
            }
        }
        let (owned, foreign) = Snapshot::read().await?.added_by(&before, &root, &device);
        self.adopt(owned, &foreign)
    }

    /// Records what a core that just started added. Leftovers that healing kept (they share a
    /// priority with something else) stay remembered, whatever this configuration's scope.
    fn adopt(&mut self, owned: Snapshot, foreign: &Rules) -> Result<()> {
        self.routes = owned;
        self.persist();
        // The caller stops the core; its own rules stay recorded until then.
        if foreign.iter().any(|rules| !rules.is_empty()) {
            bail!(
                "TUN policy routing conflicts with rules added while the core started; inspect `omash tun doctor`"
            );
        }
        Ok(())
    }

    /// A core that failed during startup may have been killed before it removed its rules.
    async fn remember_unclean_exit(&mut self, before: &Snapshot, root: &Mapping, device: &str) {
        if let Ok(now) = Snapshot::read().await {
            now.added_by(before, root, device)
                .0
                .retire_into(&mut self.residue, None);
            self.persist();
        }
    }
}

/// A validate-only request just approved these exact bytes for this owner; any other request
/// validates again.
fn take_validation(slot: &mut Option<(String, String)>, owner: &str, revision: &str) -> bool {
    slot.take()
        .is_some_and(|(approved_by, approved)| approved_by == owner && approved == revision)
}

fn ledger_path(data_dir: &Path) -> PathBuf {
    data_dir.join("tun-service").join(routes::LEDGER)
}

/// Housekeeping at startup: forget what earlier helpers left in the data directory, and
/// recall the rules of a core that died with them.
fn recover(data_dir: &Path) -> Rules {
    // Earlier helpers persisted every generation here, but nothing ever read it.
    match fs::remove_file(data_dir.join("tun-service/last-good.yaml")) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            eprintln!("omash-tun: cannot remove last-good.yaml: {error}");
        }
        _ => {}
    }
    let identity = Identity::current()
        .inspect_err(|error| eprintln!("omash-tun: cannot identify boot and network: {error:#}"))
        .ok();
    routes::recall(&ledger_path(data_dir), identity.as_ref())
}

fn check_owner(
    owner: Option<&str>,
    caller: &str,
    current: u64,
    generation: Option<u64>,
) -> Result<()> {
    if owner.is_some_and(|v| v != caller) {
        bail!("TUN generation belongs to another supervisor; wait for its lease to close");
    }
    if generation.is_some_and(|v| v != current) {
        bail!("stale TUN generation");
    }
    Ok(())
}
pub fn check_uid(expected: u32, actual: u32) -> Result<()> {
    if expected != actual {
        bail!("TUN peer UID rejected");
    }
    Ok(())
}
fn check_capabilities() -> Result<()> {
    let status = fs::read_to_string("/proc/self/status")?;
    let caps = status
        .lines()
        .find_map(|l| l.strip_prefix("CapEff:\t"))
        .context("missing process capabilities")?;
    if u64::from_str_radix(caps.trim(), 16)? & 0x3000 != 0x3000 {
        bail!("TUN helper lacks CAP_NET_ADMIN/CAP_NET_RAW; run `omash tun setup`");
    }
    if !Path::new("/dev/net/tun").exists() {
        bail!("/dev/net/tun is unavailable");
    }
    Ok(())
}
async fn connection(mut stream: UnixStream, state: Arc<Mutex<State>>, uid: u32) -> Result<()> {
    check_uid(uid, stream.peer_cred()?.uid())?;
    let owner = uuid::Uuid::new_v4().to_string();
    let result: Result<()> = async {
        loop {
            let request: Request =
                tokio::time::timeout(LEASE_TIMEOUT, read_frame(&mut stream)).await??;
            let mut state = state.lock().await;
            let result = match check_version(request.version) {
                Err(e) => Err(e),
                Ok(()) => match request.action {
                    Action::Hello | Action::Status => Ok(()),
                    Action::Start {
                        yaml,
                        validate_only,
                    } => state.apply(yaml, &owner, validate_only).await,
                    Action::Apply { yaml, generation } => {
                        match state.check_owner(&owner, Some(generation)) {
                            Ok(()) => state.apply(yaml, &owner, false).await,
                            Err(e) => Err(e),
                        }
                    }
                    Action::Stop { generation } => {
                        match state.check_owner(&owner, Some(generation)) {
                            Ok(()) => {
                                let r = state.stop().await;
                                if r.is_ok() {
                                    state.owner = None;
                                }
                                r
                            }
                            Err(e) => Err(e),
                        }
                    }
                },
            };
            let reply = Reply {
                version: VERSION,
                id: request.id,
                status: state.status().await,
                error: result.err().map(|e| format!("{e:#}")),
            };
            drop(state);
            tokio::time::timeout(
                std::time::Duration::from_secs(5),
                write_frame(&mut stream, &reply),
            )
            .await??;
        }
    }
    .await;
    let mut state = state.lock().await;
    if state.validated.as_ref().is_some_and(|(v, _)| *v == owner) {
        state.validated = None;
    }
    if state.owner.as_deref() == Some(&owner) {
        let cleanup = state.stop().await;
        state.owner = None;
        // A disconnected owner can never renew its lease. Cleanup failures are
        // retried by housekeeping; do not leave a permanently orphaned owner.
        cleanup?;
    }
    result
}

pub async fn run() -> Result<()> {
    if !cfg!(target_os = "linux") {
        bail!("TUN helper requires Linux/systemd");
    }
    let settings = load_settings()?;
    check_uid(settings.uid, unsafe { libc::geteuid() })?;
    if settings.uid == 0 {
        bail!("TUN helper must not run as root");
    }
    check_capabilities()?;
    let residue = recover(&settings.data_dir);
    // RuntimeDirectory is dedicated to this unit; systemd kills its cgroup before a restart.
    let _ = fs::remove_file(SOCKET);
    let listener = UnixListener::bind(SOCKET)?;
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(SOCKET, fs::Permissions::from_mode(0o600))?;
    }
    let uid = settings.uid;
    let state = Arc::new(Mutex::new(State::new(settings, residue)));
    // systemd has killed the previous unit's entire cgroup before starting us.
    // Remove its tagged exceptions even if no supervisor reconnects. A failure is
    // recorded and retried by housekeeping instead of crash-looping the unit.
    let _ = state.lock().await.update_firewall(None).await;
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut tasks = tokio::task::JoinSet::new();
    let mut firewall_tick = tokio::time::interval(std::time::Duration::from_secs(2));
    firewall_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            accepted = listener.accept() => { let (stream, _) = accepted?; if tasks.len() < 16 { tasks.spawn(connection(stream, Arc::clone(&state), uid)); } },
            _ = tasks.join_next(), if !tasks.is_empty() => {},
            _ = firewall_tick.tick() => {
                let mut state = state.lock().await;
                if state.process.is_some() || state.firewall_error.is_some() {
                    let _ = state.maintain_firewall().await;
                }
            },
            _ = terminate.recv() => break,
            _ = tokio::signal::ctrl_c() => break,
        }
    }
    // Wait for an in-flight apply before closing its lease, preserving graceful core cleanup.
    state.lock().await.stop().await?;
    tasks.abort_all();
    let _ = fs::remove_file(SOCKET);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(target_os = "linux")]
    use serde_json::json;
    #[test]
    fn helper_rejects_wrong_uid() {
        assert!(check_uid(1000, 1001).is_err());
    }
    fn settings(data_dir: &Path) -> Settings {
        Settings {
            uid: 1000,
            gid: 1000,
            data_dir: data_dir.to_owned(),
        }
    }
    #[cfg(target_os = "linux")]
    fn rules() -> Rules {
        [
            vec![json!({"priority": 9000, "src": "all", "table": "2022"})],
            vec![json!({"priority": 9001, "src": "all", "iif": "omash-tun", "goto": 9010})],
        ]
    }

    #[test]
    fn a_preflight_is_consumed_by_exactly_the_next_matching_request() {
        let approved = || Some(("a".to_owned(), "r1".to_owned()));
        let mut slot = approved();
        assert!(take_validation(&mut slot, "a", "r1"));
        assert!(slot.is_none());
        assert!(!take_validation(&mut slot, "a", "r1"));
        // Another owner or other bytes validate again, and the approval is gone either way.
        for (owner, revision) in [("b", "r1"), ("a", "r2")] {
            let mut slot = approved();
            assert!(!take_validation(&mut slot, owner, revision));
            assert!(slot.is_none());
        }
    }

    #[tokio::test]
    async fn stop_survives_firewall_cleanup_failure() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = State::new(settings(dir.path()), Rules::default());
        state.new_firewall = |_| Err(anyhow::anyhow!("nft unavailable"));
        state.owner = Some("owner".into());
        // The core is gone either way; a stale exception must not turn a stop into a failure.
        state.stop().await.unwrap();
        assert!(state.process.is_none());
        let error = state.firewall_error.as_deref().unwrap_or_default();
        assert!(error.contains("nft unavailable"), "{error:?}");
    }

    #[tokio::test]
    async fn status_and_firewall_use_facts_parsed_at_start() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = State::new(settings(dir.path()), Rules::default());
        state.new_firewall = |_| Err(anyhow::anyhow!("nft unavailable"));
        let spec = Spec {
            core: crate::profiles::CoreKind::Mihomo,
            data_dir: dir.path().to_owned(),
            text: "external-controller: 127.0.0.1:9\nsecret: s\ntun: {enable: true, device: omash-tun, stack: mixed}\n".into(),
        };
        let child = tokio::process::Command::new("sleep")
            .arg("60")
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut process = Process {
            device: spec.tun_device().unwrap(),
            kernel_stack: true,
            api: spec.api().unwrap(),
            child,
            spec,
        };
        // Status runs every second and housekeeping every two: neither parses the text again.
        process.spec.text = "not: [yaml".into();
        state.process = Some(process);
        assert_eq!(state.status().await.device.as_deref(), Some("omash-tun"));
        let error = state.maintain_firewall().await.unwrap_err();
        assert!(
            format!("{error:#}").contains("nft unavailable"),
            "{error:#}"
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn rules_of_a_dead_core_are_remembered_across_helper_restarts() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = State::new(settings(dir.path()), Rules::default());
        state.routes = Snapshot::with_rules(rules());
        state.persist();
        // A helper that is restarted while its core runs finds the core gone.
        assert_eq!(recover(dir.path()), rules());
        // The helper notices the death itself: the rules move from owned to leftover.
        state.retire_rules(false).await;
        assert!(!state.routes.has_rules());
        assert_eq!(state.residue, rules());
        assert_eq!(recover(dir.path()), rules());
        // Leftovers that were removed leave nothing to remember.
        state.residue = Rules::default();
        state.persist();
        assert!(!ledger_path(dir.path()).exists());
        assert_eq!(recover(dir.path()), Rules::default());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_ledger_is_only_rewritten_when_it_changes() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = State::new(settings(dir.path()), Rules::default());
        state.residue = rules();
        state.persist();
        let path = ledger_path(dir.path());
        assert!(path.exists());
        fs::remove_file(&path).unwrap();
        state.persist();
        assert!(!path.exists(), "unchanged rules are not written again");
        state.residue[0].clear();
        state.persist();
        assert!(path.exists());
    }

    #[test]
    fn a_start_keeps_leftovers_that_healing_could_not_remove() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = State::new(settings(dir.path()), Rules::default());
        // A crashed configuration's rules, kept because a foreign rule shares their priority.
        state.residue = rules();
        let theirs =
            Snapshot::with_rules([vec![json!({"priority": 11000, "table": "3000"})], vec![]]);
        state.adopt(theirs.clone(), &Rules::default()).unwrap();
        assert_eq!(state.residue, rules());
        assert_eq!(state.routes.rules(), theirs.rules());
        let path = ledger_path(dir.path());
        assert_eq!(
            routes::recall(&path, Identity::current().ok().as_ref())[0].len(),
            rules()[0].len()
        );
        // Foreign additions fail the start, with the core's own rules recorded for the stop.
        let foreign = [vec![json!({"priority": 11005, "table": "100"})], vec![]];
        let error = state.adopt(theirs.clone(), &foreign).unwrap_err();
        assert!(format!("{error:#}").contains("conflicts with rules added while the core started"));
        assert_eq!(state.routes.rules(), theirs.rules());
        assert_eq!(state.residue, rules());
    }

    #[test]
    fn startup_deletes_a_stale_last_good_generation_only() {
        let dir = tempfile::tempdir().unwrap();
        let service = dir.path().join("tun-service");
        fs::create_dir_all(&service).unwrap();
        fs::write(service.join("last-good.yaml"), "secret: x").unwrap();
        fs::write(service.join("active.yaml"), "keep").unwrap();
        assert_eq!(recover(dir.path()), Rules::default());
        assert!(!service.join("last-good.yaml").exists());
        assert!(service.join("active.yaml").exists());
        // Nothing to delete, and not even a data directory, is fine.
        assert_eq!(recover(dir.path()), Rules::default());
        assert_eq!(recover(&dir.path().join("missing")), Rules::default());
    }

    #[test]
    fn second_owner_cannot_replace_active_generation() {
        assert!(check_owner(Some("a"), "b", 1, Some(1)).is_err());
        assert!(check_owner(Some("a"), "a", 1, Some(0)).is_err());
        assert!(check_owner(Some("a"), "a", 1, Some(1)).is_ok());
    }
}
