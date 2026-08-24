//! Position lifecycle for the directional sniper.
//!
//! This is the module that makes the sniper different from every other
//! strategy in the repository. Everything else here is atomic: one bundle,
//! profit-or-revert, no state between blocks. A sniper position is *open* —
//! it survives across blocks, it is marked to market every head, and it can
//! lose money.
//!
//! The state machine is deliberately small and total:
//!
//! ```text
//!            admitted            buy landed
//!   (launch) ─────────▶ Pending ────────────▶ Open
//!                          │                   │  partial exit
//!                          │ buy failed        ├──────────────▶ Scaling ──┐
//!                          ▼                   │                          │
//!                       Abandoned              │ full exit                │
//!                                              ▼                          │
//!                                            Closed ◀────────────────────┘
//! ```
//!
//! `Scaling` is the "sold x%, still holding the rest" state the work order
//! calls for. It re-arms on the remaining quantity, so a runner can take a
//! second and third profit without any special-casing.

use alloy_primitives::{Address, B256, U256};
use serde::{Deserialize, Serialize};

use super::params::{SniperParams, BPS};

/// Which execution domain produced a position or fill.
///
/// One domain model, two ledgers: simulation and live share the same row
/// shape, and this tag is what keeps their balances, histories and totals
/// from ever bleeding into each other.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionMode {
    /// Contract-backed trade on the local Anvil fixture, settled against the
    /// paper bankroll.
    Simulation,
    /// Signed submission to the production vault on the selected chain.
    /// The serde/SQL default: pre-provenance rows were live-shaped.
    #[default]
    Live,
}

impl ExecutionMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            ExecutionMode::Simulation => "simulation",
            ExecutionMode::Live => "live",
        }
    }

    pub fn parse(raw: &str) -> Self {
        match raw {
            "live" => ExecutionMode::Live,
            _ => ExecutionMode::Simulation,
        }
    }
}

/// How a position's fills settle.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Settlement {
    /// Virtual bankroll: no chain settles these fills.
    Paper,
    /// Mined receipts on the configured chain. The serde/SQL default:
    /// pre-provenance rows were on-chain-shaped.
    #[default]
    OnChain,
}

impl Settlement {
    pub fn as_str(&self) -> &'static str {
        match self {
            Settlement::Paper => "paper",
            Settlement::OnChain => "on_chain",
        }
    }

    pub fn parse(raw: &str) -> Self {
        match raw {
            "on_chain" => Settlement::OnChain,
            _ => Settlement::Paper,
        }
    }
}

/// Settlement-lifecycle metadata, independent of [`PositionState`]: a row can
/// be `open` while its entry is still `submitted`, or `abandoned` because the
/// transaction `reverted`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TxStatus {
    /// Persisted before signing/broadcasting.
    Intent,
    /// Broadcast, receipt not yet observed.
    Submitted,
    /// Receipt observed and successful.
    #[default]
    Mined,
    /// Receipt observed, reverted (or the simulation call reverted).
    Reverted,
    /// Never submitted / dropped.
    Abandoned,
}

impl TxStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            TxStatus::Intent => "intent",
            TxStatus::Submitted => "submitted",
            TxStatus::Mined => "mined",
            TxStatus::Reverted => "reverted",
            TxStatus::Abandoned => "abandoned",
        }
    }

    pub fn parse(raw: &str) -> Self {
        match raw {
            "intent" => TxStatus::Intent,
            "submitted" => TxStatus::Submitted,
            "reverted" => TxStatus::Reverted,
            "abandoned" => TxStatus::Abandoned,
            _ => TxStatus::Mined,
        }
    }
}

/// Where a position is in its life.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PositionState {
    /// Entry bundle proposed/submitted, not yet confirmed on chain.
    Pending,
    /// Holding the full entry quantity.
    Open,
    /// A take-profit sold part of the position; the remainder is still held.
    Scaling,
    /// Fully exited (or written off). Terminal.
    Closed,
    /// The entry never landed. Terminal, and costs nothing but gas.
    Abandoned,
}

impl PositionState {
    pub fn as_str(&self) -> &'static str {
        match self {
            PositionState::Pending => "pending",
            PositionState::Open => "open",
            PositionState::Scaling => "scaling",
            PositionState::Closed => "closed",
            PositionState::Abandoned => "abandoned",
        }
    }

    /// A position that still has (or may still acquire) token exposure. These
    /// are the rows that count against `maxConcurrentPositions`.
    pub fn is_live(&self) -> bool {
        matches!(
            self,
            PositionState::Pending | PositionState::Open | PositionState::Scaling
        )
    }

    pub fn is_terminal(&self) -> bool {
        matches!(self, PositionState::Closed | PositionState::Abandoned)
    }
}

/// Why the exit monitor decided to sell.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExitReason {
    /// Mark reached `takeProfitBps` over entry.
    TakeProfitPct,
    /// Unrealised gain reached `takeProfitAbsWei`.
    TakeProfitAbs,
    /// Mark fell to `stopLossBps` below entry.
    StopLoss,
    /// Mark retraced `trailingStopBps` from its peak.
    TrailingStop,
    /// `maxHoldSecs` elapsed.
    MaxHold,
    /// The sell-side honeypot re-check started failing — get out now.
    HoneypotDetected,
    /// An operator pressed the button.
    Manual,
    /// The lane's drawdown stop tripped.
    RiskStop,
}

impl ExitReason {
    pub fn code(&self) -> &'static str {
        match self {
            Self::TakeProfitPct => "take_profit_pct",
            Self::TakeProfitAbs => "take_profit_abs",
            Self::StopLoss => "stop_loss",
            Self::TrailingStop => "trailing_stop",
            Self::MaxHold => "max_hold",
            Self::HoneypotDetected => "honeypot_detected",
            Self::Manual => "manual",
            Self::RiskStop => "risk_stop",
        }
    }
}

impl ExitReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            ExitReason::TakeProfitPct => "take_profit_pct",
            ExitReason::TakeProfitAbs => "take_profit_abs",
            ExitReason::StopLoss => "stop_loss",
            ExitReason::TrailingStop => "trailing_stop",
            ExitReason::MaxHold => "max_hold",
            ExitReason::HoneypotDetected => "honeypot_detected",
            ExitReason::Manual => "manual",
            ExitReason::RiskStop => "risk_stop",
        }
    }

    /// Urgent exits ignore `sellFractionBps` and dump the whole remaining
    /// position. Taking 50% out of a confirmed honeypot is not a strategy.
    pub fn is_full_exit(&self) -> bool {
        matches!(
            self,
            ExitReason::StopLoss
                | ExitReason::TrailingStop
                | ExitReason::MaxHold
                | ExitReason::HoneypotDetected
                | ExitReason::Manual
                | ExitReason::RiskStop
        )
    }
}

/// One directional sniper position.
///
/// All quantities are integers: `*_wei` are native/WETH amounts, `qty` is the
/// raw token amount in the token's own decimals. Nothing here is a float —
/// the mark is recomputed from pool reserves every block rather than stored
/// as a price.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Position {
    pub id: String,
    pub chain_id: u64,
    pub token: Address,
    pub pair: Address,
    /// Venue the entry executed against, e.g. `univ2` / `aerodrome`.
    pub venue: String,
    pub state: PositionState,

    /// The deployment/go-live transaction this position back-ran.
    pub trigger_tx: Option<B256>,
    /// Our entry transaction, once known.
    pub entry_tx: Option<B256>,
    /// The latest submitted exit transaction, while its receipt is pending.
    /// Receipt reconciliation books the exact fill from the vault's
    /// `ExitExecuted` event or rolls the optimistic fill back on a revert.
    pub exit_tx: Option<B256>,

    /// WETH committed on entry.
    pub entry_cost_wei: U256,
    /// Tokens received on entry.
    pub entry_qty: U256,
    /// Tokens still held.
    pub remaining_qty: U256,
    /// WETH realised from exits so far.
    pub realized_wei: U256,
    /// Gas spent on this position's entry and exits.
    pub gas_spent_wei: U256,

    /// Highest mark-to-market value the position has reached, for the
    /// trailing stop. Denominated in WETH.
    pub peak_value_wei: U256,

    pub opened_block: u64,
    pub opened_at_ms: u64,
    pub closed_at_ms: Option<u64>,
    pub exit_reason: Option<ExitReason>,
    /// Honeypot verdict recorded at admission time.
    pub entry_verdict: String,
    pub notes: String,

    // --- provenance (two-ledger model) ------------------------------------
    /// Which execution domain owns this row. Additive: old rows deserialize
    /// to the conservative defaults (`live` / `on_chain` / `mined`) and are
    /// backfilled in SQLite where their notes prove them simulation.
    #[serde(default)]
    pub execution_mode: ExecutionMode,
    #[serde(default)]
    pub settlement: Settlement,
    #[serde(default)]
    pub tx_status: TxStatus,
}

impl Position {
    /// Realised + unrealised PnL in wei, net of gas. Signed, because losing is
    /// a real outcome for this lane.
    ///
    /// `mark_value_wei` is what the remaining quantity would fetch if sold
    /// right now, already net of the sell-side price impact.
    pub fn net_pnl_wei(&self, mark_value_wei: U256) -> i128 {
        let credit = saturating_i128(self.realized_wei) + saturating_i128(mark_value_wei);
        let debit = saturating_i128(self.entry_cost_wei) + saturating_i128(self.gas_spent_wei);
        credit.saturating_sub(debit)
    }

    /// PnL as a signed bps of the entry cost. Zero-cost positions report 0
    /// rather than dividing by zero.
    pub fn net_pnl_bps(&self, mark_value_wei: U256) -> i64 {
        if self.entry_cost_wei.is_zero() {
            return 0;
        }
        let pnl = self.net_pnl_wei(mark_value_wei);
        let cost = saturating_i128(self.entry_cost_wei);
        ((pnl.saturating_mul(BPS as i128)) / cost).clamp(i64::MIN as i128, i64::MAX as i128) as i64
    }

    /// Gross (pre-gas) unrealised gain over the entry cost, in wei. Used by
    /// the absolute take-profit trigger, which the operator expresses as
    /// "sell after it has profited x ETH".
    pub fn gross_gain_wei(&self, mark_value_wei: U256) -> i128 {
        (saturating_i128(self.realized_wei) + saturating_i128(mark_value_wei))
            .saturating_sub(saturating_i128(self.entry_cost_wei))
    }

    /// Decide whether to sell, and how much.
    ///
    /// Evaluation order is by urgency, not by profitability: a honeypot or a
    /// stop-loss must win over a take-profit that happens to be true in the
    /// same block. Returns `None` to keep holding.
    pub fn evaluate_exit(
        &self,
        params: &SniperParams,
        mark_value_wei: U256,
        head_block: u64,
        now_ms: u64,
        sell_honeypot: bool,
    ) -> Option<ExitDecision> {
        self.evaluate_exit_with_staleness(
            params,
            mark_value_wei,
            head_block,
            now_ms,
            sell_honeypot,
            false,
        )
    }

    /// Evaluate exit decision, suppressing price-based rules if `mark_is_stale` is true.
    pub fn evaluate_exit_with_staleness(
        &self,
        params: &SniperParams,
        mark_value_wei: U256,
        head_block: u64,
        now_ms: u64,
        sell_honeypot: bool,
        mark_is_stale: bool,
    ) -> Option<ExitDecision> {
        if !matches!(self.state, PositionState::Open | PositionState::Scaling) {
            return None;
        }
        if self.remaining_qty.is_zero() {
            return None;
        }

        // 1. The sell side started reverting. Leave immediately, at any age:
        //    waiting out `minHoldBlocks` in a honeypot accomplishes nothing.
        if sell_honeypot {
            return Some(self.full_exit(ExitReason::HoneypotDetected));
        }

        // 2. Time stop (never suppressed by stale mark).
        if params.max_hold_secs > 0 {
            let age_ms = now_ms.saturating_sub(self.opened_at_ms);
            if age_ms >= params.max_hold_secs.saturating_mul(1_000) {
                return Some(self.full_exit(ExitReason::MaxHold));
            }
        }

        // A position must be allowed to exist for a moment before it can be
        // judged. Without this a same-block mark (which is just our own price
        // impact) instantly trips the stop-loss on every entry.
        let age_blocks = head_block.saturating_sub(self.opened_block);
        let seasoned = age_blocks >= params.min_hold_blocks;

        // Price-based rules require both a seasoned position AND a fresh mark.
        if !seasoned || mark_is_stale {
            return None;
        }

        // 3. Hard stop on the downside.
        if params.stop_loss_bps > 0 {
            let floor = mul_bps(
                self.entry_cost_wei,
                BPS.saturating_sub(params.stop_loss_bps),
            );
            if self.realized_wei.saturating_add(mark_value_wei) <= floor {
                return Some(self.full_exit(ExitReason::StopLoss));
            }
        }

        // 4. Trailing stop, only meaningful once a peak above entry exists.
        if params.trailing_stop_bps > 0
            && self.peak_value_wei > self.entry_cost_wei
            && !self.peak_value_wei.is_zero()
        {
            let trail_floor = mul_bps(
                self.peak_value_wei,
                BPS.saturating_sub(params.trailing_stop_bps),
            );
            if mark_value_wei <= trail_floor {
                return Some(self.full_exit(ExitReason::TrailingStop));
            }
        }

        // 5. Take profit — absolute first, because an operator who set an
        //    absolute target in ETH means that number literally.
        let gain = self.gross_gain_wei(mark_value_wei);
        if !params.take_profit_abs_wei.is_zero()
            && gain >= saturating_i128(params.take_profit_abs_wei)
        {
            return Some(self.partial_exit(params, ExitReason::TakeProfitAbs));
        }

        // 6. Take profit — percentage of entry cost.
        if params.take_profit_bps > 0 {
            let target = mul_bps(
                self.entry_cost_wei,
                BPS.saturating_add(params.take_profit_bps),
            );
            if self.realized_wei.saturating_add(mark_value_wei) >= target {
                return Some(self.partial_exit(params, ExitReason::TakeProfitPct));
            }
        }

        None
    }

    fn full_exit(&self, reason: ExitReason) -> ExitDecision {
        ExitDecision {
            reason,
            qty: self.remaining_qty,
            fraction_bps: BPS,
            closes_position: true,
        }
    }

    fn partial_exit(&self, params: &SniperParams, reason: ExitReason) -> ExitDecision {
        let frac = params.sell_fraction_bps.clamp(1, BPS);
        if frac >= BPS {
            return self.full_exit(reason);
        }
        let qty = mul_bps_qty(self.remaining_qty, frac);
        // Rounding on a tiny remainder can produce a zero-quantity sell, and a
        // zero-quantity sell is a wasted transaction that leaves the position
        // in exactly the state that produced it — an infinite loop. Round up
        // to the whole position instead.
        if qty.is_zero() {
            return self.full_exit(reason);
        }
        let closes = qty >= self.remaining_qty;
        ExitDecision {
            reason,
            qty,
            fraction_bps: frac,
            closes_position: closes,
        }
    }

    /// Fold a confirmed exit fill back into the position.
    pub fn apply_fill(&mut self, qty_sold: U256, proceeds_wei: U256, gas_wei: U256, now_ms: u64) {
        self.remaining_qty = self.remaining_qty.saturating_sub(qty_sold);
        self.realized_wei = self.realized_wei.saturating_add(proceeds_wei);
        self.gas_spent_wei = self.gas_spent_wei.saturating_add(gas_wei);
        if self.remaining_qty.is_zero() {
            self.state = PositionState::Closed;
            self.closed_at_ms = Some(now_ms);
        } else {
            self.state = PositionState::Scaling;
        }
    }

    /// Update the high-water mark used by the trailing stop.
    pub fn mark(&mut self, mark_value_wei: U256) {
        if mark_value_wei > self.peak_value_wei {
            self.peak_value_wei = mark_value_wei;
        }
    }
}

/// The exit monitor's instruction to the executor.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExitDecision {
    pub reason: ExitReason,
    /// Raw token quantity to sell.
    pub qty: U256,
    /// The fraction this represents, for the audit trail.
    pub fraction_bps: u32,
    /// Whether this sale takes the position to zero.
    pub closes_position: bool,
}

/// `value * bps / 10_000`, saturating. Used on WETH amounts.
fn mul_bps(value: U256, bps: u32) -> U256 {
    value.saturating_mul(U256::from(bps)) / U256::from(BPS)
}

/// Same, on a raw token quantity. Separate name because mixing the two units
/// up is the classic way to sell 10,000× too much.
fn mul_bps_qty(qty: U256, bps: u32) -> U256 {
    qty.saturating_mul(U256::from(bps)) / U256::from(BPS)
}

fn saturating_i128(v: U256) -> i128 {
    if v > U256::from(i128::MAX as u128) {
        i128::MAX
    } else {
        v.to::<u128>() as i128
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

    /// A 1 ETH entry that received 1,000 tokens, seasoned and clean.
    fn pos() -> Position {
        Position {
            id: "p1".into(),
            chain_id: 1,
            token: Address::with_last_byte(1),
            pair: Address::with_last_byte(2),
            venue: "univ2".into(),
            state: PositionState::Open,
            trigger_tx: None,
            entry_tx: None,

            exit_tx: None,
            entry_cost_wei: eth(1),
            entry_qty: U256::from(1_000u64),
            remaining_qty: U256::from(1_000u64),
            realized_wei: U256::ZERO,
            gas_spent_wei: U256::ZERO,
            peak_value_wei: eth(1),
            opened_block: 100,
            opened_at_ms: 1_000_000,
            closed_at_ms: None,
            exit_reason: None,
            entry_verdict: "clean".into(),
            notes: String::new(),
            execution_mode: ExecutionMode::Live,
            settlement: Settlement::OnChain,
            tx_status: TxStatus::Mined,
        }
    }

    fn params() -> SniperParams {
        SniperParams {
            enabled: true,
            buy_size_wei: eth(1),
            daily_budget_wei: eth(10),
            take_profit_bps: 10_000, // +100%
            sell_fraction_bps: 10_000,
            stop_loss_bps: 5_000, // -50%
            trailing_stop_bps: 0,
            max_hold_secs: 1_800,
            min_hold_blocks: 1,
            ..Default::default()
        }
    }

    #[test]
    fn holds_while_flat() {
        let p = pos();
        assert!(p
            .evaluate_exit(&params(), eth(1), 105, 1_000_000, false)
            .is_none());
    }

    #[test]
    fn take_profit_pct_fires_at_target() {
        let p = pos();
        // +100% => mark of 2 ETH on a 1 ETH entry.
        let d = p
            .evaluate_exit(&params(), eth(2), 105, 1_000_000, false)
            .expect("should exit");
        assert_eq!(d.reason, ExitReason::TakeProfitPct);
        assert!(d.closes_position);
        assert_eq!(d.qty, U256::from(1_000u64));
    }

    #[test]
    fn take_profit_pct_does_not_fire_below_target() {
        let p = pos();
        assert!(p
            .evaluate_exit(&params(), centi(199), 105, 1_000_000, false)
            .is_none());
    }

    #[test]
    fn sell_fraction_takes_only_part_and_moves_to_scaling() {
        let mut prm = params();
        prm.sell_fraction_bps = 5_000; // sell half
        let p = pos();
        let d = p
            .evaluate_exit(&prm, eth(2), 105, 1_000_000, false)
            .unwrap();
        assert_eq!(d.qty, U256::from(500u64));
        assert!(!d.closes_position);

        let mut p2 = p.clone();
        p2.apply_fill(d.qty, eth(1), U256::ZERO, 2_000_000);
        assert_eq!(p2.state, PositionState::Scaling);
        assert_eq!(p2.remaining_qty, U256::from(500u64));
        assert_eq!(p2.realized_wei, eth(1));
    }

    #[test]
    fn a_scaled_position_can_take_profit_again() {
        let mut prm = params();
        prm.sell_fraction_bps = 5_000;
        let mut p = pos();
        p.apply_fill(U256::from(500u64), eth(1), U256::ZERO, 2_000_000);
        assert_eq!(p.state, PositionState::Scaling);
        // Realised 1 ETH + remaining marked at 1.2 ETH = 2.2 ETH vs 1 ETH
        // entry: still past the +100% line, so the runner takes another bite.
        let d = p
            .evaluate_exit(&prm, centi(120), 110, 2_000_000, false)
            .expect("scaling position must stay armed");
        assert_eq!(d.reason, ExitReason::TakeProfitPct);
        assert_eq!(d.qty, U256::from(250u64));
    }

    #[test]
    fn absolute_take_profit_fires_on_wei_gain() {
        let mut prm = params();
        prm.take_profit_bps = 0;
        prm.take_profit_abs_wei = centi(50); // +0.5 ETH
        let p = pos();
        assert!(p
            .evaluate_exit(&prm, centi(140), 105, 1_000_000, false)
            .is_none());
        let d = p
            .evaluate_exit(&prm, centi(150), 105, 1_000_000, false)
            .unwrap();
        assert_eq!(d.reason, ExitReason::TakeProfitAbs);
    }

    #[test]
    fn stop_loss_fires_and_always_exits_fully() {
        let mut prm = params();
        prm.sell_fraction_bps = 2_500; // even with a small scale-out configured
        let p = pos();
        let d = p
            .evaluate_exit(&prm, centi(50), 105, 1_000_000, false)
            .unwrap();
        assert_eq!(d.reason, ExitReason::StopLoss);
        assert!(d.closes_position, "a stop must not leave a remainder");
        assert_eq!(d.qty, p.remaining_qty);
    }

    #[test]
    fn stop_loss_beats_take_profit_when_both_are_configured() {
        // Degenerate config: stop at -50%, take profit at +0.01%. A mark below
        // the stop must still exit as a stop, not as a profit.
        let mut prm = params();
        prm.take_profit_bps = 1;
        let p = pos();
        let d = p
            .evaluate_exit(&prm, centi(10), 105, 1_000_000, false)
            .unwrap();
        assert_eq!(d.reason, ExitReason::StopLoss);
    }

    #[test]
    fn trailing_stop_fires_after_a_peak() {
        let mut prm = params();
        prm.trailing_stop_bps = 2_000; // 20% off the high
        prm.take_profit_bps = 0;
        prm.stop_loss_bps = 0;
        let mut p = pos();
        p.mark(eth(3));
        assert_eq!(p.peak_value_wei, eth(3));
        // 2.5 ETH is only ~17% off the 3 ETH peak: hold.
        assert!(p
            .evaluate_exit(&prm, centi(250), 105, 1_000_000, false)
            .is_none());
        // 2.3 ETH is >20% off: exit.
        let d = p
            .evaluate_exit(&prm, centi(230), 105, 1_000_000, false)
            .unwrap();
        assert_eq!(d.reason, ExitReason::TrailingStop);
    }

    #[test]
    fn trailing_stop_ignores_positions_that_never_beat_entry() {
        let mut prm = params();
        prm.trailing_stop_bps = 1_000;
        prm.stop_loss_bps = 0;
        prm.take_profit_bps = 0;
        prm.max_hold_secs = 0;
        let p = pos(); // peak == entry == 1 ETH
        assert!(p
            .evaluate_exit(&prm, centi(95), 105, 1_000_000, false)
            .is_none());
    }

    #[test]
    fn max_hold_forces_an_exit() {
        let prm = params(); // 1800s
        let p = pos();
        let late = p.opened_at_ms + 1_800_000;
        let d = p.evaluate_exit(&prm, eth(1), 105, late, false).unwrap();
        assert_eq!(d.reason, ExitReason::MaxHold);
        assert!(d.closes_position);
    }

    #[test]
    fn honeypot_exits_immediately_even_before_min_hold() {
        let mut prm = params();
        prm.min_hold_blocks = 50;
        let p = pos();
        // Same block as entry: too young for any price-based rule...
        assert!(p
            .evaluate_exit(&prm, centi(1), 100, 1_000_000, false)
            .is_none());
        // ...but a failing sell check must not wait.
        let d = p
            .evaluate_exit(&prm, centi(1), 100, 1_000_000, true)
            .unwrap();
        assert_eq!(d.reason, ExitReason::HoneypotDetected);
        assert!(d.closes_position);
    }

    #[test]
    fn stale_mark_suppresses_price_based_exits() {
        let p = pos(); // 1 ETH entry cost
                       // Take profit target is +100% (2 ETH)
                       // Fresh mark at 2 ETH fires take profit
        assert!(p
            .evaluate_exit_with_staleness(&params(), eth(2), 105, 1_000_000, false, false)
            .is_some());
        // Stale mark at 2 ETH suppresses take profit
        assert!(p
            .evaluate_exit_with_staleness(&params(), eth(2), 105, 1_000_000, false, true)
            .is_none());

        // Stop loss target is -50% (0.5 ETH)
        // Fresh mark at 0.4 ETH fires stop loss
        assert!(p
            .evaluate_exit_with_staleness(&params(), centi(40), 105, 1_000_000, false, false)
            .is_some());
        // Stale mark at 0.4 ETH suppresses stop loss
        assert!(p
            .evaluate_exit_with_staleness(&params(), centi(40), 105, 1_000_000, false, true)
            .is_none());

        // But honeypot exit fires even with stale mark
        let d = p
            .evaluate_exit_with_staleness(&params(), eth(2), 105, 1_000_000, true, true)
            .unwrap();
        assert_eq!(d.reason, ExitReason::HoneypotDetected);
    }

    #[test]
    fn min_hold_blocks_suppresses_price_rules() {
        let mut prm = params();
        prm.min_hold_blocks = 5;
        let p = pos(); // opened at block 100
        assert!(
            p.evaluate_exit(&prm, eth(5), 102, 1_000_000, false)
                .is_none(),
            "must not act before minHoldBlocks"
        );
        assert!(
            p.evaluate_exit(&prm, eth(5), 105, 1_000_000, false)
                .is_some(),
            "must act once seasoned"
        );
    }

    #[test]
    fn terminal_positions_are_never_re_evaluated() {
        for state in [PositionState::Closed, PositionState::Abandoned] {
            let mut p = pos();
            p.state = state;
            assert!(p
                .evaluate_exit(&params(), eth(99), 999, 9_999_999, true)
                .is_none());
        }
    }

    #[test]
    fn a_pending_position_is_not_exitable() {
        let mut p = pos();
        p.state = PositionState::Pending;
        assert!(p
            .evaluate_exit(&params(), eth(99), 999, 9_999_999, false)
            .is_none());
    }

    #[test]
    fn dust_remainder_rounds_up_to_a_full_exit() {
        // 1 wei of token left with a 50% scale-out would compute a 0-quantity
        // sell. That must become a full exit, not a no-op that loops forever.
        let mut prm = params();
        prm.sell_fraction_bps = 5_000;
        let mut p = pos();
        p.remaining_qty = U256::from(1u64);
        let d = p
            .evaluate_exit(&prm, eth(2), 105, 1_000_000, false)
            .unwrap();
        assert_eq!(d.qty, U256::from(1u64));
        assert!(d.closes_position);
    }

    #[test]
    fn pnl_is_signed_and_nets_gas() {
        let mut p = pos();
        p.gas_spent_wei = centi(5); // 0.05 ETH
                                    // Marked at 1 ETH: down exactly the gas.
        assert_eq!(p.net_pnl_wei(eth(1)), -(centi(5).to::<u128>() as i128));
        // Marked at 2 ETH: up 1 ETH less gas.
        assert_eq!(
            p.net_pnl_wei(eth(2)),
            (eth(1) - centi(5)).to::<u128>() as i128
        );
    }

    #[test]
    fn pnl_bps_is_relative_to_entry_cost() {
        let p = pos();
        assert_eq!(p.net_pnl_bps(eth(2)), 10_000); // +100%
        assert_eq!(p.net_pnl_bps(eth(1)), 0);
        assert_eq!(p.net_pnl_bps(U256::ZERO), -10_000); // -100%
    }

    #[test]
    fn pnl_bps_on_a_zero_cost_position_does_not_divide_by_zero() {
        let mut p = pos();
        p.entry_cost_wei = U256::ZERO;
        assert_eq!(p.net_pnl_bps(eth(1)), 0);
    }

    #[test]
    fn apply_fill_to_zero_closes_the_position() {
        let mut p = pos();
        p.apply_fill(U256::from(1_000u64), eth(2), centi(1), 5_000_000);
        assert_eq!(p.state, PositionState::Closed);
        assert_eq!(p.closed_at_ms, Some(5_000_000));
        assert!(p.remaining_qty.is_zero());
    }

    #[test]
    fn live_states_are_exactly_the_ones_that_hold_exposure() {
        assert!(PositionState::Pending.is_live());
        assert!(PositionState::Open.is_live());
        assert!(PositionState::Scaling.is_live());
        assert!(!PositionState::Closed.is_live());
        assert!(!PositionState::Abandoned.is_live());
        assert!(PositionState::Closed.is_terminal());
        assert!(PositionState::Abandoned.is_terminal());
    }

    #[test]
    fn every_urgent_reason_is_a_full_exit() {
        for r in [
            ExitReason::StopLoss,
            ExitReason::TrailingStop,
            ExitReason::MaxHold,
            ExitReason::HoneypotDetected,
            ExitReason::Manual,
            ExitReason::RiskStop,
        ] {
            assert!(r.is_full_exit(), "{r:?} must exit fully");
        }
        assert!(!ExitReason::TakeProfitPct.is_full_exit());
        assert!(!ExitReason::TakeProfitAbs.is_full_exit());
    }
}
