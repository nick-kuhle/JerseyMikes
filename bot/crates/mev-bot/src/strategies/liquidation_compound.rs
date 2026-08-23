//! Compound V3 (Comet) liquidations.
//!
//! Comet is the odd one out among lending protocols: liquidation is a
//! *two-step storefront* rather than a single call.
//!
//! 1. `absorb(absorber, [accounts])` — moves the underwater accounts'
//!    collateral into protocol reserves and wipes their debt (excess loss is
//!    socialised to reserves). The absorber receives nothing in this call.
//! 2. `buyCollateral(asset, minAmount, baseAmount, recipient)` — buys the
//!    absorbed collateral **at a discount** (`quoteCollateral` prices it with
//!    `storeFrontPriceFactor × (1 − liquidationFactor)`), paying base token.
//!
//! The discount is the liquidation reward. The bundle therefore has to do
//! both steps atomically plus a swap leg:
//!
//!   flash USDC → approve Comet → absorb → buyCollateral (per asset) →
//!   swap collateral back to USDC → repay → keep the discount spread.
//!
//! **Discovery.** Accounts are harvested from `Supply`/`Withdraw` events on
//! the market (both indexed sides), capped and evicted least-recently-seen
//! first, then polled with batched `isLiquidatable` calls — a single boolean
//! view Comet exposes exactly for this purpose. Only for the (rare) liquidatable
//! ones do we spend the per-asset `userCollateral` and `quoteCollateral` reads.
//!
//! **Traps avoided.** `buyCollateral` reverts `NotForSale` when reserves are
//! healthy — an absorb usually pushes reserves below `targetReserves`, which
//! is why the two calls belong in one batch. A too-small `minAmount` bound
//! only caps slippage; a mispriced bundle dies in `MevExecutor`'s profit
//! guard without costing gas (private orderflow, reverting bundles are
//! dropped).
//!
//! **Honesty.** No near-miss leads are published for Compound: Comet exposes
//! only the boolean `isLiquidatable`, not a continuous health factor, so
//! "just above threshold" is not observable off-chain and the oracle
//! front-runner cannot use it. Accounts that flip underwater on a feed update
//! are simply caught on the next block tick.
//!
//! **Not yet.** Multiple accounts per absorb call (one batched bundle, the
//! gas saving is real); non-listed collateral assets with no V2 pool (the
//! opportunity is still recorded, unsimulatable); the base-market-of-market
//! (Comet on other bases than USDC).

use std::collections::HashMap;

use alloy_primitives::{Address, U256};
use alloy_sol_types::{sol, SolCall};
use async_trait::async_trait;
use parking_lot::RwLock;
use serde_json::json;

use crate::dex::{self, IERC20};
use crate::strategies::leads::liquidation_opportunity;
use crate::strategies::sandwich::build_leg;
use crate::strategies::{StrategyCtx, StrategyImpl};
use crate::types::{BlockHead, Call, Opportunity, Strategy};

sol! {
    interface IComet {
        struct AssetInfo {
            uint8 offset;
            address asset;
            address priceFeed;
            uint64 scale;
            uint64 borrowCollateralFactor;
            uint64 liquidateCollateralFactor;
            uint64 liquidationFactor;
            uint128 supplyCap;
        }

        function isLiquidatable(address account) external view returns (bool);
        function absorb(address absorber, address[] calldata accounts) external;
        function buyCollateral(address asset, uint256 minAmount, uint256 baseAmount, address recipient) external;
        function userCollateral(address account, address asset) external view returns (uint128 balance, uint128 reserved);
        function quoteCollateral(address asset, uint256 baseAmount) external view returns (uint256);
        function numAssets() external view returns (uint8);
        function getAssetInfo(uint8 index) external view returns (AssetInfo memory info);
        function baseToken() external view returns (address);
    }
}

/// `Supply(address indexed src, address indexed to, uint256 amount)`
const SUPPLY_TOPIC: &str = "0xd1cf3d156d5f8f0d50f6c122ed609cec09d35c9b9fb3fff6ea0959134dae424e";
/// `Withdraw(address indexed src, address indexed to, uint256 amount)`
const WITHDRAW_TOPIC: &str = "0x9b1bfa7fa9ee420a16e124f794c35ac9f90472acc99140eb2f6447c714cad8eb";

/// How much of the discount we book as expected profit before simulation
/// corrects it. Typical configurations sit near 3–8%; 2% is the conservative
/// bookkeeping number.
const EXPECTED_DISCOUNT_BPS: u64 = 200;

/// A candidate account: address + the block we last saw it act, for LRU
/// eviction when the watchlist grows past `watch_cap`.
#[derive(Clone, Copy)]
struct Watched {
    account: Address,
    last_seen_block: u64,
}

pub struct CompoundLiquidationStrategy {
    /// Capped, most-recently-active-first watchlist.
    watchlist: RwLock<Vec<Watched>>,
    last_log_block: RwLock<u64>,
    /// Collateral assets of the market, enumerated once per boot (and again
    /// if a read fails — governance does add assets).
    assets: RwLock<Vec<Address>>,
    /// Maximum watchlist size (`LIQUIDATION_WATCH_CAP`).
    watch_cap: usize,
}

impl CompoundLiquidationStrategy {
    pub fn new(watch_cap: usize) -> Self {
        Self {
            watchlist: RwLock::new(Vec::new()),
            last_log_block: RwLock::new(0),
            assets: RwLock::new(Vec::new()),
            watch_cap,
        }
    }

    fn touch(&self, account: Address, block: u64) {
        let cap = self.watch_cap.max(1);
        let mut wl = self.watchlist.write();
        if let Some(w) = wl.iter_mut().find(|w| w.account == account) {
            w.last_seen_block = block;
        } else {
            wl.push(Watched {
                account,
                last_seen_block: block,
            });
            if wl.len() > cap {
                // Evict the stalest entry; the cap bounds per-block RPC.
                if let Some(pos) = wl
                    .iter()
                    .enumerate()
                    .min_by_key(|(_, w)| w.last_seen_block)
                    .map(|(i, _)| i)
                {
                    wl.swap_remove(pos);
                }
            }
        }
        // Keep the vector roughly newest-first so eviction is O(1) amortised.
        wl.sort_unstable_by_key(|w| std::cmp::Reverse(w.last_seen_block));
    }

    pub fn watchlist_size(&self) -> usize {
        self.watchlist.read().len()
    }

    /// Enumerate the market's collateral assets (once; retried when empty).
    async fn asset_list(&self, ctx: &StrategyCtx) -> Vec<Address> {
        let Some(comet) = ctx.cfg.addresses.compound_v3_usdc else {
            return Vec::new(); // Comet not present on this chain
        };
        {
            let a = self.assets.read();
            if !a.is_empty() {
                return a.clone();
            }
        }
        let Ok(v) = ctx
            .rpc
            .call_raw(
                "eth_call",
                json!([
                    { "to": format!("{:?}", comet), "data": format!("0x{}", hex::encode(IComet::numAssetsCall {}.abi_encode())) },
                    "latest"
                ]),
            )
            .await
        else {
            return Vec::new();
        };
        let raw = crate::types::parse_bytes(&v);
        if raw.len() < 32 {
            return Vec::new();
        }
        let n = raw[31] as usize; // uint8 right-aligned in the last byte
        let mut calls = Vec::with_capacity(n);
        for i in 0..n {
            calls.push((
                "eth_call".to_string(),
                json!([
                    { "to": format!("{:?}", comet), "data": format!("0x{}", hex::encode(IComet::getAssetInfoCall { index: i as u8 }.abi_encode())) },
                    "latest"
                ]),
            ));
        }
        let Ok(results) = ctx.rpc.batch(&calls).await else {
            return Vec::new();
        };
        let mut out = Vec::with_capacity(n);
        for res in results {
            let Ok(v) = res else { continue };
            let raw = crate::types::parse_bytes(&v);
            // AssetInfo is a static struct; `asset` is the second word.
            if raw.len() >= 64 {
                out.push(Address::from_slice(&raw[32..44]));
            }
        }
        if !out.is_empty() {
            *self.assets.write() = out.clone();
        }
        out
    }

    /// Harvest account candidates from recent Supply/Withdraw activity.
    async fn harvest(&self, ctx: &StrategyCtx, head: &BlockHead) {
        let Some(comet) = ctx.cfg.addresses.compound_v3_usdc else {
            return; // Comet not present on this chain
        };
        let from = {
            let last = *self.last_log_block.read();
            if last == 0 {
                head.number.saturating_sub(2_000)
            } else if head.number <= last {
                return;
            } else {
                last + 1
            }
        };
        let params = json!([{
            "fromBlock": format!("0x{from:x}"),
            "toBlock": format!("0x{:x}", head.number),
            "address": format!("{:?}", comet),
            "topics": [[SUPPLY_TOPIC, WITHDRAW_TOPIC]],
        }]);
        match ctx.rpc.call_raw("eth_getLogs", params).await {
            Ok(v) => {
                if let Some(logs) = v.as_array() {
                    for log in logs {
                        for t in [1usize, 2usize] {
                            if let Some(topic) = log["topics"].get(t).and_then(|t| t.as_str()) {
                                if let Ok(bytes) = hex::decode(topic.trim_start_matches("0x")) {
                                    if bytes.len() == 32 && bytes[12..] != [0u8; 20] {
                                        self.touch(
                                            Address::from_slice(&bytes[12..32]),
                                            head.number,
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
                *self.last_log_block.write() = head.number;
            }
            Err(e) => {
                tracing::debug!(target: "strategy::liquidation_compound", error = %e, "log harvest failed")
            }
        }
    }

    /// Batched `isLiquidatable`; returns the liquidatable subset.
    async fn liquidatable(&self, ctx: &StrategyCtx) -> Vec<Address> {
        let Some(comet) = ctx.cfg.addresses.compound_v3_usdc else {
            return Vec::new(); // Comet not present on this chain
        };
        let accounts: Vec<Address> = self.watchlist.read().iter().map(|w| w.account).collect();
        let mut out = Vec::new();
        for chunk in accounts.chunks(100) {
            let calls: Vec<(String, serde_json::Value)> = chunk
                .iter()
                .map(|a| {
                    (
                        "eth_call".to_string(),
                        json!([
                            { "to": format!("{:?}", comet), "data": format!("0x{}", hex::encode(IComet::isLiquidatableCall { account: *a }.abi_encode())) },
                            "latest"
                        ]),
                    )
                })
                .collect();
            let Ok(results) = ctx.rpc.batch(&calls).await else {
                continue;
            };
            for (account, res) in chunk.iter().zip(results) {
                let Ok(v) = res else { continue };
                let raw = crate::types::parse_bytes(&v);
                if raw.len() >= 32 && raw[31] == 1 {
                    out.push(*account);
                }
            }
        }
        out
    }

    /// Per-asset collateral balance of one account (only called for the
    /// liquidatable few).
    async fn collateral_of(
        &self,
        ctx: &StrategyCtx,
        account: Address,
        assets: &[Address],
    ) -> Vec<(Address, U256)> {
        let Some(comet) = ctx.cfg.addresses.compound_v3_usdc else {
            return Vec::new(); // Comet not present on this chain
        };
        let calls: Vec<(String, serde_json::Value)> = assets
            .iter()
            .map(|a| {
                (
                    "eth_call".to_string(),
                    json!([
                        { "to": format!("{:?}", comet), "data": format!("0x{}", hex::encode(IComet::userCollateralCall { account, asset: *a }.abi_encode())) },
                        "latest"
                    ]),
                )
            })
            .collect();
        let Ok(results) = ctx.rpc.batch(&calls).await else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for (asset, res) in assets.iter().zip(results) {
            let Ok(v) = res else { continue };
            let raw = crate::types::parse_bytes(&v);
            if raw.len() >= 32 {
                let balance = U256::from_be_slice(&raw[0..32]);
                if !balance.is_zero() {
                    out.push((*asset, balance));
                }
            }
        }
        out
    }

    /// Collateral units per 1e9 base units (the quote is linear in base), so
    /// `base_needed = seized * 1e9 / rate`.
    async fn quote_rate(&self, ctx: &StrategyCtx, asset: Address) -> Option<U256> {
        let Some(comet) = ctx.cfg.addresses.compound_v3_usdc else {
            return None; // Comet not present on this chain
        };
        let Ok(v) = ctx
            .rpc
            .call_raw(
                "eth_call",
                json!([
                    { "to": format!("{:?}", comet), "data": format!("0x{}", hex::encode(IComet::quoteCollateralCall { asset, baseAmount: U256::from(1_000_000_000u64) }.abi_encode())) },
                    "latest"
                ]),
            )
            .await
        else {
            return None;
        };
        let raw = crate::types::parse_bytes(&v);
        if raw.len() < 32 {
            return None;
        }
        let rate = U256::from_be_slice(&raw[0..32]);
        if rate.is_zero() {
            None
        } else {
            Some(rate)
        }
    }
}

impl Default for CompoundLiquidationStrategy {
    fn default() -> Self {
        Self::new(200)
    }
}

/// Build the absorb → buyCollateral → swap bundle for one account. Public so
/// the oracle front-runner can reuse it verbatim (it does not today — Comet
/// publishes no near-miss band — but the shape is shared with the strategies
/// that do).
#[allow(clippy::too_many_arguments)]
pub async fn build_opportunity(
    ctx: &StrategyCtx,
    account: Address,
    balances: &[(Address, U256)],
    rates: &HashMap<Address, U256>,
    target_block: u64,
) -> Option<Opportunity> {
    let Some(comet) = ctx.cfg.addresses.compound_v3_usdc else {
        return None; // Comet not present on this chain
    };
    let usdc = ctx.cfg.addresses.usdc;
    if balances.is_empty() {
        return None;
    }
    let executor = ctx.executor;
    let mut calls = vec![Call::new(
        usdc,
        IERC20::approveCall {
            spender: comet,
            amount: U256::MAX,
        }
        .abi_encode(),
    )];
    calls.push(Call::new(
        comet,
        IComet::absorbCall {
            absorber: executor,
            accounts: vec![account],
        }
        .abi_encode(),
    ));

    let mut flash = U256::ZERO;
    let mut expected = U256::ZERO;
    let mut swap_assets = Vec::new();
    for (asset, seized) in balances {
        let Some(rate) = rates.get(asset) else {
            continue;
        };
        // base needed to buy the whole seized balance, rounded up.
        let base_needed = seized
            .saturating_mul(U256::from(1_000_000_000u64))
            .div_ceil(*rate);
        if base_needed.is_zero() {
            continue;
        }
        calls.push(Call::new(
            comet,
            IComet::buyCollateralCall {
                asset: *asset,
                // Slippage bound: at most 3% worse than the storefront quote.
                minAmount: seized.saturating_mul(U256::from(9_700u64)) / U256::from(10_000u64),
                baseAmount: base_needed,
                recipient: executor,
            }
            .abi_encode(),
        ));
        flash += base_needed;
        expected += base_needed * U256::from(EXPECTED_DISCOUNT_BPS) / U256::from(10_000u64);
        swap_assets.push((*asset, *seized));
    }
    if flash.is_zero() {
        return None;
    }

    let mut notes = format!(
        "compound v3 absorb+buyCollateral account {account:?} assets {:?}",
        swap_assets.iter().map(|(a, _)| a).collect::<Vec<_>>()
    );
    for (asset, seized) in swap_assets {
        if let Some(pair) = ctx.pools.pair_for(asset, usdc, dex::Venue::UniV2).await {
            if let Some(pool) = ctx
                .pools
                .load(pair, dex::Venue::UniV2, ctx.head().number)
                .await
            {
                calls.extend(build_leg(&pool, asset, usdc, seized, executor));
            } else {
                notes.push_str("; swap pool missing (unsimulatable leg recorded)");
            }
        }
    }

    Some(liquidation_opportunity(
        Strategy::LiquidationCompound,
        calls,
        vec![usdc],
        vec![flash],
        usdc,
        expected,
        flash,
        target_block,
        notes,
    ))
}

#[async_trait]
impl StrategyImpl for CompoundLiquidationStrategy {
    fn kind(&self) -> Strategy {
        Strategy::LiquidationCompound
    }

    async fn on_block(&self, ctx: &StrategyCtx, head: &BlockHead) -> Vec<Opportunity> {
        self.harvest(ctx, head).await;
        let accounts = self.liquidatable(ctx).await;
        if accounts.is_empty() {
            return Vec::new();
        }
        tracing::info!(
            target: "strategy::liquidation_compound",
            candidates = accounts.len(),
            watchlist = self.watchlist_size(),
            "liquidatable Comet accounts found"
        );

        let assets = self.asset_list(ctx).await;
        if assets.is_empty() {
            tracing::debug!(target: "strategy::liquidation_compound", "asset list unavailable");
            return Vec::new();
        }

        // Quote rates are per-asset, not per-account: fetch once per block.
        let mut rates = HashMap::new();
        for asset in &assets {
            if let Some(r) = self.quote_rate(ctx, *asset).await {
                rates.insert(*asset, r);
            }
        }

        let mut out = Vec::new();
        for account in accounts {
            let balances = self.collateral_of(ctx, account, &assets).await;
            if let Some(opp) =
                build_opportunity(ctx, account, &balances, &rates, ctx.target_block()).await
            {
                out.push(opp);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::known;

    #[test]
    fn absorb_encodes_the_comet_selector() {
        #[allow(clippy::useless_vec)]
        let data = IComet::absorbCall {
            absorber: Address::with_last_byte(1),
            accounts: vec![Address::with_last_byte(2)],
        }
        .abi_encode();
        assert_eq!(&data[..4], &IComet::absorbCall::SELECTOR);
        // Verified against the live mainnet Comet implementation dispatcher
        // (proxy 0xc3d688B6.. -> CometWithExtendedAssetList 0x83D49126..).
        assert_eq!(IComet::absorbCall::SELECTOR, [0xc3, 0xce, 0xcf, 0xd2]);
    }

    #[test]
    fn buy_collateral_encodes_the_verified_selector() {
        // keccak("buyCollateral(address,uint256,uint256,address)")[0..4]
        // = 0xe4e6e779, present in the live implementation dispatcher.
        let data = IComet::buyCollateralCall {
            asset: known::WETH,
            minAmount: U256::ONE,
            baseAmount: U256::ONE,
            recipient: Address::ZERO,
        }
        .abi_encode();
        assert_eq!(&data[..4], &[0xe4, 0xe6, 0xe7, 0x79]);
    }

    #[test]
    fn watchlist_is_capped_and_evicts_the_stalest() {
        let cap = 8;
        let s = CompoundLiquidationStrategy::new(cap);
        let b1 = Address::with_last_byte(1);
        let b2 = Address::with_last_byte(2);
        s.touch(b1, 10);
        s.touch(b2, 11);
        // Fill past the cap with fresh accounts; b1 (older) must go, b2 stays.
        for i in 3..cap + 3 {
            s.touch(Address::with_last_byte(i as u8), 12);
        }
        let wl = s.watchlist.read().clone();
        assert!(wl.len() <= cap);
        assert!(wl.iter().all(|w| w.last_seen_block >= 12));
        // Touching an existing account refreshes instead of duplicating.
        let before = wl.len();
        s.touch(wl[0].account, 13);
        assert_eq!(s.watchlist.read().len(), before);
    }

    #[test]
    fn asset_info_decodes_asset_from_the_low_half_of_the_second_word() {
        // Static struct: word 0 = offset, word 1 = asset (right-aligned).
        // The first 12 bytes of word 1 are padding and must not leak into
        // the address.
        let mut raw = [0u8; 64];
        raw[32..44].copy_from_slice(&[0xaa; 12]);
        raw[44..64].copy_from_slice(known::WETH.as_slice());
        assert_eq!(Address::from_slice(&raw[44..64]), known::WETH);
    }
}
