//! Alerting: rule evaluation over the engine's own state, on an interval.
//!
//! The roadmap's Phase 3 alerting item names four failure modes — kill-switch
//! trips, endpoint failures, inclusion-rate collapse, plus the operational
//! stalls around them. This module evaluates them from live engine state
//! every `ALERT_EVAL_SECS` (default 30s):
//!
//! | rule | condition | severity |
//! | --- | --- | --- |
//! | `kill_switch` | drawdown kill switch tripped | critical |
//! | `drawdown_approaching` | cumulative net below −50% of the drawdown limit | warning |
//! | `head_stalled` | no new head for `ALERT_HEAD_STALL_SECS` (RPC/WS endpoint or node dead) | critical |
//! | `pending_stalled` | no mempool tx for `ALERT_PENDING_STALL_SECS` while a WS feed is configured | warning |
//! | `no_mempool_feed` | `ETH_WS_URL` unset — the pending path is dark by configuration | info |
//! | `conversion_collapsed` | a strategy with ≥ `ALERT_MIN_CANDIDATES` live candidates converts < `ALERT_MIN_CONVERSION_PCT` | warning |
//! | `reorg_observed` | a re-org was seen since the last evaluation | warning |
//!
//! Alerts have lifecycle: a rule that fires becomes *active* until its
//! condition clears, then *resolved* (history kept in a ring buffer). Active
//! set + recent history are served on `GET /api/alerts`, transitions go to
//! the SSE feed (`FeedEvent::Alert`) and the log, and — when
//! `ALERT_WEBHOOK_URL` is set — new/transitioned alerts are POSTed as JSON
//! (Slack/Discord-compatible shape) for off-box delivery.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::RwLock;
use serde::Serialize;
use serde_json::json;

use crate::config::AlertsConfig;
use crate::types::now_ms;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Info,
    Warning,
    Critical,
}

#[derive(Clone, Debug, Serialize)]
pub struct Alert {
    pub rule: &'static str,
    pub severity: Severity,
    pub message: String,
    /// Millis since epoch of the last transition (fired or resolved).
    pub at_ms: u64,
    pub active: bool,
}

/// The signals one evaluation pass needs — gathered by the engine, evaluated
/// purely (and unit-tested) here.
#[derive(Clone, Debug, Default)]
pub struct AlertSignals {
    pub now_ms: u64,
    pub last_head_ms: u64,
    pub last_pending_ms: u64,
    pub mempool_feed_configured: bool,
    pub kill_switch_tripped: bool,
    /// (limit_wei, cumulative_net_wei) — None disables the drawdown rules.
    pub drawdown: Option<(u128, i128)>,
    /// Live-lane conversion per strategy: (candidates, submittable).
    pub conversion: Vec<(&'static str, u64, u64)>,
    pub reorgs_since_last_eval: u64,
}

/// A rule verdict for one pass.
#[derive(Clone, Debug, PartialEq)]
pub struct Condition {
    pub rule: &'static str,
    pub severity: Severity,
    pub message: String,
}

/// Evaluate the rules against one signals snapshot. Pure.
pub fn evaluate_signals(sig: &AlertSignals, cfg: &AlertsConfig) -> Vec<Condition> {
    let mut out = Vec::new();
    if sig.kill_switch_tripped {
        out.push(Condition {
            rule: "kill_switch",
            severity: Severity::Critical,
            message: "drawdown kill switch tripped — no new opportunities are being taken".into(),
        });
    } else if let Some((limit, cum)) = sig.drawdown {
        if limit > 0 && cum < 0 {
            let used = (-(cum as f64) / limit as f64) * 100.0;
            if used >= 50.0 {
                out.push(Condition {
                    rule: "drawdown_approaching",
                    severity: Severity::Warning,
                    message: format!(
                        "cumulative net {cum} wei is {used:.0}% of the {limit} wei drawdown limit"
                    ),
                });
            }
        }
    }
    if sig.last_head_ms > 0
        && sig.now_ms.saturating_sub(sig.last_head_ms) > cfg.head_stall_secs * 1_000
    {
        let stalled = (sig.now_ms - sig.last_head_ms) / 1_000;
        out.push(Condition {
            rule: "head_stalled",
            severity: Severity::Critical,
            message: format!("no new head for {stalled}s — RPC endpoint or node down?"),
        });
    }
    if !sig.mempool_feed_configured {
        out.push(Condition {
            rule: "no_mempool_feed",
            severity: Severity::Info,
            message:
                "ETH_WS_URL is unset — sandwich/JIT/arb-backrun/oracle_frontrun see no mempool"
                    .into(),
        });
    } else if sig.last_pending_ms > 0
        && sig.now_ms.saturating_sub(sig.last_pending_ms) > cfg.pending_stall_secs * 1_000
    {
        let stalled = (sig.now_ms - sig.last_pending_ms) / 1_000;
        out.push(Condition {
            rule: "pending_stalled",
            severity: Severity::Warning,
            message: format!("no mempool transaction for {stalled}s while a WS feed is configured"),
        });
    }
    for (strategy, candidates, submittable) in &sig.conversion {
        if *candidates >= cfg.min_candidates && cfg.min_conversion_pct > 0.0 {
            let pct = (*submittable as f64 / *candidates as f64) * 100.0;
            if pct < cfg.min_conversion_pct {
                out.push(Condition {
                    rule: "conversion_collapsed",
                    severity: Severity::Warning,
                    message: format!(
                        "{strategy}: {submittable}/{} candidates submittable ({pct:.2}%) — inclusion-rate collapse signal",
                        candidates
                    ),
                });
            }
        }
    }
    if sig.reorgs_since_last_eval > 0 {
        out.push(Condition {
            rule: "reorg_observed",
            severity: Severity::Warning,
            message: format!(
                "{} re-org(s) since the last evaluation",
                sig.reorgs_since_last_eval
            ),
        });
    }
    out
}

/// Runtime state: active conditions + transition history + the clocks the
/// engine ticks on every head/pending observation.
pub struct Alerts {
    cfg: AlertsConfig,
    inner: RwLock<Vec<Alert>>,
    history: RwLock<VecDeque<Alert>>,
    last_head_ms: AtomicU64,
    last_pending_ms: AtomicU64,
    last_reorgs: AtomicU64,
}

const HISTORY_CAP: usize = 200;

impl Alerts {
    pub fn new(cfg: AlertsConfig) -> Self {
        Self {
            cfg,
            inner: RwLock::new(Vec::new()),
            history: RwLock::new(VecDeque::with_capacity(HISTORY_CAP)),
            last_head_ms: AtomicU64::new(0),
            last_pending_ms: AtomicU64::new(0),
            last_reorgs: AtomicU64::new(0),
        }
    }

    pub fn config(&self) -> &AlertsConfig {
        &self.cfg
    }

    pub fn observe_head(&self) {
        self.last_head_ms.store(now_ms(), Ordering::Relaxed);
    }

    pub fn observe_pending(&self) {
        self.last_pending_ms.store(now_ms(), Ordering::Relaxed);
    }

    pub fn observe_reorg(&self) {
        self.last_reorgs.fetch_add(1, Ordering::Relaxed);
    }

    /// One evaluation pass: diff the incoming conditions against the active
    /// set, record transitions (and log them), and return the events to
    /// broadcast. Webhook delivery is best-effort fire-and-forget.
    pub fn evaluate(&self, sig: &AlertSignals) -> Vec<Alert> {
        let conditions = evaluate_signals(sig, &self.cfg);
        let now = now_ms();
        let mut events = Vec::new();
        {
            let mut active = self.inner.write();
            // Fire new conditions.
            for c in &conditions {
                if !active.iter().any(|a| a.rule == c.rule) {
                    let alert = Alert {
                        rule: c.rule,
                        severity: c.severity,
                        message: c.message.clone(),
                        at_ms: now,
                        active: true,
                    };
                    tracing::warn!(target: "alerts", rule = c.rule, severity = ?c.severity, "{}", c.message);
                    active.push(alert.clone());
                    events.push(alert);
                } else if let Some(a) = active.iter_mut().find(|a| a.rule == c.rule) {
                    // Keep the freshest wording for the panel.
                    a.message = c.message.clone();
                }
            }
            // Resolve conditions that cleared.
            let drained = active.drain(..).collect::<Vec<_>>();
            *active = drained
                .iter()
                .filter(|a| conditions.iter().any(|c| c.rule == a.rule))
                .cloned()
                .collect();
            for a in drained {
                if conditions.iter().any(|c| c.rule == a.rule) {
                    continue;
                }
                let resolved = Alert {
                    active: false,
                    at_ms: now,
                    ..a
                };
                tracing::info!(target: "alerts", rule = resolved.rule, "resolved");
                events.push(resolved);
            }
        }
        if !events.is_empty() {
            let mut hist = self.history.write();
            for e in &events {
                hist.push_back(e.clone());
                if hist.len() > HISTORY_CAP {
                    hist.pop_front();
                }
            }
            self.deliver_webhook(&events);
        }
        events
    }

    /// Best-effort POST of transitions to `ALERT_WEBHOOK_URL`.
    fn deliver_webhook(&self, events: &[Alert]) {
        let Some(url) = self.cfg.webhook_url.clone() else {
            return;
        };
        let payload = json!({"alerts": events});
        tokio::spawn(async move {
            let client = reqwest::Client::new();
            let _ = client
                .post(&url)
                .timeout(std::time::Duration::from_secs(5))
                .json(&payload)
                .send()
                .await
                .map_err(
                    |e| tracing::debug!(target: "alerts", error = %e, "webhook delivery failed"),
                );
        });
    }

    pub fn active(&self) -> Vec<Alert> {
        self.inner.read().clone()
    }

    pub fn history(&self) -> Vec<Alert> {
        self.history.read().iter().cloned().collect()
    }

    pub fn last_head_ms(&self) -> u64 {
        self.last_head_ms.load(Ordering::Relaxed)
    }

    pub fn last_pending_ms(&self) -> u64 {
        self.last_pending_ms.load(Ordering::Relaxed)
    }

    pub fn take_reorgs(&self) -> u64 {
        self.last_reorgs.swap(0, Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signals() -> AlertSignals {
        AlertSignals {
            now_ms: 10_000_000,
            last_head_ms: 10_000_000,
            last_pending_ms: 10_000_000,
            mempool_feed_configured: true,
            kill_switch_tripped: false,
            drawdown: Some((1_000, 0)),
            conversion: vec![],
            reorgs_since_last_eval: 0,
        }
    }

    fn cfg() -> AlertsConfig {
        AlertsConfig::default()
    }

    fn rules(conds: &[Condition]) -> Vec<&'static str> {
        conds.iter().map(|c| c.rule).collect()
    }

    #[test]
    fn quiet_signals_produce_no_alerts() {
        assert!(evaluate_signals(&signals(), &cfg()).is_empty());
    }

    #[test]
    fn kill_switch_and_drawdown_thresholds() {
        let mut s = signals();
        s.kill_switch_tripped = true;
        let got = evaluate_signals(&s, &cfg());
        assert!(rules(&got).contains(&"kill_switch"));
        assert!(!rules(&got).contains(&"drawdown_approaching"));

        // At 50% of the limit without tripping: the approaching warning.
        let mut s = signals();
        s.drawdown = Some((1_000, -500));
        let got = evaluate_signals(&s, &cfg());
        assert!(rules(&got).contains(&"drawdown_approaching"));

        // A disabled limit (0) warns about nothing.
        let mut s = signals();
        s.drawdown = Some((0, -900_000));
        assert!(!rules(&evaluate_signals(&s, &cfg())).contains(&"drawdown_approaching"));
    }

    #[test]
    fn head_and_pending_stalls_use_configured_windows() {
        let mut s = signals();
        s.last_head_ms = s.now_ms - 61_000;
        assert!(rules(&evaluate_signals(&s, &cfg())).contains(&"head_stalled"));
        s.last_head_ms = s.now_ms - 59_000;
        assert!(!rules(&evaluate_signals(&s, &cfg())).contains(&"head_stalled"));

        let mut s = signals();
        s.last_pending_ms = s.now_ms - 181_000;
        assert!(rules(&evaluate_signals(&s, &cfg())).contains(&"pending_stalled"));

        // No pending observation yet is not a stall (boot).
        let mut s = signals();
        s.last_pending_ms = 0;
        assert!(!rules(&evaluate_signals(&s, &cfg())).contains(&"pending_stalled"));

        // Without a WS feed the stall rule is silent; the info rule fires.
        let mut s = signals();
        s.mempool_feed_configured = false;
        s.last_pending_ms = s.now_ms - 999_000;
        let got = evaluate_signals(&s, &cfg());
        assert!(rules(&got).contains(&"no_mempool_feed"));
        assert!(!rules(&got).contains(&"pending_stalled"));
    }

    #[test]
    fn conversion_needs_minimum_candidates_and_breaks_threshold() {
        let mut s = signals();
        s.conversion = vec![("sandwich", 200, 1)]; // 0.5% < 2%
        assert!(rules(&evaluate_signals(&s, &cfg())).contains(&"conversion_collapsed"));

        s.conversion = vec![("sandwich", 50, 0)]; // under the sample floor
        assert!(!rules(&evaluate_signals(&s, &cfg())).contains(&"conversion_collapsed"));

        s.conversion = vec![("sandwich", 200, 10)]; // 5%
        assert!(!rules(&evaluate_signals(&s, &cfg())).contains(&"conversion_collapsed"));
    }

    #[test]
    fn evaluate_lifecycle_fires_and_resolves() {
        let alerts = Alerts::new(cfg());
        let mut s = signals();
        s.kill_switch_tripped = true;
        let events = alerts.evaluate(&s);
        assert_eq!(events.len(), 1);
        assert!(events[0].active);
        assert_eq!(alerts.active().len(), 1);

        // Condition clears -> one resolved event, empty active set.
        let events = alerts.evaluate(&signals());
        assert_eq!(events.len(), 1);
        assert!(!events[0].active);
        assert!(alerts.active().is_empty());
        assert_eq!(alerts.history().len(), 2);

        // A quiet pass emits nothing and rewrites no history.
        assert!(alerts.evaluate(&signals()).is_empty());
        assert_eq!(alerts.history().len(), 2);
    }
}
