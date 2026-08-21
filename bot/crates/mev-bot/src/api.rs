//! HTTP API consumed by the dashboard.
//!
//! Plain REST for history/aggregates plus one SSE stream for everything live.
//! CORS is wide open because the API is expected to run behind the operator's
//! own network boundary and be proxied by the Next.js app.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Query, State};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use futures_util::stream::Stream;
use serde::Deserialize;
use serde_json::json;
use tower_http::cors::{Any, CorsLayer};

use crate::engine::Engine;
use crate::types::Strategy;

#[derive(Clone)]
pub struct ApiState {
    pub engine: Arc<Engine>,
}

pub fn router(engine: Arc<Engine>) -> Router {
    let state = ApiState { engine };
    Router::new()
        .route("/api/health", get(health))
        .route("/api/status", get(status))
        .route("/api/config", get(config))
        .route("/api/pnl", get(pnl))
        .route("/api/pnl/series", get(pnl_series))
        .route("/api/opportunities", get(opportunities))
        .route("/api/simulations", get(simulations))
        .route("/api/relay-bids", get(relay_bids))
        .route("/api/relay-blocks", get(relay_blocks))
        .route("/api/relay-txs", get(relay_txs))
        .route("/api/funnel", get(funnel))
        .route("/api/latency", get(latency))
        .route("/api/competition", get(competition))
        .route("/api/reorgs", get(reorgs))
        .route("/api/stream", get(stream))
        .layer(
            CorsLayer::new()
                .allow_origin(Any)
                .allow_methods(Any)
                .allow_headers(Any),
        )
        .with_state(state)
}

async fn health() -> impl IntoResponse {
    Json(json!({"ok": true}))
}

async fn status(State(s): State<ApiState>) -> impl IntoResponse {
    let e = &s.engine;
    let head = e.ctx.head();
    Json(json!({
        "chain": {
            "id": e.cfg.chain.chain_id,
            "name": e.cfg.chain.name,
        },
        "head": {
            "number": head.number,
            "hash": format!("{:?}", head.hash),
            "baseFeeWei": head.base_fee_per_gas.to_string(),
            "gasUsed": head.gas_used,
            "timestamp": head.timestamp,
        },
        "mode": if e.cfg.live_execution { "live" } else { "simulation" },
        "strategies": crate::engine::enabled_strategies(&e.cfg),
        "risk": {
            "minNetProfitWei": e.cfg.risk.min_net_profit_wei.to_string(),
            "maxPositionWei": e.cfg.risk.max_position_wei.to_string(),
            "maxBaseFeeWei": e.cfg.risk.max_base_fee_wei.to_string(),
            "bribeBps": e.cfg.risk.bribe_bps,
            "killSwitchTripped": e.risk.is_tripped(),
            "cumulativeNetWei": e.risk.cumulative_net().to_string(),
        },
        "executor": format!("{:?}", e.ctx.executor),
        "pools": e.ctx.pools.len(),
        "poolsV3": e.ctx.pools_v3.len(),
        "stats": e.stats.snapshot(),
        "simBackends": {
            "anvilFork": e.sim.fork.is_some(),
            "relayCallBundle": e.sim.relay.is_some(),
        },
        "inventory": e.inventory.snapshot(),
        "latency": e.latency.snapshot(),
    }))
}

async fn config(State(s): State<ApiState>) -> impl IntoResponse {
    let e = &s.engine;
    Json(json!({
        "chainId": e.cfg.chain.chain_id,
        "weth": format!("{:?}", e.cfg.chain.weth),
        "executor": format!("{:?}", e.ctx.executor),
        "liveExecution": e.cfg.live_execution,
        "endpoints": {
            "ws": e.cfg.endpoints.ws_url.is_some(),
            "mevShare": !e.cfg.endpoints.mev_share_sse.is_empty(),
            "relays": e.cfg.endpoints.relay_data_urls.len(),
            "sequencerFeed": e.cfg.endpoints.sequencer_feed.is_some(),
            "externalMempools": e.cfg.endpoints.extra_mempool_ws.len(),
        },
        "bloxrouteRelay": {
            "url": e.cfg.endpoints.bloxroute_relay_url,
            "txIngest": e.cfg.relay_tx_ingest,
        },
    }))
}

async fn pnl(State(s): State<ApiState>) -> impl IntoResponse {
    match s.engine.store.pnl() {
        Ok(rows) => {
            let total: i64 = rows.iter().map(|r| r.net_profit_wei).sum();
            Json(json!({"byStrategy": rows, "totalNetWei": total}))
        }
        Err(e) => Json(json!({"error": e.to_string()})),
    }
}

#[derive(Deserialize)]
struct LimitQuery {
    limit: Option<i64>,
    strategy: Option<String>,
}

async fn pnl_series(State(s): State<ApiState>, Query(q): Query<LimitQuery>) -> impl IntoResponse {
    let limit = q.limit.unwrap_or(200).clamp(1, 5_000);
    match s.engine.store.pnl_series(limit) {
        Ok(rows) => Json(json!(rows)),
        Err(e) => Json(json!({"error": e.to_string()})),
    }
}

async fn opportunities(State(s): State<ApiState>, Query(q): Query<LimitQuery>) -> impl IntoResponse {
    let limit = q.limit.unwrap_or(100).clamp(1, 1_000);
    match s.engine.store.recent_opportunities(limit) {
        Ok(rows) => Json(json!(rows)),
        Err(e) => Json(json!({"error": e.to_string()})),
    }
}

async fn simulations(State(s): State<ApiState>, Query(q): Query<LimitQuery>) -> impl IntoResponse {
    let limit = q.limit.unwrap_or(100).clamp(1, 1_000);
    let strategy = q.strategy.as_deref().and_then(parse_strategy);
    match s.engine.store.recent_simulations(limit, strategy) {
        Ok(rows) => Json(json!(rows)),
        Err(e) => Json(json!({"error": e.to_string()})),
    }
}

async fn relay_bids(State(s): State<ApiState>, Query(q): Query<LimitQuery>) -> impl IntoResponse {
    let limit = q.limit.unwrap_or(50).clamp(1, 500);
    match s.engine.store.recent_relay_bids(limit) {
        Ok(rows) => Json(json!(rows)),
        Err(e) => Json(json!({"error": e.to_string()})),
    }
}

async fn relay_blocks(State(s): State<ApiState>, Query(q): Query<LimitQuery>) -> impl IntoResponse {
    let limit = q.limit.unwrap_or(50).clamp(1, 500);
    match s.engine.store.recent_relay_blocks(limit) {
        Ok(rows) => Json(json!(rows)),
        Err(e) => Json(json!({"error": e.to_string()})),
    }
}

#[derive(Deserialize)]
struct RelayTxsQuery {
    limit: Option<i64>,
    #[serde(rename = "blockNumber")]
    block_number: Option<u64>,
}

async fn relay_txs(State(s): State<ApiState>, Query(q): Query<RelayTxsQuery>) -> impl IntoResponse {
    let limit = q.limit.unwrap_or(500).clamp(1, 2_000);
    match s.engine.store.relay_block_txs(q.block_number, limit) {
        Ok(rows) => Json(json!(rows)),
        Err(e) => Json(json!({"error": e.to_string()})),
    }
}

/// Per-strategy funnel counters. See `engine::FunnelCounters` for the
/// semantics of each field. The dashboard consumes this to draw the
/// "where did my opportunities die?" panel.
async fn funnel(State(s): State<ApiState>) -> impl IntoResponse {
    Json(s.engine.stats.snapshot())
}

async fn latency(State(s): State<ApiState>) -> impl IntoResponse {
    Json(s.engine.latency.snapshot())
}

async fn competition(State(s): State<ApiState>, Query(q): Query<LimitQuery>) -> impl IntoResponse {
    let limit = q.limit.unwrap_or(50).clamp(1, 500);
    let summary = s.engine.store.competition_summary().unwrap_or_else(|_| json!({}));
    let rows = s
        .engine
        .store
        .recent_reconciliations(limit)
        .unwrap_or_default();
    Json(json!({"summary": summary, "recent": rows}))
}

async fn reorgs(State(s): State<ApiState>, Query(q): Query<LimitQuery>) -> impl IntoResponse {
    let limit = q.limit.unwrap_or(20).clamp(1, 200);
    match s.engine.store.recent_reorgs(limit) {
        Ok(rows) => Json(json!(rows)),
        Err(e) => Json(json!({"error": e.to_string()})),
    }
}

fn parse_strategy(s: &str) -> Option<Strategy> {
    match s {
        "sandwich" => Some(Strategy::Sandwich),
        "sandwich_v3" => Some(Strategy::SandwichV3),
        "jit" => Some(Strategy::Jit),
        "atomic_arb" => Some(Strategy::AtomicArb),
        "liquidation" => Some(Strategy::Liquidation),
        "sniper" => Some(Strategy::Sniper),
        _ => None,
    }
}

/// Live feed: blocks, mempool transactions, MEV-Share hints, opportunities,
/// simulations, bundles and relay bids, as they happen.
async fn stream(State(s): State<ApiState>) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let rx = s.engine.feed.subscribe();
    let stream = futures_util::stream::unfold(rx, |mut rx| async move {
        loop {
            match rx.recv().await {
                Ok(ev) => {
                    let data = serde_json::to_string(&ev).unwrap_or_else(|_| "{}".into());
                    return Some((Ok(Event::default().data(data)), rx));
                }
                // Slow consumer: skip what we missed rather than disconnecting.
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
            }
        }
    });
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}

pub async fn serve(engine: Arc<Engine>, bind: &str) -> anyhow::Result<()> {
    let app = router(engine);
    let listener = tokio::net::TcpListener::bind(bind).await?;
    tracing::info!(target: "api", "listening on http://{bind}");
    axum::serve(listener, app).await?;
    Ok(())
}
