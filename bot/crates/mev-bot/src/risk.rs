//! Risk management.
//!
//! The defaults are deliberately permissive — the first iteration is about
//! discovering how much MEV is reachable, not about being selective — but every
//! knob that matters is here and can be tightened from the environment without
//! touching code.
//!
//! Two invariants hold regardless of configuration:
//!   1. nothing is ever broadcast while `live_execution` is false,
//!   2. a bundle that is not net-positive in simulation is never marked
//!      submittable, and on chain the executor reverts it anyway — a reverted
//!      private bundle is simply dropped by the builder and costs no gas.

use std::collections::HashMap;
use std::sync::Arc;

use alloy_primitives::U256;
use parking_lot::RwLock;

use crate::config::Config;
use crate::types::{Opportunity, SimulationResult, Strategy};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reject {
    Disabled,
    TooLarge,
    BaseFeeTooHigh,
    TooManyInflight,
    KillSwitch,
    NoCalls,
}

impl Reject {
    pub fn as_str(&self) -> &'static str {
        match self {
            Reject::Disabled => "strategy_disabled",
            Reject::TooLarge => "position_too_large",
            Reject::BaseFeeTooHigh => "base_fee_too_high",
            Reject::TooManyInflight => "too_many_inflight",
            Reject::KillSwitch => "kill_switch",
            Reject::NoCalls => "empty_opportunity",
        }
    }
}

pub struct RiskEngine {
    cfg: Arc<Config>,
    inflight: RwLock<HashMap<Strategy, usize>>,
    /// Running simulated PnL in wei; drives the drawdown kill switch.
    cumulative_net: RwLock<i128>,
    tripped: RwLock<bool>,
}

impl RiskEngine {
    pub fn new(cfg: Arc<Config>) -> Self {
        Self {
            cfg,
            inflight: RwLock::new(HashMap::new()),
            cumulative_net: RwLock::new(0),
            tripped: RwLock::new(false),
        }
    }

    pub fn enabled(&self, s: Strategy) -> bool {
        let t = &self.cfg.strategies;
        match s {
            Strategy::Sandwich => t.sandwich,
            Strategy::Jit => t.jit,
            Strategy::AtomicArb => t.atomic_arb,
            Strategy::Liquidation => t.liquidation,
            Strategy::Sniper => t.sniper,
        }
    }

    /// Gate an opportunity before it costs us a simulation slot.
    pub fn check(&self, opp: &Opportunity, base_fee: U256) -> Result<(), Reject> {
        if !self.enabled(opp.strategy) {
            return Err(Reject::Disabled);
        }
        if *self.tripped.read() {
            return Err(Reject::KillSwitch);
        }
        if opp.front_calls.is_empty() && opp.back_calls.is_empty() {
            return Err(Reject::NoCalls);
        }
        if opp.notional_wei > self.cfg.risk.max_position_wei {
            return Err(Reject::TooLarge);
        }
        if base_fee > self.cfg.risk.max_base_fee_wei {
            return Err(Reject::BaseFeeTooHigh);
        }
        let inflight = self.inflight.read();
        if inflight.get(&opp.strategy).copied().unwrap_or(0) >= self.cfg.risk.max_inflight_per_strategy {
            return Err(Reject::TooManyInflight);
        }
        Ok(())
    }

    pub fn begin(&self, s: Strategy) {
        *self.inflight.write().entry(s).or_insert(0) += 1;
    }

    pub fn end(&self, s: Strategy) {
        if let Some(v) = self.inflight.write().get_mut(&s) {
            *v = v.saturating_sub(1);
        }
    }

    pub fn inflight(&self, s: Strategy) -> usize {
        self.inflight.read().get(&s).copied().unwrap_or(0)
    }

    /// Fold a simulation into the running PnL and trip the kill switch if the
    /// drawdown limit is breached.
    pub fn observe(&self, sim: &SimulationResult) {
        let mut cum = self.cumulative_net.write();
        *cum += sim.net_profit_wei;
        let limit = self.cfg.risk.max_drawdown_wei;
        if !limit.is_zero() {
            let limit_i = crate::sim::anvil::to_i128(limit);
            if *cum < -limit_i {
                *self.tripped.write() = true;
                tracing::error!(
                    target: "risk",
                    cumulative_net_wei = *cum,
                    "drawdown kill switch tripped — no new opportunities will be taken"
                );
            }
        }
    }

    pub fn cumulative_net(&self) -> i128 {
        *self.cumulative_net.read()
    }

    pub fn is_tripped(&self) -> bool {
        *self.tripped.read()
    }

    pub fn reset(&self) {
        *self.tripped.write() = false;
        *self.cumulative_net.write() = 0;
    }

    /// Would we have sent this bundle? Simulation-only builds never actually do.
    pub fn submittable(&self, sim: &SimulationResult) -> bool {
        sim.success
            && sim.net_profit_wei > 0
            && U256::from(sim.net_profit_wei.max(0) as u128) >= self.cfg.risk.min_net_profit_wei
            && sim.gas_used <= self.cfg.risk.max_gas_per_bundle
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{now_ms, Call, SimBackend};
    use alloy_primitives::Address;

    fn cfg() -> Arc<Config> {
        // Config::from_env needs ETH_HTTP_URL; build one directly instead.
        Arc::new(Config {
            chain: crate::config::ChainConfig {
                chain_id: 1,
                name: "test".into(),
                weth: crate::config::known::WETH,
                usd_stable: crate::config::known::USDC,
                block_time_ms: 12_000,
            },
            endpoints: crate::config::Endpoints {
                http_url: "http://localhost:8545".into(),
                ws_url: None,
                mev_share_sse: String::new(),
                relay_url: String::new(),
                relay_data_urls: vec![],
                bloxroute_relay_url: String::new(),
                sequencer_feed: None,
                extra_mempool_ws: vec![],
                flashbots_signer_key: None,
                executor: None,
                searcher_address: Address::ZERO,
            },
            risk: crate::config::RiskConfig {
                min_net_profit_wei: U256::from(1u8),
                max_position_wei: U256::from(1_000u64),
                max_base_fee_wei: U256::from(100u64),
                bribe_bps: 9_000,
                max_gas_per_bundle: 1_000_000,
                max_drawdown_wei: U256::from(1_000u64),
                max_inflight_per_strategy: 2,
                max_revert_rate: 1.0,
            },
            strategies: crate::config::StrategyToggles {
                sandwich: true,
                jit: false,
                atomic_arb: true,
                liquidation: true,
                sniper: true,
            },
            sim: crate::config::SimConfig {
                anvil_bin: "anvil".into(),
                anvil_port: 8548,
                anvil_replay_port: 8549,
                replay_fork: false,
                refork_every_blocks: 1,
                use_call_bundle: false,
                target_block_offset: 1,
                timeout: std::time::Duration::from_millis(1_000),
            },
            api: crate::config::ApiConfig {
                bind: "127.0.0.1:0".into(),
                db_path: ":memory:".into(),
                feed_capacity: 10,
            },
            pool_discovery: true,
            pool_discovery_v3: false,
            arb_max_cycle_len: 2,
            relay_tx_ingest: false,
            relay_tx_concurrency: 4,
            live_execution: false,
        })
    }

    fn opp(strategy: Strategy, notional: u64) -> Opportunity {
        Opportunity {
            id: "x".into(),
            strategy,
            victim_hashes: vec![],
            front_calls: vec![Call::new(Address::ZERO, vec![1])],
            back_calls: vec![],
            flash_tokens: vec![],
            flash_amounts: vec![],
            profit_token: Address::ZERO,
            expected_profit_wei: U256::from(1u8),
            notional_wei: U256::from(notional),
            target_block: 1,
            created_at_ms: now_ms(),
            notes: String::new(),
        }
    }

    fn sim(net: i128, gas: u64) -> SimulationResult {
        SimulationResult {
            opportunity_id: "x".into(),
            strategy: Strategy::Sandwich,
            backend: SimBackend::AnvilFork,
            success: net > 0,
            gross_profit_wei: U256::from(10u8),
            gas_used: gas,
            gas_price_wei: U256::ZERO,
            gas_cost_wei: U256::ZERO,
            bribe_wei: U256::ZERO,
            net_profit_wei: net,
            revert_reason: None,
            target_block: 1,
            sim_latency_ms: 1,
            created_at_ms: now_ms(),
        }
    }

    #[test]
    fn rejects_disabled_strategies() {
        let r = RiskEngine::new(cfg());
        assert_eq!(r.check(&opp(Strategy::Jit, 10), U256::ZERO), Err(Reject::Disabled));
        assert!(r.check(&opp(Strategy::Sandwich, 10), U256::ZERO).is_ok());
    }

    #[test]
    fn rejects_oversized_and_expensive() {
        let r = RiskEngine::new(cfg());
        assert_eq!(
            r.check(&opp(Strategy::Sandwich, 10_000), U256::ZERO),
            Err(Reject::TooLarge)
        );
        assert_eq!(
            r.check(&opp(Strategy::Sandwich, 10), U256::from(1_000u64)),
            Err(Reject::BaseFeeTooHigh)
        );
    }

    #[test]
    fn caps_inflight_per_strategy() {
        let r = RiskEngine::new(cfg());
        r.begin(Strategy::Sandwich);
        r.begin(Strategy::Sandwich);
        assert_eq!(
            r.check(&opp(Strategy::Sandwich, 10), U256::ZERO),
            Err(Reject::TooManyInflight)
        );
        r.end(Strategy::Sandwich);
        assert!(r.check(&opp(Strategy::Sandwich, 10), U256::ZERO).is_ok());
    }

    #[test]
    fn kill_switch_trips_on_drawdown() {
        let r = RiskEngine::new(cfg());
        r.observe(&sim(-600, 1));
        assert!(!r.is_tripped());
        r.observe(&sim(-600, 1));
        assert!(r.is_tripped());
        assert_eq!(r.check(&opp(Strategy::Sandwich, 10), U256::ZERO), Err(Reject::KillSwitch));
        r.reset();
        assert!(r.check(&opp(Strategy::Sandwich, 10), U256::ZERO).is_ok());
    }

    #[test]
    fn only_net_positive_bundles_are_submittable() {
        let r = RiskEngine::new(cfg());
        assert!(r.submittable(&sim(5, 100)));
        assert!(!r.submittable(&sim(0, 100)));
        assert!(!r.submittable(&sim(-5, 100)));
        // over the gas cap
        assert!(!r.submittable(&sim(5, 10_000_000)));
    }
}
