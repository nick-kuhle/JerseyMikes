//! Data ingestion: everything the searcher listens to.
//!
//! Sources, in rough order of usefulness on Ethereum mainnet:
//!   * **public mempool** — `newPendingTransactions` over websocket, then a
//!     batched `eth_getTransactionByHash` to hydrate the payload,
//!   * **Flashbots MEV-Share** — SSE hints for private orderflow (usually only
//!     logs + function selector, sometimes full calldata),
//!   * **new heads** — block cadence, base fee, and re-org detection,
//!   * **relay data API** — `proposer_payload_delivered` bid traces, i.e. what
//!     the winning builder actually paid (our benchmark for how much MEV was on
//!     the table each block),
//!   * **sequencer / preconfirmation feed** — for L2s, added when the second
//!     chain lands,
//!   * **external mempool streams** — bloXroute/Blocknative style websockets.
//!
//! Every source is normalised into [`IngestEvent`] and pushed onto one channel,
//! so strategies never care where a transaction came from.

use std::sync::Arc;
use std::time::Duration;

use alloy_primitives::{Address, B256, U256};
use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::config::Config;
use crate::rpc::{sse_stream, RpcClient, WsSubscription};
use crate::types::{
    now_ms, parse_address, parse_b256, parse_bytes, parse_u256, parse_u64, BlockHead, MinedAt,
    PendingTx, RelayBlock, TxSource,
};

#[derive(Clone, Debug)]
pub enum IngestEvent {
    Block(BlockHead),
    Pending(PendingTx),
    /// MEV-Share hint: we know *something* happened but not always what.
    Hint {
        hash: B256,
        to: Option<Address>,
        function_selectors: Vec<String>,
        logs: usize,
        raw: Value,
    },
    RelayBid {
        relay: String,
        slot: u64,
        builder: String,
        value_wei: U256,
    },
    /// A block delivered through the bloXroute Max Profit relay, with the
    /// transactions that actually landed in it (fetched from the execution node).
    RelayBlock {
        block: RelayBlock,
        txs: Vec<PendingTx>,
    },
}

pub struct Ingest {
    pub rx: mpsc::Receiver<IngestEvent>,
}

impl Ingest {
    /// Wire up every configured source.
    pub fn start(cfg: Arc<Config>) -> Self {
        let (tx, rx) = mpsc::channel(8192);
        let http = RpcClient::new(cfg.endpoints.http_url.clone()).expect("http rpc");

        if let Some(ws) = cfg.endpoints.ws_url.clone() {
            spawn_new_heads(ws.clone(), tx.clone());
            spawn_pending(ws, http.clone(), tx.clone());
        } else {
            // No websocket: fall back to polling heads over HTTP.
            spawn_head_poller(http.clone(), cfg.chain.block_time_ms, tx.clone());
        }

        if !cfg.endpoints.mev_share_sse.is_empty() {
            spawn_mev_share(cfg.endpoints.mev_share_sse.clone(), tx.clone());
        }

        for url in cfg.endpoints.extra_mempool_ws.clone() {
            spawn_external_mempool(url, tx.clone());
        }

        if let Some(feed) = cfg.endpoints.sequencer_feed.clone() {
            spawn_sequencer_feed(feed, tx.clone());
        }

        for relay in cfg.endpoints.relay_data_urls.clone() {
            spawn_relay_data(relay, cfg.chain.block_time_ms, tx.clone());
        }

        if cfg.relay_tx_ingest && !cfg.endpoints.bloxroute_relay_url.is_empty() {
            spawn_relay_blocks(
                cfg.endpoints.bloxroute_relay_url.clone(),
                http.clone(),
                cfg.chain.block_time_ms,
                tx.clone(),
            );
        }

        Self { rx }
    }
}

// ---------------------------------------------------------------------------
// Blocks
// ---------------------------------------------------------------------------

fn parse_head(v: &Value) -> Option<BlockHead> {
    Some(BlockHead {
        number: parse_u64(v.get("number")?),
        hash: parse_b256(v.get("hash")?)?,
        parent_hash: parse_b256(v.get("parentHash")?).unwrap_or_default(),
        timestamp: parse_u64(v.get("timestamp").unwrap_or(&Value::Null)),
        base_fee_per_gas: parse_u256(v.get("baseFeePerGas").unwrap_or(&Value::Null)),
        gas_used: parse_u64(v.get("gasUsed").unwrap_or(&Value::Null)),
        gas_limit: parse_u64(v.get("gasLimit").unwrap_or(&Value::Null)),
    })
}

fn spawn_new_heads(ws_url: String, tx: mpsc::Sender<IngestEvent>) {
    let mut sub = WsSubscription::spawn(ws_url, json!(["newHeads"]), "newHeads");
    tokio::spawn(async move {
        while let Some(v) = sub.rx.recv().await {
            if let Some(head) = parse_head(&v) {
                if tx.send(IngestEvent::Block(head)).await.is_err() {
                    return;
                }
            }
        }
    });
}

fn spawn_head_poller(http: RpcClient, block_time_ms: u64, tx: mpsc::Sender<IngestEvent>) {
    tokio::spawn(async move {
        let mut last = 0u64;
        loop {
            if let Ok(v) = http
                .call_raw("eth_getBlockByNumber", json!(["latest", false]))
                .await
            {
                if let Some(head) = parse_head(&v) {
                    if head.number > last {
                        last = head.number;
                        if tx.send(IngestEvent::Block(head)).await.is_err() {
                            return;
                        }
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(block_time_ms / 4)).await;
        }
    });
}

// ---------------------------------------------------------------------------
// Public mempool
// ---------------------------------------------------------------------------

/// Subscribe to pending hashes and hydrate them in batches.
///
/// Hydration is batched (up to 64 hashes per round trip, flushed every 25 ms) —
/// on a busy node this is the difference between keeping up and falling behind.
fn spawn_pending(ws_url: String, http: RpcClient, tx: mpsc::Sender<IngestEvent>) {
    let mut sub = WsSubscription::spawn(ws_url, json!(["newPendingTransactions"]), "pendingTxs");
    tokio::spawn(async move {
        let mut pending: Vec<String> = Vec::with_capacity(128);
        let mut ticker = tokio::time::interval(Duration::from_millis(25));
        loop {
            tokio::select! {
                maybe = sub.rx.recv() => {
                    match maybe {
                        Some(v) => {
                            // Some nodes stream full objects instead of hashes.
                            if v.is_object() {
                                if let Some(ptx) = parse_tx_object(&v, TxSource::PublicMempool) {
                                    if tx.send(IngestEvent::Pending(ptx)).await.is_err() { return; }
                                }
                            } else if let Some(h) = v.as_str() {
                                pending.push(h.to_string());
                            }
                        }
                        None => return,
                    }
                }
                _ = ticker.tick() => {}
            }

            if pending.is_empty() {
                continue;
            }
            let batch: Vec<String> = pending.drain(..pending.len().min(64)).collect();
            let calls: Vec<(String, Value)> = batch
                .iter()
                .map(|h| ("eth_getTransactionByHash".to_string(), json!([h])))
                .collect();
            let results = match http.batch(&calls).await {
                Ok(r) => r,
                Err(e) => {
                    tracing::debug!(target: "ingest", error = %e, "tx hydration batch failed");
                    continue;
                }
            };
            for r in results.into_iter().flatten() {
                if let Some(ptx) = parse_tx_object(&r, TxSource::PublicMempool) {
                    if tx.send(IngestEvent::Pending(ptx)).await.is_err() {
                        return;
                    }
                }
            }
        }
    });
}

pub fn parse_tx_object(v: &Value, source: TxSource) -> Option<PendingTx> {
    if !v.is_object() {
        return None;
    }
    let hash = parse_b256(v.get("hash")?)?;
    Some(PendingTx {
        hash,
        from: v.get("from").and_then(parse_address),
        to: v.get("to").and_then(parse_address),
        value: parse_u256(v.get("value").unwrap_or(&Value::Null)),
        gas: parse_u64(v.get("gas").unwrap_or(&Value::Null)),
        max_fee_per_gas: parse_u256(
            v.get("maxFeePerGas")
                .or_else(|| v.get("gasPrice"))
                .unwrap_or(&Value::Null),
        ),
        max_priority_fee_per_gas: parse_u256(
            v.get("maxPriorityFeePerGas")
                .or_else(|| v.get("gasPrice"))
                .unwrap_or(&Value::Null),
        ),
        nonce: parse_u64(v.get("nonce").unwrap_or(&Value::Null)),
        input: parse_bytes(
            v.get("input")
                .or_else(|| v.get("data"))
                .unwrap_or(&Value::Null),
        ),
        raw: v
            .get("raw")
            .or_else(|| v.get("rawTransaction"))
            .map(parse_bytes)
            .filter(|b| !b.is_empty()),
        source,
        // Live flow by default; the relay backfill stamps this after parsing.
        mined_at: None,
        seen_at_ms: now_ms(),
    })
}

// ---------------------------------------------------------------------------
// MEV-Share
// ---------------------------------------------------------------------------

fn spawn_mev_share(url: String, tx: mpsc::Sender<IngestEvent>) {
    let mut rx = sse_stream(url, "mev-share");
    tokio::spawn(async move {
        while let Some(line) = rx.recv().await {
            let v: Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let hash = match v.get("hash").and_then(parse_b256) {
                Some(h) => h,
                None => continue,
            };
            let logs = v
                .get("logs")
                .and_then(|l| l.as_array())
                .map(|a| a.len())
                .unwrap_or(0);
            let mut selectors = Vec::new();
            let mut to = None;
            if let Some(txsv) = v.get("txs").and_then(|t| t.as_array()) {
                for t in txsv {
                    if let Some(sel) = t.get("functionSelector").and_then(|s| s.as_str()) {
                        selectors.push(sel.to_string());
                    }
                    if to.is_none() {
                        to = t.get("to").and_then(parse_address);
                    }
                }
            }
            let ev = IngestEvent::Hint {
                hash,
                to,
                function_selectors: selectors,
                logs,
                raw: v,
            };
            if tx.send(ev).await.is_err() {
                return;
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Third-party streams & L2 sequencer feeds
// ---------------------------------------------------------------------------

fn spawn_external_mempool(url: String, tx: mpsc::Sender<IngestEvent>) {
    // Third-party streams differ in their subscribe payload; the common
    // denominator (bloXroute "newTxs", Blocknative "pendingTransactions") is an
    // eth_subscribe-shaped request, which is what WsSubscription sends.
    let mut sub = WsSubscription::spawn(
        url,
        json!(["newPendingTransactions", {"includeTransactions": true}]),
        "external",
    );
    tokio::spawn(async move {
        while let Some(v) = sub.rx.recv().await {
            let obj = v.get("txContents").unwrap_or(&v);
            if let Some(ptx) = parse_tx_object(obj, TxSource::ExternalStream) {
                if tx.send(IngestEvent::Pending(ptx)).await.is_err() {
                    return;
                }
            }
        }
    });
}

fn spawn_sequencer_feed(url: String, tx: mpsc::Sender<IngestEvent>) {
    // Arbitrum/OP-style feeds push already-sequenced (pre-confirmed) txs. Shape
    // varies per chain; we accept anything that looks like a tx object.
    let mut sub = WsSubscription::spawn(url, json!(["newPendingTransactions"]), "sequencer");
    tokio::spawn(async move {
        while let Some(v) = sub.rx.recv().await {
            if let Some(ptx) = parse_tx_object(&v, TxSource::Sequencer) {
                if tx.send(IngestEvent::Pending(ptx)).await.is_err() {
                    return;
                }
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Relay data API
// ---------------------------------------------------------------------------

/// Poll `proposer_payload_delivered` on each relay. The `value` field is what
/// the winning builder paid the proposer — the market price of a block's MEV,
/// and the yardstick for how competitive our simulated bundles are.
fn spawn_relay_data(base: String, block_time_ms: u64, tx: mpsc::Sender<IngestEvent>) {
    tokio::spawn(async move {
        let client = match reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
        {
            Ok(c) => c,
            Err(_) => return,
        };
        let url = format!(
            "{}/relay/v1/data/bidtraces/proposer_payload_delivered?limit=20",
            base.trim_end_matches('/')
        );
        let mut last_slot = 0u64;
        loop {
            match client.get(&url).send().await {
                Ok(resp) => {
                    if let Ok(items) = resp.json::<Vec<Value>>().await {
                        // The API returns newest first.
                        for item in items.iter().rev() {
                            let slot = item
                                .get("slot")
                                .and_then(|s| s.as_str())
                                .and_then(|s| s.parse::<u64>().ok())
                                .or_else(|| item.get("slot").and_then(|s| s.as_u64()))
                                .unwrap_or(0);
                            if slot <= last_slot {
                                continue;
                            }
                            last_slot = slot;
                            let value_wei = item
                                .get("value")
                                .and_then(|v| v.as_str())
                                .and_then(|v| v.parse::<U256>().ok())
                                .unwrap_or(U256::ZERO);
                            let builder = item
                                .get("builder_pubkey")
                                .and_then(|v| v.as_str())
                                .unwrap_or("unknown")
                                .to_string();
                            let ev = IngestEvent::RelayBid {
                                relay: base.clone(),
                                slot,
                                builder,
                                value_wei,
                            };
                            if tx.send(ev).await.is_err() {
                                return;
                            }
                        }
                    }
                }
                Err(e) => {
                    tracing::debug!(target: "ingest", relay = %base, error = %e, "relay data poll failed")
                }
            }
            tokio::time::sleep(Duration::from_millis(block_time_ms.max(4_000))).await;
        }
    });
}

// ---------------------------------------------------------------------------
// bloXroute Max Profit relay — delivered blocks + transactions
// ---------------------------------------------------------------------------

/// Poll the bloXroute Max Profit relay's `proposer_payload_delivered` bid traces
/// and, for every newly delivered block, fetch its full transaction list from the
/// execution node.
///
/// One `RelayBlock` event leaves this task per block, carrying the block metadata
/// plus a `PendingTx` (source `RelayDelivered`) for every transaction that
/// landed. The engine persists all of it and scores each transaction for
/// extractable value exactly like a mempool transaction.
fn spawn_relay_blocks(
    base: String,
    http: RpcClient,
    block_time_ms: u64,
    tx: mpsc::Sender<IngestEvent>,
) {
    tokio::spawn(async move {
        let client = match reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
        {
            Ok(c) => c,
            Err(_) => return,
        };
        let url = format!(
            "{}/relay/v1/data/bidtraces/proposer_payload_delivered?limit=20",
            base.trim_end_matches('/')
        );
        let mut last_block = 0u64;
        loop {
            match client.get(&url).send().await {
                Ok(resp) => {
                    if let Ok(items) = resp.json::<Vec<Value>>().await {
                        // Newest first; process oldest → newest so the high-water
                        // mark only advances once everything before it is done.
                        for item in items.iter().rev() {
                            let block_number = decimal_u64(&item["block_number"]);
                            if block_number == 0 || block_number <= last_block {
                                continue;
                            }
                            last_block = block_number;
                            let Some(block_hash) = item.get("block_hash").and_then(parse_b256)
                            else {
                                continue;
                            };
                            let block = RelayBlock {
                                relay: base.clone(),
                                slot: decimal_u64(&item["slot"]),
                                block_number,
                                block_hash,
                                builder: item
                                    .get("builder_pubkey")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("unknown")
                                    .to_string(),
                                value_wei: item
                                    .get("value")
                                    .and_then(|v| v.as_str())
                                    .and_then(|v| v.parse::<U256>().ok())
                                    .unwrap_or(U256::ZERO),
                                gas_used: decimal_u64(&item["gas_used"]),
                                num_tx: decimal_u64(&item["num_tx"]),
                            };
                            let txs = fetch_block_txs(&http, block_hash).await;
                            if tx
                                .send(IngestEvent::RelayBlock { block, txs })
                                .await
                                .is_err()
                            {
                                return;
                            }
                        }
                    }
                }
                Err(e) => tracing::debug!(
                    target: "ingest",
                    relay = %base,
                    error = %e,
                    "relay block poll failed"
                ),
            }
            tokio::time::sleep(Duration::from_millis(block_time_ms.max(4_000))).await;
        }
    });
}

/// Fetch the full transaction list of a delivered block. Missing or partial
/// blocks (pruned node, reorg race) yield an empty vec: the block metadata is
/// still recorded, only its transaction backfill is skipped.
async fn fetch_block_txs(http: &RpcClient, block_hash: alloy_primitives::B256) -> Vec<PendingTx> {
    let Ok(v) = http
        .call_raw(
            "eth_getBlockByHash",
            json!([format!("{block_hash:?}"), true]),
        )
        .await
    else {
        return Vec::new();
    };
    let Some(txs) = v.get("transactions").and_then(|t| t.as_array()) else {
        return Vec::new();
    };

    // Stamp every transaction with the block it landed in and that block's base
    // fee. Downstream this is what routes the strategies to historical pool
    // state (`number - 1`) and pins the replay fork to the same parent, instead
    // of silently scoring a mined transaction against today's head.
    let mined = MinedAt {
        block_number: parse_u64(v.get("number").unwrap_or(&Value::Null)),
        base_fee_per_gas: parse_u256(v.get("baseFeePerGas").unwrap_or(&Value::Null)),
    };
    if mined.block_number == 0 {
        // Without a block number there is no parent to fork at, and scoring
        // against the head would be worse than not scoring at all.
        tracing::debug!(target: "ingest", ?block_hash, "delivered block has no number; skipping tx backfill");
        return Vec::new();
    }

    txs.iter()
        .filter_map(|t| parse_tx_object(t, TxSource::RelayDelivered))
        .map(|mut t| {
            t.mined_at = Some(mined);
            t
        })
        .collect()
}

/// Relay data APIs encode integers as *decimal* strings (`"123"`), unlike the
/// hex quantities used everywhere else on the JSON-RPC wire.
fn decimal_u64(v: &Value) -> u64 {
    v.as_str()
        .and_then(|s| s.parse::<u64>().ok())
        .or_else(|| v.as_u64())
        .unwrap_or(0)
}

/// Fetch the raw signed bytes of a pending transaction so it can be replayed
/// inside the fork. Falls back to `None` when the node does not expose
/// `eth_getRawTransactionByHash` (most public providers do).
pub async fn fetch_raw_tx(http: &RpcClient, hash: B256) -> Option<Vec<u8>> {
    let v = http
        .call_raw("eth_getRawTransactionByHash", json!([format!("{hash:?}")]))
        .await
        .ok()?;
    let bytes = parse_bytes(&v);
    if bytes.is_empty() {
        None
    } else {
        Some(bytes)
    }
}
