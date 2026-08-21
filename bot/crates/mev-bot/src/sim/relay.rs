//! Relay / builder simulation backend (`eth_callBundle`).
//!
//! Cross-checks the local fork result against the same simulation the builder
//! would run. Purely read-only: `eth_callBundle` never enqueues anything.

use std::sync::Arc;

use alloy_primitives::U256;
use anyhow::{anyhow, Result};
use serde_json::Value;

use crate::bundle;
use crate::config::Config;
use crate::rpc::RpcClient;
use crate::signer::Signer;
use crate::types::{now_ms, parse_u256, BundleRecord, SimBackend, SimulationResult, Strategy};

pub struct RelaySim {
    rpc: RpcClient,
    signer: Arc<Signer>,
    bribe_bps: u16,
}

impl RelaySim {
    pub fn new(cfg: &Config, signer: Arc<Signer>) -> Result<Self> {
        Ok(Self {
            rpc: RpcClient::new(cfg.endpoints.relay_url.clone())?,
            signer,
            bribe_bps: cfg.risk.bribe_bps,
        })
    }

    /// Ask the relay to simulate the bundle on top of `state_block`.
    pub async fn call_bundle(
        &self,
        bundle: &BundleRecord,
        strategy: Strategy,
        state_block: &str,
    ) -> Result<SimulationResult> {
        let started = std::time::Instant::now();
        let params = bundle::call_bundle_params(bundle, state_block);
        let raw: Value = self
            .rpc
            .call_signed("eth_callBundle", params, &self.signer)
            .await?;

        let coinbase_diff = parse_u256(&raw["coinbaseDiff"]);
        let gas_fees = parse_u256(&raw["gasFees"]);
        let eth_sent = parse_u256(&raw["ethSentToCoinbase"]);
        let gas_used = raw["totalGasUsed"].as_u64().unwrap_or_else(|| {
            raw["results"]
                .as_array()
                .map(|rs| rs.iter().map(|r| r["gasUsed"].as_u64().unwrap_or(0)).sum())
                .unwrap_or(0)
        });

        let mut revert_reason = None;
        let mut success = true;
        if let Some(results) = raw["results"].as_array() {
            for r in results {
                if let Some(err) = r.get("error").and_then(|v| v.as_str()) {
                    success = false;
                    revert_reason = Some(err.to_string());
                }
                if let Some(rv) = r.get("revert").and_then(|v| v.as_str()) {
                    success = false;
                    revert_reason = Some(rv.to_string());
                }
            }
        } else {
            return Err(anyhow!("relay returned no results: {raw}"));
        }

        // `coinbaseDiff` already nets out gas fees paid to the builder, so it is
        // the closest thing the relay gives us to "value delivered".
        let net = crate::sim::anvil::to_i128(coinbase_diff) - crate::sim::anvil::to_i128(gas_fees);

        Ok(SimulationResult {
            opportunity_id: bundle.opportunity_id.clone(),
            strategy,
            backend: SimBackend::RelayCallBundle,
            success: success && coinbase_diff > U256::ZERO,
            gross_profit_wei: coinbase_diff,
            gas_used,
            gas_price_wei: if gas_used > 0 {
                gas_fees / U256::from(gas_used)
            } else {
                U256::ZERO
            },
            gas_cost_wei: gas_fees,
            bribe_wei: eth_sent
                .max(coinbase_diff * U256::from(self.bribe_bps) / U256::from(10_000u32)),
            net_profit_wei: net,
            revert_reason,
            target_block: bundle.target_block,
            sim_latency_ms: started.elapsed().as_millis() as u64,
            created_at_ms: now_ms(),
        })
    }
}
