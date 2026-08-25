//! Core domain types shared by the ingest, strategy, simulation and API layers.

use alloy_primitives::{Address, Bytes, B256, U256};
use serde::{Deserialize, Serialize};

/// Where an already-mined transaction sat when the bot saw it.
///
/// Present only for transactions that were on chain before we scored them —
/// today that means the relay delivered-block backfill. It is what lets the
/// replay path price against the state the transaction actually executed
/// against (`block_number - 1`) instead of against whatever the head happens
/// to be now, which may be hundreds of blocks later.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct MinedAt {
    /// Block the transaction was included in.
    pub block_number: u64,
    /// That block's base fee. Costing a historical bundle at today's base fee
    /// silently rewrites the economics of the opportunity in both directions.
    pub base_fee_per_gas: U256,
}

/// Identity of the preconfirmed sequencer state a transaction arrived
/// with — the Flashblocks-era replacement for "the mempool".
///
/// Base seals a preconfirmed sub-block every ~200 ms; each frame carries an
/// incremental diff of ordered, signed transactions plus a block-hash-shaped
/// identity for the resulting preconfirmed state. That identity is what a
/// quote, a simulation, and eventually a send must agree on — a candidate
/// priced against one frame and simulated against another is an invented
/// arbitrage, so the identity travels with every transaction the frame
/// produced.
///
/// Wire-format facts this type encodes (Base Flashblocks, verified against a
/// live capture 2026-08-24; see `tests/fixtures/flashblocks/README.md`):
///
/// - `state_id` is the frame's `diff.block_hash`: unique per frame, changes
///   as transactions are appended, and is deliberately *not* the sealed
///   canonical block hash;
/// - `(block_number, flashblock_index)` restarts at `(N+1, 0)` per block;
/// - `prev_frame_id` chains frames as `"<block_number>-<index>"` and is the
///   sequence/reorg signal the ingest layer watches for gaps;
/// - the ordering of a preconfirmed frame is final — this state can only be
///   back-run, never front-run.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreconfirmedState {
    /// Provider/feed label, so counters and dedupe never merge two feeds.
    pub feed: String,
    /// Canonical block this state builds towards.
    pub block_number: u64,
    /// Frame index inside the block (0 is rollover/system-only).
    pub flashblock_index: u64,
    /// Immutable identity of this exact preconfirmed state (`diff.block_hash`).
    pub state_id: B256,
    /// Payload id grouping every frame of one block build.
    pub payload_id: String,
    /// Chain link to the previous frame, when the feed supplies one
    /// (`metadata.prev_flashblock_id`, `"<block>-<index>"`).
    pub prev_frame_id: Option<String>,
    /// Parent block hash when the feed supplies one (index-0 `base` object).
    pub parent_hash: Option<B256>,
    /// Local observation time (ms); lead time is measured against it.
    pub observed_at_ms: u64,
    /// Whether the transaction order in this state is preconfirmed by the
    /// sequencer. Always true for the Flashblocks feed; carried explicitly so
    /// a future "pending hint" feed cannot silently claim ordering.
    pub ordered: bool,
}

impl PreconfirmedState {
    /// A frame `(block_number, index)` that builds on `self` within the same
    /// block — the only descendant relation the recheck path accepts: the
    /// pinned state's transactions are a prefix of the descendant's.
    pub fn is_descendant_of(&self, earlier: &PreconfirmedState) -> bool {
        self.feed == earlier.feed
            && self.block_number == earlier.block_number
            && self.flashblock_index >= earlier.flashblock_index
            && self.payload_id == earlier.payload_id
    }
}

/// A transaction observed in the public mempool or in a private orderflow stream.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PendingTx {
    pub hash: B256,
    pub from: Option<Address>,
    pub to: Option<Address>,
    pub value: U256,
    pub gas: u64,
    pub max_fee_per_gas: U256,
    pub max_priority_fee_per_gas: U256,
    pub nonce: u64,
    #[serde(with = "crate::types::bytes_hex")]
    pub input: Vec<u8>,
    /// Raw signed transaction, when the source provides it (needed for bundle sim).
    #[serde(default, with = "crate::types::opt_bytes_hex")]
    pub raw: Option<Vec<u8>>,
    pub source: TxSource,
    /// Set when the transaction was already mined when observed. `None` is
    /// live flow, which is evaluated against the current head.
    #[serde(default)]
    pub mined_at: Option<MinedAt>,
    /// Preconfirmed-state identity this transaction was observed in. Present
    /// on sequencer preconfirmation feeds (Flashblocks); `None` for ordinary
    /// pending flow. This is *provenance*, not a `victim_hashes` dependency:
    /// nothing here needs to be re-broadcast by the searcher.
    #[serde(default)]
    pub preconfirmed: Option<PreconfirmedState>,
    pub seen_at_ms: u64,
}

impl PendingTx {
    pub fn selector(&self) -> Option<[u8; 4]> {
        if self.input.len() < 4 {
            return None;
        }
        Some([self.input[0], self.input[1], self.input[2], self.input[3]])
    }

    /// True when this transaction was already on chain when we saw it.
    pub fn is_replay(&self) -> bool {
        self.mined_at.is_some()
    }

    /// The block whose **state** strategies must price against.
    ///
    /// For live flow that is the head: the next block builds on it. For a
    /// mined transaction it is the *parent* of its block — the state the
    /// transaction itself executed against. Using the head instead is the
    /// post-mortem state-divergence bug: reserves, oracle prices and account
    /// nonces have all moved on, so the sizing is computed against a world
    /// the victim never saw.
    pub fn state_block(&self, head: &BlockHead) -> u64 {
        match &self.mined_at {
            Some(m) => m.block_number.saturating_sub(1),
            None => head.number,
        }
    }

    /// The block a bundle built from this transaction targets.
    ///
    /// Live flow aims at `head + offset`; a replay aims at the block the
    /// transaction actually landed in, so the simulator forks at its parent
    /// and the victim's nonce is the one that was valid at the time.
    pub fn target_block(&self, head: &BlockHead, offset: u64) -> u64 {
        match &self.mined_at {
            Some(m) => m.block_number,
            None => head.number + offset,
        }
    }

    /// Base fee that applies when costing a bundle built from this transaction.
    pub fn base_fee(&self, head: &BlockHead) -> U256 {
        match &self.mined_at {
            Some(m) => m.base_fee_per_gas,
            None => head.base_fee_per_gas,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TxSource {
    /// `newPendingTransactions` from our own node.
    PublicMempool,
    /// Flashbots MEV-Share SSE (hints only — calldata is usually redacted).
    MevShare,
    /// L2 sequencer feed / preconfirmation. Ordered by the sequencer; the
    /// searcher cannot place a transaction in front of it.
    Sequencer,
    /// Base Flashblocks / sub-block preconfirmation. Same back-run-only
    /// constraint as [`TxSource::Sequencer`]; kept distinct so ingest can
    /// tag the feed without collapsing it onto ordinary pending semantics.
    Flashblock,
    /// Third-party mempool stream (bloXroute, Blocknative, ...).
    ExternalStream,
    /// MEV Blocker's searcher feed: *unsigned* pending transactions from
    /// private orderflow (`mevblocker_partialPendingTransactions`). Real
    /// pre-inclusion flow the public mempool never shows, but the payload has
    /// no signature, so it can only ever be back-run — see [`TxSource::backrun_only`].
    MevBlocker,
    /// Delivered inside a winning MEV-Boost block, discovered via a relay's
    /// `proposer_payload_delivered` bid traces (the bloXroute Max Profit relay).
    /// Already mined; used for post-mortem / back-run replay analysis.
    RelayDelivered,
    /// Already mined; used for backfill and post-mortem analysis.
    Mined,
}

impl TxSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            TxSource::PublicMempool => "public_mempool",
            TxSource::MevShare => "mev_share",
            TxSource::Sequencer => "sequencer",
            TxSource::Flashblock => "flashblock",
            TxSource::ExternalStream => "external_stream",
            TxSource::MevBlocker => "mev_blocker",
            TxSource::RelayDelivered => "bloxroute_relay",
            TxSource::Mined => "mined",
        }
    }

    /// Whether this source hands us transactions we cannot re-sign or reorder.
    ///
    /// MEV Blocker and MEV-Share publish the *contents* of a pending
    /// transaction without its signature. We can construct a bundle that runs
    /// **after** it (referenced by hash, which is exactly what MEV Blocker's
    /// `eth_sendBundle` expects), but we can never place a transaction in
    /// front of it — a sandwich needs the victim's signed bytes to replay the
    /// victim leg, and those do not exist here.
    pub fn backrun_only(&self) -> bool {
        matches!(
            self,
            TxSource::MevBlocker | TxSource::MevShare | TxSource::Sequencer | TxSource::Flashblock
        )
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BlockHead {
    pub number: u64,
    pub hash: B256,
    pub parent_hash: B256,
    pub timestamp: u64,
    pub base_fee_per_gas: U256,
    pub gas_used: u64,
    pub gas_limit: u64,
}

/// A block delivered through a MEV-Boost relay, as reported by the relay's
/// `proposer_payload_delivered` data API. The bloXroute **Max Profit** relay
/// (`https://bloxroute.max-profit.blxrbdn.com`) is the canonical source.
///
/// `value_wei` is what the winning builder paid the proposer — the market price
/// of that block's MEV, and the benchmark our simulated bundles are scored
/// against. The block's transactions themselves are fetched separately from the
/// execution node (`eth_getBlockByHash`) and stored next to this record.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RelayBlock {
    pub relay: String,
    pub slot: u64,
    pub block_number: u64,
    pub block_hash: B256,
    pub builder: String,
    pub value_wei: U256,
    pub gas_used: u64,
    pub num_tx: u64,
}

/// Trimmed summary of one transaction inside a delivered [`RelayBlock`], for the
/// live feed. The full record (with calldata) lives in SQLite behind the API.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RelayTxSummary {
    pub hash: B256,
    pub from: Option<Address>,
    pub to: Option<Address>,
    pub value: U256,
    pub selector: Option<String>,
}

/// One EVM call in a bundle, matching `MevExecutor.Call`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Call {
    pub target: Address,
    pub value: U256,
    #[serde(with = "crate::types::bytes_hex")]
    pub data: Vec<u8>,
}

impl Call {
    pub fn new(target: Address, data: Vec<u8>) -> Self {
        Self {
            target,
            value: U256::ZERO,
            data,
        }
    }

    pub fn with_value(target: Address, value: U256, data: Vec<u8>) -> Self {
        Self {
            target,
            value,
            data,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Strategy {
    Sandwich,
    /// V3 sandwich via QuoterV2 sizing. Separate variant so the funnel can
    /// tell V2 and V3 sandwich outcomes apart (Phase 2 W5).
    SandwichV3,
    Jit,
    AtomicArb,
    Liquidation,
    /// Compound V3 (Comet) absorb + storefront collateral purchase. Separate
    /// row: different victim population (Comet accounts), different revert
    /// modes (`NotForSale`, `TooMuchSlippage`), different reward shape
    /// (discount, not bonus bps).
    LiquidationCompound,
    /// Morpho Blue full-close liquidations. Separate row: share-math debt,
    /// lltv-proportional incentive, singleton market discovery.
    LiquidationMorpho,
    /// Maker bark + atomic clip take. Separate row: vat/clip plumbing, kick
    /// reward + auction spread instead of a liquidation bonus.
    LiquidationMaker,
    /// Back-run of a collateral price-feed update with liquidations of
    /// near-miss positions (the oracle-update front-run).
    OracleFrontrun,
    Sniper,
}

impl Strategy {
    pub fn as_str(&self) -> &'static str {
        match self {
            Strategy::Sandwich => "sandwich",
            Strategy::SandwichV3 => "sandwich_v3",
            Strategy::Jit => "jit",
            Strategy::AtomicArb => "atomic_arb",
            Strategy::Liquidation => "liquidation",
            Strategy::LiquidationCompound => "liquidation_compound",
            Strategy::LiquidationMorpho => "liquidation_morpho",
            Strategy::LiquidationMaker => "liquidation_maker",
            Strategy::OracleFrontrun => "oracle_frontrun",
            Strategy::Sniper => "sniper",
        }
    }

    /// Strategies whose bundle shape settles into a profit token the accounting
    /// layer can certify against ETH-denominated gas, and which carry the
    /// contract-level retained-profit invariant.
    ///
    /// This is only the engineering eligibility ceiling. Three further gates
    /// narrow it at runtime: the profit token must actually price at the
    /// simulated block (a liquidation settling in an unroutable collateral
    /// asset fails closed in [`crate::valuation`] and never reports success),
    /// the qualification gate must return `PASS` for the strategy, and
    /// broadcasting remains independently disabled by default.
    ///
    /// The four liquidation strategies are included because their profit is
    /// now valued at the pinned fork block rather than discarded. They are also
    /// the strategy class the market structure actually supports on a
    /// sequencer chain: liquidations are back-runs, which need no positional
    /// guarantee inside the block, whereas the sandwich family needs an atomic
    /// (front, victim, back) ordering that a private-mempool L2 with no builder
    /// market cannot sell.
    pub fn live_candidate(&self) -> bool {
        matches!(
            self,
            Strategy::Sandwich
                | Strategy::SandwichV3
                | Strategy::AtomicArb
                | Strategy::Liquidation
                | Strategy::LiquidationCompound
                | Strategy::LiquidationMorpho
                | Strategy::LiquidationMaker
        )
    }

    pub fn shadow_only_reason(&self) -> Option<&'static str> {
        match self {
            Strategy::Jit => Some("position is not yet unwound to one profit token"),
            Strategy::Sniper => {
                Some("round-trip probe is not a certified profitable execution strategy")
            }
            // Accounting is no longer the blocker here; position is. Landing an
            // oracle front-run requires being ordered ahead of a known update
            // in the same block, which needs either a builder market (mainnet)
            // or an express lane (Arbitrum TimeBoost). Neither is wired, and on
            // a private-mempool sequencer chain the ordering simply is not for
            // sale, so the strategy stays observational.
            Strategy::OracleFrontrun => {
                Some("requires guaranteed pre-update ordering: no builder market or express-lane bid is wired")
            }
            _ => None,
        }
    }

    pub fn all() -> [Strategy; 10] {
        [
            Strategy::Sandwich,
            Strategy::SandwichV3,
            Strategy::Jit,
            Strategy::AtomicArb,
            Strategy::Liquidation,
            Strategy::LiquidationCompound,
            Strategy::LiquidationMorpho,
            Strategy::LiquidationMaker,
            Strategy::OracleFrontrun,
            Strategy::Sniper,
        ]
    }
}

/// A candidate produced by a strategy, before simulation.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Opportunity {
    pub id: String,
    pub strategy: Strategy,
    /// Transaction(s) we are reacting to, in the order they must appear.
    pub victim_hashes: Vec<B256>,
    /// Calls executed *before* the victim transaction.
    pub front_calls: Vec<Call>,
    /// Calls executed *after* the victim transaction.
    pub back_calls: Vec<Call>,
    /// Tokens flash-borrowed for the batch (Balancer V2, zero fee).
    pub flash_tokens: Vec<Address>,
    pub flash_amounts: Vec<U256>,
    /// Token the profit is measured in (`Address::ZERO` == native ETH).
    pub profit_token: Address,
    /// Off-chain estimate before simulation.
    pub expected_profit_wei: U256,
    pub notional_wei: U256,
    pub target_block: u64,
    pub created_at_ms: u64,
    /// Human readable trail of how the opportunity was found.
    pub notes: String,
    /// State / route provenance for preconfirmation-pinned candidates and
    /// independent qualification samples. `Default` = canonical mempool
    /// provenance (no pin, foreign payload required when there are victims).
    pub provenance: Provenance,
}

/// One ordered hop of a priced route, in a form the state-comparison
/// producer can re-quote against canonical state without trusting a label
/// string (work order 3.1). `fee_bps` is captured because the re-quote must
/// bill the exact fee the prediction billed, not whatever is current later.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteHop {
    pub venue: crate::dex::Venue,
    pub pool: Address,
    pub token_in: Address,
    pub fee_bps: u32,
}

/// Where an [`Opportunity`] was derived from and what its execution needs
/// (work order 2.4 and 3.1).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Provenance {
    /// Preconfirmed-state pin for flashblock-derived candidates: identity of
    /// the exact state the economics were measured against. `None` for
    /// canonical-state candidates (block tick, public mempool).
    pub source_state: Option<PreconfirmedState>,
    /// Wall-clock TTL of a pinned candidate in milliseconds. The pin is dead
    /// at `created_at_ms + ttl_ms` even if the tracker still accepts the
    /// state (a stale frame cannot finance a send).
    pub ttl_ms: Option<u64>,
    /// Whether the payload must contain a foreign transaction (victim
    /// bytes). Mempool sandwiches/back-runs need the victim in the bundle;
    /// a preconfirmed-state back-run does not — the victim is already in
    /// the state — and raw transport can only deliver our own transactions.
    pub requires_foreign_payload: bool,
    /// Stable ordered route label for qualification identity
    /// (`venue:pool -> venue:pool`), matching
    /// [`crate::dex::edge::PricedCycle::route_label`].
    pub route: String,
    /// Direction of the route relative to the anchor (`"forward"` today;
    /// carried so the qualification row is never ambiguous).
    pub direction: String,
    /// Ordered route hops as priced — the exact identity the state
    /// comparison producer re-quotes at the canonical block. Empty for
    /// candidates that never passed through the priced router (or old rows
    /// predating the field).
    #[serde(default)]
    pub route_hops: Vec<RouteHop>,
    /// Gross profit of the sized route measured at the source state, before
    /// gas — the predicted side of an independent state comparison (3.1).
    /// Zero for candidates that never priced a route (victim strategies).
    pub predicted_gross_wei: U256,
}

impl Default for Provenance {
    /// Canonical mempool provenance: no pin, and victim-hashed candidates
    /// keep their historical requirement that the victim's bytes ride in the
    /// payload. Flashblock-derived back-runs override both explicitly.
    fn default() -> Self {
        Self {
            source_state: None,
            ttl_ms: None,
            requires_foreign_payload: true,
            route: String::new(),
            direction: String::new(),
            route_hops: Vec::new(),
            predicted_gross_wei: U256::ZERO,
        }
    }
}

/// Result of simulating an [`Opportunity`].
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SimulationResult {
    pub opportunity_id: String,
    pub strategy: Strategy,
    pub backend: SimBackend,
    pub success: bool,
    /// Gross profit in the profit token, as measured by the balance delta.
    pub gross_profit_wei: U256,
    pub gas_used: u64,
    pub gas_price_wei: U256,
    pub gas_cost_wei: U256,
    pub bribe_wei: U256,
    /// Retained profit minus searcher gas, in signed wei. Serialized as a
    /// decimal string so JavaScript and SQLite never round it.
    #[serde(with = "crate::types::i128_decimal")]
    pub net_profit_wei: i128,
    /// For victim-pinned bundles: the victim sender's `profit_token` balance
    /// change across the forked block, in signed wei (decimal string).
    /// This is the fork's prediction of what the victim's own transaction
    /// does — the sequencer backend's qualification compares it against the
    /// victim's realised delta in the canonical block (the "included block"
    /// second opinion; there is no relay on a sequencer chain).
    #[serde(default)]
    pub victim_predicted_out_wei: Option<String>,
    pub revert_reason: Option<String>,
    pub target_block: u64,
    pub sim_latency_ms: u64,
    pub created_at_ms: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SimBackend {
    /// Local anvil fork of mainnet at the current head.
    AnvilFork,
    /// `eth_callBundle` on the Flashbots relay.
    RelayCallBundle,
    /// `eth_call` with state overrides against the primary RPC.
    EthCall,
    /// `eth_simulateV1` at the provider's `"pending"` (preconfirmed) state
    /// with the executor fixture injected via state overrides — the only
    /// honest proof for a sub-block opportunity (work order 2.4).
    EthSimulateV1,
}

impl SimBackend {
    pub fn as_str(&self) -> &'static str {
        match self {
            SimBackend::AnvilFork => "anvil_fork",
            SimBackend::RelayCallBundle => "relay_call_bundle",
            SimBackend::EthCall => "eth_call",
            SimBackend::EthSimulateV1 => "eth_simulate_v1",
        }
    }
}

/// What the bot *would have* submitted. In simulation mode this is recorded but
/// never broadcast.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BundleRecord {
    pub id: String,
    pub opportunity_id: String,
    pub strategy: Strategy,
    pub target_block: u64,
    pub txs: Vec<BundleTx>,
    pub submitted: bool,
    pub included: Option<bool>,
    pub created_at_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BundleTx {
    pub hash: Option<B256>,
    #[serde(with = "crate::types::bytes_hex")]
    pub raw: Vec<u8>,
    pub can_revert: bool,
    /// True for transactions we did not create (the victim / target tx).
    pub foreign: bool,
}

/// Anything the UI feed shows.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FeedEvent {
    Block(BlockHead),
    Pending {
        hash: B256,
        from: Option<Address>,
        to: Option<Address>,
        value: U256,
        gas: u64,
        source: TxSource,
        selector: Option<String>,
        seen_at_ms: u64,
    },
    MevShareHint {
        hash: B256,
        logs: usize,
        functions: Vec<String>,
        seen_at_ms: u64,
    },
    /// Boxed: `Opportunity` (with its provenance) dwarfs the other variants;
    /// serde renders the box transparently so the wire shape never changed.
    Opportunity(Box<Opportunity>),
    Alert {
        rule: String,
        severity: String,
        message: String,
        active: bool,
        seen_at_ms: u64,
    },
    Simulation(SimulationResult),
    Bundle(BundleRecord),
    Relay {
        relay: String,
        slot: u64,
        builder: String,
        value_wei: U256,
        seen_at_ms: u64,
    },
    /// A delivered block plus the (trimmed) transactions that landed in it.
    RelayBlock {
        block: RelayBlock,
        tx_count: usize,
        txs: Vec<RelayTxSummary>,
    },
    /// The canonical chain rewound; simulations in `[from_block, to_block]`
    /// have been marked re-orged and dropped from P/L.
    Reorg {
        from_block: u64,
        to_block: u64,
        depth: u64,
        old_hash: B256,
        new_hash: B256,
        seen_at_ms: u64,
    },
    Log {
        level: String,
        message: String,
        at_ms: u64,
    },
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Hex helpers: we store byte blobs as `0x…` strings everywhere so the JSON that
/// reaches the frontend is directly usable by viem.
pub mod i128_decimal {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(value: &i128, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&value.to_string())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<i128, D::Error> {
        let value = String::deserialize(deserializer)?;
        value.parse::<i128>().map_err(serde::de::Error::custom)
    }
}

pub mod bytes_hex {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &Vec<u8>, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&format!("0x{}", hex::encode(v)))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let s = String::deserialize(d)?;
        let s = s.strip_prefix("0x").unwrap_or(&s);
        hex::decode(s).map_err(serde::de::Error::custom)
    }
}

pub mod opt_bytes_hex {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &Option<Vec<u8>>, s: S) -> Result<S::Ok, S::Error> {
        match v {
            Some(b) => s.serialize_str(&format!("0x{}", hex::encode(b))),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Vec<u8>>, D::Error> {
        let s = Option::<String>::deserialize(d)?;
        match s {
            Some(s) => {
                let s = s.strip_prefix("0x").unwrap_or(&s);
                Ok(Some(hex::decode(s).map_err(serde::de::Error::custom)?))
            }
            None => Ok(None),
        }
    }
}

/// Parse a `0x`-prefixed quantity into a `U256`, tolerating missing prefixes.
pub fn parse_u256(v: &serde_json::Value) -> U256 {
    match v {
        serde_json::Value::String(s) => {
            let s = s.strip_prefix("0x").unwrap_or(s);
            U256::from_str_radix(s, 16).unwrap_or(U256::ZERO)
        }
        serde_json::Value::Number(n) => U256::from(n.as_u64().unwrap_or(0)),
        _ => U256::ZERO,
    }
}

pub fn parse_u64(v: &serde_json::Value) -> u64 {
    match v {
        serde_json::Value::String(s) => {
            let s = s.strip_prefix("0x").unwrap_or(s);
            u64::from_str_radix(s, 16).unwrap_or(0)
        }
        serde_json::Value::Number(n) => n.as_u64().unwrap_or(0),
        _ => 0,
    }
}

pub fn parse_address(v: &serde_json::Value) -> Option<Address> {
    v.as_str().and_then(|s| s.parse::<Address>().ok())
}

pub fn parse_b256(v: &serde_json::Value) -> Option<B256> {
    v.as_str().and_then(|s| s.parse::<B256>().ok())
}

pub fn parse_bytes(v: &serde_json::Value) -> Vec<u8> {
    v.as_str()
        .map(|s| hex::decode(s.strip_prefix("0x").unwrap_or(s)).unwrap_or_default())
        .unwrap_or_default()
}

pub fn to_bytes(v: &[u8]) -> Bytes {
    Bytes::copy_from_slice(v)
}

/// Format wei as a human readable ETH string with 6 decimals (UI/logging only).
pub fn format_eth(wei: U256) -> String {
    let whole = wei / U256::from(1_000_000_000_000_000_000u128);
    let frac =
        (wei % U256::from(1_000_000_000_000_000_000u128)) / U256::from(1_000_000_000_000u128);
    format!("{}.{:06}", whole, frac.to::<u64>())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_quantities() {
        assert_eq!(parse_u64(&serde_json::json!("0x10")), 16);
        assert_eq!(parse_u256(&serde_json::json!("0xff")), U256::from(255));
        assert_eq!(parse_u256(&serde_json::json!(null)), U256::ZERO);
    }

    #[test]
    fn formats_eth() {
        assert_eq!(
            format_eth(U256::from(1_500_000_000_000_000_000u128)),
            "1.500000"
        );
        assert_eq!(format_eth(U256::ZERO), "0.000000");
    }

    fn head_at(number: u64, base_fee: u64) -> BlockHead {
        BlockHead {
            number,
            hash: B256::ZERO,
            parent_hash: B256::ZERO,
            timestamp: 0,
            base_fee_per_gas: U256::from(base_fee),
            gas_used: 0,
            gas_limit: 30_000_000,
        }
    }

    fn tx_with(mined_at: Option<MinedAt>) -> PendingTx {
        PendingTx {
            hash: B256::ZERO,
            from: None,
            to: None,
            value: U256::ZERO,
            gas: 0,
            max_fee_per_gas: U256::ZERO,
            max_priority_fee_per_gas: U256::ZERO,
            nonce: 0,
            input: Vec::new(),
            raw: None,
            source: TxSource::PublicMempool,
            mined_at,
            preconfirmed: None,
            seen_at_ms: 0,
        }
    }

    #[test]
    fn live_flow_prices_against_the_head() {
        let head = head_at(1_000, 7);
        let tx = tx_with(None);
        assert!(!tx.is_replay());
        assert_eq!(tx.state_block(&head), 1_000);
        assert_eq!(tx.target_block(&head, 1), 1_001);
        assert_eq!(tx.base_fee(&head), U256::from(7u64));
    }

    #[test]
    fn a_mined_transaction_prices_against_its_parent_block() {
        // The whole point of block-context tagging: a transaction delivered in
        // block 900 executed against the state left by block 899, and its
        // bundle targets 900 — regardless of how far the head has moved on.
        let head = head_at(1_000, 7);
        let tx = tx_with(Some(MinedAt {
            block_number: 900,
            base_fee_per_gas: U256::from(42u64),
        }));
        assert!(tx.is_replay());
        assert_eq!(tx.state_block(&head), 899);
        assert_eq!(tx.target_block(&head, 1), 900);
        assert_eq!(
            tx.base_fee(&head),
            U256::from(42u64),
            "a historical bundle must be costed at its own block's base fee"
        );
    }

    #[test]
    fn the_offset_never_leaks_into_a_replay_target() {
        // target_block_offset shifts live bundles into the *next* block. A
        // replay is aimed at a block that already exists, so the offset must
        // not move it.
        let head = head_at(5_000, 1);
        let tx = tx_with(Some(MinedAt {
            block_number: 4_000,
            base_fee_per_gas: U256::from(3u64),
        }));
        for offset in [0u64, 1, 2, 5] {
            assert_eq!(tx.target_block(&head, offset), 4_000);
        }
    }

    #[test]
    fn strategy_all_includes_the_v3_sandwich_row() {
        // The funnel distinguishes V2 from V3 sandwiches by variant. Dropping
        // SandwichV3 from `all()` would hide it from /api/status.strategies
        // and from the dashboard even when the toggle is on.
        assert_eq!(Strategy::all().len(), 10);
        assert_eq!(Strategy::SandwichV3.as_str(), "sandwich_v3");
        assert!(Strategy::all().contains(&Strategy::SandwichV3));
    }

    #[test]
    fn sequencer_and_flashblock_are_backrun_only() {
        assert!(TxSource::Sequencer.backrun_only());
        assert!(TxSource::Flashblock.backrun_only());
        assert!(TxSource::MevShare.backrun_only());
        assert!(TxSource::MevBlocker.backrun_only());
        assert!(!TxSource::PublicMempool.backrun_only());
        assert!(!TxSource::ExternalStream.backrun_only());
        assert!(!TxSource::RelayDelivered.backrun_only());
        assert_eq!(TxSource::Flashblock.as_str(), "flashblock");
    }

    #[test]
    fn genesis_block_does_not_underflow() {
        let head = head_at(10, 1);
        let tx = tx_with(Some(MinedAt {
            block_number: 0,
            base_fee_per_gas: U256::ZERO,
        }));
        assert_eq!(tx.state_block(&head), 0);
    }
}
