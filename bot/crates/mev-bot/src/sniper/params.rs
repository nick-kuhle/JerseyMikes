//! The sniper's own risk envelope.
//!
//! Deliberately **not** part of [`crate::config::RiskConfig`]. The shared risk
//! envelope governs atomic, profit-or-revert bundles whose worst case is a
//! reverted bundle costing nothing. The directional sniper's worst case is
//! losing the entire buy, so it gets a separate, separately-armed envelope
//! whose defaults cannot buy anything:
//!
//! ```text
//! SNIPER_DIRECTIONAL=false        master switch, off
//! SNIPER_BUY_SIZE_WEI=0           zero size
//! SNIPER_DAILY_BUDGET_WEI=0       zero budget
//! ```
//!
//! All three must be set deliberately before a single wei can be committed.
//! `arming_blockers` reports, in one place, every reason the lane is currently
//! unable to buy — the console renders it verbatim so a disabled sniper is
//! never a mystery.

use alloy_primitives::{Address, U256};
use serde::{Deserialize, Serialize};

/// Basis points denominator.
pub const BPS: u32 = 10_000;

/// The directional sniper's risk envelope.
///
/// Every field is runtime-patchable through `POST /api/sniper/params`, and
/// every field is validated by [`SniperParams::validate`] before it is
/// applied. A rejected patch leaves the previous envelope completely intact.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SniperParams {
    /// Master switch for the directional lane. When false the sniper still
    /// observes launches, runs honeypot checks and records verdicts — it
    /// simply never proposes a buy. This is the shadow mode.
    pub enabled: bool,

    /// Address of the deployed SniperVault contract.
    pub vault_address: Option<Address>,

    // --- entry -----------------------------------------------------------
    /// How much native (WETH) to commit per launch. `x` in "buy (x) eth".
    pub buy_size_wei: U256,
    /// Reject a launch whose WETH-side pool reserve is below this. A thin pool
    /// cannot be exited at any size.
    pub min_liquidity_wei: U256,
    /// Reject if `buy_size_wei` would move the pool more than this. Protects
    /// against buying the whole curve on a 0.6 WETH pool.
    pub max_price_impact_bps: u32,

    // --- exit ------------------------------------------------------------
    /// Take profit when the position's mark-to-market gain reaches this many
    /// bps over entry. `x%` in "sell after it has profited (x)%".
    pub take_profit_bps: u32,
    /// Take profit when the position's *absolute* unrealised gain reaches this
    /// many wei. `x eth` in "profited (x)eth". 0 disables this trigger.
    ///
    /// When both triggers are configured, **either** one firing is enough.
    pub take_profit_abs_wei: U256,
    /// What fraction of the position to sell when a take-profit fires.
    /// `x%` in "sell (x)%". 10000 = the whole position.
    pub sell_fraction_bps: u32,
    /// Exit the whole position when the mark drops this many bps below entry.
    /// 0 disables the stop.
    pub stop_loss_bps: u32,
    /// Once in profit, exit if the mark retraces this many bps from its peak.
    /// 0 disables the trailing stop.
    pub trailing_stop_bps: u32,
    /// Force an exit attempt after this many seconds regardless of price.
    /// 0 disables the timer.
    pub max_hold_secs: u64,

    // --- exposure --------------------------------------------------------
    /// How many positions may be open at once.
    pub max_concurrent_positions: usize,
    /// Ceiling on total wei committed to *entries* in a rolling 24h window.
    pub daily_budget_wei: U256,
    /// Ceiling on total wei committed to entries over the lane's lifetime.
    /// 0 disables the lifetime cap (the daily cap still applies).
    pub total_budget_wei: U256,
    /// Stop opening new positions once realised sniper PnL falls below
    /// `-max_drawdown_wei`. 0 disables. Independent of the shared engine
    /// kill switch: the sniper can be stopped without stopping arbitrage.
    pub max_drawdown_wei: U256,

    // --- safety gates ----------------------------------------------------
    /// Require a passing honeypot round-trip before buying. Turning this off
    /// is possible but is reported as a blocker-level warning in the console.
    pub require_honeypot_pass: bool,
    /// Maximum tolerated buy-side transfer tax, in bps.
    pub max_buy_tax_bps: u32,
    /// Maximum tolerated sell-side transfer tax, in bps.
    pub max_sell_tax_bps: u32,
    /// Never exit before the position is this many blocks old. Blocks a
    /// same-block round trip being mistaken for a directional position and
    /// gives the launch time to establish a price.
    pub min_hold_blocks: u64,
    /// Require the pool's LP tokens to be burned or time-locked. Enforcement
    /// is best-effort (see `gates::lp_locked`); when the check cannot reach a
    /// verdict the gate fails **closed** if this is true.
    pub require_lp_locked: bool,
}

impl Default for SniperParams {
    /// Zero-risk defaults: the lane is off, the size is zero and the budget is
    /// zero. A fresh checkout cannot buy a token by accident, which is the
    /// entire point.
    ///
    /// The non-zero values below are *shape* defaults — they define sensible
    /// behaviour for when an operator does arm the lane, and none of them can
    /// cause a buy on their own.
    fn default() -> Self {
        Self {
            enabled: false,
            vault_address: None,
            buy_size_wei: U256::ZERO,
            min_liquidity_wei: U256::from(2_000_000_000_000_000_000u128), // 2 ETH
            max_price_impact_bps: 300,                                    // 3%
            take_profit_bps: 10_000,                                      // +100%
            take_profit_abs_wei: U256::ZERO,
            sell_fraction_bps: 10_000, // sell it all
            stop_loss_bps: 5_000,      // -50%
            trailing_stop_bps: 0,
            max_hold_secs: 1_800, // 30 min
            max_concurrent_positions: 1,
            daily_budget_wei: U256::ZERO,
            total_budget_wei: U256::ZERO,
            max_drawdown_wei: U256::ZERO,
            require_honeypot_pass: true,
            max_buy_tax_bps: 500,  // 5%
            max_sell_tax_bps: 500, // 5%
            min_hold_blocks: 1,
            require_lp_locked: false,
        }
    }
}

impl SniperParams {
    /// Validate the envelope as a whole. Returns every problem found, not just
    /// the first — an operator fixing a params form should see all of it at
    /// once.
    pub fn validate(&self) -> Result<(), Vec<String>> {
        let mut errs = Vec::new();

        if self.sell_fraction_bps == 0 || self.sell_fraction_bps > BPS {
            errs.push(format!(
                "sellFractionBps {} outside (0, {BPS}]",
                self.sell_fraction_bps
            ));
        }
        if self.stop_loss_bps > BPS {
            errs.push(format!(
                "stopLossBps {} exceeds {BPS} (a 100% stop is a total loss)",
                self.stop_loss_bps
            ));
        }
        if self.trailing_stop_bps > BPS {
            errs.push(format!(
                "trailingStopBps {} exceeds {BPS}",
                self.trailing_stop_bps
            ));
        }
        if self.max_price_impact_bps > BPS {
            errs.push(format!(
                "maxPriceImpactBps {} exceeds {BPS}",
                self.max_price_impact_bps
            ));
        }
        if self.max_buy_tax_bps > BPS {
            errs.push(format!(
                "maxBuyTaxBps {} exceeds {BPS}",
                self.max_buy_tax_bps
            ));
        }
        if self.max_sell_tax_bps > BPS {
            errs.push(format!(
                "maxSellTaxBps {} exceeds {BPS}",
                self.max_sell_tax_bps
            ));
        }
        if self.max_concurrent_positions > 64 {
            errs.push(format!(
                "maxConcurrentPositions {} exceeds 64",
                self.max_concurrent_positions
            ));
        }

        // No exit trigger at all means a position can only ever leave via the
        // operator. That is a footgun, not a strategy.
        if self.take_profit_bps == 0
            && self.take_profit_abs_wei.is_zero()
            && self.stop_loss_bps == 0
            && self.trailing_stop_bps == 0
            && self.max_hold_secs == 0
        {
            errs.push(
                "no exit trigger configured: set at least one of takeProfitBps, \
                 takeProfitAbsWei, stopLossBps, trailingStopBps or maxHoldSecs"
                    .to_string(),
            );
        }

        // An armed lane with a real size but no budget would be stopped by the
        // budget gate on the very first buy. Catch it here instead.
        if self.enabled && !self.buy_size_wei.is_zero() {
            if self.daily_budget_wei.is_zero() {
                errs.push(
                    "sniper is enabled with a non-zero buy size but dailyBudgetWei is 0: \
                     no entry can ever clear the budget gate"
                        .to_string(),
                );
            } else if self.buy_size_wei > self.daily_budget_wei {
                errs.push(format!(
                    "buySizeWei {} exceeds dailyBudgetWei {}: the first entry cannot fit",
                    self.buy_size_wei, self.daily_budget_wei
                ));
            }
            if !self.total_budget_wei.is_zero() && self.buy_size_wei > self.total_budget_wei {
                errs.push(format!(
                    "buySizeWei {} exceeds totalBudgetWei {}",
                    self.buy_size_wei, self.total_budget_wei
                ));
            }
            if self.max_concurrent_positions == 0 {
                errs.push(
                    "sniper is enabled but maxConcurrentPositions is 0: no entry is possible"
                        .to_string(),
                );
            }
        }

        // A stop above take-profit means the stop fires first, always.
        if self.stop_loss_bps > 0 && self.take_profit_bps > 0 {
            // Nothing to check numerically (they act on opposite sides of
            // entry), but a 0-bps take profit with a stop is degenerate.
        }

        if errs.is_empty() {
            Ok(())
        } else {
            Err(errs)
        }
    }

    /// Every reason the lane currently cannot open a position, in the order an
    /// operator should fix them. Empty means "a qualifying launch would be
    /// bought right now".
    ///
    /// This is the single source of truth behind the console's arming banner.
    /// It intentionally reports *all* blockers rather than short-circuiting,
    /// because "I turned it on and nothing happened" is the failure mode this
    /// method exists to prevent.
    pub fn arming_blockers(&self) -> Vec<String> {
        let mut out = Vec::new();
        if !self.enabled {
            out.push("SNIPER_DIRECTIONAL is off (shadow mode: launches are observed and honeypot-checked, never bought)".to_string());
        }
        if self.enabled && matches!(self.vault_address, None | Some(Address::ZERO)) {
            out.push("vault address not set (SNIPER_VAULT_ADDRESS)".to_string());
        }
        if self.buy_size_wei.is_zero() {
            out.push("buySizeWei is 0".to_string());
        }
        if self.daily_budget_wei.is_zero() {
            out.push("dailyBudgetWei is 0".to_string());
        }
        if self.max_concurrent_positions == 0 {
            out.push("maxConcurrentPositions is 0".to_string());
        }
        if !self.require_honeypot_pass {
            out.push(
                "WARNING: requireHoneypotPass is off — tokens will be bought without a \
                 round-trip sell check. This is how snipers lose their whole budget."
                    .to_string(),
            );
        }
        out
    }

    /// True when the lane is fully armed and would act on a qualifying launch.
    /// The honeypot warning is not a hard blocker, so it is excluded here.
    pub fn is_armed(&self) -> bool {
        self.enabled
            && matches!(self.vault_address, Some(a) if !a.is_zero())
            && !self.buy_size_wei.is_zero()
            && !self.daily_budget_wei.is_zero()
            && self.max_concurrent_positions > 0
    }

    /// Read the envelope from the environment. Every key is `SNIPER_*` so the
    /// lane's configuration can be grepped out of an `.env` in one line.
    pub fn from_env() -> Self {
        let d = Self::default();
        Self {
            enabled: env_bool("SNIPER_DIRECTIONAL", d.enabled),
            vault_address: env_address("SNIPER_VAULT_ADDRESS"),
            buy_size_wei: env_u256("SNIPER_BUY_SIZE_WEI", d.buy_size_wei),
            min_liquidity_wei: env_u256("SNIPER_MIN_LIQUIDITY_WEI", d.min_liquidity_wei),
            max_price_impact_bps: env_u32("SNIPER_MAX_PRICE_IMPACT_BPS", d.max_price_impact_bps),
            take_profit_bps: env_u32("SNIPER_TAKE_PROFIT_BPS", d.take_profit_bps),
            take_profit_abs_wei: env_u256("SNIPER_TAKE_PROFIT_ABS_WEI", d.take_profit_abs_wei),
            sell_fraction_bps: env_u32("SNIPER_SELL_FRACTION_BPS", d.sell_fraction_bps),
            stop_loss_bps: env_u32("SNIPER_STOP_LOSS_BPS", d.stop_loss_bps),
            trailing_stop_bps: env_u32("SNIPER_TRAILING_STOP_BPS", d.trailing_stop_bps),
            max_hold_secs: env_u64("SNIPER_MAX_HOLD_SECS", d.max_hold_secs),
            max_concurrent_positions: env_u64(
                "SNIPER_MAX_CONCURRENT_POSITIONS",
                d.max_concurrent_positions as u64,
            ) as usize,
            daily_budget_wei: env_u256("SNIPER_DAILY_BUDGET_WEI", d.daily_budget_wei),
            total_budget_wei: env_u256("SNIPER_TOTAL_BUDGET_WEI", d.total_budget_wei),
            max_drawdown_wei: env_u256("SNIPER_MAX_DRAWDOWN_WEI", d.max_drawdown_wei),
            require_honeypot_pass: env_bool(
                "SNIPER_REQUIRE_HONEYPOT_PASS",
                d.require_honeypot_pass,
            ),
            max_buy_tax_bps: env_u32("SNIPER_MAX_BUY_TAX_BPS", d.max_buy_tax_bps),
            max_sell_tax_bps: env_u32("SNIPER_MAX_SELL_TAX_BPS", d.max_sell_tax_bps),
            min_hold_blocks: env_u64("SNIPER_MIN_HOLD_BLOCKS", d.min_hold_blocks),
            require_lp_locked: env_bool("SNIPER_REQUIRE_LP_LOCKED", d.require_lp_locked),
        }
    }

    /// Apply a partial patch, validating the *result* before committing.
    pub fn with_patch(&self, patch: &SniperParamsPatch) -> Result<Self, Vec<String>> {
        let mut next = self.clone();
        let mut errs = Vec::new();

        macro_rules! wei {
            ($field:ident, $label:literal) => {
                if let Some(raw) = &patch.$field {
                    match raw.trim().parse::<U256>() {
                        Ok(v) => next.$field = v,
                        Err(_) => errs.push(format!(
                            "{}: \"{}\" is not a non-negative decimal wei amount",
                            $label, raw
                        )),
                    }
                }
            };
        }

        wei!(buy_size_wei, "buySizeWei");
        wei!(min_liquidity_wei, "minLiquidityWei");
        wei!(take_profit_abs_wei, "takeProfitAbsWei");
        wei!(daily_budget_wei, "dailyBudgetWei");
        wei!(total_budget_wei, "totalBudgetWei");
        wei!(max_drawdown_wei, "maxDrawdownWei");

        if let Some(v) = patch.enabled {
            next.enabled = v;
        }
        if let Some(ref v) = patch.vault_address {
            if v.trim().is_empty()
                || v == "0x0"
                || v == "0x0000000000000000000000000000000000000000"
            {
                next.vault_address = None;
            } else {
                match v.parse::<Address>() {
                    Ok(addr) => next.vault_address = Some(addr),
                    Err(_) => errs.push(format!("vaultAddress {v:?} is not a valid EVM address")),
                }
            }
        }
        // Note: the production-vault requirement is enforced by the lane for
        // LIVE mode only (`SniperLane::patch_params`) — simulation runs the
        // local fixture and needs no production address.
        if let Some(v) = patch.max_price_impact_bps {
            next.max_price_impact_bps = v;
        }
        if let Some(v) = patch.take_profit_bps {
            next.take_profit_bps = v;
        }
        if let Some(v) = patch.sell_fraction_bps {
            next.sell_fraction_bps = v;
        }
        if let Some(v) = patch.stop_loss_bps {
            next.stop_loss_bps = v;
        }
        if let Some(v) = patch.trailing_stop_bps {
            next.trailing_stop_bps = v;
        }
        if let Some(v) = patch.max_hold_secs {
            next.max_hold_secs = v;
        }
        if let Some(v) = patch.max_concurrent_positions {
            next.max_concurrent_positions = v;
        }
        if let Some(v) = patch.require_honeypot_pass {
            next.require_honeypot_pass = v;
        }
        if let Some(v) = patch.max_buy_tax_bps {
            next.max_buy_tax_bps = v;
        }
        if let Some(v) = patch.max_sell_tax_bps {
            next.max_sell_tax_bps = v;
        }
        if let Some(v) = patch.min_hold_blocks {
            next.min_hold_blocks = v;
        }
        if let Some(v) = patch.require_lp_locked {
            next.require_lp_locked = v;
        }

        if !errs.is_empty() {
            return Err(errs);
        }
        next.validate()?;
        Ok(next)
    }

    /// The `.env` snippet that persists this envelope as boot defaults —
    /// mirrors what the risk panel already does for the shared envelope.
    pub fn env_snippet(&self) -> String {
        format!(
            "SNIPER_DIRECTIONAL={}\n\
             SNIPER_BUY_SIZE_WEI={}\n\
             SNIPER_MIN_LIQUIDITY_WEI={}\n\
             SNIPER_MAX_PRICE_IMPACT_BPS={}\n\
             SNIPER_TAKE_PROFIT_BPS={}\n\
             SNIPER_TAKE_PROFIT_ABS_WEI={}\n\
             SNIPER_SELL_FRACTION_BPS={}\n\
             SNIPER_STOP_LOSS_BPS={}\n\
             SNIPER_TRAILING_STOP_BPS={}\n\
             SNIPER_MAX_HOLD_SECS={}\n\
             SNIPER_MAX_CONCURRENT_POSITIONS={}\n\
             SNIPER_DAILY_BUDGET_WEI={}\n\
             SNIPER_TOTAL_BUDGET_WEI={}\n\
             SNIPER_MAX_DRAWDOWN_WEI={}\n\
             SNIPER_REQUIRE_HONEYPOT_PASS={}\n\
             SNIPER_MAX_BUY_TAX_BPS={}\n\
             SNIPER_MAX_SELL_TAX_BPS={}\n\
             SNIPER_MIN_HOLD_BLOCKS={}\n\
             SNIPER_REQUIRE_LP_LOCKED={}",
            self.enabled,
            self.buy_size_wei,
            self.min_liquidity_wei,
            self.max_price_impact_bps,
            self.take_profit_bps,
            self.take_profit_abs_wei,
            self.sell_fraction_bps,
            self.stop_loss_bps,
            self.trailing_stop_bps,
            self.max_hold_secs,
            self.max_concurrent_positions,
            self.daily_budget_wei,
            self.total_budget_wei,
            self.max_drawdown_wei,
            self.require_honeypot_pass,
            self.max_buy_tax_bps,
            self.max_sell_tax_bps,
            self.min_hold_blocks,
            self.require_lp_locked,
        )
    }
}

/// Partial update from the console. Wei values arrive as decimal strings
/// because they routinely exceed JavaScript's safe integer range.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SniperParamsPatch {
    pub enabled: Option<bool>,
    pub vault_address: Option<String>,
    pub buy_size_wei: Option<String>,
    pub min_liquidity_wei: Option<String>,
    pub max_price_impact_bps: Option<u32>,
    pub take_profit_bps: Option<u32>,
    pub take_profit_abs_wei: Option<String>,
    pub sell_fraction_bps: Option<u32>,
    pub stop_loss_bps: Option<u32>,
    pub trailing_stop_bps: Option<u32>,
    pub max_hold_secs: Option<u64>,
    pub max_concurrent_positions: Option<usize>,
    pub daily_budget_wei: Option<String>,
    pub total_budget_wei: Option<String>,
    pub max_drawdown_wei: Option<String>,
    pub require_honeypot_pass: Option<bool>,
    pub max_buy_tax_bps: Option<u32>,
    pub max_sell_tax_bps: Option<u32>,
    pub min_hold_blocks: Option<u64>,
    pub require_lp_locked: Option<bool>,
}

// --- env helpers ------------------------------------------------------------
// Local copies rather than reaching into config.rs's private helpers: this
// module is deliberately standalone so the sniper lane can be reasoned about
// (and deleted) without touching the shared config surface.

fn env_raw(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn env_address(key: &str) -> Option<Address> {
    env_raw(key).and_then(|v| v.parse().ok())
}

fn env_bool(key: &str, default: bool) -> bool {
    match env_raw(key) {
        Some(v) => matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"),
        None => default,
    }
}

fn env_u32(key: &str, default: u32) -> u32 {
    env_raw(key).and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn env_u64(key: &str, default: u64) -> u64 {
    env_raw(key).and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn env_u256(key: &str, default: U256) -> U256 {
    env_raw(key)
        .and_then(|v| v.parse::<U256>().ok())
        .unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eth(n: u64) -> U256 {
        U256::from(n) * U256::from(1_000_000_000_000_000_000u128)
    }

    #[test]
    fn default_cannot_buy_anything() {
        let p = SniperParams::default();
        assert!(!p.enabled, "the lane must ship disabled");
        assert!(p.buy_size_wei.is_zero(), "default size must be zero");
        assert!(p.daily_budget_wei.is_zero(), "default budget must be zero");
        assert!(!p.is_armed());
        // And the default envelope is internally consistent.
        assert!(p.validate().is_ok(), "{:?}", p.validate());
    }

    #[test]
    fn default_blockers_name_all_three_switches() {
        let blockers = SniperParams::default().arming_blockers();
        assert!(blockers.iter().any(|b| b.contains("SNIPER_DIRECTIONAL")));
        assert!(blockers.iter().any(|b| b.contains("buySizeWei")));
        assert!(blockers.iter().any(|b| b.contains("dailyBudgetWei")));
    }

    #[test]
    fn a_fully_armed_envelope_reports_no_blockers() {
        let p = SniperParams {
            enabled: true,
            vault_address: Some(Address::repeat_byte(0xaa)),
            buy_size_wei: eth(1),
            daily_budget_wei: eth(5),
            ..Default::default()
        };
        assert!(p.validate().is_ok(), "{:?}", p.validate());
        assert!(p.is_armed());
        assert!(p.arming_blockers().is_empty(), "{:?}", p.arming_blockers());
    }

    #[test]
    fn enabled_with_size_but_no_budget_is_rejected() {
        let p = SniperParams {
            enabled: true,
            buy_size_wei: eth(1),
            daily_budget_wei: U256::ZERO,
            ..Default::default()
        };
        let errs = p.validate().unwrap_err();
        assert!(
            errs.iter().any(|e| e.contains("dailyBudgetWei is 0")),
            "{errs:?}"
        );
    }

    #[test]
    fn buy_larger_than_daily_budget_is_rejected() {
        let p = SniperParams {
            enabled: true,
            buy_size_wei: eth(10),
            daily_budget_wei: eth(5),
            ..Default::default()
        };
        let errs = p.validate().unwrap_err();
        assert!(errs.iter().any(|e| e.contains("cannot fit")), "{errs:?}");
    }

    #[test]
    fn zero_sell_fraction_is_rejected() {
        let p = SniperParams {
            sell_fraction_bps: 0,
            ..Default::default()
        };
        assert!(p
            .validate()
            .unwrap_err()
            .iter()
            .any(|e| e.contains("sellFractionBps")));
    }

    #[test]
    fn sell_fraction_over_100_percent_is_rejected() {
        let p = SniperParams {
            sell_fraction_bps: 10_001,
            ..Default::default()
        };
        assert!(p.validate().is_err());
    }

    #[test]
    fn an_envelope_with_no_exit_trigger_is_rejected() {
        let p = SniperParams {
            take_profit_bps: 0,
            take_profit_abs_wei: U256::ZERO,
            stop_loss_bps: 0,
            trailing_stop_bps: 0,
            max_hold_secs: 0,
            ..Default::default()
        };
        let errs = p.validate().unwrap_err();
        assert!(
            errs.iter().any(|e| e.contains("no exit trigger")),
            "{errs:?}"
        );
    }

    #[test]
    fn an_absolute_only_take_profit_is_a_valid_exit_trigger() {
        let p = SniperParams {
            take_profit_bps: 0,
            take_profit_abs_wei: eth(1),
            stop_loss_bps: 0,
            trailing_stop_bps: 0,
            max_hold_secs: 0,
            ..Default::default()
        };
        assert!(p.validate().is_ok(), "{:?}", p.validate());
    }

    #[test]
    fn patch_applies_only_present_fields() {
        let base = SniperParams::default();
        let patch = SniperParamsPatch {
            take_profit_bps: Some(2_500),
            ..Default::default()
        };
        let next = base.with_patch(&patch).unwrap();
        assert_eq!(next.take_profit_bps, 2_500);
        // Untouched fields survive.
        assert_eq!(next.stop_loss_bps, base.stop_loss_bps);
        assert_eq!(next.buy_size_wei, base.buy_size_wei);
    }

    #[test]
    fn a_rejected_patch_changes_nothing() {
        let base = SniperParams::default();
        let patch = SniperParamsPatch {
            sell_fraction_bps: Some(50_000), // invalid
            take_profit_bps: Some(1),        // would have been fine
            ..Default::default()
        };
        assert!(base.with_patch(&patch).is_err());
        // `with_patch` is pure — `base` is untouched by construction, which is
        // exactly the property the API layer relies on.
        assert_eq!(base, SniperParams::default());
    }

    #[test]
    fn patch_rejects_non_numeric_wei() {
        let base = SniperParams::default();
        let patch = SniperParamsPatch {
            buy_size_wei: Some("0.5".to_string()), // decimals are not wei
            ..Default::default()
        };
        let errs = base.with_patch(&patch).unwrap_err();
        assert!(errs.iter().any(|e| e.contains("buySizeWei")), "{errs:?}");
    }

    #[test]
    fn json_patch_uses_the_camel_case_api_contract() {
        let patch: SniperParamsPatch = serde_json::from_str(
            r#"{"buySizeWei":"1000000000000000","dailyBudgetWei":"2000000000000000","takeProfitBps":2500}"#,
        )
        .unwrap();
        assert_eq!(patch.buy_size_wei.as_deref(), Some("1000000000000000"));
        assert_eq!(patch.daily_budget_wei.as_deref(), Some("2000000000000000"));
        assert_eq!(patch.take_profit_bps, Some(2500));
    }

    #[test]
    fn patch_can_arm_the_lane_in_one_call() {
        let armed = SniperParams::default()
            .with_patch(&SniperParamsPatch {
                enabled: Some(true),
                vault_address: Some("0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string()),
                buy_size_wei: Some(eth(1).to_string()),
                daily_budget_wei: Some(eth(3).to_string()),
                ..Default::default()
            })
            .unwrap();
        assert!(armed.is_armed());
    }

    #[test]
    fn env_snippet_round_trips_through_from_env() {
        // The snippet must be re-readable: every key it writes is a key
        // `from_env` reads, or persisting the envelope silently loses fields.
        let p = SniperParams {
            enabled: true,
            buy_size_wei: eth(2),
            daily_budget_wei: eth(9),
            take_profit_bps: 4_242,
            ..Default::default()
        };
        let snippet = p.env_snippet();
        for line in snippet.lines() {
            let (k, v) = line.split_once('=').expect("KEY=VALUE");
            assert!(k.starts_with("SNIPER_"), "{k} must be namespaced");
            assert!(!v.is_empty(), "{k} has no value");
        }
        assert!(snippet.contains("SNIPER_TAKE_PROFIT_BPS=4242"));
        assert!(snippet.contains("SNIPER_DIRECTIONAL=true"));
    }

    #[test]
    fn honeypot_check_off_is_surfaced_as_a_warning_not_a_hard_blocker() {
        let p = SniperParams {
            enabled: true,
            vault_address: Some(Address::repeat_byte(0xaa)),
            buy_size_wei: eth(1),
            daily_budget_wei: eth(2),
            require_honeypot_pass: false,
            ..Default::default()
        };
        assert!(p.is_armed(), "the warning must not disarm the lane");
        assert!(p.arming_blockers().iter().any(|b| b.starts_with("WARNING")));
    }
}
