use crate::{config::Config, core};
use anyhow::{Context, Result, bail};
use flate2::read::GzDecoder;
use reqwest::Client;
use std::{fs, io::Read as _, path::Path, time::Duration};
use tokio::process::Command;

const MIHOMO_VERSION_URL: &str =
    "https://github.com/MetaCubeX/mihomo/releases/latest/download/version.txt";
const MIHOMO_RELEASE_ROOT: &str = "https://github.com/MetaCubeX/mihomo/releases/download";
const GEODATA_RELEASE_ROOT: &str =
    "https://github.com/MetaCubeX/meta-rules-dat/releases/download/latest";

pub enum CoreUpdate {
    AlreadyLatest(String),
    Installed(String),
}

pub async fn update_core(current: &str) -> Result<CoreUpdate> {
    let client = download_client()?;
    let version = download_text(&client, MIHOMO_VERSION_URL).await?;
    let version = version.trim().trim_start_matches('v').to_owned();
    if version_numbers(&version) <= version_numbers(current) {
        return Ok(CoreUpdate::AlreadyLatest(version));
    }

    let asset = core_asset()?;
    let url = format!("{MIHOMO_RELEASE_ROOT}/v{version}/{asset}-v{version}.gz");
    let compressed = download_bytes(&client, &url).await?;
    let binary = tokio::task::spawn_blocking(move || -> Result<Vec<u8>> {
        let mut decoder = GzDecoder::new(compressed.as_slice());
        let mut binary = Vec::new();
        decoder.read_to_end(&mut binary)?;
        Ok(binary)
    })
    .await??;
    if binary.len() < 1024 * 1024 {
        bail!("downloaded Mihomo binary is unexpectedly small");
    }
    install_executable(&Config::mihomo_path(), &binary).await?;
    core::request_replace()?;
    Ok(CoreUpdate::Installed(version))
}

pub async fn update_geodata() -> Result<()> {
    let client = download_client()?;
    let metadb_url = format!("{GEODATA_RELEASE_ROOT}/geoip.metadb");
    let geosite_url = format!("{GEODATA_RELEASE_ROOT}/geosite.dat");
    let asn_url = format!("{GEODATA_RELEASE_ROOT}/GeoLite2-ASN.mmdb");
    let (metadb, geosite, asn) = tokio::try_join!(
        download_bytes(&client, &metadb_url),
        download_bytes(&client, &geosite_url),
        download_bytes(&client, &asn_url),
    )?;
    for (name, bytes) in [
        ("geoip.metadb", metadb),
        ("geosite.dat", geosite),
        ("GeoLite2-ASN.mmdb", asn),
    ] {
        if bytes.len() < 1024 {
            bail!("downloaded {name} is unexpectedly small");
        }
        atomic_replace(&Config::data_dir().join(name), &bytes)?;
    }
    core::request_replace()?;
    Ok(())
}

fn download_client() -> Result<Client> {
    Ok(Client::builder()
        .user_agent("omash/0.1 clash-verge-compatible-updater")
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(90))
        .build()?)
}

async fn download_text(client: &Client, url: &str) -> Result<String> {
    client
        .get(url)
        .send()
        .await
        .with_context(|| format!("failed to download {url}"))?
        .error_for_status()?
        .text()
        .await
        .with_context(|| format!("failed to read {url}"))
}

async fn download_bytes(client: &Client, url: &str) -> Result<Vec<u8>> {
    Ok(client
        .get(url)
        .send()
        .await
        .with_context(|| format!("failed to download {url}"))?
        .error_for_status()?
        .bytes()
        .await?
        .to_vec())
}

fn core_asset() -> Result<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Ok("mihomo-linux-amd64-v2"),
        ("linux", "aarch64") => Ok("mihomo-linux-arm64"),
        ("linux", "arm") => Ok("mihomo-linux-armv7"),
        ("linux", "riscv64") => Ok("mihomo-linux-riscv64"),
        (os, arch) => bail!("Mihomo updater does not support {os}/{arch}"),
    }
}

fn version_numbers(value: &str) -> Vec<u64> {
    value
        .trim()
        .trim_start_matches('v')
        .split('.')
        .map(|part| {
            part.split(|character: char| !character.is_ascii_digit())
                .next()
                .unwrap_or("0")
                .parse()
                .unwrap_or(0)
        })
        .collect()
}

async fn install_executable(destination: &Path, bytes: &[u8]) -> Result<()> {
    let parent = destination
        .parent()
        .ok_or_else(|| anyhow::anyhow!("managed core path has no parent"))?;
    fs::create_dir_all(parent)?;
    let staged = destination.with_extension(format!("pending-{}", std::process::id()));
    fs::write(&staged, bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(&staged, fs::Permissions::from_mode(0o755))?;
    }
    let output = Command::new(&staged).arg("-v").output().await?;
    if !output.status.success() {
        let _ = fs::remove_file(&staged);
        bail!("downloaded Mihomo failed executable validation");
    }
    fs::rename(&staged, destination)?;
    Ok(())
}

fn atomic_replace(destination: &Path, bytes: &[u8]) -> Result<()> {
    let staged = destination.with_extension(format!("pending-{}", std::process::id()));
    fs::write(&staged, bytes)?;
    fs::rename(staged, destination)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_mihomo_versions_numerically() {
        assert!(version_numbers("1.20.0") > version_numbers("1.19.29"));
        assert_eq!(version_numbers("v1.19.29"), vec![1, 19, 29]);
    }
}
