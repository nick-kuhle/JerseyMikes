//! Payload submission: relay bundles (mainnet) or raw transactions
//! (sequencer chains, v1: Base).
//!
//! Construction is always available so the exact payload is exercised in
//! shadow mode. `Engine` is the sole caller and invokes this transport only
//! after boot arming, runtime live mode, `BROADCAST_ENABLED`, qualification,
//! risk, and strategy eligibility all pass — the nine gates are identical
//! for both transports; only the delivery differs.
//!
//! - **`SubmissionMode::Bundle`**: `eth_sendBundle` to every configured
//!   relay, same-UUID retries, `eth_cancelBundle` cancellation.
//! - **`SubmissionMode::Raw`**: the signed transactions go straight to the
//!   chain's RPC (`eth_sendRawTransaction`) with the configured priority
//!   fee — a sequencer chain has no relay/builder market to send to, and
//!   the priority fee *is* the ordering currency. Cancellation is a
//!   same-nonce replacement transaction (a self-transfer at a bumped fee),
//!   not `eth_cancelBundle`: a raw tx that is already mined cannot be
//!   cancelled, and the gateway reports that as "not acknowledged" so the
//!   caller keeps nonce reuse blocked (fail-closed, same as an
//!   unacknowledged bundle cancellation).
//!
//! Raw mode additionally refuses bundles containing foreign (victim)
//! transactions: `eth_sendRawTransaction` can only send *our* signed
//! payloads, and a victim's signed tx cannot be re-sent. The engine gates
//! this before the nonce lane as well — this check is the transport-level
//! backstop.

use std::sync::Arc;

use alloy_primitives::{Address, U256};
use serde_json::{json, Value};

use crate::config::SubmissionMode;
use crate::rpc::RpcClient;
use crate::signer::Eip1559Tx;
use crate::signer::Signer;
use crate::store::Store;
use crate::types::BundleRecord;

const CANCEL_PRIORITY_FLOOR_BUMP_WEI: u64 = 1_000_000_000;

/// Worst-case gas exposure of the bot-owned transactions in a raw bundle.
/// Malformed/non-type-2 payloads return `None` so smoke fails closed.
pub fn raw_bundle_gas_at_risk(bundle: &BundleRecord) -> Option<U256> {
    let mut total = U256::ZERO;
    for tx in &bundle.txs {
        if tx.foreign {
            return None;
        }
        let envelope = crate::rlp::decode_eip1559_envelope(&tx.raw)?;
        total = total.saturating_add(
            envelope
                .max_fee_per_gas
                .saturating_mul(U256::from(envelope.gas_limit)),
        );
    }
    Some(total)
}

fn bump_by_bps(value: U256, bump_bps: u64) -> U256 {
    let bump = value
        .saturating_mul(U256::from(bump_bps))
        .saturating_add(U256::from(9_999u64))
        / U256::from(10_000u64);
    value.saturating_add(bump)
}

/// Build fee caps that satisfy both replacement pricing and current base fee.
/// Returning `None` means the operator's hard cap cannot fund a valid cancel.
fn classify_raw_rpc_result(result: anyhow::Result<Value>) -> (bool, Value) {
    match result {
        // RpcClient already unwraps JSON-RPC's outer `result` field.
        Ok(value) => (true, json!({"result": value})),
        Err(error) => (false, json!({"error": error.to_string()})),
    }
}

fn replacement_fees(
    original_priority: U256,
    original_max_fee: U256,
    current_base_fee: U256,
    configured_priority: U256,
    bump_bps: u64,
    hard_max_fee: U256,
) -> Option<(U256, U256)> {
    let priority = bump_by_bps(original_priority, bump_bps)
        .max(configured_priority.saturating_add(U256::from(CANCEL_PRIORITY_FLOOR_BUMP_WEI)));
    let max_fee = bump_by_bps(original_max_fee, bump_bps).max(
        current_base_fee
            .saturating_mul(U256::from(2u8))
            .saturating_add(priority),
    );
    if hard_max_fee.is_zero() || max_fee > hard_max_fee || priority > max_fee {
        None
    } else {
        Some((priority, max_fee))
    }
}

pub struct SubmissionGateway {
    mode: SubmissionMode,
    relays: Vec<(String, RpcClient)>,
    /// Raw mode: the chain RPC the signed transactions go straight to.
    raw_rpc: Option<RpcClient>,
    /// Raw mode: the searcher (tx signer) — signs the replacement
    /// transactions used for cancellation.
    tx_signer: Option<Arc<Signer>>,
    searcher: Option<Address>,
    chain_id: u64,
    /// Raw mode: the priority fee a replacement (cancellation) tx bids.
    /// Bundles' own fee is set at signing time by the simulator.
    raw_priority_fee: U256,
    /// Percentage increase over both original fee caps for replacements.
    raw_cancel_bump_bps: u64,
    /// Operator hard ceiling; cancellation fails closed above this fee cap.
    raw_cancel_max_fee: U256,
    signer: Arc<Signer>,
    store: Arc<Store>,
    retry_delay: std::time::Duration,
    max_attempts: u64,
}

impl SubmissionGateway {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        urls: &[String],
        signer: Arc<Signer>,
        store: Arc<Store>,
        retry_ms: u64,
        max_attempts: u64,
        mode: SubmissionMode,
        raw_rpc_url: Option<String>,
        tx_signer: Option<Arc<Signer>>,
        searcher: Option<Address>,
        chain_id: u64,
        raw_priority_fee: U256,
        raw_cancel_bump_bps: u64,
        raw_cancel_max_fee: U256,
    ) -> Self {
        let relays = urls
            .iter()
            .filter_map(|url| {
                RpcClient::new(url.clone())
                    .ok()
                    .map(|rpc| (url.clone(), rpc))
            })
            .collect();
        // Raw mode needs a chain RPC and the tx signer; if either is missing
        // the gateway degrades to "raw submission always fails" (fail-closed:
        // a live-armed Base instance with no raw path sends nothing, never
        // falls back to a relay that does not exist there).
        let raw_rpc = raw_rpc_url.and_then(|u| RpcClient::new(u).ok());
        Self {
            mode,
            relays,
            raw_rpc,
            tx_signer,
            searcher,
            chain_id,
            raw_priority_fee,
            raw_cancel_bump_bps,
            raw_cancel_max_fee,
            signer,
            store,
            retry_delay: std::time::Duration::from_millis(retry_ms),
            max_attempts: max_attempts.max(1),
        }
    }

    pub fn mode(&self) -> SubmissionMode {
        self.mode
    }

    pub async fn submit(&self, bundle: &BundleRecord) -> bool {
        if self.raw_rpc.is_some() {
            return self.submit_raw(bundle).await;
        }
        if self.relays.is_empty() {
            return false;
        }
        let params = crate::bundle::send_bundle_params(bundle);
        let mut set = tokio::task::JoinSet::new();
        for (relay, rpc) in &self.relays {
            let relay = relay.clone();
            let rpc = rpc.clone();
            let signer = self.signer.clone();
            let params = params.clone();
            let max_attempts = self.max_attempts;
            let retry_delay = self.retry_delay;
            set.spawn(async move {
                let mut last = json!({"error": "submission not attempted"});
                for attempt in 1..=max_attempts {
                    let result = tokio::time::timeout(
                        std::time::Duration::from_secs(3),
                        rpc.call_signed("eth_sendBundle", params.clone(), &signer),
                    )
                    .await;
                    match result {
                        Ok(Ok(value)) => {
                            return (
                                relay,
                                true,
                                json!({
                                    "attempt": attempt,
                                    "result": value
                                }),
                            );
                        }
                        Ok(Err(error)) => {
                            last = json!({"attempt": attempt, "error": error.to_string()});
                        }
                        Err(_) => {
                            last = json!({"attempt": attempt, "error": "submission timeout"});
                        }
                    }
                    if attempt < max_attempts {
                        tokio::time::sleep(retry_delay).await;
                    }
                }
                (relay, false, last)
            });
        }
        let mut accepted = false;
        while let Some(result) = set.join_next().await {
            let Ok((relay, ok, response)) = result else {
                continue;
            };
            accepted |= ok;
            let _ = self.store.record_relay_submission(
                &bundle.id,
                &bundle.opportunity_id,
                &relay,
                ok,
                &response,
            );
            if ok {
                tracing::info!(target: "submission", %relay, bundle = %bundle.id, "bundle accepted");
            } else {
                tracing::warn!(target: "submission", %relay, bundle = %bundle.id, %response, "bundle rejected");
            }
        }
        accepted
    }

    /// Raw transport: every non-foreign signed transaction goes straight to
    /// the chain. Foreign (victim) transactions are a transport-level
    /// refusal — they cannot be re-sent (the engine gates this earlier;
    /// this is the backstop).
    async fn submit_raw(&self, bundle: &BundleRecord) -> bool {
        if bundle.txs.iter().any(|t| t.foreign) {
            tracing::warn!(
                target: "submission",
                bundle = %bundle.id,
                "raw transport cannot include foreign (victim) transactions — \
                 refusing (sequencer chains are back-run-only at the transport \
                 level; victim-pinned strategies need a private-orderflow API)"
            );
            return false;
        }
        let rpc = match &self.raw_rpc {
            Some(r) => r,
            None => {
                tracing::error!(
                    target: "submission",
                    bundle = %bundle.id,
                    "raw submission configured but no chain RPC available — refusing"
                );
                return false;
            }
        };
        let mut all_accepted = true;
        for tx in &bundle.txs {
            let raw_hex = format!("0x{}", hex::encode(&tx.raw));
            let result = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                rpc.call_raw("eth_sendRawTransaction", json!([raw_hex])),
            )
            .await;
            // `RpcClient::call_raw` already unwraps JSON-RPC's `result`
            // field. Any `Ok(value)` is acceptance; looking for another nested
            // `result` would mark a successfully broadcast transaction false.
            let (ok, response) = match result {
                Ok(result) => classify_raw_rpc_result(result),
                Err(_) => (false, json!({"error": "submission timeout"})),
            };
            all_accepted &= ok;
            let _ = self.store.record_relay_submission(
                &bundle.id,
                &bundle.opportunity_id,
                "raw",
                ok,
                &response,
            );
            if ok {
                tracing::info!(target: "submission", bundle = %bundle.id, "raw tx accepted by sequencer");
            } else {
                tracing::warn!(target: "submission", bundle = %bundle.id, %response, "raw tx rejected");
            }
        }
        all_accepted
    }

    /// Cancel the replacement UUID at every configured relay (bundle mode),
    /// or replace each pending raw transaction with a self-transfer
    /// cancellation (raw mode). Returns true only when every transaction was
    /// actually cancelled; callers keep nonce reuse blocked otherwise
    /// (fail-closed).
    pub async fn cancel(&self, replacement_uuid: &str) -> bool {
        if self.raw_rpc.is_some() {
            return self.cancel_raw(replacement_uuid).await;
        }
        if self.relays.is_empty() {
            return false;
        }
        let mut all_acknowledged = true;
        for (relay, rpc) in &self.relays {
            let result: Result<Value, _> = rpc
                .call_signed(
                    "eth_cancelBundle",
                    json!([{"replacementUuid": replacement_uuid}]),
                    &self.signer,
                )
                .await;
            let acknowledged = result
                .as_ref()
                .map(|value| value.as_bool().unwrap_or(true))
                .unwrap_or(false);
            all_acknowledged &= acknowledged;
            let response = result.unwrap_or_else(|error| json!({"error": error.to_string()}));
            tracing::info!(target: "submission", %relay, %replacement_uuid, %response, acknowledged, "bundle cancellation sent");
        }
        all_acknowledged
    }

    /// Raw-mode cancellation: for each transaction of the bundle (looked up
    /// by id in the durable store), a mined tx cannot be cancelled — report
    /// not-acknowledged; a pending one is replaced by a same-nonce
    /// self-transfer at a bumped priority fee.
    async fn cancel_raw(&self, replacement_uuid: &str) -> bool {
        let (rpc, signer, searcher) = match (
            self.raw_rpc.as_ref(),
            self.tx_signer.as_ref(),
            self.searcher,
        ) {
            (Some(r), Some(s), Some(a)) => (r, s, a),
            _ => {
                tracing::error!(
                    target: "submission",
                    %replacement_uuid,
                    "raw cancellation configured but rpc/signer/searcher missing — not acknowledged"
                );
                return false;
            }
        };
        let Some(raw_txs) = self.store.bundle_raw_txs(replacement_uuid) else {
            tracing::error!(
                target: "submission",
                %replacement_uuid,
                "cancellation refused: bundle payload not found in the store"
            );
            return false;
        };
        let current_base_fee = match rpc
            .call_raw("eth_getBlockByNumber", json!(["latest", false]))
            .await
        {
            Ok(block) => {
                let parsed = block["baseFeePerGas"]
                    .as_str()
                    .and_then(|value| value.strip_prefix("0x"))
                    .and_then(|hex| U256::from_str_radix(hex, 16).ok());
                match parsed {
                    Some(value) => value,
                    None => {
                        tracing::error!(
                            target: "submission",
                            %replacement_uuid,
                            "cancellation refused: latest block has no valid baseFeePerGas"
                        );
                        return false;
                    }
                }
            }
            Err(error) => {
                tracing::error!(
                    target: "submission",
                    %replacement_uuid,
                    %error,
                    "cancellation refused: current base fee unavailable"
                );
                return false;
            }
        };
        let mut all_cancelled = true;
        for (i, raw) in raw_txs.iter().enumerate() {
            let tx_hash = alloy_primitives::keccak256(raw);
            // A mined transaction is final: nothing replaces it.
            let hash = format!("{tx_hash:?}");
            {
                let receipt = rpc
                    .call_raw("eth_getTransactionReceipt", json!([hash]))
                    .await
                    .ok()
                    .filter(|v| !v.is_null());
                if receipt.is_some() {
                    tracing::warn!(
                        target: "submission",
                        %replacement_uuid,
                        index = i,
                        %hash,
                        "raw tx already mined — cannot cancel; nonce reuse stays blocked"
                    );
                    all_cancelled = false;
                    continue;
                }
            }
            let Some(original) = crate::rlp::decode_eip1559_envelope(raw) else {
                tracing::error!(
                    target: "submission",
                    %replacement_uuid,
                    index = i,
                    "cancellation refused: original is not a valid signed type-2 transaction"
                );
                all_cancelled = false;
                continue;
            };
            let Some((priority, max_fee)) = replacement_fees(
                original.max_priority_fee_per_gas,
                original.max_fee_per_gas,
                current_base_fee,
                self.raw_priority_fee,
                self.raw_cancel_bump_bps,
                self.raw_cancel_max_fee,
            ) else {
                tracing::error!(
                    target: "submission",
                    %replacement_uuid,
                    index = i,
                    hard_cap = %self.raw_cancel_max_fee,
                    "cancellation refused: a valid replacement exceeds RAW_CANCEL_MAX_FEE_WEI"
                );
                all_cancelled = false;
                continue;
            };
            // Replacement: same nonce, self-transfer, both fee caps bumped
            // over the original and sufficient for the current base fee.
            let tx = Eip1559Tx {
                chain_id: self.chain_id,
                nonce: original.nonce,
                max_priority_fee_per_gas: priority,
                max_fee_per_gas: max_fee,
                gas_limit: 21_000,
                to: Some(searcher),
                value: U256::ZERO,
                data: Vec::new(),
            };
            let (raw, _) = signer.sign_eip1559(&tx);
            let result = rpc
                .call_raw(
                    "eth_sendRawTransaction",
                    json!([format!("0x{}", hex::encode(&raw))]),
                )
                .await;
            // `call_raw` returns the unwrapped result; transport/RPC success
            // means the replacement was accepted into the pool.
            let ok = result.is_ok();
            all_cancelled &= ok;
            tracing::info!(
                target: "submission",
                %replacement_uuid,
                index = i,
                nonce = original.nonce,
                max_priority_fee_per_gas = %priority,
                max_fee_per_gas = %max_fee,
                acknowledged = ok,
                "raw replacement (cancellation) sent"
            );
        }
        all_cancelled
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signer::{Eip1559Tx, Signer};
    use crate::types::{BundleTx, Strategy};

    #[test]
    fn unwrapped_raw_transaction_hash_is_acceptance() {
        let (accepted, response) = classify_raw_rpc_result(Ok(json!(
            "0x1111111111111111111111111111111111111111111111111111111111111111"
        )));
        assert!(accepted);
        assert!(response.get("result").is_some());
    }

    #[test]
    fn replacement_bumps_original_caps_and_covers_base_fee() {
        let gwei = U256::from(1_000_000_000u64);
        let (priority, max_fee) = replacement_fees(
            gwei * U256::from(2u64),
            gwei * U256::from(20u64),
            gwei * U256::from(15u64),
            gwei,
            1_250,
            gwei * U256::from(100u64),
        )
        .unwrap();
        assert_eq!(priority, U256::from(2_250_000_000u64));
        assert_eq!(max_fee, gwei * U256::from(30u64) + priority);
        assert!(replacement_fees(
            gwei * U256::from(2u64),
            gwei * U256::from(20u64),
            gwei * U256::from(50u64),
            gwei,
            1_250,
            gwei * U256::from(100u64),
        )
        .is_none());
    }

    #[test]
    fn raw_smoke_risk_uses_signed_gas_limit_times_fee_cap() {
        let signer = Signer::from_hex(Signer::SIMULATION_KEY).unwrap();
        let tx = Eip1559Tx {
            chain_id: 8453,
            nonce: 7,
            max_priority_fee_per_gas: U256::from(2u64),
            max_fee_per_gas: U256::from(10u64),
            gas_limit: 42_000,
            to: Some(Address::with_last_byte(1)),
            value: U256::ZERO,
            data: Vec::new(),
        };
        let (raw, hash) = signer.sign_eip1559(&tx);
        let bundle = BundleRecord {
            id: "bundle".into(),
            opportunity_id: "opportunity".into(),
            strategy: Strategy::AtomicArb,
            target_block: 1,
            txs: vec![BundleTx {
                hash: Some(hash),
                raw,
                can_revert: false,
                foreign: false,
            }],
            submitted: false,
            included: None,
            created_at_ms: 0,
        };
        assert_eq!(
            raw_bundle_gas_at_risk(&bundle),
            Some(U256::from(420_000u64))
        );
    }
}
