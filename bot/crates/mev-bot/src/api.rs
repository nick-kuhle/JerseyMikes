//! HTTP API consumed by the dashboard.
//!
//! Plain REST for history/aggregates plus one SSE stream for everything live.
//! CORS is wide open because the API is expected to run behind the operator's
//! own network boundary and be proxied by the Next.js app.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use alloy_primitives::{Address, U256};
use alloy_sol_types::SolCall;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::http::{header, HeaderValue, Method};
use axum::middleware;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::stream::Stream;
use serde::Deserialize;
use serde_json::json;
use tower_http::cors::CorsLayer;

use crate::engine::Engine;
use crate::risk::RiskPatch;
use crate::types::Strategy;

#[derive(Clone)]
pub struct ApiState {
    pub engine: Arc<Engine>,
}

pub fn router(engine: Arc<Engine>) -> Router {
    let cfg = engine.cfg.clone();
    let state = ApiState { engine };

    // Mutating endpoints, split out so an auth layer can be applied to them
    // alone. Reads stay open: they are already public information for anyone
    // who can see the dashboard, and gating them would break the demo flow.
    let mutating = Router::new()
        .route("/api/mode", post(set_mode))
        .route("/api/risk", post(set_risk))
        .route("/api/risk/reset", post(reset_risk))
        .route("/api/qualification", post(set_qualification))
        // The sniper lane's mutating surface. Grouped with the other mutating
        // routes so it inherits the same bearer-token gate: `sniper/params`
        // can commit real capital, and `sniper/halt` can stop it.
        .route("/api/sniper/params", post(set_sniper_params))
        .route("/api/sniper/halt", post(halt_sniper))
        .route("/api/sniper/resume", post(resume_sniper))
        .route("/api/sniper/buy", post(manual_sniper_buy))
        .route("/api/sniper/sell", post(manual_sniper_sell))
        .route("/api/sniper/trade", post(manual_sniper_trade))
        .route("/api/sniper/paper/reset", post(reset_paper))
        // The sniper's own mode switch — independent of /api/mode — and the
        // simulation fixture lifecycle. Both mutate lane state, so both sit
        // behind the bearer gate.
        .route("/api/sniper/mode", post(set_sniper_mode))
        .route(
            "/api/sniper/sim-fixture/init",
            post(init_sniper_sim_fixture),
        )
        .layer(middleware::from_fn_with_state(state.clone(), require_auth));

    // Browsers get no cross-origin access by default. The dashboard reaches
    // the bot server-side through its own `/api/bot/*` proxy, so nothing
    // legitimate needs `Access-Control-Allow-Origin: *` — and with it, any
    // page the operator visited could POST to the risk endpoints.
    let cors = if cfg.api.allowed_origins.is_empty() {
        CorsLayer::new()
    } else {
        let origins: Vec<HeaderValue> = cfg
            .api
            .allowed_origins
            .iter()
            .filter_map(|o| o.parse::<HeaderValue>().ok())
            .collect();
        CorsLayer::new()
            .allow_origin(origins)
            .allow_methods([Method::GET, Method::POST])
            .allow_headers([header::CONTENT_TYPE, header::AUTHORIZATION])
    };

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
        .route("/api/actual-mev", get(actual_mev))
        .route("/api/executions", get(executions))
        .route("/api/qualification", get(qualification))
        .route("/api/preflight", get(preflight))
        .route("/api/reorgs", get(reorgs))
        .route("/api/stream", get(stream))
        .route("/api/mode", get(mode))
        .route("/api/risk", get(risk_state))
        .route("/api/alerts", get(alerts))
        .route("/api/metrics", get(metrics))
        // Sniper lane reads.
        .route("/api/sniper/portfolio", get(sniper_portfolio))
        .route("/api/sniper/params", get(sniper_params))
        .route("/api/sniper/positions", get(sniper_positions))
        .route("/api/sniper/vault", get(sniper_vault))
        .route("/api/sniper/mode", get(sniper_mode))
        .route("/api/sniper/sim-fixture", get(sniper_sim_fixture_status))
        .merge(mutating)
        .layer(cors)
        .with_state(state)
}

/// Bearer-token gate for the mutating endpoints.
///
/// A no-op when `API_AUTH_TOKEN` is unset — which `Config::validate` only
/// permits when the API is bound to loopback, so the open case is never
/// reachable from off-box. Comparison is constant-time: these tokens are
/// low-entropy shared secrets and an early-exit `==` leaks their prefix to a
/// patient attacker.
async fn require_auth(
    State(s): State<ApiState>,
    req: axum::extract::Request,
    next: middleware::Next,
) -> Response {
    let Some(expected) = s.engine.cfg.api.auth_token.as_deref() else {
        return next.run(req).await;
    };
    let presented = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    if constant_time_eq(presented.as_bytes(), expected.as_bytes()) {
        return next.run(req).await;
    }
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({
            "ok": false,
            "error": "missing or invalid bearer token",
            "hint": "send `Authorization: Bearer <API_AUTH_TOKEN>`",
        })),
    )
        .into_response()
}

/// Length-independent byte comparison, so neither the token's length nor its
/// matching prefix is observable through response timing.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        // Still fold over the longer input so the mismatch is not a fast path.
        let mut sink = 0u8;
        for byte in a.iter().chain(b.iter()) {
            sink |= *byte;
        }
        std::hint::black_box(sink);
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
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
        "broadcastEnabled": e.cfg.broadcast_enabled,
        "qualification": e.qualification_status(),
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
        "liveSmoke": live_smoke(e),
        // Persistence queue health: a rising `dropped` means the writer
        // cannot keep up and telemetry rows are being shed to protect the
        // hot path.
        "persistence": {
            "queued": e.writes.queued(),
            "dropped": e.writes.dropped(),
        },
        "latency": e.latency.snapshot(),
    }))
}

fn live_smoke(e: &Engine) -> serde_json::Value {
    let used = e.store.smoke_used().unwrap_or(0);
    let max = e.cfg.live_smoke_max;
    json!({
        "max": max,
        "used": used,
        "remaining": crate::config::smoke_remaining(used, max),
        "gasAtRiskWei": e.store.smoke_gas_at_risk_wei().unwrap_or(U256::MAX).to_string(),
        "maxGasCostWei": e.cfg.live_smoke_max_gas_cost_wei.to_string(),
    })
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
        "sniperSearcher": format!("{:?}", e.cfg.endpoints.sniper_searcher_address),
        "sniperSearcherKeyConfigured": e.cfg.endpoints.sniper_searcher_private_key.is_some(),
        "liveExecution": e.mode.live(),
        "liveArmed": e.mode.armed(),
        "broadcastEnabled": e.cfg.broadcast_enabled,
        "qualification": e.qualification_status(),
        "strategyEligibility": Strategy::all().iter().map(|strategy| json!({
            "name": strategy.as_str(),
            "liveCandidate": strategy.live_candidate(),
            "shadowOnlyReason": strategy.shadow_only_reason(),
        })).collect::<Vec<_>>(),
        "endpoints": {
            "ws": e.cfg.endpoints.ws_url.is_some(),
            "mevShare": !e.cfg.endpoints.mev_share_sse.is_empty(),
            "relays": e.cfg.endpoints.relay_data_urls.len(),
            "sequencerFeed": e.cfg.endpoints.sequencer_feed.is_some(),
            "externalMempools": e.cfg.endpoints.extra_mempool_ws.len(),
            "flashblocks": e.cfg.endpoints.flashblocks_ws.is_some(),
            "chainBlockIngest": e.cfg.chain_block_ingest,
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
            let total = rows.iter().fold(0i128, |sum, row| {
                sum.saturating_add(row.net_profit_wei.parse::<i128>().unwrap_or(0))
            });
            Json(json!({"byStrategy": rows, "totalNetWei": total.to_string()}))
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

async fn qualification(State(s): State<ApiState>) -> impl IntoResponse {
    Json(json!({
        "status": s.engine.qualification_status(),
        "broadcastEnabled": s.engine.cfg.broadcast_enabled,
        "runtimeLive": s.engine.mode.live(),
        "armed": s.engine.mode.armed(),
        "operatorSoakHours": s.engine.qualification_hours(),
    }))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct QualificationPatch {
    required_hours: u64,
}

/// Update the operator-selected soak threshold. This is intentionally a
/// control-plane write: the value changes the evidence window, never the
/// evidence itself, and the normal per-strategy qualification gates remain
/// mandatory.
async fn set_qualification(
    State(s): State<ApiState>,
    Json(patch): Json<QualificationPatch>,
) -> Response {
    match s.engine.set_qualification_hours(patch.required_hours) {
        Ok(()) => (
            StatusCode::OK,
            Json(json!({
                "ok": true,
                "operatorSoakHours": s.engine.qualification_hours(),
                "status": s.engine.qualification_status(),
            })),
        )
            .into_response(),
        Err(error) => (
            StatusCode::BAD_REQUEST,
            Json(json!({"ok": false, "error": error})),
        )
            .into_response(),
    }
}

/// Live go-live checks. This endpoint reports reachability and configuration,
/// never secrets. Relay URLs are probed server-side because builder RPCs are
/// not reachable from a browser-hosted console and many reject browser CORS.
async fn preflight(State(s): State<ApiState>) -> impl IntoResponse {
    let e = &s.engine;
    let (rpc_ok, rpc_chain_id) = match e.http.call_raw("eth_chainId", json!([])).await {
        Ok(value) => {
            let chain_id = value
                .as_str()
                .and_then(|raw| u64::from_str_radix(raw.trim_start_matches("0x"), 16).ok());
            (chain_id == Some(e.cfg.chain.chain_id), chain_id)
        }
        Err(_) => (false, None),
    };

    let relay_required = e.cfg.submission_mode == crate::config::SubmissionMode::Bundle;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .ok();
    let relay_checks = if let Some(client) = client {
        let checks =
            futures_util::future::join_all(e.cfg.endpoints.bundle_relay_urls.iter().cloned().map(
                |url| {
                    let client = client.clone();
                    async move {
                        let ok = client
                            .get(&url)
                            .send()
                            .await
                            .map(|response| response.status().as_u16() < 500)
                            .unwrap_or(false);
                        json!({"url": url, "reachable": ok})
                    }
                },
            ))
            .await;
        checks
    } else {
        Vec::new()
    };
    let relay_ok = !relay_required
        || relay_checks
            .iter()
            .any(|row| row.get("reachable").and_then(serde_json::Value::as_bool) == Some(true));
    let qualification = e.qualification_status();
    Json(json!({
        "rpc": rpc_ok,
        "rpcChainId": rpc_chain_id,
        "expectedChainId": e.cfg.chain.chain_id,
        "relayRequired": relay_required,
        "relay": relay_ok,
        "relayChecks": relay_checks,
        "wsConfigured": e.cfg.endpoints.ws_url.is_some(),
        "sequencerFeedConfigured": e.cfg.endpoints.sequencer_feed.is_some(),
        "flashblocksConfigured": e.cfg.endpoints.flashblocks_ws.is_some(),
        "chainBlockIngest": e.cfg.chain_block_ingest,
        "qualification": qualification,
        "liveArmed": e.mode.armed(),
        "broadcastEnabled": e.cfg.broadcast_enabled,
    }))
}

async fn actual_mev(State(s): State<ApiState>, Query(q): Query<LimitQuery>) -> impl IntoResponse {
    let limit = q.limit.unwrap_or(100).clamp(1, 1_000);
    let matches = s
        .engine
        .store
        .recent_actual_mev_matches(limit)
        .unwrap_or_default();
    let summary = s
        .engine
        .store
        .actual_mev_summary()
        .unwrap_or_else(|_| json!({"matches": 0, "highConfidence": 0}));
    Json(json!({"summary": summary, "matches": matches}))
}

async fn executions(State(s): State<ApiState>, Query(q): Query<LimitQuery>) -> impl IntoResponse {
    let limit = q.limit.unwrap_or(100).clamp(1, 1_000);
    match s.engine.store.recent_execution_outcomes(limit) {
        Ok(rows) => Json(json!({"executions": rows, "finalityDepth": s.engine.cfg.finality_depth})),
        Err(error) => Json(json!({"error": error.to_string()})),
    }
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
    match s.engine.set_runtime_mode(body.live).await {
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
    match e.apply_runtime_risk(patch).await {
        Ok(()) => {
            tracing::info!(target: "api", "risk envelope updated atomically; active private bundles cancelled");
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

// ---------------------------------------------------------------------------
// Directional sniper lane
//
// A completely separate surface from `/api/risk`. Merging them was tempting
// and would have been wrong: the shared risk envelope governs bundles that
// cannot lose money, and this one governs a lane that can. Keeping the
// endpoints apart keeps the blast radius of a mistaken PATCH apart too.
// ---------------------------------------------------------------------------

/// The mini portfolio: open positions, marks, realised/unrealised PnL split,
/// and the reasons the lane is or is not armed.
async fn sniper_portfolio(
    State(s): State<ApiState>,
    Query(q): Query<LimitQuery>,
) -> impl IntoResponse {
    let limit = q.limit.unwrap_or(20).clamp(1, 200) as usize;
    Json(s.engine.sniper.portfolio(crate::types::now_ms(), limit))
}

/// Current envelope plus everything an operator needs to understand it.
async fn sniper_params(State(s): State<ApiState>) -> impl IntoResponse {
    let lane = &s.engine.sniper;
    let params = lane.params();
    let vaults = sniper_vault_addresses(&s);
    Json(json!({
        "params": params,
        "armed": lane.effective_armed(),
        "bootEnabled": lane.boot_enabled(),
        "halted": lane.is_halted(),
        "haltReason": lane.halt_reason(),
        "paperMode": lane.paper_mode(),
        "sniperMode": lane.mode().as_str(),
        "sniperLiveBootEnabled": lane.live_boot_enabled(),
        "simulationBalanceWei": lane.paper_balance_wei().to_string(),
        // The two vault addresses are never interchangeable: the simulation
        // fixture is local-anvil-only, the production vault is on the
        // selected chain.
        "simulationVaultAddress": vaults.simulation,
        "productionVaultAddress": vaults.production,
        "activeVaultKind": vaults.active_kind,
        "armingBlockers": lane.effective_arming_blockers(),
        "rejections": lane.rejection_counts(),
        "envSnippet": params.env_snippet(),
    }))
}

/// Resolved vault addresses for the console payloads.
struct VaultAddresses {
    simulation: Option<String>,
    production: Option<String>,
    active_kind: &'static str,
}

fn sniper_vault_addresses(s: &ApiState) -> VaultAddresses {
    let lane = &s.engine.sniper;
    let params = lane.params();
    let simulation = s
        .engine
        .sniper_execution
        .fixture()
        .and_then(|f| f.state())
        .map(|st| format!("{:?}", st.vault));
    let production = params
        .vault_address
        .filter(|a| !a.is_zero())
        .map(|a| format!("{a:?}"));
    let active_kind = if lane.paper_mode() {
        if simulation.is_some() {
            "simulation_fixture"
        } else {
            "none"
        }
    } else if production.is_some() {
        "production"
    } else {
        "none"
    };
    VaultAddresses {
        simulation,
        production,
        active_kind,
    }
}

/// Positions with their fill history, for the drill-down view.
async fn sniper_positions(
    State(s): State<ApiState>,
    Query(q): Query<LimitQuery>,
) -> impl IntoResponse {
    let limit = q.limit.unwrap_or(50).clamp(1, 500) as usize;
    let positions = s
        .engine
        .store
        .recent_sniper_positions(limit)
        .unwrap_or_default();
    let rows: Vec<serde_json::Value> = positions
        .into_iter()
        .map(|p| {
            let fills = s.engine.store.sniper_fills(&p.id).unwrap_or_default();
            json!({"position": p, "fills": fills})
        })
        .collect();
    Json(json!({"positions": rows}))
}

/// Update the sniper's envelope. Validated as a whole; a rejected patch
/// changes nothing.
async fn set_sniper_params(
    State(s): State<ApiState>,
    Json(patch): Json<crate::sniper::SniperParamsPatch>,
) -> Response {
    let lane = &s.engine.sniper;
    match lane.patch_params(&patch) {
        Ok(effective) => {
            if lane.effective_armed() {
                tracing::warn!(
                    target: "sniper",
                    buy_size_wei = %effective.buy_size_wei,
                    daily_budget_wei = %effective.daily_budget_wei,
                    "sniper envelope updated and the lane is ARMED"
                );
            } else {
                tracing::info!(target: "sniper", "sniper envelope updated; lane not armed");
            }
            (
                StatusCode::OK,
                Json(json!({
                    "ok": true,
                    "params": effective,
                    "armed": lane.effective_armed(),
                    "paperMode": lane.paper_mode(),
                    "simulationBalanceWei": lane.paper_balance_wei().to_string(),
                    "armingBlockers": lane.effective_arming_blockers(),
                    "envSnippet": effective.env_snippet(),
                })),
            )
                .into_response()
        }
        Err(errors) => (
            StatusCode::BAD_REQUEST,
            Json(json!({"ok": false, "errors": errors})),
        )
            .into_response(),
    }
}

/// Stop opening new positions. Existing positions keep being managed — an
/// operator halting the lane wants to stop buying, not to be trapped in what
/// they already hold.
async fn reset_paper(State(s): State<ApiState>) -> Response {
    // A simulation reset must never operate on a live-armed ledger.
    if !s.engine.sniper.paper_mode() {
        return (
            StatusCode::CONFLICT,
            Json(json!({"ok": false, "error": "paper funds cannot be reset while the sniper runs in live mode"})),
        )
            .into_response();
    }
    s.engine.sniper.reset_paper();
    let reset_at = crate::types::now_ms();
    if let Err(error) = s
        .engine
        .store
        .save_simulation_state(s.engine.sniper.paper_balance_wei(), reset_at)
    {
        tracing::error!(target: "sniper", %error, "failed to persist paper balance reset");
    }
    tracing::warn!(target: "sniper", "simulation paper balance reset to 1 ETH (history preserved)");
    (
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "paperMode": true,
            "simulationBalanceWei": s.engine.sniper.paper_balance_wei().to_string(),
            "resetAtMs": reset_at,
        })),
    )
        .into_response()
}

/// `GET /api/sniper/mode` — the sniper's independent execution mode.
///
/// Shape follows the work order: atomic and sniper modes side by side, the
/// boot ceiling, the live-switch blockers, and the vault addresses kept
/// strictly apart.
async fn sniper_mode(State(s): State<ApiState>) -> impl IntoResponse {
    let lane = &s.engine.sniper;
    let key_configured = s.engine.cfg.endpoints.sniper_searcher_private_key.is_some();
    let blockers = lane.live_switch_blockers(key_configured);
    let vaults = sniper_vault_addresses(&s);
    let fixture_state = s
        .engine
        .sniper_execution
        .fixture()
        .as_ref()
        .and_then(|f| f.state());
    let active_address = if lane.paper_mode() {
        vaults.simulation.clone()
    } else {
        vaults.production.clone()
    };
    Json(json!({
        "atomicMode": if s.engine.mode.live() { "live" } else { "simulation" },
        "sniperMode": lane.mode().as_str(),
        "sniperLiveBootEnabled": lane.live_boot_enabled(),
        "canSwitchLive": blockers.is_empty(),
        "blockers": blockers,
        "simulationVaultAddress": vaults.simulation,
        "productionVaultAddress": vaults.production,
        "simulationBalanceWei": lane.paper_balance_wei().to_string(),
        "simulationChainId": s.engine.cfg.chain.chain_id,
        "activeVault": {
            "kind": vaults.active_kind,
            "address": active_address,
        },
        "fixture": {
            "available": s.engine.sniper_execution.fixture().is_some(),
            "deployed": fixture_state.is_some(),
            "searcher": fixture_state.as_ref().map(|st| format!("{:?}", st.searcher)),
            "owner": fixture_state.as_ref().map(|st| format!("{:?}", st.owner)),
        },
    }))
}

#[derive(Deserialize)]
struct SniperModeRequest {
    /// "simulation" | "live" — the work-order contract.
    mode: Option<String>,
}

/// `POST /api/sniper/mode` — flip the sniper's independent mode.
///
/// `{"mode":"live"}` fails closed unless the boot ceiling, the production
/// vault, the dedicated sniper key and the budgets are all in place;
/// `{"mode":"simulation"}` always succeeds and immediately stops new live
/// entries. Open live positions stay tagged live and keep receiving live
/// exit management — they are never converted into paper.
async fn set_sniper_mode(
    State(s): State<ApiState>,
    Json(body): Json<SniperModeRequest>,
) -> Response {
    let Some(requested) = body
        .mode
        .as_deref()
        .and_then(crate::sniper::SniperMode::parse)
    else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "ok": false,
                "error": "mode must be simulation or live",
            })),
        )
            .into_response();
    };
    let key_configured = s.engine.cfg.endpoints.sniper_searcher_private_key.is_some();
    let lane = &s.engine.sniper;
    let previous = lane.mode();
    match lane.set_mode(requested, key_configured) {
        Ok(mode) => {
            if previous != mode {
                tracing::warn!(
                    target: "sniper",
                    from = %previous,
                    to = %mode,
                    "sniper execution mode switched"
                );
            }
            let open_live = if mode == crate::sniper::SniperMode::Simulation {
                lane.live_positions()
                    .iter()
                    .filter(|p| p.execution_mode == crate::sniper::ExecutionMode::Live)
                    .count()
            } else {
                0
            };
            (
                StatusCode::OK,
                Json(json!({
                    "ok": true,
                    "sniperMode": mode.as_str(),
                    "atomicMode": if s.engine.mode.live() { "live" } else { "simulation" },
                    "sniperLiveBootEnabled": lane.live_boot_enabled(),
                    // Live positions survive the switch to simulation tagged
                    // live; the operator must flatten them explicitly before
                    // any migration or handoff.
                    "openLivePositionsRetained": open_live,
                    "note": if mode == crate::sniper::SniperMode::Simulation && open_live > 0 {
                        "live positions remain live data and keep live exit management;                          they are not converted into paper positions"
                    } else {
                        ""
                    },
                })),
            )
                .into_response()
        }
        Err(blockers) => (
            StatusCode::CONFLICT,
            Json(json!({
                "ok": false,
                "sniperMode": lane.mode().as_str(),
                "blockers": blockers,
                "error": "switch to live refused — every gate must clear first",
            })),
        )
            .into_response(),
    }
}

/// `GET /api/sniper/sim-fixture` — fixture status for the wizard.
async fn sniper_sim_fixture_status(State(s): State<ApiState>) -> Response {
    let Some(fixture) = s.engine.sniper_execution.fixture() else {
        return (
            StatusCode::OK,
            Json(json!({
                "ready": false,
                "blocker": "simulation unavailable: local fork is not running — the bot is observation-only",
            })),
        )
            .into_response();
    };
    if fixture.state().is_none() {
        return (
            StatusCode::OK,
            Json(json!({
                "ready": false,
                "blocker": "fixture not deployed yet — use “Initialize simulation fixture”",
            })),
        )
            .into_response();
    }
    match fixture.vault_status().await {
        Ok(status) => (StatusCode::OK, Json(status)).into_response(),
        Err(error) => (
            StatusCode::OK,
            Json(json!({
                "ready": false,
                "blocker": format!("fixture status unreadable: {error}"),
            })),
        )
            .into_response(),
    }
}

/// `POST /api/sniper/sim-fixture/init` — deploy/verify the local fixture.
/// Purely local: no real funds, no live signer, no production RPC write.
async fn init_sniper_sim_fixture(State(s): State<ApiState>) -> Response {
    let Some(fixture) = s.engine.sniper_execution.fixture() else {
        return (
            StatusCode::CONFLICT,
            Json(json!({
                "ok": false,
                "error": "simulation fixture unavailable: local fork is not running",
                "hint": "start the bot with a reachable HTTP RPC so the anvil fork can spawn",
            })),
        )
            .into_response();
    };
    match fixture.ensure_deployed().await {
        Ok(_) => match fixture.vault_status().await {
            Ok(status) => (
                StatusCode::OK,
                Json(json!({
                    "ok": true,
                    "message": "Simulation vault ready · local Anvil fixture · no deployment required",
                    "fixture": status,
                })),
            )
                .into_response(),
            Err(error) => (
                StatusCode::BAD_GATEWAY,
                Json(json!({"ok": false, "error": error.to_string()})),
            )
                .into_response(),
        },
        Err(error) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({
                "ok": false,
                "error": format!("simulation fixture deployment failed: {error}"),
            })),
        )
            .into_response(),
    }
}

async fn halt_sniper(State(s): State<ApiState>, Json(body): Json<HaltBody>) -> impl IntoResponse {
    let reason = body
        .reason
        .filter(|r| !r.trim().is_empty())
        .unwrap_or_else(|| "halted from the dashboard".to_string());
    s.engine.sniper.halt(reason.clone());
    tracing::warn!(target: "sniper", %reason, "sniper lane halted");
    Json(json!({"ok": true, "halted": true, "reason": reason}))
}

async fn resume_sniper(State(s): State<ApiState>) -> impl IntoResponse {
    let lane = &s.engine.sniper;
    let was = lane.halt_reason();
    lane.resume();
    tracing::warn!(target: "sniper", previous = ?was, "sniper lane resumed");
    Json(json!({
        "ok": true,
        "halted": false,
        "previousReason": was,
        "armed": lane.effective_armed(),
        "armingBlockers": lane.effective_arming_blockers(),
    }))
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct HaltBody {
    reason: Option<String>,
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

/// Active alerts + transition history (see `alerts.rs` for the rules).
async fn alerts(State(s): State<ApiState>) -> impl IntoResponse {
    let e = &s.engine;
    Json(json!({
        "active": e.alerts.active(),
        "recent": e.alerts.history(),
        "evalSecs": e.cfg.alerts.eval_secs,
    }))
}

/// Prometheus text exposition of everything the console already shows.
async fn metrics(State(s): State<ApiState>) -> Response {
    let e = &s.engine;
    let head = e.ctx.head();
    let risk = e.runtime.risk();
    let status = json!({
        "up_ms": now_ms().saturating_sub(e.stats.started_at_ms.load(std::sync::atomic::Ordering::Relaxed)),
        "live": e.mode.live(),
        "live_armed": e.mode.armed(),
        "head_number": head.number,
        "head_base_fee_wei": head.base_fee_per_gas.to_string(),
        "pools": e.ctx.pools.len(),
        "pools_v3": e.ctx.pools_v3.len(),
        "kill_switch_tripped": e.risk.is_tripped(),
        "cumulative_net_wei": e.risk.cumulative_net().to_string(),
        "risk": {
            "min_net_profit_wei": risk.min_net_profit_wei.to_string(),
            "max_position_wei": risk.max_position_wei.to_string(),
            "max_base_fee_wei": risk.max_base_fee_wei.to_string(),
            "max_drawdown_wei": risk.max_drawdown_wei.to_string(),
            "bribe_bps": risk.bribe_bps,
            "max_gas_per_bundle": risk.max_gas_per_bundle,
        },
        "stats": e.stats.snapshot(),
        "latency": e.latency.snapshot(),
        "inventory": e.inventory.snapshot(),
        "liveSmoke": live_smoke(e),
        // Persistence queue health: a rising `dropped` means the writer
        // cannot keep up and telemetry rows are being shed to protect the
        // hot path.
        "persistence": {
            "queued": e.writes.queued(),
            "dropped": e.writes.dropped(),
        },
        "alerts_active": e.alerts.active().len(),
    });
    let mut body = crate::metrics::render(&status, "mev");
    let live = serde_json::Value::Object(crate::engine::Stats::funnel_json(&e.stats.funnel));
    let replay =
        serde_json::Value::Object(crate::engine::Stats::funnel_json(&e.stats.funnel_replay));
    body.push_str(&crate::metrics::render_funnel(&live, "mev_funnel", "live"));
    body.push_str(&crate::metrics::render_funnel(
        &replay,
        "mev_funnel",
        "replay",
    ));
    (
        StatusCode::OK,
        [("content-type", "text/plain; version=0.0.4; charset=utf-8")],
        body,
    )
        .into_response()
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

async fn sniper_vault(State(s): State<ApiState>) -> impl IntoResponse {
    let params = s.engine.sniper.params();
    let Some(vault_addr) = params.vault_address else {
        return Json(json!({
            "configured": false,
            "address": null,
            "spendableRemainingWei": "0",
            "dailyBudgetWei": "0",
            "totalBudgetWei": "0",
            "windowResetTimeSecs": 0
        }));
    };
    if vault_addr == Address::ZERO {
        return Json(json!({
            "configured": false,
            "address": null,
            "spendableRemainingWei": "0",
            "dailyBudgetWei": "0",
            "totalBudgetWei": "0",
            "windowResetTimeSecs": 0
        }));
    }

    let call_spendable =
        crate::sniper::calldata::ISniperVault::spendableRemainingCall {}.abi_encode();
    let call_daily = crate::sniper::calldata::ISniperVault::dailyBudgetCall {}.abi_encode();
    let call_total = crate::sniper::calldata::ISniperVault::totalBudgetCall {}.abi_encode();
    let call_window = crate::sniper::calldata::ISniperVault::windowStartCall {}.abi_encode();

    let head = s.engine.ctx.head().number;
    let spendable = match s
        .engine
        .http
        .eth_call(vault_addr, call_spendable, head)
        .await
    {
        Ok(b) => crate::sniper::calldata::ISniperVault::spendableRemainingCall::abi_decode_returns(
            &b, true,
        )
        .map(|v| v._0)
        .unwrap_or(U256::ZERO),
        Err(_) => U256::ZERO,
    };
    let daily = match s.engine.http.eth_call(vault_addr, call_daily, head).await {
        Ok(b) => {
            crate::sniper::calldata::ISniperVault::dailyBudgetCall::abi_decode_returns(&b, true)
                .map(|v| v._0)
                .unwrap_or(U256::ZERO)
        }
        Err(_) => U256::ZERO,
    };
    let total = match s.engine.http.eth_call(vault_addr, call_total, head).await {
        Ok(b) => {
            crate::sniper::calldata::ISniperVault::totalBudgetCall::abi_decode_returns(&b, true)
                .map(|v| v._0)
                .unwrap_or(U256::ZERO)
        }
        Err(_) => U256::ZERO,
    };
    let window_start = match s.engine.http.eth_call(vault_addr, call_window, head).await {
        Ok(b) => {
            crate::sniper::calldata::ISniperVault::windowStartCall::abi_decode_returns(&b, true)
                .map(|v| v._0.to::<u64>())
                .unwrap_or(0)
        }
        Err(_) => 0,
    };

    let reset_time = window_start.saturating_add(86_400);

    Json(json!({
        "configured": true,
        "address": vault_addr,
        "spendableRemainingWei": spendable.to_string(),
        "dailyBudgetWei": daily.to_string(),
        "totalBudgetWei": total.to_string(),
        "windowResetTimeSecs": reset_time
    }))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ManualTradePayload {
    side: String,
    token: Option<String>,
    pair: Option<String>,
    amount_wei: Option<String>,
    position_id: Option<String>,
    sell_fraction_bps: Option<u32>,
}

/// Strict bot-signer terminal endpoint. It intentionally accepts a normalized
/// trade intent rather than arbitrary calldata; the bot chooses the bounded
/// SniperVault path and validates the V2 pair/position before signing.
async fn manual_sniper_trade(
    State(s): State<ApiState>,
    Json(p): Json<ManualTradePayload>,
) -> Response {
    let head = s.engine.ctx.head();
    match p.side.to_ascii_lowercase().as_str() {
        "buy" => {
            let (Some(token), Some(pair), Some(amount_wei)) = (p.token, p.pair, p.amount_wei)
            else {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"ok": false, "error": "buy requires token, pair and amountWei"})),
                )
                    .into_response();
            };
            let Ok(token) = token.parse::<Address>() else {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"ok": false, "error": "invalid token address"})),
                )
                    .into_response();
            };
            let Ok(pair) = pair.parse::<Address>() else {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"ok": false, "error": "invalid pair address"})),
                )
                    .into_response();
            };
            let Ok(amount) = amount_wei.parse::<U256>() else {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"ok": false, "error": "amountWei must be decimal wei"})),
                )
                    .into_response();
            };
            match s
                .engine
                .sniper_execution
                .process_manual_buy(
                    token,
                    pair,
                    s.engine.cfg.chain.weth,
                    amount,
                    s.engine.cfg.chain.chain_id,
                    head.number,
                    head.base_fee_per_gas,
                    crate::types::now_ms(),
                )
                .await
            {
                Ok(Some(position)) => (
                    StatusCode::OK,
                    Json(json!({"ok": true, "position": position})),
                )
                    .into_response(),
                Ok(None) => (
                    StatusCode::CONFLICT,
                    Json(json!({"ok": false, "error": "admission rejected manual buy"})),
                )
                    .into_response(),
                Err(error) => (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"ok": false, "error": error.to_string()})),
                )
                    .into_response(),
            }
        }
        "sell" => {
            let Some(id) = p.position_id else {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"ok": false, "error": "sell requires positionId"})),
                )
                    .into_response();
            };
            match s
                .engine
                .sniper_execution
                .process_manual_sell(
                    &id,
                    p.sell_fraction_bps.unwrap_or(10_000),
                    s.engine.cfg.chain.weth,
                    head.number,
                    head.base_fee_per_gas,
                    crate::types::now_ms(),
                )
                .await
            {
                Ok(Some((position, tx_hash))) => (
                    StatusCode::OK,
                    Json(json!({"ok": true, "position": position, "txHash": tx_hash})),
                )
                    .into_response(),
                Ok(None) => (
                    StatusCode::NOT_FOUND,
                    Json(json!({"ok": false, "error": "position not found or not live"})),
                )
                    .into_response(),
                Err(error) => (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"ok": false, "error": error.to_string()})),
                )
                    .into_response(),
            }
        }
        _ => (
            StatusCode::BAD_REQUEST,
            Json(json!({"ok": false, "error": "side must be buy or sell"})),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ManualBuyPayload {
    token: String,
    pair: Option<String>,
    size_wei: String,
}

async fn manual_sniper_buy(
    State(s): State<ApiState>,
    Json(p): Json<ManualBuyPayload>,
) -> impl IntoResponse {
    let Ok(token) = p.token.parse::<Address>() else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"ok": false, "error": "invalid token address"})),
        )
            .into_response();
    };
    let Ok(size_wei) = p.size_wei.parse::<U256>() else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"ok": false, "error": "invalid sizeWei"})),
        )
            .into_response();
    };
    let pair = p
        .pair
        .and_then(|addr| addr.parse().ok())
        .unwrap_or(Address::ZERO);
    if pair == Address::ZERO || size_wei.is_zero() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"ok": false, "error": "manual buy requires a non-zero pair and sizeWei"})),
        )
            .into_response();
    }
    let head = s.engine.ctx.head();
    let now = crate::types::now_ms();
    match s
        .engine
        .sniper_execution
        .process_manual_buy(
            token,
            pair,
            s.engine.cfg.chain.weth,
            size_wei,
            s.engine.cfg.chain.chain_id,
            head.number,
            head.base_fee_per_gas,
            now,
        )
        .await
    {
        Ok(Some(position)) => (
            StatusCode::OK,
            Json(json!({"ok": true, "position": position, "manualProbeBypass": true})),
        )
            .into_response(),
        Ok(None) => (
            StatusCode::CONFLICT,
            Json(json!({"ok": false, "error": "token is already claimed or the admission gates rejected it"})),
        )
            .into_response(),
        Err(error) => (
            StatusCode::BAD_REQUEST,
            Json(json!({"ok": false, "error": error.to_string()})),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ManualSellPayload {
    id: String,
    sell_fraction_bps: Option<u32>,
}

async fn manual_sniper_sell(
    State(s): State<ApiState>,
    Json(p): Json<ManualSellPayload>,
) -> impl IntoResponse {
    let fraction = p.sell_fraction_bps.unwrap_or(10_000).min(10_000);
    let head = s.engine.ctx.head();
    match s
        .engine
        .sniper_execution
        .process_manual_sell(
            &p.id,
            fraction,
            s.engine.cfg.chain.weth,
            head.number,
            head.base_fee_per_gas,
            crate::types::now_ms(),
        )
        .await
    {
        Ok(Some((position, tx_hash))) => (
            StatusCode::OK,
            Json(json!({"ok": true, "position": position, "txHash": tx_hash})),
        )
            .into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({"ok": false, "error": "position not found or not live"})),
        )
            .into_response(),
        Err(error) => (
            StatusCode::BAD_REQUEST,
            Json(json!({"ok": false, "error": error.to_string()})),
        )
            .into_response(),
    }
}
