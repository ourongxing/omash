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
    config.remove("tun");
    let profile = config
        .entry(Value::String("profile".into()))
        .or_insert_with(|| Value::Mapping(Mapping::new()));
    if let Value::Mapping(mapping) = profile {
        mapping
            .entry(Value::String("store-selected".into()))
            .or_insert(Value::Bool(true));
    }
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
        assert!(!config.contains_key("tun"));
    }
}
