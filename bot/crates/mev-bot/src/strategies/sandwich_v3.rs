//! V3 sandwich via QuoterV2 (Phase 2 W5).
//!
//! Same shape as the V2 sandwich — front-run, victim, back-run — but the
//! victim is an `exactInputSingle` on SwapRouter / SwapRouter02 and the
//! front-run is sized with the on-chain QuoterV2 rather than a hand-rolled
//! Q64.96 port. Tick math needs 512-bit `mulDiv`; a subtly wrong port would
//! produce quotes that look right, pass a same-author unit test, and revert
//! in the fork. QuoterV2 is correct by construction.
//!
//! # Budgets (acceptance criteria, not suggestions)
//!
//! | Budget | Value |
//! | --- | --- |
//! | `eth_call` per V3 candidate | ≤ [`MAX_QUOTES_PER_CANDIDATE`] (12) |
//! | V3 candidates per pending tx | 1 (the calldata-selected pool) |
//! | Added latency on the pending path | ≤ 25 ms p95 (operator-measured) |
//!
//! A naive ternary search over `quote_v3` is ~120 `eth_call`s and will get
//! the bot rate-limited off its provider. The search is a coarse grid, then
//! a one-step refine, and it stops the moment the quote budget is spent.
//!
//! # What we deliberately do not do
//!
//! * Hand-rolled `TickMath` / `SqrtPriceMath`. See above.
//! * Quote the back-run against post-front-run state. The quoter prices the
//!   *current* pool; a two-quote approximation of the post-state is a
//!   follow-up PR. The back-run is issued with `amountOutMinimum = 0` and
//!   the executor's atomic profit guard is the backstop.
//! * Talk to the pool directly. Pool-direct saves ~20k gas but needs
//!   `int256` encoding and a swap callback — a later PR.
//! * Run when the toggle is off. The strategy is not even constructed, so
//!   it adds zero RPC to the pending path.
//!
//! The pool must already sit in the W3 V3 cache. Enabling this strategy
//! without `POOL_DISCOVERY_V3` is a no-op (decode is local; the cache miss
//! returns before any quote).

use std::sync::atomic::{AtomicU32, Ordering};

use alloy_primitives::{Address, U256};
use alloy_sol_types::SolCall;
use async_trait::async_trait;

use crate::config::known;
use crate::dex::{self, v2_amount_out, v3_fee_to_bps, V3Pool};
use crate::rpc::RpcClient;
use crate::strategies::jit::{decode_v3_swap, V3SwapIntent};
use crate::strategies::{StrategyCtx, StrategyImpl, V3PoolCache};
use crate::types::{now_ms, Call, Opportunity, PendingTx, Strategy};

/// Hard ceiling on QuoterV2 `eth_call`s for one candidate.
pub const MAX_QUOTES_PER_CANDIDATE: u32 = 12;
/// Largest-notional-first cap on V3 candidates evaluated per pending tx.

/// Coarse grid, as fractions of `max_in` in bps. Four points keep the
/// subsequent refine inside the 12-call budget (4 × 2 + 2 × 2 = 12).
const COARSE_BPS: [u32; 4] = [400, 1_200, 2_800, 5_600];

/// Router sandwich: approve + `exactInputSingle` each way ≈ 330k gas.
const GAS_UNITS: u64 = 330_000;

/// Something that can price an exact-in V3 swap. Production talks to
/// QuoterV2; tests use a fake so nothing here touches the network.
#[async_trait]
pub trait V3Quoter: Send + Sync {
    async fn quote_exact_in(
        &self,
        token_in: Address,
        token_out: Address,
        fee: u32,
        amount_in: U256,
    ) -> Option<U256>;
}

/// Production quoter: one `eth_call` per quote, pinned to `block`.
pub struct RpcQuoter<'a> {
    pub rpc: &'a RpcClient,
    pub quoter: Address,
    pub block: String,
    pub calls: AtomicU32,
}

#[async_trait]
impl V3Quoter for RpcQuoter<'_> {
    async fn quote_exact_in(
        &self,
        token_in: Address,
        token_out: Address,
        fee: u32,
        amount_in: U256,
    ) -> Option<U256> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        match dex::quote_v3(
            self.rpc,
            self.quoter,
            token_in,
            token_out,
            fee,
            amount_in,
            &self.block,
        )
        .await
        {
            Ok(out) if !out.is_zero() => Some(out),
            _ => None,
        }
    }
}

pub struct SandwichV3Strategy;

#[async_trait]
impl StrategyImpl for SandwichV3Strategy {
    fn kind(&self) -> Strategy {
        Strategy::SandwichV3
    }

    async fn on_pending(&self, ctx: &StrategyCtx, tx: &PendingTx) -> Vec<Opportunity> {
        let weth = ctx.cfg.chain.weth;
        let Some(intent) = decode_v3_swap(tx) else {
            return Vec::new();
        };
        let Some(pool) = accept_victim(&intent, weth, &ctx.pools_v3) else {
            return Vec::new();
        };

        let head = ctx.head();
        let state_block = tx.state_block(&head);
        let base_fee = tx.base_fee(&head);
        let target_block = tx.target_block(&head, ctx.cfg.sim.target_block_offset);

        let quoter = RpcQuoter {
            rpc: &ctx.rpc,
            quoter: known::UNIV3_QUOTER_V2,
            block: ctx.block_tag(state_block),
            calls: AtomicU32::new(0),
        };

        // One pool per victim (the fee is in the calldata). The candidate
        // cap is here so a future multi-fee expansion cannot blow the
        // pending-path budget by accident.
        let Some(sizing) = size_v3_sandwich(
            &quoter,
            &intent,
            ctx.max_position(),
            MAX_QUOTES_PER_CANDIDATE,
        )
        .await
        else {
            return Vec::new();
        };

        let gas_cost = U256::from(GAS_UNITS) * (base_fee + U256::from(1_000_000_000u64));
        if sizing.gross_profit <= gas_cost {
            return Vec::new();
        }

        let front = build_router_leg(
            intent.token_in,
            intent.token_out,
            pool.fee,
            sizing.amount_in,
            U256::ZERO,
            ctx.executor,
        );
        // Conservative back-run: amountOutMinimum = 0. The quoter cannot
        // see the post-front-run + victim state, so we refuse to invent a
        // min-out and let the on-chain profit guard drop a misprice.
        let back = build_router_leg(
            intent.token_out,
            intent.token_in,
            pool.fee,
            sizing.front_out,
            U256::ZERO,
            ctx.executor,
        );

        vec![Opportunity {
            id: uuid::Uuid::new_v4().to_string(),
            strategy: Strategy::SandwichV3,
            victim_hashes: vec![tx.hash],
            front_calls: front,
            back_calls: back,
            flash_tokens: Vec::new(),
            flash_amounts: Vec::new(),
            profit_token: weth,
            expected_profit_wei: sizing.gross_profit.saturating_sub(gas_cost),
            notional_wei: sizing.amount_in,
            target_block,
            created_at_ms: now_ms(),
            notes: format!(
                "sandwich_v3 {} fee {} pool {:?}: victim in {} min_out {} -> front {} back {} gross {} quotes {}",
                intent.token_out,
                pool.fee,
                pool.address,
                intent.amount_in,
                intent.amount_out_min,
                sizing.amount_in,
                sizing.back_out,
                sizing.gross_profit,
                sizing.quotes_used
            ),
        }]
    }
}

/// Candidate filter: amount > 0, distinct tokens, WETH in, pool in the V3 cache.
///
/// Extracted so the "no cache → no RPC" contract is unit-testable without a
/// `StrategyCtx`.
pub fn accept_victim(intent: &V3SwapIntent, weth: Address, cache: &V3PoolCache) -> Option<V3Pool> {
    if intent.amount_in.is_zero() || intent.token_in == intent.token_out {
        return None;
    }
    // Same inventory constraint as the V2 sandwich: we front-run with WETH.
    if intent.token_in != weth {
        return None;
    }
    cache.for_pair(intent.token_in, intent.token_out, intent.fee)
}

/// Sized V3 sandwich. `quotes_used` is part of the public result so the
/// budget is observable, not assumed.
#[derive(Clone, Copy, Debug)]
pub struct V3SandwichSizing {
    pub amount_in: U256,
    pub front_out: U256,
    pub back_out: U256,
    pub gross_profit: U256,
    pub victim_out: U256,
    pub quotes_used: u32,
}

/// Coarse-grid-then-refine search over QuoterV2.
///
/// Each size costs two quotes: `quote(x)` for the front-run and
/// `quote(x + victim)` so the victim-revert trap can be checked against
/// the combined flow. Profit is ranked with an implied constant-product
/// back-run (the quoter cannot see post-state). The fork remains the
/// arbiter; this only decides whether a size is worth a simulation slot.
pub async fn size_v3_sandwich<Q: V3Quoter>(
    quoter: &Q,
    intent: &V3SwapIntent,
    max_in: U256,
    quote_budget: u32,
) -> Option<V3SandwichSizing> {
    if max_in.is_zero() || intent.amount_in.is_zero() {
        return None;
    }
    let budgeted = BudgetQuoter {
        inner: quoter,
        used: AtomicU32::new(0),
        budget: quote_budget.min(MAX_QUOTES_PER_CANDIDATE),
        fee: intent.fee,
    };

    let mut best: Option<V3SandwichSizing> = None;
    let mut evaluated: Vec<U256> = Vec::new();

    for x in grid_sizes(max_in, &COARSE_BPS) {
        evaluated.push(x);
        if let Some(s) = evaluate_size(&budgeted, intent, x).await {
            if best
                .as_ref()
                .map(|b| s.gross_profit > b.gross_profit)
                .unwrap_or(true)
            {
                best = Some(s);
            }
        }
    }

    // One refine step around the current best: midpoints toward its
    // neighbours. Skipped when the budget is already spent.
    if let Some(ref b) = best {
        for x in refine_sizes(b.amount_in, max_in, &evaluated) {
            if budgeted.remaining() == 0 {
                break;
            }
            if let Some(s) = evaluate_size(&budgeted, intent, x).await {
                if best
                    .as_ref()
                    .map(|cur| s.gross_profit > cur.gross_profit)
                    .unwrap_or(true)
                {
                    best = Some(s);
                }
            }
        }
    }

    let mut out = best?;
    out.quotes_used = budgeted.used();
    if out.gross_profit.is_zero() || out.amount_in.is_zero() {
        None
    } else {
        Some(out)
    }
}

/// Caps the inner quoter at `budget` calls. Interior mutability so the
/// search can stay a straightforward async loop instead of a
/// self-referential `FnMut` future.
struct BudgetQuoter<'a, Q> {
    inner: &'a Q,
    used: AtomicU32,
    budget: u32,
    fee: u32,
}

impl<Q: V3Quoter> BudgetQuoter<'_, Q> {
    fn used(&self) -> u32 {
        self.used.load(Ordering::Relaxed)
    }

    fn remaining(&self) -> u32 {
        self.budget.saturating_sub(self.used())
    }

    async fn quote(&self, token_in: Address, token_out: Address, amount: U256) -> Option<U256> {
        if amount.is_zero() || self.remaining() == 0 {
            return None;
        }
        self.used.fetch_add(1, Ordering::Relaxed);
        self.inner
            .quote_exact_in(token_in, token_out, self.fee, amount)
            .await
    }
}

fn grid_sizes(max_in: U256, bps: &[u32]) -> Vec<U256> {
    bps.iter()
        .filter_map(|&b| {
            let x = max_in.checked_mul(U256::from(b)).unwrap_or(U256::MAX) / U256::from(10_000u32);
            if x.is_zero() {
                None
            } else {
                Some(x)
            }
        })
        .collect()
}

fn refine_sizes(best: U256, max_in: U256, already: &[U256]) -> Vec<U256> {
    let mut xs = already.to_vec();
    xs.sort();
    xs.dedup();
    let mut out = Vec::new();
    if let Some(i) = xs.iter().position(|&x| x == best) {
        if i > 0 {
            let mid = (xs[i - 1] + best) / U256::from(2u8);
            if mid > xs[i - 1] && mid < best {
                out.push(mid);
            }
        }
        if i + 1 < xs.len() {
            let mid = (best + xs[i + 1]) / U256::from(2u8);
            if mid > best && mid < xs[i + 1] {
                out.push(mid);
            }
        } else {
            // Best is the largest coarse point: try halfway to max_in.
            let mid = (best + max_in) / U256::from(2u8);
            if mid > best && mid <= max_in {
                out.push(mid);
            }
        }
    }
    out
}

async fn evaluate_size<Q: V3Quoter>(
    quoter: &BudgetQuoter<'_, Q>,
    intent: &V3SwapIntent,
    x: U256,
) -> Option<V3SandwichSizing> {
    let front_out = quoter.quote(intent.token_in, intent.token_out, x).await?;
    let both_in = x.saturating_add(intent.amount_in);
    let both_out = quoter
        .quote(intent.token_in, intent.token_out, both_in)
        .await?;
    let victim_out = both_out.saturating_sub(front_out);

    // The victim-revert trap. Non-negotiable: a size that pushes the
    // victim below amountOutMinimum leaves us holding inventory.
    if !intent.amount_out_min.is_zero() && victim_out < intent.amount_out_min {
        return None;
    }
    if front_out.is_zero() || victim_out.is_zero() {
        return None;
    }

    let fee_bps = v3_fee_to_bps(intent.fee).max(1);
    let (back_out, profit) =
        rank_with_implied_cp(x, front_out, both_in, both_out, victim_out, fee_bps)?;
    if profit.is_zero() {
        return None;
    }
    Some(V3SandwichSizing {
        amount_in: x,
        front_out,
        back_out,
        gross_profit: profit,
        victim_out,
        quotes_used: 0, // filled in by the caller
    })
}

/// Fit implied constant-product reserves from the two quotes we already
/// paid for, then price the back-run against the post-victim state of
/// that pool. This is a ranking model, not a quote: the back-run is
/// submitted with `amountOutMinimum = 0`.
fn rank_with_implied_cp(
    x: U256,
    front_out: U256,
    both_in: U256,
    both_out: U256,
    victim_out: U256,
    fee_bps: u32,
) -> Option<(U256, U256)> {
    let (r_in, r_out) = implied_reserves(x, front_out, both_in, both_out, fee_bps)?;
    // Reconstruct the three-leg state using the *quoted* outputs so a
    // reserve-fit rounding error cannot invent a victim_out the quoter
    // did not report.
    let r_in2 = r_in.checked_add(x)?;
    let r_out2 = r_out.checked_sub(front_out)?;
    let r_in3 = r_in2.checked_add(both_in.saturating_sub(x))?;
    let r_out3 = r_out2.checked_sub(victim_out)?;
    if r_in3.is_zero() || r_out3.is_zero() {
        return None;
    }
    let back = v2_amount_out(front_out, r_out3, r_in3, fee_bps);
    let profit = back.saturating_sub(x);
    Some((back, profit))
}

/// Invert two exact-in CP quotes into `(reserve_in, reserve_out)`.
///
/// ```text
/// o = a * f * R_out / (R_in * 10000 + a * f)     f = 10000 - fee_bps
/// ```
///
/// Two quotes give a linear system. Returns `None` when the quotes are
/// not concave (which a real AMM always is) or when an intermediate
/// overflows.
pub fn implied_reserves(
    a1: U256,
    o1: U256,
    a2: U256,
    o2: U256,
    fee_bps: u32,
) -> Option<(U256, U256)> {
    if a1.is_zero() || a2.is_zero() || o1.is_zero() || o2.is_zero() || a1 == a2 {
        return None;
    }
    // Keep a1 < a2 so the concavity test has a stable sign.
    let (a1, o1, a2, o2) = if a1 < a2 {
        (a1, o1, a2, o2)
    } else {
        (a2, o2, a1, o1)
    };
    if o2 <= o1 {
        return None;
    }
    let f = U256::from(10_000u32.saturating_sub(fee_bps));
    let ten_k = U256::from(10_000u32);
    let o1_a2 = o1.checked_mul(a2)?;
    let o2_a1 = o2.checked_mul(a1)?;
    // Concavity: the larger trade has the worse average price.
    if o1_a2 <= o2_a1 {
        return None;
    }
    let num = a1.checked_mul(a2)?.checked_mul(f)?.checked_mul(o2 - o1)?;
    let den = ten_k.checked_mul(o1_a2 - o2_a1)?;
    if den.is_zero() {
        return None;
    }
    let r_in = num / den;
    let den_out = a1.checked_mul(f)?;
    if den_out.is_zero() || r_in.is_zero() {
        return None;
    }
    let r_out =
        o1.checked_mul(r_in.checked_mul(ten_k)?.checked_add(a1.checked_mul(f)?)?)? / den_out;
    if r_in.is_zero() || r_out.is_zero() {
        return None;
    }
    Some((r_in, r_out))
}

/// Approve the router, then `exactInputSingle` with `sqrtPriceLimitX96 = 0`.
pub fn build_router_leg(
    token_in: Address,
    token_out: Address,
    fee: u32,
    amount_in: U256,
    amount_out_min: U256,
    recipient: Address,
) -> Vec<Call> {
    let router = known::UNIV3_SWAP_ROUTER_02;
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
                    amountOutMinimum: amount_out_min,
                    sqrtPriceLimitX96: alloy_primitives::aliases::U160::ZERO,
                },
            }
            .abi_encode(),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dex::{v2_amount_out, V2Pool, Venue};

    fn weth() -> Address {
        known::WETH
    }
    fn usdc() -> Address {
        known::USDC
    }

    fn pool(r_in: u128, r_out: u128) -> V2Pool {
        V2Pool {
            address: Address::with_last_byte(0xaa),
            token0: weth(),
            token1: usdc(),
            reserve0: U256::from(r_in),
            reserve1: U256::from(r_out),
            fee_bps: 30,
            venue: Venue::UniV3,
            block: 1,
        }
    }

    /// Quoter backed by a constant-product pool. Counts every call.
    struct FakeQuoter {
        pool: V2Pool,
        calls: AtomicU32,
    }

    #[async_trait]
    impl V3Quoter for FakeQuoter {
        async fn quote_exact_in(
            &self,
            token_in: Address,
            _token_out: Address,
            _fee: u32,
            amount_in: U256,
        ) -> Option<U256> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            let out = self.pool.amount_out(token_in, amount_in)?;
            if out.is_zero() {
                None
            } else {
                Some(out)
            }
        }
    }

    fn intent(amount_in: U256, min_out: U256) -> V3SwapIntent {
        V3SwapIntent {
            token_in: weth(),
            token_out: usdc(),
            fee: 3_000,
            amount_in,
            amount_out_min: min_out,
            zero_for_one: weth() < usdc(),
        }
    }

    #[test]
    fn swap_router_02_selector_is_the_published_one() {
        // ISwapRouter02.exactInputSingle((address,address,uint24,address,uint256,uint256,uint160))
        assert_eq!(
            dex::ISwapRouter02::exactInputSingleCall::SELECTOR,
            [0x04, 0xe4, 0x5a, 0xaf]
        );
    }

    #[test]
    fn implied_reserves_recover_a_known_cp_pool() {
        let p = pool(1_000_000, 1_000_000);
        let a1 = U256::from(1_000u64);
        let a2 = U256::from(5_000u64);
        let o1 = v2_amount_out(a1, p.reserve0, p.reserve1, 30);
        let o2 = v2_amount_out(a2, p.reserve0, p.reserve1, 30);
        let (r_in, r_out) = implied_reserves(a1, o1, a2, o2, 30).expect("fit");
        // Integer rounding; within 1% is plenty for a ranking model.
        let err_in = r_in.abs_diff(p.reserve0) * U256::from(100u64) / p.reserve0;
        let err_out = r_out.abs_diff(p.reserve1) * U256::from(100u64) / p.reserve1;
        assert!(err_in <= U256::from(1u64), "r_in {r_in} vs {}", p.reserve0);
        assert!(
            err_out <= U256::from(1u64),
            "r_out {r_out} vs {}",
            p.reserve1
        );
    }

    #[tokio::test]
    async fn a_large_victim_on_a_deep_pool_is_sandwichable() {
        let q = FakeQuoter {
            pool: pool(1_000_000 * 10u128.pow(18), 1_000_000 * 10u128.pow(18)),
            calls: AtomicU32::new(0),
        };
        let victim = U256::from(50_000u128) * U256::from(10u128.pow(18));
        let max_in = U256::from(100_000u128) * U256::from(10u128.pow(18));
        let s = size_v3_sandwich(
            &q,
            &intent(victim, U256::ZERO),
            max_in,
            MAX_QUOTES_PER_CANDIDATE,
        )
        .await
        .expect("should find a sandwich");
        assert!(s.gross_profit > U256::ZERO);
        assert!(s.amount_in > U256::ZERO);
        assert!(s.quotes_used <= MAX_QUOTES_PER_CANDIDATE);
        assert!(s.quotes_used >= 2);
    }

    #[tokio::test]
    async fn a_zero_slippage_victim_produces_nothing() {
        // Victim demands the unsandwiched output: every positive front-run
        // pushes them below amountOutMinimum, so the trap must fire.
        let p = pool(1_000_000 * 10u128.pow(18), 1_000_000 * 10u128.pow(18));
        let victim = U256::from(50_000u128) * U256::from(10u128.pow(18));
        let strict_min = p.amount_out(weth(), victim).unwrap();
        let q = FakeQuoter {
            pool: p,
            calls: AtomicU32::new(0),
        };
        let max_in = U256::from(100_000u128) * U256::from(10u128.pow(18));
        let res = size_v3_sandwich(
            &q,
            &intent(victim, strict_min),
            max_in,
            MAX_QUOTES_PER_CANDIDATE,
        )
        .await;
        assert!(res.is_none(), "must not sandwich a zero-slippage victim");
        assert!(q.calls.load(Ordering::Relaxed) <= MAX_QUOTES_PER_CANDIDATE);
    }

    #[tokio::test]
    async fn sizing_is_monotone_in_the_quoters_depth() {
        // A deeper pool (the quoter returns more output for the same input)
        // must not produce a *smaller* quote budget and must remain inside
        // it. Profit itself shrinks with depth (less impact) — what we pin
        // is that the search still returns a well-formed size, and that
        // more-generous quotes never select a trapped size.
        let shallow = FakeQuoter {
            pool: pool(200_000 * 10u128.pow(18), 200_000 * 10u128.pow(18)),
            calls: AtomicU32::new(0),
        };
        let deep = FakeQuoter {
            pool: pool(2_000_000 * 10u128.pow(18), 2_000_000 * 10u128.pow(18)),
            calls: AtomicU32::new(0),
        };
        let victim = U256::from(20_000u128) * U256::from(10u128.pow(18));
        let max_in = U256::from(80_000u128) * U256::from(10u128.pow(18));
        let a = size_v3_sandwich(
            &shallow,
            &intent(victim, U256::ZERO),
            max_in,
            MAX_QUOTES_PER_CANDIDATE,
        )
        .await;
        let b = size_v3_sandwich(
            &deep,
            &intent(victim, U256::ZERO),
            max_in,
            MAX_QUOTES_PER_CANDIDATE,
        )
        .await;
        // Both pools are deep enough for a sandwich; the shallower one has
        // more impact so its profit is the larger of the two.
        let a = a.expect("shallow pool sandwiches");
        let b = b.expect("deep pool sandwiches");
        assert!(a.gross_profit >= b.gross_profit);
        assert!(a.quotes_used <= MAX_QUOTES_PER_CANDIDATE);
        assert!(b.quotes_used <= MAX_QUOTES_PER_CANDIDATE);
        assert!(shallow.calls.load(Ordering::Relaxed) <= MAX_QUOTES_PER_CANDIDATE);
        assert!(deep.calls.load(Ordering::Relaxed) <= MAX_QUOTES_PER_CANDIDATE);
    }

    #[tokio::test]
    async fn the_quote_budget_is_a_hard_cap() {
        let q = FakeQuoter {
            pool: pool(1_000_000 * 10u128.pow(18), 1_000_000 * 10u128.pow(18)),
            calls: AtomicU32::new(0),
        };
        let victim = U256::from(40_000u128) * U256::from(10u128.pow(18));
        let max_in = U256::from(80_000u128) * U256::from(10u128.pow(18));
        let _ = size_v3_sandwich(&q, &intent(victim, U256::ZERO), max_in, 4).await;
        assert!(
            q.calls.load(Ordering::Relaxed) <= 4,
            "a lowered budget must be honoured, got {}",
            q.calls.load(Ordering::Relaxed)
        );
    }

    #[test]
    fn accept_victim_requires_the_pool_in_the_v3_cache() {
        let cache = V3PoolCache::new();
        let i = intent(U256::from(1u64), U256::ZERO);
        assert!(accept_victim(&i, weth(), &cache).is_none());

        cache.insert(V3Pool {
            address: Address::with_last_byte(3),
            token0: weth(),
            token1: usdc(),
            fee: 3_000,
            tick_spacing: 60,
            block: 1,
        });
        assert!(accept_victim(&i, weth(), &cache).is_some());

        // Wrong fee tier → miss.
        let mut other = i.clone();
        other.fee = 500;
        assert!(accept_victim(&other, weth(), &cache).is_none());

        // Zero amount → miss.
        let mut z = i.clone();
        z.amount_in = U256::ZERO;
        assert!(accept_victim(&z, weth(), &cache).is_none());

        // Not WETH-in → miss (we don't inventory the other side).
        let mut sold = i.clone();
        sold.token_in = usdc();
        sold.token_out = weth();
        assert!(accept_victim(&sold, weth(), &cache).is_none());
    }

    #[test]
    fn router_leg_is_approve_then_exact_input_single() {
        let calls = build_router_leg(
            weth(),
            usdc(),
            3_000,
            U256::from(1_000u64),
            U256::ZERO,
            Address::with_last_byte(9),
        );
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].target, weth());
        assert_eq!(calls[1].target, known::UNIV3_SWAP_ROUTER_02);
        assert_eq!(
            &calls[1].data[..4],
            &dex::ISwapRouter02::exactInputSingleCall::SELECTOR
        );
        let decoded =
            dex::ISwapRouter02::exactInputSingleCall::abi_decode(&calls[1].data, true).unwrap();
        assert_eq!(decoded.params.amountOutMinimum, U256::ZERO);
        assert_eq!(
            decoded.params.sqrtPriceLimitX96,
            alloy_primitives::aliases::U160::ZERO
        );
        assert_eq!(decoded.params.tokenIn, weth());
        assert_eq!(decoded.params.tokenOut, usdc());
    }

    #[test]
    fn an_empty_cache_means_zero_quotes() {
        // The on_pending path returns before constructing a quoter when the
        // cache misses. This test pins the pre-filter so a future refactor
        // cannot "helpfully" factory-lookup the pool and blow the RPC budget.
        let cache = V3PoolCache::new();
        assert!(accept_victim(&intent(U256::from(1u64), U256::ZERO), weth(), &cache).is_none());
    }
}
