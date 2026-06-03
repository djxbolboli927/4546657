use anyhow::{Context, Result};
use std::collections::HashSet;

/// Phase A subscription set: for each pool in mix.json take the minimal three
/// accounts the friend asked us to start with —
///   pool.pubkey, params.tokenAccountA, params.tokenAccountB.
///
/// Collection stops once `max_accounts` distinct pubkeys are gathered. The full
/// account taxonomy (vaults, tick/bin arrays, oracles, …) is deferred to later
/// phases — Phase A only needs to prove decode + delivery work.
pub fn load_subscription_accounts(path: &str, max_accounts: usize) -> Result<Vec<String>> {
    let data = std::fs::read_to_string(path)
        .with_context(|| format!("reading mix.json at {path}"))?;
    let json: serde_json::Value =
        serde_json::from_str(&data).context("parsing mix.json")?;
    let pools = json
        .as_array()
        .context("mix.json top-level is not a JSON array")?;

    let mut out: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    for pool in pools {
        if out.len() >= max_accounts {
            break;
        }

        let mut candidates: Vec<Option<&str>> = Vec::with_capacity(3);
        candidates.push(pool.get("pubkey").and_then(|v| v.as_str()));
        if let Some(params) = pool.get("params") {
            candidates.push(params.get("tokenAccountA").and_then(|v| v.as_str()));
            candidates.push(params.get("tokenAccountB").and_then(|v| v.as_str()));
        }

        for c in candidates.into_iter().flatten() {
            if out.len() >= max_accounts {
                break;
            }
            if !c.is_empty() && seen.insert(c.to_string()) {
                out.push(c.to_string());
            }
        }
    }

    Ok(out)
}
