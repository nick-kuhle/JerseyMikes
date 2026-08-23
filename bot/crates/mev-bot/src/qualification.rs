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
    let required_ms = cfg
        .qualification_hours
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
    if coverage.first_seen_ms.is_none() || elapsed_hours < cfg.qualification_hours {
        global_reasons.push(format!(
            "canonical shadow observations span {elapsed_hours}h; {}h required",
            cfg.qualification_hours
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
        required_hours: cfg.qualification_hours,
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
}
