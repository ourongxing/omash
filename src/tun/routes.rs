//! Only exact rules/routes observed when starting the owned core may be ignored.
//! Rules that an ended core left behind are remembered and removed once nothing else shares them.
use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use serde_yaml_ng::Mapping;
use std::{
    collections::BTreeSet,
    fs,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    time::Duration,
};

/// Policy rules per address family: IPv4, then IPv6.
pub(super) type Rules = [Vec<Value>; 2];

const FAMILIES: [&str; 2] = ["-4", "-6"];
/// Mihomo's default routing table and first rule priority (rules use up to +20).
const DEFAULT_TABLE: u64 = 2022;
const DEFAULT_PRIORITY: u64 = 9000;
/// sing-tun's default priority for the auto-redirect fallback rule (mark mode only).
const DEFAULT_FALLBACK: u64 = 32768;
/// sing-tun numbers its rules from the rule index up to a nop rule at +10, and deletes every rule
/// in that span when its core starts and stops.
const RULE_SPAN: u64 = 10;
/// The main table: Mihomo's DNS rule looks it up with a suppressor.
const MAIN_TABLE: u64 = 254;
/// Kernel default rules (local, main, default) are never deleted by priority.
const PROTECTED: [u64; 3] = [0, 32766, 32767];
/// File name below `<data>/tun-service` that keeps remembered rules across helper restarts.
pub(super) const LEDGER: &str = "owned-rules.json";

/// The `ip` command; tests point it at a script.
#[derive(Clone, Debug)]
struct Ip {
    program: PathBuf,
    lead: Vec<String>,
}

impl Ip {
    fn system() -> Self {
        Self {
            program: "/usr/bin/ip".into(),
            lead: Vec::new(),
        }
    }

    fn command(&self) -> tokio::process::Command {
        let mut command = tokio::process::Command::new(&self.program);
        command.args(&self.lead).kill_on_drop(true);
        command
    }
}

#[derive(Clone, Debug, Default)]
pub(super) struct Snapshot {
    rules: Rules,
    routes: [Vec<Value>; 2],
    /// Whether the kernel has IPv6, so that no IPv6 rules means none rather than unread.
    ipv6: bool,
}

impl Snapshot {
    pub async fn read() -> Result<Self> {
        Self::read_with(&Ip::system(), ipv6_supported()).await
    }

    async fn read_with(ip: &Ip, ipv6: bool) -> Result<Self> {
        let mut snapshot = Self {
            ipv6,
            ..Self::default()
        };
        for (index, family) in FAMILIES.iter().enumerate() {
            if index == 1 && !ipv6 {
                continue;
            }
            let mut rules = read_ip(ip, &["-N", family, "-j", "rule", "show"]).await?;
            rules.iter_mut().for_each(normalize);
            snapshot.rules[index] = rules;
            // Reading all tables succeeds even when the candidate table does not exist.
            snapshot.routes[index] =
                read_ip(ip, &["-N", family, "-j", "route", "show", "table", "all"]).await?;
        }
        Ok(snapshot)
    }

    pub fn check(&self, root: &Mapping, owned: &Self) -> Result<()> {
        let Some(scope) = routing_scope(root) else {
            return Ok(());
        };
        let table = scope.table;
        for index in 0..2 {
            if subtract(&self.rules[index], &owned.rules[index])
                .iter()
                .any(|rule| scope.conflicts(rule))
            {
                bail!(
                    "TUN policy routing conflicts with existing rules (table {table}); inspect `omash tun doctor`"
                );
            }
            if subtract(&self.routes[index], &owned.routes[index])
                .iter()
                // ip omits "table" for routes in the main table.
                .any(|route| table_index(route).unwrap_or(254) == table)
            {
                bail!("TUN routing table {table} is already in use");
            }
        }
        // Without the span, sing-tun puts its IPv6 rule just below the lowest positive priority
        // in use; with none, the kernel picks priority 0.
        if !scope.span && self.ipv6 {
            let next = subtract(&self.rules[1], &owned.rules[1])
                .iter()
                .filter_map(priority)
                .filter(|p| *p > 0)
                .min()
                .map(|p| p - 1);
            if !next.is_some_and(|p| p > 0 && !PROTECTED.contains(&p)) {
                bail!(
                    "the TUN IPv6 rule would take a reserved priority below the existing IPv6 rules; inspect `omash tun doctor`"
                );
            }
        }
        Ok(())
    }

    /// What a core added while it started: the rules shaped like Mihomo's own, which it owns,
    /// and other rules in its routing scope, which are foreign and must fail the start.
    pub fn added_by(&self, before: &Self, root: &Mapping, device: &str) -> (Self, Rules) {
        let mut added = Self::default();
        let mut foreign = Rules::default();
        let scope = routing_scope(root);
        for (index, found) in foreign.iter_mut().enumerate() {
            if let Some(scope) = scope {
                for rule in subtract(&self.rules[index], &before.rules[index]) {
                    if scope.claims(&rule, device) {
                        added.rules[index].push(rule);
                    } else if scope.conflicts(&rule) {
                        found.push(rule);
                    }
                }
            }
            added.routes[index] = subtract(&self.routes[index], &before.routes[index])
                .into_iter()
                .filter(|route| route["dev"].as_str() == Some(device))
                .collect();
        }
        (added, foreign)
    }

    pub fn rules(&self) -> &Rules {
        &self.rules
    }

    pub fn has_rules(&self) -> bool {
        self.rules.iter().any(|rules| !rules.is_empty())
    }

    /// The core that owned these entries ended. Its routes left with the TUN device; its rules
    /// become leftovers, limited to those still `present` when a fresh snapshot is known.
    pub fn retire_into(&mut self, residue: &mut Rules, present: Option<&Self>) {
        for (index, rules) in std::mem::take(&mut self.rules).into_iter().enumerate() {
            residue[index].extend(match present {
                Some(present) => retain_present(rules, &present.rules[index]),
                None => rules,
            });
        }
        self.routes = Default::default();
    }

    #[cfg(test)]
    pub fn with_rules(rules: Rules) -> Self {
        Self {
            rules,
            routes: Default::default(),
            ipv6: true,
        }
    }
}

pub(super) fn ipv6_supported() -> bool {
    // Kernels booted with ipv6.disable=1 have no IPv6 rules and `ip -6` fails there.
    Path::new("/proc/net/if_inet6").exists()
}

/// The routing table and rule priorities a Mihomo TUN configuration uses.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Scope {
    table: u64,
    priority: u64,
    /// The auto-redirect fallback rule's priority, in mark mode only.
    fallback: Option<u64>,
    /// Whether Mihomo routes automatically and so owns the rule span. Without it, Mihomo adds a
    /// single IPv6 rule sending the TUN device's output to its table, just below the lowest
    /// priority in use.
    span: bool,
}

impl Scope {
    const DEFAULT: Self = Self {
        table: DEFAULT_TABLE,
        priority: DEFAULT_PRIORITY,
        fallback: None,
        span: true,
    };

    fn conflicts(&self, rule: &Value) -> bool {
        table_index(rule) == Some(self.table)
            || self.span
                && priority(rule).is_some_and(|p| {
                    (self.priority..self.priority.saturating_add(20)).contains(&p)
                        || Some(p) == self.fallback
                })
    }

    fn in_span(&self, priority: u64) -> bool {
        (self.priority..=self.priority.saturating_add(RULE_SPAN)).contains(&priority)
    }

    /// A rule shaped like one Mihomo installs: within its span, looking up its table or main,
    /// jumping within the span, or with no lookup at all (nop); or the fallback rule; or, without
    /// the span, the device's IPv6 output rule.
    fn claims(&self, rule: &Value, device: &str) -> bool {
        let Some(p) = priority(rule) else {
            return false;
        };
        let table = table_index(rule);
        if !self.span {
            return table == Some(self.table) && rule["oif"].as_str() == Some(device);
        }
        if Some(p) == self.fallback && table == Some(self.table) {
            return true;
        }
        self.in_span(p)
            && match (table, rule.get("goto")) {
                (Some(table), _) => table == self.table || table == MAIN_TABLE,
                (None, Some(target)) => target.as_u64().is_some_and(|t| self.in_span(t)),
                (None, None) => true,
            }
    }
}

fn routing_scope(root: &Mapping) -> Option<Scope> {
    let tun = &root["tun"];
    let auto_redirect = tun["auto-redirect"].as_bool() == Some(true);
    let span = tun["auto-route"].as_bool() != Some(false) || auto_redirect;
    let inet6 = tun["inet6-address"]
        .as_sequence()
        .is_some_and(|v| !v.is_empty());
    if !span && !inet6 {
        return None;
    }
    // Mihomo replaces a missing or zero index with its default.
    let index = |key: &str, default: u64| tun[key].as_u64().filter(|v| *v != 0).unwrap_or(default);
    // Route address sets switch auto-redirect to mark mode, which adds the fallback rule.
    let address_sets = ["route-address-set", "route-exclude-address-set"]
        .iter()
        .any(|key| tun[*key].as_sequence().is_some_and(|v| !v.is_empty()));
    Some(Scope {
        table: index("iproute2-table-index", DEFAULT_TABLE),
        priority: index("iproute2-rule-index", DEFAULT_PRIORITY),
        span,
        fallback: (auto_redirect && address_sets).then(|| {
            index(
                "auto-redirect-iproute2-fallback-rule-index",
                DEFAULT_FALLBACK,
            )
        }),
    })
}

fn table_index(entry: &Value) -> Option<u64> {
    // iproute2 versions encode numeric table IDs as either JSON numbers or strings.
    entry["table"]
        .as_u64()
        .or_else(|| entry["table"].as_str()?.parse().ok())
}

fn priority(rule: &Value) -> Option<u64> {
    rule["priority"].as_u64()
}

/// Once a device is gone iproute2 marks the rules bound to it with an extra `*_detached` key.
/// A rule recorded while its core ran must still equal the leftover, so the marker is dropped.
fn normalize(rule: &mut Value) {
    if let Some(rule) = rule.as_object_mut() {
        rule.remove("iif_detached");
        rule.remove("oif_detached");
    }
}

// Treat duplicate entries separately: one owned rule cannot hide a second copy.
fn subtract(entries: &[Value], owned: &[Value]) -> Vec<Value> {
    let mut remaining = entries.to_vec();
    for entry in owned {
        if let Some(index) = remaining.iter().position(|value| value == entry) {
            remaining.remove(index);
        }
    }
    remaining
}

fn retain_present(rules: Vec<Value>, present: &[Value]) -> Vec<Value> {
    let mut present = present.to_vec();
    rules
        .into_iter()
        .filter(
            |rule| match present.iter().position(|value| value == rule) {
                Some(index) => {
                    present.remove(index);
                    true
                }
                None => false,
            },
        )
        .collect()
}

#[derive(Debug, PartialEq)]
enum Plan {
    /// Something else shares the remembered priorities: delete nothing, keep remembering.
    Keep,
    /// No remembered rule exists any more.
    Drop,
    /// Every rule at the remembered priorities is a remembered one: these may be deleted.
    Delete(Vec<Value>),
}

/// `current` is one family's snapshot, `owned` the running core's rules, `residue` the rules of
/// cores that have ended. Every rule sharing a priority with a remembered one must itself be
/// remembered and none may belong to the running core. Each is then deleted by every field the
/// kernel compares, so a rule that cannot be described that way is never deleted.
fn plan_family(index: usize, current: &[Value], owned: &[Value], residue: &[Value]) -> Plan {
    // A remembered rule at a protected priority is never deleted, nor forgotten while present.
    let others = subtract(current, owned);
    if residue
        .iter()
        .any(|rule| priority(rule).is_some_and(|p| PROTECTED.contains(&p)) && others.contains(rule))
    {
        return Plan::Keep;
    }
    let live: BTreeSet<u64> = owned.iter().filter_map(priority).collect();
    let remembered: BTreeSet<u64> = residue
        .iter()
        .filter_map(priority)
        .filter(|p| !PROTECTED.contains(p))
        .collect();
    let candidates: Vec<Value> = subtract(current, owned)
        .into_iter()
        .filter(|rule| priority(rule).is_some_and(|p| remembered.contains(&p)))
        .collect();
    if candidates.is_empty() {
        Plan::Drop
    } else if subtract(&candidates, residue).is_empty()
        && candidates
            .iter()
            .all(|rule| priority(rule).is_some_and(|p| !live.contains(&p)))
        && candidates
            .iter()
            .all(|rule| deletion_args(index, rule).is_some())
    {
        Plan::Delete(candidates)
    } else {
        Plan::Keep
    }
}

/// Removes the remembered rules of ended cores that are provably nobody else's and returns the
/// snapshot the conflict check should use. Rules that cannot be claimed stay in `residue` and
/// are reported by the normal check.
pub async fn heal(snapshot: Snapshot, owned: &Snapshot, residue: &mut Rules) -> Result<Snapshot> {
    heal_with(&Ip::system(), ipv6_supported(), snapshot, owned, residue).await
}

async fn heal_with(
    ip: &Ip,
    ipv6: bool,
    mut snapshot: Snapshot,
    owned: &Snapshot,
    residue: &mut Rules,
) -> Result<Snapshot> {
    let plans = [0, 1].map(|index| {
        if residue[index].is_empty() {
            Plan::Drop
        } else {
            plan_family(
                index,
                &snapshot.rules[index],
                &owned.rules[index],
                &residue[index],
            )
        }
    });
    if plans.iter().any(|plan| matches!(plan, Plan::Delete(_))) {
        for (index, plan) in plans.iter().enumerate() {
            if let Plan::Delete(rules) = plan {
                // One command per rule: the kernel removes the first rule that matches.
                for rule in rules {
                    delete_rule(ip, index, rule).await?;
                }
            }
        }
        let after = Snapshot::read_with(ip, ipv6).await?;
        for (index, plan) in plans.iter().enumerate() {
            if let Plan::Delete(rules) = plan {
                let gone = removed(
                    &snapshot.rules[index],
                    &after.rules[index],
                    &owned.rules[index],
                );
                if !same_rules(&gone, rules) {
                    bail!(
                        "leftover TUN policy rules were not removed as expected ({}: {} rules were planned, {} disappeared); inspect `omash tun doctor`",
                        FAMILIES[index],
                        rules.len(),
                        gone.len()
                    );
                }
            }
        }
        snapshot = after;
    }
    for (index, plan) in plans.iter().enumerate() {
        if *plan != Plan::Keep {
            residue[index].clear();
        }
    }
    Ok(snapshot)
}

async fn delete_rule(ip: &Ip, index: usize, rule: &Value) -> Result<()> {
    let rule_args = deletion_args(index, rule).context("leftover TUN rule cannot be matched")?;
    let mut args = vec![FAMILIES[index], "rule", "del"];
    args.extend(rule_args.iter().map(String::as_str));
    run_ip(ip, &args).await.map_err(|error| {
        anyhow!("cannot remove leftover TUN policy rule ({error:#}); inspect `omash tun doctor`")
    })?;
    Ok(())
}

/// `ip rule del` arguments naming every field of `rule` that the kernel compares when it looks
/// for the rule to delete. The kernel ignores a goto target and `not`, and treats a field left
/// out as a wildcard, so this narrows but cannot rule out deleting a rule that replaced this one
/// since the snapshot. `None` for anything `ip -j rule show` printed that is not understood.
fn deletion_args(index: usize, rule: &Value) -> Option<Vec<String>> {
    let rule = rule.as_object()?;
    let mut used = BTreeSet::new();
    let mut get = |key: &'static str| {
        let value = rule.get(key)?;
        used.insert(key);
        Some(value)
    };
    let mut args = vec![
        "priority".to_owned(),
        get("priority")?.as_u64()?.to_string(),
    ];
    // Keywords such as `not` carry no value: iproute2 prints them as null.
    macro_rules! word {
        ($key:literal) => {
            if let Some(value) = get($key) {
                if !value.is_null() {
                    return None;
                }
                args.push($key.to_owned());
            }
        };
    }
    word!("not");
    let host_len = if index == 0 { 32 } else { 128 };
    for (key, len, word) in [("src", "srclen", "from"), ("dst", "dstlen", "to")] {
        let len = match get(len) {
            Some(len) => Some(len.as_u64()?),
            None => None,
        };
        match get(key).map(Value::as_str) {
            None if len.is_none() => {}
            Some(Some("all")) if len.is_none() => {}
            Some(Some(address)) if address != "all" => args.extend([
                word.to_owned(),
                format!("{address}/{}", len.unwrap_or(host_len)),
            ]),
            _ => return None,
        }
    }
    for key in ["tos", "iif", "oif", "ipproto"] {
        if let Some(value) = get(key) {
            args.extend([key.to_owned(), scalar(value)?]);
        }
    }
    if let Some(mark) = get("fwmark") {
        let mut mark = scalar(mark)?;
        if let Some(mask) = get("fwmask") {
            mark = format!("{mark}/{}", scalar(mask)?);
        }
        args.extend(["fwmark".to_owned(), mark]);
    }
    word!("l3mdev");
    match (get("uid_start"), get("uid_end")) {
        (None, None) => {}
        (Some(start), Some(end)) => args.extend([
            "uidrange".to_owned(),
            format!("{}-{}", start.as_u64()?, end.as_u64()?),
        ]),
        _ => return None,
    }
    // A port is one number with a full mask, or a range.
    for (key, start, end, mask) in [
        ("sport", "sport_start", "sport_end", "sport_mask"),
        ("dport", "dport_start", "dport_end", "dport_mask"),
    ] {
        if get(mask).is_some_and(|mask| mask.as_str() != Some("0xffff")) {
            return None;
        }
        match (get(key), get(start), get(end)) {
            (None, None, None) => {}
            (Some(port), None, None) => args.extend([key.to_owned(), port.as_u64()?.to_string()]),
            (None, Some(start), Some(end)) => args.extend([
                key.to_owned(),
                format!("{}-{}", start.as_u64()?, end.as_u64()?),
            ]),
            _ => return None,
        }
    }
    // Exactly one action. `table` alone leaves the action unset, and the kernel compares an
    // action only when one is given, so a lookup names its type.
    match (get("table"), get("goto"), get("nop"), get("action")) {
        (Some(table), None, None, None) => {
            args.extend(["type", "unicast", "table"].map(String::from));
            args.push(scalar(table)?);
        }
        (None, Some(target), None, None) => {
            args.extend(["goto".to_owned(), target.as_u64()?.to_string()])
        }
        (None, None, Some(Value::Null), None) => args.push("nop".into()),
        (None, None, None, Some(Value::String(action)))
            if ["blackhole", "unreachable", "prohibit"].contains(&action.as_str()) =>
        {
            args.push(action.clone())
        }
        _ => return None,
    }
    if let Some(length) = get("suppress_prefixlen") {
        args.extend([
            "suppress_prefixlength".to_owned(),
            length.as_u64()?.to_string(),
        ]);
    }
    if let Some(group) = get("suppress_ifgroup") {
        args.extend(["suppress_ifgroup".to_owned(), scalar(group)?]);
    }
    rule.keys()
        .all(|key| used.contains(key.as_str()))
        .then_some(args)
}

fn scalar(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

/// Rules that disappeared, ignoring the running core's and anything added meanwhile.
fn removed(before: &[Value], after: &[Value], owned: &[Value]) -> Vec<Value> {
    subtract(&subtract(before, owned), &subtract(after, owned))
}

fn same_rules(a: &[Value], b: &[Value]) -> bool {
    a.len() == b.len() && subtract(a, b).is_empty()
}

async fn read_ip(ip: &Ip, args: &[&str]) -> Result<Vec<Value>> {
    let stdout = run_ip(ip, args)
        .await
        .context("cannot inspect existing policy routes")?;
    serde_json::from_slice(&stdout).context("invalid policy routing snapshot")
}

async fn run_ip(ip: &Ip, args: &[&str]) -> Result<Vec<u8>> {
    let output = tokio::time::timeout(Duration::from_secs(3), ip.command().args(args).output())
        .await
        .with_context(|| format!("`ip {}` timed out", args.join(" ")))?
        .with_context(|| format!("cannot run `ip {}`", args.join(" ")))?;
    if !output.status.success() {
        bail!(
            "`ip {}` failed ({}): {}",
            args.join(" "),
            output.status,
            brief(&output.stderr)
        );
    }
    Ok(output.stdout)
}

/// Stderr as one bounded line.
fn brief(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    match text.chars().count() {
        0 => "no error output".into(),
        1..=200 => text,
        _ => text.chars().take(199).chain(['…']).collect(),
    }
}

/// Boot and network namespace that remembered rules belong to.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Identity {
    boot_id: String,
    netns: u64,
}

impl Identity {
    pub fn current() -> Result<Self> {
        Ok(Self {
            boot_id: fs::read_to_string("/proc/sys/kernel/random/boot_id")
                .context("cannot read the boot id")?
                .trim()
                .to_owned(),
            // stat follows the namespace link; its inode identifies the namespace.
            netns: fs::metadata("/proc/self/ns/net")
                .context("cannot identify the network namespace")?
                .ino(),
        })
    }
}

#[derive(Serialize, Deserialize)]
struct Remembered {
    boot_id: String,
    netns: u64,
    owned: Rules,
    residue: Rules,
}

/// Records the rules of the running and of ended cores. Nothing to remember removes the file.
pub fn remember(path: &Path, identity: &Identity, owned: &Rules, residue: &Rules) -> Result<()> {
    if owned.iter().chain(residue).all(Vec::is_empty) {
        return match fs::remove_file(path) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error.into()),
            _ => Ok(()),
        };
    }
    let record = Remembered {
        boot_id: identity.boot_id.clone(),
        netns: identity.netns,
        owned: owned.clone(),
        residue: residue.clone(),
    };
    super::process::private_write(path, &serde_json::to_vec(&record)?)
}

/// Rules an earlier helper left in this boot and network namespace. The earlier core cannot
/// outlive its helper, so all of them are leftovers. A record from elsewhere is deleted.
pub fn recall(path: &Path, identity: Option<&Identity>) -> Rules {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) => {
            if error.kind() != std::io::ErrorKind::NotFound {
                eprintln!("omash-tun: cannot read {}: {error}", path.display());
            }
            return Rules::default();
        }
    };
    match (serde_json::from_slice::<Remembered>(&bytes), identity) {
        (Ok(record), Some(identity))
            if record.boot_id == identity.boot_id && record.netns == identity.netns =>
        {
            let mut rules = record.owned;
            for (index, residue) in record.residue.into_iter().enumerate() {
                rules[index].extend(residue);
            }
            rules.iter_mut().flatten().for_each(normalize);
            // The file is writable by the user while the helper deletes with CAP_NET_ADMIN, so
            // only rules within Mihomo's default scope can be claimed back after a restart.
            let total: usize = rules.iter().map(Vec::len).sum();
            rules
                .iter_mut()
                .for_each(|rules| rules.retain(|rule| Scope::DEFAULT.conflicts(rule)));
            let kept: usize = rules.iter().map(Vec::len).sum();
            if kept != total {
                eprintln!(
                    "omash-tun: ignoring {} remembered rules outside the default TUN routing scope",
                    total - kept
                );
            }
            rules
        }
        _ => {
            let _ = fs::remove_file(path);
            Rules::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn config(table: u64, priority: u64) -> Mapping {
        serde_yaml_ng::from_str(&format!(
            "tun: {{auto-route: true, iproute2-table-index: {table}, iproute2-rule-index: {priority}}}"
        ))
        .unwrap()
    }

    fn active() -> (Snapshot, Snapshot) {
        let before = Snapshot {
            rules: [vec![json!({"priority": 20000, "table": 199})], vec![]],
            routes: [vec![json!({"table": 199, "type": "blackhole"})], vec![]],
            ipv6: true,
        };
        let mut current = before.clone();
        current.rules[0].push(json!({"priority": 9000, "table": 2022}));
        current.routes[0].push(json!({"table": 2022, "dev": "omash-tun"}));
        let (owned, _) = current.added_by(&before, &config(2022, 9000), "omash-tun");
        (current, owned)
    }

    #[test]
    fn apply_rejects_foreign_table_and_priority_while_core_is_running() {
        let (current, owned) = active();
        assert!(current.check(&config(199, 9000), &owned).is_err());
        assert!(current.check(&config(2022, 20000), &owned).is_err());
        assert!(current.check(&config(2022, 9000), &owned).is_ok());
    }

    #[test]
    fn apply_does_not_claim_foreign_entries_added_to_its_current_scope() {
        let (mut current, owned) = active();
        current.rules[1].push(json!({"priority": 9001, "table": 199}));
        assert!(current.check(&config(2022, 9000), &owned).is_err());
        current.rules[1].clear();
        current.routes[0].push(json!({"table": 2022, "dev": "vpn-test"}));
        assert!(current.check(&config(2022, 9000), &owned).is_err());
    }

    #[test]
    fn one_owned_rule_does_not_hide_an_extra_identical_rule() {
        let (mut current, owned) = active();
        current.rules[0].push(owned.rules[0][0].clone());
        assert!(current.check(&config(2022, 9000), &owned).is_err());
    }

    #[test]
    fn numeric_table_strings_and_implicit_main_table_are_checked() {
        let mut current = Snapshot::default();
        current.rules[0].push(json!({"priority": 20000, "table": "2022"}));
        assert!(
            current
                .check(&config(2022, 9000), &Snapshot::default())
                .is_err()
        );
        current.rules[0].clear();
        current.routes[1].push(json!({"table": "2022", "dev": "vpn-test"}));
        assert!(
            current
                .check(&config(2022, 9000), &Snapshot::default())
                .is_err()
        );
        current.routes[1] = vec![json!({"dev": "eth0", "dst": "default"})];
        assert!(
            current
                .check(&config(254, 9000), &Snapshot::default())
                .is_err()
        );
    }

    // `ip -N -j rule show` of Mihomo's rules (mihomo 1.19, TUN with IPv6), as captured on a
    // running core and again after `kill -9` removed the TUN device.
    const RUNNING4: &str = r#"[{"priority":0,"src":"all","table":"255"},{"priority":9000,"src":"all","dst":"198.18.0.0","dstlen":30,"table":"2022"},{"priority":9001,"not":null,"src":"all","dport":53,"dport_mask":"0xffff","table":"254","suppress_prefixlen":0},{"priority":9001,"src":"all","iif":"omash-tun","goto":9010},{"priority":9002,"not":null,"src":"all","iif":"lo","table":"2022"},{"priority":9002,"src":"0.0.0.0","iif":"lo","table":"2022"},{"priority":9002,"src":"198.18.0.0","srclen":30,"iif":"lo","table":"2022"},{"priority":9010,"src":"all","nop":null},{"priority":32766,"src":"all","table":"254"},{"priority":32767,"src":"all","table":"253"}]"#;
    const LEFTOVER4: &str = r#"[{"priority":0,"src":"all","table":"255"},{"priority":9000,"src":"all","dst":"198.18.0.0","dstlen":30,"table":"2022"},{"priority":9001,"not":null,"src":"all","dport":53,"dport_mask":"0xffff","table":"254","suppress_prefixlen":0},{"priority":9001,"src":"all","iif":"omash-tun","iif_detached":null,"goto":9010},{"priority":9002,"not":null,"src":"all","iif":"lo","table":"2022"},{"priority":9002,"src":"0.0.0.0","iif":"lo","table":"2022"},{"priority":9002,"src":"198.18.0.0","srclen":30,"iif":"lo","table":"2022"},{"priority":9010,"src":"all","nop":null},{"priority":32766,"src":"all","table":"254"},{"priority":32767,"src":"all","table":"253"}]"#;
    const RUNNING6: &str = r#"[{"priority":0,"src":"all","table":"255"},{"priority":9000,"not":null,"src":"all","dport":53,"dport_mask":"0xffff","table":"254","suppress_prefixlen":0},{"priority":9000,"src":"all","iif":"omash-tun","goto":9010},{"priority":9000,"src":"::","srclen":1,"iif":"lo","goto":9010},{"priority":9000,"src":"8000::","srclen":1,"iif":"lo","goto":9010},{"priority":9001,"src":"fdfe:dcba:9876::","srclen":126,"iif":"lo","table":"2022"},{"priority":9002,"src":"all","table":"2022"},{"priority":9010,"src":"all","nop":null},{"priority":32766,"src":"all","table":"254"}]"#;
    const LEFTOVER6: &str = r#"[{"priority":0,"src":"all","table":"255"},{"priority":9000,"not":null,"src":"all","dport":53,"dport_mask":"0xffff","table":"254","suppress_prefixlen":0},{"priority":9000,"src":"all","iif":"omash-tun","iif_detached":null,"goto":9010},{"priority":9000,"src":"::","srclen":1,"iif":"lo","goto":9010},{"priority":9000,"src":"8000::","srclen":1,"iif":"lo","goto":9010},{"priority":9001,"src":"fdfe:dcba:9876::","srclen":126,"iif":"lo","table":"2022"},{"priority":9002,"src":"all","table":"2022"},{"priority":9010,"src":"all","nop":null},{"priority":32766,"src":"all","table":"254"}]"#;

    fn parse(json: &str) -> Vec<Value> {
        let mut rules: Vec<Value> = serde_json::from_str(json).unwrap();
        rules.iter_mut().for_each(normalize);
        rules
    }

    /// The system's own rules, what was there before the core started.
    fn system(rules: &[Value]) -> Vec<Value> {
        rules
            .iter()
            .filter(|rule| priority(rule).is_some_and(|p| PROTECTED.contains(&p)))
            .cloned()
            .collect()
    }

    /// What the helper records for the E4 core, computed like a start does.
    fn recorded() -> Rules {
        let running = Snapshot::with_rules([parse(RUNNING4), parse(RUNNING6)]);
        let before = Snapshot::with_rules([system(&parse(RUNNING4)), system(&parse(RUNNING6))]);
        running
            .added_by(&before, &config(2022, 9000), "omash-tun")
            .0
            .rules
    }

    fn leftovers() -> Rules {
        [parse(LEFTOVER4), parse(LEFTOVER6)]
    }

    #[test]
    fn detached_marker_does_not_distinguish_a_leftover_from_the_recorded_rule() {
        let raw: Vec<Value> = serde_json::from_str(LEFTOVER4).unwrap();
        assert!(raw.iter().any(|rule| rule.get("iif_detached").is_some()));
        assert_eq!(parse(LEFTOVER4), parse(RUNNING4));
        assert_eq!(parse(LEFTOVER6), parse(RUNNING6));
        let mut oif = json!({"priority": 9001, "oif": "gone", "oif_detached": null, "table": "1"});
        normalize(&mut oif);
        assert_eq!(oif, json!({"priority": 9001, "oif": "gone", "table": "1"}));
    }

    #[test]
    fn crash_leftovers_are_planned_for_deletion_in_both_families() {
        let residue = recorded();
        assert_eq!([residue[0].len(), residue[1].len()], [7, 7]);
        // Several rules share a priority: IPv6 has four at 9000.
        assert_eq!(
            residue[1]
                .iter()
                .filter(|rule| priority(rule) == Some(9000))
                .count(),
            4
        );
        let current = leftovers();
        let none = Rules::default();
        for index in 0..2 {
            // One deletion per rule, the system's own rules excluded.
            assert_eq!(
                plan_family(index, &current[index], &none[index], &residue[index]),
                Plan::Delete(residue[index].clone())
            );
        }
    }

    #[test]
    fn a_foreign_rule_sharing_a_remembered_priority_blocks_deletion() {
        let residue = recorded();
        let none = Rules::default();
        for foreign in [
            json!({"priority": 9002, "src": "10.0.0.0", "srclen": 8, "table": "100"}),
            // Same priority and selector but a second copy: one remembered rule cannot hide it.
            residue[0][6].clone(),
        ] {
            let mut current = leftovers();
            current[0].push(foreign);
            assert_eq!(
                plan_family(0, &current[0], &none[0], &residue[0]),
                Plan::Keep
            );
            // The other family is unaffected.
            assert!(matches!(
                plan_family(1, &current[1], &none[1], &residue[1]),
                Plan::Delete(_)
            ));
        }
    }

    #[test]
    fn stale_residue_without_matching_rules_is_dropped_without_deleting() {
        let residue = recorded();
        let none = Rules::default();
        let system4 = system(&parse(RUNNING4));
        assert_eq!(plan_family(0, &system4, &none[0], &residue[0]), Plan::Drop);
        assert_eq!(plan_family(0, &[], &none[0], &residue[0]), Plan::Drop);
        // Rules at other priorities are not at stake, however foreign.
        let other = vec![json!({"priority": 100, "src": "all", "table": "7"})];
        assert_eq!(plan_family(0, &other, &none[0], &residue[0]), Plan::Drop);
    }

    #[test]
    fn the_running_cores_rules_are_never_claimed_by_stale_residue() {
        // A live core whose rules are textually identical to the remembered ones.
        let residue = recorded();
        let live = residue.clone();
        let mut current = [parse(RUNNING4), parse(RUNNING6)];
        for index in 0..2 {
            assert_eq!(
                plan_family(index, &current[index], &live[index], &residue[index]),
                Plan::Drop
            );
        }
        // Residue and the live core share a priority but not the rule: deleting by priority
        // could hit the live rule, so nothing is deleted.
        let mut stale = residue[0].clone();
        stale[0] = json!({"priority": 9000, "src": "all", "dst": "198.18.9.0", "dstlen": 30, "table": "2022"});
        current[0].push(stale[0].clone());
        assert_eq!(plan_family(0, &current[0], &live[0], &stale), Plan::Keep);
    }

    #[test]
    fn kernel_default_rules_are_never_deleted() {
        let main = json!({"priority": 32766, "src": "all", "table": "254"});
        let residue = vec![main.clone()];
        let current = vec![main];
        assert_eq!(plan_family(0, &current, &[], &residue), Plan::Keep);
        assert_eq!(plan_family(0, &[], &[], &residue), Plan::Drop);
    }

    #[test]
    fn graceful_stop_remembers_only_rules_that_are_still_present() {
        let mut owned = recorded();
        owned[1].push(owned[1][0].clone());
        let mut routes = Snapshot::with_rules(owned.clone());
        routes.routes[0].push(json!({"table": 2022, "dev": "omash-tun"}));
        let mut residue = Rules::default();
        // One IPv4 rule remains, and only one of the two identical IPv6 rules.
        let present = Snapshot::with_rules([vec![owned[0][2].clone()], vec![owned[1][0].clone()]]);
        routes.retire_into(&mut residue, Some(&present));
        assert!(!routes.has_rules() && routes.routes[0].is_empty());
        assert_eq!(
            residue,
            [vec![owned[0][2].clone()], vec![owned[1][0].clone()]]
        );

        // Without a readable snapshot everything is kept.
        let mut routes = Snapshot::with_rules(owned.clone());
        let mut residue = Rules::default();
        routes.retire_into(&mut residue, None);
        assert_eq!(residue, owned);
    }

    #[test]
    fn startup_does_not_claim_a_foreign_rule_added_meanwhile() {
        let before = Snapshot::with_rules([system(&parse(RUNNING4)), vec![]]);
        let mut now = Snapshot::with_rules([parse(RUNNING4), vec![]]);
        let foreign = vec![
            json!({"priority": 9005, "src": "all", "table": "100"}),
            json!({"priority": 20000, "src": "all", "table": "2022"}),
        ];
        now.rules[0].extend(foreign.clone());
        let (owned, found) = now.added_by(&before, &config(2022, 9000), "omash-tun");
        assert_eq!(owned.rules[0], recorded()[0]);
        assert_eq!(found, [foreign, vec![]]);
        // Mihomo's own rules alone are all claimed.
        let mut now = Snapshot::with_rules([parse(RUNNING4), parse(RUNNING6)]);
        now.rules[0].push(json!({"priority": 100, "src": "all", "table": "7"}));
        let (owned, found) = now.added_by(&before, &config(2022, 9000), "omash-tun");
        assert_eq!(owned.rules, recorded());
        assert_eq!(found, Rules::default());
    }

    fn mark_mode(extra: &str) -> Mapping {
        serde_yaml_ng::from_str(&format!(
            "tun: {{auto-route: true, auto-redirect: true, route-address-set: [lan]{extra}}}"
        ))
        .unwrap()
    }

    #[test]
    fn auto_redirect_fallback_rule_is_claimed_only_in_mark_mode() {
        let at = |priority: u64, table: &str| json!({"priority": priority, "src": "all", "table": table});
        let before = Snapshot::with_rules([system(&parse(RUNNING4)), vec![]]);
        let added = |fallback: u64, root: &Mapping| {
            let mut now = Snapshot::with_rules([parse(RUNNING4), vec![]]);
            now.rules[0].extend([at(fallback, "2022"), at(fallback, "100")]);
            let (owned, foreign) = now.added_by(&before, root, "omash-tun");
            (owned.rules[0].clone(), foreign[0].clone())
        };
        let occupied = |priority: u64, root: &Mapping| {
            Snapshot::with_rules([vec![at(priority, "100")], vec![]])
                .check(root, &Snapshot::default())
                .is_err()
        };
        let mut mine = recorded()[0].clone();
        mine.push(at(32768, "2022"));

        let default = mark_mode("");
        assert_eq!(
            added(32768, &default),
            (mine.clone(), vec![at(32768, "100")])
        );
        assert!(occupied(32768, &default));

        // A configured fallback priority replaces the default one.
        let custom = mark_mode(", auto-redirect-iproute2-fallback-rule-index: 40000");
        let mut mine40 = recorded()[0].clone();
        mine40.push(at(40000, "2022"));
        assert_eq!(added(40000, &custom), (mine40, vec![at(40000, "100")]));
        assert!(occupied(40000, &custom));
        assert!(!occupied(32768, &custom));

        // Zero indices mean Mihomo's defaults, exactly as if they were missing.
        let zero = mark_mode(
            ", iproute2-table-index: 0, iproute2-rule-index: 0, auto-redirect-iproute2-fallback-rule-index: 0",
        );
        assert_eq!(routing_scope(&zero), routing_scope(&default));
        assert_eq!(added(32768, &zero), (mine, vec![at(32768, "100")]));
        assert!(occupied(32768, &zero));

        // Without mark mode there is no fallback rule: one at that priority using the TUN table
        // is foreign, and an unrelated one there is nobody's concern.
        let plain = config(2022, 9000);
        assert_eq!(
            added(32768, &plain),
            (recorded()[0].clone(), vec![at(32768, "2022")])
        );
        assert!(!occupied(32768, &plain));
        let no_sets: Mapping =
            serde_yaml_ng::from_str("tun: {auto-route: true, auto-redirect: true}").unwrap();
        assert_eq!(routing_scope(&no_sets).unwrap().fallback, None);
    }

    #[test]
    fn ipv6_rule_without_auto_route_is_checked_and_claimed() {
        let root: Mapping = serde_yaml_ng::from_str(
            "tun: {auto-route: false, device: omash-tun, inet6-address: [fdfe:dcba:9876::1/126]}",
        )
        .unwrap();
        // sing-tun adds one IPv6 rule just below the lowest priority in use.
        let theirs = json!({"priority": 32765, "src": "all", "oif": "omash-tun", "table": "2022"});
        let foreign = json!({"priority": 100, "src": "all", "table": "2022"});
        let before = Snapshot::with_rules([vec![], system(&parse(RUNNING6))]);
        let mut now = before.clone();
        now.rules[1].extend([theirs.clone(), foreign.clone()]);
        let (owned, found) = now.added_by(&before, &root, "omash-tun");
        assert_eq!(owned.rules, [vec![], vec![theirs.clone()]]);
        assert_eq!(found, [vec![], vec![foreign.clone()]]);
        // Its table is checked before a start; there is no fixed rule range.
        assert!(
            Snapshot::with_rules([vec![], vec![foreign]])
                .check(&root, &Snapshot::default())
                .is_err()
        );
        let elsewhere = json!({"priority": 9000, "src": "all", "table": "100"});
        // A crashed core leaves it behind: it is remembered, recalled within the default table,
        // and deleted by its full description before the retry.
        assert!(Scope::DEFAULT.conflicts(&theirs));
        let mut leftover = theirs.clone();
        leftover["oif_detached"] = Value::Null;
        normalize(&mut leftover);
        assert_eq!(
            plan_family(1, &[leftover], &[], std::slice::from_ref(&theirs)),
            Plan::Delete(vec![theirs.clone()])
        );
        assert_eq!(
            deletion_args(1, &theirs).unwrap().join(" "),
            "priority 32765 oif omash-tun type unicast table 2022"
        );
        // sing-tun picks the lowest positive IPv6 priority minus one: a start that would put the
        // rule at a protected priority is refused, a running core's own rule does not count.
        let v6 = |priorities: &[u64]| {
            Snapshot::with_rules([
                vec![],
                priorities
                    .iter()
                    .map(|p| json!({"priority": p, "src": "all", "table": "7"}))
                    .collect(),
            ])
        };
        assert!(
            v6(&[0, 1, 32766])
                .check(&root, &Snapshot::default())
                .is_err()
        );
        assert!(v6(&[0, 32767]).check(&root, &Snapshot::default()).is_err());
        assert!(v6(&[0]).check(&root, &Snapshot::default()).is_err());
        // IPv6 without any rule (even main flushed) makes sing-tun's rule land at priority 0;
        // only a kernel without IPv6 has no IPv6 rules to look at.
        assert!(v6(&[]).check(&root, &Snapshot::default()).is_err());
        assert!(
            Snapshot::default()
                .check(&root, &Snapshot::default())
                .is_ok()
        );
        assert!(v6(&[0, 32766]).check(&root, &Snapshot::default()).is_ok());
        let mut running = v6(&[0, 32766]);
        running.rules[1].push(theirs.clone());
        let mine = Snapshot::with_rules([vec![], vec![theirs.clone()]]);
        assert!(running.check(&root, &mine).is_ok());
        // A leftover at a protected priority is never deleted, but stays remembered.
        let zero = json!({"priority": 0, "src": "all", "oif": "omash-tun", "table": "2022"});
        assert_eq!(
            plan_family(
                1,
                std::slice::from_ref(&zero),
                &[],
                std::slice::from_ref(&zero)
            ),
            Plan::Keep
        );
        // Without an IPv6 address sing-tun adds no rule at all.
        let v4_only: Mapping = serde_yaml_ng::from_str("tun: {auto-route: false}").unwrap();
        assert_eq!(routing_scope(&v4_only), None);
        assert!(
            Snapshot::with_rules([vec![elsewhere], v6(&[0, 32766]).rules[1].clone()])
                .check(&root, &Snapshot::default())
                .is_ok()
        );
    }

    #[test]
    fn deletion_must_remove_exactly_the_planned_rules() {
        let exactly = |before: &[Value], after: &[Value], owned: &[Value], planned: &[Value]| {
            same_rules(&removed(before, after, owned), planned)
        };
        let before = leftovers();
        let planned = recorded();
        let after = system(&before[0]);
        assert!(exactly(&before[0], &after, &[], &planned[0]));
        // Something extra disappeared, or something planned remained.
        let mut missing = after.clone();
        missing.pop();
        assert!(!exactly(&before[0], &missing, &[], &planned[0]));
        let mut remaining = after.clone();
        remaining.push(planned[0][0].clone());
        assert!(!exactly(&before[0], &remaining, &[], &planned[0]));
        // Rules appearing meanwhile are not an error, and neither is the running core's rule
        // staying (or leaving) next to them.
        let live = json!({"priority": 9500, "src": "all", "table": "2022"});
        let mut added = after.clone();
        added.push(json!({"priority": 5, "src": "all", "table": "9"}));
        let mut with_live = before[0].clone();
        with_live.push(live.clone());
        assert!(!exactly(&with_live, &added, &[], &planned[0]));
        let owned = std::slice::from_ref(&live);
        assert!(exactly(&with_live, &added, owned, &planned[0]));
        added.push(live.clone());
        assert!(exactly(&with_live, &added, owned, &planned[0]));
    }

    #[test]
    fn stderr_is_one_bounded_line() {
        assert_eq!(
            brief(b"  RTNETLINK answers:\n  boom \n"),
            "RTNETLINK answers: boom"
        );
        assert_eq!(brief(b""), "no error output");
        let long = "x".repeat(500);
        assert_eq!(brief(long.as_bytes()).chars().count(), 200);
        assert!(brief("é".repeat(300).as_bytes()).ends_with('…'));
    }

    #[test]
    fn remembered_rules_survive_a_helper_restart_in_the_same_boot_and_namespace() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tun-service").join(LEDGER);
        let id = Identity {
            boot_id: "boot-a".into(),
            netns: 4026531833,
        };
        let owned = recorded();
        let mut residue = Rules::default();
        residue[1]
            .push(json!({"priority": 9005, "src": "all", "table": "2022", "iif_detached": null}));
        remember(&path, &id, &owned, &residue).unwrap();
        let recalled = recall(&path, Some(&id));
        assert_eq!(recalled[0], owned[0]);
        assert_eq!(recalled[1].len(), owned[1].len() + 1);
        // Normalized on load, whatever an older file contained.
        assert_eq!(
            recalled[1].last().unwrap(),
            &json!({"priority": 9005, "src": "all", "table": "2022"})
        );
        assert!(path.exists());
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn a_recalled_record_cannot_claim_rules_outside_the_default_scope() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LEDGER);
        let id = Identity {
            boot_id: "b".into(),
            netns: 1,
        };
        let mut owned = recorded();
        let inside = owned[0].len();
        // Anything else in the user-writable file, system rules included, is not ours to delete.
        owned[0].push(json!({"priority": 100, "src": "10.0.0.0", "srclen": 8, "table": "7"}));
        owned[0].push(json!({"priority": 32766, "src": "all", "table": "254"}));
        owned[1].push(json!({"priority": 9020, "src": "all", "table": "8"}));
        remember(&path, &id, &owned, &Rules::default()).unwrap();
        let recalled = recall(&path, Some(&id));
        assert_eq!(recalled, recorded());
        assert_eq!(recalled[0].len(), inside);
        // Table 2022 or the first 20 priorities from 9000 are in scope, whatever the other half says.
        owned = Rules::default();
        owned[0].push(json!({"priority": 20, "src": "all", "table": "2022"}));
        owned[1].push(json!({"priority": 9019, "src": "all", "goto": 9010}));
        remember(&path, &id, &owned, &Rules::default()).unwrap();
        assert_eq!(recall(&path, Some(&id)), owned);
    }

    #[test]
    fn a_record_from_another_boot_or_namespace_or_garbage_is_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LEDGER);
        let id = Identity {
            boot_id: "boot-a".into(),
            netns: 7,
        };
        let owned = recorded();
        for other in [
            Identity {
                boot_id: "boot-b".into(),
                netns: 7,
            },
            Identity {
                boot_id: "boot-a".into(),
                netns: 8,
            },
        ] {
            remember(&path, &id, &owned, &Rules::default()).unwrap();
            assert_eq!(recall(&path, Some(&other)), Rules::default());
            assert!(!path.exists());
        }
        fs::write(&path, b"{not json").unwrap();
        assert_eq!(recall(&path, Some(&id)), Rules::default());
        assert!(!path.exists());
        // An unidentifiable environment cannot vouch for a record either.
        remember(&path, &id, &owned, &Rules::default()).unwrap();
        assert_eq!(recall(&path, None), Rules::default());
        assert!(!path.exists());
        assert_eq!(recall(&path, Some(&id)), Rules::default());
    }

    #[test]
    fn nothing_to_remember_removes_the_record() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a").join(LEDGER);
        let id = Identity {
            boot_id: "b".into(),
            netns: 1,
        };
        remember(&path, &id, &Rules::default(), &Rules::default()).unwrap();
        assert!(!path.exists());
        remember(&path, &id, &recorded(), &Rules::default()).unwrap();
        assert!(path.exists());
        remember(&path, &id, &Rules::default(), &Rules::default()).unwrap();
        assert!(!path.exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_running_helper_can_identify_its_boot_and_namespace() {
        let id = Identity::current().unwrap();
        assert!(!id.boot_id.is_empty() && id.netns != 0);
        assert_eq!(id, Identity::current().unwrap());
    }

    // A stand-in for `ip` that answers from files and logs every invocation, so that the
    // commands the helper runs can be checked without privileges.
    const FAKE_IP: &str = r#"d="$1"; shift
echo "$*" >> "$d/log"
case "$*" in
  "-N -4 -j rule show") f=rules4 ;;
  "-N -6 -j rule show") f=rules6 ;;
  "-N -4 -j route show table all"|"-N -6 -j route show table all") echo '[]'; exit 0 ;;
  "-4 rule del priority "*|"-6 rule del priority "*) [ -e "$d/refuse" ] && { echo 'RTNETLINK answers: Operation not permitted' >&2; exit 2; }; touch "$d/deleted"; exit 0 ;;
  *) echo "unexpected: $*" >&2; exit 3 ;;
esac
if [ -e "$d/deleted" ] && [ -e "$d/$f.after" ]; then cat "$d/$f.after"; else cat "$d/$f.before"; fi
"#;

    fn fake_ip(dir: &Path, script: &str) -> Ip {
        Ip {
            program: "sh".into(),
            lead: vec![
                "-c".into(),
                script.into(),
                "ip".into(),
                dir.to_string_lossy().into_owned(),
            ],
        }
    }

    fn log(dir: &Path) -> Vec<String> {
        fs::read_to_string(dir.join("log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn install_fake_state(dir: &Path, deleted: bool) {
        fs::write(dir.join("rules4.before"), LEFTOVER4).unwrap();
        fs::write(dir.join("rules6.before"), LEFTOVER6).unwrap();
        let (after4, after6) = if deleted {
            (
                serde_json::to_string(&system(&parse(LEFTOVER4))).unwrap(),
                serde_json::to_string(&system(&parse(LEFTOVER6))).unwrap(),
            )
        } else {
            (LEFTOVER4.to_owned(), LEFTOVER6.to_owned())
        };
        fs::write(dir.join("rules4.after"), after4).unwrap();
        fs::write(dir.join("rules6.after"), after6).unwrap();
    }

    #[tokio::test]
    async fn kernels_without_ipv6_never_get_an_ip_6_command() {
        let dir = tempfile::tempdir().unwrap();
        install_fake_state(dir.path(), true);
        let ip = fake_ip(dir.path(), FAKE_IP);
        let snapshot = Snapshot::read_with(&ip, false).await.unwrap();
        assert_eq!(snapshot.rules[0], parse(LEFTOVER4));
        assert!(snapshot.rules[1].is_empty() && snapshot.routes[1].is_empty());
        assert_eq!(
            log(dir.path()),
            ["-N -4 -j rule show", "-N -4 -j route show table all"]
        );
        // With IPv6 both families are read, and rules come back normalized.
        let snapshot = Snapshot::read_with(&ip, true).await.unwrap();
        assert_eq!(snapshot.rules[1], parse(RUNNING6));
        assert!(
            log(dir.path())
                .iter()
                .any(|line| line.contains("-N -6 -j rule show"))
        );
    }

    #[tokio::test]
    async fn ip_failures_report_the_command_and_its_stderr() {
        let dir = tempfile::tempdir().unwrap();
        let ip = fake_ip(
            dir.path(),
            r#"echo 'RTNETLINK answers: Address family not supported by protocol' >&2; exit 2"#,
        );
        let error = format!("{:#}", Snapshot::read_with(&ip, true).await.unwrap_err());
        assert!(
            error.contains("cannot inspect existing policy routes"),
            "{error}"
        );
        assert!(error.contains("`ip -N -4 -j rule show`"), "{error}");
        assert!(
            error.contains("RTNETLINK answers: Address family not supported by protocol"),
            "{error}"
        );
        assert!(error.contains("exit status: 2"), "{error}");
    }

    #[tokio::test]
    async fn heal_deletes_each_leftover_by_its_full_rule_and_verifies() {
        let dir = tempfile::tempdir().unwrap();
        install_fake_state(dir.path(), true);
        let ip = fake_ip(dir.path(), FAKE_IP);
        let mut residue = recorded();
        let snapshot = Snapshot::read_with(&ip, true).await.unwrap();
        fs::remove_file(dir.path().join("log")).unwrap();
        let healed = heal_with(&ip, true, snapshot, &Snapshot::default(), &mut residue)
            .await
            .unwrap();
        // Every field the kernel compares, so that a rule replaced since the snapshot is not
        // deleted in its stead; lookups name their action, which `table` alone does not set.
        let mut expected: Vec<String> = [
            "-4 rule del priority 9000 to 198.18.0.0/30 type unicast table 2022",
            "-4 rule del priority 9001 not dport 53 type unicast table 254 suppress_prefixlength 0",
            "-4 rule del priority 9001 iif omash-tun goto 9010",
            "-4 rule del priority 9002 not iif lo type unicast table 2022",
            "-4 rule del priority 9002 from 0.0.0.0/32 iif lo type unicast table 2022",
            "-4 rule del priority 9002 from 198.18.0.0/30 iif lo type unicast table 2022",
            "-4 rule del priority 9010 nop",
            "-6 rule del priority 9000 not dport 53 type unicast table 254 suppress_prefixlength 0",
            "-6 rule del priority 9000 iif omash-tun goto 9010",
            "-6 rule del priority 9000 from ::/1 iif lo goto 9010",
            "-6 rule del priority 9000 from 8000::/1 iif lo goto 9010",
            "-6 rule del priority 9001 from fdfe:dcba:9876::/126 iif lo type unicast table 2022",
            "-6 rule del priority 9002 type unicast table 2022",
            "-6 rule del priority 9010 nop",
        ]
        .map(String::from)
        .into();
        // Then one fresh read of both families verifies the result.
        expected.extend(
            [
                "-N -4 -j rule show",
                "-N -4 -j route show table all",
                "-N -6 -j rule show",
                "-N -6 -j route show table all",
            ]
            .map(String::from),
        );
        assert_eq!(log(dir.path()), expected);
        assert_eq!(residue, Rules::default());
        // The returned snapshot is the post-deletion state: only the system rules remain.
        assert_eq!(healed.rules[0], system(&parse(LEFTOVER4)));
        assert_eq!(healed.rules[1], system(&parse(LEFTOVER6)));
        assert!(
            healed
                .check(&config(2022, 9000), &Snapshot::default())
                .is_ok()
        );
    }

    #[tokio::test]
    async fn a_rule_that_cannot_be_translated_is_never_deleted() {
        let dir = tempfile::tempdir().unwrap();
        install_fake_state(dir.path(), true);
        let ip = fake_ip(dir.path(), FAKE_IP);
        let mut odd = recorded()[0].clone();
        odd[6]["flowlabel"] = json!("0x1");
        assert_eq!(deletion_args(0, &odd[6]), None);
        let mut current = leftovers();
        current[0][7] = odd[6].clone();
        assert_eq!(plan_family(0, &current[0], &[], &odd), Plan::Keep);

        let mut snapshot = Snapshot::with_rules(current);
        snapshot.rules[1].clear();
        let mut residue = [odd.clone(), vec![]];
        let kept = heal_with(
            &ip,
            true,
            snapshot.clone(),
            &Snapshot::default(),
            &mut residue,
        )
        .await
        .unwrap();
        assert_eq!(kept.rules, snapshot.rules);
        assert_eq!(residue[0], odd);
        assert!(log(dir.path()).is_empty());
        // Values the kernel would not compare as recorded are refused too.
        for rule in [
            json!({"priority": 9001, "src": "all", "dport": 53, "dport_mask": "0xff00", "table": "254"}),
            json!({"priority": 9001, "src": "all", "table": "254", "goto": 9010}),
            json!({"priority": 9001, "src": "all"}),
            json!({"priority": 9001, "src": "all", "action": "nat"}),
            json!({"src": "all", "nop": null}),
        ] {
            assert_eq!(deletion_args(0, &rule), None, "{rule}");
        }
    }

    #[tokio::test]
    async fn heal_fails_loudly_when_the_rules_remain() {
        let dir = tempfile::tempdir().unwrap();
        install_fake_state(dir.path(), false);
        let ip = fake_ip(dir.path(), FAKE_IP);
        let mut residue = recorded();
        let snapshot = Snapshot::read_with(&ip, true).await.unwrap();
        let error = heal_with(&ip, true, snapshot, &Snapshot::default(), &mut residue)
            .await
            .unwrap_err();
        let error = format!("{error:#}");
        assert!(error.contains("omash tun doctor"), "{error}");
        assert!(
            error.contains("-4: 7 rules were planned, 0 disappeared"),
            "{error}"
        );
        // Remembered rules are kept so that a later attempt can try again.
        assert_eq!(residue, recorded());
    }

    #[tokio::test]
    async fn heal_reports_a_refused_deletion() {
        let dir = tempfile::tempdir().unwrap();
        install_fake_state(dir.path(), true);
        fs::write(dir.path().join("refuse"), b"").unwrap();
        let ip = fake_ip(dir.path(), FAKE_IP);
        let mut residue = recorded();
        let snapshot = Snapshot::read_with(&ip, true).await.unwrap();
        let error = format!(
            "{:#}",
            heal_with(&ip, true, snapshot, &Snapshot::default(), &mut residue)
                .await
                .unwrap_err()
        );
        assert!(
            error.contains("cannot remove leftover TUN policy rule"),
            "{error}"
        );
        assert!(error.contains("Operation not permitted"), "{error}");
        assert!(error.contains("omash tun doctor"), "{error}");
        assert_eq!(residue, recorded());
        // The first refusal ends the attempt: no further deletions were tried.
        assert_eq!(
            log(dir.path())
                .iter()
                .filter(|line| line.contains(" rule del "))
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn heal_without_residue_or_with_a_foreign_rule_deletes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        install_fake_state(dir.path(), true);
        let ip = fake_ip(dir.path(), FAKE_IP);
        let snapshot = Snapshot::read_with(&ip, true).await.unwrap();
        fs::remove_file(dir.path().join("log")).unwrap();

        let mut none = Rules::default();
        let same = heal_with(&ip, true, snapshot.clone(), &Snapshot::default(), &mut none)
            .await
            .unwrap();
        assert_eq!(same.rules, snapshot.rules);

        let mut residue = [recorded()[0].clone(), vec![]];
        let mut foreign = snapshot.clone();
        foreign.rules[0]
            .push(json!({"priority": 9001, "src": "10.1.0.0", "srclen": 16, "table": "5"}));
        let kept = heal_with(
            &ip,
            true,
            foreign.clone(),
            &Snapshot::default(),
            &mut residue,
        )
        .await
        .unwrap();
        assert_eq!(kept.rules, foreign.rules);
        // The rules stay remembered and the normal check reports the conflict.
        assert_eq!(residue[0], recorded()[0]);
        assert!(
            kept.check(&config(2022, 9000), &Snapshot::default())
                .is_err()
        );
        assert!(log(dir.path()).is_empty());
    }
}
