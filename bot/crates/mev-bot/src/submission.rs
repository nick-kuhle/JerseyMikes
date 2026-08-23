//! Multi-relay private bundle submission.
//!
//! Construction is always available so the exact payload is exercised in
//! shadow mode. `Engine` is the sole caller and invokes this transport only
//! after boot arming, runtime live mode, `BROADCAST_ENABLED`, qualification,
//! risk, and strategy eligibility all pass.

use std::sync::Arc;

use serde_json::{json, Value};

use crate::rpc::RpcClient;
use crate::signer::Signer;
use crate::store::Store;
use crate::types::BundleRecord;

pub struct SubmissionGateway {
    relays: Vec<(String, RpcClient)>,
    signer: Arc<Signer>,
    store: Arc<Store>,
    retry_delay: std::time::Duration,
    max_attempts: u64,
}

impl SubmissionGateway {
    pub fn new(
        urls: &[String],
        signer: Arc<Signer>,
        store: Arc<Store>,
        retry_ms: u64,
        max_attempts: u64,
    ) -> Self {
        let relays = urls
            .iter()
            .filter_map(|url| {
                RpcClient::new(url.clone())
                    .ok()
                    .map(|rpc| (url.clone(), rpc))
            })
            .collect();
        Self {
            relays,
            signer,
            store,
            retry_delay: std::time::Duration::from_millis(retry_ms),
            max_attempts: max_attempts.max(1),
        }
    }

    pub async fn submit(&self, bundle: &BundleRecord) -> bool {
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

    /// Cancel the replacement UUID at every configured relay. Returns true
    /// only when each relay acknowledged the request; callers otherwise keep
    /// nonce reuse blocked until the bundle's target block has expired.
    pub async fn cancel(&self, replacement_uuid: &str) -> bool {
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
}
