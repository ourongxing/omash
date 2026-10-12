use anyhow::{Context, Result, bail};
use serde_yaml_ng::{Mapping, Value};

/// Applied after all profile enhancements; the application owns the enable bit.
pub fn apply(root: &mut Mapping, enabled: bool, ipv6: bool) -> Result<()> {
    if root
        .get("listeners")
        .and_then(Value::as_sequence)
        .is_some_and(|listeners| {
            listeners
                .iter()
                .any(|v| v.get("type").and_then(Value::as_str) == Some("tun"))
        })
    {
        bail!("additional TUN listeners are unsupported; use the top-level tun settings");
    }
    let tun = section(root, "tun")?;
    tun.insert("enable".into(), enabled.into());
    if !enabled {
        return Ok(());
    }
    for (key, value) in [
        ("device", Value::from("omash-tun")),
        ("stack", Value::from("mixed")),
        ("auto-route", true.into()),
        ("auto-detect-interface", true.into()),
        ("auto-redirect", false.into()),
        ("strict-route", false.into()),
        (
            "dns-hijack",
            serde_yaml_ng::to_value(["any:53", "tcp://any:53"])?,
        ),
    ] {
        tun.entry(key.into()).or_insert(value);
    }
    let device = tun
        .get("device")
        .and_then(Value::as_str)
        .context("tun.device must be a string")?;
    validate_device(device)?;
    // Mihomo replaces only a zero index with its default, so anything else that is not a
    // kernel table ID or rule priority would reach sing-tun as it is. sing-tun places rules up
    // to ten priorities above the rule index, and those must not wrap past the u32 range.
    for (key, max) in [
        ("iproute2-table-index", u32::MAX),
        ("iproute2-rule-index", u32::MAX - 10),
        ("auto-redirect-iproute2-fallback-rule-index", u32::MAX),
    ] {
        if let Some(value) = tun.get(key)
            && !value.as_u64().is_some_and(|v| v <= u64::from(max))
        {
            bail!("tun.{key} must be an integer from 0 to {max}");
        }
    }
    if let Some(fd) = tun.get("file-descriptor")
        && fd.as_i64() != Some(0)
    {
        bail!("tun.file-descriptor is not supported by the managed service");
    }
    if ipv6 {
        tun.entry("inet6-address".into())
            .or_insert(serde_yaml_ng::to_value(["fdfe:dcba:9876::1/126"])?);
    } else {
        tun.remove("inet6-address");
        tun.remove("inet6-route-address");
    }
    let dns = section(root, "dns")?;
    dns.insert("enable".into(), true.into());
    dns.insert("ipv6".into(), ipv6.into());
    dns.entry("enhanced-mode".into())
        .or_insert("fake-ip".into());
    let fake_ip = dns
        .get("enhanced-mode")
        .and_then(Value::as_str)
        .is_some_and(|mode| mode.eq_ignore_ascii_case("fake-ip"));
    dns.entry("fake-ip-range".into())
        .or_insert("198.18.0.1/16".into());
    for key in ["default-nameserver", "nameserver"] {
        let upstreams = dns
            .entry(key.into())
            .or_insert(serde_yaml_ng::to_value(["1.1.1.1", "8.8.8.8"])?);
        if !upstreams.as_sequence().is_some_and(|v| {
            !v.is_empty()
                && v.iter()
                    .all(|v| v.as_str().is_some_and(|v| !v.trim().is_empty()))
        }) {
            bail!("dns.{key} must contain at least one nonempty upstream");
        }
    }
    if fake_ip {
        // Applications keep the fake addresses they resolved; without the cache every core
        // restart would break their connections.
        section(root, "profile")?
            .entry("store-fake-ip".into())
            .or_insert(true.into());
    }
    Ok(())
}

fn section<'a>(root: &'a mut Mapping, key: &str) -> Result<&'a mut Mapping> {
    let value = root
        .entry(key.into())
        .or_insert(Value::Mapping(Mapping::new()));
    // A key without a value is an empty section for the core as well.
    if value.is_null() {
        *value = Value::Mapping(Mapping::new());
    }
    value
        .as_mapping_mut()
        .with_context(|| format!("{key} must be a mapping"))
}

pub fn validate_device(device: &str) -> Result<()> {
    if device.is_empty()
        || device.len() > 15
        || !device
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        bail!("tun.device must use 1–15 ASCII letters, digits, '-' or '_'");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn yaml(s: &str) -> Mapping {
        serde_yaml_ng::from_str(s).unwrap()
    }

    #[test]
    fn tun_disabled_cannot_be_overridden_by_profile() {
        let mut root = yaml("tun: {enable: true, mtu: 1400}");
        apply(&mut root, false, true).unwrap();
        assert_eq!(root["tun"]["enable"], Value::Bool(false));
    }
    #[test]
    fn tun_preserves_merge_and_dns_policy() {
        let mut root = yaml(
            "tun: {stack: system, mtu: 1400}\ndns: {enhanced-mode: redir-host, nameserver: [9.9.9.9], nameserver-policy: {'example.com': 1.1.1.1}}",
        );
        apply(&mut root, true, true).unwrap();
        assert_eq!(root["tun"]["mtu"].as_u64(), Some(1400));
        assert_eq!(root["tun"]["stack"].as_str(), Some("system"));
        assert_eq!(
            root["dns"]["nameserver-policy"]["example.com"].as_str(),
            Some("1.1.1.1")
        );
        assert_eq!(root["dns"]["enhanced-mode"].as_str(), Some("redir-host"));
    }
    #[test]
    fn tun_seeds_missing_dns_upstreams() {
        let mut root = Mapping::new();
        apply(&mut root, true, true).unwrap();
        assert_eq!(root["dns"]["enable"], Value::Bool(true));
        assert!(!root["dns"]["nameserver"].as_sequence().unwrap().is_empty());
        assert!(root["tun"]["inet6-address"].as_sequence().is_some());
    }
    #[test]
    fn tun_rejects_extra_listener() {
        for enabled in [true, false] {
            assert!(
                apply(
                    &mut yaml("listeners: [{name: bypass, type: tun}]"),
                    enabled,
                    true
                )
                .is_err()
            );
        }
    }
    #[test]
    fn tun_rejects_empty_upstreams() {
        assert!(apply(&mut yaml("dns: {nameserver: []}"), true, true).is_err());
    }
    #[test]
    fn tun_rejects_routing_indexes_mihomo_would_not_default() {
        // Mihomo replaces only zero with its default; a negative rule index would make sing-tun
        // sweep the kernel's own rules from that priority on.
        for key in [
            "iproute2-table-index",
            "iproute2-rule-index",
            "auto-redirect-iproute2-fallback-rule-index",
        ] {
            for value in ["-1", "1.5", "abc"] {
                let error =
                    apply(&mut yaml(&format!("tun: {{{key}: {value}}}")), true, false).unwrap_err();
                assert!(
                    format!("{error:#}").contains(key),
                    "{key}: {value}: {error:#}"
                );
            }
            apply(&mut yaml(&format!("tun: {{{key}: 0}}")), true, false).unwrap();
            apply(&mut yaml(&format!("tun: {{{key}: 12000}}")), true, false).unwrap();
        }
        // sing-tun places rules up to ten priorities above the rule index; past the u32 range
        // they would wrap to low priorities that neither Mihomo nor omash would clean up.
        let max = u64::from(u32::MAX);
        for value in [max - 9, max] {
            let error = apply(
                &mut yaml(&format!("tun: {{iproute2-rule-index: {value}}}")),
                true,
                false,
            )
            .unwrap_err();
            assert!(
                format!("{error:#}").contains("iproute2-rule-index"),
                "{value}: {error:#}"
            );
        }
        apply(
            &mut yaml(&format!("tun: {{iproute2-rule-index: {}}}", max - 10)),
            true,
            false,
        )
        .unwrap();
        for key in [
            "iproute2-table-index",
            "auto-redirect-iproute2-fallback-rule-index",
        ] {
            apply(&mut yaml(&format!("tun: {{{key}: {max}}}")), true, false).unwrap();
        }
    }
    #[test]
    fn fake_ip_mappings_are_stored_by_default_and_for_an_explicit_fake_ip() {
        let mut root = Mapping::new();
        apply(&mut root, true, false).unwrap();
        assert_eq!(root["dns"]["enhanced-mode"].as_str(), Some("fake-ip"));
        assert_eq!(root["profile"]["store-fake-ip"], Value::Bool(true));
        for mode in ["fake-ip", "Fake-IP", "FAKE-IP"] {
            let mut root = yaml(&format!(
                "dns: {{enhanced-mode: {mode}}}\nprofile: {{store-selected: true}}"
            ));
            apply(&mut root, true, true).unwrap();
            assert_eq!(
                root["profile"]["store-fake-ip"],
                Value::Bool(true),
                "{mode}"
            );
            assert_eq!(root["profile"]["store-selected"], Value::Bool(true));
        }
    }
    #[test]
    fn fake_ip_cache_is_not_added_for_redir_host_or_without_tun() {
        let mut root = yaml("dns: {enhanced-mode: redir-host}");
        apply(&mut root, true, true).unwrap();
        assert!(root.get("profile").is_none());
        let mut root = yaml("profile: {store-selected: true}");
        apply(&mut root, false, true).unwrap();
        assert!(root["profile"].get("store-fake-ip").is_none());
        assert!(root.get("dns").is_none());
    }
    #[test]
    fn explicit_fake_ip_cache_setting_is_respected() {
        let mut root = yaml("profile: {store-fake-ip: false}");
        apply(&mut root, true, true).unwrap();
        assert_eq!(root["profile"]["store-fake-ip"], Value::Bool(false));
    }
    #[test]
    fn profile_section_must_be_a_mapping_but_may_be_empty() {
        let mut root = yaml("profile:");
        apply(&mut root, true, true).unwrap();
        assert_eq!(root["profile"]["store-fake-ip"], Value::Bool(true));
        assert!(apply(&mut yaml("profile: 5"), true, true).is_err());
        // Not consulted when the core keeps no fake addresses.
        apply(
            &mut yaml("profile: 5\ndns: {enhanced-mode: redir-host}"),
            true,
            true,
        )
        .unwrap();
    }
    #[test]
    fn legacy_tun_does_not_auto_enable() {
        let config: crate::config::Config = toml::from_str("tun = true").unwrap();
        let json = serde_json::to_value(config).unwrap();
        assert_eq!(json["tun_enabled"], false);
    }
}
