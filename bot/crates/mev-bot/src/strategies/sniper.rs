//! New-token sniper.
//!
//! Watches for `PairCreated` on the V2 factories and for the mempool
//! transactions that make a token tradable (`addLiquidityETH`, `openTrading`,
//! `enableTrading`, …). For every new pool it builds an atomic
//! **buy → sell round trip** and hands it to the simulator.
//!
//! That round trip is doing double duty:
//!   * it is a honeypot / transfer-tax detector — if the sell leg reverts or
//!     returns far less than the buy leg cost, the token is a trap and the bot
//!     records it as such instead of buying,
//!   * on genuinely mispriced launches (liquidity added at a price that is off
//!     versus another pool) it is directly profitable, and the executor's profit
//!     guard means the bundle simply never lands otherwise.

use std::collections::HashSet;

use alloy_primitives::{Address, U256};
use async_trait::async_trait;
use parking_lot::RwLock;

use crate::dex::{self, Venue};
use crate::strategies::discovery::PoolDiscovery;
use crate::strategies::sandwich::build_leg;
use crate::strategies::{try_scan_pair_created, StrategyCtx, StrategyImpl};
use crate::types::{now_ms, BlockHead, Call, Opportunity, PendingTx, Strategy};

/// Selectors that typically flip a token from "untradable" to "tradable".
const GO_LIVE_SELECTORS: [&str; 6] = [
    "0xf305d719", // addLiquidityETH
    "0xe8078d94", // addLiquidity
    "0xc9567bf9", // openTrading
    "0x8a8c523c", // enableTrading
    "0x7d1db4a5", // setMaxTxAmount
    "0xa6334231", // removeLimits
];

pub struct SniperStrategy {
    seen_pairs: RwLock<HashSet<Address>>,
    last_log_block: RwLock<u64>,
    /// Tokens the honeypot check has already rejected.
    blacklist: RwLock<HashSet<Address>>,
}

impl Default for SniperStrategy {
    fn default() -> Self {
        Self::new()
    }
}

impl SniperStrategy {
    pub fn new() -> Self {
        Self {
            seen_pairs: RwLock::new(HashSet::new()),
            last_log_block: RwLock::new(0),
            blacklist: RwLock::new(HashSet::new()),
        }
    }

    pub fn blacklist(&self, token: Address) {
        self.blacklist.write().insert(token);
    }

    pub fn is_blacklisted(&self, token: Address) -> bool {
        self.blacklist.read().contains(&token)
    }

    pub fn seen_count(&self) -> usize {
        self.seen_pairs.read().len()
    }

    /// The core primitive: buy `size` of WETH worth of the token and
    /// immediately sell it all back, in one atomic batch.
    fn round_trip(
        &self,
        ctx: &StrategyCtx,
        pool: &dex::V2Pool,
        weth: Address,
        size: U256,
    ) -> Option<Vec<Call>> {
        let token = pool.other_token(weth)?;
        let bought = pool.amount_out(weth, size)?;
        if bought.is_zero() {
            return None;
        }
        let mut calls = build_leg(pool, weth, token, size, ctx.executor);
        // The post-buy state is what the sell leg trades against.
        let (after_buy, _) = pool.with_swap(weth, size)?;
        calls.extend(build_leg(&after_buy, token, weth, bought, ctx.executor));
        Some(calls)
    }
}

#[async_trait]
impl StrategyImpl for SniperStrategy {
    fn kind(&self) -> Strategy {
        Strategy::Sniper
    }

    async fn on_block(&self, ctx: &StrategyCtx, head: &BlockHead) -> Vec<Opportunity> {
        // Use the same bounded, reorg-overlapping window as pool discovery. The
        // cursor must advance only after a successful eth_getLogs call: an empty
        // result means "no pairs", while `None` means the provider failed and
        // this range must be retried on the next block.
        let cursor = *self.last_log_block.read();
        let (from, to) = PoolDiscovery::window(cursor, head.number);
        let Some(pairs) = try_scan_pair_created(&ctx.rpc, from, to).await else {
            tracing::debug!(
                target: "strategy::sniper",
                from,
                to,
                "pair-created scan failed; retaining cursor for retry"
            );
            return Vec::new();
        };
        *self.last_log_block.write() = to;

        let weth = ctx.cfg.chain.weth;
        let mut out = Vec::new();
        for (venue, pair) in pairs {
            if self.seen_pairs.read().contains(&pair) {
                continue;
            }
            // NOTE: use `venue` (correct per-factory) instead of hardcoded UniV2.
            let Some(pool) = ctx.pools.load(pair, venue, head.number).await else {
                // A transient pool read must remain retryable, just like the
                // shared discovery path. Marking it seen before this await would
                // permanently lose a newly-created pool during rate limiting.
                continue;
            };
            // The pair metadata was read successfully. Remember it now so the
            // overlap scan does not refetch the same pool every block. Dust is
            // handled by PoolDiscovery, which periodically rechecks liquidity.
            self.seen_pairs.write().insert(pair);
            // Only WETH-quoted pools, and only once they actually hold liquidity.
            let Some(token) = pool.other_token(weth) else {
                continue;
            };
            if self.is_blacklisted(token) {
                continue;
            }
            let Some((weth_reserve, _)) = pool.reserves_for(weth) else {
                continue;
            };
            if weth_reserve < U256::from(500_000_000_000_000_000u128) {
                continue; // < 0.5 WETH of liquidity: dust
            }

            // Size at 1% of the pool — enough to measure tax, small enough to exit.
            let size = (weth_reserve / U256::from(100u64)).min(ctx.max_position());
            let Some(calls) = self.round_trip(ctx, &pool, weth, size) else {
                continue;
            };

            out.push(Opportunity {
                id: uuid::Uuid::new_v4().to_string(),
                strategy: Strategy::Sniper,
                victim_hashes: Vec::new(),
                front_calls: calls,
                back_calls: Vec::new(),
                flash_tokens: vec![weth],
                flash_amounts: vec![size],
                profit_token: weth,
                // A round trip on an untaxed token is a small loss (2×30bps); the
                // simulator decides. We record the attempt either way.
                expected_profit_wei: U256::ZERO,
                notional_wei: size,
                target_block: ctx.target_block(),
                created_at_ms: now_ms(),
                notes: format!(
                    "new pair {pair:?} token {token:?} liquidity {weth_reserve} wei; atomic round-trip probe (honeypot/tax check)"
                ),
            });
        }
        out
    }

    /// A "go live" transaction in the mempool is the highest-value moment: the
    /// pool exists but nobody can trade yet. We queue the same round-trip probe
    /// behind it.
    async fn on_pending(&self, ctx: &StrategyCtx, tx: &PendingTx) -> Vec<Opportunity> {
        let Some(sel) = tx.selector() else {
            return Vec::new();
        };
        let sel_hex = format!("0x{}", hex::encode(sel));
        if !GO_LIVE_SELECTORS.contains(&sel_hex.as_str()) {
            return Vec::new();
        }
        let Some(target) = tx.to else {
            return Vec::new();
        };

        let weth = ctx.cfg.chain.weth;
        let head = ctx.head();

        // For addLiquidityETH the token is the first argument; for the token's own
        // "openTrading" the target *is* the token.
        let token = if tx.input.len() >= 36 {
            Address::from_slice(&tx.input[16..36])
        } else {
            target
        };
        let token = if token == Address::ZERO {
            target
        } else {
            token
        };
        if self.is_blacklisted(token) {
            return Vec::new();
        }

        let Some(pair) = ctx.pools.pair_for(weth, token, Venue::UniV2).await else {
            return Vec::new();
        };
        // A replayed go-live transaction is scored against the pool as it stood
        // at the start of its own block, not as it stands now.
        let Some(pool) = ctx.pool_at(pair, Venue::UniV2, tx.state_block(&head)).await else {
            return Vec::new();
        };
        let Some((weth_reserve, _)) = pool.reserves_for(weth) else {
            return Vec::new();
        };
        if weth_reserve.is_zero() {
            return Vec::new();
        }

        let size = (weth_reserve / U256::from(50u64)).min(ctx.max_position());
        let Some(calls) = self.round_trip(ctx, &pool, weth, size) else {
            return Vec::new();
        };

        vec![Opportunity {
            id: uuid::Uuid::new_v4().to_string(),
            strategy: Strategy::Sniper,
            victim_hashes: vec![tx.hash],
            front_calls: Vec::new(),
            // Snipe *after* the go-live tx, never in front of it.
            back_calls: calls,
            flash_tokens: vec![weth],
            flash_amounts: vec![size],
            profit_token: weth,
            expected_profit_wei: U256::ZERO,
            notional_wei: size,
            target_block: tx.target_block(&head, ctx.cfg.sim.target_block_offset),
            created_at_ms: now_ms(),
            notes: format!("go-live {sel_hex} on {target:?}; snipe probe for token {token:?} via pair {pair:?}"),
        }]
    }
}

/// Classify a simulated round trip. Used by the engine to grow the blacklist.
pub fn classify(spent: U256, returned: U256) -> TokenVerdict {
    if returned.is_zero() {
        return TokenVerdict::Honeypot;
    }
    if spent.is_zero() {
        return TokenVerdict::Unknown;
    }
    let bps = returned * U256::from(10_000u64) / spent;
    let bps = bps.min(U256::from(u64::MAX)).to::<u64>();
    match bps {
        0..=5_000 => TokenVerdict::Honeypot, // lost more than half: trap or extreme tax
        5_001..=9_800 => TokenVerdict::Taxed, // 2%+ round-trip cost beyond fees
        9_801..=10_000 => TokenVerdict::Clean, // just the 2×30bps AMM fee
        _ => TokenVerdict::Profitable,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenVerdict {
    Honeypot,
    Taxed,
    Clean,
    Profitable,
    Unknown,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_round_trips() {
        let one = U256::from(1_000_000u64);
        assert_eq!(classify(one, U256::ZERO), TokenVerdict::Honeypot);
        assert_eq!(
            classify(one, U256::from(400_000u64)),
            TokenVerdict::Honeypot
        );
        assert_eq!(classify(one, U256::from(900_000u64)), TokenVerdict::Taxed);
        assert_eq!(classify(one, U256::from(994_000u64)), TokenVerdict::Clean);
        assert_eq!(
            classify(one, U256::from(1_100_000u64)),
            TokenVerdict::Profitable
        );
    }

    #[test]
    fn blacklist_round_trips() {
        let s = SniperStrategy::new();
        let t = Address::with_last_byte(6);
        assert!(!s.is_blacklisted(t));
        s.blacklist(t);
        assert!(s.is_blacklisted(t));
    }

    #[test]
    fn go_live_selectors_are_four_bytes() {
        for s in GO_LIVE_SELECTORS {
            assert_eq!(s.len(), 10, "{s} must be 0x + 8 hex chars");
        }
    }
}
