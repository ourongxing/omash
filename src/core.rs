use crate::{api::MihomoClient, config::Config, profiles::Profiles};
use anyhow::{Context, Result, bail};
use chrono::Local;
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, OpenOptions},
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, Instant, SystemTime},
};
use tokio::process::{Child, Command};

const VALIDATION_TIMEOUT: Duration = Duration::from_secs(30);
const SIDECAR_READINESS_ATTEMPTS: usize = 30;
const SIDECAR_READINESS_INTERVAL: Duration = Duration::from_millis(100);
const SIDECAR_READINESS_PROBE_TIMEOUT: Duration = Duration::from_millis(400);
const START_RETRY_BACKOFF: Duration = Duration::from_secs(5);

pub enum ConfigApply {
    Reloaded,
    Restarted,
}

pub struct CoreManager {
    child: Option<Child>,
    child_path: Option<PathBuf>,
}

impl CoreManager {
    pub const fn new() -> Self {
        Self {
            child: None,
            child_path: None,
        }
    }

    pub async fn validate(&self, config: &Config, profiles: &Profiles) -> Result<()> {
        self.validate_runtime(config, profiles, true).await
    }

    pub async fn validate_only(&self, config: &Config, profiles: &Profiles) -> Result<()> {
        self.validate_runtime(config, profiles, false).await
    }

    async fn validate_runtime(
        &self,
        config: &Config,
        profiles: &Profiles,
        commit: bool,
    ) -> Result<()> {
        ensure_core_resources()?;
        let runtime = Config::runtime_path();
        let nonce = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let staged = runtime.with_file_name(format!(
            "runtime.pending-{}-{nonce}.yaml",
            std::process::id()
        ));
        profiles.build_runtime_at(config, &staged)?;
        let mut command = Command::new(Config::mihomo_path());
        command
            .args(["-t", "-d"])
            .arg(Config::data_dir())
            .arg("-f")
            .arg(&staged)
            .kill_on_drop(true);
        let output = match tokio::time::timeout(VALIDATION_TIMEOUT, command.output()).await {
            Ok(Ok(output)) => output,
            Ok(Err(error)) => {
                let _ = fs::remove_file(&staged);
                return Err(error).context("failed to execute Mihomo validator");
            }
            Err(_) => {
                let _ = fs::remove_file(&staged);
                bail!("Mihomo configuration validation timed out after 30 seconds");
            }
        };
        if !output.status.success() {
            let _ = fs::remove_file(&staged);
            bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
        }
        if commit {
            fs::rename(&staged, &runtime).with_context(|| {
                format!("failed to commit validated runtime {}", runtime.display())
            })?;
        } else {
            fs::remove_file(&staged)?;
        }
        Ok(())
    }

    pub async fn start(&mut self, config: &Config, profiles: &Profiles) -> Result<()> {
        if self.is_sidecar_running() {
            return Ok(());
        }
        self.validate(config, profiles).await?;
        self.start_validated(config, profiles).await
    }

    async fn start_validated(&mut self, config: &Config, profiles: &Profiles) -> Result<()> {
        let log_path =
            Config::logs_dir().join(format!("mihomo-{}.log", Local::now().format("%Y-%m-%d")));
        let stdout = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)?;
        let stderr = stdout.try_clone()?;
        let mut child = Command::new(Config::mihomo_path())
            .arg("-d")
            .arg(Config::data_dir())
            .arg("-f")
            .arg(Config::runtime_path())
            .stdin(Stdio::null())
            .stdout(stdout)
            .stderr(stderr)
            .kill_on_drop(true)
            .spawn()
            .context("failed to start Mihomo")?;

        let api = MihomoClient::new(&config.controller, config.secret.clone())?;
        let mut last_error = "Mihomo API did not answer".to_owned();
        let mut ready = false;
        for attempt in 0..SIDECAR_READINESS_ATTEMPTS {
            if let Some(status) = child.try_wait()? {
                bail!(
                    "Mihomo exited during startup with {status}; see {}",
                    log_path.display()
                );
            }
            match tokio::time::timeout(SIDECAR_READINESS_PROBE_TIMEOUT, api.version()).await {
                Ok(Ok(_)) => {
                    ready = true;
                    break;
                }
                Ok(Err(error)) => last_error = error.to_string(),
                Err(_) => last_error = "readiness probe timed out".into(),
            }
            if attempt + 1 < SIDECAR_READINESS_ATTEMPTS {
                tokio::time::sleep(SIDECAR_READINESS_INTERVAL).await;
            }
        }
        if !ready {
            let _ = child.start_kill();
            let _ = tokio::time::timeout(Duration::from_secs(3), child.wait()).await;
            bail!(
                "Mihomo API did not become ready: {last_error}; see {}",
                log_path.display()
            );
        }
        self.child = Some(child);
        self.child_path = Some(Config::mihomo_path());
        let _ = restore_selected_nodes(config, profiles).await;
        Ok(())
    }

    pub async fn stop(&mut self) -> Result<()> {
        if let Some(mut child) = self.child.take() {
            child.start_kill()?;
            let _ = tokio::time::timeout(std::time::Duration::from_secs(3), child.wait()).await;
        }
        self.child_path = None;
        Ok(())
    }

    pub async fn restart(&mut self, config: &Config, profiles: &Profiles) -> Result<ConfigApply> {
        self.validate(config, profiles).await?;
        if self.child_path.as_ref() != Some(&Config::mihomo_path()) {
            self.stop().await?;
            self.start_validated(config, profiles).await?;
            return Ok(ConfigApply::Restarted);
        }
        let api = MihomoClient::new(&config.controller, config.secret.clone())?;
        if api.reload_config(&Config::runtime_path()).await.is_ok() {
            let _ = restore_selected_nodes(config, profiles).await;
            return Ok(ConfigApply::Reloaded);
        }
        self.stop().await?;
        self.start_validated(config, profiles).await?;
        Ok(ConfigApply::Restarted)
    }

    pub fn is_sidecar_running(&mut self) -> bool {
        self.child
            .as_mut()
            .is_some_and(|child| matches!(child.try_wait(), Ok(None)))
    }

    pub fn pid(&self) -> Option<u32> {
        self.child.as_ref().and_then(Child::id)
    }

    pub fn recent_logs(limit: usize) -> Result<Vec<String>> {
        let Some(path) = fs::read_dir(Config::logs_dir())?
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "log"))
            .max_by_key(|path| fs::metadata(path).and_then(|m| m.modified()).ok())
        else {
            return Ok(vec![]);
        };
        let text = fs::read_to_string(path)?;
        let mut lines: Vec<_> = text.lines().rev().take(limit).map(str::to_owned).collect();
        lines.reverse();
        Ok(lines)
    }
}

pub async fn ensure_managed_core() -> Result<bool> {
    let destination = Config::mihomo_path();
    if destination.is_file() {
        return Ok(false);
    }
    let source = resolve_executable(Path::new("mihomo")).context(
        "cannot find Mihomo for first-time installation; install the mihomo package first",
    )?;
    let parent = destination
        .parent()
        .ok_or_else(|| anyhow::anyhow!("managed core path has no parent"))?;
    fs::create_dir_all(parent)?;
    let staged = parent.join(format!("mihomo.pending-{}", std::process::id()));
    fs::copy(&source, &staged)
        .with_context(|| format!("failed to stage managed Mihomo from {}", source.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(&staged, fs::Permissions::from_mode(0o755))?;
    }
    let output = Command::new(&staged)
        .arg("-v")
        .output()
        .await
        .context("failed to validate managed Mihomo executable")?;
    if !output.status.success() {
        let _ = fs::remove_file(&staged);
        bail!(
            "managed Mihomo validation failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    fs::rename(&staged, &destination)?;
    Ok(true)
}

fn resolve_executable(command: &Path) -> Option<PathBuf> {
    if command.components().count() > 1 {
        return command.is_file().then(|| command.to_path_buf());
    }
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|directory| directory.join(command))
            .find(|candidate| candidate.is_file())
    })
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
    let data_dir = Config::data_dir();
    let search_dirs = [
        PathBuf::from("/etc/mihomo"),
        PathBuf::from("/etc/clash"),
        PathBuf::from("/usr/share/mihomo"),
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("mihomo"),
    ];
    for name in ["Country.mmdb", "GeoSite.dat", "GeoIP.dat", "geoip.metadb"] {
        let destination = data_dir.join(name);
        if destination.exists() {
            continue;
        }
        let Some(source) = search_dirs
            .iter()
            .map(|directory| directory.join(name))
            .find(|path| path.is_file())
        else {
            continue;
        };
        link_or_copy(&source, &destination).with_context(|| {
            format!(
                "failed to install Mihomo resource {} from {}",
                destination.display(),
                source.display()
            )
        })?;
    }
    Ok(())
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

pub async fn run_supervisor(mut config: Config) -> Result<()> {
    let mut manager = CoreManager::new();
    let mut fingerprint = 0;
    let mut state = SupervisorState::default();
    let mut proxy_applied = false;
    let mut last_start_attempt: Option<Instant> = None;
    loop {
        let enabled = core_desired_enabled();
        let profiles = Profiles::load().unwrap_or_default();
        let current_fingerprint = configuration_fingerprint();
        let restart_requested = Config::restart_request_path().exists();
        let replace_requested = Config::replace_request_path().exists();

        if !enabled || profiles.items.is_empty() {
            if proxy_applied {
                let _ = apply_system_proxy(&config, false).await;
                proxy_applied = false;
            }
            if manager.is_sidecar_running() {
                manager.stop().await?;
            }
            state.running = false;
            state.pid = None;
            state.error = profiles
                .items
                .is_empty()
                .then(|| "waiting for a profile".into());
        } else if !manager.is_sidecar_running() {
            let retry_due =
                last_start_attempt.is_none_or(|attempt| attempt.elapsed() >= START_RETRY_BACKOFF);
            if retry_due {
                if proxy_applied {
                    let _ = apply_system_proxy(&config, false).await;
                    proxy_applied = false;
                }
                state.running = false;
                state.pid = None;
                state.error = Some("starting Mihomo".into());
                write_supervisor_state(&state)?;
                last_start_attempt = Some(Instant::now());
                match manager.start(&config, &profiles).await {
                    Ok(()) => {
                        state.restarts = state.restarts.saturating_add(1);
                        state.error = None;
                        last_start_attempt = None;
                        if config.system_proxy {
                            match apply_system_proxy(&config, true).await {
                                Ok(()) => proxy_applied = true,
                                Err(error) => {
                                    state.error =
                                        Some(format!("core running; system proxy failed: {error}"))
                                }
                            }
                        }
                    }
                    Err(error) => state.error = Some(error.to_string()),
                }
            }
        } else if fingerprint != 0
            && (fingerprint != current_fingerprint || restart_requested || replace_requested)
        {
            if proxy_applied {
                let _ = apply_system_proxy(&config, false).await;
                proxy_applied = false;
            }
            let result = if replace_requested {
                async {
                    manager.validate(&config, &profiles).await?;
                    manager.stop().await?;
                    manager.start_validated(&config, &profiles).await?;
                    Ok(ConfigApply::Restarted)
                }
                .await
            } else {
                manager.restart(&config, &profiles).await
            };
            match result {
                Ok(outcome) => {
                    match outcome {
                        ConfigApply::Reloaded => state.reloads = state.reloads.saturating_add(1),
                        ConfigApply::Restarted => state.restarts = state.restarts.saturating_add(1),
                    }
                    state.error = None;
                    if config.system_proxy {
                        match apply_system_proxy(&config, true).await {
                            Ok(()) => proxy_applied = true,
                            Err(error) => {
                                state.error =
                                    Some(format!("core running; system proxy failed: {error}"))
                            }
                        }
                    } else if proxy_applied {
                        let _ = apply_system_proxy(&config, false).await;
                        proxy_applied = false;
                    }
                }
                Err(error) => state.error = Some(error.to_string()),
            }
        }
        if restart_requested {
            let _ = fs::remove_file(Config::restart_request_path());
        }
        if replace_requested {
            let _ = fs::remove_file(Config::replace_request_path());
        }
        state.running = manager.is_sidecar_running();
        state.pid = manager.pid();
        write_supervisor_state(&state)?;
        fingerprint = current_fingerprint;
        if wait_or_shutdown(Duration::from_secs(1)).await {
            break;
        }
        if let Ok(latest) = load_daemon_config() {
            config = latest;
        }
    }
    if proxy_applied {
        let _ = apply_system_proxy(&config, false).await;
    }
    manager.stop().await?;
    state.running = false;
    state.pid = None;
    write_supervisor_state(&state)?;
    Ok(())
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
        fs::write(path, b"disabled by user\n")?;
    }
    request_restart()
}

pub fn core_desired_enabled() -> bool {
    !Config::disabled_state_path().exists()
}

pub fn request_restart() -> Result<()> {
    fs::write(
        Config::restart_request_path(),
        format!("{}\n", Local::now().timestamp()),
    )?;
    Ok(())
}

pub fn request_replace() -> Result<()> {
    fs::write(
        Config::replace_request_path(),
        format!("{}\n", Local::now().timestamp()),
    )?;
    Ok(())
}

fn load_daemon_config() -> Result<Config> {
    Config::load(&crate::config::Cli {
        command: None,
        daemon: true,
        refresh_ms: None,
        config: None,
    })
}

fn configuration_fingerprint() -> u128 {
    let mut paths = vec![Config::default_path(), Config::profiles_path()];
    if let Ok(entries) = fs::read_dir(Config::profiles_dir()) {
        paths.extend(entries.filter_map(Result::ok).map(|entry| entry.path()));
    }
    paths
        .into_iter()
        .map(|path| modified_nanos(&path))
        .fold(0, u128::wrapping_add)
}

fn modified_nanos(path: &Path) -> u128 {
    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map_or(0, |duration| duration.as_nanos())
}

fn write_supervisor_state(state: &SupervisorState) -> Result<()> {
    let path = Config::supervisor_state_path();
    let temporary = path.with_extension("tmp");
    fs::write(&temporary, serde_json::to_vec(state)?)?;
    fs::rename(temporary, path)?;
    Ok(())
}

async fn user_systemctl(arguments: &[&str]) -> Result<()> {
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
    Ok(())
}

#[cfg(unix)]
async fn wait_or_shutdown(duration: Duration) -> bool {
    use tokio::signal::unix::{SignalKind, signal};
    let Ok(mut terminate) = signal(SignalKind::terminate()) else {
        tokio::time::sleep(duration).await;
        return false;
    };
    tokio::select! {
        () = tokio::time::sleep(duration) => false,
        _ = terminate.recv() => true,
        result = tokio::signal::ctrl_c() => result.is_ok(),
    }
}

#[cfg(not(unix))]
async fn wait_or_shutdown(duration: Duration) -> bool {
    tokio::select! {
        () = tokio::time::sleep(duration) => false,
        result = tokio::signal::ctrl_c() => result.is_ok(),
    }
}

impl Drop for CoreManager {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.start_kill();
        }
    }
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
