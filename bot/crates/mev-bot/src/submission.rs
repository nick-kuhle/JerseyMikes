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
}

impl SubmissionGateway {
    pub fn new(urls: &[String], signer: Arc<Signer>, store: Arc<Store>) -> Self {
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
            set.spawn(async move {
                let result = tokio::time::timeout(
                    std::time::Duration::from_secs(3),
                    rpc.call_signed("eth_sendBundle", params, &signer),
                )
                .await;
                match result {
                    Ok(Ok(value)) => (relay, true, value),
                    Ok(Err(error)) => (relay, false, json!({"error": error.to_string()})),
                    Err(_) => (relay, false, json!({"error": "submission timeout"})),
                }
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

    pub async fn cancel(&self, replacement_uuid: &str) {
        for (relay, rpc) in &self.relays {
            let response: Value = rpc
                .call_signed(
                    "eth_cancelBundle",
                    json!([{"replacementUuid": replacement_uuid}]),
                    &self.signer,
                )
                .await
                .unwrap_or_else(|error| json!({"error": error.to_string()}));
            tracing::info!(target: "submission", %relay, %replacement_uuid, %response, "bundle cancellation sent");
        }
    }
}
