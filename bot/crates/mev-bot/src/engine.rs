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
use crate::inventory::Inventory;
use crate::latency::{Latency, Stage};
use crate::risk::RiskEngine;
use crate::rpc::RpcClient;
use crate::signer::Signer;
use crate::sim::Simulator;
use crate::store::Store;
use crate::strategies::{
    arb::AtomicArbStrategy, discovery::PoolDiscovery, jit::JitStrategy, leads::LiquidationLeads,
    liquidation::LiquidationStrategy, liquidation_compound::CompoundLiquidationStrategy,
    liquidation_maker::MakerLiquidationStrategy, liquidation_morpho::MorphoLiquidationStrategy,
    oracle_frontrun::OracleFrontrunStrategy, sandwich::SandwichStrategy,
    sandwich_v3::SandwichV3Strategy, sniper::SniperStrategy, StrategyCtx, StrategyImpl,
};
use crate::types::{
    now_ms, BlockHead, FeedEvent, Opportunity, PendingTx, RelayTxSummary, Strategy, TxSource,
};

/// Runtime execution-mode switch.
///
/// The boot-time arming (`LIVE_EXECUTION=true` **and**
/// `I_UNDERSTAND_LIVE_RISK=yes`, read once in `Config::from_env`) is what
/// makes live execution possible at all; it cannot be granted after the fact.
/// This type holds the operator's *runtime* decision on top of that arming:
/// an armed bot can be paused back to simulation and resumed from the
/// dashboard (`POST /api/mode`) without a restart, and an unarmed bot
/// refuses the switch with the reason.
///
/// The invariant from `docs/RISK.md` is preserved structurally:
/// `live() == armed && runtime`, so the runtime switch can only ever *narrow*
/// what the environment already allowed — never widen it.
#[derive(Debug)]
pub struct LiveMode {
    /// Arming as of process start (`cfg.live_execution`). Immutable.
    armed: bool,
    /// Operator's runtime choice. Starts as `armed` (the environment already
    /// expresses intent), can be toggled while running.
    runtime: std::sync::atomic::AtomicBool,
}

impl LiveMode {
    pub fn armed_at_boot(armed: bool) -> Self {
        Self {
            armed,
            runtime: std::sync::atomic::AtomicBool::new(armed),
        }
    }

    /// True only when the process was armed for live execution at boot *and*
    /// the runtime switch is on. This is the single decision input for
    /// whether a profitable bundle is marked submitted.
    pub fn live(&self) -> bool {
        self.armed && self.runtime.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Whether the environment armed this process at boot.
    pub fn armed(&self) -> bool {
        self.armed
    }

    /// Flip the runtime switch. Returns the effective mode, or a
    /// human-readable reason when live execution was requested on a process
    /// that was never armed (the two-key switch is boot-time only by design).
    pub fn set_live(&self, on: bool) -> Result<bool, &'static str> {
        if on && !self.armed {
            return Err(
                "this bot was not started with live execution armed; set LIVE_EXECUTION=true and \
                 I_UNDERSTAND_LIVE_RISK=yes in .env and restart — POST /api/mode can never arm a \
                 process after the fact",
            );
        }
        self.runtime.store(on, std::sync::atomic::Ordering::Relaxed);
        Ok(self.live())
    }
}

pub struct Engine {
    pub cfg: Arc<Config>,
    pub store: Arc<Store>,
    /// Batching, off-hot-path front end to `store` for the append-only
    /// telemetry writes (opportunities, simulations, bundles, blocks).
    pub writes: Arc<crate::store::AsyncStore>,
    /// Rule evaluation over live engine state (kill switch, stalls,
    /// conversion collapse); served on /api/alerts, pushed to the feed.
    pub alerts: Arc<crate::alerts::Alerts>,
    /// Runtime risk envelope (shared with `risk` and `sim`); the /api/risk
    /// endpoints read and write this.
    pub runtime: crate::risk::RuntimeRisk,
    pub risk: Arc<RiskEngine>,
    pub sim: Arc<Simulator>,
    pub ctx: Arc<StrategyCtx>,
    pub feed: broadcast::Sender<FeedEvent>,
    pub stats: Arc<Stats>,
    /// Execution mode: boot-time arming + the runtime simulation/live switch
    /// exposed to the dashboard. See [`LiveMode`].
    pub mode: LiveMode,
    /// The directional new-token sniper lane. Deliberately a peer of the
    /// engine rather than a strategy inside it: it holds positions across
    /// blocks, has its own risk envelope, its own arming switch and its own
    /// contract, and none of the atomic path reads it. See `sniper/mod.rs`.
    pub sniper: Arc<crate::sniper::SniperLane>,
    strategies: Vec<Arc<dyn StrategyImpl>>,
    pool_discovery: PoolDiscovery,
    http: RpcClient,
    /// Prevent replay blocks from resetting the shared replay lane concurrently.
    replay_gate: Arc<tokio::sync::Semaphore>,
    /// Bounded hand-off to the dedicated replay worker. `None` when relay
    /// transaction ingest is off.
    replay_tx: Option<ReplayQueueTx>,
    /// Receiver, parked here until `run` starts the worker that owns it.
    replay_rx: parking_lot::Mutex<Option<ReplayQueueRx>>,
    /// Caps how many transactions are inside the strategy fan-out at once.
    /// Acquired with `try_acquire` on the live path, so a mempool burst sheds
    /// load instead of queueing work that would be stale by the time it ran.
    strategy_gate: Arc<tokio::sync::Semaphore>,
    pub latency: Arc<Latency>,
    pub inventory: Arc<Inventory>,
    /// Private relay transport. Calling it is still gated by broadcast
    /// capability, qualification, runtime mode, risk and strategy eligibility.
    pub submission: Arc<crate::submission::SubmissionGateway>,
    /// Qualification is recomputed once per head off the hot path. Candidate
    /// checks and API reads are lock-only and never scan SQLite.
    qualification: Arc<parking_lot::RwLock<crate::qualification::QualificationStatus>>,
    qualification_refreshing: Arc<std::sync::atomic::AtomicBool>,
    qualification_refreshed_at_ms: Arc<std::sync::atomic::AtomicU64>,
    own_reconciliation_running: Arc<std::sync::atomic::AtomicBool>,
    /// Exactly one live candidate may reserve/sign/submit at a time, preventing
    /// nonce gaps when a simulation or relay request fails.
    submission_gate: Arc<tokio::sync::Semaphore>,
    last_head: parking_lot::Mutex<Option<BlockHead>>,
    /// Block number of the last pool-discovery pass (`u64::MAX` = never run).
    last_discovery_block: std::sync::atomic::AtomicU64,
    /// Block number of the last inventory refresh (`u64::MAX` = never run).
    last_inventory_block: std::sync::atomic::AtomicU64,
}

/// A delivered block waiting to be scored: the block and its transactions.
type ReplayJob = (crate::types::RelayBlock, Vec<PendingTx>);
type ReplayQueueTx = tokio::sync::mpsc::Sender<ReplayJob>;
type ReplayQueueRx = tokio::sync::mpsc::Receiver<ReplayJob>;

/// Cooldown gate for per-block maintenance work.
///
/// Returns true when `block` is at least `every` blocks past the last run,
/// and records `block` as the new last run. `every <= 1` always runs.
/// A rewind (re-org to a lower height) also runs: the cached state was built
/// against a chain that no longer exists.
fn should_run(last: &std::sync::atomic::AtomicU64, block: u64, every: u64) -> bool {
    use std::sync::atomic::Ordering::Relaxed;
    if every <= 1 {
        last.store(block, Relaxed);
        return true;
    }
    let prev = last.load(Relaxed);
    // NEVER sentinel: nothing has run yet, so this block is the first pass.
    if prev == NEVER || block < prev || block.saturating_sub(prev) >= every {
        last.store(block, Relaxed);
        return true;
    }
    false
}

/// Sentinel for "this maintenance pass has never run".
const NEVER: u64 = u64::MAX;

/// Token-bucket-ish rate limiter for log lines on hot paths.
///
/// The per-opportunity rejection logs are the highest-frequency `debug!` calls
/// in the engine: every risk gate, inventory gate and missing-victim skip
/// emitted one, which on a busy block is thousands of lines that say the same
/// thing. The *counts* already live in the funnel (`gatedByRisk`,
/// `missingVictimRaw`), so the log line's only job is to show a representative
/// example — which one per interval does just as well, without the formatting
/// cost or the risk of the log becoming the bottleneck.
pub struct LogLimiter {
    last_ms: std::sync::atomic::AtomicU64,
    suppressed: std::sync::atomic::AtomicU64,
    every_ms: u64,
}

impl LogLimiter {
    pub const fn new(every_ms: u64) -> Self {
        Self {
            last_ms: std::sync::atomic::AtomicU64::new(0),
            suppressed: std::sync::atomic::AtomicU64::new(0),
            every_ms,
        }
    }

    /// Returns `Some(suppressed_since_last)` when the caller should log.
    pub fn allow(&self) -> Option<u64> {
        use std::sync::atomic::Ordering::Relaxed;
        let now = now_ms();
        let last = self.last_ms.load(Relaxed);
        if now.saturating_sub(last) < self.every_ms {
            self.suppressed.fetch_add(1, Relaxed);
            return None;
        }
        // Racing threads may both pass the check; the loser just logs twice,
        // which is harmless and cheaper than a lock on this path.
        self.last_ms.store(now, Relaxed);
        Some(self.suppressed.swap(0, Relaxed))
    }
}

/// One limiter per hot-path log site, so a flood of one kind cannot hide
/// the others.
static RISK_REJECT_LOG: LogLimiter = LogLimiter::new(1_000);
static INVENTORY_REJECT_LOG: LogLimiter = LogLimiter::new(1_000);
static MISSING_VICTIM_LOG: LogLimiter = LogLimiter::new(1_000);
static SIM_FAILED_LOG: LogLimiter = LogLimiter::new(1_000);
static SHED_LOG: LogLimiter = LogLimiter::new(1_000);

/// Per-strategy funnel counters, sharded by strategy.
///
/// The funnel is written on every single strategy invocation — the hottest
/// write path in the engine — and read only by the dashboard. A
/// `RwLock<HashMap>` serialised all ten strategies behind one writer lock for
/// updates that never touch the same key; `DashMap` shards the map so those
/// updates are genuinely concurrent.
pub type FunnelMap = dashmap::DashMap<Strategy, FunnelCounters>;

#[derive(Default)]
pub struct Stats {
    pub pending_seen: std::sync::atomic::AtomicU64,
    pub hints_seen: std::sync::atomic::AtomicU64,
    pub blocks_seen: std::sync::atomic::AtomicU64,
    /// Delivered blocks ingested from the bloXroute Max Profit relay.
    pub relay_blocks_seen: std::sync::atomic::AtomicU64,
    /// Transactions inside those delivered blocks, routed through the strategy
    /// funnel for extractable-value scoring.
    pub relay_txs_seen: std::sync::atomic::AtomicU64,
    pub opportunities: std::sync::atomic::AtomicU64,
    pub simulations: std::sync::atomic::AtomicU64,
    pub submittable: std::sync::atomic::AtomicU64,
    pub rejected: std::sync::atomic::AtomicU64,
    /// Transactions dropped before the strategy fan-out because
    /// `STRATEGY_CONCURRENCY` was already saturated. A non-zero value here is
    /// the signal that the bot is CPU/RPC bound and the cap needs raising (or
    /// a strategy needs to get cheaper) — previously this backlog was
    /// invisible, hidden inside an unbounded task queue.
    pub evaluations_shed: std::sync::atomic::AtomicU64,
    pub reorgs_seen: std::sync::atomic::AtomicU64,
    /// Delivered blocks skipped because the replay worker was still busy.
    /// Replay is post-mortem analysis, so dropping the block is preferable to
    /// letting a backlog build behind the live path.
    pub replay_blocks_dropped: std::sync::atomic::AtomicU64,
    pub started_at_ms: std::sync::atomic::AtomicU64,
    /// Per-strategy funnel for **live** flow: how many candidates each
    /// strategy emitted, how many were gated by risk, and how many
    /// simulated successfully. If the bot is seeing opportunities but not
    /// submitting any, the question "where did they die?" gets an
    /// immediate answer here.
    /// Lock-free: every strategy invocation bumps this on the hot path, and a
    /// single `RwLock<HashMap>` made all of them contend on one writer lock
    /// even though they touch disjoint keys. `DashMap` shards by key, so
    /// per-strategy updates proceed in parallel.
    pub funnel: FunnelMap,
    /// The same funnel for **replayed** flow — transactions that were
    /// already mined when the bot scored them (the relay delivered-block
    /// backfill). Kept separate so it cannot drown out the live signal;
    /// see [`FunnelLane`].
    pub funnel_replay: FunnelMap,
}

/// Which measurement lane a funnel observation belongs to.
///
/// The bloXroute delivered-block backfill scores transactions that were
/// **already mined** when the bot saw them. Those observations are valuable —
/// they are the raw material for Phase 1 replay validation — but they are not
/// opportunities the bot could have taken, and a mainnet block delivers ~150
/// of them every 12 seconds. Folded into the same counters as live mempool
/// flow they would dominate it, and every conversion rate in the funnel would
/// stop meaning what `docs/PHASE_2_HANDOFF.md` §0 says it means.
///
/// So the two are counted separately. Same shape, same code path, different
/// ledger.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FunnelLane {
    /// Flow the bot could have acted on: public mempool, MEV-Share hints,
    /// external streams, sequencer feeds, and the block-cadence strategies.
    Live,
    /// Post-mortem scoring of transactions that were already mined when
    /// observed: the relay delivered-block backfill.
    Replay,
}

impl FunnelLane {
    /// Transactions are live unless we know they were already on chain.
    pub fn for_source(source: TxSource) -> Self {
        match source {
            TxSource::RelayDelivered | TxSource::Mined => FunnelLane::Replay,
            TxSource::PublicMempool
            | TxSource::MevShare
            | TxSource::MevBlocker
            | TxSource::Sequencer
            | TxSource::Flashblock
            | TxSource::ExternalStream => FunnelLane::Live,
        }
    }
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

    /// Bump a per-strategy funnel counter in one lane. This is the only
    /// path through which funnel counters should be incremented, so the
    /// funnel-stats update is in one place.
    pub fn record_funnel(
        &self,
        lane: FunnelLane,
        strategy: Strategy,
        f: impl FnOnce(&mut FunnelCounters),
    ) {
        let map = match lane {
            FunnelLane::Live => &self.funnel,
            FunnelLane::Replay => &self.funnel_replay,
        };
        // Only the shard owning `strategy` is locked, so concurrent strategies
        // do not serialise against each other.
        f(&mut map.entry(strategy).or_default());
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
    pub fn record_invocation(&self, lane: FunnelLane, strategy: Strategy, produced: usize) {
        self.record_funnel(lane, strategy, |f| {
            if produced == 0 {
                f.invocations_empty += 1;
            } else {
                f.invocations_with_output += 1;
                f.candidates_emitted += produced as u64;
            }
        });
    }

    /// Serialise one funnel lane as `{strategy: counters}`.
    pub fn funnel_json(map: &FunnelMap) -> serde_json::Map<String, serde_json::Value> {
        map.iter()
            .map(|entry| {
                let (k, v) = (entry.key(), entry.value());
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
            .collect::<serde_json::Map<_, _>>()
    }

    pub fn snapshot(&self) -> serde_json::Value {
        use std::sync::atomic::Ordering::Relaxed;
        let funnel = Self::funnel_json(&self.funnel);
        let funnel_replay = Self::funnel_json(&self.funnel_replay);
        serde_json::json!({
            "pendingSeen": self.pending_seen.load(Relaxed),
            "hintsSeen": self.hints_seen.load(Relaxed),
            "blocksSeen": self.blocks_seen.load(Relaxed),
            "relayBlocksSeen": self.relay_blocks_seen.load(Relaxed),
            "relayTxsSeen": self.relay_txs_seen.load(Relaxed),
            "opportunities": self.opportunities.load(Relaxed),
            "simulations": self.simulations.load(Relaxed),
            "submittable": self.submittable.load(Relaxed),
            "rejected": self.rejected.load(Relaxed),
            "evaluationsShed": self.evaluations_shed.load(Relaxed),
            "replayBlocksDropped": self.replay_blocks_dropped.load(Relaxed),
            "reorgsSeen": self.reorgs_seen.load(Relaxed),
            "startedAtMs": self.started_at_ms.load(Relaxed),
            // Live flow only. Post-mortem scoring of already-mined relay
            // transactions is counted separately so it cannot inflate this.
            "funnel": funnel,
            "funnelReplay": funnel_replay,
        })
    }
}

impl Engine {
    pub async fn new(cfg: Arc<Config>) -> Result<Self> {
        let http = RpcClient::new(cfg.endpoints.http_url.clone())?;
        let store = Arc::new(Store::open(&cfg.api.db_path)?);
        // Hot-path writes go through the batching writer; the store handle
        // stays for reads and for the off-path synchronous writes.
        let writes = crate::store::AsyncStore::spawn(store.clone(), cfg.api.write_queue_capacity);
        let alerts = Arc::new(crate::alerts::Alerts::new(cfg.alerts.clone()));

        // Runtime risk envelope: boot values from the environment, mutable
        // via POST /api/risk (see RuntimeRisk for the narrowing invariants).
        let runtime = crate::risk::RuntimeRisk::new(cfg.risk.clone(), cfg.strategies.clone());
        let risk = {
            let engine = RiskEngine::new(cfg.clone(), runtime.clone()).with_store(store.clone());
            match store.load_risk_state() {
                Ok(state) => engine.restore(&state),
                Err(error) => {
                    // Cannot prove the previous process was untripped: fail
                    // closed. POST /api/risk/reset is the explicit re-arm.
                    tracing::error!(
                        target: "engine",
                        %error,
                        "could not load durable kill-switch state — starting tripped"
                    );
                    engine.restore(&crate::store::PersistedRiskState {
                        tripped: true,
                        tripped_at_ms: None,
                        cumulative_net_wei: 0,
                    });
                }
            }
            Arc::new(engine)
        };

        let relay_signer = Arc::new(match &cfg.endpoints.flashbots_signer_key {
            Some(k) => Signer::from_hex(k)?,
            None => {
                tracing::warn!(
                    target: "engine",
                    "no FLASHBOTS_SIGNER_KEY set — using an ephemeral relay-auth key (cross-checks may be rate limited)"
                );
                Signer::ephemeral()
            }
        });
        let transaction_signer = Arc::new(match &cfg.endpoints.searcher_private_key {
            Some(k) => Signer::from_hex(k)?,
            None => Signer::simulation(),
        });
        // The raw transport (sequencer chains) needs the tx signer, the
        // searcher address and the chain RPC — it signs same-nonce
        // replacement transactions for cancellation. Bundle mode ignores
        // them (relays carry the delivery).
        let raw_mode = cfg.submission_mode == crate::config::SubmissionMode::Raw;
        let submission = Arc::new(crate::submission::SubmissionGateway::new(
            &cfg.endpoints.bundle_relay_urls,
            relay_signer.clone(),
            store.clone(),
            cfg.submission_retry_ms,
            cfg.submission_max_attempts,
            cfg.submission_mode,
            raw_mode.then(|| cfg.endpoints.http_url.clone()),
            raw_mode.then(|| transaction_signer.clone()),
            raw_mode.then_some(cfg.endpoints.searcher_address),
            cfg.chain.chain_id,
            cfg.priority_fee_wei,
            cfg.raw_cancel_bump_bps,
            cfg.raw_cancel_max_fee_wei,
        ));
        if transaction_signer.address() != cfg.endpoints.searcher_address {
            anyhow::bail!(
                "transaction signer {:?} does not match configured searcher {:?}",
                transaction_signer.address(),
                cfg.endpoints.searcher_address
            );
        }

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
        // Second fork, dedicated to replaying delivered blocks. Only worth its
        // memory when there is a backfill to score, and only correct as a
        // separate instance: it pins to historical parents while the live fork
        // tracks the head.
        let replay_fork = if cfg.relay_tx_ingest && cfg.sim.replay_fork {
            match crate::sim::anvil::AnvilSim::spawn_on(
                cfg.clone(),
                head.number,
                cfg.sim.anvil_replay_port,
            )
            .await
            {
                Ok(f) => {
                    tracing::info!(
                        target: "engine",
                        port = cfg.sim.anvil_replay_port,
                        "replay fork ready — delivered blocks scored at their parent state"
                    );
                    Some(Arc::new(f))
                }
                Err(e) => {
                    tracing::warn!(
                        target: "engine",
                        error = %e,
                        "replay fork unavailable — delivered-block scoring will be skipped rather than mis-scored against head state"
                    );
                    None
                }
            }
        } else {
            None
        };

        let relay = if cfg.sim.use_call_bundle {
            crate::sim::relay::RelaySim::new(&cfg, relay_signer.clone()).ok()
        } else {
            None
        };

        let executor = fork
            .as_ref()
            .map(|f| f.executor())
            .or(cfg.endpoints.executor)
            .unwrap_or(crate::sim::anvil::SIM_EXECUTOR);

        let sim = Arc::new(Simulator::new(
            cfg.clone(),
            fork,
            replay_fork,
            relay,
            transaction_signer,
            runtime.clone(),
        ));
        let ctx = Arc::new(StrategyCtx::new(
            cfg.clone(),
            http.clone(),
            executor,
            head.clone(),
        ));
        let pool_discovery = PoolDiscovery::new();
        if cfg.pool_discovery_v3 {
            let loaded = pool_discovery.seed_core_v3(&ctx).await;
            tracing::info!(target: "discovery", loaded, "seeded established core V3 pools");
        }

        // Boot coherence: every profile/strategy/env mismatch, named.
        for warning in cfg.coherence_warnings() {
            tracing::warn!(target: "engine", "{warning}");
        }

        let mut strategies: Vec<Arc<dyn StrategyImpl>> = Vec::new();
        if cfg.strategies.sandwich {
            strategies.push(Arc::new(SandwichStrategy));
        }
        if cfg.strategies.sandwich_v3 {
            if !cfg.pool_discovery_v3 {
                tracing::warn!(
                    target: "engine",
                    "STRATEGY_SANDWICH_V3 is on but POOL_DISCOVERY_V3 is off — the V3 cache will stay empty and the strategy will emit nothing"
                );
            }
            if cfg.addresses.univ3_quoter_v2.is_none()
                || cfg.addresses.univ3_swap_router_02.is_none()
            {
                tracing::warn!(
                    target: "engine",
                    "STRATEGY_SANDWICH_V3 is on but the chain registry has no \
                     QuoterV2 or SwapRouter02 — the strategy will emit nothing"
                );
            }
            strategies.push(Arc::new(SandwichV3Strategy));
        }
        if cfg.strategies.jit {
            strategies.push(Arc::new(JitStrategy));
        }
        if cfg.strategies.atomic_arb {
            strategies.push(Arc::new(AtomicArbStrategy));
        }
        // Shared near-miss registry: the health-polling liquidation
        // strategies publish into it every block, the oracle front-runner
        // reads it when a price update appears in the mempool.
        let leads = LiquidationLeads::new();
        // Protocol availability comes from the chain registry (the coherence
        // warnings above already named any enabled-but-missing protocol).
        let aave_present = cfg.addresses.aave_v3_pool.is_some()
            && cfg.addresses.aave_v3_oracle.is_some()
            && cfg.addresses.aave_v3_data_provider.is_some();
        if cfg.strategies.liquidation && aave_present {
            strategies.push(Arc::new(LiquidationStrategy::new(
                leads.clone(),
                cfg.liquidation.watch_cap,
            )));
        }
        if cfg.strategies.liquidation_compound && cfg.addresses.compound_v3_usdc.is_some() {
            strategies.push(Arc::new(CompoundLiquidationStrategy::new(
                cfg.liquidation.watch_cap,
            )));
        }
        if cfg.strategies.liquidation_morpho && cfg.addresses.morpho_blue.is_some() {
            strategies.push(Arc::new(MorphoLiquidationStrategy::new(
                cfg.liquidation.morpho_market_cap,
                cfg.liquidation.morpho_borrower_cap,
                leads.clone(),
            )));
        }
        if cfg.strategies.liquidation_maker && cfg.addresses.maker {
            let ilks: Vec<&'static crate::strategies::liquidation_maker::maker::IlkSpec> = cfg
                .liquidation
                .maker_ilks
                .iter()
                .filter_map(|name| crate::strategies::liquidation_maker::maker::spec_by_name(name))
                .collect();
            if ilks.is_empty() {
                tracing::warn!(target: "engine", "STRATEGY_LIQUIDATION_MAKER is on but MAKER_ILKS matched nothing in the built-in table");
            }
            strategies.push(Arc::new(MakerLiquidationStrategy::new(
                ilks,
                cfg.liquidation.watch_cap,
                leads.clone(),
            )));
        }
        if cfg.strategies.oracle_frontrun {
            strategies.push(Arc::new(OracleFrontrunStrategy::new(
                cfg.oracle.watch_feeds.clone(),
                cfg.oracle.max_leads,
                leads.clone(),
            )));
        }
        if cfg.strategies.sniper {
            strategies.push(Arc::new(SniperStrategy::new()));
        }

        // The directional sniper lane. Constructed unconditionally so the
        // console can always show its state and explain why it is disabled —
        // a lane that vanishes when it is off is a lane operators cannot
        // reason about. Its own `enabled` switch (`SNIPER_DIRECTIONAL`,
        // default false) is what decides whether it may ever buy.
        let sniper = Arc::new(crate::sniper::SniperLane::from_env());
        {
            // Open exposure must survive a restart. Recover positions before
            // anything else can open new ones, so the concurrency and budget
            // gates see the true picture on the very first block.
            match store.live_sniper_positions() {
                Ok(open) if !open.is_empty() => {
                    tracing::warn!(
                        target: "sniper",
                        recovered = open.len(),
                        "recovered open sniper positions from the previous run — \
                         these are live exposure and will be marked and managed"
                    );
                    sniper.hydrate(open);
                }
                Ok(_) => {}
                Err(e) => {
                    // Fail closed: if we cannot tell what we are holding, we
                    // must not open anything new on top of it.
                    tracing::error!(
                        target: "sniper",
                        error = %e,
                        "could not read sniper positions; halting the lane"
                    );
                    sniper.halt("position recovery failed at boot");
                }
            }
            for token in store.sniper_honeypot_tokens().unwrap_or_default() {
                if let Ok(addr) = token.parse() {
                    sniper.blacklist(addr);
                }
            }
            let params = sniper.params();
            if params.is_armed() {
                tracing::warn!(
                    target: "sniper",
                    buy_size_wei = %params.buy_size_wei,
                    daily_budget_wei = %params.daily_budget_wei,
                    max_positions = params.max_concurrent_positions,
                    "DIRECTIONAL SNIPER IS ARMED — this lane can lose the full buy amount \
                     on every entry. It is not covered by the executor's profit-or-revert \
                     guard. See docs/SNIPER.md."
                );
            } else {
                tracing::info!(
                    target: "sniper",
                    blockers = ?params.arming_blockers(),
                    "directional sniper in shadow mode"
                );
            }
        }

        let (feed, _) = broadcast::channel(cfg.api.feed_capacity.max(64));
        let stats = Arc::new(Stats::default());
        stats
            .started_at_ms
            .store(now_ms(), std::sync::atomic::Ordering::Relaxed);

        let inventory = Arc::new(Inventory::new(cfg.inventory_gate));
        // Best-effort: a dummy searcher will read as nonce 0 / zero balances,
        // which is the honest picture of mainnet and does not gate unless
        // `INVENTORY_GATE` is on.
        if let Err(e) = inventory
            .refresh(
                &http,
                cfg.endpoints.searcher_address,
                cfg.chain.weth,
                cfg.endpoints.executor,
            )
            .await
        {
            tracing::debug!(target: "engine", error = %e, "inventory refresh failed at boot");
        }

        // Private bundles do not appear in the public pending nonce. Recover
        // every durable reservation before allowing a nonce to be reused.
        for reservation in store.active_nonce_reservations().unwrap_or_default() {
            if reservation.target_block < head.number {
                let _ = store.set_nonce_reservation_status(&reservation.bundle_id, "expired");
                continue;
            }
            if submission.cancel(&reservation.bundle_id).await {
                let _ = store.set_nonce_reservation_status(&reservation.bundle_id, "cancelled");
            } else {
                inventory.block_broadcast_until(reservation.target_block);
                let _ =
                    store.set_nonce_reservation_status(&reservation.bundle_id, "recovery_blocked");
                tracing::error!(
                    target: "submission",
                    bundle = %reservation.bundle_id,
                    start_nonce = reservation.start_nonce,
                    nonce_count = reservation.nonce_count,
                    target_block = reservation.target_block,
                    "could not prove private-bundle cancellation; nonce reuse blocked until target expiry"
                );
            }
        }

        let qualification = Arc::new(parking_lot::RwLock::new(crate::qualification::evaluate(
            &cfg,
            &store,
            &writes,
            now_ms(),
        )));

        if cfg.live_smoke_max > 0 {
            tracing::warn!(
                target: "engine",
                max = cfg.live_smoke_max,
                used = store.smoke_used().unwrap_or(0),
                "LIVE_SMOKE_MAX is set — up to that many bundles may be sent without qualification PASS"
            );
        }

        // Built before the struct literal below moves `cfg`.
        let mode = LiveMode::armed_at_boot(cfg.live_execution);
        // Only wire the replay queue when there is a delivered-block feed to
        // fill it — the bloXroute relay (mainnet) or the chain's own blocks
        // (sequencer chains; CHAIN_BLOCK_INGEST).
        let (replay_tx, replay_rx) = if cfg.relay_tx_ingest || cfg.chain_block_ingest {
            let (t, r) = tokio::sync::mpsc::channel(cfg.replay_queue_depth);
            (Some(t), Some(r))
        } else {
            (None, None)
        };
        let strategy_concurrency = cfg.strategy_concurrency;
        // The replay lane count stays at 1 unless the operator provisioned
        // more isolated replay forks; see `Config::replay_lanes`.
        let replay_lanes = cfg.replay_lanes;

        Ok(Self {
            cfg,
            store,
            writes,
            alerts,
            runtime,
            risk,
            sim,
            ctx,
            feed,
            stats,
            mode,
            sniper,
            strategies,
            pool_discovery,
            http,
            replay_rx: parking_lot::Mutex::new(replay_rx),
            replay_tx,
            replay_gate: Arc::new(tokio::sync::Semaphore::new(replay_lanes)),
            strategy_gate: Arc::new(tokio::sync::Semaphore::new(strategy_concurrency)),
            latency: Arc::new(Latency::default()),
            inventory,
            submission,
            qualification,
            qualification_refreshing: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            qualification_refreshed_at_ms: Arc::new(std::sync::atomic::AtomicU64::new(now_ms())),
            own_reconciliation_running: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            submission_gate: Arc::new(tokio::sync::Semaphore::new(1)),
            last_head: parking_lot::Mutex::new(Some(head)),
            // The boot-time refresh above already ran; the cooldowns start
            // unset so the first observed block always does a full pass.
            last_discovery_block: std::sync::atomic::AtomicU64::new(NEVER),
            last_inventory_block: std::sync::atomic::AtomicU64::new(NEVER),
        })
    }

    pub fn qualification_status(&self) -> crate::qualification::QualificationStatus {
        self.qualification.read().clone()
    }

    fn strategy_qualified(&self, strategy: Strategy) -> bool {
        self.qualification.read().strategy_passes(strategy)
    }

    /// Qualification PASS, or a remaining smoke slot. Smoke never promotes a
    /// shadow-only strategy — `RiskEngine::submittable` still requires
    /// `live_candidate()`.
    fn may_broadcast(&self, strategy: Strategy) -> bool {
        if self.strategy_qualified(strategy) {
            return true;
        }
        let used = self.store.smoke_used().unwrap_or(u64::MAX);
        if !crate::config::smoke_allows(used, self.cfg.live_smoke_max) {
            return false;
        }
        // Raw smoke must have a separate durable wei-denominated exposure
        // budget. Exact per-bundle reservation happens immediately before send.
        self.cfg.submission_mode != crate::config::SubmissionMode::Raw
            || (!self.cfg.live_smoke_max_gas_cost_wei.is_zero()
                && self
                    .store
                    .smoke_gas_at_risk_wei()
                    .map(|used| used < self.cfg.live_smoke_max_gas_cost_wei)
                    .unwrap_or(false))
    }

    fn refresh_qualification(self: &Arc<Self>) {
        use std::sync::atomic::Ordering;
        // One minute is comfortably below the default 120-second continuity
        // limit while avoiding repeated seven-day evidence scans.
        const REFRESH_INTERVAL_MS: u64 = 60_000;
        let now = now_ms();
        let last = self.qualification_refreshed_at_ms.load(Ordering::Acquire);
        if now.saturating_sub(last) < REFRESH_INTERVAL_MS
            || self.qualification_refreshing.swap(true, Ordering::AcqRel)
        {
            return;
        }
        self.qualification_refreshed_at_ms
            .store(now, Ordering::Release);
        let engine = self.clone();
        tokio::task::spawn_blocking(move || {
            let status = crate::qualification::evaluate(
                &engine.cfg,
                &engine.store,
                &engine.writes,
                now_ms(),
            );
            *engine.qualification.write() = status;
            engine
                .qualification_refreshing
                .store(false, Ordering::Release);
        });
    }

    /// Atomically narrow/widen runtime mode with respect to the serialized
    /// submission lane. A transition to simulation cancels active payloads
    /// before another candidate can reserve a nonce.
    pub async fn set_runtime_mode(&self, live: bool) -> Result<bool, &'static str> {
        let Ok(_nonce_lane) = self.submission_gate.acquire().await else {
            return Ok(false);
        };
        let effective = self.mode.set_live(live)?;
        if !effective {
            self.cancel_active_submissions_locked("runtime mode changed to simulation")
                .await;
        }
        Ok(effective)
    }

    /// Apply a risk patch while holding the nonce lane, then cancel every
    /// payload signed under the previous policy before releasing it.
    pub async fn apply_runtime_risk(&self, patch: crate::risk::RiskPatch) -> Result<(), String> {
        let _nonce_lane = self
            .submission_gate
            .acquire()
            .await
            .map_err(|_| "submission lane is closed".to_string())?;
        self.runtime.apply(patch)?;
        self.cancel_active_submissions_locked("runtime risk policy changed")
            .await;
        Ok(())
    }

    /// Cancel every unresolved private bundle after an operator/risk policy
    /// change. Nonces are released from highest to lowest only when every relay
    /// acknowledges cancellation; otherwise reuse remains blocked through the
    /// target block.
    pub async fn cancel_active_submissions(&self, reason: &str) {
        let Ok(_nonce_lane) = self.submission_gate.acquire().await else {
            return;
        };
        self.cancel_active_submissions_locked(reason).await;
    }

    async fn cancel_active_submissions_locked(&self, reason: &str) {
        let mut reservations = self.store.active_nonce_reservations().unwrap_or_default();
        reservations.sort_by_key(|reservation| std::cmp::Reverse(reservation.start_nonce));
        for reservation in reservations {
            if self.submission.cancel(&reservation.bundle_id).await {
                let _ = self
                    .store
                    .set_nonce_reservation_status(&reservation.bundle_id, "cancelled");
                let _ = self
                    .inventory
                    .release_nonces(reservation.start_nonce, reservation.nonce_count);
                tracing::warn!(target: "submission", bundle = %reservation.bundle_id, %reason, "active bundle cancelled by policy");
            } else {
                self.inventory
                    .block_broadcast_until(reservation.target_block);
                let _ = self
                    .store
                    .set_nonce_reservation_status(&reservation.bundle_id, "recovery_blocked");
                tracing::error!(target: "submission", bundle = %reservation.bundle_id, %reason, target_block = reservation.target_block, "policy cancellation not fully acknowledged; nonce reuse blocked");
            }
        }
    }

    /// Run forever.
    pub async fn run(self: Arc<Self>) -> Result<()> {
        self.spawn_alert_evaluator();
        if let Some(rx) = self.replay_rx.lock().take() {
            self.spawn_replay_worker(rx);
        }
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
                    let _ = self
                        .store
                        .record_relay_bid(&relay, slot, &builder, value_wei);
                    let _ = self.feed.send(FeedEvent::Relay {
                        relay,
                        slot,
                        builder,
                        value_wei,
                        seen_at_ms: now_ms(),
                    });
                }
                IngestEvent::RelayBlock { block, txs } => {
                    // Handed to the replay worker rather than scored inline.
                    // Scoring a delivered block means ~200 transactions
                    // through every strategy plus fork resets; doing that on
                    // this task stalled *ingestion itself* — live mempool
                    // transactions sat unread in the channel for as long as a
                    // post-mortem of an already-mined block took.
                    self.enqueue_replay_block(block, txs);
                }
            }
        }
        Ok(())
    }

    /// Ingest a delivered block and its transactions: persist both, push a feed
    /// event, and score every transaction for extractable value through the same
    /// strategy → risk → simulation funnel as a mempool transaction.
    async fn on_relay_block(self: Arc<Self>, block: crate::types::RelayBlock, txs: Vec<PendingTx>) {
        Stats::bump(&self.stats.relay_blocks_seen);
        self.stats
            .relay_txs_seen
            .fetch_add(txs.len() as u64, std::sync::atomic::Ordering::Relaxed);

        // One transaction for the block and all ~200 of its transactions,
        // rather than one commit per row. Runs on a blocking thread so the
        // insert never occupies a runtime worker.
        {
            let store = self.store.clone();
            let block = block.clone();
            let txs = txs.clone();
            if let Err(e) =
                tokio::task::spawn_blocking(move || store.record_relay_block_with_txs(&block, &txs))
                    .await
            {
                tracing::debug!(target: "engine", error = %e, "relay block persist task failed");
            }
        }

        // A compact summary goes to the live feed; the full records (calldata
        // included) are queryable from `/api/relay-txs`.
        let summaries: Vec<RelayTxSummary> = txs
            .iter()
            .take(8)
            .map(|t| RelayTxSummary {
                hash: t.hash,
                from: t.from,
                to: t.to,
                value: t.value,
                selector: t.selector().map(|s| format!("0x{}", hex::encode(s))),
            })
            .collect();
        let block_number = block.block_number;
        let _ = self.feed.send(FeedEvent::RelayBlock {
            block,
            tx_count: txs.len(),
            txs: summaries,
        });

        // Match decision-time opportunities before replay scoring creates its
        // own post-mortem rows for this block. Matches carry explicit evidence
        // and confidence; competitor total economics are never called exact.
        crate::attribution::reconcile_block(
            &self.store,
            &self.http,
            block_number,
            &txs,
            self.cfg.chain.weth,
        )
        .await;

        // Score each transaction: strategies propose opportunities (sandwich,
        // back-run, liquidation, sniper) and the simulator decides whether value
        // was extractable. Relay transactions are already mined, so the fork
        // replay is a post-mortem of what *could* have been captured.
        //
        // Bounded on purpose. A mainnet block carries ~150-200 transactions and
        // each fans out across every strategy, so scoring a block unbounded
        // queues a matching burst of RPC every 12 seconds — which starves the
        // live mempool path and gets the bot rate limited off its provider.
        // `RELAY_TX_CONCURRENCY` caps how many are in flight; the work still
        // completes, it just does not stampede.
        //
        // The whole-block guard is held for the duration: the replay fork is
        // reset to each victim's parent, so two delivered blocks interleaving
        // would reset it out from under each other. `REPLAY_LANES` is how many
        // may run at once, and stays at 1 unless the operator provisioned one
        // isolated replay fork per lane.
        let _block_guard = self.replay_gate.clone().acquire_owned().await.ok();
        let permits = Arc::new(tokio::sync::Semaphore::new(self.cfg.relay_tx_concurrency));
        let mut set = tokio::task::JoinSet::new();
        for t in txs {
            let Ok(permit) = permits.clone().acquire_owned().await else {
                break;
            };
            let this = self.clone();
            set.spawn(async move {
                let _permit = permit;
                this.evaluate_awaited(t).await;
            });
        }
        while let Some(res) = set.join_next().await {
            if let Err(e) = res {
                if !e.is_cancelled() {
                    tracing::debug!(target: "engine", error = %e, "replay scoring task failed");
                }
            }
        }
        // Scoring is done: we now have simulations for this block sitting next
        // to the relay's realised builder payment. Reconcile them.
        self.reconcile_block_off_thread(block_number).await;
    }

    /// Hand a delivered block to the replay worker.
    ///
    /// Non-blocking and bounded: when the replay queue is full the **oldest**
    /// pending block is the one that should lose, so a backlog cannot delay
    /// fresher post-mortems indefinitely. `try_send` failing means the worker
    /// is still busy, and the block is dropped with a counter rather than
    /// applying back-pressure all the way up into ingestion.
    fn enqueue_replay_block(&self, block: crate::types::RelayBlock, txs: Vec<PendingTx>) {
        if let Some(q) = &self.replay_tx {
            if q.try_send((block, txs)).is_err() {
                let n = self
                    .stats
                    .replay_blocks_dropped
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                    + 1;
                if n % 100 == 1 {
                    tracing::warn!(
                        target: "engine",
                        dropped = n,
                        "replay queue full — delivered block skipped; \
                         raise REPLAY_QUEUE_DEPTH or lower RELAY_TX_CONCURRENCY"
                    );
                }
            }
        }
    }

    /// Start the dedicated replay worker.
    ///
    /// Isolation is the point: replay work is post-mortem scoring of
    /// already-mined transactions, so it must never compete with the live
    /// mempool path for the event loop. It gets its own task, its own bounded
    /// queue, and its own concurrency bound.
    fn spawn_replay_worker(self: &Arc<Self>, mut rx: ReplayQueueRx) {
        let this = self.clone();
        tokio::spawn(async move {
            while let Some((block, txs)) = rx.recv().await {
                this.clone().on_relay_block(block, txs).await;
            }
            tracing::info!(target: "engine", "replay worker stopped");
        });
    }

    /// `reconcile_block` on a blocking thread.
    ///
    /// It is a synchronous SQLite read-and-write of up to 500 rows; running it
    /// directly on a runtime worker blocks that worker for the whole query.
    async fn reconcile_block_off_thread(self: &Arc<Self>, block: u64) {
        let this = self.clone();
        if let Err(e) = tokio::task::spawn_blocking(move || this.reconcile_block(block)).await {
            tracing::debug!(target: "engine", error = %e, block, "reconciliation task failed");
        }
    }

    async fn on_block(self: Arc<Self>, head: BlockHead) {
        Stats::bump(&self.stats.blocks_seen);
        self.alerts.observe_head();

        let prev = self.last_head.lock().clone();
        if let Some(prev) = prev {
            if let Some((from, to)) = detect_reorg(&prev, &head) {
                Stats::bump(&self.stats.reorgs_seen);
                let old_hash = prev.hash;
                self.alerts.observe_reorg();
                let _ = self.store.record_reorg(
                    from,
                    to,
                    &format!("{old_hash:?}"),
                    &format!("{:?}", head.hash),
                );
                let _ = self.feed.send(FeedEvent::Reorg {
                    from_block: from,
                    to_block: to,
                    depth: to.saturating_sub(from).saturating_add(1),
                    old_hash,
                    new_hash: head.hash,
                    seen_at_ms: now_ms(),
                });
                tracing::warn!(
                    target: "engine",
                    from,
                    to,
                    old = %format!("{old_hash:?}"),
                    new = %format!("{:?}", head.hash),
                    "re-org detected — simulations in range marked non-canonical"
                );
            } else if head.number == prev.number + 1 {
                // The parent just gained a child, so it is as confirmed as a
                // single new head can make it. Reconcile it against relay traces.
                let this = self.clone();
                let n = prev.number;
                // Synchronous SQLite work belongs on the blocking pool.
                tokio::task::spawn_blocking(move || {
                    this.reconcile_block(n);
                });
            }
        }
        *self.last_head.lock() = Some(head.clone());

        // Receipt polling and finality reconciliation can fan out over many
        // submitted hashes. It must never delay pool refresh or block-cadence
        // strategies, so it runs as an independent async task.
        if !self
            .own_reconciliation_running
            .swap(true, std::sync::atomic::Ordering::AcqRel)
        {
            let engine = self.clone();
            let reconcile_head = head.number;
            tokio::spawn(async move {
                crate::attribution::reconcile_own_submissions(
                    &engine.store,
                    &engine.http,
                    reconcile_head,
                    engine.cfg.finality_depth,
                    engine.cfg.endpoints.searcher_address,
                    engine.ctx.executor,
                )
                .await;
                engine
                    .own_reconciliation_running
                    .store(false, std::sync::atomic::Ordering::Release);
            });
        }

        // Both of these sit *in front of* the strategies on the block task, so
        // every millisecond they spend is a millisecond the strategies do not
        // get. Neither needs to run on every block: the searcher's nonce and
        // balances only move when the bot transacts, and new pools appear far
        // slower than 12 s. Each is gated on a block-count cooldown that
        // defaults to 1 (the previous every-block behaviour) so the default
        // build is unchanged and operators can dial it back on a busy chain.
        if should_run(
            &self.last_inventory_block,
            head.number,
            self.cfg.inventory_refresh_blocks,
        ) {
            let started = std::time::Instant::now();
            let _ = self
                .inventory
                .refresh(
                    &self.http,
                    self.cfg.endpoints.searcher_address,
                    self.cfg.chain.weth,
                    self.cfg.endpoints.executor,
                )
                .await;
            self.latency
                .observe(Stage::Inventory, started.elapsed().as_millis() as u64);
        }

        self.ctx.set_head(head.clone());
        self.writes.record_block(&head);
        self.refresh_qualification();
        let _ = self.feed.send(FeedEvent::Block(head.clone()));

        if (self.cfg.pool_discovery || self.cfg.pool_discovery_v3)
            && should_run(
                &self.last_discovery_block,
                head.number,
                self.cfg.pool_discovery_interval_blocks,
            )
        {
            let discovery = &self.pool_discovery;
            let ctx = self.ctx.clone();
            let head = head.clone();
            // Discovery is a single eth_getLogs + a few pool loads; run it on the
            // block task before strategies spawn. It feeds the cache that all
            // strategies read on their next tick. The log cursor inside
            // `PoolDiscovery` is range-based, so skipping a block widens the
            // next scan window rather than losing pools.
            let started = std::time::Instant::now();
            let _new_pools = discovery.discover(&ctx, &head).await;
            self.latency
                .observe(Stage::Discovery, started.elapsed().as_millis() as u64);
        }

        // One task for the whole block tick rather than one per strategy: the
        // block cadence is 12 s, so the fan-out does not need its own spawn
        // per strategy to stay responsive, and keeping it in a single task
        // means a slow block never leaves orphaned work behind.
        let this = self.clone();
        tokio::spawn(async move {
            // Block-cadence opportunities have no victim transaction.
            let no_victims: Arc<Vec<Vec<u8>>> = Arc::new(Vec::new());
            let mut set = tokio::task::JoinSet::new();
            for s in &this.strategies {
                let strat = s.clone();
                let engine = this.clone();
                let head = head.clone();
                let kind = s.kind();
                let no_victims = no_victims.clone();
                set.spawn(async move {
                    let opps = strat.on_block(&engine.ctx, &head).await;
                    engine
                        .stats
                        .record_invocation(FunnelLane::Live, kind, opps.len());
                    for opp in opps {
                        engine
                            .clone()
                            .consider(
                                FunnelLane::Live,
                                opp,
                                no_victims.clone(),
                                None,
                                head.base_fee_per_gas,
                                now_ms(),
                            )
                            .await;
                    }
                });
            }
            while let Some(res) = set.join_next().await {
                if let Err(e) = res {
                    if !e.is_cancelled() {
                        tracing::debug!(target: "engine", error = %e, "block strategy task failed");
                    }
                }
            }
        });
    }

    async fn on_pending(self: Arc<Self>, tx: PendingTx) {
        Stats::bump(&self.stats.pending_seen);
        self.alerts.observe_pending();
        self.latency.observe(
            Stage::IngestToStrategy,
            now_ms().saturating_sub(tx.seen_at_ms),
        );
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

        self.evaluate(tx).await;
    }

    /// Run one observed transaction through every strategy, then risk-gate,
    /// simulate and record whatever they propose. Shared by the live mempool path
    /// (`on_pending`) and the relay-delivered-block backfill path, so both are
    /// scored identically — but counted in separate funnel lanes, because one
    /// is an opportunity and the other is a post-mortem.
    ///
    /// Fan-out form: the strategies for one transaction run concurrently
    /// inside a **single** spawned task, and the live path returns immediately.
    ///
    /// The previous shape spawned one task *per strategy per transaction*. With
    /// ten strategies enabled and a busy block that is 10 × ~200 = 2000+ task
    /// spawns per block, each carrying a cloned `PendingTx` (calldata included)
    /// and all of them landing on the runtime at once. The scheduler spends its
    /// time on queue churn instead of the one transaction that actually
    /// matters, and the burst is what starves the latency-critical path.
    ///
    /// Now: one spawn per transaction. Inside it a `JoinSet` runs the
    /// strategies concurrently — the same parallelism, since these are IO-bound
    /// futures — but the fan-out is bounded and owned by one task that can be
    /// dropped as a unit. A global semaphore caps how many transactions are
    /// being evaluated at once (`STRATEGY_CONCURRENCY`), so a mempool spike
    /// applies back-pressure instead of unbounded queueing.
    async fn evaluate(self: Arc<Self>, tx: PendingTx) {
        let lane = FunnelLane::for_source(tx.source);
        // Try to claim a slot without waiting: the live mempool path must not
        // block ingestion. When the engine is already saturated the
        // transaction is dropped and counted, which is the honest outcome —
        // queueing it would only add latency to work that is already late.
        let Ok(permit) = self.strategy_gate.clone().try_acquire_owned() else {
            Stats::bump(&self.stats.evaluations_shed);
            if let Some(suppressed) = SHED_LOG.allow() {
                tracing::warn!(
                    target: "engine",
                    hash = %format!("{:?}", tx.hash),
                    suppressed,
                    "strategy fan-out saturated — shedding transaction; \
                     raise STRATEGY_CONCURRENCY if this is sustained"
                );
            }
            return;
        };
        tokio::spawn(async move {
            let _permit = permit;
            self.fan_out(tx, lane).await;
        });
    }

    /// Run every strategy against one transaction concurrently, and wait for
    /// them. Shared by the live path (inside its own task) and the replay path
    /// (which awaits it directly for back-pressure).
    async fn fan_out(self: Arc<Self>, tx: PendingTx, lane: FunnelLane) {
        // `Arc` the transaction once instead of cloning the calldata per
        // strategy: with ten strategies that is nine fewer copies of every
        // pending transaction's input bytes.
        let tx = Arc::new(tx);
        let mut set = tokio::task::JoinSet::new();
        for s in &self.strategies {
            let strat = s.clone();
            let this = self.clone();
            let tx = tx.clone();
            let kind = s.kind();
            set.spawn(async move {
                this.run_strategy(strat, kind, tx, lane).await;
            });
        }
        while let Some(res) = set.join_next().await {
            if let Err(e) = res {
                if !e.is_cancelled() {
                    tracing::debug!(target: "engine", error = %e, "strategy task failed");
                }
            }
        }
    }

    /// Awaited form of [`evaluate`]: runs the strategies one after another and
    /// returns when they are all done.
    ///
    /// Used by the replay path, where completion is what makes back-pressure
    /// possible. Already-mined transactions have no deadline, so trading
    /// latency for a bounded task and RPC footprint is free.
    async fn evaluate_awaited(self: Arc<Self>, tx: PendingTx) {
        let lane = FunnelLane::for_source(tx.source);
        // Same fan-out as the live path, but awaited: completion is what makes
        // the caller's per-block semaphore real back-pressure. The strategies
        // still run concurrently with each other — the bound that matters for
        // the replay lane is how many *transactions* are in flight, which
        // `on_relay_block` owns.
        self.fan_out(tx, lane).await;
    }

    /// One strategy against one transaction: propose, then hand each proposal
    /// to `consider`.
    async fn run_strategy(
        self: Arc<Self>,
        strat: Arc<dyn StrategyImpl>,
        kind: Strategy,
        tx: Arc<PendingTx>,
        lane: FunnelLane,
    ) {
        let started = std::time::Instant::now();
        let opps = strat.on_pending(&self.ctx, &tx).await;
        self.latency
            .observe(Stage::Strategy, started.elapsed().as_millis() as u64);
        self.stats.record_invocation(lane, kind, opps.len());
        if opps.is_empty() {
            return;
        }
        // The victim must be replayable inside the fork, which needs the
        // raw signed bytes.
        let raw = match &tx.raw {
            Some(r) => Some(r.clone()),
            None => ingest::fetch_raw_tx(&self.http, tx.hash).await,
        };
        // Shared, not cloned per opportunity: a raw signed transaction is
        // hundreds of bytes to a few KB, and a widened search (multi-leg arb,
        // V3 victims) emits many opportunities from one victim.
        let victims: Arc<Vec<Vec<u8>>> = Arc::new(raw.map(|r| vec![r]).unwrap_or_default());
        let base_fee = tx.base_fee(&self.ctx.head());
        for opp in opps {
            self.clone()
                .consider(
                    lane,
                    opp,
                    victims.clone(),
                    tx.from.map(|from| (from, tx.nonce)),
                    base_fee,
                    tx.seen_at_ms,
                )
                .await;
        }
    }

    /// Risk-gate, simulate, record.
    async fn consider(
        self: Arc<Self>,
        lane: FunnelLane,
        opp: Opportunity,
        victims_raw: Arc<Vec<Vec<u8>>>,
        victim_sender_nonce: Option<(alloy_primitives::Address, u64)>,
        base_fee: U256,
        seen_at_ms: u64,
    ) {
        let kind = opp.strategy;
        // `base_fee` is the head's for live flow and the victim's own block's
        // for a replay. Gating a historical bundle on today's gas price, in
        // either direction, invents a result.
        let risk_started = std::time::Instant::now();
        if let Err(reject) = self.risk.check(&opp, base_fee) {
            Stats::bump(&self.stats.rejected);
            self.stats
                .record_funnel(lane, kind, |f| f.gated_by_risk += 1);
            if let Some(suppressed) = RISK_REJECT_LOG.allow() {
                tracing::debug!(
                    target: "engine",
                    strategy = opp.strategy.as_str(),
                    reason = reject.as_str(),
                    suppressed,
                    "rejected"
                );
            }
            return;
        }
        self.latency
            .observe(Stage::Risk, risk_started.elapsed().as_millis() as u64);
        if !self.inventory.can_fund(&opp) {
            Stats::bump(&self.stats.rejected);
            self.stats
                .record_funnel(lane, kind, |f| f.gated_by_risk += 1);
            if let Some(suppressed) = INVENTORY_REJECT_LOG.allow() {
                tracing::debug!(
                    target: "engine",
                    strategy = opp.strategy.as_str(),
                    suppressed,
                    "rejected: insufficient inventory"
                );
            }
            return;
        }
        // A sandwich or JIT without the victim's bytes cannot be simulated faithfully.
        if !opp.victim_hashes.is_empty() && victims_raw.is_empty() {
            self.stats
                .record_funnel(lane, kind, |f| f.missing_victim_raw += 1);
            if let Some(suppressed) = MISSING_VICTIM_LOG.allow() {
                tracing::debug!(
                    target: "engine",
                    strategy = kind.as_str(),
                    suppressed,
                    "skipping: victim raw transaction unavailable"
                );
            }
            return;
        }

        Stats::bump(&self.stats.opportunities);
        self.writes.record_opportunity(&opp);
        let _ = self.feed.send(FeedEvent::Opportunity(opp.clone()));

        // Shadow simulation stays fully concurrent. Only a profitable,
        // qualified candidate enters the serialized nonce lane below.
        let simulation_nonce = self.inventory.nonce_for(&opp);

        self.risk.begin(opp.strategy);
        let outcome = self
            .sim
            .run(
                &opp,
                &victims_raw,
                victim_sender_nonce,
                base_fee,
                simulation_nonce,
                lane == FunnelLane::Replay,
            )
            .await;
        self.risk.end(opp.strategy);

        let outcome = match outcome {
            Ok(o) => o,
            Err(e) => {
                if let Some(suppressed) = SIM_FAILED_LOG.allow() {
                    tracing::debug!(target: "engine", error = %e, suppressed, "simulation failed");
                }
                self.stats
                    .record_funnel(lane, kind, |f| f.simulations_failed += 1);
                return;
            }
        };

        Stats::bump(&self.stats.simulations);
        self.latency
            .observe(Stage::Simulation, outcome.primary.sim_latency_ms);
        if seen_at_ms > 0 {
            self.latency
                .observe(Stage::Total, now_ms().saturating_sub(seen_at_ms));
        }
        // Post-mortem replay is evidence, not capital at risk; it must never
        // trip the live drawdown switch.
        if lane == FunnelLane::Live {
            self.risk.observe(&outcome.primary);
            if self.risk.is_tripped() {
                let engine = self.clone();
                tokio::spawn(async move {
                    engine
                        .cancel_active_submissions("drawdown kill switch tripped")
                        .await;
                });
            }
        }
        self.writes.record_simulation(&outcome.primary);
        let _ = self
            .feed
            .send(FeedEvent::Simulation(outcome.primary.clone()));
        if let Some(relay) = &outcome.relay {
            self.writes.record_simulation(relay);
            let _ = self.feed.send(FeedEvent::Simulation(relay.clone()));
        }

        if outcome.primary.success {
            self.stats
                .record_funnel(lane, kind, |f| f.simulations_succeeded += 1);
        } else {
            self.stats
                .record_funnel(lane, kind, |f| f.simulations_reverted += 1);
        }

        let mut bundle = outcome.bundle;
        if self.risk.submittable(&outcome.primary) {
            Stats::bump(&self.stats.submittable);
            self.stats.record_funnel(lane, kind, |f| f.submittable += 1);
            tracing::info!(
                target: "engine",
                strategy = opp.strategy.as_str(),
                net_wei = outcome.primary.net_profit_wei,
                gas = outcome.primary.gas_used,
                block = opp.target_block,
                live = self.mode.live(),
                "PROFITABLE bundle"
            );
            if lane == FunnelLane::Live && self.mode.live() && self.cfg.broadcast_enabled {
                bundle = self
                    .submit_live_candidate(
                        &opp,
                        &victims_raw,
                        victim_sender_nonce,
                        base_fee,
                        simulation_nonce,
                        bundle,
                    )
                    .await;
            }
        }
        if bundle.submitted {
            // Settlement records are safety state, not droppable telemetry.
            if let Err(error) = self.store.record_bundle(&bundle) {
                tracing::error!(target: "submission", %error, bundle = %bundle.id, "persisting submitted bundle failed");
                self.inventory.block_broadcast_until(bundle.target_block);
            }
        } else {
            self.writes.record_bundle(&bundle);
        }
        let _ = self.feed.send(FeedEvent::Bundle(bundle));
    }

    /// Serialize only profitable live candidates. Shadow simulations remain
    /// fully concurrent; when an earlier accepted bundle advanced the nonce,
    /// this reruns the candidate with the newly reserved nonce before sending.
    async fn submit_live_candidate(
        self: &Arc<Self>,
        opp: &Opportunity,
        victims_raw: &[Vec<u8>],
        victim_sender_nonce: Option<(alloy_primitives::Address, u64)>,
        base_fee: U256,
        initially_simulated_nonce: u64,
        initial_bundle: crate::types::BundleRecord,
    ) -> crate::types::BundleRecord {
        let qualification = self.qualification_status();
        let qualified = qualification.strategy_passes(opp.strategy);
        if !qualified && !self.may_broadcast(opp.strategy) {
            let reasons = qualification
                .strategies
                .iter()
                .find(|row| row.strategy == opp.strategy.as_str())
                .map(|row| row.reasons.clone())
                .unwrap_or_else(|| qualification.reasons.clone());
            tracing::warn!(target: "submission", strategy = opp.strategy.as_str(), ?reasons, "strategy has not independently passed qualification");
            return initial_bundle;
        }

        // Raw transport (sequencer chains) can only send *our* signed
        // transactions: a victim's signed bytes cannot be re-sent, so a
        // victim-pinned bundle (sandwich/JIT back-runs) is not deliverable
        // at the transport level. Block-cadence opportunities (atomic_arb:
        // no victim) are fine. The gateway repeats this check as a
        // backstop; refusing here means the nonce lane is never touched.
        if self.cfg.submission_mode == crate::config::SubmissionMode::Raw
            && !opp.victim_hashes.is_empty()
        {
            tracing::warn!(
                target: "submission",
                strategy = opp.strategy.as_str(),
                "raw submission mode cannot include the victim's signed \
                 transaction — refusing (private-orderflow delivery is a \
                 later integration on sequencer chains)"
            );
            return initial_bundle;
        }

        let Ok(_nonce_lane) = self.submission_gate.acquire().await else {
            return initial_bundle;
        };
        let head = self.ctx.head().number;
        if !self.mode.live()
            || !self.cfg.broadcast_enabled
            || !self.may_broadcast(opp.strategy)
            || !self.inventory.broadcast_available(head)
            || initial_bundle.target_block <= head
        {
            return initial_bundle;
        }

        // Re-read the pending nonce immediately before reserving. A stale
        // inventory (failed refresh, or a tx that landed since the last head)
        // is exactly how a live send becomes "nonce too low" at the builder.
        let _ = self
            .inventory
            .refresh(
                &self.http,
                self.cfg.endpoints.searcher_address,
                self.cfg.chain.weth,
                self.cfg.endpoints.executor,
            )
            .await;

        let nonce_count = Inventory::legs(opp);
        if nonce_count == 0 {
            return initial_bundle;
        }
        let start_nonce = self.inventory.reserve_nonces(nonce_count);
        let mut bundle = initial_bundle;

        if start_nonce != initially_simulated_nonce {
            self.risk.begin(opp.strategy);
            let exact = self
                .sim
                .run(
                    opp,
                    victims_raw,
                    victim_sender_nonce,
                    base_fee,
                    start_nonce,
                    false,
                )
                .await;
            self.risk.end(opp.strategy);
            let Ok(exact) = exact else {
                let _ = self.inventory.release_nonces(start_nonce, nonce_count);
                return bundle;
            };
            Stats::bump(&self.stats.simulations);
            self.writes.record_simulation(&exact.primary);
            let _ = self.feed.send(FeedEvent::Simulation(exact.primary.clone()));
            if let Some(relay) = &exact.relay {
                self.writes.record_simulation(relay);
                let _ = self.feed.send(FeedEvent::Simulation(relay.clone()));
            }
            if !self.risk.submittable(&exact.primary) {
                let _ = self.inventory.release_nonces(start_nonce, nonce_count);
                return bundle;
            }
            bundle = exact.bundle;
        }

        // Recheck mutable controls after any serialized exact-payload rerun.
        // `may_broadcast` (not `strategy_qualified`) so a remaining smoke
        // slot can still proceed; shadow-only strategies never reach here.
        if !self.mode.live()
            || !self.may_broadcast(opp.strategy)
            || self.risk.check(opp, base_fee).is_err()
            || !self.inventory.can_fund(opp)
        {
            let _ = self.inventory.release_nonces(start_nonce, nonce_count);
            return bundle;
        }

        if self
            .store
            .reserve_bundle_nonces(
                &bundle.id,
                &bundle.opportunity_id,
                start_nonce,
                nonce_count,
                bundle.target_block,
            )
            .is_err()
        {
            let _ = self.inventory.release_nonces(start_nonce, nonce_count);
            tracing::error!(target: "submission", bundle = %bundle.id, "durable nonce reservation failed; refusing broadcast");
            return bundle;
        }

        // Smoke is a bounded, durable bypass of qualification PASS only.
        // Consume after every other gate, immediately before the send.
        // A persist failure or exhausted budget refuses the send; a
        // restart cannot refill the counter. Qualified strategies do not
        // spend a slot.
        if !self.strategy_qualified(opp.strategy) {
            let raw_mode = self.cfg.submission_mode == crate::config::SubmissionMode::Raw;
            let gas_at_risk = if raw_mode {
                match crate::submission::raw_bundle_gas_at_risk(&bundle) {
                    Some(value) => value,
                    None => {
                        tracing::error!(
                            target: "submission",
                            bundle = %bundle.id,
                            "raw smoke risk could not be decoded from the exact signed payload; refusing send"
                        );
                        let _ = self
                            .store
                            .set_nonce_reservation_status(&bundle.id, "cancelled");
                        let _ = self.inventory.release_nonces(start_nonce, nonce_count);
                        return bundle;
                    }
                }
            } else {
                U256::ZERO
            };
            let raw_cap = raw_mode.then_some(self.cfg.live_smoke_max_gas_cost_wei);
            match self
                .store
                .try_consume_smoke_budget(self.cfg.live_smoke_max, raw_cap, gas_at_risk)
            {
                Ok(true) => {
                    tracing::warn!(
                        target: "submission",
                        strategy = opp.strategy.as_str(),
                        used = self.store.smoke_used().unwrap_or(0),
                        max = self.cfg.live_smoke_max,
                        gas_at_risk_wei = %gas_at_risk,
                        gas_risk_used_wei = %self.store.smoke_gas_at_risk_wei().unwrap_or(U256::MAX),
                        gas_risk_cap_wei = %self.cfg.live_smoke_max_gas_cost_wei,
                        bundle = %bundle.id,
                        "consuming a live-smoke slot — sending without qualification PASS"
                    );
                }
                Ok(false) => {
                    tracing::warn!(
                        target: "submission",
                        strategy = opp.strategy.as_str(),
                        bundle = %bundle.id,
                        "live-smoke budget exhausted; refusing send"
                    );
                    let _ = self
                        .store
                        .set_nonce_reservation_status(&bundle.id, "cancelled");
                    let _ = self.inventory.release_nonces(start_nonce, nonce_count);
                    return bundle;
                }
                Err(error) => {
                    tracing::error!(
                        target: "submission",
                        %error,
                        bundle = %bundle.id,
                        "could not persist live-smoke slot; refusing send"
                    );
                    let _ = self
                        .store
                        .set_nonce_reservation_status(&bundle.id, "cancelled");
                    let _ = self.inventory.release_nonces(start_nonce, nonce_count);
                    return bundle;
                }
            }
        }

        bundle.submitted = self.submission.submit(&bundle).await;
        if bundle.submitted {
            let _ = self
                .store
                .set_nonce_reservation_status(&bundle.id, "accepted");
        } else if self.submission.cancel(&bundle.id).await {
            let _ = self
                .store
                .set_nonce_reservation_status(&bundle.id, "cancelled");
            let _ = self.inventory.release_nonces(start_nonce, nonce_count);
        } else {
            self.inventory.block_broadcast_until(bundle.target_block);
            let _ = self
                .store
                .set_nonce_reservation_status(&bundle.id, "recovery_blocked");
        }
        bundle
    }

    /// Compare stored simulations for `block` against relay bid traces and
    /// landed transactions, and persist the result. Synchronous on the store;
    /// safe to call from a spawned task.
    fn reconcile_block(&self, block: u64) {
        match crate::replay::compare(&self.store, Some(block), Some(block), 500) {
            Ok(rows) if !rows.is_empty() => {
                if let Err(e) = crate::replay::persist(&self.store, &rows) {
                    tracing::debug!(target: "engine", error = %e, block, "reconciliation persist failed");
                }
            }
            Ok(_) => {}
            Err(e) => tracing::debug!(target: "engine", error = %e, block, "reconciliation failed"),
        }
    }
}

/// Inclusive range of blocks that are no longer canonical, given the previous
/// head and the newly observed one. `None` means the chain advanced (or this
/// is a duplicate of the same head).
pub fn detect_reorg(prev: &BlockHead, head: &BlockHead) -> Option<(u64, u64)> {
    if head.number < prev.number {
        // Rewind: the new head is behind the one we had. Everything from the
        // new height through the old tip was on a discarded fork.
        Some((head.number, prev.number))
    } else if head.number == prev.number && head.hash != prev.hash {
        Some((head.number, head.number))
    } else if head.number == prev.number + 1 && head.parent_hash != prev.hash {
        // The new block does not build on the head we stored. That head is
        // the one that got re-orged out.
        Some((prev.number, prev.number))
    } else {
        None
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
fn hint_to_pending(
    raw: &serde_json::Value,
    hash: alloy_primitives::B256,
    to: Option<alloy_primitives::Address>,
) -> Option<PendingTx> {
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
        mined_at: None,
        seen_at_ms: now_ms(),
    })
}

/// Strategies enabled at runtime, for the status endpoint.
pub fn enabled_strategies(cfg: &Config) -> Vec<&'static str> {
    Strategy::all()
        .iter()
        .filter(|s| match s {
            Strategy::Sandwich => cfg.strategies.sandwich,
            Strategy::SandwichV3 => cfg.strategies.sandwich_v3,
            Strategy::Jit => cfg.strategies.jit,
            Strategy::AtomicArb => cfg.strategies.atomic_arb,
            Strategy::Liquidation => cfg.strategies.liquidation,
            Strategy::LiquidationCompound => cfg.strategies.liquidation_compound,
            Strategy::LiquidationMorpho => cfg.strategies.liquidation_morpho,
            Strategy::LiquidationMaker => cfg.strategies.liquidation_maker,
            Strategy::OracleFrontrun => cfg.strategies.oracle_frontrun,
            Strategy::Sniper => cfg.strategies.sniper,
        })
        .map(|s| s.as_str())
        .collect()
}

impl Engine {
    /// Evaluate the alert rules on a fixed interval. Transitions are logged,
    /// pushed to the SSE feed and (optionally) delivered to a webhook.
    fn spawn_alert_evaluator(self: &Arc<Self>) {
        let this = self.clone();
        let interval = std::time::Duration::from_secs(self.cfg.alerts.eval_secs);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(interval);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tick.tick().await;
                let risk = this.runtime.risk();
                let conversion = this
                    .stats
                    .funnel
                    .iter()
                    .map(|e| {
                        let (s, c) = (e.key(), e.value());
                        (s.as_str(), c.candidates_emitted, c.submittable)
                    })
                    .collect::<Vec<_>>();
                let signals = crate::alerts::AlertSignals {
                    now_ms: now_ms(),
                    last_head_ms: this.alerts.last_head_ms(),
                    last_pending_ms: this.alerts.last_pending_ms(),
                    mempool_feed_configured: this.cfg.endpoints.ws_url.is_some(),
                    kill_switch_tripped: this.risk.is_tripped(),
                    drawdown: Some((
                        u128::try_from(risk.max_drawdown_wei).unwrap_or(u128::MAX),
                        this.risk.cumulative_net(),
                    )),
                    conversion,
                    reorgs_since_last_eval: this.alerts.take_reorgs(),
                };
                for a in this.alerts.evaluate(&signals) {
                    let _ = this.feed.send(FeedEvent::Alert {
                        rule: a.rule.to_string(),
                        severity: format!("{:?}", a.severity).to_lowercase(),
                        message: a.message.clone(),
                        active: a.active,
                        seen_at_ms: a.at_ms,
                    });
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_mode_can_never_arm_an_unarmed_process() {
        let m = LiveMode::armed_at_boot(false);
        assert!(!m.live());
        assert!(!m.armed());
        // The runtime switch cannot grant what the environment did not.
        let err = m.set_live(true).unwrap_err();
        assert!(err.contains("LIVE_EXECUTION"));
        assert!(!m.live());
        // Switching an unarmed bot "off" is a harmless no-op that succeeds.
        assert!(!m.set_live(false).unwrap());
    }

    #[test]
    fn live_mode_toggles_at_runtime_when_armed_at_boot() {
        let m = LiveMode::armed_at_boot(true);
        assert!(m.armed());
        assert!(m.live(), "an armed boot starts live");
        assert!(!m.set_live(false).unwrap(), "operator pauses to simulation");
        assert!(m.set_live(false).is_ok());
        assert!(m.set_live(true).unwrap(), "operator resumes live");
        assert!(m.live());
    }

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
        s.record_funnel(FunnelLane::Live, Strategy::Sandwich, |f| {
            f.candidates_emitted += 3
        });
        s.record_funnel(FunnelLane::Live, Strategy::Sandwich, |f| {
            f.gated_by_risk += 1
        });
        s.record_funnel(FunnelLane::Live, Strategy::AtomicArb, |f| {
            f.candidates_emitted += 7
        });
        s.record_funnel(FunnelLane::Live, Strategy::AtomicArb, |f| {
            f.simulations_succeeded += 2
        });
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
        s.record_funnel(FunnelLane::Live, Strategy::Jit, |_| {});
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
        s.record_invocation(FunnelLane::Live, Strategy::AtomicArb, 3);
        let snap = s.snapshot();
        let arb = &snap["funnel"]["atomic_arb"];
        assert_eq!(arb["candidatesEmitted"], 3);
        assert_eq!(arb["invocationsWithOutput"], 1);
        assert_eq!(arb["invocationsEmpty"], 0);
    }

    #[test]
    fn empty_invocation_counts_only_as_an_empty_call() {
        let s = Stats::default();
        s.record_invocation(FunnelLane::Live, Strategy::Sandwich, 0);
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
            s.record_invocation(FunnelLane::Live, Strategy::Jit, n);
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
    fn replay_flow_never_touches_the_live_funnel() {
        // The bloXroute backfill scores ~150 already-mined transactions per
        // block. If those landed in the live funnel they would swamp it and
        // every conversion rate in the dashboard would become meaningless.
        let s = Stats::default();
        s.record_invocation(FunnelLane::Replay, Strategy::Sandwich, 4);
        s.record_funnel(FunnelLane::Replay, Strategy::Sandwich, |f| {
            f.submittable += 2
        });

        let snap = s.snapshot();
        assert!(
            snap["funnel"].as_object().unwrap().is_empty(),
            "replay observations must not appear in the live funnel"
        );
        let replay = &snap["funnelReplay"]["sandwich"];
        assert_eq!(replay["candidatesEmitted"], 4);
        assert_eq!(replay["submittable"], 2);
    }

    #[test]
    fn the_two_lanes_count_the_same_strategy_separately() {
        let s = Stats::default();
        s.record_invocation(FunnelLane::Live, Strategy::AtomicArb, 1);
        s.record_invocation(FunnelLane::Replay, Strategy::AtomicArb, 9);

        let snap = s.snapshot();
        assert_eq!(snap["funnel"]["atomic_arb"]["candidatesEmitted"], 1);
        assert_eq!(snap["funnelReplay"]["atomic_arb"]["candidatesEmitted"], 9);
    }

    #[test]
    fn lane_is_chosen_by_transaction_provenance() {
        // Already-mined sources are post-mortem; everything else is actionable.
        assert_eq!(
            FunnelLane::for_source(TxSource::RelayDelivered),
            FunnelLane::Replay
        );
        assert_eq!(FunnelLane::for_source(TxSource::Mined), FunnelLane::Replay);
        assert_eq!(
            FunnelLane::for_source(TxSource::PublicMempool),
            FunnelLane::Live
        );
        assert_eq!(FunnelLane::for_source(TxSource::MevShare), FunnelLane::Live);
        assert_eq!(
            FunnelLane::for_source(TxSource::ExternalStream),
            FunnelLane::Live
        );
        assert_eq!(
            FunnelLane::for_source(TxSource::Sequencer),
            FunnelLane::Live
        );
        assert_eq!(
            FunnelLane::for_source(TxSource::Flashblock),
            FunnelLane::Live
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
        // Both funnel lanes untouched.
        assert!(snap["funnel"].as_object().unwrap().is_empty());
        assert!(snap["funnelReplay"].as_object().unwrap().is_empty());
    }

    fn head(number: u64, hash: u8, parent: u8) -> BlockHead {
        BlockHead {
            number,
            hash: alloy_primitives::B256::from([hash; 32]),
            parent_hash: alloy_primitives::B256::from([parent; 32]),
            timestamp: 0,
            base_fee_per_gas: U256::ZERO,
            gas_used: 0,
            gas_limit: 30_000_000,
        }
    }

    #[test]
    fn a_child_of_the_stored_head_is_not_a_reorg() {
        let prev = head(10, 1, 0);
        let next = head(11, 2, 1);
        assert_eq!(detect_reorg(&prev, &next), None);
    }

    #[test]
    fn a_parent_mismatch_reorgs_the_previous_head() {
        let prev = head(10, 1, 0);
        let next = head(11, 2, 9); // parent is not 1
        assert_eq!(detect_reorg(&prev, &next), Some((10, 10)));
    }

    #[test]
    fn a_rewind_marks_the_whole_abandoned_range() {
        let prev = head(15, 5, 4);
        let next = head(12, 9, 8);
        assert_eq!(detect_reorg(&prev, &next), Some((12, 15)));
    }

    #[test]
    fn same_height_different_hash_is_a_reorg() {
        let prev = head(10, 1, 0);
        let next = head(10, 2, 0);
        assert_eq!(detect_reorg(&prev, &next), Some((10, 10)));
    }

    // --- maintenance cooldown -------------------------------------------

    #[test]
    fn a_cooldown_of_one_runs_on_every_block() {
        // The default. Must reproduce the old unconditional behaviour
        // exactly, so upgrading without setting anything changes nothing.
        let last = std::sync::atomic::AtomicU64::new(NEVER);
        for n in 100..110 {
            assert!(should_run(&last, n, 1), "block {n} should run");
        }
    }

    #[test]
    fn a_cooldown_skips_until_the_interval_has_passed() {
        let last = std::sync::atomic::AtomicU64::new(NEVER);
        // Never run before: the first block always runs.
        assert!(should_run(&last, 100, 5));
        // Inside the window.
        assert!(!should_run(&last, 101, 5));
        assert!(!should_run(&last, 104, 5));
        // Exactly at the interval.
        assert!(should_run(&last, 105, 5));
        assert!(!should_run(&last, 106, 5));
        // A gap larger than the interval (missed blocks) still runs.
        assert!(should_run(&last, 200, 5));
    }

    #[test]
    fn a_rewind_always_refreshes() {
        // After a re-org to a lower height the cached state was built against
        // a chain that no longer exists, so the cooldown must not suppress
        // the refresh.
        let last = std::sync::atomic::AtomicU64::new(NEVER);
        assert!(should_run(&last, 500, 10));
        assert!(!should_run(&last, 502, 10));
        assert!(should_run(&last, 495, 10), "a rewind must re-run");
    }

    // --- hot-path log limiting ------------------------------------------

    #[test]
    fn the_log_limiter_passes_one_and_counts_the_rest() {
        let l = LogLimiter::new(60_000);
        // First call in the window is allowed, with nothing suppressed yet.
        assert_eq!(l.allow(), Some(0));
        // Everything else in the window is suppressed...
        for _ in 0..50 {
            assert_eq!(l.allow(), None);
        }
        // ...and the suppressed count is reported by the next allowed call.
        let l2 = LogLimiter::new(0);
        assert_eq!(l2.allow(), Some(0));
        for _ in 0..3 {
            let _ = l2.allow();
        }
        // With a zero interval every call is allowed again.
        assert!(l2.allow().is_some());
    }

    #[test]
    fn a_zero_interval_limiter_never_suppresses() {
        let l = LogLimiter::new(0);
        for _ in 0..10 {
            assert!(l.allow().is_some());
        }
    }

    // --- funnel lanes ----------------------------------------------------

    #[test]
    fn mev_blocker_flow_is_live_not_replay() {
        // MEV Blocker publishes transactions that have not been mined yet —
        // they are actionable, so they belong in the live funnel. Getting
        // this wrong would file real opportunities under post-mortem.
        assert_eq!(
            FunnelLane::for_source(TxSource::MevBlocker),
            FunnelLane::Live
        );
        assert_eq!(
            FunnelLane::for_source(TxSource::RelayDelivered),
            FunnelLane::Replay
        );
        assert_eq!(FunnelLane::for_source(TxSource::Mined), FunnelLane::Replay);
    }

    #[test]
    fn the_shed_counter_is_reported_in_the_snapshot() {
        let s = Stats::default();
        Stats::bump(&s.evaluations_shed);
        Stats::bump(&s.evaluations_shed);
        Stats::bump(&s.replay_blocks_dropped);
        let snap = s.snapshot();
        assert_eq!(snap["evaluationsShed"], 2);
        assert_eq!(snap["replayBlocksDropped"], 1);
    }

    #[test]
    fn funnel_updates_from_many_threads_are_not_lost() {
        // The dashmap swap must stay atomic per counter: this is the
        // regression guard for the lock-free funnel.
        let s = std::sync::Arc::new(Stats::default());
        let mut handles = Vec::new();
        for _ in 0..8 {
            let s = s.clone();
            handles.push(std::thread::spawn(move || {
                for _ in 0..500 {
                    s.record_funnel(FunnelLane::Live, Strategy::Sandwich, |f| {
                        f.candidates_emitted += 1
                    });
                    s.record_funnel(FunnelLane::Replay, Strategy::AtomicArb, |f| {
                        f.submittable += 1
                    });
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        let snap = s.snapshot();
        assert_eq!(snap["funnel"]["sandwich"]["candidatesEmitted"], 8 * 500);
        assert_eq!(snap["funnelReplay"]["atomic_arb"]["submittable"], 8 * 500);
    }
}
