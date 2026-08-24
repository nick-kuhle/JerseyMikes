#![deny(warnings)]

//! CLI entry point.

use std::sync::Arc;

use anyhow::Result;
use clap::{Parser, Subcommand};
use mev_bot::{api, config::Config, engine::Engine};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "mev-bot", about = "Simulation-first MEV searcher", version)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Path to the .env file to load before reading configuration.
    #[arg(long, default_value = ".env")]
    env_file: String,
}

#[derive(Subcommand)]
enum Command {
    /// Run the searcher and the dashboard API (default).
    Run,
    /// Serve only the API against the existing database (no ingestion).
    Api,
    /// Check that every configured endpoint answers, then exit.
    Doctor,
    /// Replay harness: compare stored simulations against relay bid traces.
    Replay {
        /// Inclusive lower bound (block number).
        #[arg(long)]
        from_block: Option<u64>,
        /// Inclusive upper bound (block number).
        #[arg(long)]
        to_block: Option<u64>,
        #[arg(long, default_value_t = 200)]
        limit: i64,
        /// Write the comparison into the `reconciliations` table.
        #[arg(long, default_value_t = true)]
        persist: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let _ = dotenvy::from_filename(&cli.env_file);

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(true)
        .init();

    let cfg = Arc::new(Config::from_env()?);
    tracing::info!("{}", cfg.summary());
    if cfg.live_execution && cfg.broadcast_enabled {
        if cfg.live_smoke_max > 0 {
            tracing::warn!(
                max = cfg.live_smoke_max,
                "LIVE EXECUTION AND BROADCAST ARE ENABLED — qualification-passing bundles may be sent; LIVE_SMOKE_MAX also allows that many un-qualified live-candidate sends"
            );
        } else {
            tracing::warn!(
                "LIVE EXECUTION AND BROADCAST CAPABILITY ARE ENABLED — only qualification-passing bundles may be sent"
            );
        }
    } else if cfg.live_execution {
        tracing::warn!("live mode is armed, but BROADCAST_ENABLED=false — shadow recording only");
    } else {
        tracing::info!("simulation mode: no transaction will ever be broadcast");
    }

    match cli.command.unwrap_or(Command::Run) {
        Command::Doctor => doctor(cfg).await,
        Command::Replay {
            from_block,
            to_block,
            limit,
            persist,
        } => replay(cfg, from_block, to_block, limit, persist),
        Command::Api => {
            cfg.validate()?;
            let engine = Arc::new(Engine::new(cfg.clone()).await?);
            api::serve(engine, &cfg.api.bind).await
        }
        Command::Run => {
            cfg.validate()?;
            let engine = Arc::new(Engine::new(cfg.clone()).await?);
            let api_engine = engine.clone();
            let bind = cfg.api.bind.clone();
            tokio::spawn(async move {
                if let Err(e) = api::serve(api_engine, &bind).await {
                    tracing::error!(error = %e, "api server stopped");
                }
            });

            tokio::select! {
                res = engine.clone().run() => res,
                _ = tokio::signal::ctrl_c() => {
                    tracing::info!("shutting down");
                    if let Some(fork) = &engine.sim.fork {
                        fork.shutdown().await;
                    }
                    Ok(())
                }
            }
        }
    }
}

/// Connectivity pre-flight: tells the operator exactly which data sources are
/// live before a run starts.
async fn doctor(cfg: Arc<Config>) -> Result<()> {
    use mev_bot::rpc::RpcClient;
    use serde_json::json;

    // Hard-failure accumulator. `doctor` reports everything it can, but a
    // caller like `make doctor` must be able to rely on the exit code: any
    // of these means the data plane is wrong or `run` would fail, so the
    // process exits non-zero. Soft warnings (!/) never affect the code.
    let mut hard_failures: Vec<String> = Vec::new();

    let http = RpcClient::new(cfg.endpoints.http_url.clone())?;
    match http.call_raw("eth_blockNumber", json!([])).await {
        Ok(v) => println!(
            "✓ http rpc          {} (head {})",
            cfg.endpoints.http_url, v
        ),
        Err(e) => {
            println!("✗ http rpc          {}: {e}", cfg.endpoints.http_url);
            hard_failures.push(format!("http rpc unreachable: {e}"));
        }
    }

    match cfg.validate() {
        Ok(()) => {
            if cfg.api.auth_token.is_some() {
                println!(
                    "\u{2713} api bind          {} (token required on mutating endpoints)",
                    cfg.api.bind
                );
            } else {
                println!(
                    "\u{2713} api bind          {} (loopback, no token needed)",
                    cfg.api.bind
                );
            }
        }
        // Not fatal here: `doctor` binds nothing. Report it the way every
        // other check reports, so the operator sees this problem alongside
        // the rest instead of getting a wall of text and no diagnostics.
        Err(e) => {
            println!("\u{2717} api bind          {}", cfg.api.bind);
            for line in e.to_string().lines() {
                println!("                    {line}");
            }
            hard_failures.push(format!("configuration invalid: {e}"));
        }
    }

    match http.call_raw("eth_chainId", json!([])).await {
        Ok(v) => {
            // Cross-check the RPC's chain against the configured profile: a
            // Base env pointed at a mainnet RPC (or vice versa) is the
            // classic cross-chain data-bleed setup — name it here, at
            // pre-flight, not in a poisoned soak.
            let reported = v.to_string();
            let reported = reported.trim_matches('"');
            let hex_id = reported
                .strip_prefix("0x")
                .and_then(|h| u64::from_str_radix(h, 16).ok());
            let profile = mev_bot::config::known::for_chain(cfg.chain.chain_id);
            let profile_name = match profile {
                Some(p) => p.chain_id.to_string(),
                None => "custom (env)".to_string(),
            };
            let weth = format!("{:?}", cfg.addresses.weth);
            match hex_id {
                Some(id) if id == cfg.chain.chain_id => {
                    println!("✓ chain id          {v} (profile {profile_name}, WETH {weth})");
                }
                Some(id) => {
                    println!(
                        "✗ chain id          {v} — RPC is chain {id} but CHAIN_ID is {} \
                         (WETH {weth}); the profile and the RPC disagree",
                        cfg.chain.chain_id
                    );
                    hard_failures.push(format!(
                        "chain id mismatch: RPC reports {id}, CHAIN_ID is {}",
                        cfg.chain.chain_id
                    ));
                }
                None => println!("✓ chain id          {v} (profile {profile_name}, WETH {weth})"),
            }
        }
        Err(e) => {
            println!("✗ chain id          {e}");
            hard_failures.push(format!("eth_chainId failed: {e}"));
        }
    }
    // Transport + qualification shape for this chain, so the operator sees
    // how a live send and the PASS gate actually work before arming.
    println!(
        "· transport         {} ({}), qualification backend: {}",
        if cfg.submission_mode == mev_bot::config::SubmissionMode::Raw {
            "raw tx to chain RPC"
        } else {
            "relay eth_sendBundle"
        },
        if cfg.addresses.sequencer_only {
            "sequencer chain, no relay market"
        } else {
            "relay market"
        },
        cfg.qualification_backend.as_str()
    );
    for warning in cfg.coherence_warnings() {
        println!("! coherence         {warning}");
    }

    // Raw transaction access is what makes faithful victim replay possible.
    match http
        .call_raw(
            "eth_getRawTransactionByHash",
            json!(["0x0000000000000000000000000000000000000000000000000000000000000000"]),
        )
        .await
    {
        Ok(_) => println!("✓ raw tx access     eth_getRawTransactionByHash supported"),
        Err(e) => {
            if e.to_string().contains("not found") || e.to_string().contains("null") {
                println!("✓ raw tx access     supported");
            } else {
                println!(
                    "! raw tx access     unsupported ({e}) — sandwich/JIT sims will be skipped"
                );
            }
        }
    }

    match &cfg.endpoints.ws_url {
        Some(u) => println!("· websocket         configured: {u}"),
        None => println!("! websocket         not set — falling back to HTTP head polling"),
    }

    match cfg.endpoints.flashbots_signer_key {
        Some(_) => println!("· flashbots key     configured"),
        None => println!("! flashbots key     not set — relay cross-checks use an ephemeral key"),
    }

    match &cfg.endpoints.searcher_private_key {
        Some(_) => println!(
            "✓ searcher key       configured; derived address {:?}",
            cfg.endpoints.searcher_address
        ),
        None if cfg.live_execution => {
            println!("✗ searcher key       SEARCHER_PRIVATE_KEY required before live arming")
        }
        None => println!("· searcher key       built-in public simulation key (cannot broadcast)"),
    }
    println!(
        "· broadcast gate     {} (qualification {}h / {} actual matches)",
        if cfg.broadcast_enabled {
            "enabled"
        } else {
            "disabled"
        },
        cfg.qualification_hours,
        cfg.qualification_min_actual_matches
    );
    {
        let (used, gas_at_risk) = {
            let db = std::path::Path::new(&cfg.api.db_path);
            if db.exists() {
                mev_bot::store::Store::open(&cfg.api.db_path)
                    .map(|store| {
                        (
                            store.smoke_used().unwrap_or(u64::MAX),
                            store
                                .smoke_gas_at_risk_wei()
                                .unwrap_or(alloy_primitives::U256::MAX),
                        )
                    })
                    .unwrap_or((u64::MAX, alloy_primitives::U256::MAX))
            } else {
                (0, alloy_primitives::U256::ZERO)
            }
        };
        let remaining = mev_bot::config::smoke_remaining(used, cfg.live_smoke_max);
        if cfg.live_smoke_max > 0 {
            println!(
                "{} live smoke         LIVE_SMOKE_MAX={} used={} remaining={} gas_at_risk={} / {} wei (still needs arming, broadcast, risk, inventory, live-candidate, exact sim)",
                if remaining > 0 { "!" } else { "·" },
                cfg.live_smoke_max,
                used,
                remaining,
                gas_at_risk,
                cfg.live_smoke_max_gas_cost_wei
            );
        } else {
            println!("· live smoke         off (LIVE_SMOKE_MAX=0)");
        }
    }

    match tokio::process::Command::new(&cfg.sim.anvil_bin)
        .arg("--version")
        .output()
        .await
    {
        Ok(o) => println!(
            "✓ anvil             {}",
            String::from_utf8_lossy(&o.stdout).trim()
        ),
        Err(e) => println!(
            "✗ anvil             {} not runnable: {e}",
            cfg.sim.anvil_bin
        ),
    }

    for relay in &cfg.endpoints.relay_data_urls {
        let url = format!(
            "{}/relay/v1/data/bidtraces/proposer_payload_delivered?limit=1",
            relay.trim_end_matches('/')
        );
        match reqwest::get(&url).await {
            Ok(r) => println!("· relay data        {relay} -> {}", r.status()),
            Err(e) => println!("! relay data        {relay} -> {e}"),
        }
    }

    // bloXroute Max Profit relay: the source of delivered-block transaction
    // ingestion. Only polled for blocks (not for value) when tx ingest is on.
    {
        let base = &cfg.endpoints.bloxroute_relay_url;
        let url = format!(
            "{}/relay/v1/data/bidtraces/proposer_payload_delivered?limit=1",
            base.trim_end_matches('/')
        );
        match reqwest::get(&url).await {
            Ok(r) => println!(
                "· bloxroute relay   {base} -> {} (tx ingest {})",
                r.status(),
                if cfg.relay_tx_ingest { "on" } else { "off" }
            ),
            Err(e) => println!("! bloxroute relay   {base} -> {e}"),
        }
    }

    // Endpoints that will carry eth_sendBundle once the broadcast lane is
    // armed. A dead relay in the list is silently lost inclusion, so probe
    // each one the way submission will: a read-only eth_callBundle whose
    // RPC error response still proves the endpoint is alive and speaks the
    // API. Nothing is enqueued by eth_callBundle.
    for relay in &cfg.endpoints.bundle_relay_urls {
        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "eth_callBundle",
            "params": [{ "txs": [], "blockNumber": "0x1" }],
        });
        match reqwest::Client::new()
            .post(relay)
            .header("content-type", "application/json")
            .body(body.to_string())
            .send()
            .await
        {
            Ok(r) => println!("· bundle relay      {relay} -> {}", r.status()),
            Err(e) => println!("✗ bundle relay      {relay} unreachable: {e}"),
        }
    }

    // Two trust domains: the Flashbots signer exists purely for RPC-header
    // reputation and must never be the funded searcher key.
    if let Some(fb) = &cfg.endpoints.flashbots_signer_key {
        match mev_bot::signer::Signer::from_hex(fb) {
            Ok(s) if s.address() == cfg.endpoints.searcher_address => {
                println!(
                    "✗ key separation   FLASHBOTS_SIGNER_KEY and SEARCHER_PRIVATE_KEY derive the \
                     same address ({}) — split them before arming",
                    cfg.endpoints.searcher_address
                );
                hard_failures.push(
                    "FLASHBOTS_SIGNER_KEY and SEARCHER_PRIVATE_KEY derive the same address"
                        .to_string(),
                );
            }
            Ok(_) => println!("✓ key separation   flashbots signer differs from the searcher key"),
            Err(e) => println!("! key separation   FLASHBOTS_SIGNER_KEY unparseable: {e}"),
        }
    }

    // The executor is what the broadcast lane will actually call. If it is
    // configured, verify it on-chain: deployed code, searcher allowlisted,
    // and which address owns it (the operator confirms that is the cold
    // wallet by eye — the bot cannot know which one that is).
    match cfg.endpoints.executor {
        Some(executor) => {
            let addr = format!("{executor:?}");
            match http.call_raw("eth_getCode", json!([addr, "latest"])).await {
                Ok(code) if code.as_str().is_some_and(|s| s.len() > 4) => {
                    println!("✓ executor          {addr} has on-chain code");
                }
                _ => {
                    println!(
                        "✗ executor          {addr} has no on-chain code — deploy per docs/GO_LIVE.md"
                    );
                    hard_failures.push(format!("executor {addr} has no on-chain code"));
                }
            }

            // searchers(address) — public mapping getter on MevExecutor.
            let mut cd = [0u8; 36];
            cd[..4].copy_from_slice(&alloy_primitives::keccak256("searchers(address)")[..4]);
            cd[16..].copy_from_slice(cfg.endpoints.searcher_address.as_slice());
            let data = format!("{:?}", alloy_primitives::Bytes::from(cd.to_vec()));
            let call = json!([{ "to": addr, "data": data }, "latest"]);
            match http.call_raw("eth_call", call).await {
                Ok(v) => {
                    let allowed = v
                        .as_str()
                        .and_then(|s| s.strip_prefix("0x"))
                        .and_then(|h| alloy_primitives::U256::from_str_radix(h, 16).ok());
                    match allowed {
                        Some(w) if !w.is_zero() => println!(
                            "✓ executor searcher  {:?} is allowlisted",
                            cfg.endpoints.searcher_address
                        ),
                        Some(_) => println!(
                            "! executor searcher  {:?} NOT allowlisted — call setSearcher from \
                             the owner wallet (docs/GO_LIVE.md)",
                            cfg.endpoints.searcher_address
                        ),
                        None => println!("! executor searcher  allowlist read returned {v}"),
                    }
                }
                Err(e) => println!("! executor searcher  allowlist read failed: {e}"),
            }

            // owner() — report who can sweep funds and flip the allowlist.
            let mut cd = [0u8; 4];
            cd.copy_from_slice(&alloy_primitives::keccak256("owner()")[..4]);
            let data = format!("{:?}", alloy_primitives::Bytes::from(cd.to_vec()));
            let call = json!([{ "to": addr, "data": data }, "latest"]);
            match http.call_raw("eth_call", call).await {
                Ok(v) => {
                    let owner = v.as_str().and_then(|s| s.strip_prefix("0x")).and_then(|h| {
                        let h = &h[h.len().saturating_sub(40)..];
                        h.parse::<alloy_primitives::Address>().ok()
                    });
                    match owner {
                        Some(o) => println!("· executor owner    {o:?} (verify: this must be the cold/operator wallet)"),
                        None => println!("· executor owner    unreadable ({v})"),
                    }
                }
                Err(e) => println!("· executor owner    read failed: {e}"),
            }
        }
        None => println!(
            "· executor          not set — the simulator mounts the constructor-equivalent fixture"
        ),
    }

    // Durable kill switch. A leftover trip in this file is the same class of
    // Day-0 surprise as an unwritable DB: the process will come up already
    // refusing every opportunity until POST /api/risk/reset. Only probe an
    // existing file — doctor must not create the database.
    {
        let db = std::path::Path::new(&cfg.api.db_path);
        if db.exists() {
            match mev_bot::store::Store::open(&cfg.api.db_path).and_then(|s| s.load_risk_state()) {
                Ok(state) if state.tripped => println!(
                    "✗ kill switch      durable trip persisted (cumulative {} wei{}) — \
                     POST /api/risk/reset to re-arm",
                    state.cumulative_net_wei,
                    state
                        .tripped_at_ms
                        .map(|t| format!(", at {t} ms"))
                        .unwrap_or_default()
                ),
                Ok(_) => println!("✓ kill switch      not tripped"),
                Err(e) => println!("! kill switch      could not read: {e}"),
            }
        } else {
            println!("· kill switch      no database yet");
        }
    }

    // The qualification clock lives in this file; if the directory is not
    // writable the clock silently never advances.
    {
        let db = std::path::Path::new(&cfg.api.db_path);
        match std::fs::metadata(db) {
            Ok(m) => println!("· database          {} ({} bytes)", db.display(), m.len()),
            Err(_) => println!(
                "· database          {} (created on first run)",
                db.display()
            ),
        }
        let parent = db
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(std::path::Path::new("."));
        let probe = parent.join(".doctor-write-probe");
        match std::fs::write(&probe, b"ok").and_then(|_| std::fs::remove_file(&probe)) {
            Ok(()) => println!("✓ database dir      {} writable", parent.display()),
            Err(e) => {
                println!(
                    "✗ database dir      {} not writable: {e} — qualification cannot persist",
                    parent.display()
                );
                hard_failures.push(format!(
                    "database dir {} not writable: {e}",
                    parent.display()
                ));
            }
        }
    }

    // Ghost names from older checklists. They are not read by Config::from_env,
    // so a leftover MIN_NET_PROFIT_ETH=0.005 would leave MIN_NET_PROFIT_WEI at
    // its 1-wei default. `validate` refuses to boot when any of these is set;
    // doctor reports them here so the operator sees the names next to the
    // rest of the Day-0 photograph.
    {
        let ignored = mev_bot::config::ignored_env_aliases();
        if ignored.is_empty() {
            println!("✓ env names         canonical wei/bps names only");
        } else {
            for alias in &ignored {
                println!(
                    "✗ env names         {} is set but unused — the bot reads {}",
                    alias.name, alias.canonical
                );
            }
        }
    }

    // Day-0 state photograph: everything the money switch depends on.
    let understands = std::env::var("I_UNDERSTAND_LIVE_RISK").unwrap_or_else(|_| "no".into());
    println!(
        "\nmode: {} | broadcast: {} | smoke: {} | I_UNDERSTAND_LIVE_RISK={understands} | inventory gate: {}",
        if cfg.live_execution {
            "LIVE"
        } else {
            "simulation"
        },
        if cfg.broadcast_enabled {
            "enabled"
        } else {
            "disabled"
        },
        if cfg.live_smoke_max > 0 {
            format!("LIVE_SMOKE_MAX={}", cfg.live_smoke_max)
        } else {
            "off".into()
        },
        cfg.inventory_gate,
    );
    println!(
        "risk: min net {} wei | base-fee ceiling {} wei | bribe {} bps | drawdown cap {} wei | \
         bundle gas {}",
        cfg.risk.min_net_profit_wei,
        cfg.risk.max_base_fee_wei,
        cfg.risk.bribe_bps,
        cfg.risk.max_drawdown_wei,
        cfg.risk.max_gas_per_bundle
    );
    if !hard_failures.is_empty() {
        println!(
            "\n✗ doctor FAILED ({} hard failure(s)):",
            hard_failures.len()
        );
        for f in &hard_failures {
            println!("  - {f}");
        }
        anyhow::bail!("doctor found {} hard failure(s)", hard_failures.len());
    }
    println!("\n✓ doctor passed: no hard failures");
    Ok(())
}

/// Offline Phase 1 harness: join stored simulations to relay bid traces.
fn replay(
    cfg: Arc<Config>,
    from_block: Option<u64>,
    to_block: Option<u64>,
    limit: i64,
    persist: bool,
) -> Result<()> {
    let store = mev_bot::store::Store::open(&cfg.api.db_path)?;
    let rows = mev_bot::replay::compare(&store, from_block, to_block, limit)?;
    print!("{}", mev_bot::replay::render(&rows));
    if persist {
        let n = mev_bot::replay::persist(&store, &rows)?;
        println!("persisted {n} reconciliation rows");
    }
    Ok(())
}
