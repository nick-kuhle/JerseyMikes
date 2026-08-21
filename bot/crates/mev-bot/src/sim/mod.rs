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
    /// Dedicated fork for replaying already-mined transactions.
    ///
    /// Replay pins to the parent of a historical block; the live fork tracks
    /// the head. Sharing one instance between them means an `anvil_reset` on
    /// every alternation — seconds of refork each time, in both directions,
    /// while the mempool path waits behind the same mutex. Two instances cost
    /// a second anvil and remove the thrash entirely.
    pub replay_fork: Option<Arc<anvil::AnvilSim>>,
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
        replay_fork: Option<Arc<anvil::AnvilSim>>,
        relay: Option<relay::RelaySim>,
        signer: Arc<Signer>,
    ) -> Self {
        Self {
            cfg,
            fork,
            replay_fork,
            relay,
            signer,
        }
    }

    /// Simulate an opportunity and produce the bundle record that goes with it.
    ///
    /// `replay` selects the historical lane: the opportunity was built from a
    /// transaction that is already on chain, so it is simulated on the replay
    /// fork pinned to `target_block - 1` rather than on the live fork.
    pub async fn run(
        &self,
        opp: &Opportunity,
        victims_raw: &[Vec<u8>],
        victim_sender_nonce: Option<(alloy_primitives::Address, u64)>,
        base_fee: U256,
        nonce: u64,
        replay: bool,
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

        let parent = opp.target_block.saturating_sub(1);
        let primary = if replay {
            match &self.replay_fork {
                Some(fork) => {
                    // Exact pin: the parent of the victim's own block. Anything
                    // else and the victim's nonce, the pool reserves and the
                    // oracle prices all belong to a different chain state.
                    fork.simulate_at(parent, opp, victims_raw, victim_sender_nonce, base_fee).await?
                }
                // Refusing to score is the honest outcome. Simulating a mined
                // transaction on the live fork answers a question nobody asked
                // — "would this have worked against a state it never saw?" —
                // and reports it as if it were a real result.
                None => crate::sim::empty_result(
                    opp,
                    "no replay fork: enable REPLAY_FORK to score delivered blocks",
                ),
            }
        } else {
            match &self.fork {
                Some(fork) => {
                    fork.ensure_fork_at(parent).await.ok();
                    fork.simulate(opp, victims_raw, victim_sender_nonce, base_fee).await?
                }
                None => crate::sim::empty_result(opp, "no simulation backend configured"),
            }
        };

        // `eth_callBundle` against "latest" is meaningless for a block that was
        // mined hours ago; point it at the same parent the fork used.
        let block_tag = if replay {
            format!("0x{parent:x}")
        } else {
            "latest".to_string()
        };
        let relay_result = match (&self.relay, self.cfg.sim.use_call_bundle) {
            (Some(r), true) => match r.call_bundle(&bundle, opp.strategy, &block_tag).await {
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
