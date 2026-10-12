use anyhow::{Context, Result, bail};
use std::os::unix::fs::MetadataExt;
use std::{fs, path::Path};

/// Every component of privileged executable/configuration paths must be root-owned.
pub fn trusted_path(path: &Path, executable: bool) -> Result<()> {
    if !path.is_absolute() {
        bail!("privileged path must be absolute");
    }
    for part in path.ancestors() {
        let metadata = fs::symlink_metadata(part)?;
        if metadata.file_type().is_symlink() || metadata.uid() != 0 || metadata.mode() & 0o022 != 0
        {
            bail!(
                "untrusted ownership, permissions or symlink: {}",
                part.display()
            );
        }
    }
    let metadata = fs::metadata(path)?;
    if !metadata.is_file()
        || (executable && (metadata.mode() & 0o111 == 0 || metadata.mode() & 0o6000 != 0))
    {
        bail!("invalid privileged file: {}", path.display());
    }
    Ok(())
}
pub fn trusted_core() -> Result<()> {
    let core = Path::new("/usr/bin/mihomo");
    trusted_path(core, true)?;
    let output = getcap(["/usr/sbin/getcap", "/usr/bin/getcap"], core)?;
    if !output.status.success() || !output.stdout.is_empty() {
        bail!(
            "Mihomo has file capabilities or cannot be inspected; use an unmodified system package"
        );
    }
    Ok(())
}

fn getcap(programs: [&str; 2], file: &Path) -> Result<std::process::Output> {
    let run = |program| std::process::Command::new(program).arg(file).output();
    run(programs[0])
        .or_else(|_| run(programs[1]))
        .with_context(|| {
            format!(
                "cannot run {} or {} to inspect {}; getcap comes with libcap (libcap2-bin on Debian)",
                programs[0],
                programs[1],
                file.display()
            )
        })
}

pub const HELPER: &str = "/usr/local/libexec/omash-tun-service";
pub const UNIT: &str = "/etc/systemd/system/omash-tun.service";
const UNIT_NAME: &str = "omash-tun.service";

fn linux_only() -> Result<()> {
    if !cfg!(target_os = "linux") {
        bail!("TUN setup requires Linux with systemd");
    }
    Ok(())
}
fn safe_data_dir(path: &Path, uid: u32) -> Result<()> {
    use anyhow::Context;
    let name = path.to_str().context("data path must be UTF-8")?;
    if !name.starts_with('/')
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"/._-".contains(&b))
    {
        bail!(
            "TUN data directory must use an absolute path without whitespace or systemd substitutions"
        );
    }
    if fs::canonicalize(path)? != path || fs::metadata(path)?.uid() != uid {
        bail!("data directory must be canonical and owned by the installing user");
    }
    for part in path.ancestors() {
        let m = fs::symlink_metadata(part)?;
        if m.file_type().is_symlink() || (m.uid() != 0 && m.uid() != uid) || m.mode() & 0o022 != 0 {
            bail!("unsafe TUN data directory ancestry: {}", part.display());
        }
    }
    Ok(())
}
fn target(path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("missing target parent"))?;
    for part in parent.ancestors() {
        let m = fs::symlink_metadata(part)?;
        if m.file_type().is_symlink() || m.uid() != 0 || m.mode() & 0o022 != 0 {
            bail!("unsafe installation parent {}", part.display());
        }
    }
    if fs::symlink_metadata(path).is_ok() {
        trusted_path(path, false)?;
    }
    Ok(())
}
fn systemctl(args: &[&str]) -> Result<()> {
    let status = std::process::Command::new("/usr/bin/systemctl")
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .args(args)
        .status()?;
    if !status.success() {
        bail!("systemctl {} failed ({status})", args.join(" "));
    }
    Ok(())
}
fn require_installer(uid: u32) -> Result<()> {
    linux_only()?;
    if unsafe { libc::geteuid() } != 0 || uid == 0 {
        bail!("installer requires root and a non-root service owner");
    }
    if std::env::var("SUDO_UID")
        .or_else(|_| std::env::var("PKEXEC_UID"))
        .ok()
        .as_deref()
        != Some(&uid.to_string())
    {
        bail!("installer UID must match the user who authorized elevation");
    }
    Ok(())
}

pub fn root_install(uid: u32, gid: u32, data_dir: &Path, digest: &str) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    require_installer(uid)?;
    // Numeric User/Group must describe the same account; never grant a caller an unrelated group.
    let account = unsafe { libc::getpwuid(uid) };
    if account.is_null() || unsafe { (*account).pw_gid } != gid {
        bail!("service GID must match the installing user's primary group");
    }
    safe_data_dir(data_dir, uid)?;
    trusted_core()?;
    super::firewall::program()?;
    if Path::new(super::service::SETTINGS).exists() {
        let old = super::service::load_settings()?;
        if old.uid != uid {
            bail!("TUN service belongs to another user; refusing takeover");
        }
    }
    let source = std::env::current_exe()?;
    let bytes = fs::read(&source)?;
    if super::protocol::revision(&bytes) != digest {
        bail!("installer binary changed after authorization");
    }
    // Only this dedicated directory may be created; verify its parent first.
    let parent = Path::new(HELPER).parent().unwrap();
    if !parent.exists() {
        target(parent)?;
        fs::create_dir(parent)?;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o755))?;
    }
    for path in [HELPER, UNIT, super::service::SETTINGS] {
        target(Path::new(path))?;
    }
    if Path::new(UNIT).exists() {
        systemctl(&["stop", UNIT_NAME])?;
    }
    super::process::private_write(Path::new(HELPER), &bytes)?;
    fs::set_permissions(HELPER, fs::Permissions::from_mode(0o755))?;
    if super::protocol::revision(&fs::read(HELPER)?) != digest {
        bail!("installed helper digest mismatch");
    }
    let settings = super::service::Settings {
        uid,
        gid,
        data_dir: data_dir.to_path_buf(),
    };
    super::process::private_write(
        Path::new(super::service::SETTINGS),
        &serde_json::to_vec(&settings)?,
    )?;
    fs::set_permissions(super::service::SETTINGS, fs::Permissions::from_mode(0o644))?;
    let unit = render_unit(&settings)?;
    super::process::private_write(Path::new(UNIT), unit.as_bytes())?;
    fs::set_permissions(UNIT, fs::Permissions::from_mode(0o644))?;
    systemctl(&["daemon-reload"])?;
    systemctl(&["enable", "--now", UNIT_NAME])?;
    Ok(())
}

fn render_unit(settings: &super::service::Settings) -> Result<String> {
    use anyhow::Context;
    let data = settings
        .data_dir
        .to_str()
        .context("invalid data directory")?;
    Ok(include_str!("../../systemd/omash-tun.service")
        .replace("@UID@", &settings.uid.to_string())
        .replace("@GID@", &settings.gid.to_string())
        .replace("@DATA@", data))
}

pub fn root_uninstall(uid: u32) -> Result<()> {
    require_installer(uid)?;
    let settings = super::service::load_settings()?;
    if settings.uid != uid {
        bail!("TUN helper belongs to another user");
    }
    for path in [HELPER, UNIT, super::service::SETTINGS] {
        target(Path::new(path))?;
    }
    // systemd waits for all processes in the unit cgroup before removal.
    systemctl(&["disable", "--now", UNIT_NAME])?;
    for path in [UNIT, HELPER, super::service::SETTINGS] {
        fs::remove_file(path)?;
    }
    systemctl(&["daemon-reload"])?;
    Ok(())
}

pub async fn setup() -> Result<()> {
    install_helper().await?;
    let uid = unsafe { libc::geteuid() };
    println!("TUN helper installed for UID {uid}. Enable with: omash tun on");
    Ok(())
}

/// Installs or updates the helper through sudo or pkexec and confirms that it answers; `setup`
/// and the TUI's Settings page differ only in what they say afterwards.
pub async fn install_helper() -> Result<()> {
    linux_only()?;
    let uid = unsafe { libc::geteuid() };
    let gid = unsafe { libc::getegid() };
    if uid == 0 {
        bail!("run `omash tun setup` as your normal desktop user");
    }
    let dir = crate::config::Config::data_dir();
    fs::create_dir_all(&dir)?;
    safe_data_dir(&dir, uid)?;
    let exe = std::env::current_exe()?;
    let digest = super::protocol::revision(&fs::read(&exe)?);
    let status = elevate(&exe)?
        .arg("internal-tun-install")
        .arg(uid.to_string())
        .arg(gid.to_string())
        .arg(&dir)
        .arg(&digest)
        .status()
        .await?;
    if !status.success() {
        bail!("TUN setup failed ({status})");
    }
    // Confirm the newly installed helper, including its registered data directory.
    let mut connected = false;
    for _ in 0..30 {
        if super::protocol::Client::connect().await.is_ok() {
            connected = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    if !connected {
        bail!(
            "helper installed but did not answer; inspect `systemctl status omash-tun` and `omash tun doctor`"
        );
    }
    // When the installer cannot confirm the old helper, it leaves the supervisor running the
    // replaced binary, which would keep using the previous helper protocol.
    match crate::core::supervisor_runs_replaced_binary().await {
        Ok(false) => {}
        Ok(true) => {
            use anyhow::Context;
            crate::core::restart_supervisor().await.context(
                "helper installed, but the supervisor still runs a replaced omash binary and could not be restarted; run `systemctl --user restart omash-supervisor.service`",
            )?;
            println!("Restarted the supervisor, which was still running a replaced omash binary.");
        }
        Err(error) => eprintln!(
            "Cannot check whether the supervisor runs a replaced omash binary ({error:#}); if TUN reports a protocol mismatch, run `systemctl --user restart omash-supervisor.service`."
        ),
    }
    Ok(())
}
fn elevate(exe: &Path) -> Result<tokio::process::Command> {
    use std::io::IsTerminal;
    let mut c = if std::io::stdin().is_terminal() || !Path::new("/usr/bin/pkexec").is_file() {
        let mut c = tokio::process::Command::new("/usr/bin/sudo");
        if !std::io::stdin().is_terminal() {
            c.arg("-n");
        }
        c
    } else {
        tokio::process::Command::new("/usr/bin/pkexec")
    };
    c.arg(exe);
    Ok(c)
}
pub async fn uninstall(config: &mut crate::config::Config) -> Result<()> {
    linux_only()?;
    if !Path::new(super::service::SETTINGS).exists() {
        println!("TUN helper is not installed");
        return Ok(());
    }
    let uid = unsafe { libc::geteuid() };
    if super::service::load_settings()?.uid != uid {
        bail!("run `omash tun uninstall` as the user who installed the helper");
    }
    let has_profiles = !crate::profiles::Profiles::load()?.items.is_empty();
    // Complete the backend switch before removing the service it needs to stop.
    config.tun_enabled = false;
    config.save()?;
    crate::core::request_restart()?;
    let request = super::toggle::Request::new(crate::core::desired_revision(), false);
    crate::core::ensure_supervisor(config.auto_start).await?;
    super::cli::wait_for_uninstall(request, has_profiles).await?;
    // Only the helper retries a failed removal of its firewall exceptions.
    super::cli::wait_for_firewall_cleanup().await?;
    let status = elevate(&std::env::current_exe()?)?
        .arg("internal-tun-uninstall")
        .arg(uid.to_string())
        .status()
        .await?;
    if !status.success() {
        bail!("TUN helper uninstall failed ({status})");
    }
    println!("TUN helper removed; profiles and data preserved");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn missing_getcap_names_the_tool() {
        let programs = ["/nonexistent/sbin/getcap", "/nonexistent/bin/getcap"];
        let error = getcap(programs, Path::new("/usr/bin/mihomo")).unwrap_err();
        let text = format!("{error:#}");
        assert!(text.contains("getcap") && text.contains("libcap"), "{text}");
    }
    #[test]
    fn installer_rejects_symlink_target() {
        let dir = tempfile::tempdir().unwrap();
        let link = dir.path().join("helper");
        std::os::unix::fs::symlink("/usr/bin/true", &link).unwrap();
        assert!(trusted_path(&link, true).is_err());
    }
    #[test]
    fn helper_rejects_untrusted_executable() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("core");
        fs::write(&file, b"not trusted").unwrap();
        assert!(trusted_path(&file, true).is_err());
    }
    #[test]
    fn service_unit_limits_capabilities() {
        let unit = render_unit(&super::super::service::Settings {
            uid: 1000,
            gid: 1000,
            data_dir: "/home/test/data".into(),
        })
        .unwrap();
        assert!(unit.contains("User=1000\n"));
        assert!(unit.contains("AmbientCapabilities=CAP_NET_ADMIN CAP_NET_RAW\n"));
        assert!(unit.contains("CapabilityBoundingSet=CAP_NET_ADMIN CAP_NET_RAW\n"));
        assert!(unit.contains("NoNewPrivileges=yes\n"));
        assert!(unit.contains("ReadWritePaths=/home/test/data\n"));
        for directive in [
            "SystemCallArchitectures=native\n",
            "SystemCallFilter=@system-service\n",
            "SystemCallErrorNumber=EPERM\n",
            "RestrictNamespaces=yes\n",
            "MemoryDenyWriteExecute=yes\n",
            "ProtectClock=yes\n",
            "ProtectHostname=yes\n",
            "ProtectKernelLogs=yes\n",
            "PrivateIPC=yes\n",
            "DevicePolicy=closed\n",
            "DeviceAllow=/dev/net/tun rw\n",
        ] {
            assert!(unit.contains(directive), "missing {directive:?}");
        }
        // Only the TUN node may be opened; the core still needs the host network and its own /proc.
        assert_eq!(unit.matches("DeviceAllow=").count(), 1);
        for directive in ["PrivateDevices=", "PrivateNetwork=", "ProtectProc="] {
            assert!(!unit.contains(directive), "unexpected {directive}");
        }
        for placeholder in ["@UID@", "@GID@", "@DATA@"] {
            assert!(!unit.contains(placeholder), "unreplaced {placeholder}");
        }
    }
}
