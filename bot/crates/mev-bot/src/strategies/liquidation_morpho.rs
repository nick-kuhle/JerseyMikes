//! Morpho Blue liquidations.
//!
//! Morpho Blue is a singleton with one market per `(loan, collateral, oracle,
//! irm, lltv)` tuple. Two properties drive the design:
//!
//! 1. **Health is readable in two batched calls per position.**
//!    `position(id, user)` returns the whole position (supply shares, borrow
//!    shares, collateral) and `market(id)` returns the market totals, so debt
//!    and collateral are recovered without storage-slot spelunking. Health
//!    then mirrors the deployed contract exactly:
//!    `liquidatable ⇔ collateral·price/1e36·lltv < ceil(shares→assets)`.
//! 2. **The liquidation pays an incentive proportional to how low the LLTV
//!    is:** `incentive = min(1.15e18, 1 / (1 − 0.3·(1 − lltv)))` — seized
//!    collateral is worth ~4.3% extra at `lltv = 0.777` and caps at 15%.
//!
//! The bundle: flash the loan token → approve Blue → `liquidate(marketParams,
//! borrower, 0, borrowShares, "")` (full close; Blue allows repaying the whole
//! position) → swap the seized collateral back → repay. `liquidate` pulls the
//! repaid assets from the caller, so the approval is load-bearing.
//!
//! **Discovery.** Markets are self-seeding: activity events (`Supply`,
//! `SupplyCollateral`, `Borrow` — both the current six-field signature and the
//! pre-v1.1 five-field one, OR'd in one `eth_getLogs`) reveal which market ids
//! are alive *now*, which is the population worth watching. Borrowers come
//! from the same logs (indexed `onBehalf`). Markets are capped
//! (`MORPHO_MARKET_CAP`, most-recently-active first) and filtered to a
//! whitelist of loan/collateral tokens so the swap leg is something the bot
//! can actually price.
//!
//! **Traps avoided.** `repay-or-seize`: exactly one of `seizedAssets` /
//! `repaidShares` must be zero. Repaying more than the borrow balance
//! underflows and reverts — we always repay the exact observed share count.
//! The current interface is **v1.1**: `liquidate` (no `id` argument — it is
//! derived from the market params), `MarketParams` ordered
//! `(loan, collateral, oracle, irm, lltv)`, `position(bytes32,address)` for
//! reads. All selectors below were verified against the deployed bytecode at
//! `0xBBBB…FFCb`, which is not upgradable: this ABI is frozen on mainnet.
//!
//! **Not yet.** Partial liquidations sized to available pool liquidity
//! (always full close today), MetaMorpho vault positions, markets whose
//! collateral is not in the whitelist (recorded as leads only if their
//! collateral feeds a near-miss band the oracle strategy watches).

use std::collections::HashMap;

use alloy_primitives::{Address, B256, U256};
use alloy_sol_types::{sol, SolCall};
use async_trait::async_trait;
use parking_lot::RwLock;
use serde_json::json;

use crate::config::known;
use crate::dex::{self, IERC20};
use crate::strategies::leads::{
    liquidation_opportunity, ratio_bps, Lead, LeadAction, LiquidationLeads,
};
use crate::strategies::sandwich::build_leg;
use crate::strategies::{StrategyCtx, StrategyImpl};
use crate::types::{BlockHead, Call, Opportunity, Strategy};

sol! {
    interface IMorpho {
        struct MarketParams {
            address loanToken;
            address collateralToken;
            address oracle;
            address irm;
            uint256 lltv;
        }

        struct Market {
            uint128 totalSupplyAssets;
            uint128 totalSupplyShares;
            uint128 totalBorrowAssets;
            uint128 totalBorrowShares;
            uint128 lastUpdate;
            uint128 fee;
        }

        struct Position {
            uint256 supplyShares;
            uint128 borrowShares;
            uint128 collateral;
        }

        function market(bytes32 id) external view returns (Market memory market);
        function idToMarketParams(bytes32 id) external view returns (MarketParams memory marketParams);
        function position(bytes32 id, address user) external view returns (Position memory position);
        function liquidate(
            MarketParams memory marketParams,
            address borrower,
            uint256 seizedAssets,
            uint256 repaidShares,
            bytes memory data
        ) external returns (uint256, uint256);
    }

    interface IMorphoOracle {
        function price() external view returns (uint256);
    }
}

/// `Supply(bytes32,address,address,uint256,uint256)`
const SUPPLY_TOPIC: &str = "0xedf8870433c83823eb071d3df1caa8d008f12f6440918c20d75a3602cda30fe0";
/// `SupplyCollateral(bytes32,address,address,uint256)`
const SUPPLY_COLLATERAL_TOPIC: &str =
    "0xa3b9472a1399e17e123f3c2e6586c23e504184d504de59cdaa2b375e880c6184";
/// `Borrow(bytes32,address,address,uint256,uint256)` — pre-v1.1 signature.
const BORROW_V1_TOPIC: &str = "0x8e6fd4ede24c40b1817b419873d34541a59b1f39bb3ba1a7ed22832be0930306";
/// `Borrow(bytes32,address,address,address,uint256,uint256)` — v1.1 signature.
const BORROW_V11_TOPIC: &str = "0x570954540bed6b1304a87dfe815a5eda4a648f7097a16240dcd85c9b5fd42a43";

pub const MORPHO_BLUE: Address =
    alloy_primitives::address!("BBBBBbbBBb9cC5e90e3b3Af64bdAF62C37EEFFCb");

/// `ORACLE_PRICE_SCALE` in Morpho Blue (prices are 1e36-scaled).
const PRICE_SCALE: U256 = U256::from_limbs([0xB34B9F1000000000, 0xC097CE7BC90715, 0x0, 0x0]); // 1e36
const WAD: U256 = U256::from_limbs([0xDE0B6B3A7640000, 0x0, 0x0, 0x0]); // 1e18
const LIQUIDATION_CURSOR: U256 = U256::from_limbs([0x429D069189E0000, 0x0, 0x0, 0x0]); // 0.3e18
const MAX_LIQUIDATION_INCENTIVE: U256 = U256::from_limbs([0xFF59EE833B30000, 0x0, 0x0, 0x0]); // 1.15e18
/// SharesMathLib virtual offsets, mirrored so debt math matches the chain.
const VIRTUAL_SHARES: U256 = U256::from_limbs([0xF4240, 0, 0, 0]);
const VIRTUAL_ASSETS: U256 = U256::ONE;

/// A market on the watchlist.
struct MarketWatch {
    id: B256,
    /// Resolved lazily on first sight and cached (params are immutable).
    params: Option<IMorpho::MarketParams>,
    /// Borrower candidates, most-recently-active first, capped.
    borrowers: Vec<(Address, u64)>,
    last_active_block: u64,
}

pub struct MorphoLiquidationStrategy {
    markets: RwLock<HashMap<B256, MarketWatch>>,
    last_log_block: RwLock<u64>,
    market_cap: usize,
    borrower_cap: usize,
    leads: LiquidationLeads,
}

/// Pure re-implementation of the on-chain share→asset rounding so the health
/// test matches the contract to the wei (`toAssetsUp`).
pub fn to_assets_up(shares: U256, total_assets: U256, total_shares: U256) -> U256 {
    shares
        .saturating_mul(total_assets.saturating_add(VIRTUAL_ASSETS))
        .div_ceil(total_shares.saturating_add(VIRTUAL_SHARES))
}

/// The deployed liquidation incentive: min(1.15e18, 1/(1 − 0.3·(1 − lltv))).
pub fn liquidation_incentive(lltv: U256) -> U256 {
    let x = WAD.saturating_sub(lltv);
    let numerator = LIQUIDATION_CURSOR.saturating_mul(x) / WAD;
    let denom = WAD.saturating_sub(numerator);
    let uncapped = WAD.saturating_mul(WAD) / denom;
    uncapped.min(MAX_LIQUIDATION_INCENTIVE)
}

/// Mirror of `_isHealthy` in the deployed contract: healthy positions cannot
/// be liquidated. Prices are 1e36-scaled.
pub fn is_liquidatable(
    collateral: U256,
    collateral_price: U256,
    lltv: U256,
    borrowed: U256,
) -> bool {
    if borrowed.is_zero() {
        return false;
    }
    let max_borrow = (collateral.saturating_mul(collateral_price) / PRICE_SCALE) * lltv / WAD;
    max_borrow < borrowed
}

impl MorphoLiquidationStrategy {
    pub fn new(market_cap: usize, borrower_cap: usize, leads: LiquidationLeads) -> Self {
        Self {
            markets: RwLock::new(HashMap::new()),
            last_log_block: RwLock::new(0),
            market_cap,
            borrower_cap,
            leads,
        }
    }

    pub fn market_count(&self) -> usize {
        self.markets.read().len()
    }

    /// Markets we can price a swap for. The whitelist is deliberately short:
    /// the swap leg is what turns seized collateral into repayable loan
    /// token, and unknown tokens would make the bundle unsimulatable.
    fn actionable(params: &IMorpho::MarketParams) -> bool {
        let loan_ok = matches!(
            params.loanToken,
            known::USDC | known::DAI | known::USDT | known::WETH
        );
        let collateral_ok = matches!(
            params.collateralToken,
            known::WETH | known::WBTC | known::WSTETH
        );
        loan_ok && collateral_ok && !params.lltv.is_zero()
    }

    fn touch_market(&self, id: B256, block: u64) {
        let mut ms = self.markets.write();
        let cap = self.market_cap.max(1);
        if let Some(m) = ms.get_mut(&id) {
            m.last_active_block = block;
        } else {
            if ms.len() >= cap {
                if let Some(evict) = ms
                    .iter()
                    .min_by_key(|(_, m)| m.last_active_block)
                    .map(|(k, _)| *k)
                {
                    ms.remove(&evict);
                }
            }
            ms.insert(
                id,
                MarketWatch {
                    id,
                    params: None,
                    borrowers: Vec::new(),
                    last_active_block: block,
                },
            );
        }
    }

    fn touch_borrower(&self, id: B256, borrower: Address, block: u64) {
        let mut ms = self.markets.write();
        let Some(m) = ms.get_mut(&id) else { return };
        if let Some(slot) = m.borrowers.iter_mut().find(|(b, _)| *b == borrower) {
            slot.1 = block;
        } else {
            m.borrowers.push((borrower, block));
            if m.borrowers.len() > self.borrower_cap.max(1) {
                if let Some(pos) = m
                    .borrowers
                    .iter()
                    .enumerate()
                    .min_by_key(|(_, (_, b))| *b)
                    .map(|(i, _)| i)
                {
                    m.borrowers.swap_remove(pos);
                }
            }
        }
        m.borrowers
            .sort_unstable_by_key(|(_, b)| std::cmp::Reverse(*b));
    }

    /// Resolve market params for ids that do not have them yet.
    async fn resolve_params(&self, ctx: &StrategyCtx) {
        let unresolved: Vec<B256> = {
            let ms = self.markets.read();
            ms.values()
                .filter(|m| m.params.is_none())
                .map(|m| m.id)
                .collect()
        };
        if unresolved.is_empty() {
            return;
        }
        let calls: Vec<(String, serde_json::Value)> = unresolved
            .iter()
            .map(|id| {
                (
                    "eth_call".to_string(),
                    json!([
                        { "to": format!("{MORPHO_BLUE:?}"), "data": format!("0x{}", hex::encode(IMorpho::idToMarketParamsCall { id: *id }.abi_encode())) },
                        "latest"
                    ]),
                )
            })
            .collect();
        let Ok(results) = ctx.rpc.batch(&calls).await else {
            return;
        };
        let mut ms = self.markets.write();
        for (id, res) in unresolved.iter().zip(results) {
            let Ok(v) = res else { continue };
            let raw = crate::types::parse_bytes(&v);
            // MarketParams is a static struct of five words.
            if raw.len() < 160 {
                continue;
            }
            let params = IMorpho::MarketParams {
                loanToken: Address::from_slice(&raw[12..32]),
                collateralToken: Address::from_slice(&raw[44..64]),
                oracle: Address::from_slice(&raw[76..96]),
                irm: Address::from_slice(&raw[108..128]),
                lltv: U256::from_be_slice(&raw[128..160]),
            };
            if let Some(m) = ms.get_mut(id) {
                m.params = Some(params);
            }
        }
    }

    /// Harvest active markets and their borrowers from activity events.
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
            "address": format!("{MORPHO_BLUE:?}"),
            // Position-altering events; both Borrow generations.
            "topics": [[SUPPLY_TOPIC, SUPPLY_COLLATERAL_TOPIC, BORROW_V1_TOPIC, BORROW_V11_TOPIC]],
        }]);
        match ctx.rpc.call_raw("eth_getLogs", params).await {
            Ok(v) => {
                if let Some(logs) = v.as_array() {
                    for log in logs {
                        let Some(id_word) = log["topics"].get(1).and_then(|t| t.as_str()) else {
                            continue;
                        };
                        let Ok(id_bytes) = hex::decode(id_word.trim_start_matches("0x")) else {
                            continue;
                        };
                        if id_bytes.len() != 32 {
                            continue;
                        }
                        let id = B256::from_slice(&id_bytes);
                        self.touch_market(id, head.number);
                        // Borrower: the first *indexed address-ish* topic after
                        // the id differs between event generations (`onBehalf`
                        // is topics[2] in both; the extra v1.1 `receiver` is
                        // topics[3]). `caller` (unindexed) is not a position
                        // owner, so only topics[2] is harvested.
                        if let Some(topic) = log["topics"].get(2).and_then(|t| t.as_str()) {
                            if let Ok(bytes) = hex::decode(topic.trim_start_matches("0x")) {
                                if bytes.len() == 32 && bytes[12..] != [0u8; 20] {
                                    self.touch_borrower(
                                        id,
                                        Address::from_slice(&bytes[12..32]),
                                        head.number,
                                    );
                                }
                            }
                        }
                    }
                }
                *self.last_log_block.write() = head.number;
            }
            Err(e) => {
                tracing::debug!(target: "strategy::liquidation_morpho", error = %e, "log harvest failed")
            }
        }
        self.resolve_params(ctx).await;
    }

    /// Batched health poll. Returns liquidatable positions and (for the
    /// oracle strategy) publishes near-misses.
    async fn poll(
        &self,
        ctx: &StrategyCtx,
    ) -> Vec<(B256, IMorpho::MarketParams, Address, U256, U256, U256, U256)> {
        let snapshot: Vec<(B256, IMorpho::MarketParams, Vec<Address>)> = {
            let ms = self.markets.read();
            ms.values()
                .filter_map(|m| {
                    let p = m.params.as_ref()?;
                    // Only markets we can act on are polled deeply; the rest
                    // stay dormant (they were kept for context in the log).
                    if !Self::actionable(p) {
                        return None;
                    }
                    Some((
                        m.id,
                        p.clone(),
                        m.borrowers.iter().map(|(b, _)| *b).collect(),
                    ))
                })
                .collect()
        };
        let mut out = Vec::new();
        let mut near_misses = Vec::new();
        for (id, params, borrowers) in snapshot {
            if borrowers.is_empty() {
                continue;
            }
            // Market totals + oracle price, once per market.
            let Ok(market_v) = ctx
                .rpc
                .call_raw(
                    "eth_call",
                    json!([
                        { "to": format!("{MORPHO_BLUE:?}"), "data": format!("0x{}", hex::encode(IMorpho::marketCall { id }.abi_encode())) },
                        "latest"
                    ]),
                )
                .await
            else {
                continue;
            };
            let mraw = crate::types::parse_bytes(&market_v);
            // Market: six uint128 words; we need the borrow side (3rd, 4th).
            if mraw.len() < 128 {
                continue;
            }
            let total_borrow_assets = U256::from_be_slice(&mraw[64..96]);
            let total_borrow_shares = U256::from_be_slice(&mraw[96..128]);
            if total_borrow_shares.is_zero() {
                continue;
            }
            let Ok(price_v) = ctx
                .rpc
                .call_raw(
                    "eth_call",
                    json!([
                        { "to": format!("{:?}", params.oracle), "data": format!("0x{}", hex::encode(IMorphoOracle::priceCall {}.abi_encode())) },
                        "latest"
                    ]),
                )
                .await
            else {
                continue;
            };
            let praw = crate::types::parse_bytes(&price_v);
            if praw.len() < 32 {
                continue;
            }
            let price = U256::from_be_slice(&praw[0..32]);
            if price.is_zero() {
                continue;
            }

            let calls: Vec<(String, serde_json::Value)> = borrowers
                .iter()
                .map(|u| {
                    (
                        "eth_call".to_string(),
                        json!([
                            { "to": format!("{MORPHO_BLUE:?}"), "data": format!("0x{}", hex::encode(IMorpho::positionCall { id, user: *u }.abi_encode())) },
                            "latest"
                        ]),
                    )
                })
                .collect();
            let Ok(results) = ctx.rpc.batch(&calls).await else {
                continue;
            };
            for (user, res) in borrowers.iter().zip(results) {
                let Ok(v) = res else { continue };
                let praw = crate::types::parse_bytes(&v);
                if praw.len() < 96 {
                    continue;
                }
                let supply_shares = U256::from_be_slice(&praw[0..32]);
                let borrow_shares = U256::from_be_slice(&praw[32..64]);
                let collateral = U256::from_be_slice(&praw[64..96]);
                if supply_shares.is_zero() && borrow_shares.is_zero() && collateral.is_zero() {
                    continue;
                }
                let borrowed =
                    to_assets_up(borrow_shares, total_borrow_assets, total_borrow_shares);
                if borrowed.is_zero() || collateral.is_zero() {
                    continue;
                }
                if is_liquidatable(collateral, price, params.lltv, borrowed) {
                    out.push((
                        id,
                        params.clone(),
                        *user,
                        borrow_shares,
                        total_borrow_assets,
                        total_borrow_shares,
                        price,
                    ));
                } else {
                    // Near-miss band for the oracle front-runner: health
                    // expressed in bps of the threshold.
                    let collateral_scaled = collateral.saturating_mul(price) / PRICE_SCALE;
                    let bps = ratio_bps(collateral_scaled, Some(params.lltv), borrowed);
                    if bps < 10_500 {
                        near_misses.push(Lead {
                            account: *user,
                            collateral: params.collateralToken,
                            debt_asset: params.loanToken,
                            ratio_bps: bps,
                            debt_wei: borrowed,
                            action: LeadAction::Morpho {
                                market_id: id,
                                loan_token: params.loanToken,
                                collateral_token: params.collateralToken,
                                oracle: params.oracle,
                                irm: params.irm,
                                lltv: params.lltv,
                                borrower: *user,
                                borrow_shares,
                                total_borrow_assets,
                                total_borrow_shares,
                            },
                        });
                    }
                }
            }
        }
        self.leads.publish("morpho-blue", near_misses);
        out
    }
}

/// Read a Morpho oracle price in its protocol-defined 1e36 scale.
pub async fn oracle_price(ctx: &StrategyCtx, oracle: Address) -> Option<U256> {
    let value = ctx
        .rpc
        .call_raw(
            "eth_call",
            json!([{
                "to": format!("{oracle:?}"),
                "data": format!("0x{}", hex::encode(IMorphoOracle::priceCall {}.abi_encode()))
            }, "latest"]),
        )
        .await
        .ok()?;
    let raw = crate::types::parse_bytes(&value);
    (raw.len() >= 32).then(|| U256::from_be_slice(&raw[0..32]))
}

/// Build a full-close liquidation for a Morpho position. Public: the oracle
/// front-runner rebuilds the same bundle from a stale [`LeadAction::Morpho`]
/// (the simulation is the arbiter of whether it still lands).
pub async fn build_opportunity(
    ctx: &StrategyCtx,
    params: &IMorpho::MarketParams,
    id: B256,
    borrower: Address,
    borrow_shares: U256,
    totals: (U256, U256),
    oracle_price: U256,
    target_block: u64,
) -> Option<Opportunity> {
    if borrow_shares.is_zero() {
        return None;
    }
    let repaid_assets = to_assets_up(borrow_shares, totals.0, totals.1);
    if repaid_assets.is_zero() {
        return None;
    }
    let executor = ctx.executor;
    let mut calls = vec![
        Call::new(
            params.loanToken,
            IERC20::approveCall {
                spender: MORPHO_BLUE,
                amount: U256::MAX,
            }
            .abi_encode(),
        ),
        Call::new(
            MORPHO_BLUE,
            IMorpho::liquidateCall {
                marketParams: params.clone(),
                borrower,
                seizedAssets: U256::ZERO,
                repaidShares: borrow_shares,
                data: alloy_primitives::Bytes::new(),
            }
            .abi_encode(),
        ),
    ];
    let mut notes = format!(
        "morpho blue liquidate market {id:?} borrower {borrower:?} repay {repaid_assets} loan {:?} collateral {:?} lltv {}",
        params.loanToken, params.collateralToken, params.lltv
    );

    // Seized estimate: floor(floor(repaid * incentive) * 1e36 / price) — only
    // for the expected-profit line; the chain computes the real number.
    if oracle_price.is_zero() {
        return None;
    }
    let seized_est = (repaid_assets.saturating_mul(liquidation_incentive(params.lltv)) / WAD)
        .saturating_mul(PRICE_SCALE)
        / oracle_price;

    if let Some(pair) = ctx
        .pools
        .pair_for(params.collateralToken, params.loanToken, dex::Venue::UniV2)
        .await
    {
        if let Some(pool) = ctx
            .pools
            .load(pair, dex::Venue::UniV2, ctx.head().number)
            .await
        {
            calls.extend(build_leg(
                &pool,
                params.collateralToken,
                params.loanToken,
                seized_est,
                executor,
            ));
        } else {
            notes.push_str("; swap pool state unavailable");
        }
    } else {
        notes.push_str("; no V2 pool for collateral→loan (recorded unsimulatable)");
    }

    // Expected: the incentive spread on the repaid notional, haircut 40% for
    // the swap leg; simulation produces the real number.
    let expected =
        seized_est.saturating_sub(repaid_assets) * U256::from(6_000u64) / U256::from(10_000u64);

    Some(liquidation_opportunity(
        Strategy::LiquidationMorpho,
        calls,
        vec![params.loanToken],
        vec![repaid_assets],
        params.loanToken,
        expected,
        repaid_assets,
        target_block,
        notes,
    ))
}

#[async_trait]
impl StrategyImpl for MorphoLiquidationStrategy {
    fn kind(&self) -> Strategy {
        Strategy::LiquidationMorpho
    }

    async fn on_block(&self, ctx: &StrategyCtx, head: &BlockHead) -> Vec<Opportunity> {
        self.harvest(ctx, head).await;
        let candidates = self.poll(ctx).await;
        if candidates.is_empty() {
            return Vec::new();
        }
        tracing::info!(
            target: "strategy::liquidation_morpho",
            candidates = candidates.len(),
            markets = self.market_count(),
            "unhealthy Morpho positions found"
        );
        let mut out = Vec::new();
        for (id, params, user, borrow_shares, tba, tbs, price) in candidates {
            if let Some(opp) = build_opportunity(
                ctx,
                &params,
                id,
                user,
                borrow_shares,
                (tba, tbs),
                price,
                ctx.target_block(),
            )
            .await
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

    /// The real mainnet WETH/USDC 91.5% LLTV market parameters' LLTV.
    const LLTV_915: U256 = U256::from_limbs([0xCB2BBA6F17B8000, 0x0, 0x0, 0x0]); // 0.915e18

    #[test]
    fn selectors_match_the_deployed_bytecode() {
        // Verified against the dispatcher of the deployed singleton
        // (non-upgradable, so these are frozen):
        //   liquidate(...) = 0xd8eabcb8
        //   position(bytes32,address) = 0x93c52062
        //   market(bytes32) = 0x5c60e39a
        //   idToMarketParams(bytes32) = 0x2c3c9157
        assert_eq!(IMorpho::liquidateCall::SELECTOR, [0xd8, 0xea, 0xbc, 0xb8]);
        assert_eq!(IMorpho::positionCall::SELECTOR, [0x93, 0xc5, 0x20, 0x62]);
        assert_eq!(IMorpho::marketCall::SELECTOR, [0x5c, 0x60, 0xe3, 0x9a]);
        assert_eq!(
            IMorpho::idToMarketParamsCall::SELECTOR,
            [0x2c, 0x3c, 0x91, 0x57]
        );
    }

    #[test]
    fn incentive_caps_at_fifteen_percent_and_rises_as_lltv_falls() {
        // incentive = min(1.15e18, 1 / (1 - 0.3 * (1 - lltv)))
        // lltv = 1.0  -> 1.0 (no risk, no incentive)
        assert_eq!(liquidation_incentive(WAD), WAD);
        // lltv = 0    -> 1 / 0.7 = 1.4286e18 -> capped at 1.15e18
        assert_eq!(liquidation_incentive(U256::ZERO), MAX_LIQUIDATION_INCENTIVE);
        // lltv = 0.915: x = 0.085; 0.3x = 0.0255; 1/0.9745 = 1.02617e18
        let inc = liquidation_incentive(LLTV_915);
        assert!(inc > U256::from_limbs([0xE3D1590638D0000, 0x0, 0x0, 0x0]));
        assert!(inc < U256::from_limbs([0xE3DCB7684818000, 0x0, 0x0, 0x0]));
        // Monotone: lower lltv -> higher incentive.
        assert!(liquidation_incentive(U256::from_limbs([0x9B6E64A8EC60000, 0x0, 0x0, 0x0])) > inc);
    }

    #[test]
    fn health_mirrors_the_contract() {
        // collateral 10 WETH, price 2000e36, lltv 0.915:
        // maxBorrow = 10e18 * 2000 * 0.915 = 18300e18
        let collateral = U256::from(10u128) * WAD;
        let price = U256::from(2_000u128) * PRICE_SCALE;
        let healthy_debt = U256::from(18_300u128) * WAD;
        assert!(!is_liquidatable(collateral, price, LLTV_915, healthy_debt));
        assert!(is_liquidatable(
            collateral,
            price,
            LLTV_915,
            healthy_debt + U256::ONE
        ));
        // Zero debt is never liquidatable.
        assert!(!is_liquidatable(collateral, price, LLTV_915, U256::ZERO));
    }

    #[test]
    fn to_assets_up_rounds_up_like_the_chain() {
        // shares * (tba + 1) / (tbs + 1e6), ceiling division.
        let shares = U256::from(1_000u64);
        let tba = U256::from(1_000_003u64);
        let tbs = U256::from(1_000_000u64);
        let v = to_assets_up(shares, tba, tbs);
        // exact: 1000 * 1_000_004 / 2_000_000 = 500.002 -> ceil 501
        assert_eq!(v, U256::from(501u64));
        // The virtual offsets (1 wei of assets, 1e6 shares) mean tiny
        // synthetic markets convert at ~nothing — matches the chain, where
        // they exist to pin the empty-market rate.
        assert_eq!(
            to_assets_up(U256::from(2u64), U256::from(1_000u64), U256::from(2u64)),
            U256::ONE
        );
    }

    #[test]
    fn market_and_borrower_caches_are_capped() {
        let leads = LiquidationLeads::new();
        let s = MorphoLiquidationStrategy::new(4, 8, leads);
        let ids: Vec<B256> = (0..10u8).map(|i| B256::from([i; 32])).collect();
        for (n, id) in ids.iter().enumerate() {
            s.touch_market(*id, 100 + n as u64);
        }
        assert_eq!(s.market_count(), 4);
        // Markets touched later survive.
        let ms = s.markets.read();
        assert!(ms.contains_key(&ids[9]));
        assert!(!ms.contains_key(&ids[0]));
    }

    #[test]
    fn actionable_filters_to_pricable_markets() {
        let base = IMorpho::MarketParams {
            loanToken: known::USDC,
            collateralToken: known::WETH,
            oracle: Address::with_last_byte(9),
            irm: Address::with_last_byte(8),
            lltv: LLTV_915,
        };
        assert!(MorphoLiquidationStrategy::actionable(&base));
        // Collateral we cannot swap.
        let exotic = IMorpho::MarketParams {
            collateralToken: Address::with_last_byte(7),
            ..base
        };
        assert!(!MorphoLiquidationStrategy::actionable(&exotic));
        // lltv 0 = collateral-less market, never liquidatable.
        let isolated = IMorpho::MarketParams {
            lltv: U256::ZERO,
            ..base
        };
        assert!(!MorphoLiquidationStrategy::actionable(&isolated));
    }
}
