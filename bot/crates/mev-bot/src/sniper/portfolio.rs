//! Portfolio aggregation — what the console's mini portfolio renders.
//!
//! Kept as a pure function over positions and marks so it can be unit tested
//! without a database, an RPC or a running engine. The API layer's only job is
//! to fetch rows, fetch marks, call [`summarize`], and serialise the result.
//!
//! One rule governs everything in this file: **realised and unrealised are
//! never added together silently**. They are separate fields all the way to
//! the UI, because a portfolio showing "+2.4 ETH" that is entirely unrealised
//! paper gain on an illiquid token is the single most misleading number this
//! project could render.

use std::collections::HashMap;

use alloy_primitives::{Address, U256};
use serde::{Deserialize, Serialize};

use super::params::BPS;
use super::position::{Position, PositionState};

/// One row in the console's portfolio table.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PortfolioRow {
    pub id: String,
    pub token: Address,
    pub pair: Address,
    pub venue: String,
    pub state: PositionState,
    pub symbol: Option<String>,

    /// Decimal strings throughout: these exceed JS safe integers.
    pub entry_cost_wei: String,
    pub entry_qty: String,
    pub remaining_qty: String,
    pub realized_wei: String,
    pub gas_spent_wei: String,
    /// What the remaining quantity is worth right now, net of sell impact.
    pub mark_value_wei: String,
    /// Signed, net of gas.
    pub unrealized_pnl_wei: String,
    pub net_pnl_wei: String,
    pub net_pnl_bps: i64,
    /// True when the mark is stale or unavailable — the UI must not render a
    /// confident number over a guess.
    pub mark_stale: bool,

    pub opened_block: u64,
    pub opened_at_ms: u64,
    pub closed_at_ms: Option<u64>,
    pub age_secs: u64,
    pub exit_reason: Option<String>,
    pub entry_verdict: String,
    pub notes: String,
}

/// Aggregate totals across the lane.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PortfolioTotals {
    pub open_positions: usize,
    pub closed_positions: usize,
    /// Entry cost of everything still held.
    pub open_cost_wei: String,
    /// Current mark of everything still held.
    pub open_value_wei: String,
    /// Paper PnL on open positions.
    pub unrealized_pnl_wei: String,
    /// Booked PnL from closed and partially-closed positions.
    pub realized_pnl_wei: String,
    /// The two above, added. Presented alongside them, never instead.
    pub total_pnl_wei: String,
    pub gas_spent_wei: String,
    /// Lifetime wei committed to entries.
    pub deployed_total_wei: String,
    /// Wei committed in the rolling 24h window.
    pub deployed_today_wei: String,
    /// Wins / losses among *closed* positions only.
    pub wins: usize,
    pub losses: usize,
    /// Win rate in bps of closed positions. 0 when nothing has closed.
    pub win_rate_bps: u32,
    /// True when any open position's mark could not be refreshed.
    pub any_mark_stale: bool,
}

/// The whole payload behind `GET /api/sniper/portfolio`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Portfolio {
    pub totals: PortfolioTotals,
    pub open: Vec<PortfolioRow>,
    pub recent_closed: Vec<PortfolioRow>,
    /// Reasons the lane cannot currently buy. Empty means armed.
    pub arming_blockers: Vec<String>,
    pub armed: bool,
    pub generated_at_ms: u64,
}

/// A mark for one position: what the remaining quantity would fetch.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Mark {
    pub value_wei: U256,
    pub block: u64,
    pub ts: u64,
    /// False when the pool read failed and this is a carried-forward value.
    pub fresh: bool,
}

impl Mark {
    pub fn fresh(value_wei: U256, block: u64, ts: u64) -> Self {
        Self {
            value_wei,
            block,
            ts,
            fresh: true,
        }
    }
    pub fn stale(value_wei: U256, block: u64, ts: u64) -> Self {
        Self {
            value_wei,
            block,
            ts,
            fresh: false,
        }
    }
    pub fn is_stale(&self, current_block: u64) -> bool {
        !self.fresh
            || (self.block > 0
                && current_block > 0
                && current_block.saturating_sub(self.block) > 12)
    }
}

/// Build the console payload.
///
/// `marks` is keyed by position id. A position with no mark is treated as
/// stale-at-zero, which is the conservative reading: an unmarkable position is
/// worth nothing until proven otherwise.
pub fn summarize(
    positions: &[Position],
    marks: &HashMap<String, Mark>,
    symbols: &HashMap<Address, String>,
    now_ms: u64,
    recent_closed_limit: usize,
    arming_blockers: Vec<String>,
    armed: bool,
) -> Portfolio {
    let mut open = Vec::new();
    let mut closed = Vec::new();
    let mut t = Totals::default();

    for p in positions {
        let mark = marks.get(&p.id).copied().unwrap_or(Mark {
            value_wei: U256::ZERO,
            block: 0,
            ts: 0,
            fresh: false,
        });
        // A terminal position holds nothing, so its mark is definitionally
        // zero and definitionally fresh — never let a missing pool read make a
        // closed position look stale.
        let (mark_value, fresh) = if p.state.is_terminal() {
            (U256::ZERO, true)
        } else {
            (mark.value_wei, mark.fresh)
        };

        let net = p.net_pnl_wei(mark_value);
        let unrealized = if p.state.is_terminal() {
            0i128
        } else {
            // Paper gain on what is still held, versus the share of entry cost
            // still riding. Approximated pro-rata by remaining quantity.
            let cost_still_out = pro_rata(p.entry_cost_wei, p.remaining_qty, p.entry_qty);
            to_i128(mark_value).saturating_sub(to_i128(cost_still_out))
        };

        let row = PortfolioRow {
            id: p.id.clone(),
            token: p.token,
            pair: p.pair,
            venue: p.venue.clone(),
            state: p.state,
            symbol: symbols.get(&p.token).cloned(),
            entry_cost_wei: p.entry_cost_wei.to_string(),
            entry_qty: p.entry_qty.to_string(),
            remaining_qty: p.remaining_qty.to_string(),
            realized_wei: p.realized_wei.to_string(),
            gas_spent_wei: p.gas_spent_wei.to_string(),
            mark_value_wei: mark_value.to_string(),
            unrealized_pnl_wei: unrealized.to_string(),
            net_pnl_wei: net.to_string(),
            net_pnl_bps: p.net_pnl_bps(mark_value),
            mark_stale: !fresh,
            opened_block: p.opened_block,
            opened_at_ms: p.opened_at_ms,
            closed_at_ms: p.closed_at_ms,
            age_secs: now_ms.saturating_sub(p.opened_at_ms) / 1_000,
            exit_reason: p.exit_reason.map(|r| r.as_str().to_string()),
            entry_verdict: p.entry_verdict.clone(),
            notes: p.notes.clone(),
        };

        t.gas = t.gas.saturating_add(p.gas_spent_wei);
        t.deployed_total = t.deployed_total.saturating_add(p.entry_cost_wei);
        if now_ms.saturating_sub(p.opened_at_ms) <= 86_400_000 {
            t.deployed_today = t.deployed_today.saturating_add(p.entry_cost_wei);
        }

        if p.state.is_terminal() {
            // A closed position's PnL is fully booked.
            t.realized = t.realized.saturating_add(net);
            if p.state == PositionState::Closed {
                if net > 0 {
                    t.wins += 1;
                } else if net < 0 {
                    t.losses += 1;
                }
                t.closed += 1;
            }
            closed.push(row);
        } else {
            t.open += 1;
            t.open_cost = t.open_cost.saturating_add(p.entry_cost_wei);
            t.open_value = t.open_value.saturating_add(mark_value);
            t.unrealized = t.unrealized.saturating_add(unrealized);
            // Proceeds already banked on a partially-exited position are
            // realised even though the position is still live.
            t.realized = t
                .realized
                .saturating_add(to_i128(p.realized_wei).saturating_sub(to_i128(pro_rata(
                    p.entry_cost_wei,
                    p.entry_qty.saturating_sub(p.remaining_qty),
                    p.entry_qty,
                ))));
            if !fresh {
                t.any_stale = true;
            }
            open.push(row);
        }
    }

    // Newest first in both tables.
    open.sort_by(|a, b| b.opened_at_ms.cmp(&a.opened_at_ms));
    closed.sort_by(|a, b| {
        b.closed_at_ms
            .unwrap_or(b.opened_at_ms)
            .cmp(&a.closed_at_ms.unwrap_or(a.opened_at_ms))
    });
    closed.truncate(recent_closed_limit);

    let decided = t.wins + t.losses;
    let win_rate_bps = if decided == 0 {
        0
    } else {
        ((t.wins as u64 * BPS as u64) / decided as u64) as u32
    };

    Portfolio {
        totals: PortfolioTotals {
            open_positions: t.open,
            closed_positions: t.closed,
            open_cost_wei: t.open_cost.to_string(),
            open_value_wei: t.open_value.to_string(),
            unrealized_pnl_wei: t.unrealized.to_string(),
            realized_pnl_wei: t.realized.to_string(),
            total_pnl_wei: t.realized.saturating_add(t.unrealized).to_string(),
            gas_spent_wei: t.gas.to_string(),
            deployed_total_wei: t.deployed_total.to_string(),
            deployed_today_wei: t.deployed_today.to_string(),
            wins: t.wins,
            losses: t.losses,
            win_rate_bps,
            any_mark_stale: t.any_stale,
        },
        open,
        recent_closed: closed,
        arming_blockers,
        armed,
        generated_at_ms: now_ms,
    }
}

#[derive(Default)]
struct Totals {
    open: usize,
    closed: usize,
    open_cost: U256,
    open_value: U256,
    unrealized: i128,
    realized: i128,
    gas: U256,
    deployed_total: U256,
    deployed_today: U256,
    wins: usize,
    losses: usize,
    any_stale: bool,
}

/// `total * part / whole`, saturating, zero-safe.
fn pro_rata(total: U256, part: U256, whole: U256) -> U256 {
    if whole.is_zero() {
        return U256::ZERO;
    }
    total.saturating_mul(part) / whole
}

fn to_i128(v: U256) -> i128 {
    if v > U256::from(i128::MAX as u128) {
        i128::MAX
    } else {
        v.to::<u128>() as i128
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sniper::position::ExitReason;

    fn eth(n: u64) -> U256 {
        U256::from(n) * U256::from(1_000_000_000_000_000_000u128)
    }
    fn centi(n: u64) -> U256 {
        U256::from(n) * U256::from(10_000_000_000_000_000u128)
    }

    fn p(id: &str, state: PositionState) -> Position {
        Position {
            id: id.into(),
            chain_id: 1,
            token: Address::with_last_byte(1),
            pair: Address::with_last_byte(2),
            venue: "univ2".into(),
            state,
            trigger_tx: None,
            entry_tx: None,
            entry_cost_wei: eth(1),
            entry_qty: U256::from(1_000u64),
            remaining_qty: if state.is_terminal() {
                U256::ZERO
            } else {
                U256::from(1_000u64)
            },
            realized_wei: U256::ZERO,
            gas_spent_wei: U256::ZERO,
            peak_value_wei: eth(1),
            opened_block: 1,
            opened_at_ms: 1_000,
            closed_at_ms: None,
            exit_reason: None,
            entry_verdict: "clean".into(),
            notes: String::new(),
        }
    }

    fn marks(pairs: &[(&str, U256)]) -> HashMap<String, Mark> {
        pairs
            .iter()
            .map(|(id, v)| (id.to_string(), Mark::fresh(*v, 0, 0)))
            .collect()
    }

    fn sum(positions: &[Position], m: &HashMap<String, Mark>) -> Portfolio {
        summarize(positions, m, &HashMap::new(), 100_000, 10, Vec::new(), true)
    }

    #[test]
    fn an_empty_portfolio_is_all_zeroes() {
        let out = sum(&[], &HashMap::new());
        assert_eq!(out.totals.open_positions, 0);
        assert_eq!(out.totals.total_pnl_wei, "0");
        assert_eq!(out.totals.win_rate_bps, 0);
        assert!(out.open.is_empty());
        assert!(!out.totals.any_mark_stale);
    }

    #[test]
    fn one_open_position_in_profit() {
        let out = sum(&[p("a", PositionState::Open)], &marks(&[("a", eth(2))]));
        assert_eq!(out.totals.open_positions, 1);
        assert_eq!(out.totals.open_cost_wei, eth(1).to_string());
        assert_eq!(out.totals.open_value_wei, eth(2).to_string());
        assert_eq!(out.totals.unrealized_pnl_wei, eth(1).to_string());
        assert_eq!(out.totals.realized_pnl_wei, "0");
        assert_eq!(out.open[0].net_pnl_bps, 10_000);
    }

    #[test]
    fn realized_and_unrealized_stay_separate() {
        // One open winner on paper, one closed loser booked.
        let mut closed = p("closed", PositionState::Closed);
        closed.realized_wei = centi(50); // sold for 0.5 against a 1 ETH entry
        closed.closed_at_ms = Some(50_000);
        closed.exit_reason = Some(ExitReason::StopLoss);

        let out = sum(
            &[p("open", PositionState::Open), closed],
            &marks(&[("open", eth(3))]),
        );
        assert_eq!(out.totals.unrealized_pnl_wei, eth(2).to_string());
        assert_eq!(out.totals.realized_pnl_wei, format!("-{}", centi(50)));
        // And the sum is offered as its own field, not substituted for either.
        assert_eq!(out.totals.total_pnl_wei, centi(150).to_string());
    }

    #[test]
    fn a_partially_exited_position_books_its_realized_share() {
        let mut pos = p("scaling", PositionState::Scaling);
        // Sold half the tokens for 1.5 ETH; 0.5 ETH of entry cost is booked.
        pos.remaining_qty = U256::from(500u64);
        pos.realized_wei = centi(150);
        let out = sum(&[pos], &marks(&[("scaling", centi(150))]));
        assert_eq!(out.totals.open_positions, 1);
        // Realised: 1.5 proceeds - 0.5 cost basis = +1.0
        assert_eq!(out.totals.realized_pnl_wei, eth(1).to_string());
        // Unrealised: 1.5 mark - 0.5 remaining basis = +1.0
        assert_eq!(out.totals.unrealized_pnl_wei, eth(1).to_string());
    }

    #[test]
    fn a_missing_mark_is_stale_and_worth_zero() {
        let out = sum(&[p("a", PositionState::Open)], &HashMap::new());
        assert!(out.totals.any_mark_stale);
        assert!(out.open[0].mark_stale);
        assert_eq!(out.open[0].mark_value_wei, "0");
        assert_eq!(out.open[0].net_pnl_bps, -10_000);
    }

    #[test]
    fn an_explicitly_stale_mark_is_flagged_but_keeps_its_value() {
        let mut m = HashMap::new();
        m.insert("a".to_string(), Mark::stale(centi(90), 0, 0));
        let out = sum(&[p("a", PositionState::Open)], &m);
        assert!(out.open[0].mark_stale);
        assert_eq!(out.open[0].mark_value_wei, centi(90).to_string());
    }

    #[test]
    fn a_closed_position_is_never_stale() {
        // No mark supplied at all, but a closed position holds nothing.
        let out = sum(&[p("a", PositionState::Closed)], &HashMap::new());
        assert!(!out.totals.any_mark_stale);
        assert_eq!(out.recent_closed[0].mark_value_wei, "0");
        assert!(!out.recent_closed[0].mark_stale);
    }

    #[test]
    fn win_rate_counts_only_closed_positions() {
        let mut win = p("w", PositionState::Closed);
        win.realized_wei = eth(2);
        let mut loss = p("l", PositionState::Closed);
        loss.realized_wei = centi(10);
        let open = p("o", PositionState::Open);

        let out = sum(&[win, loss, open], &marks(&[("o", eth(50))]));
        assert_eq!(out.totals.wins, 1);
        assert_eq!(out.totals.losses, 1);
        assert_eq!(out.totals.win_rate_bps, 5_000);
        assert_eq!(out.totals.closed_positions, 2);
        assert_eq!(out.totals.open_positions, 1);
    }

    #[test]
    fn abandoned_positions_do_not_count_as_losses() {
        // The entry never landed, so it is not a trading loss.
        let out = sum(&[p("a", PositionState::Abandoned)], &HashMap::new());
        assert_eq!(out.totals.losses, 0);
        assert_eq!(out.totals.wins, 0);
        assert_eq!(out.totals.closed_positions, 0);
        assert_eq!(out.totals.win_rate_bps, 0);
    }

    #[test]
    fn gas_is_subtracted_from_pnl() {
        let mut pos = p("a", PositionState::Open);
        pos.gas_spent_wei = centi(10);
        let out = sum(&[pos], &marks(&[("a", eth(1))]));
        assert_eq!(out.totals.gas_spent_wei, centi(10).to_string());
        assert_eq!(out.open[0].net_pnl_wei, format!("-{}", centi(10)));
    }

    #[test]
    fn deployed_today_excludes_older_entries() {
        let mut old = p("old", PositionState::Open);
        old.opened_at_ms = 0;
        let mut new = p("new", PositionState::Open);
        new.opened_at_ms = 90_000_000;

        let out = summarize(
            &[old, new],
            &marks(&[("old", eth(1)), ("new", eth(1))]),
            &HashMap::new(),
            90_000_100, // "now"
            10,
            Vec::new(),
            true,
        );
        assert_eq!(out.totals.deployed_total_wei, eth(2).to_string());
        assert_eq!(out.totals.deployed_today_wei, eth(1).to_string());
    }

    #[test]
    fn recent_closed_is_truncated_and_newest_first() {
        let mut ps = Vec::new();
        for i in 0..10 {
            let mut c = p(&format!("c{i}"), PositionState::Closed);
            c.closed_at_ms = Some(1_000 + i as u64);
            ps.push(c);
        }
        let out = summarize(
            &ps,
            &HashMap::new(),
            &HashMap::new(),
            100_000,
            3,
            vec![],
            true,
        );
        assert_eq!(out.recent_closed.len(), 3);
        assert_eq!(out.recent_closed[0].id, "c9");
        assert_eq!(out.recent_closed[2].id, "c7");
    }

    #[test]
    fn open_rows_are_newest_first() {
        let mut a = p("a", PositionState::Open);
        a.opened_at_ms = 10;
        let mut b = p("b", PositionState::Open);
        b.opened_at_ms = 20;
        let out = sum(&[a, b], &marks(&[("a", eth(1)), ("b", eth(1))]));
        assert_eq!(out.open[0].id, "b");
    }

    #[test]
    fn symbols_are_attached_when_known() {
        let mut sym = HashMap::new();
        sym.insert(Address::with_last_byte(1), "PEPE".to_string());
        let out = summarize(
            &[p("a", PositionState::Open)],
            &marks(&[("a", eth(1))]),
            &sym,
            100_000,
            10,
            vec![],
            true,
        );
        assert_eq!(out.open[0].symbol.as_deref(), Some("PEPE"));
    }

    #[test]
    fn arming_blockers_pass_through_verbatim() {
        let blockers = vec!["dailyBudgetWei is 0".to_string()];
        let out = summarize(
            &[],
            &HashMap::new(),
            &HashMap::new(),
            1,
            10,
            blockers.clone(),
            false,
        );
        assert_eq!(out.arming_blockers, blockers);
        assert!(!out.armed);
    }

    #[test]
    fn every_wei_field_serialises_as_a_string() {
        // JS safe integers stop at 2^53; wei routinely exceeds it. If any of
        // these ever become numbers the console silently rounds money.
        let out = sum(&[p("a", PositionState::Open)], &marks(&[("a", eth(2))]));
        let v = serde_json::to_value(&out).unwrap();
        for key in [
            "openCostWei",
            "openValueWei",
            "unrealizedPnlWei",
            "realizedPnlWei",
            "totalPnlWei",
            "gasSpentWei",
            "deployedTotalWei",
            "deployedTodayWei",
        ] {
            assert!(
                v["totals"][key].is_string(),
                "totals.{key} must serialise as a string"
            );
        }
        for key in [
            "entryCostWei",
            "realizedWei",
            "markValueWei",
            "netPnlWei",
            "unrealizedPnlWei",
        ] {
            assert!(
                v["open"][0][key].is_string(),
                "open[0].{key} must serialise as a string"
            );
        }
    }
}
