//! Admission gates for the directional sniper.
//!
//! Nothing here proposes a trade. This module answers one question — *may we
//! buy this token at all?* — and answers it with a list of reasons rather than
//! a bare bool, so every rejection is explainable in the console and countable
//! in the funnel.
//!
//! The gates are ordered cheapest-first: pure arithmetic on data we already
//! have runs before anything that costs an RPC or a simulation.

use alloy_primitives::{Address, U256};
use serde::{Deserialize, Serialize};

use super::params::{SniperParams, BPS};

/// The honeypot round trip's verdict for a token.
///
/// Produced by simulating `buy → sell` atomically against a fork. This reuses
/// the existing probe machinery in `strategies::sniper`; the difference is
/// that here the verdict *gates a real purchase* instead of being recorded as
/// an observation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HoneypotVerdict {
    /// The sell leg reverted, or returned a negligible amount. Never buy.
    Honeypot,
    /// The round trip lost more than the AMM fee: there is a transfer tax.
    /// Buyable if the measured tax is inside the configured ceilings.
    Taxed { round_trip_bps: u32 },
    /// Round trip returned ~everything but the 2 × 30 bps AMM fee.
    Clean { round_trip_bps: u32 },
    /// The probe could not be completed (RPC failure, fork error). This is
    /// **not** a pass — the gate fails closed on it.
    Unknown,
}

impl HoneypotVerdict {
    pub fn as_str(&self) -> &'static str {
        match self {
            HoneypotVerdict::Honeypot => "honeypot",
            HoneypotVerdict::Taxed { .. } => "taxed",
            HoneypotVerdict::Clean { .. } => "clean",
            HoneypotVerdict::Unknown => "unknown",
        }
    }

    pub fn code(&self) -> &'static str {
        self.as_str()
    }

    /// Round-trip return in bps of the amount spent, where 10_000 means "got
    /// everything back". `None` when no measurement exists.
    pub fn round_trip_bps(&self) -> Option<u32> {
        match self {
            HoneypotVerdict::Taxed { round_trip_bps }
            | HoneypotVerdict::Clean { round_trip_bps } => Some(*round_trip_bps),
            _ => None,
        }
    }

    /// Implied round-trip cost beyond the raw AMM fee, in bps. This is the
    /// number compared against the tax ceilings.
    ///
    /// A V2 round trip pays 30 bps twice, so a perfectly untaxed token returns
    /// ~9,940 bps. Anything materially below that is token-imposed tax.
    pub fn implied_tax_bps(&self) -> Option<u32> {
        const AMM_ROUND_TRIP_BPS: u32 = 9_940;
        self.round_trip_bps()
            .map(|rt| AMM_ROUND_TRIP_BPS.saturating_sub(rt.min(AMM_ROUND_TRIP_BPS)))
    }

    /// Classify a simulated round trip from the wei spent and returned.
    pub fn classify(spent: U256, returned: U256) -> Self {
        if spent.is_zero() {
            return HoneypotVerdict::Unknown;
        }
        if returned.is_zero() {
            return HoneypotVerdict::Honeypot;
        }
        let bps = (returned.saturating_mul(U256::from(BPS)) / spent)
            .min(U256::from(u32::MAX))
            .to::<u32>();
        match bps {
            0..=5_000 => HoneypotVerdict::Honeypot,
            5_001..=9_800 => HoneypotVerdict::Taxed {
                round_trip_bps: bps,
            },
            _ => HoneypotVerdict::Clean {
                round_trip_bps: bps,
            },
        }
    }
}

/// Everything the gate needs to know about a candidate launch.
#[derive(Clone, Debug)]
pub struct LaunchCandidate {
    pub token: Address,
    pub pair: Address,
    /// WETH-side reserve of the pool at the state we would buy against.
    pub weth_reserve: U256,
    /// Token-side reserve.
    pub token_reserve: U256,
    pub verdict: HoneypotVerdict,
    /// Whether the LP position is burned / locked, if known.
    pub lp_locked: Option<bool>,
    /// True if the token has already been rejected once.
    pub blacklisted: bool,
}

/// Live exposure state the gate checks the candidate against.
#[derive(Clone, Copy, Debug, Default)]
pub struct ExposureSnapshot {
    pub live_positions: usize,
    /// Entry wei committed in the rolling 24h window.
    pub spent_today_wei: U256,
    /// Entry wei committed over the lane's lifetime.
    pub spent_total_wei: U256,
    /// Realised sniper PnL, signed. Negative values feed the drawdown stop.
    pub realized_pnl_wei: i128,
    /// True when an operator or the drawdown rule has halted the lane.
    pub halted: bool,
}

/// Why a launch was not bought. Every variant is a counter in the funnel.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "reason", content = "detail")]
pub enum Rejection {
    /// The lane is not armed. Carries the blocker list.
    NotArmed(Vec<String>),
    Halted,
    Blacklisted,
    HoneypotFailed(String),
    VerdictUnknown,
    TaxTooHigh {
        measured_bps: u32,
        ceiling_bps: u32,
    },
    LiquidityTooThin {
        have_wei: String,
        need_wei: String,
    },
    PriceImpactTooHigh {
        impact_bps: u32,
        ceiling_bps: u32,
    },
    LpNotLocked,
    TooManyPositions {
        live: usize,
        cap: usize,
    },
    DailyBudgetExhausted {
        spent_wei: String,
        cap_wei: String,
    },
    TotalBudgetExhausted {
        spent_wei: String,
        cap_wei: String,
    },
    DrawdownStop {
        pnl_wei: String,
        cap_wei: String,
    },
}

impl Rejection {
    /// Stable short code for metrics/funnel labels.
    pub fn code(&self) -> &'static str {
        match self {
            Rejection::NotArmed(_) => "not_armed",
            Rejection::Halted => "halted",
            Rejection::Blacklisted => "blacklisted",
            Rejection::HoneypotFailed(_) => "honeypot",
            Rejection::VerdictUnknown => "verdict_unknown",
            Rejection::TaxTooHigh { .. } => "tax_too_high",
            Rejection::LiquidityTooThin { .. } => "liquidity_thin",
            Rejection::PriceImpactTooHigh { .. } => "price_impact",
            Rejection::LpNotLocked => "lp_not_locked",
            Rejection::TooManyPositions { .. } => "position_cap",
            Rejection::DailyBudgetExhausted { .. } => "daily_budget",
            Rejection::TotalBudgetExhausted { .. } => "total_budget",
            Rejection::DrawdownStop { .. } => "drawdown_stop",
        }
    }
}

/// An admitted launch, carrying the size the gate approved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Admission {
    /// The wei to actually commit. May be **less** than `buy_size_wei` when
    /// the remaining budget only allows a smaller entry.
    pub size_wei: U256,
    /// Price impact this size causes, in bps.
    pub impact_bps: u32,
    /// Verdict that admitted it, for the position's audit trail.
    pub verdict: HoneypotVerdict,
}

/// Run every admission gate. `Ok` carries the approved size.
///
/// Budget clamping deserves a note: when the daily budget has room for less
/// than a full `buy_size_wei`, the gate approves the smaller amount rather
/// than rejecting. The operator's budget is a ceiling on spend, not a
/// quantiser on entries — and a half-size entry is strictly safer than the
/// full one they already approved.
pub fn admit(
    params: &SniperParams,
    candidate: &LaunchCandidate,
    exposure: &ExposureSnapshot,
) -> Result<Admission, Rejection> {
    if !params.is_armed() {
        return Err(Rejection::NotArmed(params.arming_blockers()));
    }
    if exposure.halted {
        return Err(Rejection::Halted);
    }
    if candidate.blacklisted {
        return Err(Rejection::Blacklisted);
    }

    // --- drawdown stop ---------------------------------------------------
    if !params.max_drawdown_wei.is_zero() {
        let cap = params.max_drawdown_wei.min(U256::from(i128::MAX as u128));
        let cap_i = cap.to::<u128>() as i128;
        if exposure.realized_pnl_wei <= -cap_i {
            return Err(Rejection::DrawdownStop {
                pnl_wei: exposure.realized_pnl_wei.to_string(),
                cap_wei: params.max_drawdown_wei.to_string(),
            });
        }
    }

    // --- concurrency -----------------------------------------------------
    if exposure.live_positions >= params.max_concurrent_positions {
        return Err(Rejection::TooManyPositions {
            live: exposure.live_positions,
            cap: params.max_concurrent_positions,
        });
    }

    // --- honeypot / tax --------------------------------------------------
    match candidate.verdict {
        HoneypotVerdict::Honeypot => {
            return Err(Rejection::HoneypotFailed(
                "round-trip sell returned nothing or lost more than half".to_string(),
            ));
        }
        HoneypotVerdict::Unknown if params.require_honeypot_pass => {
            // Fail closed. An unreachable probe is not a passing probe.
            return Err(Rejection::VerdictUnknown);
        }
        _ => {}
    }
    if let Some(tax) = candidate.verdict.implied_tax_bps() {
        // A round trip crosses both sides, so it measures buy tax + sell tax
        // together. Compare it against the sum of the two ceilings.
        let ceiling = params
            .max_buy_tax_bps
            .saturating_add(params.max_sell_tax_bps);
        if tax > ceiling {
            return Err(Rejection::TaxTooHigh {
                measured_bps: tax,
                ceiling_bps: ceiling,
            });
        }
    }

    // --- LP lock ---------------------------------------------------------
    if params.require_lp_locked && candidate.lp_locked != Some(true) {
        return Err(Rejection::LpNotLocked);
    }

    // --- liquidity -------------------------------------------------------
    if candidate.weth_reserve < params.min_liquidity_wei {
        return Err(Rejection::LiquidityTooThin {
            have_wei: candidate.weth_reserve.to_string(),
            need_wei: params.min_liquidity_wei.to_string(),
        });
    }

    // --- budget ----------------------------------------------------------
    let mut size = params.buy_size_wei;

    let daily_room = params
        .daily_budget_wei
        .saturating_sub(exposure.spent_today_wei);
    if daily_room.is_zero() {
        return Err(Rejection::DailyBudgetExhausted {
            spent_wei: exposure.spent_today_wei.to_string(),
            cap_wei: params.daily_budget_wei.to_string(),
        });
    }
    size = size.min(daily_room);

    if !params.total_budget_wei.is_zero() {
        let total_room = params
            .total_budget_wei
            .saturating_sub(exposure.spent_total_wei);
        if total_room.is_zero() {
            return Err(Rejection::TotalBudgetExhausted {
                spent_wei: exposure.spent_total_wei.to_string(),
                cap_wei: params.total_budget_wei.to_string(),
            });
        }
        size = size.min(total_room);
    }

    // --- price impact ----------------------------------------------------
    // Checked last because it depends on the budget-clamped size: a smaller
    // entry has a smaller impact, and rejecting on the unclamped size would
    // turn down trades we are in fact able to make safely.
    let impact_bps = price_impact_bps(size, candidate.weth_reserve);
    if impact_bps > params.max_price_impact_bps {
        return Err(Rejection::PriceImpactTooHigh {
            impact_bps,
            ceiling_bps: params.max_price_impact_bps,
        });
    }

    Ok(Admission {
        size_wei: size,
        impact_bps,
        verdict: candidate.verdict,
    })
}

/// Price impact of buying `size` against `reserve_in`, in bps.
///
/// For a constant-product pool the input-side impact is `size / (reserve +
/// size)`. Using the post-trade denominator matters: a buy equal to the whole
/// reserve is a 50% impact, not 100%.
pub fn price_impact_bps(size: U256, reserve_in: U256) -> u32 {
    if reserve_in.is_zero() {
        return BPS; // no liquidity == maximal impact
    }
    let denom = reserve_in.saturating_add(size);
    if denom.is_zero() {
        return BPS;
    }
    (size.saturating_mul(U256::from(BPS)) / denom)
        .min(U256::from(BPS))
        .to::<u32>()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eth(n: u64) -> U256 {
        U256::from(n) * U256::from(1_000_000_000_000_000_000u128)
    }
    fn centi(n: u64) -> U256 {
        U256::from(n) * U256::from(10_000_000_000_000_000u128)
    }

    fn armed() -> SniperParams {
        SniperParams {
            enabled: true,
            vault_address: Some(Address::repeat_byte(0xaa)),
            buy_size_wei: centi(10), // 0.1 ETH
            daily_budget_wei: eth(1),
            min_liquidity_wei: eth(2),
            max_price_impact_bps: 300,
            max_concurrent_positions: 3,
            ..Default::default()
        }
    }

    fn clean_candidate() -> LaunchCandidate {
        LaunchCandidate {
            token: Address::with_last_byte(1),
            pair: Address::with_last_byte(2),
            weth_reserve: eth(10),
            token_reserve: U256::from(1_000_000u64),
            verdict: HoneypotVerdict::Clean {
                round_trip_bps: 9_940,
            },
            lp_locked: None,
            blacklisted: false,
        }
    }

    #[test]
    fn a_clean_launch_is_admitted_at_full_size() {
        let a = admit(&armed(), &clean_candidate(), &ExposureSnapshot::default()).unwrap();
        assert_eq!(a.size_wei, centi(10));
    }

    #[test]
    fn the_default_envelope_admits_nothing() {
        let err = admit(
            &SniperParams::default(),
            &clean_candidate(),
            &ExposureSnapshot::default(),
        )
        .unwrap_err();
        assert_eq!(err.code(), "not_armed");
    }

    #[test]
    fn honeypots_are_always_rejected_even_with_checks_disabled() {
        let mut p = armed();
        p.require_honeypot_pass = false;
        let mut c = clean_candidate();
        c.verdict = HoneypotVerdict::Honeypot;
        let err = admit(&p, &c, &ExposureSnapshot::default()).unwrap_err();
        assert_eq!(err.code(), "honeypot");
    }

    #[test]
    fn unknown_verdict_fails_closed_when_the_check_is_required() {
        let mut c = clean_candidate();
        c.verdict = HoneypotVerdict::Unknown;
        let err = admit(&armed(), &c, &ExposureSnapshot::default()).unwrap_err();
        assert_eq!(err.code(), "verdict_unknown");
    }

    #[test]
    fn unknown_verdict_passes_when_the_operator_opted_out() {
        let mut p = armed();
        p.require_honeypot_pass = false;
        let mut c = clean_candidate();
        c.verdict = HoneypotVerdict::Unknown;
        assert!(admit(&p, &c, &ExposureSnapshot::default()).is_ok());
    }

    #[test]
    fn tax_above_the_combined_ceiling_is_rejected() {
        let mut p = armed();
        p.max_buy_tax_bps = 200;
        p.max_sell_tax_bps = 200; // combined 400
        let mut c = clean_candidate();
        // 9,940 - 9,000 = 940 bps of tax.
        c.verdict = HoneypotVerdict::Taxed {
            round_trip_bps: 9_000,
        };
        let err = admit(&p, &c, &ExposureSnapshot::default()).unwrap_err();
        assert_eq!(err.code(), "tax_too_high");
    }

    #[test]
    fn tax_inside_the_ceiling_is_admitted() {
        let mut p = armed();
        p.max_buy_tax_bps = 600;
        p.max_sell_tax_bps = 600;
        let mut c = clean_candidate();
        c.verdict = HoneypotVerdict::Taxed {
            round_trip_bps: 9_000,
        };
        assert!(admit(&p, &c, &ExposureSnapshot::default()).is_ok());
    }

    #[test]
    fn thin_liquidity_is_rejected() {
        let mut c = clean_candidate();
        c.weth_reserve = centi(50); // 0.5 ETH, below the 2 ETH floor
        let err = admit(&armed(), &c, &ExposureSnapshot::default()).unwrap_err();
        assert_eq!(err.code(), "liquidity_thin");
    }

    #[test]
    fn price_impact_over_the_ceiling_is_rejected() {
        let mut p = armed();
        p.buy_size_wei = eth(1); // 1 ETH into a 10 ETH pool ≈ 909 bps
        p.daily_budget_wei = eth(10);
        let err = admit(&p, &clean_candidate(), &ExposureSnapshot::default()).unwrap_err();
        assert_eq!(err.code(), "price_impact");
    }

    #[test]
    fn price_impact_uses_the_post_trade_denominator() {
        // Buying the entire reserve is a 50% impact, not 100%.
        assert_eq!(price_impact_bps(eth(10), eth(10)), 5_000);
        assert_eq!(price_impact_bps(U256::ZERO, eth(10)), 0);
        // No liquidity is maximal impact, not a divide-by-zero.
        assert_eq!(price_impact_bps(eth(1), U256::ZERO), BPS);
    }

    #[test]
    fn the_position_cap_is_enforced() {
        let e = ExposureSnapshot {
            live_positions: 3,
            ..Default::default()
        };
        let err = admit(&armed(), &clean_candidate(), &e).unwrap_err();
        assert_eq!(err.code(), "position_cap");
    }

    #[test]
    fn an_exhausted_daily_budget_rejects() {
        let e = ExposureSnapshot {
            spent_today_wei: eth(1),
            ..Default::default()
        };
        let err = admit(&armed(), &clean_candidate(), &e).unwrap_err();
        assert_eq!(err.code(), "daily_budget");
    }

    #[test]
    fn a_partial_daily_budget_clamps_the_size_instead_of_rejecting() {
        // 0.96 ETH of a 1 ETH daily budget is spent; a 0.1 ETH entry does not
        // fit, but a 0.04 ETH one does.
        let e = ExposureSnapshot {
            spent_today_wei: centi(96),
            ..Default::default()
        };
        let a = admit(&armed(), &clean_candidate(), &e).unwrap();
        assert_eq!(a.size_wei, centi(4));
    }

    #[test]
    fn the_lifetime_budget_also_clamps() {
        let mut p = armed();
        p.total_budget_wei = eth(5);
        let e = ExposureSnapshot {
            spent_total_wei: centi(497), // 4.97 ETH used of 5
            ..Default::default()
        };
        let a = admit(&p, &clean_candidate(), &e).unwrap();
        assert_eq!(a.size_wei, centi(3));
    }

    #[test]
    fn an_exhausted_lifetime_budget_rejects() {
        let mut p = armed();
        p.total_budget_wei = eth(5);
        let e = ExposureSnapshot {
            spent_total_wei: eth(5),
            ..Default::default()
        };
        let err = admit(&p, &clean_candidate(), &e).unwrap_err();
        assert_eq!(err.code(), "total_budget");
    }

    #[test]
    fn a_zero_lifetime_budget_means_unlimited() {
        let mut p = armed();
        p.total_budget_wei = U256::ZERO;
        let e = ExposureSnapshot {
            spent_total_wei: eth(1_000),
            ..Default::default()
        };
        assert!(admit(&p, &clean_candidate(), &e).is_ok());
    }

    #[test]
    fn the_drawdown_stop_halts_new_entries() {
        let mut p = armed();
        p.max_drawdown_wei = centi(50); // stop at -0.5 ETH
        let e = ExposureSnapshot {
            realized_pnl_wei: -(centi(50).to::<u128>() as i128),
            ..Default::default()
        };
        let err = admit(&p, &clean_candidate(), &e).unwrap_err();
        assert_eq!(err.code(), "drawdown_stop");
    }

    #[test]
    fn profit_does_not_trip_the_drawdown_stop() {
        let mut p = armed();
        p.max_drawdown_wei = centi(50);
        let e = ExposureSnapshot {
            realized_pnl_wei: centi(500).to::<u128>() as i128,
            ..Default::default()
        };
        assert!(admit(&p, &clean_candidate(), &e).is_ok());
    }

    #[test]
    fn a_halted_lane_rejects_everything() {
        let e = ExposureSnapshot {
            halted: true,
            ..Default::default()
        };
        assert_eq!(
            admit(&armed(), &clean_candidate(), &e).unwrap_err().code(),
            "halted"
        );
    }

    #[test]
    fn blacklisted_tokens_are_rejected() {
        let mut c = clean_candidate();
        c.blacklisted = true;
        assert_eq!(
            admit(&armed(), &c, &ExposureSnapshot::default())
                .unwrap_err()
                .code(),
            "blacklisted"
        );
    }

    #[test]
    fn lp_lock_is_only_enforced_when_required() {
        let mut c = clean_candidate();
        c.lp_locked = None;
        assert!(admit(&armed(), &c, &ExposureSnapshot::default()).is_ok());

        let mut p = armed();
        p.require_lp_locked = true;
        assert_eq!(
            admit(&p, &c, &ExposureSnapshot::default())
                .unwrap_err()
                .code(),
            "lp_not_locked"
        );

        c.lp_locked = Some(true);
        assert!(admit(&p, &c, &ExposureSnapshot::default()).is_ok());
    }

    #[test]
    fn classify_matches_the_documented_bands() {
        let one = U256::from(1_000_000u64);
        assert_eq!(
            HoneypotVerdict::classify(one, U256::ZERO),
            HoneypotVerdict::Honeypot
        );
        assert_eq!(
            HoneypotVerdict::classify(one, U256::from(400_000u64)),
            HoneypotVerdict::Honeypot
        );
        assert!(matches!(
            HoneypotVerdict::classify(one, U256::from(900_000u64)),
            HoneypotVerdict::Taxed { .. }
        ));
        assert!(matches!(
            HoneypotVerdict::classify(one, U256::from(994_000u64)),
            HoneypotVerdict::Clean { .. }
        ));
        // A zero spend is unmeasurable, not clean.
        assert_eq!(
            HoneypotVerdict::classify(U256::ZERO, one),
            HoneypotVerdict::Unknown
        );
    }

    #[test]
    fn implied_tax_discounts_the_amm_fee() {
        let clean = HoneypotVerdict::Clean {
            round_trip_bps: 9_940,
        };
        assert_eq!(clean.implied_tax_bps(), Some(0));
        let taxed = HoneypotVerdict::Taxed {
            round_trip_bps: 8_940,
        };
        assert_eq!(taxed.implied_tax_bps(), Some(1_000));
        // A round trip that somehow beat the fee is not negative tax.
        let lucky = HoneypotVerdict::Clean {
            round_trip_bps: 10_100,
        };
        assert_eq!(lucky.implied_tax_bps(), Some(0));
        assert_eq!(HoneypotVerdict::Unknown.implied_tax_bps(), None);
    }
}
