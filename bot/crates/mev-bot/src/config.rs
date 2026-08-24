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

    use super::{QualificationBackend, SubmissionMode};
    use crate::dex::Venue;

    // ── Ethereum mainnet values (kept as constants: tests, the ethereum()
    //    profile, and the pre-registry call sites all reference them). ──

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

    pub const WSTETH: Address = address!("7f39C581F595B53c5cb19bD0b3f8dA6c935E2Ca0");

    pub const BALANCER_VAULT: Address = address!("BA12222222228d8Ba445958a75a0704d566BF2C8");
    pub const AAVE_V3_POOL: Address = address!("87870Bca3F3fD6335C3F4ce8392D69350B4fA4E2");
    pub const AAVE_V3_ORACLE: Address = address!("54586bE62E3c3580375aE3723C145253060Ca0C2");
    pub const AAVE_V3_DATA_PROVIDER: Address = address!("7B4EB56E7CD4b454BA8ff71E4518426369a138a3");
    pub const COMPOUND_V3_USDC: Address = address!("c3d688B66703497DAA19211EEdff47f25384cdc3");
    pub const MORPHO_BLUE: Address = address!("BBBBBbbBBb9cC5e90e3b3Af64bdAF62C37EEFFCb");

    /// Chainlink ETH/USD and BTC/USD proxy aggregators — the feeds Aave,
    /// Compound V3 and Morpho markets most commonly price collateral with.
    /// The *aggregator* (what `transmit` targets) is resolved at runtime;
    /// proxies are stable for years, aggregators rotate.
    pub const CHAINLINK_ETH_USD_PROXY: Address =
        address!("5f4eC3Df9cbd43714FE2740f5E3616155c5b8419");
    pub const CHAINLINK_BTC_USD_PROXY: Address =
        address!("F4030086522a5bEEa4988F8cA5B36dbC97BeE88c");

    // ── Base (chain 8453) values ──────────────────────────────────────────
    // Verified live against Base mainnet 2026-08-23 (code presence,
    // immutable getters where available, and official deployment registries).
    // Re-verify before deployment; every entry is env-overridable.

    pub const BASE_WETH: Address = address!("4200000000000000000000000000000000000006");
    /// Native (non-bridged) USDC on Base.
    pub const BASE_USDC: Address = address!("833589fCD6eDb6E08f4c7C32D4f71b54bdA02913");
    pub const BASE_DAI: Address = address!("50c5725949A6F0c72E6C4a641F24049A917DB0Cb");
    /// Coinbase wrapped BTC ("cbBTC").
    pub const BASE_WBTC: Address = address!("cbB7C0000aB88B473b1f5aFd9ef808440eed33Bf");
    /// Circle's USDbC (the USDC bridged through Circle CCTP).
    pub const BASE_USDBC: Address = address!("d9aAEc86B65D86f6A7B5B1b0c42FFA531710b6CA");
    pub const BASE_UNIV2_FACTORY: Address = address!("8909Dc15e40173Ff4699343b6eB8132c65e18eC6");
    /// `factory()` + `WETH()` getters verified live 2026-08-23 against Base
    /// mainnet via dRPC.
    pub const BASE_UNIV2_ROUTER: Address = address!("4752ba5DBc23f44D87826276BF6Fd6b1C372aD24");
    pub const BASE_UNIV3_FACTORY: Address = address!("33128a8fC17869897dcE68Ed026d694621f6FDfD");
    /// `factory()` + `WETH9()` getters verified live 2026-08-23.
    pub const BASE_UNIV3_QUOTER_V2: Address = address!("3d4e44Eb1374240CE5F1B871ab261CD16335B76a");
    pub const BASE_UNIV3_NPM: Address = address!("03a520b32C04BF3bEEf7BEb72E919cf822Ed34f1");
    /// Code verified live 2026-08-23; address matches Uniswap's official Base
    /// deployments registry and the Basescan verified source.
    pub const BASE_UNIVERSAL_ROUTER: Address = address!("6fF5693b99212Da76ad316178A184AB56D299b43");
    /// SwapRouter02. `factory()` + `WETH9()` getters verified live 2026-08-23.
    pub const BASE_UNIV3_SWAP_ROUTER_02: Address =
        address!("2626664c2603336E57B271c5C0b26F421741e481");
    /// Balancer V2 vault — deployed at the *same* address as mainnet. Code,
    /// non-zero `getProtocolFeesCollector()`, and Base WETH getter verified
    /// live 2026-08-23.
    pub const BASE_BALANCER_VAULT: Address = address!("BA12222222228d8Ba445958a75a0704d566BF2C8");
    /// Aerodrome router — registered for the phase-2 integration; unused by
    /// any v1 strategy (its Solidly-style math is out of scope, see the
    /// Base work order §7).
    pub const BASE_AERODROME_ROUTER: Address = address!("cF77a3Ba9A5CA399B7c97c74d54e5b1Beb874E43");

    /// Collateral tokens whose feeds the oracle front-runner maps leads to.
    pub fn collateral_universe() -> [Address; 3] {
        [WETH, WBTC, WSTETH]
    }

    // ── Chain address registry ────────────────────────────────────────────

    /// The per-chain address table the strategies, discovery and simulator
    /// read at runtime. Selected by `CHAIN_ID` at boot, then overridden
    /// field-by-field by env (see `Config::from_env`) — a new chain with
    /// env overrides needs no code change.
    ///
    /// `None` fields are *capabilities, not errors*: a strategy whose
    /// protocol is absent on a chain is skipped at construction with a
    /// warning (fail-loud, not fail-wrong — a mainnet Aave address on Base
    /// would simply simulate garbage).
    #[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
    pub struct ChainAddresses {
        pub chain_id: u64,
        // Core tokens
        pub weth: Address,
        pub usdc: Address,
        pub usdt: Option<Address>,
        pub dai: Option<Address>,
        pub wbtc: Option<Address>,
        pub wsteth: Option<Address>,
        // AMMs
        pub univ2_factory: Option<Address>,
        pub univ2_router: Option<Address>,
        pub univ3_factory: Option<Address>,
        pub univ3_quoter_v2: Option<Address>,
        pub univ3_npm: Option<Address>,
        pub univ3_swap_router: Option<Address>,
        pub univ3_swap_router_02: Option<Address>,
        pub universal_router: Option<Address>,
        pub sushi_factory: Option<Address>,
        pub balancer_vault: Option<Address>,
        /// Registry-only (no v1 strategy reads it); recorded so a future
        /// Aerodrome integration finds the router without re-verifying.
        pub aerodrome_router: Option<Address>,
        // Lending protocols (None ⇒ that liquidation strategy is
        // unavailable on this chain and is skipped at construction)
        pub aave_v3_pool: Option<Address>,
        pub aave_v3_oracle: Option<Address>,
        pub aave_v3_data_provider: Option<Address>,
        pub compound_v3_usdc: Option<Address>,
        pub morpho_blue: Option<Address>,
        /// Whether the built-in Maker ilk table (`strategies::liquidation_maker::maker`)
        /// applies to this chain.
        pub maker: bool,
        // Arb core pairs: non-WETH tokens the block-cadence arb keeps
        // loaded on every venue. Entries past `core_tokens_len` are padding.
        pub core_tokens: [Address; 4],
        pub core_tokens_len: u8,
        // Chain behaviour
        /// No public mempool: all flow arrives via the sequencer feed and is
        /// back-run-only. Sandwich/JIT are constructible but warn at boot;
        /// the relay-shaped env is inert.
        pub sequencer_only: bool,
        /// Default `REFORK_EVERY_BLOCKS`. Mainnet blocks are 12 s, Base 2 s —
        /// a per-block refork on Base is 6× the mainnet refork rate for the
        /// same wall-clock cost.
        pub refork_default: u64,
        /// Default submission transport for this chain.
        pub submission_default: SubmissionMode,
        /// Default qualification comparison backend for this chain.
        pub qualification_default: QualificationBackend,
    }

    impl ChainAddresses {
        /// The arb core-pair list (non-WETH), ready to iterate.
        pub fn core_tokens(&self) -> Vec<Address> {
            self.core_tokens[..self.core_tokens_len as usize].to_vec()
        }

        /// V2 factories to scan for `PairCreated`, in venue order.
        pub fn pair_factories(&self) -> Vec<(Venue, Address)> {
            let mut out = Vec::new();
            if let Some(f) = self.univ2_factory {
                out.push((Venue::UniV2, f));
            }
            if let Some(f) = self.sushi_factory {
                out.push((Venue::SushiV2, f));
            }
            out
        }
    }

    /// Ethereum mainnet (chain 1) registry.
    pub fn ethereum() -> &'static ChainAddresses {
        static PROFILE: std::sync::OnceLock<ChainAddresses> = std::sync::OnceLock::new();
        PROFILE.get_or_init(|| ChainAddresses {
            chain_id: 1,
            weth: WETH,
            usdc: USDC,
            usdt: Some(USDT),
            dai: Some(DAI),
            wbtc: Some(WBTC),
            wsteth: Some(WSTETH),
            univ2_factory: Some(UNIV2_FACTORY),
            univ2_router: Some(UNIV2_ROUTER),
            univ3_factory: Some(UNIV3_FACTORY),
            univ3_quoter_v2: Some(UNIV3_QUOTER_V2),
            univ3_npm: Some(UNIV3_NPM),
            univ3_swap_router: Some(UNIV3_SWAP_ROUTER),
            univ3_swap_router_02: Some(UNIV3_SWAP_ROUTER_02),
            universal_router: Some(UNIVERSAL_ROUTER),
            sushi_factory: Some(SUSHI_FACTORY),
            balancer_vault: Some(BALANCER_VAULT),
            aerodrome_router: None,
            aave_v3_pool: Some(AAVE_V3_POOL),
            aave_v3_oracle: Some(AAVE_V3_ORACLE),
            aave_v3_data_provider: Some(AAVE_V3_DATA_PROVIDER),
            compound_v3_usdc: Some(COMPOUND_V3_USDC),
            morpho_blue: Some(MORPHO_BLUE),
            maker: true,
            core_tokens: [USDC, USDT, DAI, WBTC],
            core_tokens_len: 4,
            sequencer_only: false,
            refork_default: 1,
            submission_default: SubmissionMode::Bundle,
            qualification_default: QualificationBackend::Relay,
        })
    }

    /// Base mainnet (chain 8453) registry. See the individual constants above
    /// for the verification state of each row.
    ///
    /// Current Base scope: V2 graph measurement plus shadow-only V3 discovery.
    /// Pending V2 backruns require two registered V2 venues and raw mode cannot
    /// yet express a preconfirmed dependency; see the successor work order.
    /// Lending-protocol rows are deliberately `None` (Aave/Morpho exist on
    /// Base; wiring them is the documented phase 2, and until then the
    /// strategies skip at construction with a warning).
    pub fn base() -> &'static ChainAddresses {
        static PROFILE: std::sync::OnceLock<ChainAddresses> = std::sync::OnceLock::new();
        PROFILE.get_or_init(|| ChainAddresses {
            chain_id: 8453,
            weth: BASE_WETH,
            usdc: BASE_USDC,
            usdt: None,
            dai: Some(BASE_DAI),
            wbtc: Some(BASE_WBTC),
            wsteth: None,
            univ2_factory: Some(BASE_UNIV2_FACTORY),
            univ2_router: Some(BASE_UNIV2_ROUTER),
            univ3_factory: Some(BASE_UNIV3_FACTORY),
            univ3_quoter_v2: Some(BASE_UNIV3_QUOTER_V2),
            univ3_npm: Some(BASE_UNIV3_NPM),
            univ3_swap_router: None,
            univ3_swap_router_02: Some(BASE_UNIV3_SWAP_ROUTER_02),
            universal_router: Some(BASE_UNIVERSAL_ROUTER),
            sushi_factory: None,
            balancer_vault: Some(BASE_BALANCER_VAULT),
            aerodrome_router: Some(BASE_AERODROME_ROUTER),
            aave_v3_pool: None,
            aave_v3_oracle: None,
            aave_v3_data_provider: None,
            compound_v3_usdc: None,
            morpho_blue: None,
            maker: false,
            // USDbC is a second USDC-class stable; the arb graph can route
            // through it once pool discovery finds the WETH pairs.
            core_tokens: [BASE_USDC, BASE_DAI, BASE_WBTC, BASE_USDBC],
            core_tokens_len: 4,
            sequencer_only: true,
            refork_default: 6,
            submission_default: SubmissionMode::Raw,
            qualification_default: QualificationBackend::Sequencer,
        })
    }

    /// The built-in registry for a chain id. `None` for chains without a
    /// profile — the operator supplies addresses via env overrides.
    pub fn for_chain(chain_id: u64) -> Option<&'static ChainAddresses> {
        match chain_id {
            1 => Some(ethereum()),
            8453 => Some(base()),
            _ => None,
        }
    }

    /// Apply env overrides to a profile, field-by-field: env wins.
    ///
    /// Pure (lookup injected) so the precedence is unit-testable without
    /// touching the process environment — the same pattern as
    /// `collect_ignored_env_aliases`.
    pub fn apply_address_overrides(
        mut a: ChainAddresses,
        lookup: impl Fn(&str) -> Option<String>,
    ) -> ChainAddresses {
        let override_addr = |env: &str| lookup(env).and_then(|v| v.parse::<Address>().ok());
        a.weth = override_addr("WETH_ADDRESS").unwrap_or(a.weth);
        a.usdc = override_addr("USD_STABLE_ADDRESS").unwrap_or(a.usdc);
        if let Some(v) = override_addr("USDT_ADDRESS") {
            a.usdt = Some(v);
        }
        if let Some(v) = override_addr("DAI_ADDRESS") {
            a.dai = Some(v);
        }
        if let Some(v) = override_addr("WBTC_ADDRESS") {
            a.wbtc = Some(v);
        }
        if let Some(v) = override_addr("WSTETH_ADDRESS") {
            a.wsteth = Some(v);
        }
        if let Some(v) = override_addr("UNIV2_FACTORY_ADDRESS") {
            a.univ2_factory = Some(v);
        }
        if let Some(v) = override_addr("UNIV2_ROUTER_ADDRESS") {
            a.univ2_router = Some(v);
        }
        if let Some(v) = override_addr("UNIV3_FACTORY_ADDRESS") {
            a.univ3_factory = Some(v);
        }
        if let Some(v) = override_addr("UNIV3_QUOTER_V2_ADDRESS") {
            a.univ3_quoter_v2 = Some(v);
        }
        if let Some(v) = override_addr("UNIV3_NPM_ADDRESS") {
            a.univ3_npm = Some(v);
        }
        if let Some(v) = override_addr("UNIV3_SWAP_ROUTER_ADDRESS") {
            a.univ3_swap_router = Some(v);
        }
        if let Some(v) = override_addr("UNIV3_SWAP_ROUTER_02_ADDRESS") {
            a.univ3_swap_router_02 = Some(v);
        }
        if let Some(v) = override_addr("UNIVERSAL_ROUTER_ADDRESS") {
            a.universal_router = Some(v);
        }
        if let Some(v) = override_addr("SUSHI_FACTORY_ADDRESS") {
            a.sushi_factory = Some(v);
        }
        if let Some(v) = override_addr("BALANCER_VAULT_ADDRESS") {
            a.balancer_vault = Some(v);
        }
        if let Some(v) = override_addr("AERODROME_ROUTER_ADDRESS") {
            a.aerodrome_router = Some(v);
        }
        if let Some(v) = override_addr("AAVE_V3_POOL_ADDRESS") {
            a.aave_v3_pool = Some(v);
        }
        if let Some(v) = override_addr("AAVE_V3_ORACLE_ADDRESS") {
            a.aave_v3_oracle = Some(v);
        }
        if let Some(v) = override_addr("AAVE_V3_DATA_PROVIDER_ADDRESS") {
            a.aave_v3_data_provider = Some(v);
        }
        if let Some(v) = override_addr("COMPOUND_V3_USDC_ADDRESS") {
            a.compound_v3_usdc = Some(v);
        }
        if let Some(v) = override_addr("MORPHO_BLUE_ADDRESS") {
            a.morpho_blue = Some(v);
        }
        a
    }
}

/// How signed payloads reach the chain.
///
/// `Bundle` is the Ethereum-mainnet path: `eth_sendBundle` to private
/// relays, same-UUID retries, `eth_cancelBundle` cancellation.
///
/// `Raw` is the sequencer-chain path (Base v1): there is no relay/builder
/// market — the signed transaction goes straight to the chain's RPC with a
/// priority fee, and cancellation is a replacement transaction (same nonce,
/// bumped fee) rather than `eth_cancelBundle`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SubmissionMode {
    Bundle,
    Raw,
}

impl SubmissionMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            SubmissionMode::Bundle => "bundle",
            SubmissionMode::Raw => "raw",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "bundle" => Some(SubmissionMode::Bundle),
            "raw" => Some(SubmissionMode::Raw),
            _ => None,
        }
    }
}

/// Where qualification's independent second opinion comes from.
///
/// `Relay` (mainnet): fork-vs-`eth_callBundle` — the builder's own
/// simulation of our payload.
///
/// `Sequencer` (Base): fork-vs-included-block — the victim's own
/// execution as the sequencer actually built it (canonical receipt) vs the
/// fork's replay of the same victim. No relay calls; the tolerance and
/// accuracy math is the same.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum QualificationBackend {
    Relay,
    Sequencer,
}

impl QualificationBackend {
    pub fn as_str(&self) -> &'static str {
        match self {
            QualificationBackend::Relay => "relay",
            QualificationBackend::Sequencer => "sequencer",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "relay" => Some(QualificationBackend::Relay),
            "sequencer" => Some(QualificationBackend::Sequencer),
            _ => None,
        }
    }
}

/// Liquidation-strategy tuning (Compound V3, Morpho Blue, Maker — the Aave
/// strategy predates this and stays untuned).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LiquidationConfig {
    /// Maximum accounts polled per protocol per block. Bounds the per-block
    /// `eth_call` fan-out; watchlists evict least-recently-active first.
    pub watch_cap: usize,
    /// Maximum Morpho markets tracked (most-recently-active first).
    pub morpho_market_cap: usize,
    /// Maximum borrowers tracked per Morpho market.
    pub morpho_borrower_cap: usize,
    /// Maker ilks to watch, from the built-in table (`ETH-A`, `WBTC-A`,
    /// `WSTETH-A`). Each ilk adds a log scan and urn reads per block, which
    /// is why the default is the single biggest one.
    pub maker_ilks: Vec<String>,
}

/// Alerting tunables (rules live in `alerts.rs`).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct AlertsConfig {
    /// No new head for this long is a critical endpoint/node failure.
    pub head_stall_secs: u64,
    /// No mempool transaction for this long while a WS feed is configured.
    pub pending_stall_secs: u64,
    /// A strategy whose live funnel converts under this percent (after the
    /// sample floor) trips the inclusion-collapse warning. 0 disables.
    pub min_conversion_pct: f64,
    /// Candidates a strategy needs before its conversion rate is judged.
    pub min_candidates: u64,
    /// Optional webhook (Slack/Discord-compatible JSON POST) for alert
    /// transitions.
    pub webhook_url: Option<String>,
    /// Seconds between evaluation passes.
    pub eval_secs: u64,
}

impl Default for AlertsConfig {
    fn default() -> Self {
        Self {
            head_stall_secs: 60,
            pending_stall_secs: 180,
            min_conversion_pct: 2.0,
            min_candidates: 100,
            webhook_url: None,
            eval_secs: 30,
        }
    }
}

/// Oracle-update front-runner tuning.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OracleConfig {
    /// `(Chainlink proxy, collateral token)` pairs to watch for updates.
    pub watch_feeds: Vec<(Address, Address)>,
    /// Maximum near-miss leads converted per oracle update; each becomes a
    /// simulation. Bounds the burst when a feed moves.
    pub max_leads: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Config {
    pub chain: ChainConfig,
    /// Per-chain address registry: the built-in profile for
    /// `chain.chain_id` with env overrides applied. Strategies, discovery
    /// and the simulator read this — never the `known::` constants.
    pub addresses: known::ChainAddresses,
    pub endpoints: Endpoints,
    pub risk: RiskConfig,
    pub strategies: StrategyToggles,
    /// Priority fee (wei) the bot bids on its own transactions.
    ///
    /// Mainnet (`SubmissionMode::Bundle`) it is the simulated leg's fee —
    /// builders price bundles by payment, not by the leg's fee, so the
    /// default (1 gwei) is a simulation artefact. On sequencer chains
    /// (`SubmissionMode::Raw`, e.g. Base) the priority fee *is* the
    /// ordering currency: this is the number that buys inclusion, and it is
    /// re-derived per chain (gas is ~10⁻⁴× mainnet).
    pub priority_fee_wei: U256,
    /// Percentage bump applied to both original EIP-1559 fee caps when raw
    /// cancellation replaces a pending transaction. 1,250 = 12.5%.
    pub raw_cancel_bump_bps: u64,
    /// Hard ceiling for a raw cancellation transaction's `maxFeePerGas`.
    /// If a valid replacement would exceed it, cancellation fails closed.
    pub raw_cancel_max_fee_wei: U256,
    /// How signed payloads reach the chain (relay bundles vs raw txs).
    /// Default follows the chain profile; `SUBMISSION_MODE` overrides.
    pub submission_mode: SubmissionMode,
    /// Qualification's independent comparison backend (relay
    /// `eth_callBundle` vs the sequencer's included block).
    /// Default follows the chain profile; `QUALIFICATION_BACKEND` overrides.
    pub qualification_backend: QualificationBackend,
    pub sim: SimConfig,
    /// Compound V3 / Morpho Blue / Maker liquidation tuning.
    pub liquidation: LiquidationConfig,
    /// Oracle-update front-runner tuning.
    pub oracle: OracleConfig,
    /// Alerting tunables.
    pub alerts: AlertsConfig,
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
    /// Include Uniswap V3 pools from the V3 discovery cache in the
    /// atomic-arb graph (V3↔V2 and V3↔V3 cycles). Off by default so a
    /// mainnet boot is byte-identical to the V2-only search; flip
    /// `DEX_UNIV3_ARB=true` to measure the surface. Quotes come from
    /// QuoterV2 (never a V2 approximation of concentrated liquidity).
    pub dex_univ3_arb: bool,
    /// Longest cycle the atomic-arb search will consider, in legs.
    ///
    /// Default is 3 (the first post-funnel-week raise). 2 reproduces the
    /// original pair-to-pair search exactly. Raise further (up to
    /// `MAX_CYCLE_LEN`) only after live `atomic_arb.candidatesEmitted` on
    /// the same feed moves at 3. Every additional leg costs ~120k gas.
    pub arb_max_cycle_len: usize,
    /// Whether the bloXroute Max Profit relay's delivered blocks are fetched and
    /// their transactions ingested + scored. On by default on mainnet; this is
    /// read-only (polling a public data API + the execution node), never a
    /// submission. Defaulted off on sequencer chains — bloXroute's data API is
    /// mainnet-only and would bleed mainnet blocks into a Base funnel (R6).
    pub relay_tx_ingest: bool,
    /// Whether non-native profit tokens may be valued in ETH so that
    /// ETH-denominated gas can be netted against them.
    ///
    /// Off by default. While off, any opportunity whose `profit_token` is
    /// neither native ETH nor the chain's WETH fails closed at the simulation
    /// boundary with an "uncertified accounting" reason — the behaviour that
    /// pinned every liquidation strategy to shadow mode. Turning it on enables
    /// [`crate::valuation`], which prices the profit token with a block-pinned,
    /// executable, haircut quote. See `docs/STRATEGIES.md`.
    pub token_valuation: bool,
    /// Conservatism applied to every non-native valuation, in basis points
    /// (200 = 2%). Covers the slippage of the unwind that is not simulated.
    pub valuation_haircut_bps: u64,
    /// Whether delivered blocks are ingested directly from the chain
    /// (`newHeads` + `eth_getBlockByHash`) instead of a relay data API. This is
    /// the sequencer-chain equivalent of the bloXroute path: the sequencer's
    /// own built blocks, scored for extractable value and used as the
    /// "included block" second opinion for `QUALIFICATION_BACKEND=sequencer`.
    /// Defaulted on for sequencer chains, off for mainnet (which uses the
    /// relay path).
    pub chain_block_ingest: bool,
    /// How many delivered-block transactions are scored concurrently.
    ///
    /// A mainnet block carries ~150–200 transactions and each one fans out to
    /// one task per strategy, so an unbounded backfill queues ~1000 tasks and a
    /// matching burst of RPC per block — enough to get the bot rate limited off
    /// its provider and starve the live mempool path. Replay work is never
    /// latency-critical (the block is already mined), so it runs behind this
    /// bound.
    pub relay_tx_concurrency: usize,
    /// How many transactions may be inside the strategy fan-out at once.
    ///
    /// The live mempool path acquires this without waiting: when the engine is
    /// already saturated the transaction is shed and counted in
    /// `evaluationsShed` rather than queued. Queueing is the worse failure —
    /// the work would complete after the block it was aimed at.
    pub strategy_concurrency: usize,
    /// How many delivered blocks may be replayed concurrently.
    ///
    /// Each lane resets the shared replay fork to its own parent block, so
    /// lanes are only safe when the operator has provisioned one isolated
    /// replay fork each. Default 1 — the historical, always-correct value.
    pub replay_lanes: usize,
    /// How many delivered blocks may wait in the replay queue. Small on
    /// purpose: a deep queue only means scoring blocks long after they are
    /// interesting.
    pub replay_queue_depth: usize,
    /// Minimum blocks between pool-discovery passes. 1 = every block.
    pub pool_discovery_interval_blocks: u64,
    /// Minimum blocks between searcher inventory refreshes. 1 = every block.
    pub inventory_refresh_blocks: u64,
    /// Wall-clock budget for one atomic-arb cycle enumeration pass.
    pub arb_enumeration_budget: Duration,
    /// How many pools the arb search may consider in one pass.
    pub arb_max_pools: usize,
    /// When true, opportunities whose notional exceeds the searcher's ETH+WETH
    /// balance are skipped. Off by default so a dummy searcher in simulation
    /// mode does not silence the tape; forced on when live execution is on.
    pub inventory_gate: bool,
    /// Boot arming for the runtime mode switch.
    pub live_execution: bool,
    /// Third, independent capability gate. Even an armed + runtime-live process
    /// only shadow-records bundles unless this is true and qualification passes.
    pub broadcast_enabled: bool,
    /// Minimum continuously observed shadow-data window.
    pub qualification_hours: u64,
    /// Minimum successful fork samples required independently per strategy.
    pub qualification_min_samples: u64,
    /// Minimum relay comparisons required independently per strategy.
    pub qualification_min_relay_comparisons: u64,
    /// Minimum corresponding on-chain outcomes required independently per strategy.
    pub qualification_min_actual_matches: u64,
    /// Maximum relative error allowed for a comparison, in basis points.
    pub qualification_max_error_bps: u64,
    /// Fraction of comparisons that must be within tolerance, in basis points.
    pub qualification_min_accuracy_bps: u64,
    /// Largest permitted gap between canonical block observations.
    pub qualification_max_gap_secs: u64,
    /// Canonical confirmations required before execution outcomes become final.
    pub finality_depth: u64,
    /// Delay between same-UUID relay replacement attempts.
    pub submission_retry_ms: u64,
    /// Maximum relay submission attempts per bundle.
    pub submission_max_attempts: u64,
    /// How many live submissions may proceed *without* a strategy `PASS`.
    ///
    /// 0 (default) = off. Capped at [`LIVE_SMOKE_MAX_CAP`]. Still requires
    /// boot arming, `BROADCAST_ENABLED`, runtime live mode, risk, inventory,
    /// live-candidate engineering, and an exact-payload sim. Counts durable
    /// submission attempts in SQLite so a restart cannot refill the budget.
    /// Raw mode additionally reserves worst-case gas exposure. A persist
    /// failure refuses the send.
    pub live_smoke_max: u64,
    /// Durable worst-case gas exposure allowed for unqualified raw smoke
    /// sends, in wei. Raw smoke is disabled when this is zero, even when
    /// `LIVE_SMOKE_MAX` is non-zero. Bundle-mode smoke remains count-bounded
    /// because relay bundles have revert protection and are not raw sends.
    pub live_smoke_max_gas_cost_wei: U256,
}

/// Hard ceiling on `LIVE_SMOKE_MAX`. The point is one or two proving shots,
/// not a back door around the seven-day gate.
pub const LIVE_SMOKE_MAX_CAP: u64 = 5;

/// True while `used` is still strictly below `max`. `max == 0` is off.
pub fn smoke_allows(used: u64, max: u64) -> bool {
    max > 0 && used < max
}

/// Slots remaining. Saturates at 0.
pub fn smoke_remaining(used: u64, max: u64) -> u64 {
    max.saturating_sub(used)
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

#[derive(Clone, Serialize, Deserialize)]
pub struct Endpoints {
    // NOTE: `flashbots_signer_key` below is a secret. `Serialize` skips it and
    // the hand-written `Debug` below redacts it, so neither a serialised config
    // nor a stray `{:?}` can leak a live key.
    pub http_url: String,
    pub ws_url: Option<String>,
    /// Flashbots MEV-Share SSE endpoint.
    pub mev_share_sse: String,
    /// Bundle relay used for `eth_callBundle` cross-checks.
    pub relay_url: String,
    /// Relays/builders that receive live bundles after every independent safety
    /// and qualification gate passes.
    pub bundle_relay_urls: Vec<String>,
    /// Additional relays for data queries (bid traces / payloads delivered).
    pub relay_data_urls: Vec<String>,
    /// The bloXroute Max Profit relay used to pull delivered blocks and their
    /// transactions (see `RELAY_TX_INGEST`).
    pub bloxroute_relay_url: String,
    /// Optional L2 sequencer / preconfirmation feed (websocket).
    pub sequencer_feed: Option<String>,
    /// Base Flashblocks websocket (`newFlashblocks`).
    ///
    /// A Flashblock is a 200 ms sub-block preconfirmation; the full block is
    /// 2 s. Subscribing here is a 10x improvement in event resolution over
    /// `newHeads`, and on a chain with no public pending queue it is the
    /// earliest observable state a searcher can react to. Requires a
    /// Flashblocks-integrated provider — an ordinary Base endpoint will not
    /// serve the subscription. Ordering inside a sealed Flashblock is already
    /// final, so everything it yields is strictly back-run material.
    pub flashblocks_ws: Option<String>,
    /// Optional Blocknative / Blockstream-style mempool stream.
    pub extra_mempool_ws: Vec<String>,
    /// MEV Blocker searcher websocket (`wss://searchers.mevblocker.io`).
    /// Off unless set: it is private orderflow, and the transactions it
    /// carries are unsigned, so only back-runs can act on them.
    pub mev_blocker_ws: Option<String>,
    /// Flashbots reputation key. Only used to sign the `X-Flashbots-Signature`
    /// header for read-only `eth_callBundle` requests.
    ///
    /// **Secret.** Redacted from `Debug` and omitted from `Serialize` so it
    /// cannot reach a log line or an API response by accident — `Endpoints`
    /// derives both, and a single `tracing::debug!(?cfg)` added later would
    /// otherwise print a live key.
    #[serde(skip_serializing, default)]
    pub flashbots_signer_key: Option<String>,
    /// Funded EOA key that signs atomic-engine bundles and raw transactions.
    /// This is deliberately distinct from the unfunded Flashbots reputation key.
    #[serde(skip_serializing, default)]
    pub searcher_private_key: Option<String>,
    /// Optional, independently funded key for the directional SniperVault
    /// lane. It is never used by MevExecutor or the raw cancellation lane.
    #[serde(skip_serializing, default)]
    pub sniper_searcher_private_key: Option<String>,
    /// Executor contract address, if deployed.
    pub executor: Option<Address>,
    /// Address the atomic engine signs from.
    pub searcher_address: Address,
    /// Address the directional lane signs from. Defaults to the atomic
    /// searcher address when no dedicated key is configured, preserving the
    /// simulation-only legacy behaviour; live sniper execution requires the
    /// dedicated key in `Config::validate`.
    pub sniper_searcher_address: Address,
}

/// Hand-written so the signer key is redacted.
///
/// The derive would print it verbatim. That is a real risk on a struct that
/// is cloned into every task and sits one `tracing::debug!(?cfg)` away from a
/// log aggregator.
impl std::fmt::Debug for Endpoints {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Endpoints")
            .field("http_url", &self.http_url)
            .field("ws_url", &self.ws_url)
            .field("mev_share_sse", &self.mev_share_sse)
            .field("relay_url", &self.relay_url)
            .field("bundle_relay_urls", &self.bundle_relay_urls)
            .field("relay_data_urls", &self.relay_data_urls)
            .field("bloxroute_relay_url", &self.bloxroute_relay_url)
            .field("sequencer_feed", &self.sequencer_feed)
            .field("flashblocks_ws", &self.flashblocks_ws)
            .field("extra_mempool_ws", &self.extra_mempool_ws)
            .field("mev_blocker_ws", &self.mev_blocker_ws)
            .field(
                "flashbots_signer_key",
                &self.flashbots_signer_key.as_ref().map(|_| "<redacted>"),
            )
            .field(
                "searcher_private_key",
                &self.searcher_private_key.as_ref().map(|_| "<redacted>"),
            )
            .field(
                "sniper_searcher_private_key",
                &self
                    .sniper_searcher_private_key
                    .as_ref()
                    .map(|_| "<redacted>"),
            )
            .field("executor", &self.executor)
            .field("searcher_address", &self.searcher_address)
            .field("sniper_searcher_address", &self.sniper_searcher_address)
            .finish()
    }
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
    /// Compound V3 (Comet) absorb + storefront buy.
    pub liquidation_compound: bool,
    /// Morpho Blue full-close liquidations.
    pub liquidation_morpho: bool,
    /// Maker bark + atomic clip take.
    pub liquidation_maker: bool,
    /// Back-run oracle updates with near-miss liquidations.
    pub oracle_frontrun: bool,
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
    /// Depth of the deferred-write queue feeding the background SQLite
    /// writer. When it fills, telemetry writes are dropped (and counted)
    /// rather than blocking the searcher.
    pub write_queue_capacity: usize,
    /// Shared secret required on the *mutating* endpoints
    /// (`POST /api/mode`, `/api/risk`, `/api/risk/reset`).
    ///
    /// `None` (the default) leaves them open, which is fine only while the
    /// API is bound to loopback. When `API_BIND` listens on a non-loopback
    /// address the engine refuses to start without this set — see
    /// `Config::validate`. Presented as `Authorization: Bearer <token>`.
    pub auth_token: Option<String>,
    /// Origins allowed to call the API from a browser.
    ///
    /// Empty (the default) means "reflect nothing": the dashboard talks to the
    /// bot server-side through its own `/api/bot/*` proxy, so no browser
    /// origin needs direct access. Set `API_ALLOWED_ORIGINS` only if something
    /// really does call the bot from a page.
    pub allowed_origins: Vec<String>,
}

fn env_opt(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

fn env_or(key: &str, default: &str) -> String {
    env_opt(key).unwrap_or_else(|| default.to_string())
}

/// Whether a `host:port` bind string listens only on the loopback interface.
///
/// Unparseable or unresolvable hosts are treated as **not** loopback: this
/// gates a security check, so the ambiguous case must fail closed.
pub fn bind_is_loopback(bind: &str) -> bool {
    use std::net::{SocketAddr, ToSocketAddrs};
    if let Ok(addr) = bind.parse::<SocketAddr>() {
        return addr.ip().is_loopback();
    }
    match bind.to_socket_addrs() {
        Ok(mut addrs) => {
            // Every resolved address must be loopback, or a hostname pointing
            // at both 127.0.0.1 and a routable IP would slip through.
            let mut any = false;
            let all_loopback = addrs.all(|a| {
                any = true;
                a.ip().is_loopback()
            });
            any && all_loopback
        }
        Err(_) => false,
    }
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

/// Parse `proxy:collateral,proxy:collateral` into address pairs. Malformed
/// entries are dropped with a warning rather than failing the boot — one bad
/// address should not blind the whole oracle watch.
fn parse_feed_list(raw: &str) -> Vec<(Address, Address)> {
    raw.split(',')
        .filter_map(|pair| {
            let mut it = pair.trim().split(':');
            let proxy = it.next()?.trim().parse::<Address>().ok()?;
            let collateral = it.next()?.trim().parse::<Address>().ok()?;
            Some((proxy, collateral))
        })
        .collect()
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

/// Clamp `MAX_GAS_PER_BUNDLE` into `[21_000, MAX_TX_GAS_CEILING]`.
///
/// Pure so the boundaries are unit-testable without mutating the process
/// environment. The upper bound is the EIP-7825 per-transaction protocol
/// cap (16,777,216 gas, live since Fusaka 2025-12-03) — see
/// [`crate::sim::anvil::MAX_TX_GAS_CEILING`].
fn clamp_bundle_gas(raw: u64) -> u64 {
    raw.clamp(21_000, crate::sim::anvil::MAX_TX_GAS_CEILING)
}

/// Env names that operators historically set from older checklists.
///
/// They are **not** read by [`Config::from_env`]. If they are present the
/// canonical wei/bps knobs stay at their liberal defaults — a live-money
/// footgun (`MIN_NET_PROFIT_ETH=0.005` silently leaves `MIN_NET_PROFIT_WEI=1`).
/// `validate` refuses to boot when any of these is set; `doctor` prints them
/// as `✗`. See `docs/DAY0_RUNBOOK.md`.
pub const IGNORED_ENV_ALIASES: &[(&str, &str)] = &[
    ("MIN_NET_PROFIT_ETH", "MIN_NET_PROFIT_WEI"),
    ("MAX_BASE_FEE_GWEI", "MAX_BASE_FEE_WEI"),
    ("MAX_DRAWDOWN_ETH", "MAX_DRAWDOWN_WEI"),
    ("BUILDER_SHARE_BPS", "BRIBE_BPS"),
];

/// One unused alias found in the process environment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IgnoredEnvAlias {
    pub name: &'static str,
    pub canonical: &'static str,
}

/// Collect unused checklist aliases using an injected lookup so the mapping
/// is unit-testable without mutating the process environment.
pub fn collect_ignored_env_aliases(
    lookup: impl Fn(&str) -> Option<String>,
) -> Vec<IgnoredEnvAlias> {
    IGNORED_ENV_ALIASES
        .iter()
        .filter(|(name, _)| lookup(name).is_some())
        .map(|(name, canonical)| IgnoredEnvAlias { name, canonical })
        .collect()
}

/// Unused checklist aliases currently set in the process environment.
pub fn ignored_env_aliases() -> Vec<IgnoredEnvAlias> {
    collect_ignored_env_aliases(env_opt)
}

/// Human-readable reason `validate` / `doctor` print when aliases are set.
pub fn format_ignored_env_error(found: &[IgnoredEnvAlias]) -> String {
    let mut lines = vec![
        "the following environment variables are set but are not read by the bot.".to_string(),
        "They come from an older checklist and would silently leave the canonical \
         wei/bps knobs at their liberal defaults."
            .to_string(),
        String::new(),
    ];
    for alias in found {
        lines.push(format!(
            "  {} is set — use {} instead (this bot is wei/bps denominated)",
            alias.name, alias.canonical
        ));
    }
    lines.push(String::new());
    lines.push(
        "Remove the unused names from the env file (or rename them) and restart.".to_string(),
    );
    lines.join("\n")
}

/// Fail closed when unused aliases are present. Split out so the rule is
/// testable without constructing a whole `Config`.
pub fn validate_ignored_aliases(found: &[IgnoredEnvAlias]) -> Result<()> {
    if found.is_empty() {
        return Ok(());
    }
    anyhow::bail!("{}", format_ignored_env_error(found))
}

impl Config {
    /// Load configuration from the process environment (after `.env` is applied).
    pub fn from_env() -> Result<Self> {
        let chain_id = env_u64("CHAIN_ID", 1);
        // Keep the legacy `ETH_*` names for one-process deployments while
        // allowing an operator to keep Ethereum and Base bindings in the same
        // environment. The selected chain always wins; a missing selected
        // binding is a hard startup error rather than silently using the other
        // chain's RPC (which would contaminate state and qualification data).
        let (http_url, ws_url) = if chain_id == 8453 {
            (
                env_opt("BASE_HTTP_URL").or_else(|| env_opt("ETH_HTTP_URL")),
                env_opt("BASE_WS_URL").or_else(|| env_opt("ETH_WS_URL")),
            )
        } else {
            (env_opt("ETH_HTTP_URL"), env_opt("ETH_WS_URL"))
        };
        let http_url = http_url.context(if chain_id == 8453 {
            "BASE_HTTP_URL is required for chain 8453 (an archive-capable RPC endpoint)"
        } else {
            "ETH_HTTP_URL is required (an archive-capable RPC endpoint)"
        })?;

        // ── Chain address registry: built-in profile + env overrides ──
        // Env wins field-by-field, so a chain without a built-in profile is
        // fully configurable from the environment (no code change).
        let base_profile = known::for_chain(chain_id)
            .copied()
            .unwrap_or(known::ChainAddresses {
                chain_id,
                weth: Address::ZERO,
                usdc: Address::ZERO,
                usdt: None,
                dai: None,
                wbtc: None,
                wsteth: None,
                univ2_factory: None,
                univ2_router: None,
                univ3_factory: None,
                univ3_quoter_v2: None,
                univ3_npm: None,
                univ3_swap_router: None,
                univ3_swap_router_02: None,
                universal_router: None,
                sushi_factory: None,
                balancer_vault: None,
                aerodrome_router: None,
                aave_v3_pool: None,
                aave_v3_oracle: None,
                aave_v3_data_provider: None,
                compound_v3_usdc: None,
                morpho_blue: None,
                maker: false,
                core_tokens: [Address::ZERO; 4],
                core_tokens_len: 0,
                sequencer_only: false,
                refork_default: 1,
                submission_default: SubmissionMode::Bundle,
                qualification_default: QualificationBackend::Relay,
            });
        let addresses = known::apply_address_overrides(base_profile, env_opt);

        // WETH is the anchor of the whole engine (profit token default, gas
        // accounting, discovery). A chain without a profile and without an
        // explicit WETH_ADDRESS cannot run — fail closed, not at 0x0.
        if addresses.weth == Address::ZERO {
            anyhow::bail!(
                "WETH_ADDRESS is required for chain {chain_id} (no built-in address \
                 profile for this chain id)"
            );
        }

        let submission_mode = match env_opt("SUBMISSION_MODE") {
            Some(v) => SubmissionMode::parse(&v).ok_or_else(|| {
                anyhow::anyhow!("SUBMISSION_MODE must be 'bundle' or 'raw' (got {v:?})")
            })?,
            None => addresses.submission_default,
        };
        let qualification_backend = match env_opt("QUALIFICATION_BACKEND") {
            Some(v) => QualificationBackend::parse(&v).ok_or_else(|| {
                anyhow::anyhow!("QUALIFICATION_BACKEND must be 'relay' or 'sequencer' (got {v:?})")
            })?,
            None => addresses.qualification_default,
        };
        // A sequencer-only chain has no relays to send bundles to: the raw
        // transport is the only path that can work. Refuse the mismatch at
        // boot rather than arming a send path that cannot deliver.
        if addresses.sequencer_only && submission_mode == SubmissionMode::Bundle {
            anyhow::bail!(
                "SUBMISSION_MODE=bundle cannot work on chain {chain_id}: there is no \
                 public mempool or relay market — set SUBMISSION_MODE=raw"
            );
        }

        // Relay authentication and transaction signing are different trust
        // domains. Derive the searcher address from the funded transaction key
        // so nonce, balance, allowlist and raw signatures cannot drift apart.
        let searcher_private_key = env_opt("SEARCHER_PRIVATE_KEY");
        let tx_signer = match searcher_private_key.as_deref() {
            Some(key) => crate::signer::Signer::from_hex(key)
                .context("SEARCHER_PRIVATE_KEY is not a valid secp256k1 private key")?,
            None => crate::signer::Signer::simulation(),
        };
        let searcher_address = tx_signer.address();
        if let Some(raw) = env_opt("SEARCHER_ADDRESS") {
            if let Ok(configured) = raw.parse::<Address>() {
                // The old example shipped a dummy 0x…f0000 address. Ignore it
                // when migrating an unarmed simulation; any other mismatch is
                // a configuration error rather than something to guess around.
                let legacy_dummy = address!("00000000000000000000000000000000000f0000");
                if configured != legacy_dummy && configured != searcher_address {
                    anyhow::bail!(
                        "SEARCHER_ADDRESS ({configured:?}) does not match the address derived from \
                         SEARCHER_PRIVATE_KEY ({searcher_address:?}); remove SEARCHER_ADDRESS or fix the key"
                    );
                }
            }
        }

        // The directional lane may use a separate funded EOA. Falling back to
        // the atomic searcher address keeps shadow-mode and existing single-key
        // deployments compatible, but a live sniper lane is rejected unless a
        // dedicated private key is present (see `validate`).
        let sniper_searcher_private_key = env_opt("SNIPER_SEARCHER_PRIVATE_KEY");
        let sniper_signer = match sniper_searcher_private_key.as_deref() {
            Some(key) => Some(
                crate::signer::Signer::from_hex(key)
                    .context("SNIPER_SEARCHER_PRIVATE_KEY is not a valid secp256k1 private key")?,
            ),
            None => None,
        };
        let configured_sniper_address = env_opt("SNIPER_SEARCHER_ADDRESS")
            .map(|raw| {
                raw.parse::<Address>().with_context(|| {
                    format!("SNIPER_SEARCHER_ADDRESS is not a valid EVM address: {raw:?}")
                })
            })
            .transpose()?;
        if let (Some(signer), Some(configured)) = (&sniper_signer, configured_sniper_address) {
            if signer.address() != configured {
                anyhow::bail!(
                    "SNIPER_SEARCHER_ADDRESS ({configured:?}) does not match the address derived from SNIPER_SEARCHER_PRIVATE_KEY ({:?})",
                    signer.address()
                );
            }
        }
        let sniper_searcher_address = configured_sniper_address
            .or_else(|| sniper_signer.as_ref().map(crate::signer::Signer::address))
            .unwrap_or(searcher_address);

        let relay_url = env_or("FLASHBOTS_RELAY_URL", "https://relay.flashbots.net");
        let bundle_relay_urls = {
            let configured = env_list("BUNDLE_RELAY_URLS");
            if configured.is_empty() {
                vec![relay_url.clone()]
            } else {
                configured
            }
        };

        // Read before `addresses` moves into the struct literal below.
        let refork_default = addresses.refork_default;
        let sequencer_only = addresses.sequencer_only;

        let cfg = Self {
            chain: ChainConfig {
                chain_id,
                name: env_or("CHAIN_NAME", "ethereum"),
                // From the registry (profile + env override), not a
                // hardcoded default — a Base boot must not silently
                // anchor on the mainnet WETH.
                weth: addresses.weth,
                usd_stable: addresses.usdc,
                // Sequencer chains are ~2 s; defaulting them to mainnet's
                // 12 s silently mis-labels every latency histogram.
                block_time_ms: env_u64(
                    "BLOCK_TIME_MS",
                    if sequencer_only { 2_000 } else { 12_000 },
                ),
            },
            addresses,
            endpoints: Endpoints {
                http_url,
                ws_url,
                // Mainnet-shaped feeds default ON for mainnet and OFF for
                // sequencer chains: a Base instance must not silently
                // ingest mainnet MEV-Share hints or relay data — cross-chain
                // data bleed poisons the per-chain funnel and qualification
                // evidence (work order R6: the one unacceptable bug class).
                mev_share_sse: env_opt("MEV_SHARE_SSE_URL").unwrap_or_else(|| {
                    if sequencer_only {
                        String::new()
                    } else {
                        "https://mev-share.flashbots.net".into()
                    }
                }),
                relay_url,
                bundle_relay_urls,
                relay_data_urls: {
                    let v = env_list("RELAY_DATA_URLS");
                    if v.is_empty() {
                        // Mainnet's relay data APIs; sequencer chains have
                        // no equivalent and get nothing (no bleed).
                        if sequencer_only {
                            Vec::new()
                        } else {
                            vec![
                                "https://boost-relay.flashbots.net".into(),
                                "https://bloxroute.max-profit.blxrbdn.com".into(),
                                "https://agnostic-relay.net".into(),
                            ]
                        }
                    } else {
                        v
                    }
                },
                bloxroute_relay_url: env_or(
                    "BLOXROUTE_MAX_PROFIT_URL",
                    "https://bloxroute.max-profit.blxrbdn.com",
                ),
                sequencer_feed: env_opt("SEQUENCER_FEED_URL"),
                flashblocks_ws: env_opt("FLASHBLOCKS_WS_URL"),
                extra_mempool_ws: env_list("EXTRA_MEMPOOL_WS"),
                mev_blocker_ws: env_opt("MEV_BLOCKER_WS"),
                flashbots_signer_key: env_opt("FLASHBOTS_SIGNER_KEY"),
                searcher_private_key,
                sniper_searcher_private_key,
                executor: env_opt("EXECUTOR_ADDRESS").and_then(|v| v.parse().ok()),
                searcher_address,
                sniper_searcher_address,
            },
            risk: RiskConfig {
                // Liberal defaults: record anything at all that is net positive.
                min_net_profit_wei: env_u256("MIN_NET_PROFIT_WEI", 1),
                max_position_wei: env_u256("MAX_POSITION_WEI", 100_000_000_000_000_000_000), // 100 ETH
                max_base_fee_wei: env_u256("MAX_BASE_FEE_WEI", 500_000_000_000),             // 500 gwei
                bribe_bps: env_u64("BRIBE_BPS", 9_000) as u16,
                // Clamped into the same range the runtime patch validator
                // enforces — an out-of-range env value would otherwise be
                // echoed back by GET /api/risk and poison every dashboard
                // patch that re-sends it ("outside [21000, 16777216]").
                max_gas_per_bundle: {
                    let raw = env_u64("MAX_GAS_PER_BUNDLE", 3_000_000);
                    let clamped = clamp_bundle_gas(raw);
                    if clamped != raw {
                        eprintln!(
                            "MAX_GAS_PER_BUNDLE={raw} outside [21000, {}] — clamped to {clamped} \
                             (the value is a per-bundle gas cap, not a gas price)",
                            crate::sim::anvil::MAX_TX_GAS_CEILING
                        );
                    }
                    clamped
                },
                max_drawdown_wei: env_u256("MAX_DRAWDOWN_WEI", 0),
                max_inflight_per_strategy: env_u64("MAX_INFLIGHT_PER_STRATEGY", 32) as usize,
                max_revert_rate: env_f64("MAX_REVERT_RATE", 1.0),
            },
            priority_fee_wei: env_u256("PRIORITY_FEE_WEI", 1_000_000_000), // 1 gwei
            raw_cancel_bump_bps: env_u64("RAW_CANCEL_BUMP_BPS", 1_250).clamp(1_000, 10_000),
            raw_cancel_max_fee_wei: env_u256("RAW_CANCEL_MAX_FEE_WEI", 500_000_000_000),
            submission_mode,
            qualification_backend,
            strategies: StrategyToggles {
                sandwich: env_bool("STRATEGY_SANDWICH", true),
                sandwich_v3: env_bool("STRATEGY_SANDWICH_V3", true),
                jit: env_bool("STRATEGY_JIT", true),
                atomic_arb: env_bool("STRATEGY_ATOMIC_ARB", true),
                liquidation: env_bool("STRATEGY_LIQUIDATION", true),
                liquidation_compound: env_bool("STRATEGY_LIQUIDATION_COMPOUND", true),
                liquidation_morpho: env_bool("STRATEGY_LIQUIDATION_MORPHO", true),
                liquidation_maker: env_bool("STRATEGY_LIQUIDATION_MAKER", true),
                oracle_frontrun: env_bool("STRATEGY_ORACLE_FRONTRUN", true),
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
                // Per-chain default: mainnet reforks every block (12 s
                // cadence); Base defaults to every 6 blocks (2 s cadence)
                // for the same wall-clock refork rate.
                refork_every_blocks: env_u64("REFORK_EVERY_BLOCKS", refork_default),
                use_call_bundle: env_bool("USE_CALL_BUNDLE", !sequencer_only),
                target_block_offset: env_u64("TARGET_BLOCK_OFFSET", 1),
                timeout: Duration::from_millis(env_u64("SIM_TIMEOUT_MS", 2_500)),
            },
            liquidation: LiquidationConfig {
                watch_cap: (env_u64("LIQUIDATION_WATCH_CAP", 200) as usize).max(1),
                morpho_market_cap: (env_u64("MORPHO_MARKET_CAP", 24) as usize).max(1),
                morpho_borrower_cap: (env_u64("MORPHO_BORROWER_CAP", 64) as usize).max(1),
                maker_ilks: {
                    let v = env_list("MAKER_ILKS");
                    if v.is_empty() { vec!["ETH-A".to_string()] } else { v }
                },
            },
            alerts: AlertsConfig {
                head_stall_secs: env_u64("ALERT_HEAD_STALL_SECS", 60),
                pending_stall_secs: env_u64("ALERT_PENDING_STALL_SECS", 180),
                min_conversion_pct: env_f64("ALERT_MIN_CONVERSION_PCT", 2.0),
                min_candidates: env_u64("ALERT_MIN_CANDIDATES", 100),
                webhook_url: env_opt("ALERT_WEBHOOK_URL"),
                eval_secs: env_u64("ALERT_EVAL_SECS", 30).max(5),
            },
            oracle: OracleConfig {
                watch_feeds: match env_opt("ORACLE_WATCH_FEEDS") {
                    Some(raw) => parse_feed_list(&raw),
                    None if sequencer_only => Vec::new(),
                    None => parse_feed_list(
                        "0x5f4eC3Df9cbd43714FE2740f5E3616155c5b8419:0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2,\
                         0xF4030086522a5bEEa4988F8cA5B36dbC97BeE88c:0x2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599",
                    ),
                },
                max_leads: (env_u64("ORACLE_FRONTRUN_MAX_LEADS", 3) as usize).max(1),
            },
            api: ApiConfig {
                // Loopback by default: the API has mutating endpoints and no
                // auth unless `API_AUTH_TOKEN` is set, so the out-of-the-box
                // configuration must not be reachable from the network. The
                // dashboard proxies server-side, so it is unaffected.
                // Containers that need to publish the port set `API_BIND`
                // explicitly (see deploy/docker-compose.yml) and must then
                // also set `API_AUTH_TOKEN`.
                bind: env_or("API_BIND", "127.0.0.1:8080"),
                db_path: env_or("DB_PATH", "data/mev.sqlite"),
                feed_capacity: env_u64("FEED_CAPACITY", 2_000) as usize,
                write_queue_capacity: (env_u64("WRITE_QUEUE_CAPACITY", 8_192) as usize).max(64),
                auth_token: env_opt("API_AUTH_TOKEN").filter(|t| !t.is_empty()),
                allowed_origins: env_list("API_ALLOWED_ORIGINS"),
            },
            // Infrastructure toggle (not a strategy): scan PairCreated each block.
            pool_discovery: env_bool("POOL_DISCOVERY", true),
            pool_discovery_v3: env_bool("POOL_DISCOVERY_V3", true),
            decode_universal_router: env_bool("DECODE_UNIVERSAL_ROUTER", false),
            dex_univ3_arb: env_bool("DEX_UNIV3_ARB", false),
            // Clamped to the enumerator's hard ceiling: config cannot talk the
            // search into an unbounded walk.
            arb_max_cycle_len: (env_u64("ARB_MAX_CYCLE_LEN", 3) as usize)
                .clamp(2, crate::dex::graph::MAX_CYCLE_LEN),
            // Infrastructure toggle: pull delivered blocks + transactions from the
            // bloXroute Max Profit relay and score them for extractable value.
            // Chain-appropriate defaults: mainnet polls bloXroute; a
            // sequencer chain polls its own blocks (no relay exists there,
            // and the mainnet data API would bleed cross-chain).
            relay_tx_ingest: env_bool("RELAY_TX_INGEST", !sequencer_only),
            chain_block_ingest: env_bool("CHAIN_BLOCK_INGEST", sequencer_only),
            // Fail-closed: a non-native profit token is uncertified until an
            // operator explicitly enables valuation.
            token_valuation: env_bool("TOKEN_VALUATION", false),
            valuation_haircut_bps: env_u64(
                "VALUATION_HAIRCUT_BPS",
                crate::valuation::DEFAULT_HAIRCUT_BPS,
            )
            .min(10_000),
            relay_tx_concurrency: (env_u64("RELAY_TX_CONCURRENCY", 16) as usize).max(1),
            // Sized for the live path: enough in-flight transactions to keep
            // the runtime busy, low enough that a mempool burst sheds instead
            // of building a queue of work that is stale on arrival.
            strategy_concurrency: (env_u64("STRATEGY_CONCURRENCY", 64) as usize).max(1),
            // Only raise past 1 with one isolated replay fork per lane.
            replay_lanes: (env_u64("REPLAY_LANES", 1) as usize).max(1),
            replay_queue_depth: (env_u64("REPLAY_QUEUE_DEPTH", 4) as usize).max(1),
            pool_discovery_interval_blocks: env_u64("POOL_DISCOVERY_INTERVAL_BLOCKS", 1).max(1),
            inventory_refresh_blocks: env_u64("INVENTORY_REFRESH_BLOCKS", 1).max(1),
            // Discovery plus strategies share a ~50 ms/block budget on the
            // block task; enumeration gets half of it by default.
            arb_enumeration_budget: Duration::from_millis(
                env_u64("ARB_ENUMERATION_BUDGET_MS", 25).max(1),
            ),
            arb_max_pools: (env_u64("ARB_MAX_POOLS", 200) as usize).max(2),
            inventory_gate: {
                let live = env_bool("LIVE_EXECUTION", false)
                    && env_or("I_UNDERSTAND_LIVE_RISK", "no") == "yes";
                env_bool("INVENTORY_GATE", false) || live
            },
            // Guarded by two independent switches so it cannot be flipped by accident.
            live_execution: env_bool("LIVE_EXECUTION", false)
                && env_or("I_UNDERSTAND_LIVE_RISK", "no") == "yes",
            broadcast_enabled: env_bool("BROADCAST_ENABLED", false),
            qualification_hours: env_u64("QUALIFICATION_HOURS", 168).max(1),
            qualification_min_samples: env_u64("QUALIFICATION_MIN_SAMPLES", 30).max(1),
            qualification_min_relay_comparisons: env_u64(
                "QUALIFICATION_MIN_RELAY_COMPARISONS",
                30,
            )
            .max(1),
            qualification_min_actual_matches: env_u64("QUALIFICATION_MIN_ACTUAL_MATCHES", 30)
                .max(1),
            qualification_max_error_bps: env_u64("QUALIFICATION_MAX_ERROR_BPS", 2_000)
                .clamp(1, 10_000),
            qualification_min_accuracy_bps: env_u64("QUALIFICATION_MIN_ACCURACY_BPS", 8_000)
                .clamp(1, 10_000),
            qualification_max_gap_secs: env_u64("QUALIFICATION_MAX_GAP_SECS", 120).max(15),
            finality_depth: env_u64("FINALITY_DEPTH", 12).max(1),
            submission_retry_ms: env_u64("SUBMISSION_RETRY_MS", 250).max(50),
            submission_max_attempts: env_u64("SUBMISSION_MAX_ATTEMPTS", 2).clamp(1, 5),
            live_smoke_max: env_u64("LIVE_SMOKE_MAX", 0).min(LIVE_SMOKE_MAX_CAP),
            live_smoke_max_gas_cost_wei: env_u256("LIVE_SMOKE_MAX_GAS_COST_WEI", 0),
        };
        // NOTE: `validate()` is deliberately NOT called here. `doctor` and
        // `replay` load the config without ever binding a port, and they must
        // keep working (and *reporting*) on a configuration that would be
        // unsafe to serve. The commands that actually listen call it.
        Ok(cfg)
    }

    /// Safety checks for the commands that actually bind the API port
    /// (`run`, `api`). Deliberately not run by `from_env`, so read-only
    /// commands like `doctor` can inspect and report on an unsafe config
    /// instead of refusing to run.
    ///
    /// Currently one rule, and it exists because the default configuration is
    /// genuinely dangerous on a shared network: whenever `API_BIND` is set to
    /// a routable address (containers must, to be reachable across the compose
    /// network) the API exposes **mutating** endpoints
    /// (`POST /api/mode`, `/api/risk`, `/api/risk/reset`) with no
    /// authentication. Anyone who can reach the port can trip the kill switch,
    /// disable every strategy, or set `bribeBps` to 10000 so the builder takes
    /// 100% of gross.
    ///
    /// Rather than change the default bind (which would break every existing
    /// container deployment that maps the port), the bot now refuses to start
    /// when it would listen on a non-loopback address without
    /// `API_AUTH_TOKEN`. Loopback-only setups are unaffected.
    pub fn validate(&self) -> Result<()> {
        validate_api(&self.api)?;
        validate_ignored_aliases(&ignored_env_aliases())?;
        if self.broadcast_enabled && !self.live_execution {
            anyhow::bail!(
                "BROADCAST_ENABLED=true requires both LIVE_EXECUTION=true and I_UNDERSTAND_LIVE_RISK=yes"
            );
        }
        if self.live_execution {
            if self.endpoints.searcher_private_key.is_none() {
                anyhow::bail!(
                    "live execution was armed without SEARCHER_PRIVATE_KEY; \
                     FLASHBOTS_SIGNER_KEY is an unfunded relay-authentication key and can never sign trades"
                );
            }
            if self.endpoints.executor.is_none() {
                anyhow::bail!("live execution was armed without EXECUTOR_ADDRESS");
            }
        }
        // A non-zero, enabled directional lane is its own live money path.
        // Requiring a dedicated key prevents a configuration that appears
        // armed in the console while sharing the atomic key or having no
        // signer at all. Shadow mode remains usable with zero size/budget.
        let sniper_live_intent = self.live_execution
            && env_bool("SNIPER_DIRECTIONAL", false)
            && !env_u256("SNIPER_BUY_SIZE_WEI", 0).is_zero()
            && !env_u256("SNIPER_DAILY_BUDGET_WEI", 0).is_zero();
        if sniper_live_intent && self.endpoints.sniper_searcher_private_key.is_none() {
            anyhow::bail!(
                "SNIPER_DIRECTIONAL is enabled with a non-zero buy size and budget but SNIPER_SEARCHER_PRIVATE_KEY is unset; configure a dedicated sniper key"
            );
        }
        if sniper_live_intent
            && self.endpoints.sniper_searcher_address == self.endpoints.searcher_address
        {
            anyhow::bail!(
                "SNIPER_SEARCHER_PRIVATE_KEY derives the same address as SEARCHER_PRIVATE_KEY; use separate keys for the atomic and directional nonce/risk domains"
            );
        }
        Ok(())
    }
}

/// The bind/auth rule, split out so it can be tested without constructing a
/// whole `Config`.
pub fn validate_api(api: &ApiConfig) -> Result<()> {
    if api.auth_token.is_none() && !bind_is_loopback(&api.bind) {
        anyhow::bail!(
            "API_BIND is {} (not loopback) but API_AUTH_TOKEN is unset.\n\
             The API exposes unauthenticated mutating endpoints — POST /api/mode, \
             /api/risk and /api/risk/reset — so anyone who can reach that port can \
             trip the kill switch, disable strategies, or set bribeBps to 100%.\n\
             Fix by either:\n  \
             * binding to loopback and reaching it through the dashboard proxy or an \
             SSH tunnel:  API_BIND=127.0.0.1:8080\n  \
             * or setting a shared secret:  API_AUTH_TOKEN=$(openssl rand -hex 32)",
            api.bind
        );
    }
    Ok(())
}

/// The pure core of [`Config::coherence_warnings`]: chain profile + strategy
/// toggles + relay-shaped env flags → named warnings. Free function so the
/// rules are unit-testable without constructing a full `Config`.
pub fn coherence_warnings_for(
    a: &known::ChainAddresses,
    s: &StrategyToggles,
    relay_tx_ingest: bool,
    sequencer_feed_set: bool,
    no_oracle_feeds: bool,
) -> Vec<String> {
    let mut out = Vec::new();
    if a.sequencer_only {
        for (name, on) in [
            ("STRATEGY_SANDWICH", s.sandwich),
            ("STRATEGY_SANDWICH_V3", s.sandwich_v3),
            ("STRATEGY_JIT", s.jit),
        ] {
            if on {
                out.push(format!(
                    "{name} is on but chain {} has no public mempool \
                     (sequencer feed only): front-run strategies can only \
                     act on sequencer-feed transactions, which are \
                     back-run-only (no signed victim bytes). They will \
                     self-reject every candidate.",
                    a.chain_id
                ));
            }
        }
        if relay_tx_ingest {
            out.push(
                "RELAY_TX_INGEST is on but the bloXroute Max Profit relay \
                 covers Ethereum mainnet only — it is inert on this chain \
                 (delivered-block scoring here comes from the chain's own \
                 blocks)."
                    .to_string(),
            );
        }
    }
    if !a.sequencer_only && sequencer_feed_set {
        out.push(
            "SEQUENCER_FEED_URL is set but this chain has a public \
             mempool — the sequencer feed will run as an extra stream \
             (TxSource::Sequencer), not as the primary source."
                .to_string(),
        );
    }
    for (name, on, present) in [
        (
            "STRATEGY_LIQUIDATION",
            s.liquidation,
            a.aave_v3_pool.is_some()
                && a.aave_v3_oracle.is_some()
                && a.aave_v3_data_provider.is_some(),
        ),
        (
            "STRATEGY_LIQUIDATION_COMPOUND",
            s.liquidation_compound,
            a.compound_v3_usdc.is_some(),
        ),
        (
            "STRATEGY_LIQUIDATION_MORPHO",
            s.liquidation_morpho,
            a.morpho_blue.is_some(),
        ),
        ("STRATEGY_LIQUIDATION_MAKER", s.liquidation_maker, a.maker),
    ] {
        if on && !present {
            out.push(format!(
                "{name} is on but its protocol is not present on chain \
                 {} — the strategy will not be constructed.",
                a.chain_id
            ));
        }
    }
    if s.oracle_frontrun && !a.maker && no_oracle_feeds {
        out.push(
            "STRATEGY_ORACLE_FRONTRUN is on but neither the Maker table \
             (mainnet-only) nor ORACLE_WATCH_FEEDS entries are available \
             for this chain — it will have nothing to watch."
                .to_string(),
        );
    }
    if a.balancer_vault.is_none() && s.atomic_arb {
        out.push(format!(
            "STRATEGY_ATOMIC_ARB is on but chain {} has no Balancer vault \
             registered — flashExecute bundles cannot be funded and every \
             arb candidate will revert in the fork. Set \
             BALANCER_VAULT_ADDRESS if a flash-loan source exists.",
            a.chain_id
        ));
    }
    out
}

impl Config {
    /// Boot-time coherence warnings: the chain profile vs the enabled
    /// strategies vs the relay-shaped env. Fail-loud, not fail-wrong —
    /// construction continues (the operator's explicit toggles win) but
    /// every mismatch is named, at boot and in `doctor`, so an inert
    /// strategy is never a silent one.
    pub fn coherence_warnings(&self) -> Vec<String> {
        let mut warnings = coherence_warnings_for(
            &self.addresses,
            &self.strategies,
            self.relay_tx_ingest,
            self.endpoints.sequencer_feed.is_some(),
            self.oracle.watch_feeds.is_empty(),
        );
        if self.submission_mode == SubmissionMode::Raw
            && self.live_smoke_max > 0
            && self.live_smoke_max_gas_cost_wei.is_zero()
        {
            warnings.push(
                "LIVE_SMOKE_MAX is non-zero in raw mode but LIVE_SMOKE_MAX_GAS_COST_WEI=0; \
                 unqualified raw sends remain disabled until a wei-denominated exposure cap is set"
                    .into(),
            );
        }
        if self.addresses.sequencer_only && self.risk.bribe_bps != 0 {
            warnings.push(format!(
                "BRIBE_BPS={} on sequencer-only chain {}; set BRIBE_BPS=0 because raw landing power comes from priority fee, not a coinbase transfer",
                self.risk.bribe_bps, self.chain.chain_id
            ));
        }
        warnings
    }

    pub fn summary(&self) -> String {
        format!(
            "chain={} ({}) ws={} mev_share={} call_bundle={} strategies=[{}] discovery={}/v3:{} ur={} arb_legs={} bloxroute_txs={} live={} smoke={}",
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
            self.live_execution,
            self.live_smoke_max
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
        if self.liquidation_compound {
            v.push("liquidation_compound");
        }
        if self.liquidation_morpho {
            v.push("liquidation_morpho");
        }
        if self.liquidation_maker {
            v.push("liquidation_maker");
        }
        if self.oracle_frontrun {
            v.push("oracle_frontrun");
        }
        if self.sniper {
            v.push("sniper");
        }
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_binds_are_recognised() {
        assert!(bind_is_loopback("127.0.0.1:8080"));
        assert!(bind_is_loopback("[::1]:8080"));
        assert!(bind_is_loopback("localhost:8080"));
    }

    #[test]
    fn routable_binds_are_not_loopback() {
        // The dangerous default, and the shapes people actually deploy.
        assert!(!bind_is_loopback("0.0.0.0:8080"));
        assert!(!bind_is_loopback("[::]:8080"));
        assert!(!bind_is_loopback("192.168.1.10:8080"));
        assert!(!bind_is_loopback("10.0.0.5:8080"));
    }

    #[test]
    fn unparseable_binds_fail_closed() {
        // This gates a security check: if we cannot prove it is loopback, it
        // must be treated as exposed.
        assert!(!bind_is_loopback("not a bind string"));
        assert!(!bind_is_loopback(""));
        assert!(!bind_is_loopback("8080"));
    }

    /// Regression guard for the `doctor` breakage: `validate()` used to run
    /// inside `from_env`, so *every* subcommand inherited the bind rule —
    /// including `doctor`, the command you run precisely to diagnose a bad
    /// config, and `replay`, which binds nothing. Only `run` and `api` open a
    /// port, so only they call `validate`.
    ///
    /// This asserts the rule itself still bites, and that either lever clears
    /// it. That `from_env` no longer calls it is enforced at the two call
    /// sites in `main.rs`.
    #[test]
    fn the_bind_rule_is_enforced_by_validate_not_by_loading() {
        fn api(bind: &str, token: Option<&str>) -> ApiConfig {
            ApiConfig {
                bind: bind.into(),
                db_path: ":memory:".into(),
                feed_capacity: 10,
                write_queue_capacity: 1_024,
                auth_token: token.map(str::to_string),
                allowed_origins: vec![],
            }
        }
        // The dangerous shape is still rejected...
        assert!(validate_api(&api("0.0.0.0:8080", None)).is_err());
        // ...and either remedy clears it.
        assert!(validate_api(&api("0.0.0.0:8080", Some("t"))).is_ok());
        assert!(validate_api(&api("127.0.0.1:8080", None)).is_ok());
    }

    #[test]
    fn the_signer_key_is_redacted_from_debug() {
        // Endpoints is cloned into every task; one `tracing::debug!(?cfg)`
        // added later must not print a live Flashbots key.
        let e = Endpoints {
            http_url: "http://localhost:8545".into(),
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
            flashbots_signer_key: Some("0xdeadbeefsupersecretkeymaterial".into()),
            searcher_private_key: None,
            sniper_searcher_private_key: None,
            executor: None,
            searcher_address: Address::ZERO,
            sniper_searcher_address: Address::ZERO,
        };
        let rendered = format!("{e:?}");
        assert!(
            !rendered.contains("supersecret"),
            "signer key leaked into Debug: {rendered}"
        );
        assert!(rendered.contains("<redacted>"), "{rendered}");
    }

    #[test]
    fn the_signer_key_is_not_serialised() {
        let e = Endpoints {
            http_url: "http://localhost:8545".into(),
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
            flashbots_signer_key: Some("0xdeadbeefsupersecretkeymaterial".into()),
            searcher_private_key: None,
            sniper_searcher_private_key: None,
            executor: None,
            searcher_address: Address::ZERO,
            sniper_searcher_address: Address::ZERO,
        };
        let json = serde_json::to_string(&e).unwrap();
        assert!(!json.contains("supersecret"), "signer key leaked: {json}");
        assert!(!json.contains("flashbots_signer_key"), "{json}");
    }

    #[test]
    fn unused_checklist_aliases_are_detected_without_touching_the_process_env() {
        // A lookup, not `std::env`: this test must stay hermetic so it cannot
        // race other tests or depend on the operator's shell.
        let lookup = |name: &str| match name {
            "MIN_NET_PROFIT_ETH" => Some("0.005".into()),
            "BUILDER_SHARE_BPS" => Some("9000".into()),
            _ => None,
        };
        let found = collect_ignored_env_aliases(lookup);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].name, "MIN_NET_PROFIT_ETH");
        assert_eq!(found[0].canonical, "MIN_NET_PROFIT_WEI");
        assert_eq!(found[1].name, "BUILDER_SHARE_BPS");
        assert_eq!(found[1].canonical, "BRIBE_BPS");
        assert!(validate_ignored_aliases(&found).is_err());
        assert!(validate_ignored_aliases(&[]).is_ok());
    }

    #[test]
    fn unused_alias_error_names_the_canonical_knob() {
        let found =
            collect_ignored_env_aliases(|name| (name == "MAX_BASE_FEE_GWEI").then(|| "100".into()));
        let err = format_ignored_env_error(&found);
        assert!(err.contains("MAX_BASE_FEE_GWEI"), "{err}");
        assert!(err.contains("MAX_BASE_FEE_WEI"), "{err}");
        assert!(!err.contains("100"), "do not echo the unused value: {err}");
        assert!(err.contains("wei/bps"), "{err}");
    }

    #[test]
    fn every_documented_ghost_name_is_in_the_alias_table() {
        // PATH_TO_PRODUCTION §3.2 / DAY0_RUNBOOK: these four names do not
        // exist in the bot and must not silently no-op.
        let names: Vec<&str> = IGNORED_ENV_ALIASES.iter().map(|(n, _)| *n).collect();
        for required in [
            "MIN_NET_PROFIT_ETH",
            "MAX_BASE_FEE_GWEI",
            "MAX_DRAWDOWN_ETH",
            "BUILDER_SHARE_BPS",
        ] {
            assert!(
                names.contains(&required),
                "{required} dropped from IGNORED_ENV_ALIASES"
            );
        }
        assert_eq!(IGNORED_ENV_ALIASES.len(), 4);
    }

    #[test]
    fn smoke_allows_only_inside_a_positive_budget() {
        assert!(!smoke_allows(0, 0), "default off");
        assert!(smoke_allows(0, 2));
        assert!(smoke_allows(1, 2));
        assert!(!smoke_allows(2, 2));
        assert!(!smoke_allows(3, 2));
        assert_eq!(smoke_remaining(0, 2), 2);
        assert_eq!(smoke_remaining(2, 2), 0);
        assert_eq!(smoke_remaining(5, 2), 0);
        // The cap is a named const so a compile-time check is enough; a
        // runtime `assert!(const)` is optimized out and trips clippy.
        const _: () = assert!(LIVE_SMOKE_MAX_CAP >= 2 && LIVE_SMOKE_MAX_CAP <= 5);
    }

    #[test]
    fn bundle_gas_clamps_to_the_eip7825_per_tx_cap() {
        // Lower bound: one intrinsic-cost transfer.
        assert_eq!(clamp_bundle_gas(0), 21_000);
        assert_eq!(clamp_bundle_gas(21_000), 21_000);
        // The 3M default passes through untouched.
        assert_eq!(clamp_bundle_gas(3_000_000), 3_000_000);
        // The EIP-7825 cap itself is still a legal tx gas limit.
        assert_eq!(clamp_bundle_gas(16_777_216), 16_777_216);
        // Anything above it would be protocol-invalid since Fusaka
        // (2025-12-03) regardless of the 60M block gas limit.
        assert_eq!(clamp_bundle_gas(20_000_000), 16_777_216);
        assert_eq!(clamp_bundle_gas(30_000_000), 16_777_216);
        assert_eq!(clamp_bundle_gas(u64::MAX), 16_777_216);
    }

    // ── WS-A: chain profiles & address registry ─────────────────────────

    #[test]
    fn for_chain_selects_the_built_in_profiles() {
        assert_eq!(known::for_chain(1).unwrap().chain_id, 1);
        assert_eq!(known::for_chain(8453).unwrap().chain_id, 8453);
        assert!(
            known::for_chain(42161).is_none(),
            "no Arbitrum profile in v1"
        );
        assert!(known::for_chain(0).is_none());
    }

    #[test]
    fn base_profile_carries_the_verified_addresses() {
        // Pinned against live Base mainnet 2026-08-23 (code presence +
        // cross-checked immutable getters; see the verification notes on the constants).
        let b = known::base();
        assert_eq!(b.weth, known::BASE_WETH);
        assert_eq!(b.usdc, known::BASE_USDC);
        assert_eq!(b.dai, Some(known::BASE_DAI));
        assert_eq!(b.wbtc, Some(known::BASE_WBTC));
        assert_eq!(b.univ2_factory, Some(known::BASE_UNIV2_FACTORY));
        assert_eq!(b.univ3_factory, Some(known::BASE_UNIV3_FACTORY));
        assert_eq!(b.univ3_quoter_v2, Some(known::BASE_UNIV3_QUOTER_V2));
        assert_eq!(
            b.univ3_swap_router_02,
            Some(known::BASE_UNIV3_SWAP_ROUTER_02)
        );
        assert_eq!(b.universal_router, Some(known::BASE_UNIVERSAL_ROUTER));
        // Balancer vault is deployed at the same address as mainnet.
        assert_eq!(b.balancer_vault, Some(known::BASE_BALANCER_VAULT));
        assert_eq!(b.balancer_vault, Some(known::BALANCER_VAULT));
        // v1 scope: no lending protocols, no Maker, no Sushi, no USDT.
        assert!(b.aave_v3_pool.is_none());
        assert!(b.compound_v3_usdc.is_none());
        assert!(b.morpho_blue.is_none());
        assert!(!b.maker);
        assert!(b.sushi_factory.is_none());
        assert!(b.usdt.is_none());
        // Chain behaviour: sequencer feed only, slower refork, raw transport,
        // sequencer-shaped qualification.
        assert!(b.sequencer_only);
        assert_eq!(b.refork_default, 6);
        assert_eq!(b.submission_default, SubmissionMode::Raw);
        assert_eq!(b.qualification_default, QualificationBackend::Sequencer);
        assert_eq!(b.pair_factories().len(), 1, "Base has one V2 venue");
    }

    #[test]
    fn ethereum_profile_unchanged_by_the_registry() {
        // The pre-registry constants are the ethereum() profile values —
        // a mainnet boot must see byte-identical addresses.
        let e = known::ethereum();
        assert_eq!(e.weth, known::WETH);
        assert_eq!(e.usdc, known::USDC);
        assert_eq!(e.univ3_factory, Some(known::UNIV3_FACTORY));
        assert_eq!(e.univ3_quoter_v2, Some(known::UNIV3_QUOTER_V2));
        assert_eq!(e.univ3_swap_router_02, Some(known::UNIV3_SWAP_ROUTER_02));
        assert_eq!(e.universal_router, Some(known::UNIVERSAL_ROUTER));
        assert_eq!(e.balancer_vault, Some(known::BALANCER_VAULT));
        assert!(e.maker);
        assert!(!e.sequencer_only);
        assert_eq!(e.refork_default, 1);
        assert_eq!(e.submission_default, SubmissionMode::Bundle);
        assert_eq!(e.qualification_default, QualificationBackend::Relay);
        assert_eq!(e.pair_factories().len(), 2);
    }

    #[test]
    fn env_overrides_win_over_the_profile() {
        let base = *known::base();
        let override_weth = alloy_primitives::address!("000000000000000000000000000000000000C0DE");
        let a = known::apply_address_overrides(base, |name| match name {
            "WETH_ADDRESS" => Some("0x000000000000000000000000000000000000C0DE".into()),
            "UNIV3_FACTORY_ADDRESS" => Some("0x000000000000000000000000000000000000FACE".into()),
            _ => None,
        });
        assert_eq!(a.weth, override_weth);
        assert_eq!(
            a.univ3_factory,
            Some(alloy_primitives::address!(
                "000000000000000000000000000000000000FACE"
            ))
        );
        // Non-overridden fields keep the profile values.
        assert_eq!(a.usdc, known::BASE_USDC);
        assert_eq!(a.univ3_quoter_v2, Some(known::BASE_UNIV3_QUOTER_V2));
    }

    #[test]
    fn a_chain_without_a_profile_is_fully_env_driven() {
        // Unknown chain id → empty skeleton; env overrides fill it in. No
        // code change needed to add a chain.
        let skeleton = known::ChainAddresses {
            chain_id: 42161,
            weth: Address::ZERO,
            usdc: Address::ZERO,
            usdt: None,
            dai: None,
            wbtc: None,
            wsteth: None,
            univ2_factory: None,
            univ2_router: None,
            univ3_factory: None,
            univ3_quoter_v2: None,
            univ3_npm: None,
            univ3_swap_router: None,
            univ3_swap_router_02: None,
            universal_router: None,
            sushi_factory: None,
            balancer_vault: None,
            aerodrome_router: None,
            aave_v3_pool: None,
            aave_v3_oracle: None,
            aave_v3_data_provider: None,
            compound_v3_usdc: None,
            morpho_blue: None,
            maker: false,
            core_tokens: [Address::ZERO; 4],
            core_tokens_len: 0,
            sequencer_only: false,
            refork_default: 1,
            submission_default: SubmissionMode::Bundle,
            qualification_default: QualificationBackend::Relay,
        };
        assert!(skeleton.weth == Address::ZERO);
        let a = known::apply_address_overrides(skeleton, |name| match name {
            "WETH_ADDRESS" => Some("0x4200000000000000000000000000000000000006".into()),
            _ => None,
        });
        assert_eq!(a.weth, known::BASE_WETH, "env fills the empty skeleton");
    }

    #[test]
    fn submission_mode_and_backend_parse_case_insensitively() {
        assert_eq!(
            SubmissionMode::parse("bundle"),
            Some(SubmissionMode::Bundle)
        );
        assert_eq!(SubmissionMode::parse("RAW"), Some(SubmissionMode::Raw));
        assert_eq!(SubmissionMode::parse("bundles"), None);
        assert_eq!(SubmissionMode::parse(""), None);
        assert_eq!(
            QualificationBackend::parse("relay"),
            Some(QualificationBackend::Relay)
        );
        assert_eq!(
            QualificationBackend::parse("Sequencer"),
            Some(QualificationBackend::Sequencer)
        );
        assert_eq!(QualificationBackend::parse("mempool"), None);
    }

    #[test]
    fn sequencer_only_chain_names_the_inert_strategies() {
        let base = *known::base();
        let mut s = StrategyToggles {
            sandwich: true,
            sandwich_v3: true,
            jit: true,
            atomic_arb: true,
            liquidation: true,
            liquidation_compound: true,
            liquidation_morpho: true,
            liquidation_maker: true,
            oracle_frontrun: true,
            sniper: true,
        };
        let warnings = coherence_warnings_for(&base, &s, true, false, true);
        let text = warnings.join("\n");
        for needle in [
            "STRATEGY_SANDWICH is on but chain 8453 has no public mempool",
            "STRATEGY_SANDWICH_V3 is on but chain 8453 has no public mempool",
            "STRATEGY_JIT is on but chain 8453 has no public mempool",
            "RELAY_TX_INGEST is on but the bloXroute Max Profit relay",
            "STRATEGY_LIQUIDATION is on but its protocol is not present",
            "STRATEGY_LIQUIDATION_COMPOUND is on but its protocol is not present",
            "STRATEGY_LIQUIDATION_MORPHO is on but its protocol is not present",
            "STRATEGY_LIQUIDATION_MAKER is on but its protocol is not present",
            "STRATEGY_ORACLE_FRONTRUN is on but neither the Maker table",
        ] {
            assert!(
                text.contains(needle),
                "missing warning: {needle}\n---\n{text}"
            );
        }
        // atomic_arb is fine on Base (Balancer vault present).
        assert!(!text.contains("STRATEGY_ATOMIC_ARB is on"));

        // The documented .env.example.base shape: front-run + liquidations
        // off, arb on, relay ingest off → no warnings at all.
        s.sandwich = false;
        s.sandwich_v3 = false;
        s.jit = false;
        s.liquidation = false;
        s.liquidation_compound = false;
        s.liquidation_morpho = false;
        s.liquidation_maker = false;
        s.oracle_frontrun = false;
        assert!(
            coherence_warnings_for(&base, &s, false, true, true).is_empty(),
            "the Base v1 toggle set must boot clean"
        );
    }

    #[test]
    fn ethereum_boot_with_default_toggles_is_quiet() {
        let e = *known::ethereum();
        let s = StrategyToggles {
            sandwich: true,
            sandwich_v3: true,
            jit: true,
            atomic_arb: true,
            liquidation: true,
            liquidation_compound: true,
            liquidation_morpho: true,
            liquidation_maker: true,
            oracle_frontrun: true,
            sniper: true,
        };
        assert!(
            coherence_warnings_for(&e, &s, true, false, false).is_empty(),
            "a default mainnet boot must not warn"
        );
    }
}
