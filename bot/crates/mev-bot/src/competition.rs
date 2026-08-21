//! Competition model: rank our bundle against the block's realised builder
//! payment to estimate true inclusion probability.
//!
//! The relay's `proposer_payload_delivered.value` is what the winning builder
//! actually paid the proposer — the market-clearing price of that block's MEV.
//! Our bribe is `bribe_bps` of simulated gross profit, paid via coinbase
//! transfer. Builders sort by payment, so the ratio of the two is the only
//! number that answers "would we have landed?".

use alloy_primitives::U256;
use serde_json::{json, Value};

/// Logistic steepness. Chosen so that matching the winning bid is p = 0.5
/// (we might or might not have won the race), 2× the bid is ~0.88, and
/// half the bid is ~0.12. The model is deliberately simple: it is a ranking
/// against a realised price, not a simulation of the builder's full auction.
const LOGISTIC_K: f64 = 2.2;

#[derive(Clone, Debug, PartialEq)]
pub struct Competition {
    pub our_bribe_wei: U256,
    pub winning_bid_wei: U256,
    /// `our_bribe > winning_bid`. Strict: matching the clearing price is a
    /// coin-flip, not a win.
    pub would_outbid: bool,
    /// Logistic of `our_bribe / winning_bid`, centred at 1.0.
    pub inclusion_p: f64,
}

impl Competition {
    pub fn rank(our_bribe: U256, winning_bid: U256) -> Self {
        Self {
            our_bribe_wei: our_bribe,
            winning_bid_wei: winning_bid,
            would_outbid: our_bribe > winning_bid,
            inclusion_p: inclusion_probability(our_bribe, winning_bid),
        }
    }

    pub fn to_json(&self) -> Value {
        json!({
            "ourBribeWei": self.our_bribe_wei.to_string(),
            "winningBidWei": self.winning_bid_wei.to_string(),
            "wouldOutbid": self.would_outbid,
            "inclusionP": self.inclusion_p,
        })
    }
}

/// Inclusion probability in `[0, 1]`.
///
/// * Both zero → 0 (nothing to bid with, nothing to beat).
/// * Winning bid zero, we bid something → 1 (we would have been the only bid).
/// * Otherwise the logistic of the ratio, centred at equality.
pub fn inclusion_probability(our_bribe: U256, winning_bid: U256) -> f64 {
    if winning_bid.is_zero() {
        return if our_bribe.is_zero() { 0.0 } else { 1.0 };
    }
    if our_bribe.is_zero() {
        return 0.0;
    }
    let ours = u256_as_f64(our_bribe);
    let win = u256_as_f64(winning_bid);
    if !ours.is_finite() || !win.is_finite() || win == 0.0 {
        return 0.0;
    }
    let x = -LOGISTIC_K * (ours / win - 1.0);
    let p = 1.0 / (1.0 + x.exp());
    if p.is_finite() {
        p.clamp(0.0, 1.0)
    } else if ours > win {
        1.0
    } else {
        0.0
    }
}

fn u256_as_f64(v: U256) -> f64 {
    // Ratios of similar-magnitude wei values survive f64's mantissa. We never
    // use the absolute value as money.
    v.to_string().parse::<f64>().unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(n: u64) -> U256 {
        U256::from(n)
    }

    #[test]
    fn matching_the_clearing_price_is_a_coin_flip() {
        let p = inclusion_probability(w(1_000), w(1_000));
        assert!((p - 0.5).abs() < 1e-9, "p={p}");
        let c = Competition::rank(w(1_000), w(1_000));
        assert!(!c.would_outbid, "equality is not an outbid");
    }

    #[test]
    fn doubling_the_bid_is_likely_inclusion() {
        let p = inclusion_probability(w(2_000), w(1_000));
        assert!(p > 0.85 && p < 0.95, "p={p}");
        assert!(Competition::rank(w(2_000), w(1_000)).would_outbid);
    }

    #[test]
    fn half_the_bid_is_unlikely() {
        let p = inclusion_probability(w(500), w(1_000));
        assert!(p > 0.05 && p < 0.20, "p={p}");
        assert!(!Competition::rank(w(500), w(1_000)).would_outbid);
    }

    #[test]
    fn zeros_are_defined() {
        assert_eq!(inclusion_probability(U256::ZERO, U256::ZERO), 0.0);
        assert_eq!(inclusion_probability(w(1), U256::ZERO), 1.0);
        assert_eq!(inclusion_probability(U256::ZERO, w(1)), 0.0);
    }

    #[test]
    fn ranking_is_monotonic_in_our_bribe() {
        let win = w(10_000);
        let mut last = 0.0;
        for ours in [1u64, 100, 1_000, 5_000, 10_000, 20_000, 100_000] {
            let p = inclusion_probability(w(ours), win);
            assert!(p >= last, "p dropped from {last} to {p} at bribe {ours}");
            last = p;
        }
    }

    #[test]
    fn json_round_trips_the_fields_the_dashboard_reads() {
        let c = Competition::rank(w(3), w(2));
        let v = c.to_json();
        assert_eq!(v["ourBribeWei"], "3");
        assert_eq!(v["winningBidWei"], "2");
        assert_eq!(v["wouldOutbid"], true);
        assert!(v["inclusionP"].as_f64().unwrap() > 0.5);
    }
}
