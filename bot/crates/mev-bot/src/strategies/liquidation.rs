//! Aave V3 liquidations.
//!
//! Borrowers are discovered from `Borrow`/`Supply` logs, then their health
//! factor is polled once per block with batched `getUserAccountData` calls.
//! When HF < 1 the bot builds a flash-loan-funded `liquidationCall`:
//!
//!   flash borrow debt asset → repay up to the close factor → receive collateral
//!   at a bonus → swap collateral back → repay the flash loan → keep the spread.
//!
//! The swap leg uses the deepest V2 pool we know about for the collateral; when
//! there isn't one, the opportunity is still recorded (with the collateral as
//! the profit token) so the dashboard shows what was missed.

use std::collections::HashSet;

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
    watchlist: RwLock<HashSet<Address>>,
    last_log_block: RwLock<u64>,
    /// Shared near-miss registry (see `leads.rs`).
    leads: LiquidationLeads,
}

impl LiquidationStrategy {
    pub fn new(leads: LiquidationLeads) -> Self {
        Self {
            watchlist: RwLock::new(HashSet::new()),
            last_log_block: RwLock::new(0),
            leads,
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
                                    set.insert(Address::from_slice(&bytes[12..32]));
                                }
                            }
                        }
                    }
                }
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
        let users: Vec<Address> = self.watchlist.read().iter().copied().collect();
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
                if let Some(opp) = build_opportunity(ctx, user, total_debt_base, health).await {
                    out.push(opp);
                }
            } else {
                // Near-miss (HF in [1, 1.05)): publish for the oracle
                // front-runner. Ratio in bps of the threshold: HF 1.02 ->
                // 10_200 bps.
                let bps = ratio_bps(health, Some(hf_one), hf_one);
                near_misses.push(Lead {
                    account: user,
                    collateral: ctx.cfg.chain.weth,
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

/// Build the flash-borrow → liquidationCall → swap bundle for one Aave
/// borrower. Public so the oracle front-runner can rebuild the exact same
/// bundle as a back-run of a feed update.
pub async fn build_opportunity(
    ctx: &StrategyCtx,
    user: alloy_primitives::Address,
    total_debt_base: U256,
    health: U256,
) -> Option<Opportunity> {
    let weth = ctx.cfg.chain.weth;
    // Aave's close factor: at most 50% of the debt in one call (100% when
    // HF < 0.95). Base units are USD with 8 decimals; we size in the debt
    // asset by assuming the stable leg, which the simulation will correct.
    let close_factor_bps = if health < U256::from(950_000_000_000_000_000u128) {
        10_000u64
    } else {
        5_000u64
    };
    let debt_to_cover = total_debt_base * U256::from(close_factor_bps) / U256::from(10_000u64);

    // Default shape: repay USDC debt, seize WETH collateral.
    let debt_asset = known::USDC;
    let collateral = weth;
    // Base units are 1e8; USDC is 1e6.
    let debt_amount = debt_to_cover / U256::from(100u64);
    if debt_amount.is_zero() {
        return None;
    }

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
                user,
                debtToCover: debt_amount,
                receiveAToken: false,
            }
            .abi_encode(),
        ),
    ];

    // Swap the seized collateral back into the debt asset to repay the flash loan.
    if let Some(pair) = ctx
        .pools
        .pair_for(collateral, debt_asset, Venue::UniV2)
        .await
    {
        if let Some(pool) = ctx.pools.load(pair, Venue::UniV2, ctx.head().number).await {
            // 5% liquidation bonus is the common configuration.
            let seized = debt_amount * U256::from(105u64) / U256::from(100u64);
            let collateral_amount = pool
                .reserves_for(debt_asset)
                .map(|(r_in, r_out)| {
                    if r_in.is_zero() {
                        U256::ZERO
                    } else {
                        seized * r_out / r_in
                    }
                })
                .unwrap_or(U256::ZERO);
            if !collateral_amount.is_zero() {
                calls.extend(build_leg(
                    &pool,
                    collateral,
                    debt_asset,
                    collateral_amount,
                    ctx.executor,
                ));
            }
        }
    }

    Some(crate::strategies::leads::liquidation_opportunity(
        Strategy::Liquidation,
        calls,
        vec![debt_asset],
        vec![debt_amount],
        debt_asset,
        // 5% bonus minus swap slippage; the simulator produces the real number.
        debt_amount * U256::from(5u64) / U256::from(100u64),
        debt_amount,
        ctx.target_block(),
        format!(
            "aave v3 liquidation user {user:?} hf {health} debt_base {total_debt_base} cover {debt_amount}"
        ),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let s = LiquidationStrategy::new(LiquidationLeads::new());
        assert_eq!(s.watchlist_size(), 0);
    }
}
