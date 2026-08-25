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

// Pending is the hot variant and deliberately held inline (it crosses the
// whole mempool funnel per transaction); the frame variant is boxed. The
// spread between them is intentional, so the size-difference lint is waived
// here rather than shoving the hot variant behind a pointer.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug)]
pub enum IngestEvent {
    Block(BlockHead),
    Pending(PendingTx),
    /// A preconfirmed-state frame arrived (Flashblocks). Emitted once per
    /// accepted frame — even one that adds no transactions, because the state
    /// identity itself advanced and TTL/expiry bookkeeping depends on it.
    /// Always precedes the transactions parsed from the same frame.
    /// Boxed: ten frames a second must not bloat every `IngestEvent` copy out
    /// of the mempool-hot `Pending` variant's class.
    PreconfirmedState(Box<crate::flashblocks::PreconfirmedFrame>),
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
    /// Feed counters when `FLASHBLOCKS_WS_URL` is configured; surfaced on
    /// `/api/status` so a dead/broken/gapping preconfirmation feed is
    /// distinguishable from calm order flow at a glance.
    pub flashblocks: Option<Arc<crate::flashblocks::FlashblockStats>>,
}

impl Ingest {
    /// Wire up every configured source. `stats_slot` lets the caller (the
    /// engine) hand in the `FlashblockStats` handle it already published for
    /// `/api/status`, so the counters the operator watches are the ones the
    /// parser actually increments. `chain_blocks` is the same hand-off for
    /// the chain-native full-block poller's fetch-coverage counters.
    pub fn start(
        cfg: Arc<Config>,
        stats_slot: Option<Arc<crate::flashblocks::FlashblockStats>>,
        chain_blocks: Option<Arc<ChainBlockStats>>,
    ) -> Self {
        let (tx, rx) = mpsc::channel(8192);
        let http = RpcClient::new(cfg.endpoints.http_url.clone()).expect("http rpc");
        let mut flashblocks: Option<Arc<crate::flashblocks::FlashblockStats>> = None;

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

        if let Some(url) = cfg.endpoints.mev_blocker_ws.clone() {
            spawn_mev_blocker(url, tx.clone());
        }

        if let Some(feed) = cfg.endpoints.sequencer_feed.clone() {
            spawn_sequencer_feed(feed, tx.clone());
        }

        // Base Flashblocks: 200 ms preconfirmations, versus 2 s for a full
        // block. On a private-mempool chain this is the earliest state a
        // searcher can see at all.
        if let Some(url) = cfg.endpoints.flashblocks_ws.clone() {
            let stats = spawn_flashblocks(url, tx.clone(), stats_slot.clone());
            flashblocks = Some(stats);
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

        // Sequencer chains have no relay data API: the chain's own built
        // blocks are the delivered blocks. Defaulted on for them (see
        // Config::chain_block_ingest).
        if cfg.chain_block_ingest {
            spawn_chain_blocks(
                http.clone(),
                cfg.chain.block_time_ms,
                tx.clone(),
                chain_blocks,
            );
        }

        Self { rx, flashblocks }
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

/// Hashes buffered awaiting hydration. At ~200 pending transactions per block
/// this is several blocks of slack; past that the feed is outrunning the RPC
/// and the oldest entries are stale anyway.
const PENDING_RING_CAPACITY: usize = 2_048;

/// Hashes hydrated per `eth_getTransactionByHash` batch.
const HYDRATION_BATCH: usize = 64;

/// Subscribe to pending hashes and hydrate them in batches.
///
/// Hydration is batched (up to 64 hashes per round trip, flushed every 25 ms) —
/// on a busy node this is the difference between keeping up and falling behind.
fn spawn_pending(ws_url: String, http: RpcClient, tx: mpsc::Sender<IngestEvent>) {
    let mut sub = WsSubscription::spawn(ws_url, json!(["newPendingTransactions"]), "pendingTxs");
    tokio::spawn(async move {
        // Fixed-capacity ring buffer instead of a `Vec` that is drained from
        // the front. `drain(..64)` shifts every remaining element left on each
        // flush — O(n) memmove per batch, on the ingest task, at mempool rate.
        // `VecDeque` pops from the front in O(1) and never reallocates here
        // because the capacity is fixed up front.
        //
        // It is also *bounded*: if hydration cannot keep up with the feed, the
        // old code grew this vector without limit until the process died. Now
        // the oldest hash is evicted, which is the correct one to lose — a
        // pending transaction we have not hydrated in a full buffer's worth of
        // time is almost certainly mined or replaced already.
        let mut pending: std::collections::VecDeque<(String, u64)> =
            std::collections::VecDeque::with_capacity(PENDING_RING_CAPACITY);
        let mut dropped: u64 = 0;
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
                                if pending.len() == PENDING_RING_CAPACITY {
                                    pending.pop_front();
                                    dropped += 1;
                                    if dropped % 1_000 == 1 {
                                        tracing::warn!(
                                            target: "ingest",
                                            dropped,
                                            "pending hydration buffer full — evicting oldest hashes"
                                        );
                                    }
                                }
                                // Preserve the websocket observation time.
                                // Starting latency after hydration hid the RPC
                                // round trip from the 150 ms budget.
                                pending.push_back((h.to_string(), now_ms()));
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
            let take = pending.len().min(HYDRATION_BATCH);
            let batch: Vec<(String, u64)> = pending.drain(..take).collect();
            let calls: Vec<(String, Value)> = batch
                .iter()
                .map(|(hash, _)| ("eth_getTransactionByHash".to_string(), json!([hash])))
                .collect();
            let results = match http.batch(&calls).await {
                Ok(r) => r,
                Err(e) => {
                    tracing::debug!(target: "ingest", error = %e, "tx hydration batch failed");
                    continue;
                }
            };
            for (result, (_, observed_at_ms)) in results.into_iter().zip(batch) {
                let Ok(r) = result else { continue };
                if let Some(mut ptx) = parse_tx_object(&r, TxSource::PublicMempool) {
                    ptx.seen_at_ms = observed_at_ms;
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
        preconfirmed: None,
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

/// MEV Blocker's searcher feed.
///
/// `wss://searchers.mevblocker.io` streams *unsigned* pending transactions
/// (`mevblocker_partialPendingTransactions`) — private orderflow that never
/// reaches the public mempool, which is where a growing share of retail swap
/// volume now goes. The payload is a normal transaction object minus `v`,
/// `r`, `s`, so `parse_tx_object` already handles it; what it cannot give us
/// is `raw`, and the engine's existing "victim raw bytes required" gate is
/// what keeps that honest: sandwiches self-reject, back-runs go through.
///
/// This is deliberately a separate source from `spawn_external_mempool`: the
/// subscription name differs, and the transactions need their own `TxSource`
/// so the funnel can show what private flow is actually worth.
fn spawn_mev_blocker(url: String, tx: mpsc::Sender<IngestEvent>) {
    let mut sub = WsSubscription::spawn(
        url,
        json!(["mevblocker_partialPendingTransactions"]),
        "mevBlocker",
    );
    tokio::spawn(async move {
        while let Some(v) = sub.rx.recv().await {
            // The feed sends the transaction object directly; tolerate a
            // `txContents` wrapper the way the external-stream path does.
            let obj = v.get("txContents").unwrap_or(&v);
            if let Some(ptx) = parse_tx_object(obj, TxSource::MevBlocker) {
                if tx.send(IngestEvent::Pending(ptx)).await.is_err() {
                    return;
                }
            }
        }
    });
}

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

/// Subscribe to Base Flashblocks — 200 ms sub-block preconfirmations.
///
/// The full block is 2 s, so `newHeads` sees one event where this sees ten.
/// On a chain whose mempool is private there is no pending queue to watch at
/// all, which makes this the *earliest* observable state in the system and the
/// only feed that can support a competitive back-run.
///
/// Two things make this source different from every other one here:
///
/// 1. **The payload is a diff, not a block.** Each Flashblock carries only the
///    transactions added since the previous one in the same block, plus an
///    `index` that restarts at zero on every new block. Provenance
///    (`block_number`, `index`) therefore has to be read per message; a
///    consumer that treats two Flashblocks with the same index as the same
///    state is reading across a block boundary.
/// 2. **Transactions arrive as raw signed bytes**, not as JSON objects. That
///    is strictly better for us: the bundle transport requires raw bytes to
///    carry someone else's transaction, and the object feeds cannot supply
///    them. [`crate::rlp::decode_raw_transaction`] recovers the fields and the
///    sender from the same bytes we would resubmit.
///
/// Everything emitted is tagged [`TxSource::Flashblock`], which is
/// `backrun_only()`: once a Flashblock is sealed its ordering is final, so
/// there is no front position to buy at any price.
/// Returns the shared feed counters (`/api/status` reads them).
fn spawn_flashblocks(
    url: String,
    tx: mpsc::Sender<IngestEvent>,
    stats_slot: Option<Arc<crate::flashblocks::FlashblockStats>>,
) -> Arc<crate::flashblocks::FlashblockStats> {
    let stats =
        stats_slot.unwrap_or_else(|| Arc::new(crate::flashblocks::FlashblockStats::default()));
    let reconnects = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let mut sub = WsSubscription::spawn_observed(
        url,
        json!(["newFlashblocks"]),
        "flashblocks",
        Some(reconnects.clone()),
    );
    let parser_stats = stats.clone();
    tokio::spawn(async move {
        let mut parser = crate::flashblocks::FlashblockParser::new();
        let mut noticed_reconnects = 0u64;
        while let Some(v) = sub.rx.recv().await {
            // Bridge the transport's reconnect counter into the feed's.
            let now = reconnects.load(std::sync::atomic::Ordering::Relaxed);
            if now > noticed_reconnects {
                parser_stats.reconnects.fetch_add(
                    now - noticed_reconnects,
                    std::sync::atomic::Ordering::Relaxed,
                );
                noticed_reconnects = now;
            }
            let parsed = parser.parse(&v, Some(&parser_stats));
            if let Some(state) = parsed.state {
                // The frame itself moves the state identity even when it adds
                // no transactions — TTL/expiry bookkeeping must see it.
                let tx_hashes: Vec<alloy_primitives::B256> =
                    parsed.txs.iter().map(|t| t.hash).collect();
                if tx
                    .send(IngestEvent::PreconfirmedState(Box::new(
                        crate::flashblocks::PreconfirmedFrame { state, tx_hashes },
                    )))
                    .await
                    .is_err()
                {
                    return;
                }
            }
            for ptx in parsed.txs {
                if tx.send(IngestEvent::Pending(ptx)).await.is_err() {
                    return;
                }
            }
        }
    });
    stats
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

// ---------------------------------------------------------------------------
// Chain-native delivered blocks (sequencer chains)
// ---------------------------------------------------------------------------

/// Poll the chain's own head and push every newly built block through the
/// delivered-block pipeline, the same way the bloXroute relay path does on
/// mainnet.
///
/// On a sequencer chain (Base v1) there is no builder/relay market and no
/// data API to poll — the sequencer's own built blocks *are* the delivered
/// blocks. Each block is fetched in full, persisted, scored for extractable
/// value on the replay fork, and reconciled against the opportunities that
/// targeted it (`reconcile_block` matches on `target_block`). This is also
/// what feeds the `Sequencer` qualification backend's included-block
/// evidence.
///
/// Fetch-coverage counters for the chain-native full-block poller (work
/// order 0.3). A public sequencer RPC rate-limits `eth_getBlockByNumber`
/// aggressively, and without these counters a feed quietly returning half
/// its blocks was indistinguishable from a calm chain in every other panel.
/// All plain atomics, shared by every clone of the handle.
#[derive(Default, Debug)]
pub struct ChainBlockStats {
    /// Full-block bodies successfully fetched and forwarded to the engine.
    pub blocks_fetched: std::sync::atomic::AtomicU64,
    /// Full-block fetches that failed (typically provider rate limits).
    pub fetches_failed: std::sync::atomic::AtomicU64,
    /// Transactions carried by the fetched blocks.
    pub txs_seen: std::sync::atomic::AtomicU64,
    /// Wall clock of the last successful body fetch (unix ms); 0 = never.
    pub last_fetch_ms: std::sync::atomic::AtomicU64,
}

impl ChainBlockStats {
    pub fn snapshot(&self) -> serde_json::Value {
        use std::sync::atomic::Ordering::Relaxed;
        let ok = self.blocks_fetched.load(Relaxed);
        let failed = self.fetches_failed.load(Relaxed);
        let total = ok + failed;
        json!({
            "blocksFetched": ok,
            "fetchesFailed": failed,
            // 0 before the first attempt, never a synthetic 100%.
            "fetchSuccessRateBps": ok.saturating_mul(10_000).checked_div(total).unwrap_or(0),
            "txsSeen": self.txs_seen.load(Relaxed),
            "lastFetchMs": self.last_fetch_ms.load(Relaxed),
        })
    }
}

/// Polling rather than a `newHeads` subscription: sequencer-chain WS
/// endpoints are not uniform (and often absent on free tiers), while one
/// `eth_getBlockByNumber` per block is a negligible RPC cost. Missed blocks
/// (a blip) are caught up by the range walk below.
fn spawn_chain_blocks(
    http: RpcClient,
    block_time_ms: u64,
    tx: mpsc::Sender<IngestEvent>,
    stats: Option<Arc<ChainBlockStats>>,
) {
    tokio::spawn(async move {
        let mut last = 0u64;
        let interval = (block_time_ms / 2).max(250);
        loop {
            let result = http.call_raw("eth_blockNumber", json!([])).await;
            if let Ok(v) = result {
                let head = parse_u64(&v);
                if last == 0 {
                    // Baseline: start after the current head so a cold boot
                    // does not replay history (the relay path only ever sees
                    // *new* deliveries too).
                    last = head;
                } else if head > last {
                    // Catch up over any missed blocks (normally exactly one).
                    for block in last + 1..=head {
                        let result = http
                            .call_raw(
                                "eth_getBlockByNumber",
                                json!([format!("0x{block:x}"), true]),
                            )
                            .await;
                        let Ok(v) = result else {
                            if let Some(s) = &stats {
                                s.fetches_failed
                                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            }
                            // Silent `continue` is how a rate-limited provider
                            // hides itself: log (throttled) and move on.
                            static ERR_LOG: std::sync::atomic::AtomicU64 =
                                std::sync::atomic::AtomicU64::new(0);
                            if ERR_LOG
                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                                .is_multiple_of(25)
                            {
                                tracing::warn!(
                                    target: "ingest",
                                    block,
                                    error = %result.unwrap_err(),
                                    "chain block fetch failed (public RPCs rate-limit                                      full-block reads — use a paid endpoint for                                      chain-native ingestion)"
                                );
                            }
                            continue;
                        };
                        let Some(block_hash) = parse_b256(&v["hash"]) else {
                            continue;
                        };
                        let block = RelayBlock {
                            relay: "chain".into(),
                            slot: block,
                            block_number: block,
                            block_hash,
                            builder: "sequencer".into(),
                            // No observable builder payment on a sequencer
                            // chain in v1: the delivered-block table records
                            // the build, not an auction result.
                            value_wei: U256::ZERO,
                            gas_used: parse_u64(&v["gasUsed"]),
                            num_tx: v
                                .get("transactions")
                                .and_then(Value::as_array)
                                .map(|t| t.len() as u64)
                                .unwrap_or(0),
                        };
                        let txs = txs_from_block(&v);
                        if let Some(s) = &stats {
                            use std::sync::atomic::Ordering::Relaxed;
                            s.blocks_fetched.fetch_add(1, Relaxed);
                            s.txs_seen.fetch_add(txs.len() as u64, Relaxed);
                            s.last_fetch_ms.store(now_ms(), Relaxed);
                        }
                        if tx
                            .send(IngestEvent::RelayBlock { block, txs })
                            .await
                            .is_err()
                        {
                            return;
                        }
                    }
                    last = head;
                }
            } else {
                // A throttled provider must not be hammered: back off and
                // log (throttled) instead of spinning at interval speed.
                static HEAD_ERR: std::sync::atomic::AtomicU64 =
                    std::sync::atomic::AtomicU64::new(0);
                if HEAD_ERR
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                    .is_multiple_of(25)
                {
                    tracing::warn!(
                        target: "ingest",
                        error = %result.unwrap_err(),
                        "chain block ingest: head poll failed (public RPCs rate-limit                          aggressively — a paid endpoint is expected for this path)"
                    );
                }
                tokio::time::sleep(std::time::Duration::from_millis(interval * 2)).await;
                continue;
            }
            tokio::time::sleep(std::time::Duration::from_millis(interval)).await;
        }
    });
}

/// Parse a full-block object's transactions the way the relay path does,
/// stamping each with the block it landed in (replay-lane semantics).
fn txs_from_block(block: &Value) -> Vec<PendingTx> {
    let mined = MinedAt {
        block_number: parse_u64(block.get("number").unwrap_or(&Value::Null)),
        base_fee_per_gas: parse_u256(block.get("baseFeePerGas").unwrap_or(&Value::Null)),
    };
    if mined.block_number == 0 {
        return Vec::new();
    }
    block
        .get("transactions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|t| parse_tx_object(t, TxSource::RelayDelivered))
        .map(|mut t| {
            t.mined_at = Some(mined);
            t
        })
        .collect()
}
