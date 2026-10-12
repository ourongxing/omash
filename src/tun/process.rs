//! Shared, bounded process lifecycle. Runtime files are private and committed only after readiness.
use crate::{
    api::{MihomoClient, RuntimeConfig},
    profiles::CoreKind,
};
use anyhow::{Context, Result, anyhow, bail};
use serde_yaml_ng::Value;
use std::{
    fs::{self, OpenOptions},
    io::Write,
    net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV6},
    path::{Path, PathBuf},
    process::{Output, Stdio},
    time::Duration,
};
use tokio::process::{Child, Command};

#[derive(Clone)]
pub struct Spec {
    pub core: CoreKind,
    pub data_dir: PathBuf,
    pub text: String,
}

impl Spec {
    pub fn mode(&self) -> Result<String> {
        match self.core {
            CoreKind::Mihomo => {
                let root: Value = serde_yaml_ng::from_str(&self.text)?;
                Ok(root["mode"].as_str().unwrap_or("rule").to_ascii_lowercase())
            }
            CoreKind::Singbox => {
                let root: serde_json::Value = serde_json::from_str(&self.text)?;
                Ok(root["experimental"]["clash_api"]["default_mode"]
                    .as_str()
                    .unwrap_or("rule")
                    .to_ascii_lowercase())
            }
        }
    }

    pub fn set_mode(&mut self, mode: &str) -> Result<()> {
        if !matches!(mode, "rule" | "global" | "direct") || self.mode()? == mode {
            return Ok(());
        }
        match self.core {
            CoreKind::Mihomo => {
                let mut root: serde_yaml_ng::Mapping = serde_yaml_ng::from_str(&self.text)?;
                root.insert("mode".into(), mode.into());
                self.text = serde_yaml_ng::to_string(&root)?;
            }
            CoreKind::Singbox => {
                let mut root: serde_json::Value = serde_json::from_str(&self.text)?;
                root["experimental"]["clash_api"]["default_mode"] = mode.into();
                self.text = serde_json::to_string_pretty(&root)?;
            }
        }
        Ok(())
    }

    pub fn controller(&self) -> Result<(String, String)> {
        match self.core {
            CoreKind::Mihomo => mihomo_controller(&serde_yaml_ng::from_str(&self.text)?),
            CoreKind::Singbox => {
                let v: serde_json::Value = serde_json::from_str(&self.text)?;
                let v = &v["experimental"]["clash_api"];
                Ok((
                    v["external_controller"]
                        .as_str()
                        .context("missing controller")?
                        .into(),
                    v["secret"].as_str().unwrap_or("").into(),
                ))
            }
        }
    }

    pub fn tun_device(&self) -> Result<Option<String>> {
        if self.core != CoreKind::Mihomo {
            return Ok(None);
        }
        mihomo_tun_device(&serde_yaml_ng::from_str(&self.text)?)
    }

    pub fn api(&self) -> Result<MihomoClient> {
        let (controller, secret) = self.controller()?;
        controller_client(&controller, secret)
    }

    fn command(&self, path: &Path, validate: bool) -> Command {
        let mut c = Command::new(crate::config::Config::core_path(self.core));
        // Every core, Direct or the helper's, gets only PATH and HOME: desktop proxy variables
        // and dynamic-loader settings must not reach a privileged core, and the supervisor's
        // preflight and the helper's validation must judge a configuration alike.
        c.env_clear().env("PATH", "/usr/bin:/bin");
        if let Some(home) = dirs::home_dir() {
            c.env("HOME", home);
        }
        c.current_dir(&self.data_dir);
        match self.core {
            CoreKind::Mihomo => {
                if validate {
                    c.arg("-t");
                }
                c.arg("-d").arg(&self.data_dir).arg("-f").arg(path);
            }
            CoreKind::Singbox => {
                c.arg(if validate { "check" } else { "run" })
                    .arg("-c")
                    .arg(path)
                    .arg("-D")
                    .arg(&self.data_dir)
                    .arg("--disable-color");
            }
        }
        c.stdin(Stdio::null()).kill_on_drop(true);
        c
    }

    pub async fn validate(&self, path: &Path) -> Result<()> {
        private_write(path, self.text.as_bytes())?;
        let log = self.data_dir.join("logs/core-validation.log");
        let mut file = private_log(&log)?;
        let output =
            tokio::time::timeout(Duration::from_secs(30), self.command(path, true).output())
                .await
                .context("core validation timed out (30s)")??;
        judge_validation(&mut file, &log, &output)
    }
}

fn mihomo_controller(root: &Value) -> Result<(String, String)> {
    Ok((
        root["external-controller"]
            .as_str()
            .context("missing controller")?
            .into(),
        root["secret"].as_str().unwrap_or("").into(),
    ))
}

fn mihomo_tun_device(root: &Value) -> Result<Option<String>> {
    if root["tun"]["enable"].as_bool() != Some(true) {
        return Ok(None);
    }
    let device = root["tun"]["device"]
        .as_str()
        .context("missing tun.device")?;
    super::config::validate_device(device)?;
    Ok(Some(device.to_owned()))
}

fn controller_client(controller: &str, secret: String) -> Result<MihomoClient> {
    let url = if controller.contains("://") {
        controller.to_owned()
    } else {
        format!("http://{controller}")
    };
    MihomoClient::new(&url, secret)
}

/// Appends the core's whole output to the validation log; a rejection carries its last error.
fn judge_validation(file: &mut impl Write, log: &Path, output: &Output) -> Result<()> {
    let mut text = output.stdout.clone();
    if !text.is_empty() && !text.ends_with(b"\n") {
        text.push(b'\n');
    }
    text.extend_from_slice(&output.stderr);
    // Failing to keep the log must not turn an accepted configuration into an error.
    let _ = file.write_all(&text);
    if output.status.success() {
        return Ok(());
    }
    let reason = rejection_reason(&String::from_utf8_lossy(&text))
        .unwrap_or_else(|| output.status.to_string());
    bail!(
        "core rejected configuration: {reason}; see {}",
        log.display()
    )
}

/// The message of the core's last error: Mihomo logs `level=error msg="..."`, sing-box prints
/// `FATAL[0000] ...` or `ERROR[0000] ...`; failing both, the last line says the most.
fn rejection_reason(output: &str) -> Option<String> {
    let lines = || {
        output
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
    };
    let reason = lines()
        .rev()
        .find_map(logfmt_error)
        .or_else(|| lines().rev().find_map(singbox_error))
        .or_else(|| lines().next_back().map(str::to_owned))?;
    Some(match reason.chars().count() {
        0..=300 => reason,
        _ => reason.chars().take(299).chain(['…']).collect(),
    })
}

fn logfmt_error(line: &str) -> Option<String> {
    let fields = logfmt(line);
    let value = |key: &str| {
        fields
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    };
    matches!(value("level")?, "error" | "fatal" | "panic").then(|| {
        value("msg")
            .filter(|msg| !msg.is_empty())
            .map(str::to_owned)
    })?
}

/// `key=value` pairs; values are bare words or Go-quoted strings.
fn logfmt(line: &str) -> Vec<(String, String)> {
    let mut fields = Vec::new();
    let mut chars = line.chars().peekable();
    loop {
        while chars.next_if(|c| c.is_whitespace()).is_some() {}
        let mut key = String::new();
        while let Some(c) = chars.next_if(|c| !c.is_whitespace() && *c != '=') {
            key.push(c);
        }
        if chars.peek().is_none() {
            return fields;
        }
        if chars.next_if_eq(&'=').is_none() {
            continue;
        }
        let mut value = String::new();
        if chars.next_if_eq(&'"').is_some() {
            while let Some(c) = chars.next() {
                match c {
                    '"' => break,
                    '\\' => match chars.next() {
                        Some('n' | 't' | 'r') => value.push(' '),
                        Some(escaped) => value.push(escaped),
                        None => break,
                    },
                    c => value.push(c),
                }
            }
        } else {
            while let Some(c) = chars.next_if(|c| !c.is_whitespace()) {
                value.push(c);
            }
        }
        if !key.is_empty() {
            fields.push((key, value));
        }
    }
}

fn singbox_error(line: &str) -> Option<String> {
    let rest = line
        .strip_prefix("FATAL")
        .or_else(|| line.strip_prefix("ERROR"))?;
    let rest = match rest.strip_prefix('[') {
        Some(rest) => rest.split_once(']')?.1,
        None if rest.is_empty() || rest.starts_with(char::is_whitespace) => rest,
        None => return None,
    };
    let message = rest.trim();
    (!message.is_empty()).then(|| message.to_owned())
}

pub struct Process {
    pub child: Child,
    pub spec: Spec,
    // Parsed once at start: a running core's configuration never changes, and status
    // checks run every second.
    pub device: Option<String>,
    /// The kernel stack carries this TUN device's traffic and needs a firewall exception.
    pub kernel_stack: bool,
    pub api: MihomoClient,
}

impl Process {
    pub async fn start(spec: Spec, path: &Path) -> Result<Self> {
        // One parse gives the controller, the TUN device, its stack and the ports; a sing-box
        // core needs only its controller.
        let root: Option<Value> = match spec.core {
            CoreKind::Mihomo => Some(serde_yaml_ng::from_str(&spec.text)?),
            CoreKind::Singbox => None,
        };
        let (controller, secret) = match &root {
            Some(root) => mihomo_controller(root)?,
            None => spec.controller()?,
        };
        let api = controller_client(&controller, secret)?;
        let device = root.as_ref().map(mihomo_tun_device).transpose()?.flatten();
        let kernel_stack = device.is_some()
            && root
                .as_ref()
                .and_then(Value::as_mapping)
                .is_some_and(super::firewall::kernel_stack);
        // A listener left by another core must not satisfy this process's readiness probe.
        let listener = std::net::TcpListener::bind(&controller)
            .context("controller port is already in use")?;
        drop(listener);
        if let Some(root) = &root {
            check_mixed_port(root)?;
        }
        private_write(path, spec.text.as_bytes())?;
        let log = spec.data_dir.join(format!(
            "logs/{}-{}.log",
            spec.core.as_str(),
            chrono::Local::now().format("%Y-%m-%d")
        ));
        let output = private_log(&log)?;
        let child = spec
            .command(path, false)
            .stdout(output.try_clone()?)
            .stderr(output)
            .spawn()
            .context("failed to start core")?;
        let mut process = Self {
            child,
            spec,
            device,
            kernel_stack,
            api,
        };
        if let Err(e) = process.wait_ready().await {
            let cleanup = process.stop().await;
            return Err(e).with_context(|| {
                format!(
                    "startup failed; cleanup={}; see {}",
                    if cleanup.is_ok() { "ok" } else { "failed" },
                    log.display()
                )
            });
        }
        Ok(process)
    }

    async fn wait_ready(&mut self) -> Result<()> {
        let expected = self.device.as_deref();
        // Let bind failures surface before accepting another process's controller response.
        tokio::time::sleep(Duration::from_millis(200)).await;
        for _ in 0..50 {
            if let Some(exit) = self.child.try_wait()? {
                bail!("core exited during startup ({exit})");
            }
            if let Ok(Ok(config)) =
                tokio::time::timeout(Duration::from_millis(300), self.api.runtime_config()).await
                && confirms_tun(&config, expected)
                && device_exists(expected).await
            {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        bail!("core did not confirm the requested runtime/TUN device")
    }

    pub fn running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
    pub fn pid(&self) -> Option<u32> {
        self.child.id()
    }
    pub async fn stop(&mut self) -> Result<()> {
        stop_child(&mut self.child).await
    }
}

pub async fn device_exists(device: Option<&str>) -> bool {
    let Some(device) = device else {
        return true;
    };
    // Answered by the kernel for this process's own network namespace; no process is spawned.
    std::ffi::CString::new(device).is_ok_and(|name| {
        // SAFETY: `name` is a valid NUL-terminated string that outlives the call.
        unsafe { libc::if_nametoindex(name.as_ptr()) != 0 }
    })
}

pub fn confirms_tun(config: &RuntimeConfig, expected: Option<&str>) -> bool {
    match expected {
        Some(device) => config
            .tun
            .as_ref()
            .is_some_and(|v| v.enable && v.device == device),
        None => config.tun.as_ref().is_none_or(|v| !v.enable),
    }
}

pub async fn stop_child(child: &mut Child) -> Result<()> {
    if child.try_wait()?.is_some() {
        return Ok(());
    }
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        // Child still belongs to this handle and has not been reaped; the PID cannot be reused.
        let rc = unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
        if rc != 0 && std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH) {
            return Err(std::io::Error::last_os_error()).context("SIGTERM failed");
        }
    }
    match tokio::time::timeout(Duration::from_secs(5), child.wait()).await {
        Ok(result) => {
            result?;
        }
        Err(_) => {
            child.start_kill()?;
            child.wait().await?;
        }
    }
    Ok(())
}

pub fn private_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("missing parent")?;
    fs::create_dir_all(parent)?;
    let tmp = parent.join(format!(".omash-{}", uuid::Uuid::new_v4()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| -> Result<()> {
        let mut file = options.open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&tmp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(tmp);
    }
    result
}

fn private_log(path: &Path) -> Result<fs::File> {
    fs::create_dir_all(path.parent().context("missing log parent")?)?;
    let mut options = OpenOptions::new();
    options.append(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    Ok(options.open(path)?)
}

/// Fails when Mihomo could not listen on `mixed-port`: Mihomo only logs that and keeps running.
fn check_mixed_port(root: &Value) -> Result<()> {
    let Some(port) = root["mixed-port"].as_u64().filter(|p| *p > 0) else {
        return Ok(());
    };
    let port = u16::try_from(port)?;
    for address in mixed_addresses(root, port, super::routes::ipv6_supported(), interface_index)? {
        std::net::TcpListener::bind(address)
            .with_context(|| format!("mixed port {address} is unavailable"))?;
        // The mixed listener serves SOCKS UDP on the same address.
        std::net::UdpSocket::bind(address)
            .with_context(|| format!("mixed port {address} is unavailable for UDP"))?;
    }
    Ok(())
}

/// Where Mihomo listens for `mixed-port`, following its `genAddr` and `preResolve`: loopback
/// unless LAN access is allowed, else `bind-address`, where `*`, an empty host and an
/// unspecified address are Go's dual-stack wildcard and `localhost` is IPv4 loopback.
fn mixed_addresses(
    root: &Value,
    port: u16,
    ipv6: bool,
    interface_index: impl Fn(&str) -> Option<u32>,
) -> Result<Vec<SocketAddr>> {
    let wildcard = || {
        let mut all = vec![SocketAddr::from((Ipv4Addr::UNSPECIFIED, port))];
        if ipv6 {
            all.push(SocketAddr::from((Ipv6Addr::UNSPECIFIED, port)));
        }
        all
    };
    if root["allow-lan"].as_bool() != Some(true) {
        return Ok(vec![SocketAddr::from((Ipv4Addr::LOCALHOST, port))]);
    }
    let host = match &root["bind-address"] {
        Value::Null => "*".to_owned(),
        Value::String(host) => host.clone(),
        Value::Number(host) => host.to_string(),
        _ => bail!("bind-address must be a string"),
    };
    match host.as_str() {
        "*" | "" => return Ok(wildcard()),
        "localhost" => return Ok(vec![SocketAddr::from((Ipv4Addr::LOCALHOST, port))]),
        _ => {}
    }
    let invalid =
        || anyhow!("bind-address must be *, localhost or an IP address (IPv6 in brackets): {host}");
    // Mihomo appends the port as it is, so an IPv6 address listens only in brackets.
    let address = match format!("{host}:{port}").parse::<SocketAddr>() {
        Ok(address) => address,
        Err(_) => {
            // Go also resolves an interface name as the zone of a scoped address.
            let (ip, zone) = host
                .strip_prefix('[')
                .and_then(|host| host.strip_suffix(']'))
                .and_then(|host| host.split_once('%'))
                .ok_or_else(invalid)?;
            let ip: Ipv6Addr = ip.parse().map_err(|_| invalid())?;
            let scope = interface_index(zone)
                .ok_or_else(|| anyhow!("bind-address names an unknown interface: {host}"))?;
            SocketAddr::V6(SocketAddrV6::new(ip, port, 0, scope))
        }
    };
    // Go treats an IPv4-mapped address as the IPv4 one, the wildcard included.
    let address = match address {
        SocketAddr::V6(v6) => match v6.ip().to_ipv4_mapped() {
            Some(ip) => SocketAddr::from((ip, port)),
            None => address,
        },
        v4 => v4,
    };
    Ok(if address.ip().is_unspecified() {
        wildcard()
    } else {
        vec![address]
    })
}

fn interface_index(name: &str) -> Option<u32> {
    if name.is_empty() || name.contains('/') || name.starts_with('.') {
        return None;
    }
    fs::read_to_string(format!("/sys/class/net/{name}/ifindex"))
        .ok()?
        .trim()
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn tun_requires_runtime_confirmation() {
        let mut v = RuntimeConfig::default();
        assert!(!confirms_tun(&v, Some("omash-tun")));
        v.tun = Some(crate::api::TunRuntime {
            enable: true,
            device: "other".into(),
        });
        assert!(!confirms_tun(&v, Some("omash-tun")));
        v.tun.as_mut().unwrap().device = "omash-tun".into();
        assert!(confirms_tun(&v, Some("omash-tun")));
        assert!(!confirms_tun(&v, None));
    }
    #[test]
    fn staged_configuration_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pending.yaml");
        private_write(&path, b"secret: test").unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn mode_override_preserves_other_runtime_settings() {
        let mut spec = Spec {
            core: CoreKind::Mihomo,
            data_dir: PathBuf::new(),
            text: "mode: rule\ntun: {enable: true, device: omash-tun}\n".into(),
        };
        spec.set_mode("global").unwrap();
        assert_eq!(spec.mode().unwrap(), "global");
        assert_eq!(spec.tun_device().unwrap().as_deref(), Some("omash-tun"));
    }

    fn finished(code: i32, stdout: &str, stderr: &str) -> Output {
        use std::os::unix::process::ExitStatusExt;
        Output {
            status: std::process::ExitStatus::from_raw(code << 8),
            stdout: stdout.into(),
            stderr: stderr.into(),
        }
    }

    // What `mihomo -t` prints for a rejected configuration (Mihomo 1.19, to stdout).
    const MIHOMO_REJECTION: &str = concat!(
        "time=\"2026-10-07T16:33:54.771229197-07:00\" level=info msg=\"Start initial configuration in progress\"\n",
        "time=\"2026-10-07T16:33:54.771284858-07:00\" level=error msg=\"proxy 0: unsupport proxy type: nosuchtype\"\n",
        "configuration file /home/u/.local/share/omash/tun-service/pending.yaml test failed\n",
    );

    #[test]
    fn rejection_carries_the_cores_last_error_and_the_log_keeps_everything() {
        let log_path = Path::new("/data/logs/core-validation.log");
        let mut log = Vec::new();
        let error =
            judge_validation(&mut log, log_path, &finished(1, MIHOMO_REJECTION, "")).unwrap_err();
        assert_eq!(
            error.to_string(),
            "core rejected configuration: proxy 0: unsupport proxy type: nosuchtype; see /data/logs/core-validation.log"
        );
        assert_eq!(log, MIHOMO_REJECTION.as_bytes());
    }

    #[test]
    fn accepted_configuration_is_logged_and_survives_an_unwritable_log() {
        let success = finished(
            0,
            "time=\"x\" level=info msg=\"Initial configuration complete\"\nconfiguration file /p.yaml test is successful\n",
            "warning on stderr\n",
        );
        let mut log = Vec::new();
        judge_validation(&mut log, Path::new("/log"), &success).unwrap();
        assert!(
            String::from_utf8(log)
                .unwrap()
                .ends_with("is successful\nwarning on stderr\n")
        );

        struct Broken;
        impl Write for Broken {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("disk full"))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        judge_validation(&mut Broken, Path::new("/log"), &success).unwrap();
        let error = judge_validation(
            &mut Broken,
            Path::new("/log"),
            &finished(1, MIHOMO_REJECTION, ""),
        )
        .unwrap_err();
        assert!(error.to_string().contains("unsupport proxy type"));
    }

    #[test]
    fn reason_comes_from_error_or_fatal_logfmt_lines() {
        assert_eq!(
            rejection_reason(MIHOMO_REJECTION).as_deref(),
            Some("proxy 0: unsupport proxy type: nosuchtype")
        );
        // Mihomo logs parse failures as fatal; the last error line wins over earlier ones.
        let fatal = concat!(
            "time=\"t\" level=error msg=\"an earlier problem\"\n",
            "time=\"t\" level=fatal msg=\"Parse config error: rules[0] [MATCH] error: format invalid\"\n",
            "time=\"t\" level=warning msg=\"ignored\"\n",
        );
        assert_eq!(
            rejection_reason(fatal).as_deref(),
            Some("Parse config error: rules[0] [MATCH] error: format invalid")
        );
        // Quoted values are decoded, bare values and fields in any order are understood.
        assert_eq!(
            rejection_reason("time=\"t\" msg=\"rule \\\"x\\\" is\\ninvalid\" level=error")
                .as_deref(),
            Some("rule \"x\" is invalid")
        );
        assert_eq!(
            rejection_reason("level=error msg=denied time=t").as_deref(),
            Some("denied")
        );
        // `level=error` inside a message does not make a line an error.
        assert_eq!(
            rejection_reason("level=info msg=\"saw level=error here\"\nlast line").as_deref(),
            Some("last line")
        );
    }

    #[test]
    fn reason_understands_singbox_and_falls_back_to_the_last_line() {
        let singbox = "INFO[0000] starting\nERROR[0000] early\nFATAL[0000] decode config at /x.json: unknown field \"foo\"\n";
        assert_eq!(
            rejection_reason(singbox).as_deref(),
            Some("decode config at /x.json: unknown field \"foo\"")
        );
        assert_eq!(
            rejection_reason("ERROR no brackets here").as_deref(),
            Some("no brackets here")
        );
        assert_eq!(
            rejection_reason("ERRORS are not levels\n").as_deref(),
            Some("ERRORS are not levels")
        );
        assert_eq!(
            rejection_reason("something odd\n\n  segmentation fault  \n\n").as_deref(),
            Some("segmentation fault")
        );
        assert_eq!(rejection_reason(""), None);
        assert_eq!(rejection_reason(" \n\t\n"), None);
        // Without any output the exit status is the reason.
        let error =
            judge_validation(&mut Vec::new(), Path::new("/l"), &finished(2, "", "")).unwrap_err();
        assert_eq!(
            error.to_string(),
            "core rejected configuration: exit status: 2; see /l"
        );
    }

    #[test]
    fn reason_is_bounded() {
        let long = format!("level=error msg=\"{}\"", "ü".repeat(1000));
        let reason = rejection_reason(&long).unwrap();
        assert_eq!(reason.chars().count(), 300);
        assert!(reason.ends_with('…'));
        let exact = "x".repeat(300);
        assert_eq!(rejection_reason(&exact).as_deref(), Some(exact.as_str()));
    }

    #[tokio::test]
    async fn device_check_asks_the_kernel_instead_of_spawning_ip() {
        assert!(device_exists(None).await);
        #[cfg(target_os = "linux")]
        assert!(device_exists(Some("lo")).await);
        assert!(!device_exists(Some("omash-no-such-dev")).await);
        // Names the kernel cannot hold are not devices.
        assert!(!device_exists(Some("")).await);
        assert!(!device_exists(Some("a\0b")).await);
        assert!(!device_exists(Some(&"x".repeat(64))).await);
    }

    #[tokio::test]
    async fn stop_sends_sigterm_before_kill_and_waits_for_exit() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("term");
        let mut child = Command::new("sh").arg("-c").arg("trap 'echo stopped > \"$1\"; exit 0' TERM; echo ready; while :; do sleep 0.1; done").arg("sh").arg(&marker).stdout(Stdio::piped()).spawn().unwrap();
        use tokio::io::{AsyncBufReadExt, BufReader};
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .await
            .unwrap();
        stop_child(&mut child).await.unwrap();
        assert_eq!(fs::read_to_string(marker).unwrap().trim(), "stopped");
        assert!(child.try_wait().unwrap().is_some());
        stop_child(&mut child).await.unwrap();
    }

    #[test]
    fn mixed_port_probe_follows_bind_address() {
        let config = |port: u16, lan: bool, bind: Option<&str>| -> Value {
            let mut root: Value =
                serde_yaml_ng::from_str(&format!("{{mixed-port: {port}, allow-lan: {lan}}}"))
                    .unwrap();
            if let Some(bind) = bind {
                root["bind-address"] = bind.into();
            }
            root
        };
        let free_port = || {
            std::net::TcpListener::bind("127.0.0.1:0")
                .unwrap()
                .local_addr()
                .unwrap()
                .port()
        };
        let held = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = held.local_addr().unwrap().port();
        // Linux treats all of 127.0.0.0/8 as local: another loopback address is free.
        check_mixed_port(&config(port, true, Some("127.0.0.2"))).unwrap();
        for bind in [
            Some("127.0.0.1"),
            Some("localhost"),
            Some(""),
            Some("*"),
            None,
        ] {
            let error = check_mixed_port(&config(port, true, bind)).unwrap_err();
            assert!(
                format!("{error:#}").contains("is unavailable"),
                "{bind:?}: {error:#}"
            );
        }
        let error = check_mixed_port(&config(port, true, Some("localhost"))).unwrap_err();
        assert!(
            format!("{error:#}").contains(&format!("mixed port 127.0.0.1:{port} is unavailable")),
            "{error:#}"
        );
        assert!(check_mixed_port(&config(port, false, Some("127.0.0.2"))).is_err());
        // Mihomo drops the mixed listener when its UDP side cannot bind either.
        let udp = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let udp_port = udp.local_addr().unwrap().port();
        let error = check_mixed_port(&config(udp_port, false, None)).unwrap_err();
        assert!(
            format!("{error:#}").contains(&format!("127.0.0.1:{udp_port}")),
            "{error:#}"
        );
        drop(udp);
        check_mixed_port(&config(free_port(), true, Some(""))).unwrap();
        // What Mihomo cannot listen on is refused instead of left to a core that keeps running.
        for bind in ["example.invalid", "::1", "[::1%missing0]"] {
            let error = check_mixed_port(&config(free_port(), true, Some(bind))).unwrap_err();
            assert!(
                format!("{error:#}").contains("bind-address"),
                "{bind}: {error:#}"
            );
        }

        let addresses = |bind: &str, ipv6: bool| {
            mixed_addresses(&config(7890, true, Some(bind)), 7890, ipv6, |name| {
                (name == "eth0").then_some(2)
            })
        };
        let v6 = |ip: &str, scope: u32| {
            SocketAddr::V6(SocketAddrV6::new(ip.parse().unwrap(), 7890, 0, scope))
        };
        assert_eq!(
            addresses("[fe80::1%eth0]", true).unwrap(),
            [v6("fe80::1", 2)]
        );
        assert_eq!(addresses("[fe80::1%3]", true).unwrap(), [v6("fe80::1", 3)]);
        assert!(addresses("[fe80::1%missing0]", true).is_err());
        let both: Vec<SocketAddr> = vec![
            "0.0.0.0:7890".parse().unwrap(),
            "[::]:7890".parse().unwrap(),
        ];
        for bind in ["*", "", "0.0.0.0", "[::]", "[::ffff:0.0.0.0]"] {
            assert_eq!(addresses(bind, true).unwrap(), both, "{bind}");
            assert_eq!(addresses(bind, false).unwrap(), both[..1], "{bind}");
        }

        // Go listens dual-stack on an explicit IPv4 wildcard, so IPv6 conflicts count too.
        match std::net::TcpListener::bind("[::1]:0") {
            Ok(held6) if crate::tun::routes::ipv6_supported() => {
                let port = held6.local_addr().unwrap().port();
                assert!(check_mixed_port(&config(port, true, Some("0.0.0.0"))).is_err());
                assert!(check_mixed_port(&config(port, true, Some("[::1%lo]"))).is_err());
                check_mixed_port(&config(free_port(), true, Some("[::1%lo]"))).unwrap();
            }
            _ => eprintln!("skipping the IPv6 probes: no IPv6 loopback"),
        }
    }
}
