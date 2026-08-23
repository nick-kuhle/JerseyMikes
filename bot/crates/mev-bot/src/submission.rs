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
    /// Raw mode: how much a replacement tx bumps over the bundle's own
    /// priority fee so the sequencer pool actually accepts it.
    raw_cancel_bump: U256,
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
            raw_cancel_bump: U256::from(1_000_000_000u64), // +1 gwei
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
            let response = match result {
                Ok(Ok(v)) => v,
                Ok(Err(e)) => json!({"error": e.to_string()}),
                Err(_) => json!({"error": "submission timeout"}),
            };
            let ok = response.get("result").is_some();
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
            let Some(nonce) = crate::rlp::decode_eip1559_nonce(raw) else {
                all_cancelled = false;
                continue;
            };
            // Replacement: same nonce, self-transfer, bumped fee.
            let tx = Eip1559Tx {
                chain_id: self.chain_id,
                nonce,
                max_priority_fee_per_gas: self.raw_priority_fee + self.raw_cancel_bump,
                max_fee_per_gas: (self.raw_priority_fee + self.raw_cancel_bump) * U256::from(2u8),
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
            let ok = result
                .as_ref()
                .map(|v| v.get("result").is_some())
                .unwrap_or(false);
            all_cancelled &= ok;
            tracing::info!(
                target: "submission",
                %replacement_uuid,
                index = i,
                nonce,
                acknowledged = ok,
                "raw replacement (cancellation) sent"
            );
        }
        all_cancelled
    }
}
