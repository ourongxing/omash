use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};
use std::{fs, path::PathBuf, time::Duration};

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
    /// Change the Mihomo routing mode
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

    pub fn data_dir() -> PathBuf {
        dirs::data_local_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("omash")
    }

    pub fn mihomo_path() -> PathBuf {
        PathBuf::from("/usr/bin/mihomo")
    }

    pub fn profiles_dir() -> PathBuf {
        Self::data_dir().join("profiles")
    }

    pub fn runtime_path() -> PathBuf {
        Self::data_dir().join("runtime.yaml")
    }

    pub fn proxy_group_order() -> Vec<String> {
        fs::read_to_string(Self::runtime_path())
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
        fs::write(path, toml::to_string_pretty(self)?)
            .with_context(|| format!("failed to write {}", path.display()))?;
        Self::secure_config_permissions(path)?;
        Ok(())
    }

    fn secure_config_permissions(path: &std::path::Path) -> Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
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

    #[test]
    fn removes_legacy_external_core_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            "manage_core = false\nmihomo_path = '/tmp/mihomo'\ntun = true\nsecret = 'key'\n",
        )
        .unwrap();
        Config::load(&Cli {
            command: None,
            daemon: false,
            refresh_ms: None,
            config: Some(path.clone()),
        })
        .unwrap();
        let migrated = fs::read_to_string(path).unwrap();
        assert!(!migrated.contains("manage_core"));
        assert!(!migrated.contains("mihomo_path"));
        assert!(!migrated.contains("tun"));
    }
}
