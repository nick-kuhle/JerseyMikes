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
    arb::AtomicArbStrategy, jit::JitStrategy, liquidation::LiquidationStrategy, sandwich::SandwichStrategy,
    sniper::SniperStrategy, StrategyCtx, StrategyImpl,
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
}

impl Stats {
    fn bump(counter: &std::sync::atomic::AtomicU64) {
        counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> serde_json::Value {
        use std::sync::atomic::Ordering::Relaxed;
        serde_json::json!({
            "pendingSeen": self.pending_seen.load(Relaxed),
            "hintsSeen": self.hints_seen.load(Relaxed),
            "blocksSeen": self.blocks_seen.load(Relaxed),
            "opportunities": self.opportunities.load(Relaxed),
            "simulations": self.simulations.load(Relaxed),
            "submittable": self.submittable.load(Relaxed),
            "rejected": self.rejected.load(Relaxed),
            "startedAtMs": self.started_at_ms.load(Relaxed),
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

        Ok(Self {
            cfg,
            store,
            risk,
            sim,
            ctx,
            feed,
            stats,
            strategies,
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

        for s in &self.strategies {
            let strat = s.clone();
            let this = self.clone();
            let head = head.clone();
            tokio::spawn(async move {
                let opps = strat.on_block(&this.ctx, &head).await;
                for opp in opps {
                    this.clone().consider(opp, Vec::new()).await;
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
            tokio::spawn(async move {
                let opps = strat.on_pending(&this.ctx, &tx).await;
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
                    this.clone().consider(opp, victims.clone()).await;
                }
            });
        }
    }

    /// Risk-gate, simulate, record.
    async fn consider(self: Arc<Self>, opp: Opportunity, victims_raw: Vec<Vec<u8>>) {
        let head = self.ctx.head();
        if let Err(reject) = self.risk.check(&opp, head.base_fee_per_gas) {
            Stats::bump(&self.stats.rejected);
            tracing::debug!(target: "engine", strategy = opp.strategy.as_str(), reason = reject.as_str(), "rejected");
            return;
        }
        // A sandwich or JIT without the victim's bytes cannot be simulated faithfully.
        if !opp.victim_hashes.is_empty() && victims_raw.is_empty() {
            tracing::debug!(target: "engine", "skipping: victim raw transaction unavailable");
            return;
        }

        Stats::bump(&self.stats.opportunities);
        let _ = self.store.record_opportunity(&opp);
        let _ = self.feed.send(FeedEvent::Opportunity(opp.clone()));

        self.risk.begin(opp.strategy);
        let outcome = self
            .sim
            .run(&opp, &victims_raw, head.base_fee_per_gas, 0)
            .await;
        self.risk.end(opp.strategy);

        let outcome = match outcome {
            Ok(o) => o,
            Err(e) => {
                tracing::debug!(target: "engine", error = %e, "simulation failed");
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

        let mut bundle = outcome.bundle;
        if self.risk.submittable(&outcome.primary) {
            Stats::bump(&self.stats.submittable);
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
