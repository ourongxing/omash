//! Runtime exceptions for packets reinjected by Mihomo's kernel TUN stack.
//! An accept in a separate nft base chain cannot override another chain's drop.
//! Put an interface-scoped exception first in each existing input filter chain.
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::{fs, path::PathBuf, process::Stdio, time::Duration};
use tokio::{io::AsyncWriteExt, process::Command};

pub(super) fn program() -> Result<PathBuf> {
    for path in ["/usr/bin/nft", "/usr/sbin/nft"] {
        if let Ok(path) = fs::canonicalize(path) {
            super::install::trusted_path(&path, true)?;
            return Ok(path);
        }
    }
    bail!(
        "TUN firewall management requires nftables; install the nftables package and run `omash tun setup`"
    )
}

pub(super) fn kernel_stack(root: &serde_yaml_ng::Mapping) -> bool {
    !root
        .get("tun")
        .and_then(|v| v.get("stack"))
        .and_then(serde_yaml_ng::Value::as_str)
        .is_some_and(|stack| matches!(stack.to_ascii_lowercase().as_str(), "gvisor" | "mips"))
}

/// nftables can neither see nor change legacy xtables rules, and the unprivileged
/// helper cannot read their root-only table list. A loaded legacy filter module is
/// therefore only reported by `omash tun doctor`; it never blocks TUN.
pub(super) fn legacy_filter_warning() -> Option<String> {
    legacy_filter_hint(|module| std::path::Path::new("/sys/module").join(module).exists())
}

fn legacy_filter_hint(loaded: impl Fn(&str) -> bool) -> Option<String> {
    let modules: Vec<&str> = ["iptable_filter", "ip6table_filter"]
        .into_iter()
        .filter(|module| loaded(module))
        .collect();
    (!modules.is_empty()).then(|| {
        format!(
            "legacy iptables filter module loaded ({}); omash manages only nftables exceptions. If TCP through TUN times out, allow the TUN interface in that firewall",
            modules.join(", ")
        )
    })
}

pub(super) struct Firewall {
    program: PathBuf,
    tag: String,
}

/// Arguments of the ruleset snapshot that every reconciliation reads. `--terse`
/// omits set elements, so the cost does not grow with large blocklist sets.
const SNAPSHOT: [&str; 5] = ["--json", "--numeric", "--terse", "list", "ruleset"];

impl Firewall {
    pub fn new(uid: u32) -> Result<Self> {
        Ok(Self {
            program: program()?,
            tag: format!("omash-tun:{uid}:input"),
        })
    }

    async fn run(&self, args: &[&str], input: Option<&Value>) -> Result<Value> {
        let mut command = Command::new(&self.program);
        command
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .args(args)
            .stdin(if input.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let output = tokio::time::timeout(Duration::from_secs(3), async {
            let mut child = command.spawn().context("cannot start nft")?;
            if let Some(input) = input {
                let mut stdin = child.stdin.take().context("missing nft input")?;
                stdin.write_all(&serde_json::to_vec(input)?).await?;
                stdin.shutdown().await?;
            }
            child
                .wait_with_output()
                .await
                .context("cannot read nft result")
        })
        .await
        .context("nft firewall operation timed out (3s)")??;
        if !output.status.success() {
            bail!(
                "nft firewall operation failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        if input.is_some() {
            Ok(Value::Null)
        } else {
            serde_json::from_slice(&output.stdout).context("invalid nft ruleset")
        }
    }

    pub async fn check(&self) -> Result<()> {
        plan(&self.snapshot().await?, &self.tag, None)?;
        Ok(())
    }

    async fn snapshot(&self) -> Result<Value> {
        self.run(&SNAPSHOT, None).await
    }

    /// Idempotent, atomic batches; a concurrent UFW reload can invalidate a handle,
    /// so retry from a fresh snapshot. Never flush a chain or change its policy.
    pub async fn reconcile(&self, device: Option<&str>) -> Result<()> {
        if let Some(device) = device {
            super::config::validate_device(device)?;
        }
        let mut last_error = None;
        for _ in 0..3 {
            let commands = plan(&self.snapshot().await?, &self.tag, device)?;
            if commands.is_empty() {
                return Ok(());
            }
            let result = self
                .run(
                    &["--json", "--file", "-"],
                    Some(&json!({"nftables": commands})),
                )
                .await;
            if let Err(error) = result {
                last_error = Some(error);
            }
            // Also re-read after success: readiness includes confirmation, not just a write.
        }
        if plan(&self.snapshot().await?, &self.tag, device)?.is_empty() {
            return Ok(());
        }
        Err(last_error
            .unwrap_or_else(|| anyhow::anyhow!("firewall rules changed during reconciliation")))
    }
}

fn expression(device: &str) -> Value {
    // Keep the representation compatible with iptables-nft (and thus UFW).
    json!([
        {"match": {"op": "==", "left": {"meta": {"key": "iifname"}}, "right": device}},
        {"counter": {"packets": 0, "bytes": 0}},
        {"accept": null}
    ])
}

fn owned_device<'a>(rule: &'a Value, tag: &str) -> Option<&'a str> {
    if rule["comment"].as_str() != Some(tag) {
        return None;
    }
    let expr = rule["expr"].as_array()?;
    let device = expr.first()?["match"]["right"].as_str()?;
    if super::config::validate_device(device).is_err() {
        return None;
    }
    let expected = expression(device);
    (expr.len() == 3
        && expr[0] == expected[0]
        && expr[1].as_object().is_some_and(|v| v.len() == 1)
        && expr[1]["counter"]
            .as_object()
            .is_some_and(|v| v.len() == 2 && v["packets"].is_u64() && v["bytes"].is_u64())
        && expr[2] == expected[2])
        .then_some(device)
}

fn plan(snapshot: &Value, tag: &str, device: Option<&str>) -> Result<Vec<Value>> {
    let objects = snapshot["nftables"]
        .as_array()
        .context("missing nftables ruleset")?;
    let mut commands = Vec::new();
    for chain in objects.iter().filter_map(|v| v.get("chain")) {
        if !matches!(chain["family"].as_str(), Some("ip" | "ip6" | "inet"))
            || chain["type"] != "filter"
            || chain["hook"] != "input"
        {
            continue;
        }
        let rules: Vec<_> = objects
            .iter()
            .filter_map(|v| v.get("rule"))
            .filter(|v| {
                v["family"] == chain["family"]
                    && v["table"] == chain["table"]
                    && v["chain"] == chain["name"]
            })
            .collect();
        let keep_first = device.is_some()
            && rules
                .first()
                .is_some_and(|r| owned_device(r, tag) == device);
        for (index, rule) in rules.iter().enumerate() {
            if owned_device(rule, tag).is_some() && !(keep_first && index == 0) {
                let handle = rule["handle"]
                    .as_u64()
                    .context("owned firewall rule has no handle")?;
                commands.push(json!({"delete": {"rule": {
                    "family": chain["family"], "table": chain["table"], "chain": chain["name"], "handle": handle
                }}}));
            }
        }
        if !keep_first && let Some(device) = device {
            commands.push(json!({"insert": {"rule": {
                "family": chain["family"], "table": chain["table"], "chain": chain["name"],
                "expr": expression(device), "comment": tag
            }}}));
        }
    }
    Ok(commands)
}

#[cfg(test)]
mod tests {
    use super::*;
    const TAG: &str = "omash-tun:1000:input";

    fn chain(family: &str, hook: &str) -> Value {
        json!({"chain": {"family": family, "table": "filter", "name": hook, "type": "filter", "hook": hook, "policy": "drop"}})
    }
    fn rule(device: &str, handle: u64) -> Value {
        json!({"rule": {"family": "ip", "table": "filter", "chain": "input", "handle": handle, "expr": expression(device), "comment": TAG}})
    }

    #[test]
    fn every_input_chain_is_covered_but_forward_and_output_are_untouched() {
        let snapshot = json!({"nftables": [chain("ip", "input"), chain("ip6", "input"), chain("inet", "input"), chain("ip", "forward"), chain("ip", "output"), chain("bridge", "input")]});
        let commands = plan(&snapshot, TAG, Some("custom-tun")).unwrap();
        assert_eq!(commands.len(), 3);
        for command in commands {
            assert_eq!(command["insert"]["rule"]["chain"], "input");
            assert_eq!(
                command["insert"]["rule"]["expr"][0]["match"]["right"],
                "custom-tun"
            );
        }
        assert!(
            plan(&json!({"nftables": []}), TAG, Some("omash-tun"))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn preserves_counters_and_does_not_rewrite_an_unchanged_rule() {
        let mut active = rule("omash-tun", 2);
        active["rule"]["expr"][1]["counter"] = json!({"packets": 123, "bytes": 456});
        let snapshot = json!({"nftables": [chain("ip", "input"), active]});
        assert!(plan(&snapshot, TAG, Some("omash-tun")).unwrap().is_empty());
    }

    #[test]
    fn reload_order_duplicates_and_device_changes_are_repaired() {
        let foreign = json!({"rule": {"family": "ip", "table": "filter", "chain": "input", "handle": 1, "expr": [{"drop": null}]}});
        let snapshot = json!({"nftables": [chain("ip", "input"), foreign, rule("old-tun", 2), rule("omash-tun", 3)]});
        let commands = plan(&snapshot, TAG, Some("omash-tun")).unwrap();
        assert_eq!(commands.len(), 3);
        assert_eq!(commands[0]["delete"]["rule"]["handle"], 2);
        assert_eq!(commands[1]["delete"]["rule"]["handle"], 3);
        assert_eq!(
            commands[2]["insert"]["rule"]["expr"],
            expression("omash-tun")
        );
        let reloaded = json!({"nftables": [chain("ip", "input")]});
        assert_eq!(plan(&reloaded, TAG, Some("omash-tun")).unwrap().len(), 1);
    }

    #[test]
    fn cleanup_only_removes_our_exact_rules_in_input_chains() {
        let mut foreign = rule("vpn-tun", 3);
        foreign["rule"]["comment"] = "another application".into();
        let mut modified = rule("omash-tun", 4);
        modified["rule"]["expr"][2] = json!({"drop": null});
        let snapshot =
            json!({"nftables": [chain("ip", "input"), rule("omash-tun", 2), foreign, modified]});
        let commands = plan(&snapshot, TAG, None).unwrap();
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0]["delete"]["rule"]["handle"], 2);
    }

    #[test]
    fn userspace_stacks_need_no_exception() {
        for (stack, kernel) in [
            ("mixed", true),
            ("system", true),
            ("gVisor", false),
            ("mips", false),
        ] {
            let root = serde_yaml_ng::from_str(&format!("tun: {{stack: {stack}}}")).unwrap();
            assert_eq!(kernel_stack(&root), kernel);
        }
    }

    #[test]
    fn snapshot_omits_set_elements() {
        // Housekeeping lists the ruleset every two seconds; large blocklist sets
        // must not make that cost, or the nft timeout, grow with their size.
        assert!(SNAPSHOT.contains(&"--terse"), "{SNAPSHOT:?}");
    }

    #[test]
    fn legacy_filter_modules_are_reported_not_rejected() {
        assert_eq!(legacy_filter_hint(|_| false), None);
        let hint = legacy_filter_hint(|module| module == "ip6table_filter").unwrap();
        assert!(hint.contains("(ip6table_filter)"), "{hint}");
        let hint = legacy_filter_hint(|_| true).unwrap();
        assert!(hint.contains("(iptable_filter, ip6table_filter)"), "{hint}");
        assert!(
            hint.contains("omash manages only nftables exceptions"),
            "{hint}"
        );
    }

    /// Run with OMASH_FIREWALL_TEST=1 inside `unshare -Urn`. The Python fixture
    /// supplies real traffic and UFW; every rule change uses the production code.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    #[ignore = "requires an isolated Linux user/network namespace, Mihomo, nftables and UFW"]
    async fn kernel_traffic() {
        use tokio::io::{AsyncBufReadExt, BufReader};
        assert_eq!(std::env::var("OMASH_FIREWALL_TEST").as_deref(), Ok("1"));
        assert_ne!(
            fs::read_link("/proc/self/ns/net")
                .unwrap()
                .to_str()
                .unwrap(),
            std::env::var("OMASH_FIREWALL_PARENT_NETNS").unwrap()
        );
        // Root-owned host tools appear as uid 65534 in an unprivileged user
        // namespace. Bypass that installation check only in this test fixture.
        let firewall = Firewall {
            program: ["/usr/bin/nft", "/usr/sbin/nft"]
                .into_iter()
                .map(PathBuf::from)
                .find(|p| p.exists())
                .unwrap(),
            tag: "omash-tun:0:input".into(),
        };
        let mut child = Command::new("python3")
            .args([
                "-u",
                "-c",
                include_str!("../../scripts/check-tun-firewall.py"),
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut input = child.stdin.take().unwrap();
        let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
        while let Some(line) = tokio::time::timeout(Duration::from_secs(45), lines.next_line())
            .await
            .unwrap()
            .unwrap()
        {
            let request: Value = serde_json::from_str(&line).unwrap();
            let error = firewall
                .reconcile(request["device"].as_str())
                .await
                .err()
                .map(|e| format!("{e:#}"));
            input
                .write_all(format!("{}\n", json!({"error": error})).as_bytes())
                .await
                .unwrap();
            input.flush().await.unwrap();
        }
        assert!(
            child.wait().await.unwrap().success(),
            "firewall traffic fixture failed"
        );
    }
}
