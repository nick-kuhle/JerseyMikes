//! Per-stage timing histograms for the mempool → bundle path.
//!
//! Phase 1's latency budget is ~150 ms end-to-end. Anything slower is not a
//! competitive searcher on Ethereum mainnet: the next block is 12 s away but
//! the *auction* for it is decided in the last couple of hundred milliseconds.
//! These histograms are how we know whether we are inside that window.

use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;
use serde_json::{json, Value};

/// The mempool → signed-bundle budget, in milliseconds.
pub const BUDGET_MS: u64 = 150;

/// Inclusive upper bounds of each histogram bucket, in milliseconds.
///
/// The 150 ms bucket is the budget itself so the dashboard can read
/// "how much of the mass is inside the budget" off a single bar.
pub const BOUNDS_MS: [u64; 14] = [
    1, 2, 5, 10, 20, 50, 75, 100, 150, 200, 300, 500, 1_000, 2_500,
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Stage {
    /// Pending hash observed → hydrated `PendingTx` handed to a strategy.
    IngestToStrategy,
    /// `StrategyImpl::on_pending` / `on_block` wall time.
    Strategy,
    /// `RiskEngine::check` (should be noise).
    Risk,
    /// Fork + relay simulation.
    Simulation,
    /// Pending observed → bundle recorded. This is the number that has to
    /// clear [`BUDGET_MS`].
    Total,
}

impl Stage {
    pub fn as_str(&self) -> &'static str {
        match self {
            Stage::IngestToStrategy => "ingest_to_strategy",
            Stage::Strategy => "strategy",
            Stage::Risk => "risk",
            Stage::Simulation => "simulation",
            Stage::Total => "total",
        }
    }

    pub fn all() -> [Stage; 5] {
        [
            Stage::IngestToStrategy,
            Stage::Strategy,
            Stage::Risk,
            Stage::Simulation,
            Stage::Total,
        ]
    }
}

/// A lock-free-ish count of observations plus a mutex-guarded bucket array.
///
/// Writes are the hot path (every pending tx, every sim) so the buckets sit
/// behind a short critical section; reads (the dashboard poll) take the same
/// lock for a consistent snapshot.
#[derive(Default)]
pub struct Histogram {
    inner: Mutex<Inner>,
    count: AtomicU64,
}

#[derive(Clone)]
struct Inner {
    buckets: [u64; 15], // BOUNDS_MS.len() + 1 overflow bucket
    sum_ms: u128,
    min_ms: u64,
    max_ms: u64,
}

impl Default for Inner {
    fn default() -> Self {
        Self {
            buckets: [0; 15],
            sum_ms: 0,
            min_ms: u64::MAX,
            max_ms: 0,
        }
    }
}

impl Histogram {
    pub fn observe(&self, ms: u64) {
        let mut g = self.inner.lock();
        let idx = bucket_index(ms);
        g.buckets[idx] += 1;
        g.sum_ms += ms as u128;
        if ms < g.min_ms {
            g.min_ms = ms;
        }
        if ms > g.max_ms {
            g.max_ms = ms;
        }
        self.count.fetch_add(1, Ordering::Relaxed);
    }

    pub fn count(&self) -> u64 {
        self.count.load(Ordering::Relaxed)
    }

    pub fn snapshot(&self) -> Value {
        let g = self.inner.lock();
        let n = self.count.load(Ordering::Relaxed);
        let mean = if n == 0 {
            0.0
        } else {
            g.sum_ms as f64 / n as f64
        };
        json!({
            "count": n,
            "meanMs": mean,
            "minMs": if n == 0 { 0 } else { g.min_ms },
            "maxMs": g.max_ms,
            "p50Ms": percentile(&g.buckets, n, 0.50),
            "p95Ms": percentile(&g.buckets, n, 0.95),
            "p99Ms": percentile(&g.buckets, n, 0.99),
            "buckets": bucket_json(&g.buckets),
        })
    }
}

fn bucket_index(ms: u64) -> usize {
    for (i, bound) in BOUNDS_MS.iter().enumerate() {
        if ms <= *bound {
            return i;
        }
    }
    BOUNDS_MS.len()
}

/// Approximate percentile from the cumulative bucket counts. Returns the
/// *upper bound* of the bucket that contains the percentile, which is the
/// honest reading of a coarse histogram (we never claim more resolution
/// than we recorded).
fn percentile(buckets: &[u64; 15], count: u64, p: f64) -> u64 {
    if count == 0 {
        return 0;
    }
    let target = ((count as f64) * p).ceil().max(1.0) as u64;
    let mut acc = 0u64;
    for (i, c) in buckets.iter().enumerate() {
        acc += *c;
        if acc >= target {
            return if i < BOUNDS_MS.len() {
                BOUNDS_MS[i]
            } else {
                // overflow bucket: report the last bound as a floor
                BOUNDS_MS[BOUNDS_MS.len() - 1]
            };
        }
    }
    BOUNDS_MS[BOUNDS_MS.len() - 1]
}

fn bucket_json(buckets: &[u64; 15]) -> Value {
    let mut out = Vec::with_capacity(15);
    for (i, bound) in BOUNDS_MS.iter().enumerate() {
        out.push(json!({"leMs": bound, "count": buckets[i]}));
    }
    out.push(json!({"leMs": null, "count": buckets[BOUNDS_MS.len()]}));
    json!(out)
}

/// One histogram per [`Stage`].
#[derive(Default)]
pub struct Latency {
    ingest: Histogram,
    strategy: Histogram,
    risk: Histogram,
    simulation: Histogram,
    total: Histogram,
}

impl Latency {
    pub fn observe(&self, stage: Stage, ms: u64) {
        match stage {
            Stage::IngestToStrategy => self.ingest.observe(ms),
            Stage::Strategy => self.strategy.observe(ms),
            Stage::Risk => self.risk.observe(ms),
            Stage::Simulation => self.simulation.observe(ms),
            Stage::Total => self.total.observe(ms),
        }
    }

    pub fn snapshot(&self) -> Value {
        let total = self.total.snapshot();
        let p95 = total.get("p95Ms").and_then(|v| v.as_u64()).unwrap_or(0);
        json!({
            "budgetMs": BUDGET_MS,
            "withinBudget": p95 > 0 && p95 <= BUDGET_MS,
            "stages": {
                Stage::IngestToStrategy.as_str(): self.ingest.snapshot(),
                Stage::Strategy.as_str(): self.strategy.snapshot(),
                Stage::Risk.as_str(): self.risk.snapshot(),
                Stage::Simulation.as_str(): self.simulation.snapshot(),
                Stage::Total.as_str(): total,
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_histogram_is_zeros() {
        let h = Histogram::default();
        let s = h.snapshot();
        assert_eq!(s["count"], 0);
        assert_eq!(s["p50Ms"], 0);
        assert_eq!(s["p95Ms"], 0);
    }

    #[test]
    fn observations_land_in_the_right_bucket() {
        let h = Histogram::default();
        h.observe(1);
        h.observe(150);
        h.observe(151);
        h.observe(10_000);
        let s = h.snapshot();
        assert_eq!(s["count"], 4);
        assert_eq!(s["minMs"], 1);
        assert_eq!(s["maxMs"], 10_000);
        let buckets = s["buckets"].as_array().unwrap();
        // 1 ms → first bucket (le 1)
        assert_eq!(buckets[0]["count"], 1);
        // 150 ms → the budget bucket
        let budget = buckets
            .iter()
            .find(|b| b["leMs"] == 150)
            .unwrap();
        assert_eq!(budget["count"], 1);
        // 151 ms → the 200 ms bucket
        let two_hundred = buckets
            .iter()
            .find(|b| b["leMs"] == 200)
            .unwrap();
        assert_eq!(two_hundred["count"], 1);
        // overflow
        assert_eq!(buckets.last().unwrap()["count"], 1);
    }

    #[test]
    fn percentiles_track_the_mass() {
        let h = Histogram::default();
        // 90 observations at 10 ms, 10 at 200 ms → p50 in the 10 ms bucket,
        // p95 in the 200 ms bucket.
        for _ in 0..90 {
            h.observe(10);
        }
        for _ in 0..10 {
            h.observe(200);
        }
        let s = h.snapshot();
        assert_eq!(s["p50Ms"], 10);
        assert_eq!(s["p95Ms"], 200);
        assert_eq!(s["p99Ms"], 200);
    }

    #[test]
    fn latency_snapshot_flags_the_budget() {
        let l = Latency::default();
        for _ in 0..20 {
            l.observe(Stage::Total, 40);
        }
        let s = l.snapshot();
        assert_eq!(s["budgetMs"], BUDGET_MS);
        assert_eq!(s["withinBudget"], true);
        assert_eq!(s["stages"]["total"]["p95Ms"], 50); // 40 lands in the 50 ms bucket

        let over = Latency::default();
        for _ in 0..20 {
            over.observe(Stage::Total, 400);
        }
        assert_eq!(over.snapshot()["withinBudget"], false);
    }

    #[test]
    fn every_stage_has_a_stable_name() {
        for s in Stage::all() {
            assert!(!s.as_str().is_empty());
        }
    }
}
