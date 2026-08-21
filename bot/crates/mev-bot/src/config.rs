//! Configuration, entirely driven by environment variables (see `.env.example`).
//!
//! Everything has a safe default so the bot boots with only an RPC URL set. Risk
//! parameters intentionally default to a *liberal* profile: the point of the first
//! iteration is to observe as much MEV as possible, not to be selective.

use std::time::Duration;

use alloy_primitives::{address, Address, U256};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Well-known mainnet addresses used by the strategy engine.
pub mod known {
    use alloy_primitives::{address, Address};

    pub const WETH: Address = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
    pub const USDC: Address = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
    pub const USDT: Address = address!("dAC17F958D2ee523a2206206994597C13D831ec7");
    pub const DAI: Address = address!("6B175474E89094C44Da98b954EedeAC495271d0F");
    pub const WBTC: Address = address!("2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599");

    pub const UNIV2_FACTORY: Address = address!("5C69bEe701ef814a2B6a3EDD4B1652CB9cc5aA6f");
    pub const UNIV2_ROUTER: Address = address!("7a250d5630B4cF539739dF2C5dAcb4c659F2488D");
    pub const UNIV3_FACTORY: Address = address!("1F98431c8aD98523631AE4a59f267346ea31F984");
    pub const UNIV3_QUOTER_V2: Address = address!("61fFE014bA17989E743c5F6cB21bF9697530B21e");
    pub const UNIV3_NPM: Address = address!("C36442b4a4522E871399CD717aBDD847Ab11FE88");
    /// Original SwapRouter (`exactInputSingle` with a deadline). Selector `0x414bf389`.
    pub const UNIV3_SWAP_ROUTER: Address = address!("E592427A0AEce92De3Edee1F18E0157C05861564");
    /// SwapRouter02 (`exactInputSingle` without a deadline). Selector `0x04e45aaf`.
    pub const UNIV3_SWAP_ROUTER_02: Address = address!("68b3465833fb72A70ecDF485E0e4C7bD8665Fc45");
    /// UniversalRouter. `execute(bytes,bytes[])` is `0x24856bc3`;
    /// `execute(bytes,bytes[],uint256)` is `0x3593564c`.
    pub const UNIVERSAL_ROUTER: Address = address!("3fC91A3afd70395Cd496C647d5a6CC9D4B2b7FAD");
    pub const SUSHI_FACTORY: Address = address!("C0AEe478e3658e2610c5F7A4A2E1777cE9e4f2Ac");

    pub const BALANCER_VAULT: Address = address!("BA12222222228d8Ba445958a75a0704d566BF2C8");
    pub const AAVE_V3_POOL: Address = address!("87870Bca3F3fD6335C3F4ce8392D69350B4fA4E2");
    pub const AAVE_V3_ORACLE: Address = address!("54586bE62E3c3580375aE3723C145253060Ca0C2");
    pub const AAVE_V3_DATA_PROVIDER: Address = address!("7B4EB56E7CD4b454BA8ff71E4518426369a138a3");
    pub const COMPOUND_V3_USDC: Address = address!("c3d688B66703497DAA19211EEdff47f25384cdc3");
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Config {
    pub chain: ChainConfig,
    pub endpoints: Endpoints,
    pub risk: RiskConfig,
    pub strategies: StrategyToggles,
    pub sim: SimConfig,
    pub api: ApiConfig,
    /// Whether the V2 pool-discovery scan runs each block.
    pub pool_discovery: bool,
    /// Whether the UniswapV3 `PoolCreated` scan runs each block. On with
    /// `STRATEGY_SANDWICH_V3`: discovery without the strategy wastes an
    /// `eth_getLogs` per block; the strategy without discovery sees an empty
    /// `V3PoolCache` and emits nothing.
    pub pool_discovery_v3: bool,
    /// Decode Uniswap UniversalRouter `execute` calldata on the pending path.
    /// Off by default: Phase 2 W6 is gated on a week of funnel data showing a
    /// public-mempool gap, and turning this on expands the V2 sandwich / arb
    /// surface. The decoder itself is pure calldata parsing.
    pub decode_universal_router: bool,
    /// Longest cycle the atomic-arb search will consider, in legs.
    ///
    /// Default is 3 (the first post-funnel-week raise). 2 reproduces the
    /// original pair-to-pair search exactly. Raise further (up to
    /// `MAX_CYCLE_LEN`) only after live `atomic_arb.candidatesEmitted` on
    /// the same feed moves at 3. Every additional leg costs ~120k gas.
    pub arb_max_cycle_len: usize,
    /// Whether the bloXroute Max Profit relay's delivered blocks are fetched and
    /// their transactions ingested + scored. On by default; this is read-only
    /// (polling a public data API + the execution node), never a submission.
    pub relay_tx_ingest: bool,
    /// How many delivered-block transactions are scored concurrently.
    ///
    /// A mainnet block carries ~150–200 transactions and each one fans out to
    /// one task per strategy, so an unbounded backfill queues ~1000 tasks and a
    /// matching burst of RPC per block — enough to get the bot rate limited off
    /// its provider and starve the live mempool path. Replay work is never
    /// latency-critical (the block is already mined), so it runs behind this
    /// bound.
    pub relay_tx_concurrency: usize,
    /// When true, opportunities whose notional exceeds the searcher's ETH+WETH
    /// balance are skipped. Off by default so a dummy searcher in simulation
    /// mode does not silence the tape; forced on when live execution is on.
    pub inventory_gate: bool,
    /// Master switch. When false (the default and the only supported value today)
    /// the bot will *never* broadcast a transaction to a public node or a relay.
    pub live_execution: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChainConfig {
    pub chain_id: u64,
    pub name: String,
    pub weth: Address,
    /// Native/stable reference pool used to price gas in USD.
    pub usd_stable: Address,
    pub block_time_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Endpoints {
    pub http_url: String,
    pub ws_url: Option<String>,
    /// Flashbots MEV-Share SSE endpoint.
    pub mev_share_sse: String,
    /// Bundle relay used for `eth_callBundle` cross-checks.
    pub relay_url: String,
    /// Additional relays for data queries (bid traces / payloads delivered).
    pub relay_data_urls: Vec<String>,
    /// The bloXroute Max Profit relay used to pull delivered blocks and their
    /// transactions (see `RELAY_TX_INGEST`).
    pub bloxroute_relay_url: String,
    /// Optional L2 sequencer / preconfirmation feed (websocket).
    pub sequencer_feed: Option<String>,
    /// Optional Blocknative / Blockstream-style mempool stream.
    pub extra_mempool_ws: Vec<String>,
    /// Flashbots reputation key. Only used to sign the `X-Flashbots-Signature`
    /// header for read-only `eth_callBundle` requests.
    pub flashbots_signer_key: Option<String>,
    /// Executor contract address, if deployed.
    pub executor: Option<Address>,
    /// Address the simulated searcher trades from.
    pub searcher_address: Address,
}

/// Deliberately permissive to start: we want observations, not profit.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RiskConfig {
    /// Minimum *net* profit (after gas) in wei for an opportunity to be recorded
    /// as executable. 1 wei == "anything that is not a loss".
    pub min_net_profit_wei: U256,
    /// Maximum notional the bot may risk in a single bundle.
    pub max_position_wei: U256,
    /// Hard ceiling on gas price we will bid.
    pub max_base_fee_wei: U256,
    /// Share of gross profit offered to the builder, in bps.
    pub bribe_bps: u16,
    /// Maximum gas units a single bundle may consume.
    pub max_gas_per_bundle: u64,
    /// Stop opening new positions once cumulative simulated PnL drops below this
    /// (negative) number of wei. 0 disables the kill switch.
    pub max_drawdown_wei: U256,
    /// Per-strategy concurrency cap.
    pub max_inflight_per_strategy: usize,
    /// Skip opportunities whose simulated revert rate is above this.
    pub max_revert_rate: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StrategyToggles {
    pub sandwich: bool,
    /// V3 sandwich via QuoterV2. On by default after the funnel week,
    /// paired with `POOL_DISCOVERY_V3`.
    pub sandwich_v3: bool,
    pub jit: bool,
    pub atomic_arb: bool,
    pub liquidation: bool,
    pub sniper: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SimConfig {
    /// Path to the `anvil` binary used for forked simulation.
    pub anvil_bin: String,
    /// Port anvil listens on.
    pub anvil_port: u16,
    /// Port for the second anvil used to replay delivered blocks. Must differ
    /// from `anvil_port`: the two forks pin to different heights on purpose.
    pub anvil_replay_port: u16,
    /// Whether to spawn that second fork at all. Without it, delivered-block
    /// opportunities are recorded but not simulated — which is the honest
    /// outcome, since scoring them on the live fork would measure them against
    /// a state they never executed in.
    pub replay_fork: bool,
    /// Re-fork the simulator every N blocks.
    pub refork_every_blocks: u64,
    /// Also cross-check with the relay's `eth_callBundle`.
    pub use_call_bundle: bool,
    /// Number of blocks ahead the bundle targets.
    pub target_block_offset: u64,
    pub timeout: Duration,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ApiConfig {
    pub bind: String,
    pub db_path: String,
    /// How many events to keep in the in-memory ring buffer served to the UI.
    pub feed_capacity: usize,
}

fn env_opt(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

fn env_or(key: &str, default: &str) -> String {
    env_opt(key).unwrap_or_else(|| default.to_string())
}

fn env_u64(key: &str, default: u64) -> u64 {
    env_opt(key).and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn env_f64(key: &str, default: f64) -> f64 {
    env_opt(key).and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn env_bool(key: &str, default: bool) -> bool {
    match env_opt(key) {
        Some(v) => matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"),
        None => default,
    }
}

fn env_u256(key: &str, default: u128) -> U256 {
    match env_opt(key) {
        Some(v) => v.parse::<U256>().unwrap_or_else(|_| U256::from(default)),
        None => U256::from(default),
    }
}

fn env_addr(key: &str, default: Address) -> Address {
    env_opt(key)
        .and_then(|v| v.parse::<Address>().ok())
        .unwrap_or(default)
}

fn env_list(key: &str) -> Vec<String> {
    env_opt(key)
        .map(|v| {
            v.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

impl Config {
    /// Load configuration from the process environment (after `.env` is applied).
    pub fn from_env() -> Result<Self> {
        let http_url = env_opt("ETH_HTTP_URL")
            .context("ETH_HTTP_URL is required (an archive-capable mainnet RPC endpoint)")?;

        let chain_id = env_u64("CHAIN_ID", 1);

        Ok(Self {
            chain: ChainConfig {
                chain_id,
                name: env_or("CHAIN_NAME", "ethereum"),
                weth: env_addr("WETH_ADDRESS", known::WETH),
                usd_stable: env_addr("USD_STABLE_ADDRESS", known::USDC),
                block_time_ms: env_u64("BLOCK_TIME_MS", 12_000),
            },
            endpoints: Endpoints {
                http_url,
                ws_url: env_opt("ETH_WS_URL"),
                mev_share_sse: env_or("MEV_SHARE_SSE_URL", "https://mev-share.flashbots.net"),
                relay_url: env_or("FLASHBOTS_RELAY_URL", "https://relay.flashbots.net"),
                relay_data_urls: {
                    let v = env_list("RELAY_DATA_URLS");
                    if v.is_empty() {
                        vec![
                            "https://boost-relay.flashbots.net".into(),
                            "https://bloxroute.max-profit.blxrbdn.com".into(),
                            "https://agnostic-relay.net".into(),
                        ]
                    } else {
                        v
                    }
                },
                bloxroute_relay_url: env_or(
                    "BLOXROUTE_MAX_PROFIT_URL",
                    "https://bloxroute.max-profit.blxrbdn.com",
                ),
                sequencer_feed: env_opt("SEQUENCER_FEED_URL"),
                extra_mempool_ws: env_list("EXTRA_MEMPOOL_WS"),
                flashbots_signer_key: env_opt("FLASHBOTS_SIGNER_KEY"),
                executor: env_opt("EXECUTOR_ADDRESS").and_then(|v| v.parse().ok()),
                searcher_address: env_addr(
                    "SEARCHER_ADDRESS",
                    address!("00000000000000000000000000000000000f0000"),
                ),
            },
            risk: RiskConfig {
                // Liberal defaults: record anything at all that is net positive.
                min_net_profit_wei: env_u256("MIN_NET_PROFIT_WEI", 1),
                max_position_wei: env_u256("MAX_POSITION_WEI", 100_000_000_000_000_000_000), // 100 ETH
                max_base_fee_wei: env_u256("MAX_BASE_FEE_WEI", 500_000_000_000), // 500 gwei
                bribe_bps: env_u64("BRIBE_BPS", 9_000) as u16,
                max_gas_per_bundle: env_u64("MAX_GAS_PER_BUNDLE", 3_000_000),
                max_drawdown_wei: env_u256("MAX_DRAWDOWN_WEI", 0),
                max_inflight_per_strategy: env_u64("MAX_INFLIGHT_PER_STRATEGY", 32) as usize,
                max_revert_rate: env_f64("MAX_REVERT_RATE", 1.0),
            },
            strategies: StrategyToggles {
                sandwich: env_bool("STRATEGY_SANDWICH", true),
                sandwich_v3: env_bool("STRATEGY_SANDWICH_V3", true),
                jit: env_bool("STRATEGY_JIT", true),
                atomic_arb: env_bool("STRATEGY_ATOMIC_ARB", true),
                liquidation: env_bool("STRATEGY_LIQUIDATION", true),
                sniper: env_bool("STRATEGY_SNIPER", true),
            },
            sim: SimConfig {
                anvil_bin: env_or("ANVIL_BIN", "anvil"),
                anvil_port: env_u64("ANVIL_PORT", 8548) as u16,
                anvil_replay_port: {
                    // Two anvils cannot share a port. A misconfigured pair
                    // would fail at spawn time with a confusing bind error, so
                    // nudge it here instead.
                    let live = env_u64("ANVIL_PORT", 8548) as u16;
                    let replay = env_u64("ANVIL_REPLAY_PORT", 8549) as u16;
                    if replay == live {
                        live.saturating_add(1)
                    } else {
                        replay
                    }
                },
                replay_fork: env_bool("REPLAY_FORK", true),
                refork_every_blocks: env_u64("REFORK_EVERY_BLOCKS", 1),
                use_call_bundle: env_bool("USE_CALL_BUNDLE", true),
                target_block_offset: env_u64("TARGET_BLOCK_OFFSET", 1),
                timeout: Duration::from_millis(env_u64("SIM_TIMEOUT_MS", 2_500)),
            },
            api: ApiConfig {
                bind: env_or("API_BIND", "0.0.0.0:8080"),
                db_path: env_or("DB_PATH", "data/mev.sqlite"),
                feed_capacity: env_u64("FEED_CAPACITY", 2_000) as usize,
            },
            // Infrastructure toggle (not a strategy): scan PairCreated each block.
            pool_discovery: env_bool("POOL_DISCOVERY", true),
            pool_discovery_v3: env_bool("POOL_DISCOVERY_V3", true),
            decode_universal_router: env_bool("DECODE_UNIVERSAL_ROUTER", false),
            // Clamped to the enumerator's hard ceiling: config cannot talk the
            // search into an unbounded walk.
            arb_max_cycle_len: (env_u64("ARB_MAX_CYCLE_LEN", 3) as usize)
                .clamp(2, crate::dex::graph::MAX_CYCLE_LEN),
            // Infrastructure toggle: pull delivered blocks + transactions from the
            // bloXroute Max Profit relay and score them for extractable value.
            relay_tx_ingest: env_bool("RELAY_TX_INGEST", true),
            relay_tx_concurrency: (env_u64("RELAY_TX_CONCURRENCY", 16) as usize).max(1),
            inventory_gate: {
                let live = env_bool("LIVE_EXECUTION", false)
                    && env_or("I_UNDERSTAND_LIVE_RISK", "no") == "yes";
                env_bool("INVENTORY_GATE", false) || live
            },
            // Guarded by two independent switches so it cannot be flipped by accident.
            live_execution: env_bool("LIVE_EXECUTION", false)
                && env_or("I_UNDERSTAND_LIVE_RISK", "no") == "yes",
        })
    }

    pub fn summary(&self) -> String {
        format!(
            "chain={} ({}) ws={} mev_share={} call_bundle={} strategies=[{}] discovery={}/v3:{} ur={} arb_legs={} bloxroute_txs={} live={}",
            self.chain.name,
            self.chain.chain_id,
            self.endpoints.ws_url.is_some(),
            !self.endpoints.mev_share_sse.is_empty(),
            self.sim.use_call_bundle,
            self.strategies.enabled_names().join(","),
            self.pool_discovery,
            self.pool_discovery_v3,
            self.decode_universal_router,
            self.arb_max_cycle_len,
            self.relay_tx_ingest,
            self.live_execution
        )
    }
}

impl StrategyToggles {
    pub fn enabled_names(&self) -> Vec<&'static str> {
        let mut v = Vec::new();
        if self.sandwich {
            v.push("sandwich");
        }
        if self.sandwich_v3 {
            v.push("sandwich_v3");
        }
        if self.jit {
            v.push("jit");
        }
        if self.atomic_arb {
            v.push("atomic_arb");
        }
        if self.liquidation {
            v.push("liquidation");
        }
        if self.sniper {
            v.push("sniper");
        }
        v
    }
}
