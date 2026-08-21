//! Core domain types shared by the ingest, strategy, simulation and API layers.

use alloy_primitives::{Address, Bytes, B256, U256};
use serde::{Deserialize, Serialize};

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
    pub seen_at_ms: u64,
}

impl PendingTx {
    pub fn selector(&self) -> Option<[u8; 4]> {
        if self.input.len() < 4 {
            return None;
        }
        Some([self.input[0], self.input[1], self.input[2], self.input[3]])
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
    Jit,
    AtomicArb,
    Liquidation,
    Sniper,
}

impl Strategy {
    pub fn as_str(&self) -> &'static str {
        match self {
            Strategy::Sandwich => "sandwich",
            Strategy::Jit => "jit",
            Strategy::AtomicArb => "atomic_arb",
            Strategy::Liquidation => "liquidation",
            Strategy::Sniper => "sniper",
        }
    }

    pub fn all() -> [Strategy; 5] {
        [
            Strategy::Sandwich,
            Strategy::Jit,
            Strategy::AtomicArb,
            Strategy::Liquidation,
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
    let frac = (wei % U256::from(1_000_000_000_000_000_000u128)) / U256::from(1_000_000_000_000u128);
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
        assert_eq!(format_eth(U256::from(1_500_000_000_000_000_000u128)), "1.500000");
        assert_eq!(format_eth(U256::ZERO), "0.000000");
    }
}
