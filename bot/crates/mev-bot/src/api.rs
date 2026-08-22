//! HTTP API consumed by the dashboard.
//!
//! Plain REST for history/aggregates plus one SSE stream for everything live.
//! CORS is wide open because the API is expected to run behind the operator's
//! own network boundary and be proxied by the Next.js app.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::stream::Stream;
use serde::Deserialize;
use serde_json::json;
use tower_http::cors::{Any, CorsLayer};

use crate::engine::Engine;
use crate::risk::RiskPatch;
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
        .route("/api/mode", get(mode).post(set_mode))
        .route("/api/risk", get(risk_state).post(set_risk))
        .route("/api/risk/reset", post(reset_risk))
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
        "mode": if e.mode.live() { "live" } else { "simulation" },
        // Boot-time arming (`LIVE_EXECUTION=true` + `I_UNDERSTAND_LIVE_RISK=yes`).
        // The runtime switch can only narrow this, never widen it.
        "liveArmed": e.mode.armed(),
        // Runtime-effective enablement (already intersected with what was
        // constructed at boot); the boot set is in `bootStrategies` and
        // /api/config.
        "strategies": e.runtime.enabled_names(),
        "bootStrategies": crate::engine::enabled_strategies(&e.cfg),
        "risk": {
            "minNetProfitWei": e.runtime.risk().min_net_profit_wei.to_string(),
            "maxPositionWei": e.runtime.risk().max_position_wei.to_string(),
            "maxBaseFeeWei": e.runtime.risk().max_base_fee_wei.to_string(),
            "maxDrawdownWei": e.runtime.risk().max_drawdown_wei.to_string(),
            "bribeBps": e.runtime.risk().bribe_bps,
            "maxGasPerBundle": e.runtime.risk().max_gas_per_bundle,
            "maxInflightPerStrategy": e.runtime.risk().max_inflight_per_strategy,
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
        // The bot's signer EOA — prefills the go-live panel's setSearcher
        // step and the executor-allowlist check.
        "searcher": format!("{:?}", e.cfg.endpoints.searcher_address),
        "liveExecution": e.mode.live(),
        "liveArmed": e.mode.armed(),
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

async fn opportunities(
    State(s): State<ApiState>,
    Query(q): Query<LimitQuery>,
) -> impl IntoResponse {
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
    let summary = s
        .engine
        .store
        .competition_summary()
        .unwrap_or_else(|_| json!({}));
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
        "liquidation_compound" => Some(Strategy::LiquidationCompound),
        "liquidation_morpho" => Some(Strategy::LiquidationMorpho),
        "liquidation_maker" => Some(Strategy::LiquidationMaker),
        "oracle_frontrun" => Some(Strategy::OracleFrontrun),
        "sniper" => Some(Strategy::Sniper),
        _ => None,
    }
}

/// Read the execution mode: `{"mode": "simulation"|"live", "liveArmed": bool}`.
///
/// `liveArmed` is the boot-time two-key switch (`LIVE_EXECUTION=true` +
/// `I_UNDERSTAND_LIVE_RISK=yes`); `mode` is what the engine actually does
/// right now (armed && runtime switch).
async fn mode(State(s): State<ApiState>) -> impl IntoResponse {
    Json(json!({
        "mode": if s.engine.mode.live() { "live" } else { "simulation" },
        "liveArmed": s.engine.mode.armed(),
    }))
}

#[derive(Deserialize)]
struct ModeRequest {
    live: bool,
}

/// Flip the runtime simulation/live switch.
///
/// `POST /api/mode {"live": true}` arms nothing on its own: live execution
/// requires the process to have been started with both env keys set, and
/// that decision is read exactly once at boot. An unarmed process gets a
/// 409 with the restart instructions rather than a silent mode change.
async fn set_mode(State(s): State<ApiState>, Json(body): Json<ModeRequest>) -> Response {
    match s.engine.mode.set_live(body.live) {
        Ok(live) => Json(json!({
            "ok": true,
            "mode": if live { "live" } else { "simulation" },
            "liveArmed": s.engine.mode.armed(),
        }))
        .into_response(),
        Err(hint) => (
            StatusCode::CONFLICT,
            Json(json!({
                "ok": false,
                "error": "live execution is not armed for this process",
                "hint": hint,
                "mode": "simulation",
                "liveArmed": false,
            })),
        )
            .into_response(),
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

/// Effective + boot risk envelope and strategy enablement.
async fn risk_state(State(s): State<ApiState>) -> impl IntoResponse {
    let e = &s.engine;
    let rt = &e.runtime;
    let effective = rt.risk();
    let boot = e.cfg.risk.clone();
    let serialize = |r: crate::config::RiskConfig| {
        json!({
            "minNetProfitWei": r.min_net_profit_wei.to_string(),
            "maxPositionWei": r.max_position_wei.to_string(),
            "maxBaseFeeWei": r.max_base_fee_wei.to_string(),
            "maxDrawdownWei": r.max_drawdown_wei.to_string(),
            "bribeBps": r.bribe_bps,
            "maxGasPerBundle": r.max_gas_per_bundle,
            "maxInflightPerStrategy": r.max_inflight_per_strategy,
        })
    };
    Json(json!({
        "effective": serialize(effective),
        "boot": serialize(boot),
        // `{name, enabled, bootEnabled}`: runtime vs what was constructed.
        // A strategy can be re-enabled at runtime only if bootEnabled.
        "strategies": Strategy::all().iter().map(|s| json!({
            "name": s.as_str(),
            "enabled": rt.enabled(*s),
            "bootEnabled": crate::engine::enabled_strategies(&e.cfg).contains(&s.as_str()),
        })).collect::<Vec<_>>(),
        "killSwitch": {
            "tripped": e.risk.is_tripped(),
            "cumulativeNetWei": e.risk.cumulative_net().to_string(),
        },
    }))
}

/// Apply a partial risk update. Validation failures return 400 with the
/// human-readable reason and change nothing.
async fn set_risk(State(s): State<ApiState>, Json(patch): Json<RiskPatch>) -> Response {
    let e = &s.engine;
    match e.runtime.apply(patch) {
        Ok(()) => {
            tracing::info!(target: "api", "risk envelope updated at runtime");
            let effective = e.runtime.risk();
            (
                StatusCode::OK,
                Json(json!({
                    "ok": true,
                    "effective": {
                        "minNetProfitWei": effective.min_net_profit_wei.to_string(),
                        "maxPositionWei": effective.max_position_wei.to_string(),
                        "maxBaseFeeWei": effective.max_base_fee_wei.to_string(),
                        "maxDrawdownWei": effective.max_drawdown_wei.to_string(),
                        "bribeBps": effective.bribe_bps,
                        "maxGasPerBundle": effective.max_gas_per_bundle,
                        "maxInflightPerStrategy": effective.max_inflight_per_strategy,
                    },
                    "strategies": e.runtime.enabled_names(),
                })),
            )
                .into_response()
        }
        Err(reason) => (
            StatusCode::BAD_REQUEST,
            Json(json!({"ok": false, "error": reason})),
        )
            .into_response(),
    }
}

/// Clear the drawdown kill switch and zero the cumulative PnL it tracks.
/// Deliberately explicit: it re-arms a bot that stopped itself.
async fn reset_risk(State(s): State<ApiState>) -> impl IntoResponse {
    let e = &s.engine;
    let was = e.risk.is_tripped();
    e.risk.reset();
    tracing::warn!(target: "api", was_tripped = was, "kill switch reset from the dashboard");
    Json(json!({"ok": true, "wasTripped": was, "tripped": false}))
}
