//! Directed venue edges that can quote and build executor calls.
//!
//! The historical cycle search in [`super::graph`] is `V2Pool`-specific and
//! stays that way: V2 behaviour is pinned by the existing fixtures and must
//! remain byte-identical. This module is the extension point for every other
//! venue (Uniswap V3 now, Aerodrome later).
//!
//! A [`PricedEdge`] never pretends a concentrated-liquidity pool is a
//! constant-product reserve pair. V3 quotes come from an exact
//! [`QuoteBook`] (QuoterV2 probes at runtime, a fixture in tests). A miss
//! is `None`, never an interpolation.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::{Duration, Instant};

use alloy_primitives::{Address, B256, U256};
use alloy_sol_types::SolCall;

use crate::dex::{self, graph, AeroPool, V2Pool, V3Pool, Venue};
use crate::types::Call;

/// Capability flags a downstream searcher can filter on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EdgeCaps {
    pub volatile: bool,
    pub stable: bool,
    pub concentrated: bool,
}

/// Exact-in quotes keyed by input amount. Fail-closed: a size that was
/// never probed does not invent an output.
#[derive(Clone, Debug, Default)]
pub struct QuoteBook {
    points: BTreeMap<U256, U256>,
}

impl QuoteBook {
    pub fn new() -> Self {
        Self {
            points: BTreeMap::new(),
        }
    }

    pub fn insert(&mut self, amount_in: U256, amount_out: U256) {
        if !amount_in.is_zero() && !amount_out.is_zero() {
            self.points.insert(amount_in, amount_out);
        }
    }

    pub fn get(&self, amount_in: U256) -> Option<U256> {
        self.points.get(&amount_in).copied()
    }

    pub fn sizes(&self) -> impl Iterator<Item = U256> + '_ {
        self.points.keys().copied()
    }

    pub fn is_empty(&self) -> bool {
        self.points.is_empty()
    }

    pub fn len(&self) -> usize {
        self.points.len()
    }
}

/// One directed hop that can quote an exact input and emit executor calls.
#[derive(Clone, Debug)]
pub struct PricedEdge {
    pub pool: Address,
    pub venue: Venue,
    pub token_in: Address,
    pub token_out: Address,
    pub block: u64,
    pub state_id: Option<B256>,
    pub gas: u64,
    pub caps: EdgeCaps,
    kind: EdgeKind,
}

#[derive(Clone, Debug)]
enum EdgeKind {
    V2 {
        pool: V2Pool,
    },
    V3 {
        pool: V3Pool,
        router: Address,
        quotes: QuoteBook,
    },
    AeroVolatile {
        pool: AeroPool,
        router: Address,
        factory: Address,
    },
}

impl PricedEdge {
    /// Machine-readable hop identity for the WS-R state-comparison producer:
    /// venue, pool, direction token and the exact fee the prediction billed.
    pub fn route_hop(&self) -> crate::types::RouteHop {
        let fee_bps = match &self.kind {
            EdgeKind::V2 { pool } => pool.fee_bps,
            EdgeKind::V3 { pool, .. } => pool.fee,
            EdgeKind::AeroVolatile { pool, .. } => pool.fee_bps,
        };
        crate::types::RouteHop {
            venue: self.venue,
            pool: self.pool,
            token_in: self.token_in,
            fee_bps,
        }
    }

    /// Both directions of a V2 pool. Dust sides are dropped.
    pub fn from_v2(pool: &V2Pool) -> Vec<Self> {
        let mut out = Vec::with_capacity(2);
        if pool.reserve0.is_zero() || pool.reserve1.is_zero() {
            return out;
        }
        for (token_in, token_out) in [(pool.token0, pool.token1), (pool.token1, pool.token0)] {
            out.push(Self {
                pool: pool.address,
                venue: pool.venue,
                token_in,
                token_out,
                block: pool.block,
                state_id: None,
                gas: 120_000,
                caps: EdgeCaps {
                    volatile: true,
                    stable: false,
                    concentrated: false,
                },
                kind: EdgeKind::V2 { pool: *pool },
            });
        }
        out
    }

    /// Both directions of an Aerodrome **volatile** pool. Dust sides and
    /// stable pools are dropped — stable is a separate capability flag and a
    /// separately-gated work item, never silently priced by volatile math.
    pub fn from_aero(pool: &AeroPool, router: Address, factory: Address) -> Vec<Self> {
        let mut out = Vec::with_capacity(2);
        if pool.stable || pool.reserve0.is_zero() || pool.reserve1.is_zero() {
            return out;
        }
        for (token_in, token_out) in [(pool.token0, pool.token1), (pool.token1, pool.token0)] {
            out.push(Self {
                pool: pool.address,
                venue: Venue::AeroVolatile,
                token_in,
                token_out,
                block: pool.block,
                state_id: None,
                gas: 170_000,
                caps: EdgeCaps {
                    volatile: true,
                    stable: false,
                    concentrated: false,
                },
                kind: EdgeKind::AeroVolatile {
                    pool: *pool,
                    router,
                    factory,
                },
            });
        }
        out
    }

    /// One direction of a V3 pool, quoted only at the sizes in `quotes`.
    pub fn v3(
        pool: V3Pool,
        token_in: Address,
        token_out: Address,
        router: Address,
        quotes: QuoteBook,
    ) -> Option<Self> {
        if pool.other_token(token_in)? != token_out || quotes.is_empty() {
            return None;
        }
        Some(Self {
            pool: pool.address,
            venue: Venue::UniV3,
            token_in,
            token_out,
            block: pool.block,
            state_id: None,
            gas: 180_000,
            caps: EdgeCaps {
                volatile: false,
                stable: false,
                concentrated: true,
            },
            kind: EdgeKind::V3 {
                pool,
                router,
                quotes,
            },
        })
    }

    pub fn is_v3(&self) -> bool {
        matches!(self.kind, EdgeKind::V3 { .. })
    }

    pub fn is_v2(&self) -> bool {
        matches!(self.kind, EdgeKind::V2 { .. })
    }

    /// Exact output for `amount_in`, or `None` when this venue cannot price
    /// that size (V3 book miss, zero input, broken pool).
    pub fn quote(&self, amount_in: U256) -> Option<U256> {
        if amount_in.is_zero() {
            return None;
        }
        match &self.kind {
            EdgeKind::V2 { pool } => {
                let out = pool.amount_out(self.token_in, amount_in)?;
                if out.is_zero() {
                    None
                } else {
                    Some(out)
                }
            }
            EdgeKind::V3 { quotes, .. } => quotes.get(amount_in),
            EdgeKind::AeroVolatile { pool, .. } => pool.amount_out(self.token_in, amount_in),
        }
    }

    /// Discrete sizes this edge can be evaluated at. Empty for closed-form
    /// venues (V2, Aerodrome volatile) — any size is quotable.
    pub fn discrete_sizes(&self) -> Vec<U256> {
        match &self.kind {
            EdgeKind::V2 { .. } | EdgeKind::AeroVolatile { .. } => Vec::new(),
            EdgeKind::V3 { quotes, .. } => quotes.sizes().collect(),
        }
    }

    /// Executor calls that realise this hop for `amount_in`.
    pub fn build_calls(&self, amount_in: U256, recipient: Address) -> Option<Vec<Call>> {
        let _out = self.quote(amount_in)?;
        match &self.kind {
            EdgeKind::V2 { pool } => Some(build_v2_leg(pool, self.token_in, amount_in, recipient)),
            EdgeKind::V3 { pool, router, .. } => Some(build_v3_router_leg(
                *router,
                self.token_in,
                self.token_out,
                pool.fee,
                amount_in,
                recipient,
            )),
            EdgeKind::AeroVolatile {
                router, factory, ..
            } => Some(build_aero_leg(
                *router,
                *factory,
                self.token_in,
                self.token_out,
                amount_in,
                recipient,
            )),
        }
    }
}

fn build_v2_leg(
    pool: &V2Pool,
    token_in: Address,
    amount_in: U256,
    recipient: Address,
) -> Vec<Call> {
    let amount_out = pool.amount_out(token_in, amount_in).unwrap_or(U256::ZERO);
    let (amount0_out, amount1_out) = if token_in == pool.token0 {
        (U256::ZERO, amount_out)
    } else {
        (amount_out, U256::ZERO)
    };
    vec![
        Call::new(
            token_in,
            dex::IERC20::transferCall {
                to: pool.address,
                amount: amount_in,
            }
            .abi_encode(),
        ),
        Call::new(
            pool.address,
            dex::IUniswapV2Pair::swapCall {
                amount0Out: amount0_out,
                amount1Out: amount1_out,
                to: recipient,
                data: alloy_primitives::Bytes::new(),
            }
            .abi_encode(),
        ),
    ]
}

/// Approve SwapRouter02 then `exactInputSingle`. Shared with the V3 sandwich
/// so a strategy change never forks the calldata.
pub fn build_v3_router_leg(
    router: Address,
    token_in: Address,
    token_out: Address,
    fee: u32,
    amount_in: U256,
    recipient: Address,
) -> Vec<Call> {
    vec![
        Call::new(
            token_in,
            dex::IERC20::approveCall {
                spender: router,
                amount: amount_in,
            }
            .abi_encode(),
        ),
        Call::new(
            router,
            dex::ISwapRouter02::exactInputSingleCall {
                params: dex::ISwapRouter02::ExactInputSingleParams {
                    tokenIn: token_in,
                    tokenOut: token_out,
                    fee: alloy_primitives::aliases::U24::from(fee),
                    recipient,
                    amountIn: amount_in,
                    amountOutMinimum: U256::ZERO,
                    sqrtPriceLimitX96: alloy_primitives::aliases::U160::ZERO,
                },
            }
            .abi_encode(),
        ),
    ]
}

/// Approve the Aerodrome router then one `swapExactTokensForTokens` hop over
/// a single `Route { from, to, stable: false, factory }`. The volatile
/// `stable: false` flag is what tells the router which invariant the pool
/// runs — a stable route here would price wrong.
pub fn build_aero_leg(
    router: Address,
    factory: Address,
    token_in: Address,
    token_out: Address,
    amount_in: U256,
    recipient: Address,
) -> Vec<Call> {
    vec![
        Call::new(
            token_in,
            dex::IERC20::approveCall {
                spender: router,
                amount: amount_in,
            }
            .abi_encode(),
        ),
        Call::new(
            router,
            dex::IAerodromeRouter::swapExactTokensForTokensCall {
                amountIn: amount_in,
                amountOutMin: U256::ZERO,
                routes: vec![dex::IAerodromeRouter::Route {
                    from: token_in,
                    to: token_out,
                    stable: false,
                    factory,
                }],
                to: recipient,
                deadline: U256::MAX,
            }
            .abi_encode(),
        ),
    ]
}

/// A sized cycle over [`PricedEdge`]s. Indices refer to the slice the
/// search was given.
#[derive(Clone, Debug)]
pub struct PricedCycle {
    pub edges: Vec<usize>,
    pub amount_in: U256,
    pub gross_profit: U256,
    pub anchor: Address,
}

impl PricedCycle {
    pub fn legs(&self) -> usize {
        self.edges.len()
    }

    pub fn uses_v3(&self, edges: &[PricedEdge]) -> bool {
        self.edges
            .iter()
            .any(|&i| edges.get(i).is_some_and(|e| e.is_v3()))
    }

    /// True when any leg is a non-V2 venue (V3 QuoterV2 book, Aerodrome
    /// volatile, ...). The mixed-graph search emits only these: all-V2 cycles
    /// were already emitted by the byte-pinned [`graph::search`] pass, and
    /// re-emitting them here would double-count the funnel.
    pub fn uses_non_v2(&self, edges: &[PricedEdge]) -> bool {
        self.edges
            .iter()
            .any(|&i| edges.get(i).is_some_and(|e| !e.is_v2()))
    }

    pub fn route_label(&self, edges: &[PricedEdge]) -> String {
        self.edges
            .iter()
            .filter_map(|&i| edges.get(i))
            .map(|e| format!("{}:{:?}", e.venue.as_str(), e.pool))
            .collect::<Vec<_>>()
            .join(" -> ")
    }

    pub fn build_calls(&self, edges: &[PricedEdge], recipient: Address) -> Option<Vec<Call>> {
        let mut calls = Vec::new();
        let mut amount = self.amount_in;
        for &i in &self.edges {
            let edge = edges.get(i)?;
            calls.extend(edge.build_calls(amount, recipient)?);
            amount = edge.quote(amount)?;
        }
        Some(calls)
    }
}

/// Enumerate and size cycles over a mixed venue graph.
///
/// All-V2 cycles are sized with the same ternary search as [`graph::search`]
/// (and a dedicated test pins the two equal). Cycles that touch a V3 edge
/// are sized on the discrete book sizes only — we never invent a QuoterV2
/// output.
pub fn search_priced(
    edges: &[PricedEdge],
    weth: Address,
    max_in: U256,
    max_len: usize,
    budget: Duration,
) -> Vec<PricedCycle> {
    let max_len = max_len.clamp(2, graph::MAX_CYCLE_LEN);
    let mut adj: HashMap<Address, Vec<usize>> = HashMap::new();
    for (i, e) in edges.iter().enumerate() {
        adj.entry(e.token_in).or_default().push(i);
    }
    let deadline = Instant::now().checked_add(budget);
    let mut cycles: Vec<Vec<usize>> = Vec::new();
    let mut seen: HashSet<Vec<usize>> = HashSet::new();
    let mut path: Vec<usize> = Vec::with_capacity(max_len);
    let mut visited: HashSet<Address> = HashSet::new();
    let mut used_pools: HashSet<Address> = HashSet::new();
    visited.insert(weth);
    walk_priced(
        weth,
        weth,
        edges,
        &adj,
        max_len,
        deadline,
        &mut path,
        &mut visited,
        &mut used_pools,
        &mut seen,
        &mut cycles,
    );

    let mut out: Vec<PricedCycle> = Vec::new();
    for cycle in cycles {
        if let Some(d) = deadline {
            if Instant::now() >= d {
                break;
            }
        }
        if let Some((amount_in, gross_profit)) = size_cycle(&cycle, edges, weth, max_in) {
            out.push(PricedCycle {
                edges: cycle,
                amount_in,
                gross_profit,
                anchor: weth,
            });
        }
    }
    out.sort_by_key(|c| std::cmp::Reverse(c.gross_profit));
    out.truncate(graph::MAX_CANDIDATES);
    out
}

#[allow(clippy::too_many_arguments)]
fn walk_priced(
    anchor: Address,
    current: Address,
    edges: &[PricedEdge],
    adj: &HashMap<Address, Vec<usize>>,
    max_len: usize,
    deadline: Option<Instant>,
    path: &mut Vec<usize>,
    visited: &mut HashSet<Address>,
    used_pools: &mut HashSet<Address>,
    seen: &mut HashSet<Vec<usize>>,
    out: &mut Vec<Vec<usize>>,
) {
    if out.len() >= 1_024 {
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
    for &i in candidates {
        let edge = &edges[i];
        if used_pools.contains(&edge.pool) {
            continue;
        }
        if edge.token_out == anchor {
            if path.len() + 1 >= 2 {
                let mut cycle = path.clone();
                cycle.push(i);
                let mut key = cycle.clone();
                key.sort_unstable();
                if seen.insert(key) {
                    out.push(cycle);
                    if out.len() >= 1_024 {
                        return;
                    }
                }
            }
            continue;
        }
        if path.len() + 1 >= max_len || visited.contains(&edge.token_out) {
            continue;
        }
        path.push(i);
        visited.insert(edge.token_out);
        used_pools.insert(edge.pool);
        walk_priced(
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

fn evaluate_cycle(cycle: &[usize], edges: &[PricedEdge], amount_in: U256) -> Option<U256> {
    let mut amount = amount_in;
    for &i in cycle {
        amount = edges.get(i)?.quote(amount)?;
        if amount.is_zero() {
            return Some(U256::ZERO);
        }
    }
    Some(amount)
}

fn size_cycle(
    cycle: &[usize],
    edges: &[PricedEdge],
    _anchor: Address,
    max_in: U256,
) -> Option<(U256, U256)> {
    let has_discrete = cycle
        .iter()
        .any(|&i| edges.get(i).is_some_and(|e| !e.discrete_sizes().is_empty()));
    if !has_discrete {
        // All closed-form (V2 / Aerodrome volatile): same ternary as
        // graph::optimal_cycle_in.
        let first = edges.get(*cycle.first()?)?;
        let hi = match &first.kind {
            EdgeKind::V2 { pool } => max_in.min(pool.reserves_for(first.token_in)?.0),
            EdgeKind::AeroVolatile { pool, .. } => max_in.min(pool.reserves_for(first.token_in)?.0),
            EdgeKind::V3 { .. } => max_in,
        };
        if hi.is_zero() {
            return None;
        }
        let profit_of = |x: U256| -> U256 {
            match evaluate_cycle(cycle, edges, x) {
                Some(out) => out.saturating_sub(x),
                None => U256::ZERO,
            }
        };
        let (x, p) = dex::ternary_search_max(U256::ZERO, hi, profit_of);
        if x.is_zero() || p.is_zero() {
            None
        } else {
            Some((x, p))
        }
    } else {
        // Discrete only. Prefer sizes the first V3-ish edge actually quoted;
        // fall back to every book on the cycle.
        let mut sizes: Vec<U256> = Vec::new();
        for &i in cycle {
            sizes.extend(edges.get(i)?.discrete_sizes());
        }
        sizes.sort();
        sizes.dedup();
        let mut best: Option<(U256, U256)> = None;
        for x in sizes {
            if x.is_zero() || x > max_in {
                continue;
            }
            let Some(out) = evaluate_cycle(cycle, edges, x) else {
                continue;
            };
            let profit = out.saturating_sub(x);
            if profit.is_zero() {
                continue;
            }
            if best.map(|(_, p)| profit > p).unwrap_or(true) {
                best = Some((x, profit));
            }
        }
        best
    }
}

/// Default QuoterV2 probe sizes as fractions of `max_in` (bps). Four points
/// keep a V3×V2 pair inside the same 12-call budget sandwich_v3 uses.
pub const V3_PROBE_BPS: [u32; 4] = [400, 1_200, 2_800, 5_600];

pub fn probe_sizes(max_in: U256) -> Vec<U256> {
    V3_PROBE_BPS
        .iter()
        .filter_map(|&b| {
            let x = max_in.checked_mul(U256::from(b))? / U256::from(10_000u32);
            if x.is_zero() {
                None
            } else {
                Some(x)
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::known;
    use crate::dex::graph;

    fn pool(venue: Venue, addr: u8, r0: u128, r1: u128) -> V2Pool {
        V2Pool {
            address: Address::with_last_byte(addr),
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
    fn v2_adapter_emits_two_live_directions_and_skips_dust() {
        let live = pool(Venue::UniV2, 1, 1_000, 1_000);
        assert_eq!(PricedEdge::from_v2(&live).len(), 2);
        let dust = pool(Venue::UniV2, 2, 0, 0);
        assert!(PricedEdge::from_v2(&dust).is_empty());
    }

    #[test]
    fn v2_quote_matches_the_pool() {
        let p = pool(Venue::UniV2, 1, 1_000_000, 1_000_000);
        let edges = PricedEdge::from_v2(&p);
        let weth_in = edges.iter().find(|e| e.token_in == known::WETH).unwrap();
        let want = p.amount_out(known::WETH, U256::from(1_000u64)).unwrap();
        assert_eq!(weth_in.quote(U256::from(1_000u64)), Some(want));
        assert_eq!(weth_in.quote(U256::ZERO), None);
    }

    #[test]
    fn v3_quote_is_fail_closed_on_a_book_miss() {
        let meta = V3Pool {
            address: Address::with_last_byte(9),
            token0: known::WETH,
            token1: known::USDC,
            fee: 3_000,
            tick_spacing: 60,
            block: 1,
        };
        let mut book = QuoteBook::new();
        book.insert(U256::from(1_000u64), U256::from(2_000u64));
        let edge = PricedEdge::v3(
            meta,
            known::WETH,
            known::USDC,
            known::UNIV3_SWAP_ROUTER_02,
            book,
        )
        .unwrap();
        assert_eq!(edge.quote(U256::from(1_000u64)), Some(U256::from(2_000u64)));
        assert_eq!(
            edge.quote(U256::from(1_001u64)),
            None,
            "a size that was never probed must not interpolate"
        );
        assert!(edge.caps.concentrated);
        assert!(!edge.caps.volatile);
    }

    #[test]
    fn search_priced_on_v2_edges_matches_graph_search() {
        // Byte-identical regression: wrapping the same two pools as
        // PricedEdges and running the new search must recover the same
        // sized 2-leg cycle the historical enumerator finds.
        let a = pool(Venue::UniV2, 1, 1_000e18 as u128, 2_200_000e6 as u128);
        let b = pool(Venue::SushiV2, 2, 1_000e18 as u128, 2_000_000e6 as u128);
        let max_in = U256::from(10u128.pow(21));
        let budget = Duration::from_secs(1);

        let (_, _, want) = graph::search(&[a, b], known::WETH, max_in, 2, budget);
        let best = want.first().expect("historical search finds the cycle");

        let mut edges = PricedEdge::from_v2(&a);
        edges.extend(PricedEdge::from_v2(&b));
        let found = search_priced(&edges, known::WETH, max_in, 2, budget);
        let got = found.first().expect("priced search finds the same cycle");
        assert_eq!(got.legs(), 2);
        assert_eq!(got.amount_in, best.amount_in);
        assert_eq!(got.gross_profit, best.gross_profit);
        assert!(!got.uses_v3(&edges));
    }

    #[test]
    fn a_v3_book_plus_a_diverged_v2_pool_produces_a_cross_venue_cycle() {
        // V3 "book" is a deep quote in WETH→USDC; V2 is cheaper in USDC.
        // Buying on V3 and selling on V2 must surface as a 2-leg cycle.
        let v2 = pool(Venue::UniV2, 1, 1_000e18 as u128, 1_800_000e6 as u128);
        let meta = V3Pool {
            address: Address::with_last_byte(9),
            token0: known::WETH,
            token1: known::USDC,
            fee: 500,
            tick_spacing: 10,
            block: 1,
        };
        // Fabricate a book whose WETH→USDC output is *better* than the V2
        // pool, so the round trip pays after the V2 sell.
        let size = U256::from(10u128.pow(19));
        let v2_out = v2.amount_out(known::WETH, size).unwrap();
        let generous = v2_out + v2_out / U256::from(20u64); // +5%
        let mut book = QuoteBook::new();
        book.insert(size, generous);
        // Reverse book so the other direction is also walkable.
        let mut back = QuoteBook::new();
        back.insert(generous, size / U256::from(2u64)); // deliberately unprofitable

        let mut edges = PricedEdge::from_v2(&v2);
        edges.push(
            PricedEdge::v3(
                meta,
                known::WETH,
                known::USDC,
                known::UNIV3_SWAP_ROUTER_02,
                book,
            )
            .unwrap(),
        );
        edges.push(
            PricedEdge::v3(
                meta,
                known::USDC,
                known::WETH,
                known::UNIV3_SWAP_ROUTER_02,
                back,
            )
            .unwrap(),
        );

        let found = search_priced(&edges, known::WETH, size, 2, Duration::from_secs(1));
        let cross = found
            .iter()
            .find(|c| c.uses_v3(&edges))
            .expect("a V3↔V2 cycle exists");
        assert_eq!(cross.legs(), 2);
        assert_eq!(cross.amount_in, size);
        assert!(cross.gross_profit > U256::ZERO);
        let calls = cross
            .build_calls(&edges, Address::with_last_byte(0xee))
            .expect("executable");
        assert!(
            calls
                .iter()
                .any(|c| c.target == known::UNIV3_SWAP_ROUTER_02),
            "V3 leg must hit SwapRouter02"
        );
    }

    fn aero_pool(addr: u8, r0: u128, r1: u128, stable: bool) -> AeroPool {
        AeroPool {
            address: Address::with_last_byte(addr),
            token0: crate::config::known::WETH,
            token1: crate::config::known::USDC,
            reserve0: U256::from(r0),
            reserve1: U256::from(r1),
            fee_bps: 30,
            stable,
            block: 1,
        }
    }

    #[test]
    fn aero_adapter_emits_two_volatile_directions_and_skips_stable_and_dust() {
        let router = Address::with_last_byte(0x99);
        let factory = Address::with_last_byte(0x88);
        let volatile = aero_pool(1, 1_000, 1_000, false);
        let edges = PricedEdge::from_aero(&volatile, router, factory);
        assert_eq!(edges.len(), 2);
        assert!(edges.iter().all(|e| e.venue == Venue::AeroVolatile));
        assert!(edges
            .iter()
            .all(|e| e.caps.volatile && !e.caps.concentrated));
        assert!(
            PricedEdge::from_aero(&aero_pool(2, 1_000, 1_000, true), router, factory).is_empty()
        );
        assert!(PricedEdge::from_aero(&aero_pool(3, 0, 0, false), router, factory).is_empty());
    }

    #[test]
    fn aero_quote_tracks_the_volatile_formula_not_univ2() {
        let router = Address::with_last_byte(0x99);
        let factory = Address::with_last_byte(0x88);
        let p = aero_pool(1, 2_000_000, 2_000_000, false);
        let edges = PricedEdge::from_aero(&p, router, factory);
        let weth_in = edges.iter().find(|e| e.token_in == p.token0).unwrap();
        let x = U256::from(1_000u64);
        assert_eq!(
            weth_in.quote(x),
            Some(dex::aero_volatile_amount_out(x, p.reserve0, p.reserve1, 30))
        );
        assert_eq!(weth_in.quote(U256::ZERO), None);
    }

    #[test]
    fn a_v2_pool_and_a_diverged_aero_pool_produce_a_cross_venue_cycle() {
        let v2 = pool(Venue::UniV2, 1, 1_000e18 as u128, 2_200_000e6 as u128);
        let aero = aero_pool(2, 1_000e18 as u128, 1_800_000e6 as u128, false);
        let mut edges = PricedEdge::from_v2(&v2);
        edges.extend(PricedEdge::from_aero(
            &aero,
            Address::with_last_byte(0x99),
            Address::with_last_byte(0x88),
        ));
        let found = search_priced(
            &edges,
            known::WETH,
            U256::from(10u128.pow(20)),
            2,
            Duration::from_secs(1),
        );
        let best = found.first().expect("a V2↔Aero cycle exists");
        assert_eq!(best.legs(), 2);
        assert!(best.gross_profit > U256::ZERO);
        let calls = best
            .build_calls(&edges, Address::with_last_byte(0xee))
            .expect("executable");
        // The Aero leg targets the Aerodrome router.
        assert!(calls
            .iter()
            .any(|c| c.target == Address::with_last_byte(0x99)));
        // And its swap call routes with stable: false.
        let swap = calls
            .iter()
            .find(|c| c.target == Address::with_last_byte(0x99))
            .unwrap();
        assert_eq!(
            &swap.data[..4],
            &dex::IAerodromeRouter::swapExactTokensForTokensCall::SELECTOR
        );
    }

    #[test]
    fn aero_leg_is_approve_then_router_swap_exact_tokens() {
        let calls = build_aero_leg(
            known::BASE_AERODROME_ROUTER,
            known::BASE_AERODROME_FACTORY,
            known::WETH,
            known::USDC,
            U256::from(1_000u64),
            Address::with_last_byte(9),
        );
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].target, known::WETH);
        assert_eq!(calls[1].target, known::BASE_AERODROME_ROUTER);
        assert_eq!(
            &calls[1].data[..4],
            &dex::IAerodromeRouter::swapExactTokensForTokensCall::SELECTOR
        );
    }

    #[test]
    fn v3_router_leg_is_approve_then_exact_input_single() {
        let calls = build_v3_router_leg(
            known::UNIV3_SWAP_ROUTER_02,
            known::WETH,
            known::USDC,
            3_000,
            U256::from(1_000u64),
            Address::with_last_byte(9),
        );
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].target, known::WETH);
        assert_eq!(calls[1].target, known::UNIV3_SWAP_ROUTER_02);
        assert_eq!(
            &calls[1].data[..4],
            &dex::ISwapRouter02::exactInputSingleCall::SELECTOR
        );
    }
}
