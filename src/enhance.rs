use anyhow::{Context, Result, bail};
use serde_yaml_ng::{Mapping, Value};
use std::{fs, path::Path};

const SEQUENCES: [(&str, &str); 3] = [
    ("rules", "rules"),
    ("proxies", "proxies"),
    ("proxy-groups", "proxy-groups"),
];

pub fn build_runtime(
    base: &Path,
    merge: Option<&Path>,
    chains: &[(&str, &Path)],
) -> Result<Mapping> {
    let mut config = read_mapping(base)?;
    if let Some(path) = merge.filter(|path| path.exists()) {
        let patch = read_mapping(path)?;
        apply_merge(&mut config, patch);
    }
    for (key, path) in chains.iter().filter(|(_, path)| path.exists()) {
        let value: Value = serde_yaml_ng::from_str(&fs::read_to_string(path)?)
            .with_context(|| format!("invalid enhancement {}", path.display()))?;
        let sequence = match value {
            Value::Sequence(sequence) => sequence,
            Value::Mapping(mapping) => mapping
                .get(*key)
                .and_then(Value::as_sequence)
                .cloned()
                .unwrap_or_default(),
            _ => bail!("{} must contain a YAML sequence", path.display()),
        };
        config.insert(Value::String((*key).into()), Value::Sequence(sequence));
    }
    Ok(config)
}

pub fn apply_runtime_defaults(
    config: &mut Mapping,
    controller: &str,
    secret: &str,
    mixed_port: u16,
    allow_lan: bool,
    ipv6: bool,
) {
    let controller = controller
        .trim_start_matches("http://")
        .trim_start_matches("https://")
        .trim_end_matches('/');
    set(config, "external-controller", controller);
    set(config, "secret", secret);
    set(config, "mixed-port", mixed_port);
    set(config, "allow-lan", allow_lan);
    set(config, "ipv6", ipv6);
    let profile = config
        .entry(Value::String("profile".into()))
        .or_insert_with(|| Value::Mapping(Mapping::new()));
    if let Value::Mapping(mapping) = profile {
        mapping
            .entry(Value::String("store-selected".into()))
            .or_insert(Value::Bool(true));
    }
}

pub fn apply_singbox_defaults(
    config: &mut serde_json::Value,
    controller: &str,
    secret: &str,
    mixed_port: u16,
    allow_lan: bool,
) -> Result<()> {
    let controller = controller
        .trim_start_matches("http://")
        .trim_start_matches("https://")
        .trim_end_matches('/');
    let root = config
        .as_object_mut()
        .context("sing-box configuration must be a JSON object")?;
    let experimental = root
        .entry("experimental".to_owned())
        .or_insert_with(|| serde_json::json!({}));
    let experimental = experimental
        .as_object_mut()
        .context("experimental must be an object")?;
    let clash_api = experimental
        .entry("clash_api".to_owned())
        .or_insert_with(|| serde_json::json!({}));
    let clash_api = clash_api
        .as_object_mut()
        .context("clash_api must be an object")?;
    clash_api.insert("external_controller".to_owned(), controller.into());
    clash_api.insert("secret".to_owned(), secret.into());
    clash_api
        .entry("default_mode".to_owned())
        .or_insert_with(|| "rule".into());
    // sing-box ≥1.11 always persists selected proxies once cache_file is
    // enabled; the removed store_selected flag must not be re-added.
    let cache_file = experimental
        .entry("cache_file".to_owned())
        .or_insert_with(|| serde_json::json!({}));
    let cache_file = cache_file
        .as_object_mut()
        .context("cache_file must be an object")?;
    cache_file.insert("enabled".to_owned(), true.into());

    let inbounds = root
        .entry("inbounds".to_owned())
        .or_insert_with(|| serde_json::json!([]));
    let inbounds = inbounds
        .as_array_mut()
        .context("inbounds must be an array")?;
    let port_taken = inbounds.iter().any(|inbound| {
        inbound
            .get("listen_port")
            .and_then(serde_json::Value::as_u64)
            .is_some_and(|port| port == mixed_port as u64)
    });
    if !port_taken {
        inbounds.push(serde_json::json!({
            "type": "mixed",
            "tag": "omash-mixed",
            "listen": if allow_lan { "0.0.0.0" } else { "127.0.0.1" },
            "listen_port": mixed_port,
        }));
    }

    // Clash modes other than rule only exist in sing-box as clash_mode route
    // rules. Seed them from the config's own outbounds so mode switching works
    // without the user having to write the rules by hand.
    let outbounds = root
        .get("outbounds")
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default();
    let outbound_tag = |wanted: &str| {
        outbounds
            .iter()
            .find(|outbound| {
                outbound.get("type").and_then(serde_json::Value::as_str) == Some(wanted)
            })
            .and_then(|outbound| outbound.get("tag"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
    };
    let has_clash_mode_rules = root
        .get("route")
        .and_then(|route| route.get("rules"))
        .and_then(serde_json::Value::as_array)
        .is_some_and(|rules| rules.iter().any(|rule| rule.get("clash_mode").is_some()));
    if !has_clash_mode_rules {
        let global_target = outbound_tag("selector").or_else(|| {
            root.get("route")
                .and_then(|route| route.get("final"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        });
        let direct_target = outbounds
            .iter()
            .find(|outbound| {
                outbound.get("tag").and_then(serde_json::Value::as_str) == Some("direct")
            })
            .or_else(|| {
                outbounds.iter().find(|outbound| {
                    outbound.get("type").and_then(serde_json::Value::as_str) == Some("direct")
                })
            })
            .and_then(|outbound| outbound.get("tag"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        if global_target.is_some() || direct_target.is_some() {
            let route = root
                .entry("route".to_owned())
                .or_insert_with(|| serde_json::json!({}));
            let route = route.as_object_mut().context("route must be an object")?;
            let rules = route
                .entry("rules".to_owned())
                .or_insert_with(|| serde_json::json!([]));
            let rules = rules.as_array_mut().context("rules must be an array")?;
            let mut seeded = Vec::with_capacity(rules.len() + 2);
            if let Some(target) = global_target {
                seeded.push(serde_json::json!({ "clash_mode": "Global", "outbound": target }));
            }
            if let Some(target) = direct_target {
                seeded.push(serde_json::json!({ "clash_mode": "Direct", "outbound": target }));
            }
            seeded.append(rules);
            *rules = seeded;
        }
    }
    Ok(())
}

fn apply_merge(config: &mut Mapping, mut patch: Mapping) {
    for (name, target) in SEQUENCES {
        let prepend = patch.remove(Value::String(format!("prepend-{name}")));
        let append = patch.remove(Value::String(format!("append-{name}")));
        if prepend.is_some() || append.is_some() {
            let existing = config
                .get(target)
                .and_then(Value::as_sequence)
                .cloned()
                .unwrap_or_default();
            let mut combined = prepend
                .and_then(|v| v.as_sequence().cloned())
                .unwrap_or_default();
            combined.extend(existing);
            combined.extend(
                append
                    .and_then(|v| v.as_sequence().cloned())
                    .unwrap_or_default(),
            );
            patch.insert(Value::String(target.into()), Value::Sequence(combined));
        }
    }
    deep_merge(config, patch);
}

fn deep_merge(target: &mut Mapping, patch: Mapping) {
    for (key, value) in patch {
        match (target.get_mut(&key), value) {
            (Some(Value::Mapping(existing)), Value::Mapping(incoming)) => {
                deep_merge(existing, incoming)
            }
            (_, value) => {
                target.insert(key, value);
            }
        }
    }
}

fn read_mapping(path: &Path) -> Result<Mapping> {
    serde_yaml_ng::from_str(
        &fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?,
    )
    .with_context(|| format!("invalid YAML in {}", path.display()))
}

fn set(mapping: &mut Mapping, key: &str, value: impl Into<Value>) {
    mapping.insert(Value::String(key.into()), value.into());
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn merge_supports_prepend_and_append() {
        let mut base: Mapping =
            serde_yaml_ng::from_str("rules: [base]\ndns: {enable: false}\n").unwrap();
        let patch: Mapping = serde_yaml_ng::from_str(
            "prepend-rules: [first]\nappend-rules: [last]\ndns: {enable: true}\n",
        )
        .unwrap();
        apply_merge(&mut base, patch);
        assert_eq!(base["rules"].as_sequence().unwrap().len(), 3);
        assert_eq!(base["dns"]["enable"], Value::Bool(true));
    }

    #[test]
    fn runtime_defaults_store_selected_nodes() {
        let mut config: Mapping = serde_yaml_ng::from_str("tun: {enable: true}\n").unwrap();
        apply_runtime_defaults(
            &mut config,
            "http://127.0.0.1:9090",
            "secret",
            7897,
            false,
            true,
        );
        assert_eq!(config["profile"]["store-selected"], Value::Bool(true));
        assert!(config.contains_key("tun"));
    }

    #[test]
    fn singbox_defaults_inject_clash_api_and_mixed_inbound() {
        let mut config: serde_json::Value =
            serde_json::from_str(r#"{"outbounds":[{"type":"direct","tag":"direct"}]}"#).unwrap();
        apply_singbox_defaults(
            &mut config,
            "http://127.0.0.1:9090",
            "topsecret",
            7897,
            false,
        )
        .unwrap();
        assert_eq!(
            config["experimental"]["clash_api"]["external_controller"],
            "127.0.0.1:9090"
        );
        assert_eq!(config["experimental"]["clash_api"]["secret"], "topsecret");
        assert_eq!(config["experimental"]["clash_api"]["default_mode"], "rule");
        assert_eq!(config["experimental"]["cache_file"]["enabled"], true);
        let inbound = &config["inbounds"][0];
        assert_eq!(inbound["type"], "mixed");
        assert_eq!(inbound["listen_port"], 7897);
        assert_eq!(inbound["listen"], "127.0.0.1");
    }

    #[test]
    fn singbox_defaults_keep_existing_inbound_on_mixed_port() {
        let mut config: serde_json::Value = serde_json::from_str(
            r#"{"inbounds":[{"type":"mixed","listen_port":7897,"listen":"127.0.0.1"}],"outbounds":[]}"#,
        )
        .unwrap();
        apply_singbox_defaults(&mut config, "127.0.0.1:9090", "", 7897, true).unwrap();
        assert_eq!(config["inbounds"].as_array().unwrap().len(), 1);
        // allow_lan must not rewrite the user's own inbound listen address.
        assert_eq!(config["inbounds"][0]["listen"], "127.0.0.1");
        assert_eq!(config["experimental"]["clash_api"]["secret"], "");
    }

    #[test]
    fn singbox_defaults_preserve_existing_default_mode() {
        let mut config: serde_json::Value = serde_json::from_str(
            r#"{"experimental":{"clash_api":{"default_mode":"global"}},"outbounds":[]}"#,
        )
        .unwrap();
        apply_singbox_defaults(&mut config, "127.0.0.1:9090", "s", 7897, false).unwrap();
        assert_eq!(
            config["experimental"]["clash_api"]["default_mode"],
            "global"
        );
    }

    #[test]
    fn singbox_defaults_seed_clash_mode_rules() {
        let mut config: serde_json::Value = serde_json::from_str(
            r#"{
                "outbounds": [
                    {"type":"selector","tag":"sel","outbounds":["direct"]},
                    {"type":"direct","tag":"direct"}
                ],
                "route": {
                    "rules": [{"domain_suffix":"example.com","outbound":"sel"}],
                    "final": "sel"
                }
            }"#,
        )
        .unwrap();
        apply_singbox_defaults(&mut config, "127.0.0.1:9090", "", 7897, false).unwrap();
        let rules = config["route"]["rules"].as_array().unwrap();
        assert_eq!(
            rules[0],
            serde_json::json!({"clash_mode": "Global", "outbound": "sel"})
        );
        assert_eq!(
            rules[1],
            serde_json::json!({"clash_mode": "Direct", "outbound": "direct"})
        );
        assert_eq!(rules[2]["domain_suffix"], "example.com");
        assert_eq!(rules.len(), 3);
    }

    #[test]
    fn singbox_defaults_keep_user_clash_mode_rules() {
        let mut config: serde_json::Value = serde_json::from_str(
            r#"{
                "outbounds": [{"type":"selector","tag":"sel","outbounds":["direct"]}],
                "route": {"rules": [{"clash_mode":"Global","outbound":"sel"}]}
            }"#,
        )
        .unwrap();
        apply_singbox_defaults(&mut config, "127.0.0.1:9090", "", 7897, false).unwrap();
        let rules = config["route"]["rules"].as_array().unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0]["clash_mode"], "Global");
    }

    #[test]
    fn singbox_defaults_seed_direct_mode_without_selector() {
        let mut config: serde_json::Value =
            serde_json::from_str(r#"{"outbounds":[{"type":"direct","tag":"direct"}]}"#).unwrap();
        apply_singbox_defaults(&mut config, "127.0.0.1:9090", "", 7897, false).unwrap();
        let rules = config["route"]["rules"].as_array().unwrap();
        assert_eq!(
            rules[0],
            serde_json::json!({"clash_mode": "Direct", "outbound": "direct"})
        );
        assert_eq!(rules.len(), 1);
    }
}
