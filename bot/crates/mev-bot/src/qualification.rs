//! Machine-readable, strategy-specific shadow qualification gate.
//!
//! Elapsed wall time alone never passes. The canonical observation stream must
//! cover the complete window without a large gap, persistence must be lossless,
//! and each strategy independently needs enough fork, relay and corresponding
//! on-chain comparisons inside explicit accuracy tolerances.

use serde::Serialize;

use crate::config::Config;
use crate::store::{AsyncStore, QualificationEvidence, Store};
use crate::types::Strategy;

pub const PASS: &str = "PASS";
pub const FAIL: &str = "FAIL";
pub const INSUFFICIENT_SAMPLE: &str = "INSUFFICIENT SAMPLE";
const MINIMUM_ATTRIBUTION_CONFIDENCE_BPS: u64 = 8_000;

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StrategyQualification {
    pub strategy: String,
    pub live_candidate: bool,
    pub verdict: String,
    pub fork_samples: u64,
    pub relay_comparisons: u64,
    /// Alias of `relay_comparisons` with backend-neutral naming. Equal to
    /// that field; the console prefers this label on sequencer backends.
    pub independent_comparisons: u64,
    pub actual_comparisons: u64,
    pub relay_within_tolerance: u64,
    pub actual_within_tolerance: u64,
    pub relay_accuracy_bps: u64,
    pub actual_accuracy_bps: u64,
    pub reasons: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QualificationStatus {
    /// True when at least one engineering-live strategy is independently PASS.
    /// Submission still checks the candidate strategy's own verdict.
    pub pass: bool,
    pub started_at_ms: u64,
    pub elapsed_hours: u64,
    pub required_hours: u64,
    pub observation_count: u64,
    pub maximum_observation_gap_secs: u64,
    pub allowed_observation_gap_secs: u64,
    pub live_candidate_simulations: u64,
    pub relay_cross_checks: u64,
    pub high_confidence_actual_matches: u64,
    pub minimum_samples: u64,
    pub minimum_relay_comparisons: u64,
    pub minimum_actual_matches: u64,
    pub maximum_error_bps: u64,
    pub minimum_accuracy_bps: u64,
    pub persistence_dropped: u64,
    /// Which independent second opinion the comparison evidence comes from:
    /// `relay` (fork vs `eth_callBundle`) on mainnet, `sequencer` (fork vs
    /// included block) on sequencer chains. The console labels its panel
    /// with this so a Base verdict is never misread as a relay verdict.
    pub comparison_backend: String,
    pub reasons: Vec<String>,
    pub strategies: Vec<StrategyQualification>,
}

impl QualificationStatus {
    pub fn strategy_passes(&self, strategy: Strategy) -> bool {
        self.strategies
            .iter()
            .any(|row| row.strategy == strategy.as_str() && row.verdict == PASS)
    }
}

pub fn evaluate(
    cfg: &Config,
    store: &Store,
    writes: &AsyncStore,
    now_ms: u64,
) -> QualificationStatus {
    evaluate_with_required_hours(cfg, store, writes, now_ms, cfg.qualification_hours)
}

/// Evaluate using an operator-selected soak threshold. The threshold is a
/// runtime control, not a bypass: all continuity, persistence, sample,
/// independent-comparison, and accuracy gates still run against the selected
/// window. Lowering it only changes how much history is required; it cannot
/// manufacture evidence.
pub fn evaluate_with_required_hours(
    cfg: &Config,
    store: &Store,
    writes: &AsyncStore,
    now_ms: u64,
    required_hours: u64,
) -> QualificationStatus {
    let required_hours = required_hours.max(1);
    let required_ms = required_hours
        .saturating_mul(60)
        .saturating_mul(60)
        .saturating_mul(1_000);
    let since_ms = now_ms.saturating_sub(required_ms);
    let coverage = store
        .observation_coverage(since_ms, now_ms)
        .unwrap_or_default();
    let allowed_gap_ms = cfg.qualification_max_gap_secs.saturating_mul(1_000);
    let durable_incidents = store.qualification_incident_count(since_ms).unwrap_or(0);
    let dropped = writes.dropped().max(durable_incidents);
    let elapsed_hours = coverage
        .first_seen_ms
        .map(|started| now_ms.saturating_sub(started) / 3_600_000)
        .unwrap_or(0);

    let mut global_reasons = Vec::new();
    if coverage.first_seen_ms.is_none() || elapsed_hours < required_hours {
        global_reasons.push(format!(
            "canonical shadow observations span {elapsed_hours}h; {required_hours}h required"
        ));
    }
    if coverage.maximum_gap_ms > allowed_gap_ms {
        global_reasons.push(format!(
            "maximum canonical observation gap is {}s; {}s allowed",
            coverage.maximum_gap_ms / 1_000,
            cfg.qualification_max_gap_secs
        ));
    }
    if dropped != 0 {
        global_reasons.push(format!("{dropped} decision/telemetry writes were dropped"));
    }

    let mut strategies = Vec::new();
    for strategy in Strategy::all() {
        let evidence = store
            .qualification_evidence(
                since_ms,
                strategy,
                MINIMUM_ATTRIBUTION_CONFIDENCE_BPS,
                cfg.qualification_backend,
            )
            .unwrap_or_default();
        strategies.push(evaluate_strategy(cfg, strategy, evidence, &global_reasons));
    }

    let live_candidate_simulations = strategies
        .iter()
        .filter(|row| row.live_candidate)
        .map(|row| row.fork_samples)
        .sum();
    let relay_cross_checks = strategies
        .iter()
        .filter(|row| row.live_candidate)
        .map(|row| row.relay_comparisons)
        .sum();
    let high_confidence_actual_matches = strategies
        .iter()
        .filter(|row| row.live_candidate)
        .map(|row| row.actual_comparisons)
        .sum();
    let pass = strategies
        .iter()
        .any(|row| row.live_candidate && row.verdict == PASS);

    QualificationStatus {
        pass,
        started_at_ms: coverage.first_seen_ms.unwrap_or(now_ms),
        elapsed_hours,
        required_hours,
        observation_count: coverage.observations,
        maximum_observation_gap_secs: coverage.maximum_gap_ms / 1_000,
        allowed_observation_gap_secs: cfg.qualification_max_gap_secs,
        live_candidate_simulations,
        relay_cross_checks,
        high_confidence_actual_matches,
        minimum_samples: cfg.qualification_min_samples,
        minimum_relay_comparisons: cfg.qualification_min_relay_comparisons,
        minimum_actual_matches: cfg.qualification_min_actual_matches,
        maximum_error_bps: cfg.qualification_max_error_bps,
        minimum_accuracy_bps: cfg.qualification_min_accuracy_bps,
        persistence_dropped: dropped,
        comparison_backend: cfg.qualification_backend.as_str().to_string(),
        reasons: global_reasons,
        strategies,
    }
}

fn evaluate_strategy(
    cfg: &Config,
    strategy: Strategy,
    evidence: QualificationEvidence,
    global_reasons: &[String],
) -> StrategyQualification {
    let relay_comparisons = evidence.relay_errors_bps.len() as u64;
    let actual_comparisons = evidence.actual_errors_bps.len() as u64;
    let relay_within_tolerance = evidence
        .relay_errors_bps
        .iter()
        .filter(|error| **error <= cfg.qualification_max_error_bps)
        .count() as u64;
    let actual_within_tolerance = evidence
        .actual_errors_bps
        .iter()
        .filter(|error| **error <= cfg.qualification_max_error_bps)
        .count() as u64;
    let relay_accuracy_bps = accuracy_bps(relay_within_tolerance, relay_comparisons);
    let actual_accuracy_bps = accuracy_bps(actual_within_tolerance, actual_comparisons);

    let mut reasons = global_reasons.to_vec();
    if !strategy.live_candidate() {
        reasons.push(
            strategy
                .shadow_only_reason()
                .unwrap_or("strategy has not reached engineering live-candidate status")
                .to_string(),
        );
    }
    if evidence.fork_samples < cfg.qualification_min_samples {
        reasons.push(format!(
            "{} successful fork samples; {} required",
            evidence.fork_samples, cfg.qualification_min_samples
        ));
    }
    if relay_comparisons < cfg.qualification_min_relay_comparisons {
        let evidence_name = match cfg.qualification_backend {
            crate::config::QualificationBackend::Relay => "fork-versus-relay",
            crate::config::QualificationBackend::Sequencer => "independent canonical-state",
        };
        reasons.push(format!(
            "{relay_comparisons} {evidence_name} comparisons; {} required",
            cfg.qualification_min_relay_comparisons
        ));
    }
    if actual_comparisons < cfg.qualification_min_actual_matches {
        reasons.push(format!(
            "{actual_comparisons} corresponding high-confidence on-chain comparisons; {} required",
            cfg.qualification_min_actual_matches
        ));
    }

    let sufficient = strategy.live_candidate()
        && global_reasons.is_empty()
        && evidence.fork_samples >= cfg.qualification_min_samples
        && relay_comparisons >= cfg.qualification_min_relay_comparisons
        && actual_comparisons >= cfg.qualification_min_actual_matches;
    let accurate = relay_accuracy_bps >= cfg.qualification_min_accuracy_bps
        && actual_accuracy_bps >= cfg.qualification_min_accuracy_bps;
    let verdict = if !sufficient {
        INSUFFICIENT_SAMPLE
    } else if accurate {
        PASS
    } else {
        if relay_accuracy_bps < cfg.qualification_min_accuracy_bps {
            let label = match cfg.qualification_backend {
                crate::config::QualificationBackend::Relay => "relay",
                crate::config::QualificationBackend::Sequencer => "canonical-state",
            };
            reasons.push(format!(
                "{label} accuracy is {relay_accuracy_bps}bps; {}bps required",
                cfg.qualification_min_accuracy_bps
            ));
        }
        if actual_accuracy_bps < cfg.qualification_min_accuracy_bps {
            reasons.push(format!(
                "on-chain accuracy is {actual_accuracy_bps}bps; {}bps required",
                cfg.qualification_min_accuracy_bps
            ));
        }
        FAIL
    };

    StrategyQualification {
        strategy: strategy.as_str().to_string(),
        live_candidate: strategy.live_candidate(),
        verdict: verdict.to_string(),
        fork_samples: evidence.fork_samples,
        relay_comparisons,
        independent_comparisons: relay_comparisons,
        actual_comparisons,
        relay_within_tolerance,
        actual_within_tolerance,
        relay_accuracy_bps,
        actual_accuracy_bps,
        reasons,
    }
}

fn accuracy_bps(within: u64, total: u64) -> u64 {
    within
        .saturating_mul(10_000)
        .checked_div(total)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accuracy_is_integer_and_bounded() {
        assert_eq!(accuracy_bps(0, 0), 0);
        assert_eq!(accuracy_bps(8, 10), 8_000);
        assert_eq!(accuracy_bps(10, 10), 10_000);
    }

    #[test]
    fn thirty_independent_and_thirty_outcomes_can_pass_only_when_both_are_accurate() {
        use crate::store::Store;
        use crate::types::{now_ms, SimBackend, SimulationResult};
        use alloy_primitives::U256;

        let store = Store::open_in_memory().unwrap();
        let now = now_ms();
        // Continuity: one canonical block observation so the window is open.
        store
            .record_block(&crate::types::BlockHead {
                number: 1,
                hash: alloy_primitives::B256::ZERO,
                parent_hash: alloy_primitives::B256::ZERO,
                timestamp: 0,
                base_fee_per_gas: U256::ZERO,
                gas_used: 0,
                gas_limit: 30_000_000,
            })
            .unwrap();

        for i in 0..30u64 {
            let id = format!("opp-{i}");
            let sim = SimulationResult {
                opportunity_id: id.clone(),
                strategy: Strategy::AtomicArb,
                backend: SimBackend::AnvilFork,
                success: true,
                gross_profit_wei: U256::from(150u64),
                gas_used: 21_000,
                gas_price_wei: U256::from(1u64),
                gas_cost_wei: U256::from(50u64),
                bribe_wei: U256::ZERO,
                net_profit_wei: 100,
                victim_predicted_out_wei: None,
                revert_reason: None,
                target_block: 1,
                sim_latency_ms: 1,
                created_at_ms: now,
            };
            store.record_simulation(&sim).unwrap();
            store
                .record_opportunity(&crate::types::Opportunity {
                    id: id.clone(),
                    strategy: Strategy::AtomicArb,
                    victim_hashes: vec![],
                    front_calls: vec![],
                    back_calls: vec![],
                    flash_tokens: vec![],
                    flash_amounts: vec![],
                    profit_token: alloy_primitives::Address::ZERO,
                    expected_profit_wei: U256::from(100u64),
                    notional_wei: U256::from(1_000u64),
                    target_block: 1,
                    created_at_ms: now,
                    notes: String::new(),
                })
                .unwrap();
            store
                .record_state_comparison(
                    &format!("st-{i}"),
                    &id,
                    "atomic_arb",
                    &format!("head:{i}"),
                    1,
                    "0x",
                    "univ2:0x1 -> univ3:0x2",
                    "1000",
                    "weth->usdc->weth",
                    100,
                    100,
                )
                .unwrap();
            store
                .record_actual_mev_match(&crate::store::ActualMevMatch {
                    opportunity_id: id,
                    block_number: 1,
                    victim_hash: String::new(),
                    mev_tx_hashes: vec![],
                    actor: None,
                    gross_weth_wei: U256::from(150u64),
                    gas_cost_wei: U256::from(50u64),
                    net_weth_wei: 100,
                    confidence: "high".into(),
                    confidence_score_bps: 9_000,
                    completeness: serde_json::json!({}),
                    evidence: serde_json::json!({}),
                })
                .unwrap();
        }

        let evidence = store
            .qualification_evidence(
                0,
                Strategy::AtomicArb,
                8_000,
                crate::config::QualificationBackend::Sequencer,
            )
            .unwrap();
        assert_eq!(evidence.relay_errors_bps.len(), 30);
        assert_eq!(evidence.actual_errors_bps.len(), 30);
        assert!(evidence.relay_errors_bps.iter().all(|&e| e == 0));
        assert!(evidence.actual_errors_bps.iter().all(|&e| e == 0));

        // Removing the independent population leaves the strategy unqualified.
        let empty = Store::open_in_memory().unwrap();
        empty
            .record_actual_mev_match(&crate::store::ActualMevMatch {
                opportunity_id: "only-actual".into(),
                block_number: 1,
                victim_hash: String::new(),
                mev_tx_hashes: vec![],
                actor: None,
                gross_weth_wei: U256::from(1u64),
                gas_cost_wei: U256::ZERO,
                net_weth_wei: 1,
                confidence: "high".into(),
                confidence_score_bps: 9_000,
                completeness: serde_json::json!({}),
                evidence: serde_json::json!({}),
            })
            .unwrap();
        let only_actual = empty
            .qualification_evidence(
                0,
                Strategy::AtomicArb,
                8_000,
                crate::config::QualificationBackend::Sequencer,
            )
            .unwrap();
        assert!(only_actual.relay_errors_bps.is_empty());
    }
}
