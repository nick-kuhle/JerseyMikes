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
use serde::Deserialize;

use crate::config::{Config, RiskConfig, StrategyToggles};
use crate::types::{Opportunity, SimulationResult, Strategy};

/// The runtime-adjustable slice of the risk envelope.
///
/// Boot values come from the environment (`Config::from_env`); the dashboard
/// can change them while the bot runs via `POST /api/risk` — the console's
/// risk panel is instant-apply, no restart. Two boundaries are deliberate:
///
/// - **Strategy toggles can only narrow.** A strategy that was not
///   constructed at boot (its env toggle was off) cannot be switched on at
///   runtime — the engine never built it, so "enabling" it would silently do
///   nothing. Same shape as the live-mode switch: runtime can only restrict
///   what the environment allowed.
/// - **Risk *limits* can loosen at runtime.** Tightening or loosening caps
///   is an explicit operator decision made visible on the dashboard; the
///   one-way protections (live arming, broadcasting) live elsewhere.
#[derive(Clone)]
pub struct RuntimeRisk {
    risk: std::sync::Arc<RwLock<RiskConfig>>,
    strategies: std::sync::Arc<RwLock<StrategyToggles>>,
    /// Boot toggles — the ceiling a runtime toggle cannot exceed.
    boot: StrategyToggles,
}

/// A partial risk update from the dashboard. Every field is optional; only
/// present fields are applied. Wei values arrive as decimal strings (they
/// routinely exceed JS safe integers).
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RiskPatch {
    pub min_net_profit_wei: Option<String>,
    pub max_position_wei: Option<String>,
    pub max_base_fee_wei: Option<String>,
    pub max_drawdown_wei: Option<String>,
    pub bribe_bps: Option<u16>,
    pub max_gas_per_bundle: Option<u64>,
    pub max_inflight_per_strategy: Option<usize>,
    /// `{"sandwich": false, "liquidation_maker": true, ...}` — partial map.
    pub strategies: Option<std::collections::HashMap<String, bool>>,
}

fn parse_wei(label: &str, raw: &str) -> Result<U256, String> {
    raw.trim()
        .parse::<U256>()
        .map_err(|_| format!("{label}: \"{raw}\" is not a non-negative decimal wei amount"))
}

impl RuntimeRisk {
    pub fn new(risk: RiskConfig, boot: StrategyToggles) -> Self {
        Self {
            risk: std::sync::Arc::new(RwLock::new(risk)),
            strategies: std::sync::Arc::new(RwLock::new(boot.clone())),
            boot,
        }
    }

    /// Current effective risk configuration (snapshot copy).
    pub fn risk(&self) -> RiskConfig {
        self.risk.read().clone()
    }

    /// Current effective strategy toggles (already intersected with boot).
    pub fn strategies(&self) -> StrategyToggles {
        self.strategies.read().clone()
    }

    /// Boot-time strategy toggles — the ceiling for runtime toggles.
    pub fn boot_strategies(&self) -> &StrategyToggles {
        &self.boot
    }

    /// Effective enablement of one strategy: runtime toggle **and** boot
    /// construction. Strategies not built at boot report `false` here.
    pub fn enabled(&self, s: Strategy) -> bool {
        let t = self.strategies.read();
        let b = &self.boot;
        let (rt, bt) = match s {
            Strategy::Sandwich => (t.sandwich, b.sandwich),
            Strategy::SandwichV3 => (t.sandwich_v3, b.sandwich_v3),
            Strategy::Jit => (t.jit, b.jit),
            Strategy::AtomicArb => (t.atomic_arb, b.atomic_arb),
            Strategy::Liquidation => (t.liquidation, b.liquidation),
            Strategy::LiquidationCompound => (t.liquidation_compound, b.liquidation_compound),
            Strategy::LiquidationMorpho => (t.liquidation_morpho, b.liquidation_morpho),
            Strategy::LiquidationMaker => (t.liquidation_maker, b.liquidation_maker),
            Strategy::OracleFrontrun => (t.oracle_frontrun, b.oracle_frontrun),
            Strategy::Sniper => (t.sniper, b.sniper),
        };
        rt && bt
    }

    /// Names of strategies effectively enabled right now.
    pub fn enabled_names(&self) -> Vec<&'static str> {
        Strategy::all()
            .iter()
            .filter(|s| self.enabled(**s))
            .map(|s| s.as_str())
            .collect()
    }

    /// Validate and apply a patch. On success every listed field is applied
    /// atomically; on failure nothing changes and the reason comes back as a
    /// human-readable string (surfaced 400 by the API).
    pub fn apply(&self, patch: RiskPatch) -> Result<(), String> {
        // Validate everything first, then write — a rejected patch must not
        // leave half of it applied.
        let mut risk = self.risk.read().clone();
        if let Some(v) = &patch.min_net_profit_wei {
            risk.min_net_profit_wei = parse_wei("minNetProfitWei", v)?;
        }
        if let Some(v) = &patch.max_position_wei {
            risk.max_position_wei = parse_wei("maxPositionWei", v)?;
            if risk.max_position_wei.is_zero() {
                return Err("maxPositionWei must be > 0".to_string());
            }
        }
        if let Some(v) = &patch.max_base_fee_wei {
            risk.max_base_fee_wei = parse_wei("maxBaseFeeWei", v)?;
            if risk.max_base_fee_wei.is_zero() {
                return Err("maxBaseFeeWei must be > 0".to_string());
            }
        }
        if let Some(v) = &patch.max_drawdown_wei {
            risk.max_drawdown_wei = parse_wei("maxDrawdownWei", v)?; // 0 == disabled, by design
        }
        if let Some(v) = patch.bribe_bps {
            if v > 10_000 {
                return Err(format!("bribeBps {v} exceeds 10000 (100% of gross)"));
            }
            risk.bribe_bps = v;
        }
        if let Some(v) = patch.max_gas_per_bundle {
            if !(21_000..=crate::sim::anvil::MAX_TX_GAS_CEILING).contains(&v) {
                return Err(format!(
                    "maxGasPerBundle {v} outside [21000, {}]",
                    crate::sim::anvil::MAX_TX_GAS_CEILING
                ));
            }
            risk.max_gas_per_bundle = v;
        }
        if let Some(v) = patch.max_inflight_per_strategy {
            if !(1..=256).contains(&v) {
                return Err(format!("maxInflightPerStrategy {v} outside [1, 256]"));
            }
            risk.max_inflight_per_strategy = v;
        }
        let mut strategies = self.strategies.read().clone();
        if let Some(wanted) = &patch.strategies {
            for (name, on) in wanted {
                let all = Strategy::all();
                let Some(strategy) = all.iter().find(|s| s.as_str() == name.as_str()) else {
                    return Err(format!("strategies: unknown strategy \"{name}\""));
                };
                if *on && !self.enabled_at_boot(*strategy) {
                    return Err(format!(
                        "strategies: \"{name}\" was not constructed at boot (its env toggle was off) — set STRATEGY_{}=true and restart to make it available",
                        name.to_ascii_uppercase()
                    ));
                }
                match strategy {
                    Strategy::Sandwich => strategies.sandwich = *on,
                    Strategy::SandwichV3 => strategies.sandwich_v3 = *on,
                    Strategy::Jit => strategies.jit = *on,
                    Strategy::AtomicArb => strategies.atomic_arb = *on,
                    Strategy::Liquidation => strategies.liquidation = *on,
                    Strategy::LiquidationCompound => strategies.liquidation_compound = *on,
                    Strategy::LiquidationMorpho => strategies.liquidation_morpho = *on,
                    Strategy::LiquidationMaker => strategies.liquidation_maker = *on,
                    Strategy::OracleFrontrun => strategies.oracle_frontrun = *on,
                    Strategy::Sniper => strategies.sniper = *on,
                }
            }
        }
        *self.risk.write() = risk;
        *self.strategies.write() = strategies;
        Ok(())
    }

    fn enabled_at_boot(&self, s: Strategy) -> bool {
        let b = &self.boot;
        match s {
            Strategy::Sandwich => b.sandwich,
            Strategy::SandwichV3 => b.sandwich_v3,
            Strategy::Jit => b.jit,
            Strategy::AtomicArb => b.atomic_arb,
            Strategy::Liquidation => b.liquidation,
            Strategy::LiquidationCompound => b.liquidation_compound,
            Strategy::LiquidationMorpho => b.liquidation_morpho,
            Strategy::LiquidationMaker => b.liquidation_maker,
            Strategy::OracleFrontrun => b.oracle_frontrun,
            Strategy::Sniper => b.sniper,
        }
    }
}

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
    /// Kept for fields that are not runtime-adjustable.
    #[allow(dead_code)]
    cfg: Arc<Config>,
    /// The live risk envelope — shared with the simulator and the API, so a
    /// dashboard change gates the very next opportunity and prices the very
    /// next bundle's guards with it.
    runtime: RuntimeRisk,
    inflight: RwLock<HashMap<Strategy, usize>>,
    /// Running simulated PnL in wei; drives the drawdown kill switch.
    cumulative_net: RwLock<i128>,
    tripped: RwLock<bool>,
    /// Optional SQLite handle. Tests leave this `None` so they stay hermetic;
    /// production wires it so a trip survives `systemctl restart`.
    store: Option<Arc<crate::store::Store>>,
}

impl RiskEngine {
    pub fn new(cfg: Arc<Config>, runtime: RuntimeRisk) -> Self {
        Self {
            cfg,
            runtime,
            inflight: RwLock::new(HashMap::new()),
            cumulative_net: RwLock::new(0),
            tripped: RwLock::new(false),
            store: None,
        }
    }

    /// Attach the durable store. Must be called before [`Self::restore`] so a
    /// subsequent trip or reset writes through.
    pub fn with_store(mut self, store: Arc<crate::store::Store>) -> Self {
        self.store = Some(store);
        self
    }

    /// Re-apply a snapshot loaded from SQLite at boot. A previously tripped
    /// process comes back tripped; `POST /api/risk/reset` is the only re-arm.
    pub fn restore(&self, state: &crate::store::PersistedRiskState) {
        *self.tripped.write() = state.tripped;
        *self.cumulative_net.write() = state.cumulative_net_wei;
        if state.tripped {
            tracing::error!(
                target: "risk",
                cumulative_net_wei = state.cumulative_net_wei,
                tripped_at_ms = ?state.tripped_at_ms,
                "restored durable drawdown kill switch from SQLite — POST /api/risk/reset to re-arm"
            );
        }
    }

    fn persist(&self) {
        let Some(store) = &self.store else {
            return;
        };
        if let Err(error) = store.persist_kill_switch(self.is_tripped(), self.cumulative_net()) {
            tracing::error!(target: "risk", %error, "could not persist kill-switch state");
        }
    }

    pub fn runtime(&self) -> &RuntimeRisk {
        &self.runtime
    }

    pub fn enabled(&self, s: Strategy) -> bool {
        self.runtime.enabled(s)
    }

    /// Gate an opportunity before it costs us a simulation slot.
    pub fn check(&self, opp: &Opportunity, base_fee: U256) -> Result<(), Reject> {
        let risk = self.runtime.risk();
        if !self.enabled(opp.strategy) {
            return Err(Reject::Disabled);
        }
        if *self.tripped.read() {
            return Err(Reject::KillSwitch);
        }
        if opp.front_calls.is_empty() && opp.back_calls.is_empty() {
            return Err(Reject::NoCalls);
        }
        if opp.notional_wei > risk.max_position_wei {
            return Err(Reject::TooLarge);
        }
        if base_fee > risk.max_base_fee_wei {
            return Err(Reject::BaseFeeTooHigh);
        }
        let inflight = self.inflight.read();
        if inflight.get(&opp.strategy).copied().unwrap_or(0) >= risk.max_inflight_per_strategy {
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
        let mut newly_tripped = false;
        {
            let mut cum = self.cumulative_net.write();
            *cum += sim.net_profit_wei;
            let limit = self.runtime.risk().max_drawdown_wei;
            if !limit.is_zero() {
                let limit_i = crate::sim::anvil::to_i128(limit);
                if *cum < -limit_i {
                    let mut tripped = self.tripped.write();
                    if !*tripped {
                        *tripped = true;
                        newly_tripped = true;
                        tracing::error!(
                            target: "risk",
                            cumulative_net_wei = *cum,
                            "drawdown kill switch tripped — no new opportunities will be taken"
                        );
                    }
                }
            }
        }
        if newly_tripped {
            self.persist();
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
        self.persist();
    }

    /// Would we have sent this bundle? Simulation-only builds never actually do.
    pub fn submittable(&self, sim: &SimulationResult) -> bool {
        let risk = self.runtime.risk();
        sim.strategy.live_candidate()
            && sim.success
            && sim.net_profit_wei > 0
            && U256::from(sim.net_profit_wei.max(0) as u128) >= risk.min_net_profit_wei
            && sim.gas_used <= risk.max_gas_per_bundle
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
                bundle_relay_urls: vec![],
                relay_data_urls: vec![],
                bloxroute_relay_url: String::new(),
                sequencer_feed: None,
                extra_mempool_ws: vec![],
                mev_blocker_ws: None,
                flashbots_signer_key: None,
                searcher_private_key: None,
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
                sandwich_v3: false,
                jit: false,
                atomic_arb: true,
                liquidation: true,
                liquidation_compound: false,
                liquidation_morpho: false,
                liquidation_maker: false,
                oracle_frontrun: false,
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
            liquidation: crate::config::LiquidationConfig {
                watch_cap: 8,
                morpho_market_cap: 4,
                morpho_borrower_cap: 4,
                maker_ilks: vec!["ETH-A".to_string()],
            },
            oracle: crate::config::OracleConfig {
                watch_feeds: vec![],
                max_leads: 3,
            },
            alerts: crate::config::AlertsConfig::default(),
            api: crate::config::ApiConfig {
                bind: "127.0.0.1:0".into(),
                db_path: ":memory:".into(),
                feed_capacity: 10,
                write_queue_capacity: 1_024,
                auth_token: None,
                allowed_origins: vec![],
            },
            pool_discovery: true,
            pool_discovery_v3: false,
            decode_universal_router: false,
            arb_max_cycle_len: 2,
            relay_tx_ingest: false,
            relay_tx_concurrency: 4,
            strategy_concurrency: 64,
            replay_lanes: 1,
            replay_queue_depth: 4,
            pool_discovery_interval_blocks: 1,
            inventory_refresh_blocks: 1,
            arb_enumeration_budget: std::time::Duration::from_millis(25),
            arb_max_pools: 200,
            inventory_gate: false,
            live_execution: false,
            broadcast_enabled: false,
            qualification_hours: 168,
            qualification_min_samples: 30,
            qualification_min_relay_comparisons: 30,
            qualification_min_actual_matches: 30,
            qualification_max_error_bps: 2_000,
            qualification_min_accuracy_bps: 8_000,
            qualification_max_gap_secs: 120,
            finality_depth: 12,
            submission_retry_ms: 250,
            submission_max_attempts: 2,
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
        let c = cfg();
        let rt = crate::risk::RuntimeRisk::new(c.risk.clone(), c.strategies.clone());
        let r = RiskEngine::new(c, rt.clone());
        assert_eq!(
            r.check(&opp(Strategy::Jit, 10), U256::ZERO),
            Err(Reject::Disabled)
        );
        assert_eq!(
            r.check(&opp(Strategy::SandwichV3, 10), U256::ZERO),
            Err(Reject::Disabled),
            "sandwich_v3 stays off unless the operator flips the toggle"
        );
        assert!(r.check(&opp(Strategy::Sandwich, 10), U256::ZERO).is_ok());
    }

    #[test]
    fn rejects_oversized_and_expensive() {
        let c = cfg();
        let rt = crate::risk::RuntimeRisk::new(c.risk.clone(), c.strategies.clone());
        let r = RiskEngine::new(c, rt.clone());
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
        let c = cfg();
        let rt = crate::risk::RuntimeRisk::new(c.risk.clone(), c.strategies.clone());
        let r = RiskEngine::new(c, rt.clone());
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
        let c = cfg();
        let rt = crate::risk::RuntimeRisk::new(c.risk.clone(), c.strategies.clone());
        let r = RiskEngine::new(c, rt.clone());
        r.observe(&sim(-600, 1));
        assert!(!r.is_tripped());
        r.observe(&sim(-600, 1));
        assert!(r.is_tripped());
        assert_eq!(
            r.check(&opp(Strategy::Sandwich, 10), U256::ZERO),
            Err(Reject::KillSwitch)
        );
        r.reset();
        assert!(r.check(&opp(Strategy::Sandwich, 10), U256::ZERO).is_ok());
    }

    #[test]
    fn a_tripped_kill_switch_survives_a_new_engine() {
        // The defect: trip lived only in process memory, so `systemctl restart`
        // after a drawdown silently re-armed the bot. Persist + restore must
        // bring the next process up already tripped; reset is the only re-arm.
        let store = std::sync::Arc::new(crate::store::Store::open_in_memory().unwrap());
        let c = cfg();
        let rt = crate::risk::RuntimeRisk::new(c.risk.clone(), c.strategies.clone());
        let first = RiskEngine::new(c.clone(), rt.clone()).with_store(store.clone());
        first.observe(&sim(-600, 1));
        first.observe(&sim(-600, 1));
        assert!(first.is_tripped());

        let restored = RiskEngine::new(c.clone(), rt.clone()).with_store(store.clone());
        restored.restore(&store.load_risk_state().unwrap());
        assert!(restored.is_tripped());
        assert_eq!(restored.cumulative_net(), first.cumulative_net());
        assert_eq!(
            restored.check(&opp(Strategy::Sandwich, 10), U256::ZERO),
            Err(Reject::KillSwitch)
        );

        restored.reset();
        assert!(!restored.is_tripped());
        let after_reset = RiskEngine::new(c, rt).with_store(store.clone());
        after_reset.restore(&store.load_risk_state().unwrap());
        assert!(!after_reset.is_tripped());
        assert_eq!(after_reset.cumulative_net(), 0);
        assert!(after_reset
            .check(&opp(Strategy::Sandwich, 10), U256::ZERO)
            .is_ok());
    }

    #[test]
    fn only_net_positive_bundles_are_submittable() {
        let c = cfg();
        let rt = crate::risk::RuntimeRisk::new(c.risk.clone(), c.strategies.clone());
        let r = RiskEngine::new(c, rt.clone());
        assert!(r.submittable(&sim(5, 100)));
        assert!(!r.submittable(&sim(0, 100)));
        assert!(!r.submittable(&sim(-5, 100)));
        // over the gas cap
        assert!(!r.submittable(&sim(5, 10_000_000)));
    }

    fn runtime() -> crate::risk::RuntimeRisk {
        let c = cfg();
        // Boot: everything on except jit and sniper (to exercise narrowing).
        let mut toggles = c.strategies.clone();
        toggles.jit = false;
        toggles.sniper = false;
        crate::risk::RuntimeRisk::new(c.risk.clone(), toggles)
    }

    #[test]
    fn runtime_patch_applies_and_is_read_immediately() {
        let rt = runtime();
        let patch = crate::risk::RiskPatch {
            min_net_profit_wei: Some("123".to_string()),
            bribe_bps: Some(5_000),
            max_inflight_per_strategy: Some(2),
            ..Default::default()
        };
        rt.apply(patch).expect("valid patch");
        let r = rt.risk();
        assert_eq!(r.min_net_profit_wei.to_string(), "123");
        assert_eq!(r.bribe_bps, 5_000);
        assert_eq!(r.max_inflight_per_strategy, 2);
        // Untouched fields keep boot values.
        assert_eq!(r.max_gas_per_bundle, 1_000_000);
    }

    #[test]
    fn runtime_patch_rejects_out_of_range_values_atomically() {
        let rt = runtime();
        let before = rt.risk();
        let patch = crate::risk::RiskPatch {
            bribe_bps: Some(10_001),
            ..Default::default()
        }; // > 100%
        assert!(rt.apply(patch).is_err());
        // A valid field in the same patch shape must NOT have been applied.
        let patch = crate::risk::RiskPatch {
            min_net_profit_wei: Some("7".to_string()),
            bribe_bps: Some(10_001),
            ..Default::default()
        };
        assert!(rt.apply(patch).is_err());
        let after = rt.risk();
        assert_eq!(after.bribe_bps, before.bribe_bps);
        assert_eq!(after.min_net_profit_wei, before.min_net_profit_wei);
        // Gas cap is bounded by the same ceiling the simulator clamps to.
        let bad_gas = crate::risk::RiskPatch {
            max_gas_per_bundle: Some(10_000),
            ..Default::default()
        };
        assert!(rt.apply(bad_gas).is_err());
    }

    #[test]
    fn runtime_patch_rejects_non_numeric_wei() {
        let rt = runtime();
        let patch = crate::risk::RiskPatch {
            max_position_wei: Some("0.1 ETH".to_string()),
            ..Default::default()
        };
        let err = rt.apply(patch).unwrap_err();
        assert!(err.contains("maxPositionWei"), "{err}");
    }

    #[test]
    fn runtime_strategy_toggles_can_only_narrow() {
        let rt = runtime(); // jit + sniper off at boot
        let mut off = std::collections::HashMap::new();
        off.insert("sandwich".to_string(), false);
        rt.apply(crate::risk::RiskPatch {
            strategies: Some(off),
            ..Default::default()
        })
        .expect("disabling a boot-enabled strategy is fine");
        assert!(!rt.enabled(Strategy::Sandwich));

        let mut on = std::collections::HashMap::new();
        on.insert("jit".to_string(), true);
        let err = rt
            .apply(crate::risk::RiskPatch {
                strategies: Some(on),
                ..Default::default()
            })
            .unwrap_err();
        assert!(err.contains("not constructed at boot"), "{err}");
        // And the failed patch changed nothing.
        assert!(!rt.enabled(Strategy::Jit));

        let mut unknown = std::collections::HashMap::new();
        unknown.insert("frontrun_v2".to_string(), true);
        assert!(rt
            .apply(crate::risk::RiskPatch {
                strategies: Some(unknown),
                ..Default::default()
            })
            .is_err());

        // Re-enabling a boot-enabled strategy that was disabled at runtime
        // is allowed — runtime may always return to the boot set.
        let mut re_on = std::collections::HashMap::new();
        re_on.insert("sandwich".to_string(), true);
        rt.apply(crate::risk::RiskPatch {
            strategies: Some(re_on),
            ..Default::default()
        })
        .expect("re-enabling within the boot set is fine");
        assert!(rt.enabled(Strategy::Sandwich));
    }

    #[test]
    fn risk_engine_gates_with_runtime_values() {
        let rt = runtime();
        let c = cfg();
        let r = RiskEngine::new(c, rt.clone());
        // Boot min profit is 1 wei, so this passes...
        assert!(r.check(&opp(Strategy::Sandwich, 10), U256::ZERO).is_ok());
        // ...until the runtime floor is raised above it.
        let patch = crate::risk::RiskPatch {
            min_net_profit_wei: Some("1000".to_string()),
            ..Default::default()
        };
        rt.apply(patch).unwrap();
        // check() gates notional/base-fee/inflight, not profit (the sim
        // measures that); submittable() is where the floor bites.
        let sim = crate::types::SimulationResult {
            opportunity_id: "x".into(),
            strategy: Strategy::Sandwich,
            backend: crate::types::SimBackend::AnvilFork,
            success: true,
            gross_profit_wei: U256::from(100u64),
            gas_used: 100,
            gas_price_wei: U256::ONE,
            gas_cost_wei: U256::from(100u64),
            bribe_wei: U256::ZERO,
            net_profit_wei: 0,
            revert_reason: None,
            target_block: 1,
            sim_latency_ms: 1,
            created_at_ms: 0,
        };
        assert!(!r.submittable(&sim));
        // And a disabled-at-runtime strategy is rejected on the spot.
        let mut off = std::collections::HashMap::new();
        off.insert("sandwich".to_string(), false);
        rt.apply(crate::risk::RiskPatch {
            strategies: Some(off),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(
            r.check(&opp(Strategy::Sandwich, 10), U256::ZERO),
            Err(Reject::Disabled)
        );
    }
}
