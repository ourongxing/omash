use crate::{config::Config, core::CoreManager, enhance};
use anyhow::{Context, Result, bail};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_yaml_ng::Mapping;
use std::{
    fs,
    path::{Path, PathBuf},
};
use uuid::Uuid;

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Profiles {
    pub current: Option<String>,
    #[serde(default)]
    pub items: Vec<Profile>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Profile {
    pub uid: String,
    #[serde(rename = "type")]
    pub kind: ProfileKind,
    pub name: String,
    pub file: String,
    pub url: Option<String>,
    pub home: Option<String>,
    pub updated: i64,
    pub update_interval: Option<u64>,
    pub merge: Option<String>,
    pub rules: Option<String>,
    pub proxies: Option<String>,
    pub groups: Option<String>,
    pub subscription: Option<SubscriptionInfo>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub selected: Vec<ProfileSelection>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct ProfileSelection {
    pub name: String,
    pub now: String,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ProfileKind {
    #[default]
    Remote,
    Local,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
pub struct SubscriptionInfo {
    pub upload: u64,
    pub download: u64,
    pub total: u64,
    pub expire: u64,
}

struct FetchedProfile {
    content: String,
    subscription: Option<SubscriptionInfo>,
    home: Option<String>,
    update_interval: Option<u64>,
}

impl Profiles {
    pub fn load() -> Result<Self> {
        let path = Config::profiles_path();
        if !path.exists() {
            return Ok(Self::default());
        }
        serde_yaml_ng::from_str(&fs::read_to_string(&path)?)
            .with_context(|| format!("invalid profiles index {}", path.display()))
    }

    pub fn save(&self) -> Result<()> {
        atomic_write(
            &Config::profiles_path(),
            serde_yaml_ng::to_string(self)?.as_bytes(),
        )
    }

    pub async fn import_remote(
        &mut self,
        url: &str,
        name: Option<&str>,
        config: &Config,
    ) -> Result<String> {
        let fetched = fetch_remote_profile(url).await?;
        let uid = format!("R{}", Uuid::new_v4().simple());
        let file = format!("{uid}.yaml");
        let pending_file = format!(".{uid}.pending.yaml");
        let pending_path = Config::profiles_dir().join(&pending_file);
        atomic_write(&pending_path, fetched.content.as_bytes())?;
        let mut candidate = self.clone();
        candidate.items.push(Profile {
            uid: uid.clone(),
            kind: ProfileKind::Remote,
            name: name.unwrap_or("Remote Profile").into(),
            file: pending_file,
            url: Some(url.into()),
            home: fetched.home,
            updated: Utc::now().timestamp(),
            update_interval: fetched.update_interval,
            subscription: fetched.subscription,
            ..Profile::default()
        });
        let final_current = self.current.clone().or_else(|| Some(uid.clone()));
        candidate.current = Some(uid.clone());
        if let Err(error) = CoreManager::new().validate_only(config, &candidate).await {
            let _ = fs::remove_file(&pending_path);
            return Err(error).context("imported profile was rejected");
        }
        fs::rename(&pending_path, Config::profiles_dir().join(&file))?;
        candidate
            .items
            .last_mut()
            .ok_or_else(|| anyhow::anyhow!("imported profile disappeared before commit"))?
            .file = file;
        candidate.current = final_current;
        candidate.save()?;
        *self = candidate;
        Ok(uid)
    }

    pub async fn import_local(
        &mut self,
        source: &Path,
        name: Option<&str>,
        config: &Config,
    ) -> Result<String> {
        let content = fs::read_to_string(source)?;
        validate_yaml(&content)?;
        let uid = format!("L{}", Uuid::new_v4().simple());
        let file = format!("{uid}.yaml");
        let pending_file = format!(".{uid}.pending.yaml");
        let pending_path = Config::profiles_dir().join(&pending_file);
        atomic_write(&pending_path, content.as_bytes())?;
        let mut candidate = self.clone();
        candidate.items.push(Profile {
            uid: uid.clone(),
            kind: ProfileKind::Local,
            name: name
                .map(str::to_owned)
                .or_else(|| source.file_stem()?.to_str().map(str::to_owned))
                .unwrap_or_else(|| "Local Profile".into()),
            file: pending_file,
            updated: Utc::now().timestamp(),
            ..Profile::default()
        });
        let final_current = self.current.clone().or_else(|| Some(uid.clone()));
        candidate.current = Some(uid.clone());
        if let Err(error) = CoreManager::new().validate_only(config, &candidate).await {
            let _ = fs::remove_file(&pending_path);
            return Err(error).context("imported profile was rejected");
        }
        fs::rename(&pending_path, Config::profiles_dir().join(&file))?;
        candidate
            .items
            .last_mut()
            .ok_or_else(|| anyhow::anyhow!("imported profile disappeared before commit"))?
            .file = file;
        candidate.current = final_current;
        candidate.save()?;
        *self = candidate;
        Ok(uid)
    }

    pub async fn update_validated(&mut self, uid: &str, config: &Config) -> Result<()> {
        let item = self
            .items
            .iter()
            .find(|item| item.uid == uid)
            .ok_or_else(|| anyhow::anyhow!("profile not found"))?;
        let url = item
            .url
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("local profiles cannot be updated"))?;
        let fetched = fetch_remote_profile(url).await?;
        let pending_file = format!(".{uid}.pending.yaml");
        let pending_path = Config::profiles_dir().join(&pending_file);
        atomic_write(&pending_path, fetched.content.as_bytes())?;

        let mut candidate = self.clone();
        let final_file;
        {
            let candidate_item = candidate
                .items
                .iter_mut()
                .find(|item| item.uid == uid)
                .ok_or_else(|| anyhow::anyhow!("profile disappeared during update"))?;
            final_file = candidate_item.file.clone();
            candidate_item.file = pending_file;
            candidate_item.updated = Utc::now().timestamp();
            candidate_item.subscription = fetched.subscription;
            if fetched.home.is_some() {
                candidate_item.home = fetched.home;
            }
            if fetched.update_interval.is_some() {
                candidate_item.update_interval = fetched.update_interval;
            }
        }
        candidate.current = Some(uid.to_owned());

        let validation = CoreManager::new().validate_only(config, &candidate).await;
        if let Err(error) = validation {
            let _ = fs::remove_file(&pending_path);
            return Err(error).context("downloaded profile was rejected; previous profile kept");
        }

        let final_path = Config::profiles_dir().join(&final_file);
        fs::rename(&pending_path, &final_path)?;
        let committed_item = candidate
            .items
            .iter_mut()
            .find(|item| item.uid == uid)
            .ok_or_else(|| anyhow::anyhow!("validated profile disappeared before commit"))?;
        committed_item.file = final_file;
        candidate.current.clone_from(&self.current);
        candidate.save()?;
        *self = candidate;
        Ok(())
    }

    pub fn delete(&mut self, uid: &str) -> Result<()> {
        let index = self
            .items
            .iter()
            .position(|item| item.uid == uid)
            .ok_or_else(|| anyhow::anyhow!("profile not found"))?;
        let item = self.items.remove(index);
        let _ = fs::remove_file(Config::profiles_dir().join(item.file));
        if self.current.as_deref() == Some(uid) {
            self.current = self.items.first().map(|item| item.uid.clone());
        }
        self.save()
    }

    pub fn record_selection(&mut self, group: &str, node: &str) -> Result<()> {
        let uid = self
            .current
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("no profile selected"))?;
        let profile = self
            .items
            .iter_mut()
            .find(|item| item.uid == uid)
            .ok_or_else(|| anyhow::anyhow!("selected profile not found"))?;
        upsert_selection(&mut profile.selected, group, node);
        self.save()
    }

    pub fn current_selections(&self) -> &[ProfileSelection] {
        let Some(current) = self.current.as_deref() else {
            return &[];
        };
        self.items
            .iter()
            .find(|item| item.uid == current)
            .map_or(&[], |profile| profile.selected.as_slice())
    }

    pub fn build_runtime_at(&self, config: &Config, destination: &Path) -> Result<PathBuf> {
        let uid = self
            .current
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("no profile selected"))?;
        let item = self
            .items
            .iter()
            .find(|item| item.uid == uid)
            .ok_or_else(|| anyhow::anyhow!("selected profile not found"))?;
        let path = Config::profiles_dir().join(&item.file);
        let merge = item
            .merge
            .as_ref()
            .map(|name| Config::profiles_dir().join(name));
        let owned_chains: Vec<(&str, PathBuf)> = [
            ("rules", item.rules.as_ref()),
            ("proxies", item.proxies.as_ref()),
            ("proxy-groups", item.groups.as_ref()),
        ]
        .into_iter()
        .filter_map(|(key, file)| file.map(|file| (key, Config::profiles_dir().join(file))))
        .collect();
        let chains: Vec<_> = owned_chains
            .iter()
            .map(|(key, path)| (*key, path.as_path()))
            .collect();
        let mut runtime = enhance::build_runtime(&path, merge.as_deref(), &chains)?;
        enhance::apply_runtime_defaults(
            &mut runtime,
            &config.controller,
            &config.secret,
            config.mixed_port,
            config.allow_lan,
            config.ipv6,
            config.tun,
        );
        atomic_write(destination, serde_yaml_ng::to_string(&runtime)?.as_bytes())?;
        Ok(destination.to_path_buf())
    }
}

fn upsert_selection(selected: &mut Vec<ProfileSelection>, group: &str, node: &str) {
    match selected
        .iter_mut()
        .find(|selection| selection.name == group)
    {
        Some(selection) => selection.now = node.to_owned(),
        None => selected.push(ProfileSelection {
            name: group.to_owned(),
            now: node.to_owned(),
        }),
    }
}

async fn fetch_remote_profile(url: &str) -> Result<FetchedProfile> {
    let response = reqwest::Client::builder()
        .user_agent("clash-verge/v2.5.3 omash")
        .connect_timeout(std::time::Duration::from_secs(5))
        .timeout(std::time::Duration::from_secs(30))
        .build()?
        .get(url)
        .send()
        .await?
        .error_for_status()?;
    let subscription = response.headers().iter().find_map(|(key, value)| {
        let key = key.as_str().to_ascii_lowercase();
        key.strip_suffix("subscription-userinfo")
            .is_some_and(|prefix| prefix.is_empty() || prefix.ends_with('-'))
            .then(|| value.to_str().ok())
            .flatten()
            .and_then(parse_subscription_info)
    });
    let home = response
        .headers()
        .get("profile-web-page-url")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let update_interval = response
        .headers()
        .get("profile-update-interval")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());
    let content = response.text().await?;
    let content = content.trim_start_matches('\u{feff}').to_owned();
    validate_yaml(&content)?;
    Ok(FetchedProfile {
        content,
        subscription,
        home,
        update_interval,
    })
}

fn validate_yaml(content: &str) -> Result<Mapping> {
    let mapping: Mapping = serde_yaml_ng::from_str(content).context("profile is not valid YAML")?;
    if !mapping.contains_key("proxies") && !mapping.contains_key("proxy-providers") {
        bail!("profile has neither proxies nor proxy-providers");
    }
    Ok(mapping)
}

fn parse_subscription_info(value: &str) -> Option<SubscriptionInfo> {
    let mut info = SubscriptionInfo::default();
    for pair in value.split(';') {
        let (key, value) = pair.trim().split_once('=')?;
        let parsed = value.trim().parse().ok()?;
        match key.trim().to_ascii_lowercase().as_str() {
            "upload" => info.upload = parsed,
            "download" => info.download = parsed,
            "total" => info.total = parsed,
            "expire" => info.expire = parsed,
            _ => {}
        }
    }
    Some(info)
}

fn atomic_write(path: &Path, content: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension("tmp");
    fs::write(&temporary, content)?;
    fs::rename(&temporary, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_subscription_header() {
        let info = parse_subscription_info("upload=1; download=2; total=10; expire=20").unwrap();
        assert_eq!(
            (info.upload, info.download, info.total, info.expire),
            (1, 2, 10, 20)
        );
    }

    #[test]
    fn records_one_durable_selection_per_group() {
        let mut selected = Vec::new();
        upsert_selection(&mut selected, "GLOBAL", "node-a");
        upsert_selection(&mut selected, "GLOBAL", "node-b");
        upsert_selection(&mut selected, "AI", "node-c");
        assert_eq!(selected.len(), 2);
        assert_eq!(selected[0].now, "node-b");
    }
}
