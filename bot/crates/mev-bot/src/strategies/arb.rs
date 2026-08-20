//! Atomic (cyclic) arbitrage across constant-product venues.
//!
//! On every new block the bot refreshes its pool cache and looks for a
//! WETH → token → WETH cycle whose two legs sit on different venues. The input
//! size is solved exactly with a ternary search over the integer AMM curve, and
//! the capital comes from a zero-fee Balancer flash loan, so the strategy needs
//! no inventory at all.
//!
//! It also runs on pending transactions: a large swap creates the imbalance we
//! want to back-run, so the same search is repeated against the *post-victim*
//! pool state.

use alloy_primitives::{Address, U256};
use alloy_sol_types::SolCall;
use async_trait::async_trait;

use crate::config::known;
use crate::dex::{self, V2Pool, Venue};
use crate::strategies::sandwich::build_leg;
use crate::strategies::{decode_swap, StrategyCtx, StrategyImpl};
use crate::types::{now_ms, BlockHead, Call, Opportunity, PendingTx, Strategy};

/// Tokens we always keep pools loaded for. Everything else is discovered from
/// mempool flow.
pub const CORE_TOKENS: [Address; 4] = [known::USDC, known::USDT, known::DAI, known::WBTC];

pub struct AtomicArbStrategy;

#[async_trait]
impl StrategyImpl for AtomicArbStrategy {
    fn kind(&self) -> Strategy {
        Strategy::AtomicArb
    }

    async fn on_block(&self, ctx: &StrategyCtx, head: &BlockHead) -> Vec<Opportunity> {
        let weth = ctx.cfg.chain.weth;

        // Make sure the core pairs are loaded on both venues.
        for token in CORE_TOKENS {
            for venue in [Venue::UniV2, Venue::SushiV2] {
                if let Some(pair) = ctx.pools.pair_for(weth, token, venue).await {
                    ctx.pools.load(pair, venue, head.number).await;
                }
            }
        }
        ctx.pools.refresh_all(head.number).await;

        let mut out = Vec::new();
        let pools = ctx.pools.all();
        for (i, a) in pools.iter().enumerate() {
            for b in pools.iter().skip(i + 1) {
                if a.venue == b.venue {
                    continue;
                }
                if let Some(opp) = try_cycle(ctx, a, b, weth, head) {
                    out.push(opp);
                }
                if let Some(opp) = try_cycle(ctx, b, a, weth, head) {
                    out.push(opp);
                }
            }
        }
        out
    }

    /// Back-run a large swap: recompute the arb against the pool state the
    /// victim will leave behind.
    async fn on_pending(&self, ctx: &StrategyCtx, tx: &PendingTx) -> Vec<Opportunity> {
        let weth = ctx.cfg.chain.weth;
        let Some(intent) = decode_swap(tx, weth) else {
            return Vec::new();
        };
        if intent.path.len() != 2 {
            return Vec::new();
        }
        let head = ctx.head();

        let Some(victim_pair) = ctx.pools.pair_for(intent.token_in, intent.token_out, Venue::UniV2).await else {
            return Vec::new();
        };
        let Some(victim_pool) = ctx.pools.load(victim_pair, Venue::UniV2, head.number).await else {
            return Vec::new();
        };
        let Some((after, _)) = victim_pool.with_swap(intent.token_in, intent.amount_in) else {
            return Vec::new();
        };

        let Some(other_pair) = ctx
            .pools
            .pair_for(intent.token_in, intent.token_out, Venue::SushiV2)
            .await
        else {
            return Vec::new();
        };
        let Some(other) = ctx.pools.load(other_pair, Venue::SushiV2, head.number).await else {
            return Vec::new();
        };

        let mut opps = Vec::new();
        for (a, b) in [(&after, &other), (&other, &after)] {
            if let Some(mut opp) = try_cycle(ctx, a, b, weth, &head) {
                opp.victim_hashes = vec![tx.hash];
                // Back-run only: nothing goes in front of the victim.
                opp.back_calls = std::mem::take(&mut opp.front_calls);
                opp.notes = format!("back-run of {:?}; {}", tx.hash, opp.notes);
                opps.push(opp);
            }
        }
        opps
    }
}

/// Try `token_in → mid → token_in` buying on `a` and selling on `b`.
fn try_cycle(ctx: &StrategyCtx, a: &V2Pool, b: &V2Pool, token_in: Address, head: &BlockHead) -> Option<Opportunity> {
    if a.other_token(token_in).is_none() {
        return None;
    }
    let mid = a.other_token(token_in)?;
    if b.other_token(mid)? != token_in {
        return None;
    }

    let (amount_in, gross) = dex::optimal_two_leg_arb(a, b, token_in, ctx.max_position())?;

    // Flash loan + two swaps + repayment ≈ 320k gas.
    let gas_estimate = U256::from(320_000u64);
    let gas_cost = gas_estimate * (head.base_fee_per_gas + U256::from(1_000_000_000u64));
    if gross <= gas_cost {
        return None;
    }

    let mid_amount = a.amount_out(token_in, amount_in)?;
    let mut calls: Vec<Call> = Vec::new();
    calls.extend(build_leg(a, token_in, mid, amount_in, ctx.executor));
    calls.extend(build_leg(b, mid, token_in, mid_amount, ctx.executor));

    Some(Opportunity {
        id: uuid::Uuid::new_v4().to_string(),
        strategy: Strategy::AtomicArb,
        victim_hashes: Vec::new(),
        front_calls: calls,
        back_calls: Vec::new(),
        flash_tokens: vec![token_in],
        flash_amounts: vec![amount_in],
        profit_token: token_in,
        expected_profit_wei: gross.saturating_sub(gas_cost),
        notional_wei: amount_in,
        target_block: head.number + ctx.cfg.sim.target_block_offset,
        created_at_ms: now_ms(),
        notes: format!(
            "arb {:?} -> {:?} ({} -> {}) in {} gross {}",
            a.address,
            b.address,
            a.venue.as_str(),
            b.venue.as_str(),
            amount_in,
            gross
        ),
    })
}

/// Balancer flash-loan repayment happens inside the executor, but the pool must
/// be approved to pull the mid token when the venue is a router-based one.
pub fn approve_call(token: Address, spender: Address) -> Call {
    Call::new(
        token,
        dex::IERC20::approveCall {
            spender,
            amount: U256::MAX,
        }
        .abi_encode(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool(venue: Venue, r0: u128, r1: u128) -> V2Pool {
        V2Pool {
            address: Address::with_last_byte(if venue == Venue::UniV2 { 1 } else { 2 }),
            token0: known::WETH,
            token1: known::USDC,
            reserve0: U256::from(r0),
            reserve1: U256::from(r1),
            fee_bps: 30,
            venue,
            block: 1,
        }
    }

    #[test]
    fn approve_encodes_max() {
        let c = approve_call(known::WETH, known::UNIV2_ROUTER);
        let d = dex::IERC20::approveCall::abi_decode(&c.data, true).unwrap();
        assert_eq!(d.amount, U256::MAX);
        assert_eq!(d.spender, known::UNIV2_ROUTER);
    }

    #[test]
    fn cycle_requires_a_price_difference() {
        let a = pool(Venue::UniV2, 1_000e18 as u128, 2_000_000e6 as u128);
        let b = pool(Venue::SushiV2, 1_000e18 as u128, 2_000_000e6 as u128);
        // identical pricing -> the fee makes every size a loss
        assert!(dex::optimal_two_leg_arb(&a, &b, known::WETH, U256::from(10u128.pow(20))).is_none());
    }

    #[test]
    fn cycle_is_found_when_pools_diverge() {
        let a = pool(Venue::UniV2, 1_000e18 as u128, 2_200_000e6 as u128);
        let b = pool(Venue::SushiV2, 1_000e18 as u128, 2_000_000e6 as u128);
        let found = dex::optimal_two_leg_arb(&a, &b, known::WETH, U256::from(10u128.pow(21)));
        assert!(found.is_some());
        let (amount, profit) = found.unwrap();
        assert!(amount > U256::ZERO && profit > U256::ZERO);
    }
}
