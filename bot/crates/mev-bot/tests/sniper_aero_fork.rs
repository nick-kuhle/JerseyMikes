//! W4.2: Aerodrome sniper execution adapters, round-tripped on a real Base
//! fork against the chain's actual WETH/USDC volatile pool and measured
//! against SniperVault guard semantics.
//!
//! Environment-gated like the WS-Q live half: the deterministic suite never
//! runs it, and it is skipped (not failed) without the env var.
//!
//! ```sh
//! BASE_FORK_RPC=https://mainnet.base.org cargo test --test sniper_aero_fork -- --ignored
//! ```

use std::sync::Arc;

use alloy_primitives::{Address, U256};
use mev_bot::config::{known, Config};
use mev_bot::dex::{aero_volatile_amount_out, fetch_aero_pool};
use mev_bot::rpc::RpcClient;
use mev_bot::signer::Signer;
use mev_bot::sniper::calldata::{build_entry_aero, build_exit_aero, make_tag};
use mev_bot::sniper::sim_vault::{SimTxOutcome, SimVaultFixture};
use serde_json::json;

/// A generous fee-band ceiling for the round trip: two 30 bps pool fees
/// plus reserve drift between the entry and exit quotes. Nothing about a
/// profitable-looking round trip could slip past this — the sniper is a
/// directional strategy and a "lossless" buy/sell would be the anomaly.
const MAX_ROUND_TRIP_LOSS_BPS: u128 = 300;

#[tokio::test(flavor = "multi_thread")]
#[ignore = "live Base RPC fork check; run explicitly with BASE_FORK_RPC set"]
async fn aero_round_trip_on_a_base_fork_matches_vault_guard_semantics() {
    let Ok(url) = std::env::var("BASE_FORK_RPC") else {
        eprintln!("BASE_FORK_RPC unset — nothing to run");
        return;
    };
    let cfg = base_cfg(&url);

    /// The public Base RPC rate-limits bursts even inside one batch; retry
    /// with backoff and fail loudly using the operation name.
    async fn retrying<T, Fut>(what: &str, mut f: impl FnMut() -> Fut) -> T
    where
        Fut: std::future::Future<Output = anyhow::Result<T>>,
    {
        let mut delay = std::time::Duration::from_millis(750);
        for attempt in 0..8 {
            match f().await {
                Ok(v) => return v,
                Err(e) if attempt == 7 => panic!("{what} failed after retries: {e:#}"),
                Err(_) => {
                    tokio::time::sleep(delay).await;
                    delay = (delay * 2).min(std::time::Duration::from_secs(8));
                }
            }
        }
        unreachable!()
    }

    // Fail loudly if the env var points at the wrong chain.
    let remote = RpcClient::new(url.clone()).expect("rpc client");
    let chain = retrying("eth_chainId", || remote.call_raw("eth_chainId", json!([]))).await;
    assert_eq!(
        chain.as_str().map(trim_hex_to_u64),
        Some(8453),
        "BASE_FORK_RPC must point at Base mainnet"
    );

    // Local anvil fork at the live head; every state transition below is a
    // real mined transaction against Base state.
    let bn = retrying("eth_blockNumber", || {
        remote.call_raw("eth_blockNumber", json!([]))
    })
    .await;
    let head = u64::from_str_radix(bn.as_str().unwrap().trim_start_matches("0x"), 16).unwrap();
    eprintln!("forking Base at block {head}");
    let fork = mev_bot::sim::anvil::AnvilSim::spawn(cfg.clone(), head)
        .await
        .expect("anvil fork");
    let f_rpc: RpcClient = fork.rpc().clone();

    // The contract-backed fixture deploys the real SniperVault on the fork,
    // authorizes the simulation searcher, and funds the vault with real
    // Base WETH wrapped from owner ETH.
    let one_eth = U256::from(1_000_000_000_000_000_000u128);
    let fixture = SimVaultFixture::new(f_rpc.clone(), known::BASE_WETH, 8453, one_eth, one_eth)
        .with_shared_lock(fork.sim_lock());
    let state = fixture.ensure_deployed().await.expect("fixture deploy");
    let vault = state.vault;

    // Live pool state, read from the fork's own view of Base (the registry
    // constant is what WS-P cross-checked against `factory.getPool`).
    let pool = fetch_aero_pool(
        &f_rpc,
        known::BASE_AERODROME_FACTORY,
        known::BASE_AERO_WETH_USDC_VOLATILE,
        head,
    )
    .await
    .expect("fetch aero pool");
    assert!(!pool.stable, "registry must name the volatile pool");
    let (weth_r, usdc_r) = pool.reserves_for(known::BASE_WETH).expect("WETH in pool");
    assert!(
        weth_r > one_eth,
        "pool is deep enough for a 0.001 ETH probe: {weth_r}"
    );

    let balance_of = |token: Address, who: Address| {
        let rpc = f_rpc.clone();
        async move {
            let data = json!([{"to": format!("{token:?}"), "data": format!(
                "0x70a08231000000000000000000000000{}",
                hex::encode(who)
            )}, "latest"]);
            let v = rpc.call_raw("eth_call", data).await.expect("balanceOf");
            let raw = hex::decode(v.as_str().unwrap().trim_start_matches("0x")).unwrap();
            U256::from_be_slice(&raw)
        }
    };

    // ── ENTRY: build the WETH→USDC adapter calldata, execute through the
    //    real vault, and hold the result to the entry guard's floor. ──
    let size = one_eth / U256::from(1_000u64); // 0.001 ETH
    let expected_tokens = aero_volatile_amount_out(size, weth_r, usdc_r, pool.fee_bps);
    assert!(!expected_tokens.is_zero(), "aero entry quote");
    let entry_block = {
        let v = f_rpc
            .call_raw("eth_blockNumber", json!([]))
            .await
            .expect("fork head");
        u64::from_str_radix(v.as_str().unwrap().trim_start_matches("0x"), 16).unwrap()
    };
    let (_, entry_guard, entry_cd) = build_entry_aero(
        vault,
        known::BASE_AERODROME_ROUTER,
        known::BASE_AERODROME_FACTORY,
        known::BASE_WETH,
        known::BASE_USDC,
        size,
        expected_tokens,
        500, // 5% impact ceiling; the pool is deep, so actual impact is dust
        entry_block,
        1_000_000,
        U256::ZERO,
        make_tag("aero-fork-round-trip", 0),
    );
    let usdc_before = balance_of(known::BASE_USDC, vault).await;
    let entry = fixture
        .execute_vault_calldata(&entry_cd)
        .await
        .expect("entry executes");
    assert!(
        entry.is_mined(),
        "entry must mine through the real vault: {entry:?}"
    );
    let usdc_after = balance_of(known::BASE_USDC, vault).await;
    let bought = usdc_after.saturating_sub(usdc_before);
    assert!(
        bought >= entry_guard.minTokensOut && !bought.is_zero(),
        "SniperVault balance-delta semantics: got {bought}, guard floor {}",
        entry_guard.minTokensOut
    );

    // ── EXIT: sell the acquired USDC back through the exit adapter and hold
    //    the result to the exit guard's floor. ──
    // Re-read reserves *after* our own entry moved them, and re-read the
    // pool's fee — the production exit path quotes exactly this way.
    let pool_now = fetch_aero_pool(
        &f_rpc,
        known::BASE_AERODROME_FACTORY,
        known::BASE_AERO_WETH_USDC_VOLATILE,
        entry_block + 1,
    )
    .await
    .expect("refetch aero pool");
    let (weth_r2, usdc_r2) = pool_now
        .reserves_for(known::BASE_WETH)
        .expect("WETH in pool");
    let expected_weth = aero_volatile_amount_out(bought, usdc_r2, weth_r2, pool_now.fee_bps);
    assert!(!expected_weth.is_zero(), "aero exit quote");
    let weth_before = balance_of(known::BASE_WETH, vault).await;
    let (_, exit_guard, exit_cd) = build_exit_aero(
        vault,
        known::BASE_AERODROME_ROUTER,
        known::BASE_AERODROME_FACTORY,
        known::BASE_WETH,
        known::BASE_USDC,
        bought,
        expected_weth,
        500, // 5% slippage ceiling
        entry_block,
        1_000_000,
        U256::ZERO,
        make_tag("aero-fork-round-trip", 1),
    );
    let exit = fixture
        .execute_vault_calldata(&exit_cd)
        .await
        .expect("exit executes");
    assert!(
        exit.is_mined(),
        "exit must mine through the real vault: {exit:?}"
    );
    let weth_after = balance_of(known::BASE_WETH, vault).await;
    let recovered = weth_after.saturating_sub(weth_before);
    assert!(
        recovered >= exit_guard.minWethOut && !recovered.is_zero(),
        "SniperVault balance-delta semantics: got {recovered}, guard floor {}",
        exit_guard.minWethOut
    );

    // ── Honest economics: the round trip must cost the two pool fees plus
    //    dust, and never read as a manufactured gain. ──
    let (gas_entry, gas_exit) = match (&entry, &exit) {
        (
            SimTxOutcome::Mined {
                gas_cost_wei: a, ..
            },
            SimTxOutcome::Mined {
                gas_cost_wei: b, ..
            },
        ) => (*a, *b),
        _ => unreachable!("both mined asserted above"),
    };
    assert!(recovered <= size, "a profitable round trip would be a bug");
    let loss = size - recovered;
    let loss_bps = loss
        .saturating_mul(U256::from(10_000u64))
        .checked_div(size)
        .map(|v| v.to::<u128>())
        .unwrap_or(u128::MAX);
    assert!(
        loss_bps <= MAX_ROUND_TRIP_LOSS_BPS,
        "round trip lost {loss_bps} bps — far beyond 2x{}bps fee; something else is wrong",
        pool.fee_bps
    );
    eprintln!(
        "round trip: spent {size} wei, recovered {recovered} ({loss_bps} bps band incl. 2x{}bps fee); \
         gas {} + {}",
        pool.fee_bps, gas_entry, gas_exit
    );
}

fn trim_hex_to_u64(s: &str) -> u64 {
    u64::from_str_radix(s.trim_start_matches("0x"), 16).unwrap_or(0)
}

fn base_cfg(http_url: &str) -> Arc<Config> {
    let mut cfg = box_config();
    cfg.endpoints.http_url = http_url.to_string();
    Arc::new(cfg)
}

/// The full Base chain profile for the test, mirroring the WS-Q harness.
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
            anvil_port: 8551,
            anvil_replay_port: 8552,
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
