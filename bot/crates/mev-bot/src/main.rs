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
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let _ = dotenvy::from_filename(&cli.env_file);

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with_target(true)
        .init();

    let cfg = Arc::new(Config::from_env()?);
    tracing::info!("{}", cfg.summary());
    if cfg.live_execution {
        tracing::warn!("LIVE EXECUTION IS ENABLED — bundles may be broadcast");
    } else {
        tracing::info!("simulation mode: no transaction will ever be broadcast");
    }

    match cli.command.unwrap_or(Command::Run) {
        Command::Doctor => doctor(cfg).await,
        Command::Api => {
            let engine = Arc::new(Engine::new(cfg.clone()).await?);
            api::serve(engine, &cfg.api.bind).await
        }
        Command::Run => {
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

    let http = RpcClient::new(cfg.endpoints.http_url.clone())?;
    match http.call_raw("eth_blockNumber", json!([])).await {
        Ok(v) => println!("✓ http rpc          {} (head {})", cfg.endpoints.http_url, v),
        Err(e) => println!("✗ http rpc          {}: {e}", cfg.endpoints.http_url),
    }

    match http.call_raw("eth_chainId", json!([])).await {
        Ok(v) => println!("✓ chain id          {v}"),
        Err(e) => println!("✗ chain id          {e}"),
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
                println!("! raw tx access     unsupported ({e}) — sandwich/JIT sims will be skipped");
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

    match tokio::process::Command::new(&cfg.sim.anvil_bin)
        .arg("--version")
        .output()
        .await
    {
        Ok(o) => println!(
            "✓ anvil             {}",
            String::from_utf8_lossy(&o.stdout).trim()
        ),
        Err(e) => println!("✗ anvil             {} not runnable: {e}", cfg.sim.anvil_bin),
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

    println!("\nmode: {}", if cfg.live_execution { "LIVE" } else { "simulation" });
    Ok(())
}
