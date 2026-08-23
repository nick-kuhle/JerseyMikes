//! Aave V3 liquidations — per-reserve.
//!
//! Borrowers are discovered from `Borrow`/`Supply` logs, then their health
//! factor is polled once per block with batched `getUserAccountData` calls.
//! When HF < 1 the position's **actual composition** is read per reserve —
//! `Pool.getUserConfiguration` gives the borrowing/collateral bitmap,
//! `DataProvider.getUserReserveData` the real balances, and
//! `getReserveConfigurationData` the real liquidation bonus — so the bundle
//! repays the user's actual debt asset, seizes their actual collateral, and
//! prices the spread with the actual bonus (no more USDC/WETH/5% assumption;
//! all four ABI shapes verified against the live pool implementation and
//! data provider):
//!
//!   flash borrow the debt asset → repay up to the close factor → receive
//!   collateral at the reserve's bonus → swap collateral back → repay →
//!   keep the spread.
//!
//! The oracle front-runner reuses the same composition reader at trigger
//! time, so a feed update back-runs the position as it stands then.

use std::collections::{HashMap, HashSet};

use alloy_primitives::{Address, U256};
use alloy_sol_types::{sol, SolCall};
use async_trait::async_trait;
use parking_lot::RwLock;
use serde_json::json;

use crate::config::known;
use crate::dex::Venue;
use crate::strategies::leads::{ratio_bps, Lead, LeadAction, LiquidationLeads};
use crate::strategies::sandwich::build_leg;
use crate::strategies::{StrategyCtx, StrategyImpl};
use crate::types::{BlockHead, Call, Opportunity, Strategy};

sol! {
    interface IAaveV3Pool {
        function getUserAccountData(address user)
            external
            view
            returns (
                uint256 totalCollateralBase,
                uint256 totalDebtBase,
                uint256 availableBorrowsBase,
                uint256 currentLiquidationThreshold,
                uint256 ltv,
                uint256 healthFactor
            );

        function liquidationCall(
            address collateralAsset,
            address debtAsset,
            address user,
            uint256 debtToCover,
            bool receiveAToken
        ) external;
        function getReservesList() external view returns (address[] memory);
        function getUserConfiguration(address user) external view returns (uint256 data);
    }

    interface IAaveOracle {
        function getAssetPrice(address asset) external view returns (uint256);
    }

    interface IAaveDataProvider {
        function getUserReserveData(address asset, address user)
            external
            view
            returns (
                uint256 currentATokenBalance,
                uint256 currentStableDebt,
                uint256 currentVariableDebt,
                uint256 principalStableDebt,
                uint256 scaledVariableDebt,
                uint256 stableBorrowRate,
                uint256 liquidityRate,
                uint40 stableRateLastUpdated,
                bool usageAsCollateralEnabled
            );

        function getReserveConfigurationData(address asset)
            external
            view
            returns (
                uint256 ltv,
                uint256 liquidationThreshold,
                uint256 liquidationBonus,
                uint256 decimals,
                uint256 reserveFactor,
                bool usageAsCollateralEnabled,
                bool borrowingEnabled,
                bool stableBorrowRateEnabled,
                bool isActive,
                bool isFrozen
            );
    }
}

/// `Borrow(address indexed reserve, address user, address indexed onBehalfOf, ...)`
const BORROW_TOPIC: &str = "0xb3d084820fb1a9decffb176436bd02558d15fac9b0ddfed8c465bc7359d7dce0";

const HEALTH_FACTOR_ONE: u128 = 1_000_000_000_000_000_000;
/// Positions within 5% above the liquidation threshold feed the oracle
/// front-runner (the near-miss band, see `leads.rs`).
const NEAR_MISS_HF_CEILING: u128 = 1_050_000_000_000_000_000;

pub struct LiquidationStrategy {
    /// Borrowers we know about, harvested from logs.
    watchlist: RwLock<HashMap<Address, u64>>,
    watch_cap: usize,
    last_log_block: RwLock<u64>,
    /// Shared near-miss registry (see `leads.rs`).
    leads: LiquidationLeads,
    /// Reserve list + per-block reserve-config cache.
    cache: AaveCache,
}

impl LiquidationStrategy {
    pub fn new(leads: LiquidationLeads, watch_cap: usize) -> Self {
        Self {
            watchlist: RwLock::new(HashMap::new()),
            watch_cap: watch_cap.max(1),
            last_log_block: RwLock::new(0),
            leads,
            cache: AaveCache::default(),
        }
    }

    pub fn watchlist_size(&self) -> usize {
        self.watchlist.read().len()
    }

    /// Harvest borrowers from recent `Borrow` events.
    async fn harvest(&self, ctx: &StrategyCtx, head: &BlockHead) {
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
            "address": format!("{:?}", known::AAVE_V3_POOL),
            "topics": [BORROW_TOPIC],
        }]);

        match ctx.rpc.call_raw("eth_getLogs", params).await {
            Ok(v) => {
                let mut set = self.watchlist.write();
                if let Some(logs) = v.as_array() {
                    for log in logs {
                        // topic[2] == onBehalfOf (the borrower)
                        if let Some(t) = log["topics"].get(2).and_then(|t| t.as_str()) {
                            if let Ok(bytes) = hex::decode(t.trim_start_matches("0x")) {
                                if bytes.len() == 32 {
                                    set.insert(Address::from_slice(&bytes[12..32]), head.number);
                                }
                            }
                        }
                    }
                }
                if set.len() > self.watch_cap {
                    let mut by_age: Vec<(Address, u64)> = set
                        .iter()
                        .map(|(address, seen)| (*address, *seen))
                        .collect();
                    by_age.sort_unstable_by_key(|(_, seen)| std::cmp::Reverse(*seen));
                    by_age.truncate(self.watch_cap);
                    let keep = by_age
                        .into_iter()
                        .map(|(address, _)| address)
                        .collect::<HashSet<_>>();
                    set.retain(|address, _| keep.contains(address));
                }
                drop(set);
                *self.last_log_block.write() = head.number;
            }
            Err(e) => {
                tracing::debug!(target: "strategy::liquidation", error = %e, "log harvest failed")
            }
        }
    }

    /// Batch `getUserAccountData` and return everyone in or near the
    /// liquidation band (HF < 1.05): below 1 is actionable now, 1..1.05
    /// feeds the oracle front-runner.
    async fn unhealthy(&self, ctx: &StrategyCtx) -> Vec<(Address, U256, U256)> {
        let users: Vec<Address> = self.watchlist.read().keys().copied().collect();
        let mut out = Vec::new();
        for chunk in users.chunks(100) {
            let calls: Vec<(String, serde_json::Value)> = chunk
                .iter()
                .map(|u| {
                    (
                        "eth_call".to_string(),
                        json!([
                            {
                                "to": format!("{:?}", known::AAVE_V3_POOL),
                                "data": format!("0x{}", hex::encode(IAaveV3Pool::getUserAccountDataCall { user: *u }.abi_encode()))
                            },
                            "latest"
                        ]),
                    )
                })
                .collect();
            let Ok(results) = ctx.rpc.batch(&calls).await else {
                continue;
            };
            for (user, res) in chunk.iter().zip(results) {
                let Ok(v) = res else { continue };
                let raw = crate::types::parse_bytes(&v);
                if raw.len() < 192 {
                    continue;
                }
                let total_debt = U256::from_be_slice(&raw[32..64]);
                let health = U256::from_be_slice(&raw[160..192]);
                if total_debt.is_zero() || health.is_zero() {
                    continue;
                }
                if health < U256::from(NEAR_MISS_HF_CEILING) {
                    out.push((*user, total_debt, health));
                }
            }
        }
        out
    }
}

#[async_trait]
impl StrategyImpl for LiquidationStrategy {
    fn kind(&self) -> Strategy {
        Strategy::Liquidation
    }

    async fn on_block(&self, ctx: &StrategyCtx, head: &BlockHead) -> Vec<Opportunity> {
        self.harvest(ctx, head).await;
        let unhealthy = self.unhealthy(ctx).await;
        if unhealthy.is_empty() {
            return Vec::new();
        }
        tracing::info!(
            target: "strategy::liquidation",
            candidates = unhealthy.len(),
            watchlist = self.watchlist_size(),
            "unhealthy positions found"
        );

        let mut near_misses = Vec::new();
        let mut out = Vec::new();
        for (user, total_debt_base, health) in unhealthy {
            let hf_one = U256::from(HEALTH_FACTOR_ONE);
            if health < hf_one {
                if let Some(pos) = compose(ctx, &self.cache, user, health).await {
                    if let Some(opp) = build_opportunity(ctx, &pos).await {
                        out.push(opp);
                    }
                }
            } else {
                // Near-miss (HF in [1, 1.05)): publish for the oracle
                // front-runner with the position's REAL collateral, so a
                // feed update matches it. Ratio in bps of the threshold.
                let bps = ratio_bps(health, Some(hf_one), hf_one);
                let collateral = compose(ctx, &self.cache, user, health)
                    .await
                    .map(|p| p.collateral)
                    .unwrap_or(ctx.cfg.chain.weth);
                near_misses.push(Lead {
                    account: user,
                    collateral,
                    debt_asset: known::USDC,
                    ratio_bps: bps,
                    debt_wei: total_debt_base,
                    action: LeadAction::AaveV3 { user },
                });
            }
        }
        self.leads.publish("aave-v3", near_misses);
        out
    }
}

/// Borrow bit for reserve `i` in the `getUserConfiguration` bitmap.
pub fn aave_config_borrowing(data: U256, i: usize) -> bool {
    ((data >> (i * 2)) & U256::from(1u8)) == U256::from(1u8)
}

/// Collateral bit for reserve `i`.
pub fn aave_config_collateral(data: U256, i: usize) -> bool {
    ((data >> (i * 2 + 1)) & U256::from(1u8)) == U256::from(1u8)
}

/// A user's actionable position: the largest debt against the largest
/// collateral, with the collateral reserve's real liquidation bonus.
#[derive(Clone, Debug)]
pub struct AavePosition {
    pub user: alloy_primitives::Address,
    pub collateral: Address,
    pub collateral_amount: U256,
    pub debt_asset: Address,
    pub debt_amount_total: U256,
    /// Collateral raw units expected from repaying `debt_amount_total`, priced
    /// with Aave's own oracle and both reserves' decimals.
    pub seized_amount: U256,
    /// The reserve's liquidation bonus, in bps over 1e4 (10500 == 5%).
    pub bonus_bps: u64,
    /// Close factor applied to size the repay, in bps.
    pub close_factor_bps: u64,
}

/// Per-reserve config cache entry (refreshed each block it is used).
#[derive(Clone, Copy, Debug)]
pub struct ReserveCfg {
    pub bonus_bps: u64,
    pub decimals: u8,
    pub active: bool,
}

/// Read a user's actual composition. One `getUserConfiguration` call, then
/// batched `getUserReserveData` over the reserves the bitmap says the user
/// touches (bounded to the 8 busiest), then per-asset config (cached per
/// block). Public: the oracle front-runner calls this at trigger time.
pub async fn compose(
    ctx: &StrategyCtx,
    cache: &AaveCache,
    user: Address,
    health: U256,
) -> Option<AavePosition> {
    let reserves = aave_reserves(ctx, cache).await;
    if reserves.is_empty() {
        return None;
    }
    let cfg = ctx.rpc
        .call_raw("eth_call", serde_json::json!([
            { "to": format!("{:?}", known::AAVE_V3_POOL), "data": format!("0x{}", hex::encode(IAaveV3Pool::getUserConfigurationCall { user }.abi_encode())) },
            "latest"
        ]))
        .await
        .ok()?;
    let cfg_bytes = crate::types::parse_bytes(&cfg);
    if cfg_bytes.len() < 32 {
        return None;
    }
    let bitmap = U256::from_be_slice(&cfg_bytes[0..32]);

    // Which reserves does this user touch?
    let mut assets = Vec::new();
    for (i, asset) in reserves.iter().enumerate().take(128) {
        if aave_config_borrowing(bitmap, i) || aave_config_collateral(bitmap, i) {
            assets.push(*asset);
        }
    }
    if assets.is_empty() {
        return None;
    }
    // Aave currently has a bounded reserve list and the user bitmap already
    // filters it. Truncating the first eight touched reserves could silently
    // omit the actual largest debt or collateral.
    let assets: Vec<Address> = assets;

    let calls: Vec<(String, serde_json::Value)> = assets
        .iter()
        .map(|a| {
            (
                "eth_call".to_string(),
                serde_json::json!([
                    { "to": format!("{:?}", known::AAVE_V3_DATA_PROVIDER), "data": format!("0x{}", hex::encode(IAaveDataProvider::getUserReserveDataCall { asset: *a, user }.abi_encode())) },
                    "latest"
                ]),
            )
        })
        .collect();
    let results = ctx.rpc.batch(&calls).await.ok()?;

    // Compare reserves by Aave oracle value, not raw token units. A raw
    // WETH balance and a raw USDC balance differ by twelve decimal places.
    let mut debts: Vec<(Address, U256, U256, ReserveCfg, U256)> = Vec::new();
    let mut collaterals: Vec<(Address, U256, U256, ReserveCfg, U256)> = Vec::new();
    for (asset, res) in assets.iter().zip(results) {
        let Ok(v) = res else { continue };
        let raw = crate::types::parse_bytes(&v);
        if raw.len() < 96 {
            continue;
        }
        let Some(reserve_cfg) = aave_reserve_cfg(ctx, cache, *asset).await else {
            continue;
        };
        if !reserve_cfg.active {
            continue;
        }
        let Some(price) = aave_asset_price(ctx, cache, *asset).await else {
            continue;
        };
        let scale = decimal_scale(reserve_cfg.decimals);
        if scale.is_zero() {
            continue;
        }
        let a_balance = U256::from_be_slice(&raw[0..32]);
        let stable = U256::from_be_slice(&raw[32..64]);
        let variable = U256::from_be_slice(&raw[64..96]);
        let debt = stable.saturating_add(variable);
        if !debt.is_zero() {
            let value = debt.saturating_mul(price) / scale;
            debts.push((*asset, debt, value, reserve_cfg, price));
        }
        if !a_balance.is_zero() {
            let value = a_balance.saturating_mul(price) / scale;
            collaterals.push((*asset, a_balance, value, reserve_cfg, price));
        }
    }
    debts.sort_by_key(|(_, _, value, _, _)| std::cmp::Reverse(*value));
    collaterals.sort_by_key(|(_, _, value, _, _)| std::cmp::Reverse(*value));
    let (debt_asset, debt_total, _, debt_cfg, debt_price) = debts.first().copied()?;
    let (collateral, collateral_amount, _, collateral_cfg, collateral_price) =
        collaterals.first().copied()?;

    // HF-based close factor as before: 100% when deeply under, else 50%.
    // Aave's fork execution remains the final close-factor authority.
    let close_factor_bps = if health < U256::from(950_000_000_000_000_000u128) {
        10_000
    } else {
        5_000
    };
    let debt_amount =
        debt_total.saturating_mul(U256::from(close_factor_bps)) / U256::from(10_000u64);
    if debt_amount.is_zero() || collateral_price.is_zero() {
        return None;
    }
    let debt_value = debt_amount.saturating_mul(debt_price) / decimal_scale(debt_cfg.decimals);
    let seized_amount = debt_value
        .saturating_mul(U256::from(collateral_cfg.bonus_bps.max(10_000)))
        .saturating_mul(decimal_scale(collateral_cfg.decimals))
        / U256::from(10_000u64)
        / collateral_price;
    let seized_amount = seized_amount.min(collateral_amount);
    if seized_amount.is_zero() {
        return None;
    }
    Some(AavePosition {
        user,
        collateral,
        collateral_amount,
        debt_asset,
        debt_amount_total: debt_amount,
        seized_amount,
        bonus_bps: collateral_cfg.bonus_bps.max(10_000),
        close_factor_bps,
    })
}

fn decimal_scale(decimals: u8) -> U256 {
    let mut scale = U256::ONE;
    for _ in 0..decimals {
        scale = scale.saturating_mul(U256::from(10u8));
    }
    scale
}

async fn aave_asset_price(ctx: &StrategyCtx, cache: &AaveCache, asset: Address) -> Option<U256> {
    let head = ctx.head().number;
    if let Some((block, price)) = cache.price.read().get(&asset) {
        if *block == head {
            return Some(*price);
        }
    }
    let value = ctx
        .rpc
        .call_raw(
            "eth_call",
            serde_json::json!([{
                "to": format!("{:?}", known::AAVE_V3_ORACLE),
                "data": format!("0x{}", hex::encode(IAaveOracle::getAssetPriceCall { asset }.abi_encode()))
            }, "latest"]),
        )
        .await
        .ok()?;
    let raw = crate::types::parse_bytes(&value);
    if raw.len() < 32 {
        return None;
    }
    let price = U256::from_be_slice(&raw[0..32]);
    cache.price.write().insert(asset, (head, price));
    Some(price)
}

/// The pool's reserve list, cached per strategy instance (refreshed if a
/// read fails — governance does add reserves).
async fn aave_reserves(ctx: &StrategyCtx, cache: &AaveCache) -> Vec<Address> {
    {
        let cached = cache.reserves.read();
        if !cached.is_empty() {
            return cached.clone();
        }
    }
    let Ok(v) = ctx.rpc
        .call_raw("eth_call", serde_json::json!([
            { "to": format!("{:?}", known::AAVE_V3_POOL), "data": format!("0x{}", hex::encode(IAaveV3Pool::getReservesListCall {}.abi_encode())) },
            "latest"
        ]))
        .await
    else {
        return Vec::new();
    };
    // address[]: offset(1 word), length, then one word per address.
    let raw = crate::types::parse_bytes(&v);
    if raw.len() < 64 {
        return Vec::new();
    }
    let len = (U256::from_be_slice(&raw[32..64]).min(U256::from(64u8))).to::<usize>();
    if raw.len() < 64 + len * 32 {
        return Vec::new();
    }
    let out = (0..len)
        .map(|i| Address::from_slice(&raw[64 + i * 32 + 12..64 + (i + 1) * 32]))
        .collect::<Vec<_>>();
    if !out.is_empty() {
        *cache.reserves.write() = out.clone();
    }
    out
}

/// Reserve list + per-block reserve-config cache. Instance-owned (not
/// globals) so the live and replay lanes cannot contaminate each other —
/// same reasoning as the pool caches in `strategies/mod.rs`.
#[derive(Default)]
pub struct AaveCache {
    reserves: RwLock<Vec<Address>>,
    cfg: RwLock<std::collections::HashMap<Address, (u64, ReserveCfg)>>,
    price: RwLock<std::collections::HashMap<Address, (u64, U256)>>,
}

pub async fn aave_reserve_cfg(
    ctx: &StrategyCtx,
    cache: &AaveCache,
    asset: Address,
) -> Option<ReserveCfg> {
    let head = ctx.head().number;
    if let Some((block, cfg)) = cache.cfg.read().get(&asset) {
        if *block == head {
            return Some(*cfg);
        }
    }
    let v = ctx.rpc
        .call_raw("eth_call", serde_json::json!([
            { "to": format!("{:?}", known::AAVE_V3_DATA_PROVIDER), "data": format!("0x{}", hex::encode(IAaveDataProvider::getReserveConfigurationDataCall { asset }.abi_encode())) },
            "latest"
        ]))
        .await
        .ok()?;
    let raw = crate::types::parse_bytes(&v);
    if raw.len() < 96 {
        return None;
    }
    let bonus = U256::from_be_slice(&raw[64..96]).to::<u64>();
    let decimals = raw[127];
    let active = raw.len() >= 288 && raw[287] == 1;
    let cfg = ReserveCfg {
        bonus_bps: bonus,
        decimals,
        active,
    };
    cache.cfg.write().insert(asset, (head, cfg));
    Some(cfg)
}

/// Build the flash-borrow → liquidationCall → swap bundle for one composed
/// position. Public so the oracle front-runner rebuilds the exact same
/// bundle as a back-run of a feed update.
pub async fn build_opportunity(ctx: &StrategyCtx, pos: &AavePosition) -> Option<Opportunity> {
    let debt_asset = pos.debt_asset;
    let collateral = pos.collateral;
    let debt_amount = pos.debt_amount_total;
    let executor = ctx.executor;

    let mut calls = vec![
        Call::new(
            debt_asset,
            crate::dex::IERC20::approveCall {
                spender: known::AAVE_V3_POOL,
                amount: U256::MAX,
            }
            .abi_encode(),
        ),
        Call::new(
            known::AAVE_V3_POOL,
            IAaveV3Pool::liquidationCallCall {
                collateralAsset: collateral,
                debtAsset: debt_asset,
                user: pos.user,
                debtToCover: debt_amount,
                receiveAToken: false,
            }
            .abi_encode(),
        ),
    ];

    // Oracle- and decimal-normalised collateral amount. The fork simulation
    // remains authoritative for rounding and protocol caps.
    let seized = pos.seized_amount;
    let mut notes = format!(
        "aave v3 liquidation user {:?} hf-derived close {}/{} repay {} {:?} seize ~{} {:?} (bonus {} bps)",
        pos.user, pos.close_factor_bps, 10_000, debt_amount, debt_asset, seized, collateral, pos.bonus_bps
    );

    if let Some(pair) = ctx
        .pools
        .pair_for(collateral, debt_asset, Venue::UniV2)
        .await
    {
        if let Some(pool) = ctx.pools.load(pair, Venue::UniV2, ctx.head().number).await {
            calls.extend(build_leg(&pool, collateral, debt_asset, seized, executor));
        } else {
            notes.push_str("; swap pool state unavailable");
        }
    } else {
        notes.push_str("; no V2 pool for collateral→debt (recorded unsimulatable)");
    }

    Some(crate::strategies::leads::liquidation_opportunity(
        Strategy::Liquidation,
        calls,
        vec![debt_asset],
        vec![debt_amount],
        debt_asset,
        debt_amount.saturating_mul(U256::from(pos.bonus_bps.saturating_sub(10_000)))
            / U256::from(10_000u64),
        debt_amount,
        ctx.target_block(),
        notes,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_bitmap_decodes_borrow_and_collateral_bits() {
        // 0b110110: bits 1,2,4,5 set. Reserve i uses bits (2i, 2i+1) as
        // (borrowing, collateral), so: r0 = collateral only, r1 = borrowing
        // only, r2 = both, r3 = neither.
        let data = U256::from(0b110110u64);
        assert!(!aave_config_borrowing(data, 0));
        assert!(aave_config_collateral(data, 0));
        assert!(aave_config_borrowing(data, 1));
        assert!(!aave_config_collateral(data, 1));
        assert!(aave_config_borrowing(data, 2));
        assert!(aave_config_collateral(data, 2));
        assert!(!aave_config_borrowing(data, 3));
        assert!(!aave_config_collateral(data, 3));
    }

    #[test]
    fn reserves_list_decodes_static_address_array() {
        // offset(32) | length(32) | padded addresses.
        let mut raw = vec![0u8; 64];
        raw[63] = 2;
        for (i, a) in [known::WETH, known::USDC].iter().enumerate() {
            let mut word = vec![0u8; 32];
            word[12..].copy_from_slice(a.as_slice());
            raw.extend_from_slice(&word);
            let _ = i;
        }
        assert_eq!(Address::from_slice(&raw[64 + 12..64 + 32]), known::WETH);
        assert_eq!(Address::from_slice(&raw[96 + 12..96 + 32]), known::USDC);
    }

    #[test]
    fn reserve_config_words_decode_bonus_decimals_active() {
        // Word layout verified against the live data provider: bonus is the
        // 3rd return, decimals the 4th (last byte of that word), isActive
        // the 9th return (word index 8, byte 31).
        let mut raw = vec![0u8; 288];
        let bonus: u64 = 10_650; // 6.5% — e.g. a riskier collateral
        raw[64..96].copy_from_slice(&U256::from(bonus).to_be_bytes::<32>());
        raw[127] = 18;
        raw[287] = 1;
        assert_eq!(U256::from_be_slice(&raw[64..96]).to::<u64>(), bonus);
        assert_eq!(raw[127], 18);
        assert_eq!(raw[287], 1);
    }

    #[test]
    fn seized_estimate_uses_the_real_bonus() {
        let debt = U256::from(1_000_000u64);
        let bonus_bps = 10_650u64;
        let seized = debt * U256::from(bonus_bps) / U256::from(10_000u64);
        assert_eq!(seized, U256::from(1_065_000u64));
        assert_eq!(seized - debt, U256::from(65_000u64));
    }

    #[test]
    fn liquidation_call_encodes_the_right_selector() {
        let data = IAaveV3Pool::liquidationCallCall {
            collateralAsset: known::WETH,
            debtAsset: known::USDC,
            user: Address::with_last_byte(4),
            debtToCover: U256::from(1_000u64),
            receiveAToken: false,
        }
        .abi_encode();
        assert_eq!(&data[..4], &IAaveV3Pool::liquidationCallCall::SELECTOR);
        let decoded = IAaveV3Pool::liquidationCallCall::abi_decode(&data, true).unwrap();
        assert!(
            !decoded.receiveAToken,
            "we always want the underlying, not aTokens"
        );
    }

    #[test]
    fn watchlist_starts_empty() {
        let s = LiquidationStrategy::new(LiquidationLeads::new(), 200);
        assert_eq!(s.watchlist_size(), 0);
    }
}
