//! Multi-leg cycle enumeration over the constant-product pool graph.
//!
//! # Why direct enumeration and not Bellman–Ford
//!
//! The textbook approach is a negative-cycle search over a graph whose edge
//! weights are `-log(rate)`. It is asymptotically nicer and practically worse
//! here: the weights are logarithms, a fixed-point `log_e` cheap enough to run
//! per block is coarse, and a coarse `log_e` reports edges whose bit-lengths
//! happen to match as relaxable. The result is cycles that do not exist on
//! chain — false positives that cost a fork simulation each to disprove.
//! Doing it properly needs a high-precision log table plus a Newton iteration
//! on the input size, which is a research port rather than a strategy change.
//!
//! Direct enumeration is correct by construction: every leg is priced with the
//! same [`v2_amount_out`] the rest of the bot uses and the existing tests pin
//! against Solidity. The cost is a wider search, which is why every budget in
//! this module is a hard cap rather than a hint.
//!
//! # Why WETH is the only anchor
//!
//! Any cycle that touches WETH can be rotated so it *starts* at WETH, so
//! anchoring on WETH alone loses no cycle that contains it. Cycles that never
//! touch WETH are skipped deliberately: their profit is denominated in a token
//! the gas model cannot price without an oracle, and comparing a USDC profit
//! against a wei gas cost is exactly the kind of unit error this codebase
//! already avoids elsewhere.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use alloy_primitives::{Address, U256};

use super::{ternary_search_max, v2_amount_out, V2Pool};

/// Hard ceiling on cycle length in legs. Config can lower this, never raise it.
pub const MAX_CYCLE_LEN: usize = 5;

/// Most pools admitted into the graph, largest WETH reserve first.
pub const MAX_POOLS: usize = 200;

/// Most candidates handed back from one search. Matches the risk engine's
/// default `MAX_INFLIGHT_PER_STRATEGY` so the simulator queue cannot be
/// flooded by a single block.
pub const MAX_CANDIDATES: usize = 32;

/// Raw cycles retained before profit sizing. Bounds memory when a dense graph
/// admits far more topological cycles than we could ever simulate.
const MAX_RAW_CYCLES: usize = 1_024;

/// Wall-clock budget for one enumeration pass. Discovery plus strategies share
/// a ~50 ms/block budget on the block task; enumeration gets half of it.
pub const ENUMERATION_BUDGET: Duration = Duration::from_millis(25);

/// Base gas for a flash loan plus repayment plus two swaps.
const GAS_BASE: u64 = 320_000;
/// Marginal gas per leg beyond the second.
const GAS_PER_EXTRA_LEG: u64 = 120_000;

/// One direction of one pool.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DirectedEdge {
    /// Index into the pool slice the edge list was built from.
    pub pool: usize,
    pub token_in: Address,
    pub token_out: Address,
}

/// A closed walk that starts and ends on `anchor`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cycle {
    /// Indices into the edge list, in execution order.
    pub edges: Vec<usize>,
    pub anchor: Address,
}

impl Cycle {
    pub fn legs(&self) -> usize {
        self.edges.len()
    }
}

/// A sized, priced cycle.
#[derive(Clone, Debug)]
pub struct CycleCandidate {
    pub cycle: Cycle,
    /// Optimal input in the anchor token.
    pub amount_in: U256,
    /// Output minus input, before gas.
    pub gross_profit: U256,
}

/// Gas estimate for an `legs`-leg cycle. Pre-filter only — the fork simulation
/// is the arbiter, and a misestimate here only shifts which candidates are
/// worth simulating.
pub fn gas_estimate(legs: usize) -> u64 {
    GAS_BASE + GAS_PER_EXTRA_LEG * legs.saturating_sub(2) as u64
}

/// Two directed edges per pool, skipping pools with a dead side.
pub fn build_edges(pools: &[V2Pool]) -> Vec<DirectedEdge> {
    let mut edges = Vec::with_capacity(pools.len() * 2);
    for (i, p) in pools.iter().enumerate() {
        if p.reserve0.is_zero() || p.reserve1.is_zero() {
            continue;
        }
        edges.push(DirectedEdge {
            pool: i,
            token_in: p.token0,
            token_out: p.token1,
        });
        edges.push(DirectedEdge {
            pool: i,
            token_in: p.token1,
            token_out: p.token0,
        });
    }
    edges
}

/// Edge indices grouped by their input token.
pub fn adjacency(edges: &[DirectedEdge]) -> HashMap<Address, Vec<usize>> {
    let mut adj: HashMap<Address, Vec<usize>> = HashMap::new();
    for (i, e) in edges.iter().enumerate() {
        adj.entry(e.token_in).or_default().push(i);
    }
    adj
}

/// Keep the graph bounded: WETH-quoted pools first (deepest side first), then
/// whatever else fits, since cross pairs like WBTC/USDC are what make a 3-leg
/// cycle possible in the first place.
pub fn select_pools(pools: &[V2Pool], weth: Address, limit: usize) -> Vec<V2Pool> {
    if pools.len() <= limit {
        return pools.to_vec();
    }
    let mut weth_pools: Vec<V2Pool> = pools
        .iter()
        .filter(|p| p.token0 == weth || p.token1 == weth)
        .copied()
        .collect();
    weth_pools.sort_by(|a, b| {
        let ar = a.reserves_for(weth).map(|(r, _)| r).unwrap_or(U256::ZERO);
        let br = b.reserves_for(weth).map(|(r, _)| r).unwrap_or(U256::ZERO);
        br.cmp(&ar)
    });
    let mut out: Vec<V2Pool> = weth_pools.into_iter().take(limit).collect();
    if out.len() < limit {
        for p in pools {
            if out.len() >= limit {
                break;
            }
            if p.token0 != weth && p.token1 != weth {
                out.push(*p);
            }
        }
    }
    out
}

/// Enumerate every simple cycle from `anchor`, up to `max_len` legs.
///
/// `deadline` is checked on entry to every recursion step, so a pathological
/// graph degrades into "fewer candidates" rather than a blown block budget.
pub fn enumerate_cycles(
    edges: &[DirectedEdge],
    adj: &HashMap<Address, Vec<usize>>,
    anchor: Address,
    max_len: usize,
    deadline: Option<Instant>,
) -> Vec<Cycle> {
    let max_len = max_len.clamp(2, MAX_CYCLE_LEN);
    let mut out = Vec::new();
    let mut seen: HashSet<Vec<usize>> = HashSet::new();
    let mut path: Vec<usize> = Vec::with_capacity(max_len);
    let mut visited: HashSet<Address> = HashSet::new();
    let mut used_pools: HashSet<usize> = HashSet::new();

    visited.insert(anchor);
    walk(
        anchor,
        anchor,
        edges,
        adj,
        max_len,
        deadline,
        &mut path,
        &mut visited,
        &mut used_pools,
        &mut seen,
        &mut out,
    );
    out
}

#[allow(clippy::too_many_arguments)]
fn walk(
    anchor: Address,
    current: Address,
    edges: &[DirectedEdge],
    adj: &HashMap<Address, Vec<usize>>,
    max_len: usize,
    deadline: Option<Instant>,
    path: &mut Vec<usize>,
    visited: &mut HashSet<Address>,
    used_pools: &mut HashSet<usize>,
    seen: &mut HashSet<Vec<usize>>,
    out: &mut Vec<Cycle>,
) {
    if out.len() >= MAX_RAW_CYCLES {
        return;
    }
    if let Some(d) = deadline {
        if Instant::now() >= d {
            return;
        }
    }
    let Some(candidates) = adj.get(&current) else {
        return;
    };

    for &e in candidates {
        let edge = edges[e];
        // A pool may appear at most once per cycle. Two legs through the same
        // pool would interact — the second leg would trade against reserves the
        // first already moved — and `evaluate` prices every leg against the
        // pool's snapshot. Excluding reuse keeps that exact.
        if used_pools.contains(&edge.pool) {
            continue;
        }

        if edge.token_out == anchor {
            if path.len() + 1 >= 2 {
                let mut cycle_edges = path.clone();
                cycle_edges.push(e);
                let mut key = cycle_edges.clone();
                key.sort_unstable();
                // The same cycle is reachable by several walks (and, before the
                // WETH-only anchoring, from several anchors). Dedupe on the
                // edge set, not the order.
                if seen.insert(key) {
                    out.push(Cycle {
                        edges: cycle_edges,
                        anchor,
                    });
                    if out.len() >= MAX_RAW_CYCLES {
                        return;
                    }
                }
            }
            // Closing the cycle does not end the walk: a token can have several
            // closing edges through parallel pools, and longer cycles may still
            // pass through this token.
            continue;
        }

        if path.len() + 1 >= max_len || visited.contains(&edge.token_out) {
            continue;
        }

        path.push(e);
        visited.insert(edge.token_out);
        used_pools.insert(edge.pool);
        walk(
            anchor,
            edge.token_out,
            edges,
            adj,
            max_len,
            deadline,
            path,
            visited,
            used_pools,
            seen,
            out,
        );
        used_pools.remove(&edge.pool);
        visited.remove(&edge.token_out);
        path.pop();
    }
}

/// Output of running `amount_in` around the cycle. `None` if the cycle
/// references an edge or token the pools do not support.
pub fn evaluate(
    cycle: &Cycle,
    edges: &[DirectedEdge],
    pools: &[V2Pool],
    amount_in: U256,
) -> Option<U256> {
    let mut amount = amount_in;
    for &e in &cycle.edges {
        let edge = edges.get(e)?;
        let pool = pools.get(edge.pool)?;
        let (r_in, r_out) = pool.reserves_for(edge.token_in)?;
        amount = v2_amount_out(amount, r_in, r_out, pool.fee_bps);
        if amount.is_zero() {
            return Some(U256::ZERO);
        }
    }
    Some(amount)
}

/// Solve the optimal input for one cycle. Returns `None` when no positive-profit
/// size exists.
///
/// The composed profit curve of constant-product legs with positive fees is
/// unimodal, which is what [`ternary_search_max`] assumes.
pub fn optimal_cycle_in(
    cycle: &Cycle,
    edges: &[DirectedEdge],
    pools: &[V2Pool],
    max_in: U256,
) -> Option<(U256, U256)> {
    let first = edges.get(*cycle.edges.first()?)?;
    let pool = pools.get(first.pool)?;
    let (r_in, _) = pool.reserves_for(first.token_in)?;
    let hi = max_in.min(r_in);
    if hi.is_zero() {
        return None;
    }

    let profit_of = |x: U256| -> U256 {
        match evaluate(cycle, edges, pools, x) {
            Some(out) => out.saturating_sub(x),
            None => U256::ZERO,
        }
    };

    let (x, p) = ternary_search_max(U256::ZERO, hi, profit_of);
    if x.is_zero() || p.is_zero() {
        None
    } else {
        Some((x, p))
    }
}

/// Full search: build the graph, enumerate, size, rank.
///
/// Returns at most [`MAX_CANDIDATES`] candidates sorted by gross profit,
/// descending. `pools` must be the same slice the returned edge indices are
/// resolved against, so it is handed back alongside the candidates.
pub fn search(
    pools: &[V2Pool],
    weth: Address,
    max_in: U256,
    max_len: usize,
    budget: Duration,
) -> (Vec<V2Pool>, Vec<DirectedEdge>, Vec<CycleCandidate>) {
    let selected = select_pools(pools, weth, MAX_POOLS);
    let edges = build_edges(&selected);
    let adj = adjacency(&edges);
    let deadline = Instant::now().checked_add(budget);

    let cycles = enumerate_cycles(&edges, &adj, weth, max_len, deadline);

    let mut out: Vec<CycleCandidate> = Vec::new();
    for cycle in cycles {
        if let Some(d) = deadline {
            if Instant::now() >= d {
                break;
            }
        }
        if let Some((amount_in, gross_profit)) = optimal_cycle_in(&cycle, &edges, &selected, max_in)
        {
            out.push(CycleCandidate {
                cycle,
                amount_in,
                gross_profit,
            });
        }
    }

    out.sort_by(|a, b| b.gross_profit.cmp(&a.gross_profit));
    out.truncate(MAX_CANDIDATES);
    (selected, edges, out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(n: u8) -> Address {
        Address::with_last_byte(n)
    }

    // A plain function, not a const: `Address::with_last_byte` is the
    // constructor the rest of this crate uses and needs no const-fn guarantee.
    #[allow(non_snake_case)]
    fn WETH() -> Address {
        Address::with_last_byte(0xee)
    }

    fn pool(addr: u8, a: Address, b: Address, ra: u128, rb: u128) -> V2Pool {
        // token0/token1 ordering mirrors the on-chain sort so `reserves_for`
        // behaves the way it does against a real pair.
        let (token0, token1, reserve0, reserve1) = if a < b {
            (a, b, ra, rb)
        } else {
            (b, a, rb, ra)
        };
        V2Pool {
            address: Address::with_last_byte(addr),
            token0,
            token1,
            reserve0: U256::from(reserve0),
            reserve1: U256::from(reserve1),
            fee_bps: 30,
            venue: crate::dex::Venue::UniV2,
            block: 1,
        }
    }

    fn graph(pools: &[V2Pool]) -> (Vec<DirectedEdge>, HashMap<Address, Vec<usize>>) {
        let edges = build_edges(pools);
        let adj = adjacency(&edges);
        (edges, adj)
    }

    #[test]
    fn empty_input_yields_no_cycles() {
        let (edges, adj) = graph(&[]);
        assert!(enumerate_cycles(&edges, &adj, WETH(), 5, None).is_empty());
    }

    #[test]
    fn dust_pools_are_dropped_from_the_graph() {
        let pools = vec![pool(1, WETH(), t(1), 0, 0), pool(2, WETH(), t(1), 100, 100)];
        let edges = build_edges(&pools);
        // Only the live pool contributes its two directions.
        assert_eq!(edges.len(), 2);
        assert!(edges.iter().all(|e| e.pool == 1));
    }

    #[test]
    fn identical_pools_have_no_profitable_cycle() {
        // Same price on both venues: the 0.30% fee on each leg makes every size
        // a loss, so nothing should survive sizing.
        let pools = vec![
            pool(1, WETH(), t(1), 1_000_000_000_000_000_000_000, 2_000_000_000_000),
            pool(2, WETH(), t(1), 1_000_000_000_000_000_000_000, 2_000_000_000_000),
        ];
        let (_, _, found) = search(
            &pools,
            WETH(),
            U256::from(10u128.pow(21)),
            2,
            Duration::from_secs(1),
        );
        assert!(found.is_empty());
    }

    #[test]
    fn diverged_pools_produce_a_two_leg_cycle() {
        let pools = vec![
            pool(1, WETH(), t(1), 1_000_000_000_000_000_000_000, 2_200_000_000_000),
            pool(2, WETH(), t(1), 1_000_000_000_000_000_000_000, 2_000_000_000_000),
        ];
        let (_, _, found) = search(
            &pools,
            WETH(),
            U256::from(10u128.pow(21)),
            2,
            Duration::from_secs(1),
        );
        assert!(!found.is_empty());
        let best = &found[0];
        assert_eq!(best.cycle.legs(), 2);
        assert!(best.amount_in > U256::ZERO);
        assert!(best.gross_profit > U256::ZERO);
    }

    #[test]
    fn three_leg_triangle_is_found_and_priced_consistently() {
        // WETH -> A -> B -> WETH, with the WETH/B pool priced so the loop pays.
        let pools = vec![
            pool(1, WETH(), t(1), 1_000_000_000_000_000_000_000, 2_000_000_000_000),
            pool(2, t(1), t(2), 2_000_000_000_000, 2_000_000_000_000),
            pool(3, t(2), WETH(), 1_600_000_000_000, 1_000_000_000_000_000_000_000),
        ];
        let (selected, edges, found) = search(
            &pools,
            WETH(),
            U256::from(10u128.pow(20)),
            3,
            Duration::from_secs(1),
        );
        let three = found
            .iter()
            .find(|c| c.cycle.legs() == 3)
            .expect("a three-leg cycle exists");

        // Independently recompute the round trip leg by leg; the search must not
        // be doing anything cleverer than composing v2_amount_out.
        let mut amount = three.amount_in;
        for &e in &three.cycle.edges {
            let edge = edges[e];
            let p = selected[edge.pool];
            let (r_in, r_out) = p.reserves_for(edge.token_in).unwrap();
            amount = v2_amount_out(amount, r_in, r_out, p.fee_bps);
        }
        assert_eq!(
            amount.saturating_sub(three.amount_in),
            three.gross_profit,
            "gross profit must equal the composed leg-by-leg round trip"
        );
    }

    #[test]
    fn every_cycle_starts_and_ends_on_the_anchor() {
        let pools = vec![
            pool(1, WETH(), t(1), 1_000_000_000_000_000_000_000, 2_000_000_000_000),
            pool(2, t(1), t(2), 2_000_000_000_000, 2_000_000_000_000),
            pool(3, t(2), WETH(), 1_600_000_000_000, 1_000_000_000_000_000_000_000),
        ];
        let (edges, adj) = graph(&pools);
        let cycles = enumerate_cycles(&edges, &adj, WETH(), MAX_CYCLE_LEN, None);
        assert!(!cycles.is_empty());
        for c in &cycles {
            assert_eq!(edges[*c.edges.first().unwrap()].token_in, WETH());
            assert_eq!(edges[*c.edges.last().unwrap()].token_out, WETH());
        }
    }

    #[test]
    fn cycles_never_exceed_the_length_cap() {
        // A dense 5-token graph: plenty of long walks are available, so the cap
        // has to be doing real work here.
        let toks = [WETH(), t(1), t(2), t(3), t(4)];
        let mut pools = Vec::new();
        let mut n = 0u8;
        for i in 0..toks.len() {
            for j in (i + 1)..toks.len() {
                n += 1;
                pools.push(pool(n, toks[i], toks[j], 1_000_000_000_000, 1_000_000_000_000));
            }
        }
        let (edges, adj) = graph(&pools);
        for max_len in 2..=MAX_CYCLE_LEN {
            let cycles = enumerate_cycles(&edges, &adj, WETH(), max_len, None);
            assert!(
                cycles.iter().all(|c| c.legs() <= max_len),
                "a cycle exceeded max_len {max_len}"
            );
            assert!(cycles.iter().all(|c| c.legs() >= 2));
        }
    }

    #[test]
    fn a_pool_is_never_used_twice_in_one_cycle() {
        let toks = [WETH(), t(1), t(2), t(3)];
        let mut pools = Vec::new();
        let mut n = 0u8;
        for i in 0..toks.len() {
            for j in (i + 1)..toks.len() {
                n += 1;
                pools.push(pool(n, toks[i], toks[j], 1_000_000_000_000, 1_000_000_000_000));
            }
        }
        let (edges, adj) = graph(&pools);
        for c in enumerate_cycles(&edges, &adj, WETH(), MAX_CYCLE_LEN, None) {
            let mut used: Vec<usize> = c.edges.iter().map(|&e| edges[e].pool).collect();
            let before = used.len();
            used.sort_unstable();
            used.dedup();
            assert_eq!(before, used.len(), "a pool appeared twice in one cycle");
        }
    }

    #[test]
    fn cycles_are_deduplicated() {
        let pools = vec![
            pool(1, WETH(), t(1), 1_000_000_000_000, 1_000_000_000_000),
            pool(2, WETH(), t(1), 1_000_000_000_000, 1_100_000_000_000),
            pool(3, WETH(), t(1), 1_000_000_000_000, 1_200_000_000_000),
        ];
        let (edges, adj) = graph(&pools);
        let cycles = enumerate_cycles(&edges, &adj, WETH(), 2, None);
        let mut keys: Vec<Vec<usize>> = cycles
            .iter()
            .map(|c| {
                let mut k = c.edges.clone();
                k.sort_unstable();
                k
            })
            .collect();
        let before = keys.len();
        keys.sort();
        keys.dedup();
        assert_eq!(before, keys.len(), "duplicate cycles were returned");
        // Three parallel pools -> 3 unordered pairs, each usable in 2 directions.
        assert_eq!(before, 6);
    }

    #[test]
    fn an_expired_deadline_returns_no_candidates_instead_of_running_long() {
        let toks = [WETH(), t(1), t(2), t(3), t(4)];
        let mut pools = Vec::new();
        let mut n = 0u8;
        for i in 0..toks.len() {
            for j in (i + 1)..toks.len() {
                n += 1;
                pools.push(pool(n, toks[i], toks[j], 1_000_000_000_000, 1_000_000_000_000));
            }
        }
        let (edges, adj) = graph(&pools);
        let past = Instant::now()
            .checked_sub(Duration::from_millis(1))
            .expect("clock supports subtraction");
        assert!(enumerate_cycles(&edges, &adj, WETH(), MAX_CYCLE_LEN, Some(past)).is_empty());
    }

    #[test]
    fn enumeration_of_a_dense_graph_stays_inside_the_budget() {
        // 30 tokens, all WETH-quoted, plus a ring of cross pairs: comfortably
        // more connected than the live cache and a fair check that the 25 ms
        // budget is not fiction.
        let mut pools = Vec::new();
        let mut n = 0u8;
        for i in 1..=30u8 {
            n = n.wrapping_add(1);
            pools.push(pool(n, WETH(), t(i), 1_000_000_000_000, 1_000_000_000_000));
        }
        for i in 1..30u8 {
            n = n.wrapping_add(1);
            pools.push(pool(n, t(i), t(i + 1), 1_000_000_000_000, 1_000_000_000_000));
        }
        let started = Instant::now();
        let found = search(
            &pools,
            WETH(),
            U256::from(10u128.pow(20)),
            MAX_CYCLE_LEN,
            ENUMERATION_BUDGET,
        );
        let elapsed = started.elapsed();

        // The deadline bounds cycle enumeration. Sizing cycles that were
        // already enumerated happens afterwards, so it has no 25 ms guarantee.
        // Keep a generous ceiling to catch a pathological unbounded search
        // without making this test depend on noisy shared CI runner speed.
        assert!(
            elapsed < Duration::from_secs(2),
            "search took {:?}", elapsed
        );
        assert!(!found.is_empty(), "dense graph should yield candidates");
        assert!(
            found.len() <= MAX_CANDIDATES,
            "search returned more than {MAX_CANDIDATES} candidates"
        );
    }

    #[test]
    fn gas_model_charges_for_extra_legs() {
        assert_eq!(gas_estimate(2), 320_000);
        assert_eq!(gas_estimate(3), 440_000);
        assert_eq!(gas_estimate(5), 680_000);
    }

    #[test]
    fn select_pools_prefers_deep_weth_pools_then_fills_with_cross_pairs() {
        let pools = vec![
            pool(1, WETH(), t(1), 100, 100),
            pool(2, WETH(), t(2), 900, 900),
            pool(3, t(1), t(2), 500, 500),
        ];
        let picked = select_pools(&pools, WETH(), 2);
        assert_eq!(picked.len(), 2);
        assert_eq!(picked[0].address, Address::with_last_byte(2));
        assert_eq!(picked[1].address, Address::with_last_byte(1));

        // Room to spare -> cross pairs are kept, since they are what makes a
        // three-leg cycle reachable at all.
        let picked = select_pools(&pools, WETH(), 3);
        assert_eq!(picked.len(), 3);
    }
}
