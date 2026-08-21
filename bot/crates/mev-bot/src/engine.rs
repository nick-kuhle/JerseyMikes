//! The engine: ingest → strategies → risk → simulation → storage → UI feed.
//!
//! One task owns the event loop. Simulations are spawned onto the runtime so a
//! slow fork call can never stall ingestion, and the risk engine's inflight
//! counters bound how many can be outstanding at once.

use std::sync::Arc;

use alloy_primitives::U256;
use anyhow::Result;
use tokio::sync::broadcast;

use crate::config::Config;
use crate::ingest::{self, Ingest, IngestEvent};
use crate::risk::RiskEngine;
use crate::rpc::RpcClient;
use crate::sim::Simulator;
use crate::signer::Signer;
use crate::store::Store;
use crate::strategies::{
    arb::AtomicArbStrategy, discovery::PoolDiscovery, jit::JitStrategy, liquidation::LiquidationStrategy,
    sandwich::SandwichStrategy, sniper::SniperStrategy, StrategyCtx, StrategyImpl,
};
use crate::types::{now_ms, BlockHead, FeedEvent, Opportunity, PendingTx, Strategy, TxSource};

pub struct Engine {
    pub cfg: Arc<Config>,
    pub store: Arc<Store>,
    pub risk: Arc<RiskEngine>,
    pub sim: Arc<Simulator>,
    pub ctx: Arc<StrategyCtx>,
    pub feed: broadcast::Sender<FeedEvent>,
    pub stats: Arc<Stats>,
    strategies: Vec<Arc<dyn StrategyImpl>>,
    pool_discovery: PoolDiscovery,
    http: RpcClient,
}

#[derive(Default)]
pub struct Stats {
    pub pending_seen: std::sync::atomic::AtomicU64,
    pub hints_seen: std::sync::atomic::AtomicU64,
    pub blocks_seen: std::sync::atomic::AtomicU64,
    pub opportunities: std::sync::atomic::AtomicU64,
    pub simulations: std::sync::atomic::AtomicU64,
    pub submittable: std::sync::atomic::AtomicU64,
    pub rejected: std::sync::atomic::AtomicU64,
    pub started_at_ms: std::sync::atomic::AtomicU64,
    /// Per-strategy funnel: how many candidates each strategy *saw*, how
    /// many it *emitted* (i.e. built an `Opportunity` for), how many were
    /// *gated by risk*, and how many *simulated successfully*. The point
    /// of this counter is to make the funnel visible: if the bot is
    /// seeing opportunities but not submitting any, the question "where
    /// did they die?" gets an immediate answer.
    pub funnel: parking_lot::RwLock<std::collections::HashMap<Strategy, FunnelCounters>>,
}

/// Per-strategy funnel counters.
///
/// **Two different units live in this struct and mixing them is a
/// mistake that has already been made once.** The `invocations_*`
/// fields count *strategy calls*; everything else counts *individual
/// opportunities*. A conversion rate is only meaningful between fields
/// of the same unit — `candidates_emitted` → `gated_by_risk` →
/// `simulations_*` → `submittable` is the per-opportunity funnel, and
/// `invocations_with_output` / `invocations_empty` is the separate
/// "how often does this strategy fire at all" signal.
#[derive(Clone, Copy, Debug, Default)]
pub struct FunnelCounters {
    /// **Unit: strategy calls.** `on_pending` / `on_block` calls that
    /// produced at least one `Opportunity`. This is the strategy's
    /// "fires at all" rate, not its output volume.
    pub invocations_with_output: u64,
    /// **Unit: strategy calls.** `on_pending` / `on_block` calls that
    /// returned zero `Opportunity`s — typically min-notional filters,
    /// victim-revert pre-checks, or pool-cache misses.
    pub invocations_empty: u64,
    /// **Unit: opportunities.** Total `Opportunity`s emitted, i.e. the
    /// sum of `opps.len()` over every call. This is the number that
    /// moves when a strategy widens its search (multi-leg arb, V3
    /// victims), and the one every downstream counter is comparable to.
    pub candidates_emitted: u64,
    /// `Opportunity`s rejected by `RiskEngine::check`. The reject
    /// reason is recorded elsewhere; this counter is just the count.
    pub gated_by_risk: u64,
    /// `Opportunity`s rejected because the victim's raw signed
    /// transaction could not be fetched (so the simulation cannot
    /// replay the victim's calldata faithfully).
    pub missing_victim_raw: u64,
    /// Simulations that returned `success = true`. Subset of
    /// `simulations`; useful for the "revert rate" calculation.
    pub simulations_succeeded: u64,
    /// Simulations that returned `success = false` (revert, gas
    /// overshoot, or zero net output).
    pub simulations_reverted: u64,
    /// Simulations that the `Simulator` itself failed to run (RPC
    /// timeout, anvil fork died, etc.). Distinct from
    /// `simulations_reverted` because the failure happened before
    /// any state was executed.
    pub simulations_failed: u64,
    /// `submittable` opportunities: the simulations cleared the
    /// net-profit and gas-cap gates.
    pub submittable: u64,
}

impl Stats {
    fn bump(counter: &std::sync::atomic::AtomicU64) {
        counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// Bump a per-strategy funnel counter. This is the only path
    /// through which funnel counters should be incremented, so the
    /// funnel-stats update is in one place.
    pub fn record_funnel(&self, strategy: Strategy, f: impl FnOnce(&mut FunnelCounters)) {
        let mut guard = self.funnel.write();
        let entry = guard.entry(strategy).or_default();
        f(entry);
    }

    /// Record one strategy invocation together with the number of
    /// opportunities it produced.
    ///
    /// This is the only correct way to bump the first funnel stage: it keeps
    /// the two units consistent (exactly one invocation counter per call,
    /// plus `produced` candidates). Bumping `candidates_emitted` by one per
    /// call — as this code did before — makes a block that yields 30
    /// candidates indistinguishable from one that yields a single candidate,
    /// which is precisely the measurement multi-leg arb and V3 sandwiching
    /// are judged by.
    pub fn record_invocation(&self, strategy: Strategy, produced: usize) {
        self.record_funnel(strategy, |f| {
            if produced == 0 {
                f.invocations_empty += 1;
            } else {
                f.invocations_with_output += 1;
                f.candidates_emitted += produced as u64;
            }
        });
    }

    pub fn snapshot(&self) -> serde_json::Value {
        use std::sync::atomic::Ordering::Relaxed;
        let funnel = self
            .funnel
            .read()
            .iter()
            .map(|(k, v)| {
                (
                    k.as_str().to_string(),
                    serde_json::json!({
                        "invocationsWithOutput": v.invocations_with_output,
                        "invocationsEmpty": v.invocations_empty,
                        "candidatesEmitted": v.candidates_emitted,
                        "gatedByRisk": v.gated_by_risk,
                        "missingVictimRaw": v.missing_victim_raw,
                        "simulationsSucceeded": v.simulations_succeeded,
                        "simulationsReverted": v.simulations_reverted,
                        "simulationsFailed": v.simulations_failed,
                        "submittable": v.submittable,
                    }),
                )
            })
            .collect::<serde_json::Map<_, _>>();
        serde_json::json!({
            "pendingSeen": self.pending_seen.load(Relaxed),
            "hintsSeen": self.hints_seen.load(Relaxed),
            "blocksSeen": self.blocks_seen.load(Relaxed),
            "opportunities": self.opportunities.load(Relaxed),
            "simulations": self.simulations.load(Relaxed),
            "submittable": self.submittable.load(Relaxed),
            "rejected": self.rejected.load(Relaxed),
            "startedAtMs": self.started_at_ms.load(Relaxed),
            "funnel": funnel,
        })
    }
}

impl Engine {
    pub async fn new(cfg: Arc<Config>) -> Result<Self> {
        let http = RpcClient::new(cfg.endpoints.http_url.clone())?;
        let store = Arc::new(Store::open(&cfg.api.db_path)?);
        let risk = Arc::new(RiskEngine::new(cfg.clone()));

        let signer = Arc::new(match &cfg.endpoints.flashbots_signer_key {
            Some(k) => Signer::from_hex(k)?,
            None => {
                tracing::warn!(
                    target: "engine",
                    "no FLASHBOTS_SIGNER_KEY set — using an ephemeral key (relay cross-checks may be rate limited)"
                );
                Signer::ephemeral()
            }
        });

        // Current head, needed before anything else can be sized.
        let head = fetch_head(&http).await?;
        tracing::info!(target: "engine", block = head.number, "synced to head");

        // Local fork simulator. Absence is not fatal: the bot still observes and
        // records, it just cannot score opportunities.
        let fork = match crate::sim::anvil::AnvilSim::spawn(cfg.clone(), head.number).await {
            Ok(f) => Some(Arc::new(f)),
            Err(e) => {
                tracing::error!(target: "engine", error = %e, "anvil fork unavailable — simulations disabled");
                None
            }
        };
        let relay = if cfg.sim.use_call_bundle {
            crate::sim::relay::RelaySim::new(&cfg, signer.clone()).ok()
        } else {
            None
        };

        let executor = fork
            .as_ref()
            .map(|f| f.executor())
            .or(cfg.endpoints.executor)
            .unwrap_or(crate::sim::anvil::SIM_EXECUTOR);

        let sim = Arc::new(Simulator::new(cfg.clone(), fork, relay, signer));
        let ctx = Arc::new(StrategyCtx::new(cfg.clone(), http.clone(), executor, head));

        let mut strategies: Vec<Arc<dyn StrategyImpl>> = Vec::new();
        if cfg.strategies.sandwich {
            strategies.push(Arc::new(SandwichStrategy));
        }
        if cfg.strategies.jit {
            strategies.push(Arc::new(JitStrategy));
        }
        if cfg.strategies.atomic_arb {
            strategies.push(Arc::new(AtomicArbStrategy));
        }
        if cfg.strategies.liquidation {
            strategies.push(Arc::new(LiquidationStrategy::new()));
        }
        if cfg.strategies.sniper {
            strategies.push(Arc::new(SniperStrategy::new()));
        }

        let (feed, _) = broadcast::channel(cfg.api.feed_capacity.max(64));
        let stats = Arc::new(Stats::default());
        stats
            .started_at_ms
            .store(now_ms(), std::sync::atomic::Ordering::Relaxed);

        let pool_discovery = PoolDiscovery::new();

        Ok(Self {
            cfg,
            store,
            risk,
            sim,
            ctx,
            feed,
            stats,
            strategies,
            pool_discovery,
            http,
        })
    }

    /// Run forever.
    pub async fn run(self: Arc<Self>) -> Result<()> {
        let mut ingest = Ingest::start(self.cfg.clone());
        tracing::info!(target: "engine", "ingest started: {}", self.cfg.summary());

        while let Some(ev) = ingest.rx.recv().await {
            match ev {
                IngestEvent::Block(head) => self.clone().on_block(head).await,
                IngestEvent::Pending(tx) => self.clone().on_pending(tx).await,
                IngestEvent::Hint {
                    hash,
                    to,
                    function_selectors,
                    logs,
                    raw,
                } => {
                    Stats::bump(&self.stats.hints_seen);
                    let _ = self.feed.send(FeedEvent::MevShareHint {
                        hash,
                        logs,
                        functions: function_selectors.clone(),
                        seen_at_ms: now_ms(),
                    });
                    // MEV-Share hints sometimes carry enough calldata to act on.
                    if let Some(tx) = hint_to_pending(&raw, hash, to) {
                        self.clone().on_pending(tx).await;
                    }
                }
                IngestEvent::RelayBid {
                    relay,
                    slot,
                    builder,
                    value_wei,
                } => {
                    let _ = self.store.record_relay_bid(&relay, slot, &builder, value_wei);
                    let _ = self.feed.send(FeedEvent::Relay {
                        relay,
                        slot,
                        builder,
                        value_wei,
                        seen_at_ms: now_ms(),
                    });
                }
            }
        }
        Ok(())
    }

    async fn on_block(self: Arc<Self>, head: BlockHead) {
        Stats::bump(&self.stats.blocks_seen);
        self.ctx.set_head(head.clone());
        let _ = self.store.record_block(&head);
        let _ = self.feed.send(FeedEvent::Block(head.clone()));

        if self.cfg.pool_discovery || self.cfg.pool_discovery_v3 {
            let discovery = &self.pool_discovery;
            let ctx = self.ctx.clone();
            let head = head.clone();
            // Discovery is a single eth_getLogs + a few pool loads; run it on the
            // block task before strategies spawn. It feeds the cache that all
            // strategies read on their next tick.
            let _new_pools = discovery.discover(&ctx, &head).await;
        }

        for s in &self.strategies {
            let strat = s.clone();
            let this = self.clone();
            let head = head.clone();
            let kind = s.kind();
            tokio::spawn(async move {
                let opps = strat.on_block(&this.ctx, &head).await;
                this.stats.record_invocation(kind, opps.len());
                for opp in opps {
                    this.clone().consider(opp, Vec::new(), None).await;
                }
            });
        }
    }

    async fn on_pending(self: Arc<Self>, tx: PendingTx) {
        Stats::bump(&self.stats.pending_seen);
        let _ = self.feed.send(FeedEvent::Pending {
            hash: tx.hash,
            from: tx.from,
            to: tx.to,
            value: tx.value,
            gas: tx.gas,
            source: tx.source,
            selector: tx.selector().map(|s| format!("0x{}", hex::encode(s))),
            seen_at_ms: tx.seen_at_ms,
        });

        for s in &self.strategies {
            let strat = s.clone();
            let this = self.clone();
            let tx = tx.clone();
            let kind = s.kind();
            tokio::spawn(async move {
                let opps = strat.on_pending(&this.ctx, &tx).await;
                this.stats.record_invocation(kind, opps.len());
                if opps.is_empty() {
                    return;
                }
                // The victim must be replayable inside the fork, which needs the
                // raw signed bytes.
                let raw = match &tx.raw {
                    Some(r) => Some(r.clone()),
                    None => ingest::fetch_raw_tx(&this.http, tx.hash).await,
                };
                let victims = raw.map(|r| vec![r]).unwrap_or_default();
                for opp in opps {
                    this.clone()
                        .consider(opp, victims.clone(), tx.from.map(|from| (from, tx.nonce)))
                        .await;
                }
            });
        }
    }

    /// Risk-gate, simulate, record.
    async fn consider(
        self: Arc<Self>,
        opp: Opportunity,
        victims_raw: Vec<Vec<u8>>,
        victim_sender_nonce: Option<(alloy_primitives::Address, u64)>,
    ) {
        let head = self.ctx.head();
        let kind = opp.strategy;
        if let Err(reject) = self.risk.check(&opp, head.base_fee_per_gas) {
            Stats::bump(&self.stats.rejected);
            self.stats
                .record_funnel(kind, |f| f.gated_by_risk += 1);
            tracing::debug!(target: "engine", strategy = opp.strategy.as_str(), reason = reject.as_str(), "rejected");
            return;
        }
        // A sandwich or JIT without the victim's bytes cannot be simulated faithfully.
        if !opp.victim_hashes.is_empty() && victims_raw.is_empty() {
            self.stats
                .record_funnel(kind, |f| f.missing_victim_raw += 1);
            tracing::debug!(target: "engine", "skipping: victim raw transaction unavailable");
            return;
        }

        Stats::bump(&self.stats.opportunities);
        let _ = self.store.record_opportunity(&opp);
        let _ = self.feed.send(FeedEvent::Opportunity(opp.clone()));

        self.risk.begin(opp.strategy);
        let outcome = self
            .sim
            .run(
                &opp,
                &victims_raw,
                victim_sender_nonce,
                head.base_fee_per_gas,
                0,
            )
            .await;
        self.risk.end(opp.strategy);

        let outcome = match outcome {
            Ok(o) => o,
            Err(e) => {
                tracing::debug!(target: "engine", error = %e, "simulation failed");
                self.stats
                    .record_funnel(kind, |f| f.simulations_failed += 1);
                return;
            }
        };

        Stats::bump(&self.stats.simulations);
        self.risk.observe(&outcome.primary);
        let _ = self.store.record_simulation(&outcome.primary);
        let _ = self.feed.send(FeedEvent::Simulation(outcome.primary.clone()));
        if let Some(relay) = &outcome.relay {
            let _ = self.store.record_simulation(relay);
            let _ = self.feed.send(FeedEvent::Simulation(relay.clone()));
        }

        if outcome.primary.success {
            self.stats
                .record_funnel(kind, |f| f.simulations_succeeded += 1);
        } else {
            self.stats
                .record_funnel(kind, |f| f.simulations_reverted += 1);
        }

        let mut bundle = outcome.bundle;
        if self.risk.submittable(&outcome.primary) {
            Stats::bump(&self.stats.submittable);
            self.stats
                .record_funnel(kind, |f| f.submittable += 1);
            tracing::info!(
                target: "engine",
                strategy = opp.strategy.as_str(),
                net_wei = outcome.primary.net_profit_wei,
                gas = outcome.primary.gas_used,
                block = opp.target_block,
                "PROFITABLE bundle (simulation only — not submitted)"
            );
            // `submitted` stays false: this build never broadcasts. Flipping it
            // requires LIVE_EXECUTION=true *and* I_UNDERSTAND_LIVE_RISK=yes, which
            // `Config::from_env` refuses to set together with the default profile.
            bundle.submitted = self.cfg.live_execution;
        }
        let _ = self.store.record_bundle(&bundle);
        let _ = self.feed.send(FeedEvent::Bundle(bundle));
    }
}

async fn fetch_head(http: &RpcClient) -> Result<BlockHead> {
    let v = http
        .call_raw("eth_getBlockByNumber", serde_json::json!(["latest", false]))
        .await?;
    Ok(BlockHead {
        number: crate::types::parse_u64(&v["number"]),
        hash: crate::types::parse_b256(&v["hash"]).unwrap_or_default(),
        parent_hash: crate::types::parse_b256(&v["parentHash"]).unwrap_or_default(),
        timestamp: crate::types::parse_u64(&v["timestamp"]),
        base_fee_per_gas: crate::types::parse_u256(&v["baseFeePerGas"]),
        gas_used: crate::types::parse_u64(&v["gasUsed"]),
        gas_limit: crate::types::parse_u64(&v["gasLimit"]),
    })
}

/// MEV-Share hints occasionally include full calldata; when they do we can treat
/// them exactly like a mempool transaction.
fn hint_to_pending(raw: &serde_json::Value, hash: alloy_primitives::B256, to: Option<alloy_primitives::Address>) -> Option<PendingTx> {
    let txs = raw.get("txs")?.as_array()?;
    let first = txs.first()?;
    let calldata = first.get("callData").map(crate::types::parse_bytes)?;
    if calldata.len() < 4 {
        return None;
    }
    Some(PendingTx {
        hash,
        from: None,
        to: to.or_else(|| first.get("to").and_then(crate::types::parse_address)),
        value: U256::ZERO,
        gas: 300_000,
        max_fee_per_gas: U256::ZERO,
        max_priority_fee_per_gas: U256::ZERO,
        nonce: 0,
        input: calldata,
        raw: None,
        source: TxSource::MevShare,
        seen_at_ms: now_ms(),
    })
}

/// Strategies enabled at runtime, for the status endpoint.
pub fn enabled_strategies(cfg: &Config) -> Vec<&'static str> {
    Strategy::all()
        .iter()
        .filter(|s| match s {
            Strategy::Sandwich => cfg.strategies.sandwich,
            Strategy::Jit => cfg.strategies.jit,
            Strategy::AtomicArb => cfg.strategies.atomic_arb,
            Strategy::Liquidation => cfg.strategies.liquidation,
            Strategy::Sniper => cfg.strategies.sniper,
        })
        .map(|s| s.as_str())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn funnel_starts_empty() {
        let s = Stats::default();
        let snap = s.snapshot();
        // The funnel map is empty until a strategy has had at least one
        // record_funnel call; the snapshot must serialise without panicking
        // even when no strategies have been observed.
        let funnel = snap.get("funnel").expect("funnel key present");
        assert!(funnel.as_object().unwrap().is_empty());
    }

    #[test]
    fn record_funnel_bumps_the_right_strategy() {
        let s = Stats::default();
        s.record_funnel(Strategy::Sandwich, |f| f.candidates_emitted += 3);
        s.record_funnel(Strategy::Sandwich, |f| f.gated_by_risk += 1);
        s.record_funnel(Strategy::AtomicArb, |f| f.candidates_emitted += 7);
        s.record_funnel(Strategy::AtomicArb, |f| f.simulations_succeeded += 2);
        let snap = s.snapshot();
        let funnel = snap.get("funnel").unwrap().as_object().unwrap();
        let sandwich = funnel.get("sandwich").unwrap();
        assert_eq!(sandwich["candidatesEmitted"], 3);
        assert_eq!(sandwich["gatedByRisk"], 1);
        let arb = funnel.get("atomic_arb").unwrap();
        assert_eq!(arb["candidatesEmitted"], 7);
        assert_eq!(arb["simulationsSucceeded"], 2);
    }

    #[test]
    fn funnel_counters_default_to_zero() {
        // Strategy never recorded: entry is created on first record_funnel
        // with all fields at zero. Important: a strategy that has been
        // recorded but for which no fields have been bumped should still
        // appear in the snapshot with zeros, so the dashboard always has
        // a row for every enabled strategy.
        let s = Stats::default();
        s.record_funnel(Strategy::Jit, |_| {});
        let snap = s.snapshot();
        let jit = snap["funnel"]["jit"].clone();
        assert_eq!(jit["candidatesEmitted"], 0);
        assert_eq!(jit["invocationsWithOutput"], 0);
        assert_eq!(jit["invocationsEmpty"], 0);
        assert_eq!(jit["gatedByRisk"], 0);
        assert_eq!(jit["submittable"], 0);
    }

    #[test]
    fn invocation_with_three_opportunities_counts_three_candidates() {
        // The defect this replaces: one call returning three opportunities
        // used to bump candidatesEmitted by one, making it impossible to
        // see search-width changes (multi-leg arb, V3 victims) in the funnel.
        let s = Stats::default();
        s.record_invocation(Strategy::AtomicArb, 3);
        let snap = s.snapshot();
        let arb = &snap["funnel"]["atomic_arb"];
        assert_eq!(arb["candidatesEmitted"], 3);
        assert_eq!(arb["invocationsWithOutput"], 1);
        assert_eq!(arb["invocationsEmpty"], 0);
    }

    #[test]
    fn empty_invocation_counts_only_as_an_empty_call() {
        let s = Stats::default();
        s.record_invocation(Strategy::Sandwich, 0);
        let snap = s.snapshot();
        let sw = &snap["funnel"]["sandwich"];
        assert_eq!(sw["candidatesEmitted"], 0);
        assert_eq!(sw["invocationsEmpty"], 1);
        assert_eq!(sw["invocationsWithOutput"], 0);
    }

    #[test]
    fn candidates_never_fall_below_invocations_with_output() {
        // Invariant the dashboard relies on: every invocation that produced
        // output contributed at least one candidate, so candidatesEmitted is
        // always >= invocationsWithOutput. Mixed traffic must preserve it.
        let s = Stats::default();
        for n in [0usize, 1, 0, 5, 2, 0] {
            s.record_invocation(Strategy::Jit, n);
        }
        let snap = s.snapshot();
        let jit = &snap["funnel"]["jit"];
        assert_eq!(jit["candidatesEmitted"], 8); // 1 + 5 + 2
        assert_eq!(jit["invocationsWithOutput"], 3);
        assert_eq!(jit["invocationsEmpty"], 3);
        assert!(
            jit["candidatesEmitted"].as_u64().unwrap()
                >= jit["invocationsWithOutput"].as_u64().unwrap()
        );
    }

    #[test]
    fn global_counters_are_independent_of_funnel() {
        // The pre-existing pending/blocks/opportunities counters must
        // continue to work after the funnel refactor. This is a regression
        // guard.
        let s = Stats::default();
        Stats::bump(&s.pending_seen);
        Stats::bump(&s.pending_seen);
        Stats::bump(&s.blocks_seen);
        let snap = s.snapshot();
        assert_eq!(snap["pendingSeen"], 2);
        assert_eq!(snap["blocksSeen"], 1);
        // Funnel untouched.
        assert!(snap["funnel"].as_object().unwrap().is_empty());
    }
}
