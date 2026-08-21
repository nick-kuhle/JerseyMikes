//! Replay harness: re-read historical blocks from SQLite and compare our
//! simulations against what actually landed on chain.
//!
//! Two jobs share this module:
//!
//! 1. **Offline compare** (`mev-bot replay`). Pure SQLite: join stored
//!    simulations to relay bid traces and delivered-block transactions, and
//!    emit a true-positive / false-positive / competitive ranking. No RPC.
//! 2. **Online reconciliation** (engine, every new head). Same comparison,
//!    written to the `reconciliations` table so the dashboard can chart it.
//!
//! Re-simulating the *exact* signed bundle from SQLite is not possible —
//! `opportunities` does not persist calldata — so the harness does not pretend
//! to. What it *can* do, and what Phase 1 asked for, is: take every simulation
//! we recorded for a block, look at the relay's realised builder payment and
//! the transactions that actually landed, and say whether the observation
//! would have been a real, competitive inclusion.

use alloy_primitives::U256;
use anyhow::Result;
use serde_json::{json, Value};

use crate::competition::Competition;
use crate::store::Store;

#[derive(Clone, Debug)]
pub struct ReplayRow {
    pub block_number: u64,
    pub opportunity_id: String,
    pub strategy: String,
    pub sim_success: bool,
    pub sim_net_wei: i64,
    pub our_bribe_wei: String,
    pub winning_bid_wei: String,
    pub victim_landed: bool,
    pub would_outbid: bool,
    pub inclusion_p: f64,
    /// Sim succeeded *and* the victim actually landed. This is the true
    /// positive: a bundle that the on-chain profit guard would have let
    /// through, against a transaction that was in the block.
    pub true_positive: bool,
    /// Sim succeeded but the victim did *not* land. State-divergence or a
    /// competing searcher got there first; either way, not a real fill.
    pub false_positive: bool,
}

impl ReplayRow {
    pub fn to_json(&self) -> Value {
        json!({
            "blockNumber": self.block_number,
            "opportunityId": self.opportunity_id,
            "strategy": self.strategy,
            "simSuccess": self.sim_success,
            "simNetWei": self.sim_net_wei,
            "ourBribeWei": self.our_bribe_wei,
            "winningBidWei": self.winning_bid_wei,
            "victimLanded": self.victim_landed,
            "wouldOutbid": self.would_outbid,
            "inclusionP": self.inclusion_p,
            "truePositive": self.true_positive,
            "falsePositive": self.false_positive,
        })
    }
}

#[derive(Clone, Debug, Default)]
pub struct ReplaySummary {
    pub rows: usize,
    pub blocks: usize,
    pub true_positives: usize,
    pub false_positives: usize,
    pub victims_landed: usize,
    pub would_outbid: usize,
    pub mean_inclusion_p: f64,
}

impl ReplaySummary {
    pub fn from_rows(rows: &[ReplayRow]) -> Self {
        let mut blocks = std::collections::BTreeSet::new();
        let mut tp = 0usize;
        let mut fp = 0usize;
        let mut landed = 0usize;
        let mut outbid = 0usize;
        let mut p_sum = 0.0;
        for r in rows {
            blocks.insert(r.block_number);
            if r.true_positive {
                tp += 1;
            }
            if r.false_positive {
                fp += 1;
            }
            if r.victim_landed {
                landed += 1;
            }
            if r.would_outbid {
                outbid += 1;
            }
            p_sum += r.inclusion_p;
        }
        Self {
            rows: rows.len(),
            blocks: blocks.len(),
            true_positives: tp,
            false_positives: fp,
            victims_landed: landed,
            would_outbid: outbid,
            mean_inclusion_p: if rows.is_empty() {
                0.0
            } else {
                p_sum / rows.len() as f64
            },
        }
    }

    pub fn true_positive_rate(&self) -> f64 {
        let denom = self.true_positives + self.false_positives;
        if denom == 0 {
            0.0
        } else {
            self.true_positives as f64 / denom as f64
        }
    }

    pub fn to_json(&self) -> Value {
        json!({
            "rows": self.rows,
            "blocks": self.blocks,
            "truePositives": self.true_positives,
            "falsePositives": self.false_positives,
            "truePositiveRate": self.true_positive_rate(),
            "victimsLanded": self.victims_landed,
            "wouldOutbid": self.would_outbid,
            "meanInclusionP": self.mean_inclusion_p,
        })
    }
}

/// Compare stored anvil-fork simulations in `[from_block, to_block]` (inclusive
/// on both ends; `None` means unbounded) against relay bid traces and
/// delivered-block transactions.
pub fn compare(store: &Store, from_block: Option<u64>, to_block: Option<u64>, limit: i64) -> Result<Vec<ReplayRow>> {
    let sims = store.replay_candidates(from_block, to_block, limit)?;
    let mut out = Vec::with_capacity(sims.len());
    for s in sims {
        let winning = store
            .winning_bid_for_block(s.block_number)?
            .unwrap_or(U256::ZERO);
        let bribe = s.bribe_wei.parse::<U256>().unwrap_or(U256::ZERO);
        let rank = Competition::rank(bribe, winning);
        let victim_landed = if s.victims.is_empty() {
            // No victim (arb / liquidation / sniper): the "landed" question
            // does not apply. Treat as landed so a successful sim counts as a
            // true positive against the block that actually got built.
            true
        } else {
            store.any_victim_landed(s.block_number, &s.victims)?
        };
        let true_positive = s.success && victim_landed;
        let false_positive = s.success && !victim_landed;
        out.push(ReplayRow {
            block_number: s.block_number,
            opportunity_id: s.opportunity_id,
            strategy: s.strategy,
            sim_success: s.success,
            sim_net_wei: s.net_wei,
            our_bribe_wei: s.bribe_wei,
            winning_bid_wei: winning.to_string(),
            victim_landed,
            would_outbid: rank.would_outbid,
            inclusion_p: rank.inclusion_p,
            true_positive,
            false_positive,
        });
    }
    Ok(out)
}

/// Persist a compare-pass as reconciliation rows (idempotent per
/// opportunity + block).
pub fn persist(store: &Store, rows: &[ReplayRow]) -> Result<usize> {
    let mut n = 0;
    for r in rows {
        store.record_reconciliation(
            r.block_number,
            &r.opportunity_id,
            &r.strategy,
            r.sim_net_wei,
            &r.our_bribe_wei,
            &r.winning_bid_wei,
            r.victim_landed,
            r.would_outbid,
            r.inclusion_p,
            r.true_positive,
            r.false_positive,
        )?;
        n += 1;
    }
    Ok(n)
}

/// Pretty-print a summary plus a short table. Used by the CLI.
pub fn render(rows: &[ReplayRow]) -> String {
    let sum = ReplaySummary::from_rows(rows);
    let mut out = String::new();
    out.push_str(&format!(
        "replay: {} sims across {} blocks\n  true-positives {}  false-positives {}  tpr {:.3}\n  victims-landed {}  would-outbid {}  mean inclusion p {:.3}\n",
        sum.rows,
        sum.blocks,
        sum.true_positives,
        sum.false_positives,
        sum.true_positive_rate(),
        sum.victims_landed,
        sum.would_outbid,
        sum.mean_inclusion_p,
    ));
    if rows.is_empty() {
        out.push_str("(no stored simulations in the requested window)\n");
        return out;
    }
    out.push_str("block      strategy      net wei     bribe/bid          p    flags\n");
    for r in rows.iter().take(50) {
        let flags = format!(
            "{}{}{}",
            if r.true_positive { "TP " } else { "" },
            if r.false_positive { "FP " } else { "" },
            if r.would_outbid { "WIN" } else { "" },
        );
        out.push_str(&format!(
            "{:<10} {:<12} {:>10}  {:>8}/{:<8}  {:>4.2}  {}\n",
            r.block_number,
            r.strategy,
            r.sim_net_wei,
            truncate_wei(&r.our_bribe_wei),
            truncate_wei(&r.winning_bid_wei),
            r.inclusion_p,
            flags.trim(),
        ));
    }
    if rows.len() > 50 {
        out.push_str(&format!("… {} more\n", rows.len() - 50));
    }
    out
}

fn truncate_wei(s: &str) -> String {
    if s.len() <= 8 {
        s.to_string()
    } else {
        format!("{}…", &s[..6])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{now_ms, Opportunity, PendingTx, RelayBlock, SimBackend, SimulationResult, Strategy, TxSource};
    use alloy_primitives::{Address, B256};

    fn seed(store: &Store) {
        let block = RelayBlock {
            relay: "https://example.invalid".into(),
            slot: 1,
            block_number: 100,
            block_hash: B256::from([1u8; 32]),
            builder: "0xbuilder".into(),
            value_wei: U256::from(1_000u64),
            gas_used: 1,
            num_tx: 1,
        };
        store.record_relay_block(&block).unwrap();

        let victim = B256::from([9u8; 32]);
        let tx = PendingTx {
            hash: victim,
            from: Some(Address::with_last_byte(1)),
            to: Some(Address::with_last_byte(2)),
            value: U256::ZERO,
            gas: 21_000,
            max_fee_per_gas: U256::ZERO,
            max_priority_fee_per_gas: U256::ZERO,
            nonce: 0,
            input: vec![0xaa, 0xbb, 0xcc, 0xdd],
            raw: None,
            source: TxSource::RelayDelivered,
            mined_at: None,
            seen_at_ms: now_ms(),
        };
        store.record_relay_block_tx(&block, &tx, 0).unwrap();

        let opp_id = "opp-tp".to_string();
        store
            .record_opportunity(&Opportunity {
                id: opp_id.clone(),
                strategy: Strategy::Sandwich,
                victim_hashes: vec![victim],
                front_calls: vec![],
                back_calls: vec![],
                flash_tokens: vec![],
                flash_amounts: vec![],
                profit_token: Address::ZERO,
                expected_profit_wei: U256::from(1u8),
                notional_wei: U256::ZERO,
                target_block: 100,
                created_at_ms: now_ms(),
                notes: String::new(),
            })
            .unwrap();
        store
            .record_simulation(&SimulationResult {
                opportunity_id: opp_id,
                strategy: Strategy::Sandwich,
                backend: SimBackend::AnvilFork,
                success: true,
                gross_profit_wei: U256::from(5_000u64),
                gas_used: 100,
                gas_price_wei: U256::from(1u8),
                gas_cost_wei: U256::from(1u8),
                bribe_wei: U256::from(2_000u64),
                net_profit_wei: 100,
                revert_reason: None,
                target_block: 100,
                sim_latency_ms: 10,
                created_at_ms: now_ms(),
            })
            .unwrap();

        // A successful sim whose victim never landed → false positive.
        store
            .record_opportunity(&Opportunity {
                id: "opp-fp".into(),
                strategy: Strategy::Sandwich,
                victim_hashes: vec![B256::from([8u8; 32])],
                front_calls: vec![],
                back_calls: vec![],
                flash_tokens: vec![],
                flash_amounts: vec![],
                profit_token: Address::ZERO,
                expected_profit_wei: U256::from(1u8),
                notional_wei: U256::ZERO,
                target_block: 100,
                created_at_ms: now_ms(),
                notes: String::new(),
            })
            .unwrap();
        store
            .record_simulation(&SimulationResult {
                opportunity_id: "opp-fp".into(),
                strategy: Strategy::Sandwich,
                backend: SimBackend::AnvilFork,
                success: true,
                gross_profit_wei: U256::from(5u8),
                gas_used: 1,
                gas_price_wei: U256::from(1u8),
                gas_cost_wei: U256::from(1u8),
                bribe_wei: U256::from(1u8),
                net_profit_wei: 1,
                revert_reason: None,
                target_block: 100,
                sim_latency_ms: 1,
                created_at_ms: now_ms(),
            })
            .unwrap();
    }

    #[test]
    fn harness_labels_true_and_false_positives() {
        let store = Store::open_in_memory().unwrap();
        seed(&store);
        let rows = compare(&store, Some(100), Some(100), 50).unwrap();
        assert_eq!(rows.len(), 2);
        let tp = rows.iter().find(|r| r.opportunity_id == "opp-tp").unwrap();
        assert!(tp.victim_landed);
        assert!(tp.true_positive);
        assert!(!tp.false_positive);
        assert!(tp.would_outbid, "bribe 2000 > winning bid 1000");

        let fp = rows.iter().find(|r| r.opportunity_id == "opp-fp").unwrap();
        assert!(!fp.victim_landed);
        assert!(fp.false_positive);

        let sum = ReplaySummary::from_rows(&rows);
        assert_eq!(sum.true_positives, 1);
        assert_eq!(sum.false_positives, 1);
        assert_eq!(sum.true_positive_rate(), 0.5);
    }

    #[test]
    fn persist_is_idempotent() {
        let store = Store::open_in_memory().unwrap();
        seed(&store);
        let rows = compare(&store, None, None, 50).unwrap();
        assert_eq!(persist(&store, &rows).unwrap(), 2);
        assert_eq!(persist(&store, &rows).unwrap(), 2);
        let rec = store.recent_reconciliations(10).unwrap();
        assert_eq!(rec.len(), 2);
    }

    #[test]
    fn empty_database_is_a_valid_replay() {
        let store = Store::open_in_memory().unwrap();
        let rows = compare(&store, None, None, 10).unwrap();
        assert!(rows.is_empty());
        let rendered = render(&rows);
        assert!(rendered.contains("no stored simulations"));
    }
}
