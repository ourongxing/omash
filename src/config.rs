use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};
use std::{fs, path::PathBuf, time::Duration};

use crate::profiles::CoreKind;

#[derive(Debug, Default, Deserialize)]
struct RuntimeConfig {
    #[serde(rename = "proxy-groups", default)]
    proxy_groups: Vec<RuntimeGroup>,
}

#[derive(Debug, Deserialize)]
struct RuntimeGroup {
    name: String,
}

#[derive(Debug, Parser)]
#[command(version, about)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
    /// Run the internal core supervisor
    #[arg(long, hide = true)]
    pub daemon: bool,
    /// Refresh interval in milliseconds
    #[arg(long, env = "OMASH_REFRESH_MS")]
    pub refresh_ms: Option<u64>,
    /// Alternative configuration file
    #[arg(long)]
    pub config: Option<PathBuf>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Internal status-bar integration
    #[command(hide = true)]
    Bar(BarArgs),
    /// Stop the proxy core and clear the system proxy
    Stop,
    /// Start the proxy core and re-apply the system proxy
    Start,
    /// Restart the proxy core
    Restart,
    /// Manage the optional Linux Mihomo TUN service
    Tun {
        #[command(subcommand)]
        command: TunCommand,
    },
    #[command(hide = true)]
    InternalTunService,
    #[command(hide = true)]
    InternalTunInstall {
        uid: u32,
        gid: u32,
        data_dir: PathBuf,
        digest: String,
    },
    #[command(hide = true)]
    InternalTunUninstall { uid: u32 },
}

#[derive(Debug, Subcommand)]
pub enum TunCommand {
    /// Enable TUN and wait for the requested revision
    On,
    /// Disable TUN and wait for the requested revision
    Off,
    /// Show desired and actual TUN state (without credentials)
    Status {
        #[arg(long, hide = true)]
        check_helper: bool,
    },
    /// Install or update the optional system helper (requires authorization)
    Setup,
    /// Inspect TUN permissions, runtime state and routes
    Doctor,
    /// Stop and remove the optional helper, preserving user data
    Uninstall,
}

#[derive(Debug, Args)]
pub struct BarArgs {
    #[command(subcommand)]
    pub command: BarCommand,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum ProxyMode {
    Rule,
    Global,
    Direct,
}

impl ProxyMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Rule => "rule",
            Self::Global => "global",
            Self::Direct => "direct",
        }
    }
}

#[derive(Debug, Subcommand)]
pub enum BarCommand {
    /// Print status-bar state as JSON
    State,
    /// Change the core routing mode
    Mode { mode: ProxyMode },
    /// Select a proxy in a selector group
    Proxy { group: String, proxy: String },
    /// Test every proxy in a selector group
    Delay { group: String },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct Config {
    pub controller: String,
    pub secret: String,
    pub refresh_ms: u64,
    pub delay_test_url: String,
    pub auto_start: bool,
    pub mixed_port: u16,
    pub allow_lan: bool,
    pub ipv6: bool,
    pub system_proxy: bool,
    pub tun_enabled: bool,
    pub proxy_bypass: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            controller: "http://127.0.0.1:9090".into(),
            secret: String::new(),
            refresh_ms: 1500,
            delay_test_url: "https://www.gstatic.com/generate_204".into(),
            auto_start: true,
            mixed_port: 7897,
            allow_lan: false,
            ipv6: true,
            system_proxy: true,
            tun_enabled: false,
            proxy_bypass: "localhost,127.0.0.1,::1,192.168.0.0/16,10.0.0.0/8,172.16.0.0/12".into(),
        }
    }
}

impl Config {
    pub fn load(cli: &Cli) -> Result<Self> {
        let path = cli.config.clone().unwrap_or_else(Self::default_path);
        let (mut value, legacy_fields) = if path.exists() {
            let text = fs::read_to_string(&path)
                .with_context(|| format!("failed to read {}", path.display()))?;
            let legacy_fields = toml::from_str::<toml::Value>(&text)
                .ok()
                .and_then(|document| document.as_table().cloned())
                .is_some_and(|table| {
                    table.contains_key("manage_core")
                        || table.contains_key("mihomo_path")
                        || table.contains_key("tun")
                });
            (
                toml::from_str(&text)
                    .with_context(|| format!("invalid config in {}", path.display()))?,
                legacy_fields,
            )
        } else {
            (Self::default(), false)
        };
        let needs_secret = value.secret.is_empty();
        if needs_secret {
            value.secret = uuid::Uuid::new_v4().simple().to_string();
        }
        if let Some(refresh_ms) = cli.refresh_ms {
            value.refresh_ms = refresh_ms;
        }
        value.controller = value.controller.trim_end_matches('/').to_owned();
        value.ensure_dirs()?;
        if needs_secret || legacy_fields || !path.exists() {
            value.save_to(&path)?;
        }
        Self::secure_config_permissions(&path)?;
        Ok(value)
    }

    pub fn refresh_interval(&self) -> Duration {
        Duration::from_millis(self.refresh_ms.max(250))
    }

    pub fn default_path() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("omash/config.toml")
    }

    /// `tun_enabled` as config.toml holds it now, if the file can be read.
    pub fn saved_tun_enabled() -> Option<bool> {
        let text = fs::read_to_string(Self::default_path()).ok()?;
        toml::from_str::<Self>(&text)
            .ok()
            .map(|saved| saved.tun_enabled)
    }

    pub fn data_dir() -> PathBuf {
        dirs::data_local_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("omash")
    }

    pub fn core_path(core: CoreKind) -> PathBuf {
        PathBuf::from(match core {
            CoreKind::Mihomo => "/usr/bin/mihomo",
            CoreKind::Singbox => "/usr/bin/sing-box",
        })
    }

    pub fn profiles_dir() -> PathBuf {
        Self::data_dir().join("profiles")
    }

    pub fn runtime_path_for(core: CoreKind) -> PathBuf {
        Self::data_dir().join(format!("runtime.{}", core.extension()))
    }

    pub fn proxy_group_order() -> Vec<String> {
        let singbox_runtime = Self::runtime_path_for(CoreKind::Singbox);
        if singbox_runtime.is_file() {
            return fs::read_to_string(singbox_runtime)
                .ok()
                .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
                .map_or_else(Vec::new, |value| singbox_group_order(&value));
        }
        fs::read_to_string(Self::runtime_path_for(CoreKind::Mihomo))
            .ok()
            .and_then(|text| serde_yaml_ng::from_str::<RuntimeConfig>(&text).ok())
            .map(|config| {
                config
                    .proxy_groups
                    .into_iter()
                    .map(|group| group.name)
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn profiles_path() -> PathBuf {
        Self::data_dir().join("profiles.yaml")
    }

    pub fn logs_dir() -> PathBuf {
        Self::data_dir().join("logs")
    }

    pub fn backups_dir() -> PathBuf {
        Self::data_dir().join("backups")
    }

    pub fn disabled_state_path() -> PathBuf {
        Self::data_dir().join("core-disabled")
    }

    pub fn restart_request_path() -> PathBuf {
        Self::data_dir().join("restart-request")
    }

    pub fn supervisor_state_path() -> PathBuf {
        Self::data_dir().join("supervisor-state.json")
    }

    pub fn save(&self) -> Result<()> {
        self.save_to(&Self::default_path())
    }

    fn save_to(&self, path: &std::path::Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        crate::tun::process::private_write(path, toml::to_string_pretty(self)?.as_bytes())
            .with_context(|| format!("failed to write {}", path.display()))?;
        Self::secure_config_permissions(path)?;
        Ok(())
    }

    fn secure_config_permissions(path: &std::path::Path) -> Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            // Even a no-op chmod bumps the ctime, and the supervisor loads this every second.
            if fs::metadata(path)?.permissions().mode() & 0o7777 != 0o600 {
                fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
            }
        }
        Ok(())
    }

    fn ensure_dirs(&self) -> Result<()> {
        for dir in [
            Self::data_dir(),
            Self::profiles_dir(),
            Self::logs_dir(),
            Self::backups_dir(),
        ] {
            fs::create_dir_all(&dir)
                .with_context(|| format!("failed to create {}", dir.display()))?;
        }
        Ok(())
    }
}

fn singbox_group_order(config: &serde_json::Value) -> Vec<String> {
    config
        .get("outbounds")
        .and_then(serde_json::Value::as_array)
        .map(|outbounds| {
            outbounds
                .iter()
                .filter(|outbound| {
                    outbound
                        .get("type")
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|kind| matches!(kind, "selector" | "urltest"))
                })
                .filter_map(|outbound| {
                    outbound
                        .get("tag")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned)
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refresh_cli_overrides_file_value() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            "controller = 'http://old:1/'\nsecret = 'key'\nrefresh_ms = 99\n",
        )
        .unwrap();
        let cli = Cli {
            command: None,
            daemon: false,
            refresh_ms: None,
            config: Some(path),
        };
        let config = Config::load(&cli).unwrap();
        assert_eq!(config.controller, "http://old:1");
        assert_eq!(config.secret, "key");
        assert_eq!(config.refresh_ms, 99);
        assert_eq!(config.refresh_interval(), Duration::from_millis(250));
        assert!(config.system_proxy);
    }

    #[cfg(unix)]
    #[test]
    fn config_permissions_are_changed_only_when_needed() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "secret = 'key'\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        Config::secure_config_permissions(&path).unwrap();
        let mode = |path: &PathBuf| fs::metadata(path).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode(&path), 0o600);
        // A chmod bumps the ctime even when the mode is unchanged.
        let ctime = |path: &PathBuf| {
            let meta = fs::metadata(path).unwrap();
            (meta.ctime(), meta.ctime_nsec())
        };
        let before = ctime(&path);
        std::thread::sleep(Duration::from_millis(30));
        Config::secure_config_permissions(&path).unwrap();
        assert_eq!(ctime(&path), before);
        // Stray special bits are still removed, and a missing file is still an error.
        fs::set_permissions(&path, fs::Permissions::from_mode(0o1600)).unwrap();
        Config::secure_config_permissions(&path).unwrap();
        assert_eq!(mode(&path), 0o600);
        assert!(Config::secure_config_permissions(&dir.path().join("missing.toml")).is_err());
    }

    #[test]
    fn removes_legacy_external_core_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            "manage_core = false\nmihomo_path = '/tmp/mihomo'\ntun = true\nsecret = 'key'\n",
        )
        .unwrap();
        let config = Config::load(&Cli {
            command: None,
            daemon: false,
            refresh_ms: None,
            config: Some(path.clone()),
        })
        .unwrap();
        let migrated = fs::read_to_string(path).unwrap();
        assert!(!migrated.contains("manage_core"));
        assert!(!migrated.contains("mihomo_path"));
        assert!(!migrated.lines().any(|line| line.starts_with("tun =")));
        assert!(!config.tun_enabled);
    }

    #[test]
    fn singbox_group_order_tracks_outbound_declaration() {
        let config: serde_json::Value = serde_json::from_str(
            r#"{"outbounds":[
                {"type":"selector","tag":"AI"},
                {"type":"direct","tag":"direct"},
                {"type":"urltest","tag":"Auto"},
                {"type":"block","tag":"block"}
            ]}"#,
        )
        .unwrap();
        assert_eq!(singbox_group_order(&config), vec!["AI", "Auto"]);
        assert!(singbox_group_order(&serde_json::json!({})).is_empty());
    }
}
