//! Machine-readable seven-day shadow qualification gate.
//!
//! Duration alone never passes: the run must be uninterrupted, retain every
//! critical decision record, produce enough exact-payload fork/relay samples,
//! and match enough high-confidence on-chain MEV observations.

use serde::Serialize;

use crate::config::Config;
use crate::store::{AsyncStore, Store};

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QualificationStatus {
    pub pass: bool,
    pub started_at_ms: u64,
    pub elapsed_hours: u64,
    pub required_hours: u64,
    pub live_candidate_simulations: u64,
    pub relay_cross_checks: u64,
    pub high_confidence_actual_matches: u64,
    pub minimum_actual_matches: u64,
    pub persistence_dropped: u64,
    pub reasons: Vec<String>,
}

pub fn evaluate(
    cfg: &Config,
    store: &Store,
    writes: &AsyncStore,
    started_at_ms: u64,
    now_ms: u64,
) -> QualificationStatus {
    let elapsed_hours = now_ms.saturating_sub(started_at_ms) / 3_600_000;
    let (live_candidates, relay_cross_checks, actual_matches) = store
        .qualification_counts(started_at_ms)
        .unwrap_or((0, 0, 0));
    let dropped = writes.dropped();
    let minimum = cfg.qualification_min_actual_matches;
    let mut reasons = Vec::new();
    if elapsed_hours < cfg.qualification_hours {
        reasons.push(format!(
            "uninterrupted shadow duration is {elapsed_hours}h; {}h required",
            cfg.qualification_hours
        ));
    }
    if dropped != 0 {
        reasons.push(format!("{dropped} decision/telemetry writes were dropped"));
    }
    if live_candidates < minimum {
        reasons.push(format!(
            "{live_candidates} successful live-candidate fork simulations; {minimum} required"
        ));
    }
    if relay_cross_checks < minimum {
        reasons.push(format!(
            "{relay_cross_checks} successful exact-payload relay cross-checks; {minimum} required"
        ));
    }
    if actual_matches < minimum {
        reasons.push(format!(
            "{actual_matches} high-confidence on-chain MEV matches; {minimum} required"
        ));
    }
    QualificationStatus {
        pass: reasons.is_empty(),
        started_at_ms,
        elapsed_hours,
        required_hours: cfg.qualification_hours,
        live_candidate_simulations: live_candidates,
        relay_cross_checks,
        high_confidence_actual_matches: actual_matches,
        minimum_actual_matches: minimum,
        persistence_dropped: dropped,
        reasons,
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn elapsed_time_cannot_underflow() {
        assert_eq!(10u64.saturating_sub(20) / 3_600_000, 0);
    }
}
