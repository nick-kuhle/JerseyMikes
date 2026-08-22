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
    /// L2 sequencer feed / preconfirmation.
    Sequencer,
    /// Third-party mempool stream (bloXroute, Blocknative, ...).
    ExternalStream,
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
            TxSource::ExternalStream => "external_stream",
            TxSource::RelayDelivered => "bloxroute_relay",
            TxSource::Mined => "mined",
        }
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
    /// gross - gas - bribe. Signed: negative means the bundle would have lost money.
    pub net_profit_wei: i128,
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
}

impl SimBackend {
    pub fn as_str(&self) -> &'static str {
        match self {
            SimBackend::AnvilFork => "anvil_fork",
            SimBackend::RelayCallBundle => "relay_call_bundle",
            SimBackend::EthCall => "eth_call",
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
    Opportunity(Opportunity),
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
    fn genesis_block_does_not_underflow() {
        let head = head_at(10, 1);
        let tx = tx_with(Some(MinedAt {
            block_number: 0,
            base_fee_per_gas: U256::ZERO,
        }));
        assert_eq!(tx.state_block(&head), 0);
    }
}
