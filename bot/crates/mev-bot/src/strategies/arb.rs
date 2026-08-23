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
use crate::dex::graph::{self, CycleCandidate, DirectedEdge};
use crate::dex::{self, V2Pool, Venue};
use crate::strategies::sandwich::build_leg;
use crate::strategies::{decode_router, StrategyCtx, StrategyImpl};
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

        // Cycle search over the whole cached graph. With `arb_max_cycle_len`
        // at its default of 2 this reproduces the original pair-to-pair scan;
        // raising it adds longer cycles through the pools discovery brings in.
        let pools = ctx.pools.all();
        // Both search budgets come from config now: the wall-clock deadline
        // and the graph width. Defaults reproduce the previous hard-coded
        // 25 ms / 200 pools exactly.
        let (selected, edges, candidates) = graph::search_with(
            &pools,
            weth,
            ctx.max_position(),
            ctx.cfg.arb_max_cycle_len,
            ctx.cfg.arb_enumeration_budget,
            ctx.cfg.arb_max_pools,
        );

        let mut out = Vec::new();
        for candidate in candidates {
            if let Some(opp) = build_cycle_opportunity(ctx, &candidate, &edges, &selected, head) {
                out.push(opp);
            }
        }
        out
    }

    /// Back-run a large swap: recompute the arb against the pool state the
    /// victim will leave behind.
    async fn on_pending(&self, ctx: &StrategyCtx, tx: &PendingTx) -> Vec<Opportunity> {
        let weth = ctx.cfg.chain.weth;
        let Some(intent) = decode_router(tx, weth, ctx.cfg.decode_universal_router) else {
            return Vec::new();
        };
        if intent.path.len() != 2 {
            return Vec::new();
        }
        let head = ctx.head();
        // Back-running a mined transaction means back-running the state its own
        // block started from, not the state a hundred blocks later.
        let state_block = tx.state_block(&head);

        let Some(victim_pair) = ctx
            .pools
            .pair_for(intent.token_in, intent.token_out, Venue::UniV2)
            .await
        else {
            return Vec::new();
        };
        let Some(victim_pool) = ctx.pool_at(victim_pair, Venue::UniV2, state_block).await else {
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
        let Some(other) = ctx.pool_at(other_pair, Venue::SushiV2, state_block).await else {
            return Vec::new();
        };

        // `try_cycle` costs gas and stamps the target block from this head; for a
        // replay that must be the victim's own block, at its own base fee.
        let mut ctx_head = head.clone();
        ctx_head.number = tx
            .target_block(&head, ctx.cfg.sim.target_block_offset)
            .saturating_sub(ctx.cfg.sim.target_block_offset);
        ctx_head.base_fee_per_gas = tx.base_fee(&head);

        let mut opps = Vec::new();
        for (a, b) in [(&after, &other), (&other, &after)] {
            if let Some(mut opp) = try_cycle(ctx, a, b, weth, &ctx_head) {
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

/// Turn a sized cycle into an `Opportunity`, or drop it if gas eats the profit.
///
/// Profit is denominated in the anchor token, which the search guarantees is
/// WETH — that is what makes comparing it against a wei-denominated gas cost
/// legitimate.
fn build_cycle_opportunity(
    ctx: &StrategyCtx,
    candidate: &CycleCandidate,
    edges: &[DirectedEdge],
    pools: &[V2Pool],
    head: &BlockHead,
) -> Option<Opportunity> {
    let legs = candidate.cycle.legs();
    let gas_estimate = U256::from(graph::gas_estimate(legs));
    let gas_cost = gas_estimate * (head.base_fee_per_gas + U256::from(1_000_000_000u64));
    if candidate.gross_profit <= gas_cost {
        return None;
    }

    // Walk the legs, threading each leg's output into the next leg's input.
    let mut calls: Vec<Call> = Vec::new();
    let mut amount = candidate.amount_in;
    let mut route: Vec<String> = Vec::with_capacity(legs);
    for &e in &candidate.cycle.edges {
        let edge = edges.get(e)?;
        let pool = pools.get(edge.pool)?;
        calls.extend(build_leg(
            pool,
            edge.token_in,
            edge.token_out,
            amount,
            ctx.executor,
        ));
        route.push(format!("{}:{:?}", pool.venue.as_str(), pool.address));
        amount = pool.amount_out(edge.token_in, amount)?;
    }

    Some(Opportunity {
        id: uuid::Uuid::new_v4().to_string(),
        strategy: Strategy::AtomicArb,
        victim_hashes: Vec::new(),
        front_calls: calls,
        back_calls: Vec::new(),
        flash_tokens: vec![candidate.cycle.anchor],
        flash_amounts: vec![candidate.amount_in],
        profit_token: candidate.cycle.anchor,
        expected_profit_wei: candidate.gross_profit.saturating_sub(gas_cost),
        notional_wei: candidate.amount_in,
        target_block: head.number + ctx.cfg.sim.target_block_offset,
        created_at_ms: now_ms(),
        notes: format!(
            "arb {legs}-leg [{}] in {} gross {}",
            route.join(" -> "),
            candidate.amount_in,
            candidate.gross_profit
        ),
    })
}

/// Try `token_in → mid → token_in` buying on `a` and selling on `b`.
fn try_cycle(
    ctx: &StrategyCtx,
    a: &V2Pool,
    b: &V2Pool,
    token_in: Address,
    head: &BlockHead,
) -> Option<Opportunity> {
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
        assert!(
            dex::optimal_two_leg_arb(&a, &b, known::WETH, U256::from(10u128.pow(20))).is_none()
        );
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

    #[test]
    fn cycle_search_reproduces_the_original_two_leg_result() {
        // The multi-leg search replaced a hand-rolled pair-to-pair loop. At
        // `max_len = 2` it must find exactly what that loop found, sized
        // identically — otherwise the "superset" claim is wrong and the
        // funnel comparison across the change is meaningless.
        let a = pool(Venue::UniV2, 1_000e18 as u128, 2_200_000e6 as u128);
        let b = pool(Venue::SushiV2, 1_000e18 as u128, 2_000_000e6 as u128);
        let max_in = U256::from(10u128.pow(21));

        let (want_amount, want_profit) =
            dex::optimal_two_leg_arb(&a, &b, known::WETH, max_in).expect("baseline cycle exists");

        let (_, _, found) = graph::search(
            &[a, b],
            known::WETH,
            max_in,
            2,
            std::time::Duration::from_secs(1),
        );
        let best = found.first().expect("graph search finds the same cycle");
        assert_eq!(best.cycle.legs(), 2);
        assert_eq!(best.amount_in, want_amount);
        assert_eq!(best.gross_profit, want_profit);
    }

    #[test]
    fn raising_the_leg_cap_never_loses_the_two_leg_cycle() {
        // Superset property: whatever a 2-leg search finds must still be found
        // when longer cycles are allowed to compete.
        let a = pool(Venue::UniV2, 1_000e18 as u128, 2_200_000e6 as u128);
        let b = pool(Venue::SushiV2, 1_000e18 as u128, 2_000_000e6 as u128);
        let max_in = U256::from(10u128.pow(21));
        let budget = std::time::Duration::from_secs(1);

        let (_, _, two) = graph::search(&[a, b], known::WETH, max_in, 2, budget);
        let (_, _, five) = graph::search(&[a, b], known::WETH, max_in, 5, budget);

        assert!(!two.is_empty());
        assert!(five.len() >= two.len());
        assert_eq!(five[0].gross_profit, two[0].gross_profit);
    }
}
