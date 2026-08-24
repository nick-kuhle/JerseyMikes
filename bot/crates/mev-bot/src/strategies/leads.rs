//! Shared liquidation leads: the near-miss registry that connects the
//! block-cadence liquidation strategies to the oracle-update front-runner.
//!
//! The block-cadence strategies (Aave today; Compound V3, Morpho Blue and
//! Maker in this change) already poll the health of every account on their
//! watchlist once per block. Most accounts are *not* liquidatable yet — they
//! sit just above their threshold. Those accounts are exactly what the
//! oracle-update front-runner wants: a downward price update on the collateral
//! feed is the event most likely to push them under, and being in the same
//! bundle as the oracle transaction is the whole edge (see
//! `oracle_frontrun.rs`).
//!
//! So each strategy publishes its just-above-threshold population into a
//! shared registry ([`LiquidationLeads`]), together with everything needed to
//! rebuild the liquidation call later, and the oracle strategy reads it when a
//! price-update transaction shows up in the mempool. Publishing costs nothing
//! extra — the health numbers were computed anyway — and the registry is
//! refreshed wholesale each block, so stale leads age out in twelve seconds.
//!
//! Ratio units differ per protocol (Aave HF 1e18, Morpho and Maker are
//! collateral/debt at threshold 1e18 in `ratio` form). We normalise everything
//! to **basis points of the liquidation threshold** (`10_000` == exactly at
//! the threshold) so the oracle strategy can filter one scale-agnostic band:
//! `near_miss_min_bps() ..= 10_000`.

use std::collections::HashMap;
use std::sync::Arc;

use alloy_primitives::{Address, B256, U256};
use parking_lot::RwLock;

use crate::types::{now_ms, Call, Opportunity, Strategy};

/// Everything the oracle front-runner needs to rebuild one liquidation
/// back-run, captured at publish time.
///
/// Market totals and share counts go stale the moment someone else touches
/// the position. That is acceptable by design: the rebuilt call is priced by
/// the fork simulation, and a position that moved reverts the liquidation
/// (underflow / healthy-position) — the bundle dies and nothing is broadcast.
#[derive(Clone, Debug)]
pub struct Lead {
    /// Account to liquidate (user / borrower / urn).
    pub account: Address,
    /// Collateral token whose price feed is the trigger (WETH, WBTC, ...).
    pub collateral: Address,
    /// Debt token repaid by the liquidation (profit denomination).
    pub debt_asset: Address,
    /// Health in bps of the liquidation threshold: 10_000 == at threshold,
    /// 10_500 == 5% above it. Values < 10_000 are already liquidatable and
    /// belong to the block-cadence path, not the oracle path.
    pub ratio_bps: u64,
    /// Protocol-native debt size, for note trails only.
    pub debt_wei: U256,
    /// How to rebuild the calls.
    pub action: LeadAction,
}

/// Protocol-specific rebuild instructions. Each variant maps 1:1 onto the
/// builder function of the strategy that published the lead, so the oracle
/// strategy never re-implements protocol logic — it calls the same builder
/// the block path uses.
///
/// The Morpho variant is large (it carries the market snapshot needed to
/// rebuild sizing); the registry holds at most a few dozen leads, so the
/// size difference is not worth boxing.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug)]
pub enum LeadAction {
    AaveV3 {
        user: Address,
    },
    Morpho {
        /// Market id, as emitted in every Blue event.
        market_id: B256,
        loan_token: Address,
        collateral_token: Address,
        oracle: Address,
        irm: Address,
        lltv: U256,
        borrower: Address,
        /// Borrow shares at publish time; the liquidation repays all of them.
        borrow_shares: U256,
        /// Market totals at publish time (assets + shares of the borrow side).
        total_borrow_assets: U256,
        total_borrow_shares: U256,
    },
    Maker {
        /// 32-byte ilk key, e.g. `"ETH-A"` right-padded with zeros.
        ilk: B256,
        /// Urn address (usually the owner's proxy).
        urn: Address,
    },
}

impl LeadAction {
    /// Registry key. Each protocol owns one slot so a strategy publishing an
    /// empty batch clears *its own* leads without touching anyone else's.
    pub fn protocol(&self) -> &'static str {
        match self {
            LeadAction::AaveV3 { .. } => "aave-v3",
            LeadAction::Morpho { .. } => "morpho-blue",
            LeadAction::Maker { .. } => "maker",
        }
    }
}

/// Lower bound (inclusive) of the near-miss band, in bps of the threshold.
/// 10_000 **is** the threshold; the band is the 5% just above it. (Publishers
/// only ever emit non-liquidatable positions here — below-threshold values
/// belong to the block-cadence path, not the oracle path.)
pub const NEAR_MISS_MIN_BPS: u64 = 10_000;
/// Upper bound (exclusive) of the near-miss band. A -5% collateral move is a
/// large but real event; wider bands multiply leads that almost never flip.
pub const NEAR_MISS_MAX_BPS: u64 = 10_500;

/// Registry of near-miss positions, one slot per protocol, replaced every
/// block by that protocol's strategy. Cheap to clone (one `Arc`).
#[derive(Clone, Default)]
pub struct LiquidationLeads {
    inner: Arc<RwLock<HashMap<&'static str, Vec<Lead>>>>,
}

impl LiquidationLeads {
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace one protocol's slot. Called once per block by each
    /// health-polling strategy; publishing an empty batch clears that
    /// protocol's stale leads and leaves the other slots alone.
    pub fn publish(&self, protocol: &'static str, batch: Vec<Lead>) {
        self.inner.write().insert(protocol, batch);
    }

    /// Leads whose collateral is `asset` and whose ratio sits inside the
    /// near-miss band. Called only when an oracle update for `asset` is seen,
    /// so the read path costs nothing on quiet blocks.
    pub fn near_misses_for(&self, asset: Address, max: usize) -> Vec<Lead> {
        let mut out: Vec<Lead> = self
            .inner
            .read()
            .values()
            .flatten()
            .filter(|l| {
                l.collateral == asset
                    && l.ratio_bps >= NEAR_MISS_MIN_BPS
                    && l.ratio_bps < NEAR_MISS_MAX_BPS
            })
            .cloned()
            .collect();
        // Most-at-risk first: the lowest ratio is the closest to flipping.
        out.sort_unstable_by_key(|l| l.ratio_bps);
        out.truncate(max);
        out
    }

    pub fn len(&self) -> usize {
        self.inner.read().values().map(Vec::len).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Basis-point ratio of "collateral value at threshold" to debt, floored.
/// `collateral_scaled * lltv / 1e18` vs `debt`: ratio = 10_000 means exactly
/// at the liquidation threshold.
pub fn ratio_bps(collateral_scaled: U256, lltv_wad: Option<U256>, debt: U256) -> u64 {
    if debt.is_zero() {
        return u64::MAX;
    }
    let effective = match lltv_wad {
        Some(lltv) => collateral_scaled
            .checked_mul(lltv)
            .map(|v| v / U256::from(1_000_000_000_000_000_000u128)),
        None => Some(collateral_scaled),
    };
    let Some(effective) = effective else {
        return u64::MAX;
    };
    // bps = effective * 10_000 / debt, computed in U256 to avoid overflow.
    let bps = effective.saturating_mul(U256::from(10_000u64)) / debt;
    if bps > U256::from(u64::MAX) {
        u64::MAX
    } else {
        bps.to::<u64>()
    }
}

/// Common tail for every liquidation-flavoured opportunity so the protocol
/// modules only differ in their call list.
#[allow(clippy::too_many_arguments)]
pub fn liquidation_opportunity(
    strategy: Strategy,
    calls: Vec<Call>,
    flash_tokens: Vec<Address>,
    flash_amounts: Vec<U256>,
    profit_token: Address,
    expected_profit_wei: U256,
    notional_wei: U256,
    target_block: u64,
    notes: String,
) -> Opportunity {
    Opportunity {
        id: uuid::Uuid::new_v4().to_string(),
        strategy,
        victim_hashes: Vec::new(),
        front_calls: calls,
        back_calls: Vec::new(),
        flash_tokens,
        flash_amounts,
        profit_token,
        expected_profit_wei,
        notional_wei,
        target_block,
        created_at_ms: now_ms(),
        notes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::address;

    const WETH: Address = crate::config::known::WETH;

    #[test]
    fn ratio_bps_maps_threshold_to_ten_thousand() {
        // Exactly at the liquidation threshold (collateral * lltv == debt).
        let debt = U256::from(1_000e18 as u128);
        let lltv = U256::from(0.8e18 as u128);
        let collateral = U256::from(1_250e18 as u128); // * 0.8 == 1000e18
        assert_eq!(ratio_bps(collateral, Some(lltv), debt), 10_000);
    }

    #[test]
    fn ratio_bps_five_percent_above_is_ten_thousand_five_hundred() {
        let debt = U256::from(1_000e18 as u128);
        let lltv = U256::from(1_000_000_000_000_000_000u128);
        let collateral = U256::from(1_050e18 as u128);
        assert_eq!(ratio_bps(collateral, Some(lltv), debt), 10_500);
    }

    #[test]
    fn ratio_bps_zero_debt_is_infinite() {
        assert_eq!(ratio_bps(U256::ZERO, None, U256::ZERO), u64::MAX);
    }

    #[test]
    fn registry_filters_by_collateral_and_band() {
        let leads = LiquidationLeads::new();
        let eth_lead = Lead {
            account: address!("0000000000000000000000000000000000000001"),
            collateral: WETH,
            debt_asset: crate::config::known::USDC,
            ratio_bps: 10_200,
            debt_wei: U256::from(1u64),
            action: LeadAction::AaveV3 {
                user: address!("0000000000000000000000000000000000000001"),
            },
        };
        let btc_far = Lead {
            account: address!("0000000000000000000000000000000000000002"),
            collateral: crate::config::known::WBTC,
            debt_asset: crate::config::known::USDC,
            ratio_bps: 9_900,
            debt_wei: U256::from(1u64),
            action: LeadAction::AaveV3 {
                user: address!("0000000000000000000000000000000000000002"),
            },
        };
        // Already liquidatable: not a near-miss, the block path owns it.
        let eth_below = Lead {
            account: address!("0000000000000000000000000000000000000003"),
            collateral: WETH,
            debt_asset: crate::config::known::USDC,
            ratio_bps: 9_800,
            debt_wei: U256::from(1u64),
            action: LeadAction::AaveV3 {
                user: address!("0000000000000000000000000000000000000003"),
            },
        };
        // Comfortably above the band: an ETH feed tick will not flip it.
        let eth_comfortable = Lead {
            account: address!("0000000000000000000000000000000000000004"),
            collateral: WETH,
            debt_asset: crate::config::known::USDC,
            ratio_bps: 10_600,
            debt_wei: U256::from(1u64),
            action: LeadAction::AaveV3 {
                user: address!("0000000000000000000000000000000000000004"),
            },
        };
        leads.publish(
            "aave-v3",
            vec![eth_lead.clone(), btc_far, eth_below, eth_comfortable],
        );
        assert_eq!(leads.len(), 4);
        let got = leads.near_misses_for(WETH, 10);
        assert_eq!(got.len(), 1, "only the in-band WETH lead survives");
        assert_eq!(got[0].account, eth_lead.account);
        // Cap is respected.
        leads.publish("aave-v3", vec![eth_lead.clone(); 8]);
        assert_eq!(leads.near_misses_for(WETH, 3).len(), 3);
    }

    #[test]
    fn publishing_an_empty_batch_clears() {
        let leads = LiquidationLeads::new();
        leads.publish(
            "aave-v3",
            vec![Lead {
                account: Address::ZERO,
                collateral: WETH,
                debt_asset: Address::ZERO,
                ratio_bps: 9_600,
                debt_wei: U256::ONE,
                action: LeadAction::AaveV3 {
                    user: Address::ZERO,
                },
            }],
        );
        assert!(!leads.is_empty());
        // A second protocol publishing does not clobber the first.
        leads.publish("maker", Vec::new());
        assert!(!leads.is_empty());
        leads.publish("aave-v3", Vec::new());
        assert!(leads.is_empty());
    }

    /// Every liquidation-flavoured opportunity settles in the protocol's debt
    /// or loan asset (Aave: `debt_asset`, Compound V3: USDC, Morpho Blue:
    /// `loanToken`, Maker: DAI) — never in the native asset. That is what
    /// routes these strategies through `valuation::value_in_native` in the
    /// fork simulator instead of the native-accounting shortcut, so the
    /// invariant is load-bearing for their profit numbers rather than
    /// cosmetic: a regression to `Address::ZERO` here would silently make the
    /// simulator treat an ERC-20 balance delta as if it were wei.
    ///
    /// `liquidation_opportunity` takes `profit_token` positionally between two
    /// other `Address`-shaped arguments, which is exactly the shape of call
    /// that survives a careless edit, so pin it.
    #[test]
    fn liquidation_opportunities_settle_in_a_non_native_token() {
        let usdc = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");

        for (strategy, token) in [
            (Strategy::Liquidation, usdc),
            (Strategy::LiquidationCompound, usdc),
            (Strategy::LiquidationMorpho, WETH),
            (Strategy::LiquidationMaker, crate::config::known::DAI),
        ] {
            let opp = liquidation_opportunity(
                strategy,
                Vec::new(),
                vec![token],
                vec![U256::ONE],
                token,
                U256::ONE,
                U256::ONE,
                1,
                String::new(),
            );

            assert_eq!(
                opp.profit_token, token,
                "{strategy:?} must carry its settlement token through to the simulator",
            );
            assert_ne!(
                opp.profit_token,
                Address::ZERO,
                "{strategy:?} must never fall back to the native sentinel",
            );
            // `live_candidate()` is what admits these rows to the broadcast
            // path; pairing the two assertions keeps the promotion and the
            // valuation requirement from drifting apart.
            assert!(
                strategy.live_candidate(),
                "{strategy:?} is expected to be a live candidate",
            );
        }
    }
}
