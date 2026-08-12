use crate::{
    api::{MihomoClient, ProxyResponse},
    config::{BarCommand, Config},
    profiles::Profiles,
};
use anyhow::Result;
use serde::Serialize;
use std::collections::HashMap;

#[derive(Debug, Serialize)]
struct BarState {
    online: bool,
    mode: String,
    groups: Vec<BarGroup>,
    delays: HashMap<String, u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, Serialize)]
struct BarGroup {
    name: String,
    now: String,
    all: Vec<String>,
}

pub async fn run(config: &Config, command: &BarCommand) -> Result<()> {
    let api = MihomoClient::new(&config.controller, config.secret.clone())?;
    match command {
        BarCommand::State => print_state(&api).await,
        BarCommand::Mode { mode } => api.set_mode(mode.as_str()).await,
        BarCommand::Proxy { group, proxy } => {
            api.select_proxy(group, proxy).await?;
            if let Err(error) =
                Profiles::load().and_then(|mut profiles| profiles.record_selection(group, proxy))
            {
                eprintln!("proxy changed but selection was not saved: {error}");
            }
            Ok(())
        }
        BarCommand::Delay { group } => print_delays(&api, group, &config.delay_test_url).await,
    }
}

async fn print_state(api: &MihomoClient) -> Result<()> {
    let state = match tokio::try_join!(api.runtime_config(), api.proxies()) {
        Ok((config, proxies)) => {
            let delays = proxy_delays(&proxies);
            let group_order = Config::proxy_group_order();
            BarState {
                online: true,
                mode: config.mode.to_ascii_lowercase(),
                groups: groups(proxies, &group_order),
                delays,
                error: None,
            }
        }
        Err(error) => BarState {
            online: false,
            mode: String::new(),
            groups: Vec::new(),
            delays: HashMap::new(),
            error: Some(error.to_string()),
        },
    };
    println!("{}", serde_json::to_string(&state)?);
    Ok(())
}

async fn print_delays(api: &MihomoClient, group: &str, target: &str) -> Result<()> {
    let delays = api.test_group_delay(group, target).await?;
    println!(
        "{}",
        serde_json::to_string(&serde_json::json!({
            "group": group,
            "delays": delays,
        }))?
    );
    Ok(())
}

fn proxy_delays(proxies: &ProxyResponse) -> HashMap<String, u32> {
    proxies
        .proxies
        .iter()
        .filter_map(|(name, proxy)| {
            proxy
                .history
                .last()
                .filter(|history| history.delay > 0)
                .map(|history| (name.clone(), history.delay))
        })
        .collect()
}

fn groups(mut proxies: ProxyResponse, configured_order: &[String]) -> Vec<BarGroup> {
    let mut groups: Vec<_> = configured_order
        .iter()
        .filter_map(|name| {
            let proxy = proxies.proxies.remove(name)?;
            proxy
                .kind
                .eq_ignore_ascii_case("selector")
                .then(|| BarGroup {
                    name: name.clone(),
                    now: proxy.now,
                    all: proxy.all,
                })
        })
        .collect();

    let mut unconfigured: Vec<_> = proxies
        .proxies
        .into_iter()
        .filter(|(name, proxy)| name != "GLOBAL" && proxy.kind.eq_ignore_ascii_case("selector"))
        .map(|(name, proxy)| BarGroup {
            name,
            now: proxy.now,
            all: proxy.all,
        })
        .collect();
    unconfigured.sort_by_key(|group| group.name.to_ascii_lowercase());
    groups.extend(unconfigured);
    groups
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::Proxy;
    use std::collections::HashMap;

    #[test]
    fn status_bar_preserves_configured_selector_group_order() {
        let proxies = ProxyResponse {
            proxies: HashMap::from([
                (
                    "Fallback".into(),
                    Proxy {
                        kind: "Selector".into(),
                        now: "B".into(),
                        all: vec!["A".into(), "B".into()],
                        ..Proxy::default()
                    },
                ),
                (
                    "Auto".into(),
                    Proxy {
                        kind: "Selector".into(),
                        now: "A".into(),
                        all: vec!["A".into()],
                        ..Proxy::default()
                    },
                ),
                ("DIRECT".into(), Proxy::default()),
            ]),
        };

        let groups = groups(proxies, &["Fallback".into(), "Auto".into()]);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].name, "Fallback");
        assert_eq!(groups[1].name, "Auto");
        assert_eq!(groups[0].now, "B");
    }

    #[test]
    fn status_bar_uses_latest_positive_proxy_delay() {
        let proxies = ProxyResponse {
            proxies: HashMap::from([
                (
                    "Fast".into(),
                    Proxy {
                        history: vec![
                            crate::api::DelayHistory { delay: 90 },
                            crate::api::DelayHistory { delay: 120 },
                        ],
                        ..Proxy::default()
                    },
                ),
                (
                    "Failed".into(),
                    Proxy {
                        history: vec![crate::api::DelayHistory { delay: 0 }],
                        ..Proxy::default()
                    },
                ),
            ]),
        };

        let delays = proxy_delays(&proxies);
        assert_eq!(delays.get("Fast"), Some(&120));
        assert!(!delays.contains_key("Failed"));
    }
}
