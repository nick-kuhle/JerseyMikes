//! Simulation backends.
//!
//! `Simulator` is the facade the engine talks to. It runs the local anvil fork
//! first (ground truth, always available) and, when configured, cross-checks
//! with the relay's own `eth_callBundle`.

pub mod anvil;
pub mod relay;

use std::sync::Arc;

use alloy_primitives::U256;
use anyhow::Result;

use crate::bundle::{self, BundleContext};
use crate::config::Config;
use crate::signer::Signer;
use crate::types::{BundleRecord, Opportunity, SimulationResult};

pub struct Simulator {
    cfg: Arc<Config>,
    pub fork: Option<Arc<anvil::AnvilSim>>,
    pub relay: Option<relay::RelaySim>,
    signer: Arc<Signer>,
}

pub struct SimOutcome {
    /// Ground-truth result used for PnL accounting.
    pub primary: SimulationResult,
    /// Relay cross-check, when available.
    pub relay: Option<SimulationResult>,
    /// The bundle that *would* have been submitted.
    pub bundle: BundleRecord,
}

impl Simulator {
    pub fn new(
        cfg: Arc<Config>,
        fork: Option<Arc<anvil::AnvilSim>>,
        relay: Option<relay::RelaySim>,
        signer: Arc<Signer>,
    ) -> Self {
        Self {
            cfg,
            fork,
            relay,
            signer,
        }
    }

    /// Simulate an opportunity and produce the bundle record that goes with it.
    pub async fn run(
        &self,
        opp: &Opportunity,
        victims_raw: &[Vec<u8>],
        victim_sender_nonce: Option<(alloy_primitives::Address, u64)>,
        base_fee: U256,
        nonce: u64,
    ) -> Result<SimOutcome> {
        let executor = self
            .fork
            .as_ref()
            .map(|f| f.executor())
            .or(self.cfg.endpoints.executor)
            .unwrap_or(anvil::SIM_EXECUTOR);

        let ctx = BundleContext {
            chain_id: self.cfg.chain.chain_id,
            executor,
            nonce,
            base_fee,
            priority_fee: U256::from(1_000_000_000u64),
            gas_limit: self.cfg.risk.max_gas_per_bundle,
        };
        let bundle = bundle::build_bundle(opp, victims_raw, &ctx, &self.cfg.risk, &self.signer);

        let primary = match &self.fork {
            Some(fork) => {
                fork.ensure_fork_at(opp.target_block.saturating_sub(1)).await.ok();
                fork.simulate(opp, victims_raw, victim_sender_nonce, base_fee).await?
            }
            None => crate::sim::empty_result(opp, "no simulation backend configured"),
        };

        let relay_result = match (&self.relay, self.cfg.sim.use_call_bundle) {
            (Some(r), true) => match r.call_bundle(&bundle, opp.strategy, "latest").await {
                Ok(res) => Some(res),
                Err(e) => {
                    tracing::debug!(target: "sim", error = %e, "relay call_bundle failed");
                    None
                }
            },
            _ => None,
        };

        Ok(SimOutcome {
            primary,
            relay: relay_result,
            bundle,
        })
    }
}

pub fn empty_result(opp: &Opportunity, reason: &str) -> SimulationResult {
    SimulationResult {
        opportunity_id: opp.id.clone(),
        strategy: opp.strategy,
        backend: crate::types::SimBackend::EthCall,
        success: false,
        gross_profit_wei: U256::ZERO,
        gas_used: 0,
        gas_price_wei: U256::ZERO,
        gas_cost_wei: U256::ZERO,
        bribe_wei: U256::ZERO,
        net_profit_wei: 0,
        revert_reason: Some(reason.to_string()),
        target_block: opp.target_block,
        sim_latency_ms: 0,
        created_at_ms: crate::types::now_ms(),
    }
}
