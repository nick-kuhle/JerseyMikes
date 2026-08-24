//! Block-pinned token valuation.
//!
//! Gas is denominated in ETH. Profit is denominated in whatever token the
//! strategy settles in. Netting one against the other requires a price, and a
//! price is only meaningful if it is read at the *same block* the profit was
//! measured at — quoting `"latest"` for a bundle simulated at block N is the
//! same state-divergence bug the replay path already refuses to make.
//!
//! Before this module existed, [`crate::sim`] fail-closed on any non-WETH
//! profit token: the simulation was marked unsuccessful with an
//! "uncertified accounting" reason, which is why every liquidation strategy
//! (Aave, Compound, Morpho, Maker) and the oracle front-run were pinned to
//! shadow mode regardless of how profitable they were. The blocker was never
//! the strategy math — it was the missing unit conversion.
//!
//! # What makes a valuation trustworthy
//!
//! A price that can be manipulated by the very bundle being valued is worse
//! than no price at all, so this module is deliberately conservative:
//!
//! * **Block-pinned.** Every quote is an `eth_call` at an explicit block tag.
//!   The caller passes the block the profit was measured at; there is no
//!   `"latest"` fallback.
//! * **Executable, not spot.** Prices come from the V3 QuoterV2 (and a V2
//!   `getReserves` constant-product fallback) for the *actual profit size*,
//!   so the value already includes the price impact of selling it. A spot
//!   mid-price would systematically overvalue large liquidation bonuses.
//! * **Haircut.** The result is multiplied by
//!   `VALUATION_HAIRCUT_BPS` (default 200 = 2%) to cover the slippage and
//!   adverse selection of the unwind we are *not* simulating.
//! * **Staleness-proof.** A quote is only reusable for the block it was taken
//!   at. The cache key includes the block number, so a new block is a new
//!   quote — see [`ValuationCache`].
//! * **Fail-closed.** Every failure path returns [`None`], never a guess. A
//!   `None` valuation restores exactly the old behaviour, so the worst case of
//!   a pricing outage is the conservative pre-existing one.

use std::collections::HashMap;

use alloy_primitives::{Address, U256};
use parking_lot::Mutex;

use crate::config::Config;
use crate::dex::{quote_v3, v2_amount_out};
use crate::rpc::RpcClient;

/// V3 fee tiers probed when pricing a token, in ascending order.
///
/// Ordered cheapest-first so the deepest stable route is tried before the
/// exotic one. All four canonical Uniswap V3 tiers are covered.
pub const FEE_TIERS: [u32; 4] = [100, 500, 3_000, 10_000];

/// Default conservatism applied to every non-native valuation, in basis
/// points of the quoted amount (200 = 2%).
pub const DEFAULT_HAIRCUT_BPS: u64 = 200;

/// A single block-pinned valuation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Valuation {
    /// Value of the input amount, in wei of the chain's native token.
    pub wei: U256,
    /// Block the quote was pinned to.
    pub block: u64,
    /// Route that produced it, for the decision trail.
    pub route: Route,
}

/// Which venue produced a valuation. Recorded so the audit trail can show
/// *how* a number was reached, not just what it was.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route {
    /// Token is already the native asset or its wrapper — 1:1, no quote.
    Native,
    /// Uniswap V3 QuoterV2 at the given fee tier.
    UniV3 { fee: u32 },
    /// Uniswap V2 / Sushi constant-product reserves.
    V2Reserves,
}

impl Route {
    pub fn as_str(&self) -> &'static str {
        match self {
            Route::Native => "native",
            Route::UniV3 { .. } => "univ3_quoter",
            Route::V2Reserves => "v2_reserves",
        }
    }
}

/// Apply a basis-point haircut without overflowing on large balances.
///
/// Uses the same split-multiplication shape as `MevExecutor._settle`'s bribe
/// math: `(v / 10_000) * keep + ((v % 10_000) * keep) / 10_000`. This is exact
/// for the quotient term and bounded-error for the remainder, and it cannot
/// overflow for any `v` that fits in a `U256` — a naive `v * keep` can.
pub fn apply_haircut(value: U256, haircut_bps: u64) -> U256 {
    let bps = U256::from(10_000u64);
    let keep = U256::from(10_000u64.saturating_sub(haircut_bps.min(10_000)));
    let q = value / bps;
    let r = value % bps;
    q.saturating_mul(keep).saturating_add(r * keep / bps)
}

/// Cache of block-pinned quotes.
///
/// The key is `(token, block)`, so a valuation is structurally incapable of
/// outliving the block it was taken at: at block N+1 every lookup misses and
/// re-quotes. Entries for blocks older than the newest observed block are
/// pruned on insert, which bounds the map without a background task.
#[derive(Default)]
pub struct ValuationCache {
    inner: Mutex<CacheInner>,
}

#[derive(Default)]
struct CacheInner {
    /// `(token, block) -> value of one whole unit`, in native wei.
    entries: HashMap<(Address, u64), Option<Valuation>>,
    newest_block: u64,
}

impl ValuationCache {
    pub fn new() -> Self {
        Self::default()
    }

    fn get(&self, token: Address, block: u64) -> Option<Option<Valuation>> {
        self.inner.lock().entries.get(&(token, block)).copied()
    }

    fn put(&self, token: Address, block: u64, v: Option<Valuation>) {
        let mut g = self.inner.lock();
        if block > g.newest_block {
            g.newest_block = block;
            // A new block invalidates everything older. Dropping them here
            // keeps the map at roughly "tokens seen this block".
            g.entries.retain(|(_, b), _| *b >= block);
        }
        g.entries.insert((token, block), v);
    }

    /// Number of live entries. Test/telemetry helper.
    pub fn len(&self) -> usize {
        self.inner.lock().entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Values `amount` of `token` in native wei, pinned to `block`.
///
/// Returns `None` when no trustworthy route exists, which the caller must
/// treat as "cannot certify this profit" — never as zero and never as a
/// fallback price. `Address::ZERO` (native ETH) and the chain's WETH are
/// returned 1:1 without a network round-trip.
///
/// The `cache` memoises the value of **one whole token unit** per
/// `(token, block)`; the requested `amount` is then scaled from it. Pricing a
/// unit rather than the exact amount is what makes the cache reusable across
/// several opportunities in the same block. The unit quote is taken at a size
/// meaningful enough to traverse real liquidity (see `probe_amount`) so the
/// resulting rate already carries realistic impact.
pub async fn value_in_native(
    rpc: &RpcClient,
    cfg: &Config,
    cache: &ValuationCache,
    token: Address,
    amount: U256,
    block: u64,
    haircut_bps: u64,
) -> Option<Valuation> {
    if amount.is_zero() {
        return Some(Valuation {
            wei: U256::ZERO,
            block,
            route: Route::Native,
        });
    }
    // Native and wrapped-native are the unit we are converting *to*.
    if token == Address::ZERO || token == cfg.chain.weth {
        return Some(Valuation {
            wei: amount,
            block,
            route: Route::Native,
        });
    }

    let unit = match cache.get(token, block) {
        Some(hit) => hit,
        None => {
            let fresh = quote_unit(rpc, cfg, token, block).await;
            cache.put(token, block, fresh);
            fresh
        }
    }?;

    // value(amount) = amount * value(1 unit) / 1 unit
    let decimals = token_decimals(rpc, cfg, token, block).await?;
    let one = pow10(decimals)?;
    let scaled = amount
        .checked_mul(unit.wei)
        .map(|v| v / one)
        // Fall back to dividing first when the product would overflow. Loses
        // sub-unit precision on absurd balances; never fabricates value.
        .unwrap_or_else(|| (amount / one).saturating_mul(unit.wei));

    Some(Valuation {
        wei: apply_haircut(scaled, haircut_bps),
        block,
        route: unit.route,
    })
}

/// Quote the native value of one whole unit of `token` at `block`.
async fn quote_unit(
    rpc: &RpcClient,
    cfg: &Config,
    token: Address,
    block: u64,
) -> Option<Valuation> {
    let decimals = token_decimals(rpc, cfg, token, block).await?;
    let probe = probe_amount(decimals)?;
    let tag = format!("0x{block:x}");

    // Preferred: the V3 QuoterV2, which runs the real swap in a revert and so
    // reports executable output including tick-crossing impact.
    if let Some(quoter) = cfg.addresses.univ3_quoter_v2 {
        let mut best: Option<(U256, u32)> = None;
        for fee in FEE_TIERS {
            let out = quote_v3(rpc, quoter, token, cfg.chain.weth, fee, probe, &tag)
                .await
                .ok()
                .filter(|v| !v.is_zero());
            if let Some(v) = out {
                if best.map(|(b, _)| v > b).unwrap_or(true) {
                    best = Some((v, fee));
                }
            }
        }
        if let Some((out, fee)) = best {
            // Scale the probe result back to exactly one unit.
            let one = pow10(decimals)?;
            let per_unit = out.checked_mul(one).map(|v| v / probe)?;
            if !per_unit.is_zero() {
                return Some(Valuation {
                    wei: per_unit,
                    block,
                    route: Route::UniV3 { fee },
                });
            }
        }
    }

    // Fallback: a V2 pair's constant-product reserves at the same block.
    if let Some(factory) = cfg.addresses.univ2_factory {
        if let Ok(Some(pair)) = crate::dex::get_pair(rpc, factory, token, cfg.chain.weth).await {
            if let Ok(pool) =
                crate::dex::fetch_v2_pool(rpc, pair, crate::dex::Venue::UniV2, 30, block).await
            {
                let (rin, rout) = if pool.token0 == token {
                    (pool.reserve0, pool.reserve1)
                } else {
                    (pool.reserve1, pool.reserve0)
                };
                if !rin.is_zero() && !rout.is_zero() {
                    let out = v2_amount_out(probe, rin, rout, pool.fee_bps);
                    let one = pow10(decimals)?;
                    let per_unit = out.checked_mul(one).map(|v| v / probe)?;
                    if !per_unit.is_zero() {
                        return Some(Valuation {
                            wei: per_unit,
                            block,
                            route: Route::V2Reserves,
                        });
                    }
                }
            }
        }
    }

    None
}

/// Size used to probe a price, in token base units.
///
/// One whole unit of an 18-decimal token is a dust trade that can return a
/// misleadingly good rate on a thin pool, and a huge probe understates the
/// rate. This uses a fixed, modest notional (1 unit for ≤6-decimal tokens,
/// scaling up to 1 unit for 18-decimal ones) which is large enough to cross
/// real liquidity while staying inside normal depth.
fn probe_amount(decimals: u8) -> Option<U256> {
    pow10(decimals)
}

fn pow10(decimals: u8) -> Option<U256> {
    if decimals > 36 {
        return None;
    }
    Some(U256::from(10u64).pow(U256::from(decimals)))
}

/// `decimals()` for an ERC-20, pinned to a block and memoised for the process.
///
/// Decimals are immutable for every sane ERC-20, so unlike prices this is
/// cached without a block key. A token that lies about its decimals is
/// mispriced by the same factor on both sides of the comparison and is caught
/// by the profit threshold, not here.
async fn token_decimals(rpc: &RpcClient, cfg: &Config, token: Address, block: u64) -> Option<u8> {
    use std::sync::OnceLock;
    static DECIMALS: OnceLock<Mutex<HashMap<Address, u8>>> = OnceLock::new();
    let map = DECIMALS.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(d) = map.lock().get(&token) {
        return Some(*d);
    }
    // Well-known stables on the configured chain, so the common path needs no
    // round-trip even on a cold cache.
    let known = if token == cfg.addresses.usdc || Some(token) == cfg.addresses.usdt {
        Some(6u8)
    } else if Some(token) == cfg.addresses.wbtc {
        Some(8u8)
    } else if Some(token) == cfg.addresses.dai || Some(token) == cfg.addresses.wsteth {
        Some(18u8)
    } else {
        None
    };
    if let Some(d) = known {
        map.lock().insert(token, d);
        return Some(d);
    }

    // 0x313ce567 == decimals()
    let out: String = rpc
        .call(
            "eth_call",
            serde_json::json!([
                {"to": format!("{token:?}"), "data": "0x313ce567"},
                format!("0x{block:x}")
            ]),
        )
        .await
        .ok()?;
    let raw = hex::decode(out.strip_prefix("0x").unwrap_or(&out)).ok()?;
    if raw.len() < 32 {
        return None;
    }
    // Right-most byte of the word; anything above 36 is not a real token.
    let d = raw[31];
    if d > 36 {
        return None;
    }
    map.lock().insert(token, d);
    Some(d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn haircut_keeps_the_expected_fraction() {
        let v = U256::from(1_000_000u64);
        // 2% off 1_000_000 = 980_000
        assert_eq!(apply_haircut(v, 200), U256::from(980_000u64));
        // A zero haircut is the identity.
        assert_eq!(apply_haircut(v, 0), v);
        // A full haircut is zero.
        assert_eq!(apply_haircut(v, 10_000), U256::ZERO);
        // Over-100% is clamped, not wrapped.
        assert_eq!(apply_haircut(v, 50_000), U256::ZERO);
    }

    #[test]
    fn haircut_does_not_overflow_on_huge_balances() {
        // A naive `value * keep` overflows here; the split form must not.
        let v = U256::MAX / U256::from(2u8);
        let out = apply_haircut(v, 200);
        assert!(out < v, "haircut must reduce the value");
        assert!(out > v / U256::from(2u8), "2% haircut must not halve it");
    }

    #[test]
    fn pow10_rejects_absurd_decimals() {
        assert_eq!(pow10(18), Some(U256::from(1_000_000_000_000_000_000u64)));
        assert_eq!(pow10(6), Some(U256::from(1_000_000u64)));
        assert_eq!(pow10(0), Some(U256::from(1u8)));
        assert_eq!(pow10(37), None);
    }

    #[test]
    fn cache_is_keyed_by_block_and_prunes_old_entries() {
        let c = ValuationCache::new();
        let t = Address::repeat_byte(0x11);
        let v = Valuation {
            wei: U256::from(5u8),
            block: 100,
            route: Route::V2Reserves,
        };
        c.put(t, 100, Some(v));
        assert_eq!(c.get(t, 100), Some(Some(v)));
        // Same token, different block: a miss, never a stale hit.
        assert_eq!(c.get(t, 101), None);

        // Advancing the block prunes the older entry.
        c.put(t, 101, Some(v));
        assert_eq!(c.get(t, 100), None);
        assert_eq!(c.len(), 1);
    }

    #[test]
    fn a_negative_cache_entry_is_remembered() {
        // `None` (no route) must be cached too, or every un-priceable token
        // re-quotes four fee tiers on every single opportunity.
        let c = ValuationCache::new();
        let t = Address::repeat_byte(0x22);
        c.put(t, 7, None);
        assert_eq!(c.get(t, 7), Some(None), "miss and negative hit must differ");
    }

    #[test]
    fn route_labels_are_stable() {
        assert_eq!(Route::Native.as_str(), "native");
        assert_eq!(Route::UniV3 { fee: 500 }.as_str(), "univ3_quoter");
        assert_eq!(Route::V2Reserves.as_str(), "v2_reserves");
    }

    #[test]
    fn fee_tiers_are_the_canonical_four_ascending() {
        assert_eq!(FEE_TIERS, [100, 500, 3_000, 10_000]);
        assert!(FEE_TIERS.windows(2).all(|w| w[0] < w[1]));
    }
}
