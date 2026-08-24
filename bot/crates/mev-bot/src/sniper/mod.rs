//! The directional new-token sniper lane.
//!
//! # Why this is a separate lane
//!
//! Every other strategy in this repository is *atomic*: one bundle, one block,
//! profit-or-revert enforced on chain by `MevExecutor`. That invariant is what
//! makes a losing bundle free — it simply never lands.
//!
//! A directional sniper cannot have that property. Buying a token is a pure
//! spend; the position is held across blocks and can go to zero. So the lane
//! is isolated at every level rather than bolted onto the shared machinery:
//!
//! | Concern | Shared engine | Sniper lane |
//! | --- | --- | --- |
//! | Contract | `MevExecutor` (profit-or-revert) | `SniperVault` (budget-capped spend) |
//! | Risk envelope | `RiskConfig` | [`params::SniperParams`] |
//! | Arming | `LIVE_EXECUTION` + qualification `PASS` | `SNIPER_DIRECTIONAL` + budget + its own gates |
//! | Worst case | reverted bundle, gas only | the entire buy |
//! | Accounting | per-bundle net profit | open positions marked to market |
//! | Kill switch | engine drawdown | [`SniperLane::halt`], independent |
//!
//! Deleting this directory and its three call sites removes the lane whole,
//! leaving the certified atomic path byte-for-byte unchanged. That property is
//! deliberate and worth preserving.
//!
//! # Flow
//!
//! ```text
//!   PairCreated / addLiquidityETH in mempool
//!            │
//!            ▼
//!   honeypot round-trip probe (atomic buy→sell on a fork)
//!            │  verdict
//!            ▼
//!   gates::admit  ── rejected ──▶ recorded with a reason, counted in the funnel
//!            │ approved (size)
//!            ▼
//!   backrun buy  ──▶ Position { Open }
//!            │
//!            ▼  every block
//!   mark to market ──▶ position::evaluate_exit ──▶ sell x% / all
//!            │
//!            ▼
//!   Closed, PnL booked, portfolio updated
//! ```

pub mod calldata;
pub mod execution;
pub mod gates;
pub mod marks;
pub mod params;
pub mod portfolio;
pub mod position;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use alloy_primitives::{Address, U256};
use parking_lot::RwLock;

/// Initial paper-trading bankroll. It is deliberately separate from every
/// on-chain balance and is never used by a live signer.
pub const INITIAL_PAPER_BALANCE_WEI: U256 = U256::from_limbs([1_000_000_000_000_000_000, 0, 0, 0]);

pub use gates::{Admission, ExposureSnapshot, HoneypotVerdict, LaunchCandidate, Rejection};
pub use params::{SniperParams, SniperParamsPatch};
pub use portfolio::{Mark, Portfolio, PortfolioRow, PortfolioTotals};
pub use position::{ExitDecision, ExitReason, Position, PositionState};

/// Runtime state for the sniper lane.
///
/// Cheap to clone (everything behind an `Arc`), so the engine, the exit
/// monitor and the API can all hold one.
#[derive(Clone)]
pub struct SniperLane {
    params: Arc<RwLock<SniperParams>>,
    /// Paper bankroll used only by the simulation lane.
    paper_balance_wei: Arc<RwLock<U256>>,
    /// Boot envelope — a runtime patch may not exceed the boot arming state.
    /// Mirrors how `RuntimeRisk` treats strategy toggles: runtime can only
    /// *narrow* what the environment allowed. A bot booted with
    /// `SNIPER_DIRECTIONAL=false` cannot be armed from the dashboard.
    boot_enabled: bool,
    /// Simulation is an explicit non-live mode, so its paper lane may be
    /// enabled at runtime without a deployed vault or a live signer.
    paper_mode: bool,
    positions: Arc<RwLock<HashMap<String, Position>>>,
    marks: Arc<RwLock<HashMap<String, Mark>>>,
    symbols: Arc<RwLock<HashMap<Address, String>>>,
    blacklist: Arc<RwLock<HashSet<Address>>>,
    /// Tokens already handled, so one launch is not sniped twice.
    seen_tokens: Arc<RwLock<HashSet<Address>>>,
    halted: Arc<RwLock<Option<String>>>,
    rejections: Arc<RwLock<HashMap<&'static str, u64>>>,
}

impl SniperLane {
    pub fn new(params: SniperParams) -> Self {
        Self::new_with_mode(params, false)
    }

    pub fn new_with_mode(params: SniperParams, paper_mode: bool) -> Self {
        let boot_enabled = params.enabled;
        Self {
            params: Arc::new(RwLock::new(params)),
            paper_balance_wei: Arc::new(RwLock::new(INITIAL_PAPER_BALANCE_WEI)),
            boot_enabled,
            paper_mode,
            positions: Arc::new(RwLock::new(HashMap::new())),
            marks: Arc::new(RwLock::new(HashMap::new())),
            symbols: Arc::new(RwLock::new(HashMap::new())),
            blacklist: Arc::new(RwLock::new(HashSet::new())),
            seen_tokens: Arc::new(RwLock::new(HashSet::new())),
            halted: Arc::new(RwLock::new(None)),
            rejections: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub fn from_env() -> Self {
        Self::from_env_with_mode(false)
    }

    pub fn from_env_with_mode(paper_mode: bool) -> Self {
        Self::new_with_mode(SniperParams::from_env(), paper_mode)
    }

    pub fn paper_mode(&self) -> bool {
        self.paper_mode
    }

    pub fn paper_ready(&self) -> bool {
        let params = self.params();
        self.paper_mode
            && !self.is_halted()
            && params.enabled
            && !params.buy_size_wei.is_zero()
            && !params.daily_budget_wei.is_zero()
            && params.max_concurrent_positions > 0
    }

    pub fn effective_armed(&self) -> bool {
        if self.paper_mode {
            self.paper_ready()
        } else {
            let params = self.params();
            params.is_armed() && !self.is_halted() && self.boot_enabled
        }
    }

    pub fn params(&self) -> SniperParams {
        self.params.read().clone()
    }

    pub fn boot_enabled(&self) -> bool {
        self.boot_enabled
    }

    pub fn paper_balance_wei(&self) -> U256 {
        *self.paper_balance_wei.read()
    }

    /// Atomically reserve paper funds for a simulated entry.
    pub fn reserve_paper(&self, amount: U256) -> bool {
        let mut balance = self.paper_balance_wei.write();
        if *balance < amount {
            return false;
        }
        *balance -= amount;
        true
    }

    pub fn credit_paper(&self, amount: U256) {
        let mut balance = self.paper_balance_wei.write();
        *balance = balance.saturating_add(amount);
    }

    pub fn reset_paper(&self) {
        *self.paper_balance_wei.write() = INITIAL_PAPER_BALANCE_WEI;
    }

    /// Paper mode can be enabled with a non-zero size/budget but no deployed
    /// vault. A live lane still requires a vault and boot-enabled signer.
    pub fn admit_paper(
        &self,
        candidate: &LaunchCandidate,
        now_ms: u64,
    ) -> Result<Admission, Rejection> {
        let mut params = self.params.read().clone();
        params.vault_address = Some(Address::repeat_byte(1));
        let exposure = self.exposure(now_ms);
        let result = gates::admit(&params, candidate, &exposure);
        if let Err(rejection) = &result {
            self.record_rejection(rejection.code());
        }
        result
    }

    /// Apply a runtime patch. Rejects any attempt to arm a lane that was not
    /// armed at boot, and validates the resulting envelope as a whole.
    pub fn patch_params(&self, patch: &SniperParamsPatch) -> Result<SniperParams, Vec<String>> {
        let current = self.params.read().clone();
        let next = current.with_patch(patch)?;
        // A simulation-only envelope may be enabled without a vault so the
        // paper balance can be exercised. A patch that would enable a funded
        // live envelope still cannot widen a false boot ceiling.
        let live_intent = next.enabled
            && next.vault_address.is_some()
            && !next.buy_size_wei.is_zero()
            && !next.daily_budget_wei.is_zero();
        if live_intent && !self.boot_enabled && !self.paper_mode {
            return Err(vec![
                "sniper: SNIPER_DIRECTIONAL was false at boot — the live lane cannot be armed at \
                 runtime. Set SNIPER_DIRECTIONAL=true and restart."
                    .to_string(),
            ]);
        }
        *self.params.write() = next.clone();
        Ok(next)
    }

    /// Stop opening positions. Existing positions keep being managed — exiting
    /// is always allowed, because trapping the lane in a position is worse
    /// than any reason to halt it.
    pub fn halt(&self, reason: impl Into<String>) {
        *self.halted.write() = Some(reason.into());
    }

    pub fn resume(&self) {
        *self.halted.write() = None;
    }

    pub fn halt_reason(&self) -> Option<String> {
        self.halted.read().clone()
    }

    pub fn is_halted(&self) -> bool {
        self.halted.read().is_some()
    }

    pub fn blacklist(&self, token: Address) {
        self.blacklist.write().insert(token);
    }

    pub fn is_blacklisted(&self, token: Address) -> bool {
        self.blacklist.read().contains(&token)
    }

    /// Manual sell decision from console. Actual submission lives in
    /// `SniperExecution`, so this lane remains a state/risk owner rather than
    /// growing a second transaction path.
    pub fn manual_sell(&self, id: &str, sell_fraction_bps: u32) -> Option<ExitDecision> {
        let mut guard = self.positions.write();
        let p = guard.get_mut(id)?;
        if !p.state.is_live() || p.remaining_qty.is_zero() {
            return None;
        }
        let bps_clamped = sell_fraction_bps.min(10_000);
        let qty = p.remaining_qty * U256::from(bps_clamped) / U256::from(10_000u64);
        let decision = ExitDecision {
            reason: ExitReason::Manual,
            qty,
            fraction_bps: bps_clamped,
            closes_position: bps_clamped >= 10_000 || qty >= p.remaining_qty,
        };
        Some(decision)
    }

    /// Claim a token for evaluation. Returns false when it has already been
    /// seen, so a launch observed on both the log scan and the mempool path is
    /// only ever acted on once.
    pub fn claim_token(&self, token: Address) -> bool {
        self.seen_tokens.write().insert(token)
    }

    /// Explicit operator action may retry a token that was only observed in
    /// shadow mode. Admission and the existing-position/concurrency gates still
    /// run; this only releases the de-duplication marker for the manual path.
    pub fn release_token_claim(&self, token: Address) {
        self.seen_tokens.write().remove(&token);
    }

    pub fn set_symbol(&self, token: Address, symbol: String) {
        self.symbols.write().insert(token, symbol);
    }

    pub fn positions(&self) -> Vec<Position> {
        self.positions.read().values().cloned().collect()
    }

    pub fn position(&self, id: &str) -> Option<Position> {
        self.positions.read().get(id).cloned()
    }

    pub fn live_positions(&self) -> Vec<Position> {
        self.positions
            .read()
            .values()
            .filter(|p| p.state.is_live())
            .cloned()
            .collect()
    }

    pub fn upsert_position(&self, p: Position) {
        self.positions.write().insert(p.id.clone(), p);
    }

    /// Load positions recovered from SQLite at boot. A restart must not lose
    /// track of open exposure — an unmanaged open position is the worst
    /// failure mode this lane has.
    pub fn hydrate(&self, positions: Vec<Position>) {
        let mut guard = self.positions.write();
        let mut seen = self.seen_tokens.write();
        for p in positions {
            seen.insert(p.token);
            guard.insert(p.id.clone(), p);
        }
    }

    pub fn set_mark(&self, id: &str, mark: Mark) {
        self.marks.write().insert(id.to_string(), mark);
        if let Some(p) = self.positions.write().get_mut(id) {
            p.mark(mark.value_wei);
        }
    }

    pub fn marks(&self) -> HashMap<String, Mark> {
        self.marks.read().clone()
    }

    fn record_rejection(&self, code: &'static str) {
        *self.rejections.write().entry(code).or_insert(0) += 1;
    }

    /// Rejection counters, for the funnel panel.
    pub fn rejection_counts(&self) -> HashMap<String, u64> {
        self.rejections
            .read()
            .iter()
            .map(|(k, v)| (k.to_string(), *v))
            .collect()
    }

    /// Current exposure, computed from live positions.
    pub fn exposure(&self, now_ms: u64) -> ExposureSnapshot {
        let positions = self.positions.read();
        let mut e = ExposureSnapshot {
            halted: self.halted.read().is_some(),
            ..Default::default()
        };
        for p in positions.values() {
            if p.state.is_live() {
                e.live_positions += 1;
            }
            e.spent_total_wei = e.spent_total_wei.saturating_add(p.entry_cost_wei);
            if now_ms.saturating_sub(p.opened_at_ms) <= 86_400_000 {
                e.spent_today_wei = e.spent_today_wei.saturating_add(p.entry_cost_wei);
            }
            if p.state.is_terminal() {
                e.realized_pnl_wei = e.realized_pnl_wei.saturating_add(p.net_pnl_wei(U256::ZERO));
            }
        }
        e
    }

    /// Full admission decision for a launch. Counts the rejection reason.
    pub fn admit(&self, candidate: &LaunchCandidate, now_ms: u64) -> Result<Admission, Rejection> {
        let params = self.params.read().clone();
        let exposure = self.exposure(now_ms);
        let result = gates::admit(&params, candidate, &exposure);
        if let Err(rejection) = &result {
            self.record_rejection(rejection.code());
        }
        result
    }

    pub fn effective_arming_blockers(&self) -> Vec<String> {
        let params = self.params();
        let mut blockers = params.arming_blockers();
        if self.paper_mode {
            blockers.retain(|blocker| !blocker.contains("SNIPER_VAULT_ADDRESS"));
        }
        if let Some(reason) = self.halt_reason() {
            blockers.insert(0, format!("lane halted: {reason}"));
        }
        if params.enabled && !self.boot_enabled && !self.paper_mode {
            blockers.push(
                "SNIPER_DIRECTIONAL was false at boot; runtime arming is refused".to_string(),
            );
        }
        blockers
    }

    /// Build the console payload.
    pub fn portfolio(&self, now_ms: u64, recent_closed_limit: usize) -> Portfolio {
        let armed = self.effective_armed();
        let blockers = self.effective_arming_blockers();
        portfolio::summarize(
            &self.positions(),
            &self.marks(),
            &self.symbols.read().clone(),
            now_ms,
            recent_closed_limit,
            blockers,
            armed,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eth(n: u64) -> U256 {
        U256::from(n) * U256::from(1_000_000_000_000_000_000u128)
    }
    fn centi(n: u64) -> U256 {
        U256::from(n) * U256::from(10_000_000_000_000_000u128)
    }

    fn armed_params() -> SniperParams {
        SniperParams {
            enabled: true,
            vault_address: Some(Address::repeat_byte(0xaa)),
            buy_size_wei: centi(10),
            daily_budget_wei: eth(1),
            min_liquidity_wei: eth(2),
            max_concurrent_positions: 2,
            ..Default::default()
        }
    }

    #[test]
    fn paper_ledger_starts_at_one_eth_and_is_atomic() {
        let lane = SniperLane::new_with_mode(SniperParams::default(), true);
        assert_eq!(lane.paper_balance_wei(), INITIAL_PAPER_BALANCE_WEI);
        assert!(lane.reserve_paper(U256::from(250_000_000_000_000_000u128)));
        assert_eq!(
            lane.paper_balance_wei(),
            U256::from(750_000_000_000_000_000u128)
        );
        lane.credit_paper(U256::from(50_000_000_000_000_000u128));
        assert_eq!(
            lane.paper_balance_wei(),
            U256::from(800_000_000_000_000_000u128)
        );
        assert!(!lane.reserve_paper(U256::from(900_000_000_000_000_000u128)));
        lane.reset_paper();
        assert_eq!(lane.paper_balance_wei(), INITIAL_PAPER_BALANCE_WEI);
    }

    #[test]
    fn paper_lane_can_enable_a_vault_bound_envelope_without_live_boot() {
        let lane = SniperLane::new_with_mode(SniperParams::default(), true);
        let result = lane.patch_params(&SniperParamsPatch {
            enabled: Some(true),
            vault_address: Some("0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into()),
            buy_size_wei: Some(eth(1).to_string()),
            daily_budget_wei: Some(eth(2).to_string()),
            ..Default::default()
        });
        assert!(
            result.is_ok(),
            "paper mode must not require live boot arming"
        );
        assert!(lane.effective_armed());
    }

    fn candidate() -> LaunchCandidate {
        LaunchCandidate {
            token: Address::with_last_byte(1),
            pair: Address::with_last_byte(2),
            weth_reserve: eth(10),
            token_reserve: U256::from(1_000_000u64),
            verdict: HoneypotVerdict::Clean {
                round_trip_bps: 9_940,
            },
            lp_locked: None,
            blacklisted: false,
        }
    }

    fn open_position(id: &str, token: u8) -> Position {
        Position {
            id: id.into(),
            chain_id: 1,
            token: Address::with_last_byte(token),
            pair: Address::with_last_byte(200 + token),
            venue: "univ2".into(),
            state: PositionState::Open,
            trigger_tx: None,
            entry_tx: None,
            entry_cost_wei: centi(10),
            entry_qty: U256::from(1_000u64),
            remaining_qty: U256::from(1_000u64),
            realized_wei: U256::ZERO,
            gas_spent_wei: U256::ZERO,
            peak_value_wei: centi(10),
            opened_block: 1,
            opened_at_ms: 1_000,
            closed_at_ms: None,
            exit_reason: None,
            entry_verdict: "clean".into(),
            notes: String::new(),
        }
    }

    #[test]
    fn a_default_lane_is_disarmed_and_says_why() {
        let lane = SniperLane::new(SniperParams::default());
        let pf = lane.portfolio(1_000, 10);
        assert!(!pf.armed);
        assert!(!pf.arming_blockers.is_empty());
    }

    #[test]
    fn an_armed_lane_admits_a_clean_launch() {
        let lane = SniperLane::new(armed_params());
        let a = lane.admit(&candidate(), 1_000).unwrap();
        assert_eq!(a.size_wei, centi(10));
        assert!(lane.portfolio(1_000, 10).armed);
    }

    #[test]
    fn a_lane_disabled_at_boot_cannot_be_armed_at_runtime() {
        let lane = SniperLane::new(SniperParams::default()); // enabled: false
        let err = lane
            .patch_params(&SniperParamsPatch {
                enabled: Some(true),
                vault_address: Some("0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string()),
                buy_size_wei: Some(centi(10).to_string()),
                daily_budget_wei: Some(eth(1).to_string()),
                ..Default::default()
            })
            .unwrap_err();
        assert!(err[0].contains("cannot be armed at runtime"), "{err:?}");
        assert!(!lane.params().enabled);
    }

    #[test]
    fn a_lane_armed_at_boot_can_be_disarmed_and_re_armed_at_runtime() {
        let lane = SniperLane::new(armed_params());
        lane.patch_params(&SniperParamsPatch {
            enabled: Some(false),
            ..Default::default()
        })
        .unwrap();
        assert!(!lane.params().enabled);
        // Re-arming is allowed because boot permitted it.
        lane.patch_params(&SniperParamsPatch {
            enabled: Some(true),
            ..Default::default()
        })
        .unwrap();
        assert!(lane.params().enabled);
    }

    #[test]
    fn an_invalid_patch_leaves_the_envelope_untouched() {
        let lane = SniperLane::new(armed_params());
        let before = lane.params();
        assert!(lane
            .patch_params(&SniperParamsPatch {
                sell_fraction_bps: Some(99_999),
                ..Default::default()
            })
            .is_err());
        assert_eq!(lane.params(), before);
    }

    #[test]
    fn halting_blocks_admission_but_not_the_portfolio() {
        let lane = SniperLane::new(armed_params());
        lane.halt("operator stopped the lane");
        assert_eq!(
            lane.admit(&candidate(), 1_000).unwrap_err().code(),
            "halted"
        );
        let pf = lane.portfolio(1_000, 10);
        assert!(!pf.armed);
        assert!(pf.arming_blockers[0].contains("operator stopped the lane"));

        lane.resume();
        assert!(lane.admit(&candidate(), 1_000).is_ok());
    }

    #[test]
    fn a_token_can_only_be_claimed_once() {
        let lane = SniperLane::new(armed_params());
        let t = Address::with_last_byte(7);
        assert!(lane.claim_token(t));
        assert!(!lane.claim_token(t), "second claim must be refused");
    }

    #[test]
    fn exposure_counts_live_positions_and_spend() {
        let lane = SniperLane::new(armed_params());
        lane.upsert_position(open_position("a", 1));
        lane.upsert_position(open_position("b", 2));
        let e = lane.exposure(1_000);
        assert_eq!(e.live_positions, 2);
        assert_eq!(e.spent_today_wei, centi(20));
        assert_eq!(e.spent_total_wei, centi(20));
    }

    #[test]
    fn the_concurrency_cap_stops_the_third_entry() {
        let lane = SniperLane::new(armed_params()); // cap 2
        lane.upsert_position(open_position("a", 1));
        lane.upsert_position(open_position("b", 2));
        assert_eq!(
            lane.admit(&candidate(), 1_000).unwrap_err().code(),
            "position_cap"
        );
    }

    #[test]
    fn closed_positions_free_a_concurrency_slot() {
        let lane = SniperLane::new(armed_params());
        let mut a = open_position("a", 1);
        a.state = PositionState::Closed;
        a.remaining_qty = U256::ZERO;
        a.realized_wei = centi(20);
        lane.upsert_position(a);
        lane.upsert_position(open_position("b", 2));
        assert!(lane.admit(&candidate(), 1_000).is_ok());
    }

    #[test]
    fn rejections_are_counted_by_code() {
        let lane = SniperLane::new(armed_params());
        let mut c = candidate();
        c.verdict = HoneypotVerdict::Honeypot;
        for _ in 0..3 {
            let _ = lane.admit(&c, 1_000);
        }
        assert_eq!(lane.rejection_counts().get("honeypot"), Some(&3));
    }

    #[test]
    fn hydrate_restores_positions_and_marks_their_tokens_seen() {
        let lane = SniperLane::new(armed_params());
        lane.hydrate(vec![open_position("a", 9)]);
        assert_eq!(lane.live_positions().len(), 1);
        assert!(
            !lane.claim_token(Address::with_last_byte(9)),
            "a recovered position's token must not be sniped again"
        );
    }

    #[test]
    fn setting_a_mark_updates_the_positions_peak() {
        let lane = SniperLane::new(armed_params());
        lane.upsert_position(open_position("a", 1));
        lane.set_mark("a", Mark::fresh(eth(3), 0, 0));
        assert_eq!(lane.position("a").unwrap().peak_value_wei, eth(3));
        // A lower mark does not lower the peak.
        lane.set_mark("a", Mark::fresh(eth(1), 0, 0));
        assert_eq!(lane.position("a").unwrap().peak_value_wei, eth(3));
    }

    #[test]
    fn the_blacklist_is_honoured_by_admission() {
        let lane = SniperLane::new(armed_params());
        let c = candidate();
        lane.blacklist(c.token);
        assert!(lane.is_blacklisted(c.token));
        let mut blacklisted = c.clone();
        blacklisted.blacklisted = true;
        assert_eq!(
            lane.admit(&blacklisted, 1_000).unwrap_err().code(),
            "blacklisted"
        );
    }
}

#[cfg(test)]
mod api_contract_tests {
    //! The JSON shapes the console depends on.
    //!
    //! These are contract tests, not behaviour tests: they assert the *shape*
    //! of what `/api/sniper/*` returns, because the panel reads these fields by
    //! name and a silent rename would blank a portfolio rather than fail a
    //! build.

    use super::*;

    fn eth(n: u64) -> U256 {
        U256::from(n) * U256::from(1_000_000_000_000_000_000u128)
    }

    fn lane_with_position() -> SniperLane {
        let lane = SniperLane::new(SniperParams {
            enabled: true,
            vault_address: Some(Address::repeat_byte(0xaa)),
            buy_size_wei: eth(1),
            daily_budget_wei: eth(5),
            ..Default::default()
        });
        lane.upsert_position(Position {
            id: "p1".into(),
            chain_id: 1,
            token: Address::with_last_byte(1),
            pair: Address::with_last_byte(2),
            venue: "univ2".into(),
            state: PositionState::Open,
            trigger_tx: None,
            entry_tx: None,
            entry_cost_wei: eth(1),
            entry_qty: U256::from(1_000u64),
            remaining_qty: U256::from(1_000u64),
            realized_wei: U256::ZERO,
            gas_spent_wei: U256::ZERO,
            peak_value_wei: eth(1),
            opened_block: 1,
            opened_at_ms: 1_000,
            closed_at_ms: None,
            exit_reason: None,
            entry_verdict: "clean".into(),
            notes: String::new(),
        });
        lane.set_mark("p1", Mark::fresh(eth(2), 0, 0));
        lane
    }

    #[test]
    fn portfolio_payload_has_every_field_the_panel_reads() {
        let v = serde_json::to_value(lane_with_position().portfolio(100_000, 20)).unwrap();
        for key in [
            "totals",
            "open",
            "recentClosed",
            "armingBlockers",
            "armed",
            "generatedAtMs",
        ] {
            assert!(v.get(key).is_some(), "portfolio.{key} is missing");
        }
        for key in [
            "openPositions",
            "closedPositions",
            "openCostWei",
            "openValueWei",
            "unrealizedPnlWei",
            "realizedPnlWei",
            "totalPnlWei",
            "gasSpentWei",
            "deployedTotalWei",
            "deployedTodayWei",
            "wins",
            "losses",
            "winRateBps",
            "anyMarkStale",
        ] {
            assert!(v["totals"].get(key).is_some(), "totals.{key} is missing");
        }
        for key in [
            "id",
            "token",
            "pair",
            "venue",
            "state",
            "symbol",
            "entryCostWei",
            "entryQty",
            "remainingQty",
            "realizedWei",
            "gasSpentWei",
            "markValueWei",
            "unrealizedPnlWei",
            "netPnlWei",
            "netPnlBps",
            "markStale",
            "openedBlock",
            "openedAtMs",
            "closedAtMs",
            "ageSecs",
            "exitReason",
            "entryVerdict",
            "notes",
        ] {
            assert!(v["open"][0].get(key).is_some(), "open[0].{key} is missing");
        }
    }

    #[test]
    fn params_payload_round_trips_through_json() {
        let params = SniperParams {
            enabled: true,
            buy_size_wei: eth(1),
            daily_budget_wei: eth(5),
            ..Default::default()
        };
        let v = serde_json::to_value(&params).unwrap();
        // camelCase on the wire, matching every other endpoint.
        assert!(v.get("buySizeWei").is_some());
        assert!(v.get("takeProfitBps").is_some());
        assert!(v.get("sellFractionBps").is_some());
        assert!(v.get("requireHoneypotPass").is_some());
        assert!(v.get("buy_size_wei").is_none(), "must not emit snake_case");

        let back: SniperParams = serde_json::from_value(v).unwrap();
        assert_eq!(back, params, "params must survive a JSON round trip");
    }

    #[test]
    fn wei_params_serialise_as_strings_not_numbers() {
        // U256 wei exceeds JS safe integers; a number here silently rounds money.
        let v = serde_json::to_value(SniperParams {
            buy_size_wei: U256::from(u128::MAX),
            ..Default::default()
        })
        .unwrap();
        assert!(v["buySizeWei"].is_string(), "buySizeWei must be a string");
    }

    #[test]
    fn a_patch_deserialises_from_the_consoles_camel_case_body() {
        let body = serde_json::json!({
            "enabled": true,
            "buySizeWei": "50000000000000000",
            "takeProfitBps": 5000,
            "sellFractionBps": 5000
        });
        let patch: SniperParamsPatch = serde_json::from_value(body).unwrap();
        assert_eq!(patch.enabled, Some(true));
        assert_eq!(patch.take_profit_bps, Some(5_000));
        assert_eq!(patch.buy_size_wei.as_deref(), Some("50000000000000000"));
    }

    #[test]
    fn an_empty_patch_body_is_valid_and_changes_nothing() {
        let patch: SniperParamsPatch = serde_json::from_value(serde_json::json!({})).unwrap();
        let lane = lane_with_position();
        let before = lane.params();
        assert_eq!(lane.patch_params(&patch).unwrap(), before);
    }

    #[test]
    fn position_state_and_exit_reason_serialise_as_snake_case_strings() {
        assert_eq!(
            serde_json::to_value(PositionState::Scaling).unwrap(),
            serde_json::json!("scaling")
        );
        assert_eq!(
            serde_json::to_value(ExitReason::TakeProfitPct).unwrap(),
            serde_json::json!("take_profit_pct")
        );
        assert_eq!(
            serde_json::to_value(ExitReason::HoneypotDetected).unwrap(),
            serde_json::json!("honeypot_detected")
        );
    }

    #[test]
    fn a_disarmed_lane_reports_blockers_in_its_payload() {
        let v = serde_json::to_value(SniperLane::new(SniperParams::default()).portfolio(1, 10))
            .unwrap();
        assert_eq!(v["armed"], serde_json::json!(false));
        assert!(
            !v["armingBlockers"].as_array().unwrap().is_empty(),
            "a disarmed lane must explain itself"
        );
    }
}
