//! WS-Q / work order 2.4 end-to-end: fixture frame → pinned candidate → TTL /
//! identity rechecks → `eth_simulateV1` state-pinned simulation (mock
//! provider, deterministic) → signed raw chain-8453 transaction with zero
//! foreign payload.
//!
//! The live fork-route half of the acceptance is the separate env-gated test
//! at the bottom (`BASE_FORK_RPC`), which the default suite never runs.

use std::sync::Arc;

use alloy_primitives::{Address, B256, U256};
use mev_bot::bundle::{build_bundle, BundleContext};
use mev_bot::config::{known, Config};
use mev_bot::dex::{AeroPool, Venue};
use mev_bot::flashblocks::{
    check_pin, raw_transportable, FlashblockParser, PinReject, PreconfirmedTracker,
};
use mev_bot::risk::RuntimeRisk;
use mev_bot::rpc::RpcClient;
use mev_bot::signer::Signer;
use mev_bot::sim::anvil::SIM_EXECUTOR;
use mev_bot::sim::pending::PendingSim;
use mev_bot::types::{now_ms, Call, Opportunity, Provenance, SimBackend, Strategy};
use serde_json::{json, Value};

// ── mock provider ───────────────────────────────────────────────────────────

const PRE_BALANCE: u128 = 1_000_000_000_000_000_000_000u128; // 1000 WETH
const POST_BALANCE: u128 = PRE_BALANCE + 500_000_000_000_000_000u128; // +0.5 WETH

fn word(v: u128) -> String {
    format!("0x{:064x}", v)
}

/// Spin up a JSON-RPC stub that answers `eth_simulateV1` with three canned
/// call results and records every request body for later inspection.
async fn spawn_mock_rpc() -> (String, Arc<tokio::sync::Mutex<Vec<Value>>>) {
    let seen: Arc<tokio::sync::Mutex<Vec<Value>>> = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let st = seen.clone();
    let app = axum::Router::new().route(
        "/",
        axum::routing::post(move |axum::extract::Json(req): axum::extract::Json<Value>| {
            let st = st.clone();
            async move {
                st.lock().await.push(req.clone());
                let id = req["id"].clone();
                let resp = match req["method"].as_str().unwrap_or("") {
                    "eth_simulateV1" => json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": [{
                            "calls": [
                                { "returnData": word(PRE_BALANCE), "gasUsed": "0x5208", "status": "0x1" },
                                { "returnData": "0x", "gasUsed": "0x249f0", "status": "0x1" },
                                { "returnData": word(POST_BALANCE), "gasUsed": "0x5208", "status": "0x1" }
                            ],
                            "blockNumber": "0x3000000",
                            "baseFeePerGas": "0x64"
                        }]
                    }),
                    other => json!({
                        "jsonrpc": "2.0", "id": id,
                        "error": { "code": -32601, "message": format!("unsupported method {other}") }
                    }),
                };
                axum::Json(resp)
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{addr}"), seen)
}

// ── candidate construction ──────────────────────────────────────────────────

/// A pinned V2↔Aerodrome back-run candidate derived from the fixture frame's
/// state, exactly as `AtomicArbStrategy` would stamp it.
fn pinned_backrun(state: &mev_bot::types::PreconfirmedState, cfg: &Config) -> Opportunity {
    let weth = cfg.chain.weth;
    let usdc = cfg.chain.usd_stable;
    // Legs priced off the real, live-verified Base WETH/USDC Aerodrome
    // volatile pool (0xcDAC0d…85C43, fee 30 bps off input) and a synthetic
    // diverged V2 counterparty — the exact shape the cross-venue search
    // emits.
    let v2 = mev_bot::dex::V2Pool {
        address: Address::with_last_byte(0x22),
        token0: weth,
        token1: usdc,
        reserve0: U256::from(1_000u64) * U256::from(10u128.pow(18)),
        reserve1: U256::from(2_200_000u64) * U256::from(10u128.pow(6)),
        fee_bps: 30,
        venue: Venue::UniV2,
        block: state.block_number - 1,
    };
    let aero = AeroPool {
        address: known::BASE_AERO_WETH_USDC_VOLATILE,
        token0: weth,
        token1: usdc,
        reserve0: U256::from(1_000u64) * U256::from(10u128.pow(18)),
        reserve1: U256::from(2_000_000u64) * U256::from(10u128.pow(6)),
        fee_bps: 30,
        stable: false,
        block: state.block_number,
    };
    let mut edges = mev_bot::dex::edge::PricedEdge::from_v2(&v2);
    edges.extend(mev_bot::dex::edge::PricedEdge::from_aero(
        &aero,
        known::BASE_AERODROME_ROUTER,
        known::BASE_AERODROME_FACTORY,
    ));
    let found = mev_bot::dex::edge::search_priced(
        &edges,
        weth,
        U256::from(10u128.pow(19)),
        2,
        std::time::Duration::from_secs(1),
    );
    let candidate = found
        .into_iter()
        .find(|c| c.uses_non_v2(&edges))
        .expect("fixture-shaped pools price a cross-venue cycle");
    let mut legs: Vec<Call> = candidate
        .build_calls(&edges, SIM_EXECUTOR)
        .expect("executable route");

    // Back-run: the whole route settles after the victim; nothing goes in
    // front (the victim is already inside the preconfirmed state).
    Opportunity {
        id: "fixture-pinned".into(),
        strategy: Strategy::AtomicArb,
        victim_hashes: vec![B256::with_last_byte(7)],
        front_calls: Vec::new(),
        back_calls: std::mem::take(&mut legs),
        flash_tokens: vec![candidate.anchor],
        flash_amounts: vec![candidate.amount_in],
        profit_token: candidate.anchor,
        expected_profit_wei: candidate.gross_profit,
        notional_wei: candidate.amount_in,
        target_block: state.block_number,
        created_at_ms: now_ms(),
        notes: format!("back-run fixture; {}", candidate.route_label(&edges)),
        provenance: Provenance {
            source_state: Some(state.clone()),
            ttl_ms: Some(cfg.preconfirmed_ttl_ms),
            requires_foreign_payload: false,
            route: candidate.route_label(&edges),
            direction: "forward".into(),
            route_hops: candidate
                .edges
                .iter()
                .map(|&i| edges[i].route_hop())
                .collect(),
            predicted_gross_wei: candidate.gross_profit,
        },
    }
}

fn base_cfg(http_url: &str) -> Arc<Config> {
    let mut cfg = box_config();
    cfg.endpoints.http_url = http_url.to_string();
    Arc::new(cfg)
}

/// The full Base chain profile for the test, mirroring the unit-test config
/// builders.
fn box_config() -> Config {
    Config {
        chain: mev_bot::config::ChainConfig {
            chain_id: 8453,
            name: "base".into(),
            weth: known::BASE_WETH,
            usd_stable: known::BASE_USDC,
            block_time_ms: 2_000,
        },
        addresses: *known::base(),
        priority_fee_wei: U256::from(1_000_000_000u64),
        token_valuation: false,
        valuation_haircut_bps: 200,
        raw_cancel_bump_bps: 1_250,
        raw_cancel_max_fee_wei: U256::from(500_000_000_000u64),
        submission_mode: mev_bot::config::SubmissionMode::Raw,
        qualification_backend: mev_bot::config::QualificationBackend::Sequencer,
        chain_block_ingest: false,
        endpoints: mev_bot::config::Endpoints {
            http_url: String::new(),
            ws_url: None,
            mev_share_sse: String::new(),
            relay_url: String::new(),
            bundle_relay_urls: vec![],
            relay_data_urls: vec![],
            bloxroute_relay_url: String::new(),
            sequencer_feed: None,
            flashblocks_ws: None,
            extra_mempool_ws: vec![],
            mev_blocker_ws: None,
            flashbots_signer_key: None,
            searcher_private_key: None,
            sniper_searcher_private_key: None,
            executor: None,
            searcher_address: Signer::simulation().address(),
            sniper_searcher_address: Address::ZERO,
        },
        risk: mev_bot::config::RiskConfig {
            min_net_profit_wei: U256::ZERO,
            max_position_wei: U256::from(10u128.pow(21)),
            max_base_fee_wei: U256::from(u64::MAX),
            bribe_bps: 0,
            max_gas_per_bundle: 3_000_000,
            max_drawdown_wei: U256::ZERO,
            max_inflight_per_strategy: 2,
            max_revert_rate: 1.0,
        },
        strategies: mev_bot::config::StrategyToggles {
            sandwich: false,
            sandwich_v3: false,
            jit: false,
            atomic_arb: true,
            liquidation: false,
            liquidation_compound: false,
            liquidation_morpho: false,
            liquidation_maker: false,
            oracle_frontrun: false,
            sniper: false,
        },
        sim: mev_bot::config::SimConfig {
            anvil_bin: "anvil".into(),
            anvil_port: 8548,
            anvil_replay_port: 8549,
            replay_fork: false,
            refork_every_blocks: 1,
            use_call_bundle: false,
            target_block_offset: 1,
            timeout: std::time::Duration::from_millis(5_000),
        },
        sniper_mode: mev_bot::sniper::SniperModeBoot::default(),
        liquidation: mev_bot::config::LiquidationConfig {
            watch_cap: 8,
            morpho_market_cap: 4,
            morpho_borrower_cap: 4,
            maker_ilks: vec![],
        },
        oracle: mev_bot::config::OracleConfig {
            watch_feeds: vec![],
            max_leads: 3,
        },
        alerts: mev_bot::config::AlertsConfig::default(),
        api: mev_bot::config::ApiConfig {
            bind: "127.0.0.1:0".into(),
            db_path: ":memory:".into(),
            feed_capacity: 10,
            write_queue_capacity: 1_024,
            auth_token: None,
            allowed_origins: vec![],
        },
        pool_discovery: true,
        pool_discovery_v3: false,
        decode_universal_router: false,
        dex_univ3_arb: false,
        dex_aerodrome_arb: true,
        dex_aerodrome_stable: false,
        arb_max_cycle_len: 2,
        relay_tx_ingest: false,
        relay_tx_concurrency: 4,
        strategy_concurrency: 64,
        replay_lanes: 1,
        replay_queue_depth: 4,
        pool_discovery_interval_blocks: 1,
        inventory_refresh_blocks: 1,
        arb_enumeration_budget: std::time::Duration::from_millis(25),
        arb_max_pools: 200,
        inventory_gate: false,
        live_execution: false,
        broadcast_enabled: false,
        qualification_hours: 168,
        qualification_min_samples: 30,
        qualification_min_relay_comparisons: 30,
        qualification_min_actual_matches: 30,
        qualification_max_error_bps: 2_000,
        qualification_min_accuracy_bps: 8_000,
        qualification_max_gap_secs: 120,
        finality_depth: 12,
        preconfirmed_ttl_ms: 1_000,
        submission_retry_ms: 250,
        submission_max_attempts: 2,
        live_smoke_max: 0,
        live_smoke_max_gas_cost_wei: U256::ZERO,
    }
}

// ── the acceptance test ─────────────────────────────────────────────────────

#[tokio::test]
async fn fixture_to_state_pinned_sim_to_signed_raw_base_transaction() {
    // 1. Parse the real fixture stream and pin the frame state.
    let mut parser = FlashblockParser::new();
    let base = parser.parse(&fixture("index0_base.json"), None);
    let multi = parser.parse(&fixture("multi_tx.json"), None);
    assert!(base.state.is_some() && multi.state.is_some());
    let state = multi.state.clone().expect("multi fixture has state");
    let tracker = PreconfirmedTracker::new();
    for parsed in [base, multi] {
        let hashes: Vec<B256> = parsed.txs.iter().map(|t| t.hash).collect();
        tracker.observe_frame(&parsed.state.expect("state"), &hashes);
    }

    let cfg = base_cfg("http://127.0.0.1:0");
    let opp = pinned_backrun(&state, &cfg);
    // Zero foreign payload by construction, and raw transport accepts it.
    assert!(!opp.provenance.requires_foreign_payload);
    assert!(raw_transportable(&opp));
    // The payload carries the Aerodrome router somewhere (cross-venue route).
    assert!(opp
        .back_calls
        .iter()
        .any(|c| c.target == known::BASE_AERODROME_ROUTER));

    // 2. The pin passes its identity/TTL recheck against a live tracker.
    check_pin(&opp, now_ms(), &tracker).expect("pin is current");

    // 3. State-pinned simulation on the mock provider; the request itself is
    //    the contract (`"pending"` tag, fixture overrides, exact payload).
    let (url, seen) = spawn_mock_rpc().await;
    let cfg = base_cfg(&url);
    let risk = RuntimeRisk::new(cfg.risk.clone(), cfg.strategies.clone());
    let sim = PendingSim::new(cfg.clone(), RpcClient::new(url).unwrap(), risk.clone());
    let base_fee = U256::from(100u64);
    let result = sim
        .simulate_pinned(&opp, base_fee, std::time::Instant::now())
        .await
        .expect("mock provider answers");

    assert_eq!(result.backend, SimBackend::EthSimulateV1);
    assert!(result.success);
    assert_eq!(
        result.gross_profit_wei,
        U256::from(POST_BALANCE - PRE_BALANCE)
    );
    assert_eq!(result.gas_used, 0x249f0u64);
    assert_eq!(
        result.gas_price_wei,
        base_fee + cfg.priority_fee_wei,
        "sequencer-chains price gas at base + configured priority fee"
    );

    let seen = seen.lock().await;
    assert_eq!(seen.len(), 1, "exactly one eth_simulateV1");
    let req = &seen[0];
    let params = req["params"].as_array().expect("params array");
    assert_eq!(params[1].as_str().unwrap(), "pending", "preconfirmed tag");
    let block_state = &params[0]["blockStateCalls"][0];
    let calls = block_state["calls"].as_array().unwrap();
    assert_eq!(
        calls[1]["to"].as_str().unwrap().to_lowercase(),
        format!("{SIM_EXECUTOR:?}").to_lowercase()
    );
    let exec_data = calls[1]["data"].as_str().unwrap();
    // Byte-identical payload to what the raw send signs.
    let expected_data = mev_bot::bundle::encode_execute(&opp, &opp.back_calls, false, &risk.risk());
    assert_eq!(
        exec_data,
        format!("0x{}", hex::encode(&expected_data)),
        "simulated payload must equal the to-be-signed payload"
    );
    // Fixture overrides: code is installed on the sim executor, the WETH
    // override is a one-slot stateDiff, never a full storage wipe.
    let overrides = &block_state["stateOverrides"];
    assert!(overrides[format!("{SIM_EXECUTOR:?}")]["code"]
        .as_str()
        .is_some_and(|c| c.len() > 1_000));
    let weth_override = &overrides[format!("{:?}", cfg.chain.weth)];
    assert!(weth_override.get("state").is_none());
    assert_eq!(weth_override["stateDiff"].as_object().unwrap().len(), 1);
    drop(seen);

    // 4. Sign the exact bundle for chain 8453 with the simulation key:
    //    one searcher-owned tx, no foreign payload anywhere.
    let signer = Signer::simulation();
    let ctx = BundleContext {
        chain_id: 8453,
        executor: SIM_EXECUTOR,
        nonce: 7,
        base_fee,
        priority_fee: cfg.priority_fee_wei,
        gas_limit: 3_000_000,
    };
    let bundle = build_bundle(&opp, &[], &ctx, &risk.risk(), &signer);
    assert_eq!(bundle.txs.len(), 1, "raw transport carries exactly one tx");
    let tx = &bundle.txs[0];
    assert!(!tx.foreign, "zero foreign payload (work order 2.4)");
    assert_eq!(tx.raw[0], 0x02, "EIP-1559 typed transaction");
    let (chain_id, nonce, to) = decode_head_fields(&tx.raw);
    assert_eq!(chain_id, 8453, "signed for Base");
    assert_eq!(nonce, 7);
    assert_eq!(to, SIM_EXECUTOR);
    assert!(tx.hash.is_some());
}

// ── pin gate semantics ──────────────────────────────────────────────────────

#[test]
fn ttl_and_supersede_fail_closed() {
    let cfg = base_cfg("http://127.0.0.1:0");
    let mut parser = FlashblockParser::new();
    parser.parse(&fixture("index0_base.json"), None);
    let multi = parser.parse(&fixture("multi_tx.json"), None);
    let state = multi.state.unwrap();
    let tracker = PreconfirmedTracker::new();
    tracker.observe_frame(&state, &[]);
    let mut opp = pinned_backrun(&state, &cfg);

    // Live pin passes.
    check_pin(&opp, now_ms(), &tracker).expect("current");

    // Wall-clock TTL: pretend the candidate was stamped an hour ago.
    opp.created_at_ms = now_ms().saturating_sub(3_600_000);
    assert_eq!(
        check_pin(&opp, now_ms(), &tracker),
        Err(PinReject::TtlExpired)
    );
    opp.created_at_ms = now_ms();

    // A newer block supersedes the pin — descendant within the block is
    // fine, a new block is not. `rollover_next` is the first frame of the
    // block after the pinned one.
    let mut parser = FlashblockParser::new();
    parser.parse(&fixture("rollover_prev.json"), None);
    let next = parser.parse(&fixture("rollover_next.json"), None);
    let t2 = PreconfirmedTracker::new();
    t2.observe_frame(&next.state.clone().unwrap(), &[]);
    assert_eq!(
        check_pin(&opp, now_ms(), &t2),
        Err(PinReject::StateSuperseded)
    );

    // A mempool-style back-run (requires the foreign victim payload) is not
    // raw-transportable.
    let mut mempool_style = opp.clone();
    mempool_style.provenance.requires_foreign_payload = true;
    assert!(!raw_transportable(&mempool_style));
}

// ── helpers ─────────────────────────────────────────────────────────────────

fn fixture(name: &str) -> Value {
    let path = format!(
        "{}/tests/fixtures/flashblocks/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{path}: {e}"))
}

/// Decode `(chain_id, nonce, to)` from a raw EIP-1559 RLP envelope.
/// Layout: `0x02 || rlp([chain_id, nonce, max_priority, max_fee, gas_limit,
/// to (item 5), value, data, access_list, y_parity, r, s])`.
fn decode_head_fields(raw: &[u8]) -> (u64, u64, Address) {
    assert_eq!(raw[0], 0x02);
    let payload = &raw[1..];
    let (_list_hdr, list_body) = read_rlp_header(payload, 0xc0);
    let mut rest = list_body;
    let mut items: Vec<&[u8]> = Vec::new();
    // Only items up to `to` (index 5) are needed — stop before `value`,
    // which may be the empty string.
    while !rest.is_empty() && items.len() <= 5 {
        let (hdr_len, body) = read_rlp_item(rest);
        items.push(body);
        rest = &rest[hdr_len + body.len()..];
    }
    assert_eq!(items.len(), 6, "envelope head fully parsed");
    let chain_id = rlp_u64(items[0]);
    let nonce = rlp_u64(items[1]);
    let to = Address::from_slice(items[5]);
    (chain_id, nonce, to)
}

fn read_rlp_header(data: &[u8], base: u8) -> (usize, &[u8]) {
    let b0 = data[0];
    if b0 < base + 56 {
        let len = (b0 - base) as usize;
        (1, &data[1..1 + len])
    } else {
        let len_of_len = (b0 - base - 55) as usize;
        let mut len = 0usize;
        for &b in &data[1..1 + len_of_len] {
            len = len * 256 + b as usize;
        }
        (1 + len_of_len, &data[1 + len_of_len..1 + len_of_len + len])
    }
}

/// Read one RLP string item; returns (header_len, payload).
fn read_rlp_item(data: &[u8]) -> (usize, &[u8]) {
    let b0 = data[0];
    if b0 < 0x80 {
        (0, &data[..1])
    } else if b0 < 0xb8 {
        let len = (b0 - 0x80) as usize;
        (1, &data[1..1 + len])
    } else {
        let len_of_len = (b0 - 0xb7) as usize;
        let mut len = 0usize;
        for &b in &data[1..1 + len_of_len] {
            len = len * 256 + b as usize;
        }
        (1 + len_of_len, &data[1 + len_of_len..1 + len_of_len + len])
    }
}

fn rlp_u64(data: &[u8]) -> u64 {
    data.iter().fold(0u64, |v, &b| v * 256 + b as u64)
}

// ── env-gated live fork route (work order 2.4 fork acceptance) ────────────

/// The fork half of the 2.4 acceptance, against a live Base provider:
/// resolve both venues from the live factories, read their reserves and the
/// pool's own fee at `"pending"` — the same preconfirmed tag the pinned
/// lane simulates at — build the production V2 + Aerodrome volatile legs,
/// and dry-run the payload through the fixture executor's `quoteFrom`
/// inside `eth_simulateV1` at `"pending"`.
///
/// Nothing about the outcome is faked: a WETH → USDC → WETH round trip pays
/// two venue fees, so on an efficient market the assertion is that the
/// batch *executes* (status 1, gas spent, ABI'd delta returned) and
/// honestly reports a fee-band loss. A live bug in either venue's
/// calldata, in the fixture override, or in `"pending"` support surfaces as
/// a revert, a wrong chain id, or an absurd delta — never as a green pass.
///
/// Never runs by default (`#[ignore]` + env gate). Execute explicitly with:
/// `BASE_FORK_RPC=https://mainnet.base.org cargo test --test state_pinned_send -- --ignored`
#[tokio::test]
#[ignore = "live Base RPC fork check; run explicitly with BASE_FORK_RPC set"]
async fn live_preconfirmed_fork_route_executes_and_reports_fee_loss() {
    use alloy_primitives::I256;
    use alloy_sol_types::SolCall;
    use mev_bot::dex::{IAerodromeFactory, IAerodromePool, IUniswapV2Pair};

    let Ok(url) = std::env::var("BASE_FORK_RPC") else {
        eprintln!("BASE_FORK_RPC unset — nothing to run");
        return;
    };
    let cfg = base_cfg(&url);
    // Public Base endpoints sit behind Cloudflare; a browser UA is accepted
    // there and ignored by private providers either way.
    let rpc = RpcClient::new(url.clone())
        .expect("rpc client")
        .with_header(
            "user-agent",
            "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 \
             (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36",
        );

    /// Public endpoints rate-limit bursts (some even within one batched
    /// HTTP request), so every call is paced and retried with backoff. This
    /// helper exists only here — the in-bot paths have their own retry
    /// policies.
    async fn retrying<T, Fut>(what: &str, mut f: impl FnMut() -> Fut) -> T
    where
        Fut: std::future::Future<Output = anyhow::Result<T>>,
    {
        for attempt in 0..8u32 {
            tokio::time::sleep(std::time::Duration::from_millis(750)).await; // pacing
            match f().await {
                Ok(v) => return v,
                Err(e) => {
                    let wait = std::time::Duration::from_millis(900 * (attempt as u64 + 1));
                    eprintln!(
                        "{what}: {e:#} — retry in {wait:?} (attempt {})",
                        attempt + 1
                    );
                    tokio::time::sleep(wait).await;
                }
            }
        }
        panic!("{what}: still failing after 8 attempts");
    }
    let call_params = |to: Address, data: Vec<u8>| json!([{ "to": format!("{to:?}"), "data": format!("0x{}", hex::encode(&data)) }, "pending"]);
    let call_pending = |to: Address, data: Vec<u8>| {
        let rpc = &rpc;
        let params = call_params(to, data);
        async move {
            let out: String = rpc.call("eth_call", params).await?;
            Ok(hex::decode(out.strip_prefix("0x").unwrap_or(&out))?)
        }
    };
    fn word(raw: &[u8], i: usize) -> U256 {
        U256::from_be_slice(&raw[i * 32..(i + 1) * 32])
    }
    fn addr(raw: &[u8]) -> Address {
        Address::from_slice(&raw[12..32])
    }

    // Fail closed on the wrong chain — a mislabelled endpoint must never
    // produce a green run.
    let chain = retrying("eth_chainId", || rpc.call_raw("eth_chainId", json!([]))).await;
    assert_eq!(
        chain.as_str().unwrap_or_default(),
        "0x2105",
        "BASE_FORK_RPC must point at Base mainnet"
    );

    let weth = cfg.chain.weth;
    let usdc = cfg.chain.usd_stable;

    // Resolve pools from the live factories — the registry constant must
    // agree with the chain, never be trusted blindly.
    let univ2_factory = cfg.addresses.univ2_factory.expect("base univ2 factory");
    let aero_factory = cfg.addresses.aerodrome_factory.expect("base aero factory");
    let v2_addr = retrying("getPair", || {
        mev_bot::dex::get_pair(&rpc, univ2_factory, weth, usdc)
    })
    .await
    .expect("UniV2 WETH/USDC pair exists on Base");
    let aero_addr = retrying("getPool", || {
        mev_bot::dex::aero_get_pool(&rpc, aero_factory, weth, usdc, false)
    })
    .await
    .expect("Aerodrome WETH/USDC volatile pool exists");
    assert_eq!(
        aero_addr,
        known::BASE_AERO_WETH_USDC_VOLATILE,
        "registry constant disagrees with the live default factory"
    );

    // Token ordering, reserves, and the aero pool's own fee — all paced
    // reads at the preconfirmed tag the pinned lane executes against.
    let v2_token0 = addr(
        &retrying("v2 token0", || {
            call_pending(v2_addr, IUniswapV2Pair::token0Call {}.abi_encode())
        })
        .await,
    );
    let aero_token0 = addr(
        &retrying("aero token0", || {
            call_pending(aero_addr, IAerodromePool::token0Call {}.abi_encode())
        })
        .await,
    );
    assert_eq!(v2_token0, weth, "UniV2 pair token ordering");
    assert_eq!(aero_token0, weth, "Aerodrome pool token ordering");
    let v2_res = retrying("v2 reserves", || {
        call_pending(v2_addr, IUniswapV2Pair::getReservesCall {}.abi_encode())
    })
    .await;
    let aero_res = retrying("aero reserves", || {
        call_pending(aero_addr, IAerodromePool::getReservesCall {}.abi_encode())
    })
    .await;
    let aero_fee_bps = word(
        &retrying("aero getFee", || {
            call_pending(
                aero_factory,
                IAerodromeFactory::getFeeCall {
                    pool: aero_addr,
                    stable: false,
                }
                .abi_encode(),
            )
        })
        .await,
        0,
    )
    .to::<u32>();
    assert!(
        (1..=1_000).contains(&aero_fee_bps),
        "sane live aero fee: {aero_fee_bps} bps"
    );

    let bn = retrying("eth_blockNumber", || {
        rpc.call_raw("eth_blockNumber", json!([]))
    })
    .await;
    let block = u64::from_str_radix(bn.as_str().unwrap().trim_start_matches("0x"), 16).unwrap();

    let amount_in = U256::from(10_000_000_000_000_000u128); // 0.01 WETH
    let v2_pool = mev_bot::dex::V2Pool {
        address: v2_addr,
        token0: weth,
        token1: usdc,
        reserve0: word(&v2_res, 0),
        reserve1: word(&v2_res, 1),
        fee_bps: 30,
        venue: Venue::UniV2,
        block,
    };
    let aero_pool = AeroPool {
        address: aero_addr,
        token0: weth,
        token1: usdc,
        reserve0: word(&aero_res, 0),
        reserve1: word(&aero_res, 1),
        fee_bps: aero_fee_bps,
        stable: false,
        block,
    };
    assert!(
        !v2_pool.reserve0.is_zero() && !aero_pool.reserve0.is_zero(),
        "live reserves"
    );

    // Production legs: direct UniV2-pair swap then the Aerodrome router hop,
    // recipients back to the executor. The second leg's input is quoted
    // off-chain minus 1 % drift slack — the slack stays in the executor as
    // dust; no profit is ever manufactured.
    let v2_edge = mev_bot::dex::edge::PricedEdge::from_v2(&v2_pool)
        .into_iter()
        .find(|e| e.token_in == weth)
        .expect("WETH→USDC direction");
    let aero_edge = mev_bot::dex::edge::PricedEdge::from_aero(
        &aero_pool,
        known::BASE_AERODROME_ROUTER,
        known::BASE_AERODROME_FACTORY,
    )
    .into_iter()
    .find(|e| e.token_in == usdc)
    .expect("USDC→WETH direction");
    let v2_quote = v2_pool.amount_out(weth, amount_in).expect("v2 quotes");
    let aero_in = v2_quote * U256::from(99u8) / U256::from(100u8);
    let mut legs = v2_edge
        .build_calls(amount_in, SIM_EXECUTOR)
        .expect("v2 leg builds");
    legs.extend(
        aero_edge
            .build_calls(aero_in, SIM_EXECUTOR)
            .expect("aero leg builds"),
    );
    assert_eq!(legs.len(), 4, "transfer+swap, approve+swap");

    // Dry-run at the preconfirmed state through the same fixture the pinned
    // lane installs: executor code, owner slot, searcher gas, one WETH slot.
    let data = mev_bot::bundle::encode_quote_from(&legs, weth);
    let fixture_code =
        mev_bot::sim::anvil::executor_fixture_runtime(&cfg).expect("fixture runtime");
    let searcher = cfg.endpoints.searcher_address;
    let params = json!([{
        "blockStateCalls": [{
            "stateOverrides": mev_bot::sim::pending::fixture_overrides(weth, searcher, &fixture_code),
            "calls": [{
                "from": format!("{searcher:?}"),
                "to": format!("{SIM_EXECUTOR:?}"),
                "data": format!("0x{}", hex::encode(&data)),
            }],
        }],
        "traceTransfers": false,
        "validation": false,
    }, "pending"]);
    let resp = retrying("eth_simulateV1", || {
        rpc.call_raw("eth_simulateV1", params.clone())
    })
    .await;
    let sim_block = resp
        .as_array()
        .and_then(|b| b.first())
        .expect("one simulated block");
    let calls = sim_block["calls"].as_array().expect("calls array");
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0]["status"].as_str().unwrap_or_default(),
        "0x1",
        "the two-venue batch must execute at the preconfirmed state: {calls:?}"
    );

    let ret = calls[0]["returnData"].as_str().expect("returnData");
    let raw = hex::decode(ret.strip_prefix("0x").unwrap_or(ret)).expect("return hex");
    assert_eq!(raw.len(), 64, "(int256 delta, uint256 gasUsed)");
    let delta: i128 = I256::from_be_bytes::<32>(raw[..32].try_into().unwrap())
        .to_string()
        .parse()
        .expect("delta fits i128");
    let quote_gas = word(&raw, 1).to::<u64>();
    // Informational only: full block objects carry `number`; some providers
    // use `blockNumber`. Not a correctness gate.
    let sim_block_num = ["number", "blockNumber"]
        .iter()
        .find_map(|k| sim_block[k].as_str())
        .and_then(|s| u64::from_str_radix(s.trim_start_matches("0x"), 16).ok())
        .unwrap_or(0);

    let amount_i128 = amount_in.to::<u128>() as i128;
    assert!(
        quote_gas > 30_000,
        "two venues must actually run (gas {quote_gas})"
    );
    assert!(
        delta < 0,
        "two venue fees must show as an honest loss, got {delta}"
    );
    assert!(
        delta.unsigned_abs() > (amount_i128 / 1_000) as u128,
        "loss below the 0.1 % fee floor — fees were not charged: {delta}"
    );
    assert!(
        delta.unsigned_abs() < (amount_i128 / 10) as u128,
        "loss above the 10 % sanity band — reserves or calldata are wrong: {delta}"
    );
    eprintln!(
        "live fork route OK: block {sim_block_num}, in {amount_in} wei WETH, \
         delta {delta} wei WETH (fee-band loss), gas {quote_gas}"
    );
}
