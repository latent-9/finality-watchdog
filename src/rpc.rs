//! Thin JSON-RPC over HTTP for the polling calls (`getEpochInfo`,
//! `getLeaderSchedule`, `getVoteAccounts`).
//!
//! The websocket stream is push-based; only these three calls poll. They run
//! on a blocking thread via `spawn_blocking` (ureq) so the async runtime's
//! workers are never blocked on I/O.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{anyhow, Context};
use serde_json::Value;

use crate::stake::{StakeMap, VoteAccount};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// One JSON-RPC call; returns the `result` value. Non-2xx and `error` fields
/// surface as errors.
async fn rpc(url: &str, method: &str, params: Value) -> anyhow::Result<Value> {
    let url = url.to_string();
    let body = serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
    let text = tokio::task::spawn_blocking(move || {
        let response = ureq::post(&url)
            .timeout(REQUEST_TIMEOUT)
            .set("content-type", "application/json")
            .send_string(&body.to_string())
            .map_err(|e| anyhow!("http: {e}"))?;
        response
            .into_string()
            .map_err(|e| anyhow!("read body: {e}"))
    })
    .await
    .map_err(|e| anyhow!("join: {e}"))??;
    let value: Value = serde_json::from_str(&text)
        .with_context(|| format!("json: {}", &text[..text.len().min(200)]))?;
    if let Some(err) = value.get("error") {
        anyhow::bail!("rpc error from {method}: {err}");
    }
    value
        .get("result")
        .cloned()
        .ok_or_else(|| anyhow!("no result in {method} response"))
}

/// Fetches `getEpochInfo`.
pub async fn fetch_epoch_info(url: &str) -> anyhow::Result<Value> {
    rpc(url, "getEpochInfo", serde_json::json!([])).await
}

/// Fetches the leader schedule as identity → relative indices.
///
/// Called with NO params: the public RPC returns `null` for an explicit
/// epoch (verified live against api.mainnet-beta.solana.com), and without
/// params it serves the current epoch's schedule, which is exactly what the
/// watcher wants, since the map is refreshed per epoch anyway.
pub async fn fetch_leader_schedule(url: &str) -> anyhow::Result<HashMap<String, Vec<u64>>> {
    let value = rpc(url, "getLeaderSchedule", serde_json::json!([])).await?;
    serde_json::from_value(value).context("leader schedule shape")
}

/// Fetches the current (non-delinquent) and delinquent vote accounts.
pub async fn fetch_vote_accounts(url: &str) -> anyhow::Result<Vec<VoteAccount>> {
    let value = rpc(url, "getVoteAccounts", serde_json::json!([])).await?;
    let mut accounts = Vec::new();
    for key in ["current", "delinquent"] {
        let bucket = value
            .get(key)
            .cloned()
            .ok_or_else(|| anyhow!("vote accounts without {key} bucket"))?;
        let mut parsed: Vec<VoteAccount> =
            serde_json::from_value(bucket).with_context(|| format!("{key} vote accounts shape"))?;
        if key == "delinquent" {
            for account in &mut parsed {
                account.delinquent = true;
            }
        }
        accounts.extend(parsed);
    }
    Ok(accounts)
}

/// Fetches and assembles the full [`StakeMap`] for the current epoch.
pub async fn fetch_stake_map(url: &str) -> anyhow::Result<StakeMap> {
    let info = fetch_epoch_info(url).await?;
    let absolute_slot = info
        .get("absoluteSlot")
        .and_then(Value::as_u64)
        .ok_or_else(|| anyhow!("epoch info without absoluteSlot"))?;
    let slot_index = info
        .get("slotIndex")
        .and_then(Value::as_u64)
        .ok_or_else(|| anyhow!("epoch info without slotIndex"))?;
    let slots_in_epoch = info
        .get("slotsInEpoch")
        .and_then(Value::as_u64)
        .ok_or_else(|| anyhow!("epoch info without slotsInEpoch"))?;
    let epoch = info
        .get("epoch")
        .and_then(Value::as_u64)
        .ok_or_else(|| anyhow!("epoch info without epoch"))?;
    let epoch_start = absolute_slot - slot_index;
    let schedule = fetch_leader_schedule(url).await?;
    let vote_accounts = fetch_vote_accounts(url).await?;
    Ok(StakeMap::from_parts(
        epoch,
        epoch_start,
        slots_in_epoch,
        &schedule,
        &vote_accounts,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Vote account parsing: both bucket shapes deserialize correctly.
    #[test]
    fn vote_account_deserializes() {
        let current: Vec<VoteAccount> = serde_json::from_str(
            r#"[{"votePubkey":"V1","nodePubkey":"N1","activatedStake":100,"delinquent":false},
                {"votePubkey":"V2","nodePubkey":"N2","activatedStake":200}]"#,
        )
        .unwrap();
        assert_eq!(current.len(), 2);
        assert!(!current[0].delinquent);
        assert_eq!(current[1].activated_stake, 200);
        // Delinquent bucket entries are flagged by fetch_vote_accounts; the
        // struct itself deserializes the field when present.
        let delinquent: Vec<VoteAccount> = serde_json::from_str(
            r#"[{"votePubkey":"V3","nodePubkey":"N3","activatedStake":50,"delinquent":true}]"#,
        )
        .unwrap();
        assert!(delinquent[0].delinquent);
    }
}
