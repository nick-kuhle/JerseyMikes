//! Atomic (cyclic) arbitrage across venues.
//!
//! On every new block the bot refreshes its pool cache and looks for a
//! WETH-anchored cycle. The historical V2 search (`graph::search`) is always
//! run and is byte-identical to the pre-graph enumerator at `max_len = 2`.
//! `DEX_UNIV3_ARB=true` additionally probes QuoterV2 for a small V3 book and
//! searches the mixed graph; V2-only cycles from that pass are dropped so they
//! are not double-counted. Capital comes from a zero-fee Balancer flash loan.
//!
//! It also runs on pending transactions: a large swap creates the imbalance we
//! want to back-run, so the same search is repeated against the *post-victim*
//! pool state.

use alloy_primitives::{Address, U256};
use alloy_sol_types::SolCall;
use async_trait::async_trait;

use crate::dex::edge::{self, PricedCycle, PricedEdge, QuoteBook};
use crate::dex::graph::{self, CycleCandidate, DirectedEdge};
use crate::dex::{self, V2Pool, V3Pool, Venue};
use crate::strategies::sandwich::build_leg;
use crate::strategies::{decode_router, StrategyCtx, StrategyImpl};
use crate::types::{now_ms, BlockHead, Call, Opportunity, PendingTx, Strategy};

pub struct AtomicArbStrategy;

#[async_trait]
impl StrategyImpl for AtomicArbStrategy {
    fn kind(&self) -> Strategy {
        Strategy::AtomicArb
    }

    async fn on_block(&self, ctx: &StrategyCtx, head: &BlockHead) -> Vec<Opportunity> {
        let weth = ctx.cfg.chain.weth;

        // Make sure the chain's core pairs are loaded on every registered
        // V2 venue (Base has one, mainnet has two).
        for token in ctx.cfg.addresses.core_tokens() {
            for (venue, _) in ctx.cfg.addresses.pair_factories() {
                if let Some(pair) = ctx.pools.pair_for(weth, token, venue).await {
                    ctx.pools.load(pair, venue, head.number).await;
                }
            }
        }
        ctx.pools.refresh_all(head.number).await;

        // Aerodrome volatile core pools (opt-in): same seed-and-refresh
        // shape as the V2 lane, through its own cache so the fee-off-input
        // math never touches a UniV2 formula. `pool_for(_, _, false)` is the
        // volatile lane; stable pools are never seeded (work order P4).
        if ctx.cfg.dex_aerodrome_arb {
            for token in ctx.cfg.addresses.core_tokens() {
                if let Some(pool) = ctx.pools_aero.pool_for(weth, token, false).await {
                    ctx.pools_aero.load(pool, head.number).await;
                }
            }
            ctx.pools_aero.refresh_all(head.number).await;
        }

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
        // Mixed-venue (V3↔V2 / V3↔V3 / Aero↔any) search is opt-in so a
        // default mainnet boot stays byte-identical to the historical V2
        // enumerator.
        if ctx.cfg.dex_univ3_arb || ctx.cfg.dex_aerodrome_arb {
            out.extend(cross_venue_opportunities(ctx, head).await);
        }
        out
    }

    /// Back-run a large swap: recompute the arb against the pool state the
    /// victim will leave behind.
    async fn on_pending(&self, ctx: &StrategyCtx, tx: &PendingTx) -> Vec<Opportunity> {
        let weth = ctx.cfg.chain.weth;
        let Some(intent) = decode_router(
            tx,
            weth,
            ctx.cfg.decode_universal_router,
            ctx.cfg.addresses.universal_router,
        ) else {
            return Vec::new();
        };
        if intent.path.len() != 2 {
            return Vec::new();
        }
        let head = ctx.head();
        // Back-running a mined transaction means back-running the state its own
        // block started from, not the state a hundred blocks later.
        let state_block = tx.state_block(&head);

        // The back-run needs the same couple on *two* venues: buy on one,
        // sell on the other. Walk the registered V2 factories in order.
        let venues: Vec<Venue> = ctx
            .cfg
            .addresses
            .pair_factories()
            .iter()
            .map(|(v, _)| *v)
            .collect();
        if venues.len() < 2 {
            // Single-venue chains have no V2↔V2 back-run. V3 cross-venue
            // candidates are emitted on the block tick (`DEX_UNIV3_ARB`).
            return Vec::new();
        }
        let Some(victim_pair) = ctx
            .pools
            .pair_for(intent.token_in, intent.token_out, venues[0])
            .await
        else {
            return Vec::new();
        };
        let Some(victim_pool) = ctx.pool_at(victim_pair, venues[0], state_block).await else {
            return Vec::new();
        };
        let Some((after, _)) = victim_pool.with_swap(intent.token_in, intent.amount_in) else {
            return Vec::new();
        };

        let Some(other_pair) = ctx
            .pools
            .pair_for(intent.token_in, intent.token_out, venues[1])
            .await
        else {
            return Vec::new();
        };
        let Some(other) = ctx.pool_at(other_pair, venues[1], state_block).await else {
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

fn estimated_gas_cost(gas: u64, base_fee: U256, priority_fee: U256) -> U256 {
    U256::from(gas).saturating_mul(base_fee.saturating_add(priority_fee))
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
    let gas_cost = estimated_gas_cost(
        graph::gas_estimate(legs),
        head.base_fee_per_gas,
        ctx.cfg.priority_fee_wei,
    );
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
    let gas_cost = estimated_gas_cost(320_000, head.base_fee_per_gas, ctx.cfg.priority_fee_wei);
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

/// Hard ceiling on V3 pools admitted into one mixed-venue search.
const MAX_V3_POOLS: usize = 4;
/// QuoterV2 `eth_call`s spent building the V3 quote book for one block.
/// Four probe sizes × two directions × two pools, or two sizes across four.
const V3_QUOTE_BUDGET: u32 = 16;

/// Prefer WETH-quoted, actionable-fee V3 pools. Cap is a hard budget, not a hint.
fn select_v3_pools(pools: &[V3Pool], weth: Address) -> Vec<V3Pool> {
    let mut weth_quoted: Vec<V3Pool> = pools
        .iter()
        .copied()
        .filter(|p| V3Pool::is_actionable_fee(p.fee) && (p.token0 == weth || p.token1 == weth))
        .collect();
    weth_quoted.sort_by_key(|p| (p.fee, p.address));
    weth_quoted.truncate(MAX_V3_POOLS);
    weth_quoted
}

/// Probe QuoterV2 at the discrete book sizes and return the V3 lane's edges.
///
/// A book miss is `None` (never an interpolated V2 quote). Returns an empty
/// vec when the chain has no V3 router/quoter, no actionable V3 pools, or no
/// usable probe sizes — callers treat that as "no V3 lane this block".
async fn v3_probed_edges(ctx: &StrategyCtx, head: &BlockHead) -> Vec<PricedEdge> {
    let (Some(quoter), Some(router)) = (
        ctx.cfg.addresses.univ3_quoter_v2,
        ctx.cfg.addresses.univ3_swap_router_02,
    ) else {
        return Vec::new();
    };
    let weth = ctx.cfg.chain.weth;
    let v3_pools = select_v3_pools(&ctx.pools_v3.all(), weth);
    let sizes = edge::probe_sizes(ctx.max_position());
    if v3_pools.is_empty() || sizes.is_empty() {
        return Vec::new();
    }

    let hops: Vec<(V3Pool, Address, Address)> = v3_pools
        .iter()
        .flat_map(|p| [(*p, p.token0, p.token1), (*p, p.token1, p.token0)])
        .collect();
    let mut books: Vec<QuoteBook> = vec![QuoteBook::new(); hops.len()];
    let mut quotes_left = V3_QUOTE_BUDGET;
    let block_tag = ctx.block_tag(head.number);

    // Interleave sizes across hops so the budget covers the graph instead
    // of exhausting on the first pool's four-point book.
    for &size in &sizes {
        if quotes_left == 0 {
            break;
        }
        for (i, (pool, token_in, token_out)) in hops.iter().enumerate() {
            if quotes_left == 0 {
                break;
            }
            quotes_left = quotes_left.saturating_sub(1);
            match dex::quote_v3(
                &ctx.rpc, quoter, *token_in, *token_out, pool.fee, size, &block_tag,
            )
            .await
            {
                Ok(out) if !out.is_zero() => books[i].insert(size, out),
                _ => {}
            }
        }
    }

    hops.into_iter()
        .zip(books)
        .filter_map(|((pool, token_in, token_out), book)| {
            PricedEdge::v3(pool, token_in, token_out, router, book)
        })
        .collect()
}

/// Search the mixed venue graph and emit every cycle that a purely-V2 pass
/// could not have found.
///
/// Edge inventory per lane, each opt-in:
///   - V2 (always): the same pools [`graph::search`] priced this block.
///   - V3 (`DEX_UNIV3_ARB`): QuoterV2-probed books only.
///   - Aerodrome volatile (`DEX_AERODROME_ARB`): closed-form per-pool-fee
///     math, executed through the Aerodrome router.
///
/// V2-only cycles are dropped: `on_block` already emitted those from
/// [`graph::search`], and double-emitting corrupts the funnel counts.
async fn cross_venue_opportunities(ctx: &StrategyCtx, head: &BlockHead) -> Vec<Opportunity> {
    let weth = ctx.cfg.chain.weth;
    let mut edges: Vec<PricedEdge> = Vec::new();
    for p in &ctx.pools.all() {
        edges.extend(PricedEdge::from_v2(p));
    }

    if ctx.cfg.dex_univ3_arb {
        edges.extend(v3_probed_edges(ctx, head).await);
    }
    if ctx.cfg.dex_aerodrome_arb {
        // Router and factory are a deployment pair (`router.defaultFactory()`
        // returned this factory on Base); with either missing there is no
        // Aero lane — fail closed, never guess an address.
        if let (Some(router), Some(factory)) = (
            ctx.cfg.addresses.aerodrome_router,
            ctx.cfg.addresses.aerodrome_factory,
        ) {
            for p in ctx.pools_aero.all_volatile() {
                edges.extend(PricedEdge::from_aero(&p, router, factory));
            }
        }
    }

    // Without at least one non-V2 edge every cycle here would be a duplicate
    // of the byte-pinned V2 pass.
    if !edges.iter().any(|e| !e.is_v2()) {
        return Vec::new();
    }

    let found = edge::search_priced(
        &edges,
        weth,
        ctx.max_position(),
        ctx.cfg.arb_max_cycle_len,
        ctx.cfg.arb_enumeration_budget,
    );
    found
        .into_iter()
        .filter(|c| c.uses_non_v2(&edges))
        .filter_map(|c| build_priced_opportunity(ctx, &c, &edges, head))
        .collect()
}

/// Turn a sized mixed-venue cycle into an `Opportunity`.
fn build_priced_opportunity(
    ctx: &StrategyCtx,
    candidate: &PricedCycle,
    edges: &[PricedEdge],
    head: &BlockHead,
) -> Option<Opportunity> {
    let legs = candidate.legs();
    let gas_units = candidate
        .edges
        .iter()
        .filter_map(|&i| edges.get(i).map(|e| e.gas))
        .sum::<u64>()
        .max(graph::gas_estimate(legs));
    let gas_cost = estimated_gas_cost(gas_units, head.base_fee_per_gas, ctx.cfg.priority_fee_wei);
    if candidate.gross_profit <= gas_cost {
        return None;
    }
    let calls = candidate.build_calls(edges, ctx.executor)?;
    Some(Opportunity {
        id: uuid::Uuid::new_v4().to_string(),
        strategy: Strategy::AtomicArb,
        victim_hashes: Vec::new(),
        front_calls: calls,
        back_calls: Vec::new(),
        flash_tokens: vec![candidate.anchor],
        flash_amounts: vec![candidate.amount_in],
        profit_token: candidate.anchor,
        expected_profit_wei: candidate.gross_profit.saturating_sub(gas_cost),
        notional_wei: candidate.amount_in,
        target_block: head.number + ctx.cfg.sim.target_block_offset,
        created_at_ms: now_ms(),
        notes: format!(
            "arb {legs}-leg [{}] in {} gross {} (mixed)",
            candidate.route_label(edges),
            candidate.amount_in,
            candidate.gross_profit
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
    use crate::config::known;

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
    fn configured_priority_fee_changes_the_prefilter_cost() {
        let base = U256::from(10u64);
        assert_eq!(
            estimated_gas_cost(100, base, U256::from(1u64)),
            U256::from(1_100u64)
        );
        assert_eq!(
            estimated_gas_cost(100, base, U256::from(5u64)),
            U256::from(1_500u64)
        );
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

    fn v3_meta(addr: u8, fee: u32) -> V3Pool {
        V3Pool {
            address: Address::with_last_byte(addr),
            token0: known::WETH,
            token1: known::USDC,
            fee,
            tick_spacing: 60,
            block: 1,
        }
    }

    #[test]
    fn select_v3_pools_keeps_weth_actionable_and_caps() {
        let pools = vec![
            v3_meta(1, 3_000),
            v3_meta(2, 100), // 1 bp — not actionable
            V3Pool {
                address: Address::with_last_byte(3),
                token0: known::USDC,
                token1: known::USDT,
                fee: 500,
                tick_spacing: 10,
                block: 1,
            },
            v3_meta(4, 500),
            v3_meta(5, 10_000),
            v3_meta(6, 500),
            v3_meta(7, 3_000),
        ];
        let got = select_v3_pools(&pools, known::WETH);
        assert!(got.iter().all(|p| V3Pool::is_actionable_fee(p.fee)));
        assert!(got
            .iter()
            .all(|p| p.token0 == known::WETH || p.token1 == known::WETH));
        assert_eq!(got.len(), MAX_V3_POOLS);
        assert!(got[0].fee <= got[got.len() - 1].fee);
    }

    #[test]
    fn mixed_search_drops_v2_only_cycles() {
        // Same two V2 pools the historical search prices, plus an empty V3
        // book that never becomes an edge. The V3 path must not re-emit the
        // V2 cycle — that would double-count against graph::search.
        let a = pool(Venue::UniV2, 1_000e18 as u128, 2_200_000e6 as u128);
        let b = pool(Venue::SushiV2, 1_000e18 as u128, 2_000_000e6 as u128);
        let mut edges = PricedEdge::from_v2(&a);
        edges.extend(PricedEdge::from_v2(&b));
        let found = edge::search_priced(
            &edges,
            known::WETH,
            U256::from(10u128.pow(21)),
            2,
            std::time::Duration::from_secs(1),
        );
        assert!(
            !found.is_empty(),
            "V2 cycle still exists on the priced graph"
        );
        assert!(
            found.iter().all(|c| !c.uses_v3(&edges)),
            "without a V3 book every cycle is V2-only and must be filtered"
        );
    }

    #[test]
    fn mixed_search_drops_v2_only_cycles_but_keeps_aero_cross() {
        // Two identical-price V2 pools (no V2-only profit, and even if there
        // were it must not be double-emitted) plus an Aerodrome pool priced
        // far enough off to make a WETH→USDC→WETH cross-venue cycle.
        let uni = pool(Venue::UniV2, 1_000e18 as u128, 2_000_000e6 as u128);
        let sushi = pool(Venue::SushiV2, 1_000e18 as u128, 2_000_000e6 as u128);
        let aero = crate::dex::AeroPool {
            address: Address::with_last_byte(9),
            token0: known::WETH,
            token1: known::USDC,
            reserve0: U256::from(1_000e18 as u128),
            reserve1: U256::from(2_200_000e6 as u128),
            fee_bps: 30,
            stable: false,
            block: 1,
        };
        let router = known::BASE_AERODROME_ROUTER;
        let factory = known::BASE_AERODROME_FACTORY;

        let mut edges = PricedEdge::from_v2(&uni);
        edges.extend(PricedEdge::from_v2(&sushi));
        edges.extend(PricedEdge::from_aero(&aero, router, factory));

        let found = edge::search_priced(
            &edges,
            known::WETH,
            U256::from(10u128.pow(21)),
            2,
            std::time::Duration::from_secs(1),
        );
        let cross: Vec<_> = found.iter().filter(|c| c.uses_non_v2(&edges)).collect();
        assert!(
            !cross.is_empty(),
            "the V2↔Aero cycle survives the same drop-V2-only filter the V3 lane uses"
        );
        assert!(
            cross
                .iter()
                .all(|c| c.edges.iter().any(|&i| edges[i].venue == Venue::AeroVolatile)),
            "every kept cycle actually touches Aerodrome"
        );
        // And the callee chain is executable: the Aero leg approves the
        // Aerodrome router, never a UniV2 pair swap.
        let calls = cross[0]
            .build_calls(&edges, Address::with_last_byte(0xee))
            .expect("executable");
        assert!(calls.iter().any(|c| c.target == router));
        assert!(
            calls
                .iter()
                .any(|c| c.data[..4] == dex::IAerodromeRouter::swapExactTokensForTokensCall::SELECTOR),
            "a swapExactTokensForTokens call is present"
        );
    }

    #[test]
    fn a_quoted_v3_book_plus_v2_produces_a_v3_cycle() {
        let v2 = pool(Venue::UniV2, 1_000e18 as u128, 1_800_000e6 as u128);
        let size = U256::from(10u128.pow(19));
        let v2_out = v2.amount_out(known::WETH, size).unwrap();
        let generous = v2_out + v2_out / U256::from(20u64);
        let mut book = QuoteBook::new();
        book.insert(size, generous);
        let mut back = QuoteBook::new();
        back.insert(generous, size / U256::from(2u64));

        let mut edges = PricedEdge::from_v2(&v2);
        edges.push(
            PricedEdge::v3(
                v3_meta(9, 500),
                known::WETH,
                known::USDC,
                known::UNIV3_SWAP_ROUTER_02,
                book,
            )
            .unwrap(),
        );
        edges.push(
            PricedEdge::v3(
                v3_meta(9, 500),
                known::USDC,
                known::WETH,
                known::UNIV3_SWAP_ROUTER_02,
                back,
            )
            .unwrap(),
        );

        let found = edge::search_priced(
            &edges,
            known::WETH,
            size,
            2,
            std::time::Duration::from_secs(1),
        );
        let cross = found
            .iter()
            .find(|c| c.uses_v3(&edges))
            .expect("V3↔V2 cycle survives the uses_v3 filter");
        assert_eq!(cross.amount_in, size);
        assert!(cross.gross_profit > U256::ZERO);
        let calls = cross
            .build_calls(&edges, Address::with_last_byte(0xee))
            .expect("executable");
        assert!(calls
            .iter()
            .any(|c| c.target == known::UNIV3_SWAP_ROUTER_02));
    }
}
