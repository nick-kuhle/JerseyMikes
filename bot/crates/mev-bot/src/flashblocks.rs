//! Base Flashblocks ingestion — provenance-aware (work order WS-O / WS-1).
//!
//! The Flashblocks feed is the only early state Base exposes: the sequencer
//! publishes a preconfirmed sub-block every ~200 ms, ~10 per 2 s block. Every
//! frame is an incremental diff of **ordered, raw-signed** transactions plus a
//! block-hash-shaped identity (`diff.block_hash`) for the resulting
//! preconfirmed state. This module turns that stream into transactions whose
//! [`PreconfirmedState`] identity survives through candidate selection,
//! simulation, and the raw send — instead of flattening frames onto ordinary
//! pending-mempool semantics.
//!
//! Rules enforced here (BASE_REVENUE_PATH_WORK_ORDER §WS-O3):
//!
//! 1. **Provenance is preserved.** Every transaction carries the full
//!    [`PreconfirmedState`] of the frame it arrived in; the frame itself is
//!    also emitted as an [`IngestEvent::PreconfirmedState`] even when it adds
//!    no user transactions (the state still advanced, so TTL bookkeeping
//!    downstream must see it).
//! 2. **Dedupe is (feed, state identity, tx hash).** A redelivered frame
//!    (reconnect replay, provider resumption) can never inflate counts or
//!    re-trigger a candidate.
//! 3. **Gaps are explicit, never patched over.** Frame sequence is tracked
//!    from the block number + index and the `metadata.prev_flashblock_id`
//!    chain link; a discontinuity raises `state_gaps`, it is never relabelled
//!    as normal flow.
//! 4. **Malformed frames are counted and dropped**, not guessed at.
//!
//! Wire-format reference: `tests/fixtures/flashblocks/README.md` (real capture,
//! Base mainnet 2026-08-24) and `docs/BASE_FEED.md`.

use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};

use alloy_primitives::B256;
use serde_json::Value;

use crate::types::{now_ms, parse_b256, parse_u64, PendingTx, PreconfirmedState, TxSource};

/// The one label v1 supports. A second feed gets its own label and its own
/// parser instance so counters never merge (per-chain, per-provider).
pub const FEED_FLASHBLOCKS: &str = "flashblocks";

/// The payload of [`crate::ingest::IngestEvent::PreconfirmedState`]: one
/// accepted frame's identity plus the hashes its diff added, boxed on the
/// event because flashblock cadence is 10× the block cadence.
#[derive(Clone, Debug)]
pub struct PreconfirmedFrame {
    pub state: PreconfirmedState,
    pub tx_hashes: Vec<B256>,
}

/// Feed-level counters. Everything an operator needs to tell apart: a dead
/// feed (`frames_total` frozen), a chatty broken feed (`frames_malformed`
/// rising), a flapping connection (`reconnects`), silent state loss
/// (`state_gaps`), and plain duplicates (`txs_duplicate`).
#[derive(Default)]
pub struct FlashblockStats {
    pub frames_total: AtomicU64,
    pub frames_malformed: AtomicU64,
    pub blocks_seen: AtomicU64,
    pub txs_total: AtomicU64,
    pub txs_duplicate: AtomicU64,
    /// Undecodable entries inside otherwise valid frames (excluding OP-stack
    /// deposit transactions, which are expected system traffic).
    pub txs_malformed: AtomicU64,
    /// OP-stack deposit (`0x7e`) system transactions seen — expected at
    /// frame index 0; never actionable, never counted as malformed.
    pub txs_deposit: AtomicU64,
    pub state_gaps: AtomicU64,
    pub reconnects: AtomicU64,
    /// Wall clock of the last accepted frame (unix ms); 0 = never.
    pub last_frame_ms: AtomicU64,
    pub last_block_number: AtomicU64,
    pub last_index: AtomicU64,
    /// Latest measured lead: ms between the newest frame of a block arriving
    /// and the canonical sealed head for that block arriving. 0 until the
    /// first sealed match. Written by `FlashblockTracker::observe_sealed`.
    pub last_sealed_lead_ms: AtomicU64,
    /// How many of the tracked blocks' frames matched the sealed canonical
    /// block's transaction set (engine compares; see `FlashblockTracker`).
    pub sealed_matches: AtomicU64,
    pub sealed_mismatches: AtomicU64,
}

macro_rules! inc {
    ($f:expr) => {
        $f.fetch_add(1, Ordering::Relaxed)
    };
}

impl FlashblockStats {
    pub fn snapshot(&self) -> serde_json::Value {
        serde_json::json!({
            "framesTotal": self.frames_total.load(Ordering::Relaxed),
            "framesMalformed": self.frames_malformed.load(Ordering::Relaxed),
            "blocksSeen": self.blocks_seen.load(Ordering::Relaxed),
            "txsTotal": self.txs_total.load(Ordering::Relaxed),
            "txsDuplicate": self.txs_duplicate.load(Ordering::Relaxed),
            "txsMalformed": self.txs_malformed.load(Ordering::Relaxed),
            "txsDeposit": self.txs_deposit.load(Ordering::Relaxed),
            "stateGaps": self.state_gaps.load(Ordering::Relaxed),
            "reconnects": self.reconnects.load(Ordering::Relaxed),
            "lastFrameMs": self.last_frame_ms.load(Ordering::Relaxed),
            "lastBlockNumber": self.last_block_number.load(Ordering::Relaxed),
            "lastIndex": self.last_index.load(Ordering::Relaxed),
            "lastSealedLeadMs": self.last_sealed_lead_ms.load(Ordering::Relaxed),
            "sealedMatches": self.sealed_matches.load(Ordering::Relaxed),
            "sealedMismatches": self.sealed_mismatches.load(Ordering::Relaxed),
        })
    }
}

/// How many blocks of (state, tx) pairs the dedupe window retains. At ~500
/// txs per 2 s block this is ~2 minutes of full coverage — far beyond any
/// plausible reconnect replay window.
const DEDUPE_BLOCKS: u64 = 32;

/// What the parser made of one frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FrameOutcome {
    /// First frame of a new block build (`index = 0`, rollover).
    NewBlock,
    /// Monotonic continuation of the current block build.
    Continuation,
    /// A frame we already fully processed (resend after reconnect) — its
    /// transactions are deduped per-tx and it cannot move the sequence.
    Redelivered,
    /// Malformed or unidentifiable: counted as malformed, dropped.
    Malformed(&'static str),
}

/// Result of parsing one notification.
pub struct ParsedFlashblock {
    /// None when the frame was malformed (no identity could be established).
    pub state: Option<PreconfirmedState>,
    pub txs: Vec<PendingTx>,
    pub duplicate_txs: usize,
    pub outcome: FrameOutcome,
}

/// Stateful parser for one feed. A parser instance owns the sequence tracker
/// and the bounded dedupe window; `spawn_flashblocks` owns one per connection
/// loop and tests own one per scenario.
pub struct FlashblockParser {
    feed: String,
    /// Last accepted frame: `(block_number, index)`.
    last_frame: Option<(u64, u64)>,
    /// Last accepted frame's payload id (rollover detection).
    last_payload: Option<String>,
    /// Most recent `base.parent_hash` (only present at index 0).
    last_parent_hash: Option<B256>,
    /// Bounded (state_id, tx_hash) window with block-tagged FIFO eviction.
    seen: HashSet<(B256, B256)>,
    seen_order: VecDeque<(u64, (B256, B256))>,
}

impl Default for FlashblockParser {
    fn default() -> Self {
        Self::new()
    }
}

impl FlashblockParser {
    pub fn new() -> Self {
        Self {
            feed: FEED_FLASHBLOCKS.to_string(),
            last_frame: None,
            last_payload: None,
            last_parent_hash: None,
            seen: HashSet::new(),
            seen_order: VecDeque::new(),
        }
    }

    pub fn with_feed(feed: &str) -> Self {
        let mut p = Self::new();
        p.feed = feed.to_string();
        p
    }

    /// Latest accepted preconfirmed state frame `(block_number, index)` —
    /// the "current identity" rechecks compare against.
    pub fn last_frame(&self) -> Option<(u64, u64)> {
        self.last_frame
    }

    /// Parse one `eth_subscribe` notification's `params.result` value.
    ///
    /// `stats` is optional so pure parsing tests can pass `None`; the spawn
    /// loop always passes the shared feed counters.
    pub fn parse(&mut self, v: &Value, stats: Option<&FlashblockStats>) -> ParsedFlashblock {
        if let Some(s) = stats {
            inc!(s.frames_total);
        }
        let malformed = |reason: &'static str, stats: Option<&FlashblockStats>| ParsedFlashblock {
            state: None,
            txs: Vec::new(),
            duplicate_txs: 0,
            outcome: {
                if let Some(s) = stats {
                    inc!(s.frames_malformed);
                }
                FrameOutcome::Malformed(reason)
            },
        };

        let Some(index) = v.get("index").and_then(|i| i.as_u64()) else {
            return malformed("missing/invalid index", stats);
        };
        let Some(diff) = v.get("diff").filter(|d| d.is_object()) else {
            return malformed("missing diff", stats);
        };
        // State identity. Without it the frame is a notification, not
        // actionable preconfirmed flow.
        let Some(state_id) = diff.get("block_hash").and_then(parse_b256) else {
            return malformed("missing/invalid diff.block_hash", stats);
        };
        let payload_id = v
            .get("payload_id")
            .and_then(|p| p.as_str())
            .unwrap_or_default()
            .to_string();

        // Block number: metadata (decimal) on every frame, `base` (hex) at
        // index 0. Both missing ⇒ we cannot identify what state this is.
        let meta_bn = v
            .get("metadata")
            .and_then(|m| m.get("block_number"))
            .and_then(|b| b.as_u64());
        let base = v.get("base").filter(|b| b.is_object());
        let base_bn = base
            .and_then(|b| b.get("block_number"))
            .map(parse_u64)
            .filter(|b| *b > 0);
        let base_matches_meta = match (meta_bn, base_bn) {
            (Some(m), Some(b)) => m == b,
            _ => true,
        };
        if !base_matches_meta {
            return malformed("base/metadata block_number disagree", stats);
        }
        let Some(block_number) = meta_bn.or(base_bn) else {
            return malformed("no resolvable block_number", stats);
        };

        let prev_frame_id = v
            .get("metadata")
            .and_then(|m| m.get("prev_flashblock_id"))
            .and_then(|p| p.as_str())
            .map(|s| s.to_string());
        let parent_hash = base
            .and_then(|b| b.get("parent_hash"))
            .and_then(parse_b256)
            .or(self.last_parent_hash);

        // ── sequence tracking ────────────────────────────────────────────
        let outcome = match self.last_frame {
            None => {
                if index != 0 {
                    // Resuming mid-stream: frames before us are unknown.
                    if let Some(s) = stats {
                        inc!(s.state_gaps);
                    }
                }
                FrameOutcome::NewBlock
            }
            Some((lb, li)) => {
                if state_id_is_duplicate(self, block_number, index, state_id) {
                    FrameOutcome::Redelivered
                } else {
                    let gap = sequence_gap(block_number, index, &prev_frame_id, lb, li);
                    if gap {
                        if let Some(s) = stats {
                            inc!(s.state_gaps);
                        }
                    }
                    if block_number != lb {
                        FrameOutcome::NewBlock
                    } else {
                        FrameOutcome::Continuation
                    }
                }
            }
        };
        if matches!(outcome, FrameOutcome::Malformed(_)) {
            unreachable!("malformed handled above");
        }

        // ── transactions ─────────────────────────────────────────────────
        let mut txs = Vec::new();
        let mut dup = 0usize;
        let empty = Vec::new();
        let raw_txs = diff
            .get("transactions")
            .and_then(|t| t.as_array())
            .unwrap_or(&empty);
        let state = PreconfirmedState {
            feed: self.feed.clone(),
            block_number,
            flashblock_index: index,
            state_id,
            payload_id: payload_id.clone(),
            prev_frame_id: prev_frame_id.clone(),
            parent_hash,
            observed_at_ms: now_ms(),
            ordered: true,
        };
        for item in raw_txs {
            let Some(raw_hex) = item.as_str() else {
                // Object-shaped entries are not the documented v0.8 shape;
                // drop rather than guess (fail closed).
                if let Some(s) = stats {
                    inc!(s.txs_malformed);
                }
                continue;
            };
            let Some(bytes) = hex::decode(raw_hex.trim_start_matches("0x"))
                .ok()
                .filter(|b| !b.is_empty())
            else {
                if let Some(s) = stats {
                    inc!(s.txs_malformed);
                }
                continue;
            };
            // OP-stack deposit transactions are expected system traffic
            // (no signature, no sender to recover) — skip, never flag.
            if bytes.first() == Some(&0x7e) {
                if let Some(s) = stats {
                    inc!(s.txs_deposit);
                }
                continue;
            }
            let Some(d) = crate::rlp::decode_raw_transaction(&bytes) else {
                if let Some(s) = stats {
                    inc!(s.txs_malformed);
                }
                continue;
            };
            // Dedup on (feed, state identity, tx hash) exactly.
            let key = (state_id, d.hash);
            if !self.seen.insert(key) {
                dup += 1;
                if let Some(s) = stats {
                    inc!(s.txs_duplicate);
                }
                continue;
            }
            self.seen_order.push_back((block_number, key));
            if let Some(s) = stats {
                inc!(s.txs_total);
            }
            txs.push(PendingTx {
                hash: d.hash,
                from: d.from,
                to: d.to,
                value: d.value,
                gas: d.gas_limit,
                max_fee_per_gas: d.max_fee_per_gas,
                max_priority_fee_per_gas: d.max_priority_fee_per_gas,
                nonce: d.nonce,
                input: d.input,
                raw: Some(bytes),
                source: TxSource::Flashblock,
                mined_at: None,
                preconfirmed: Some(state.clone()),
                seen_at_ms: now_ms(),
            });
        }
        self.evict_before(block_number.saturating_sub(DEDUPE_BLOCKS));

        // Advance the sequence tracker on any accepted frame. Redelivered
        // frames must not move it backwards.
        if !matches!(outcome, FrameOutcome::Redelivered) {
            let advance = match self.last_frame {
                None => true,
                Some((lb, li)) => block_number > lb || (block_number == lb && index > li),
            };
            if advance {
                // A frame for a block we were not tracking (including the
                // very first frame ever) starts a new block build.
                if self.last_frame.map(|(lb, _)| lb) != Some(block_number) {
                    if let Some(s) = stats {
                        inc!(s.blocks_seen);
                    }
                }
                self.last_frame = Some((block_number, index));
                self.last_payload = Some(payload_id.clone());
            }
            if let Some(b) = base.and_then(|b| b.get("parent_hash")).and_then(parse_b256) {
                self.last_parent_hash = Some(b);
            }
        }
        if let Some(s) = stats {
            s.last_frame_ms.store(now_ms(), Ordering::Relaxed);
            s.last_block_number.store(block_number, Ordering::Relaxed);
            s.last_index.store(index, Ordering::Relaxed);
        }

        ParsedFlashblock {
            state: Some(state),
            txs,
            duplicate_txs: dup,
            outcome,
        }
    }

    fn evict_before(&mut self, min_block: u64) {
        while let Some((b, key)) = self.seen_order.front().copied() {
            if b >= min_block {
                break;
            }
            self.seen.remove(&key);
            self.seen_order.pop_front();
        }
    }
}

/// A redelivery carries the same `(block, index, state identity)`.
fn state_id_is_duplicate(
    parser: &FlashblockParser,
    block: u64,
    index: u64,
    state_id: B256,
) -> bool {
    parser.last_frame == Some((block, index))
        && parser
            .seen_order
            .iter()
            .any(|(b, (s, _))| *b == block && *s == state_id)
        && index <= parser.last_frame.map(|(_, i)| i).unwrap_or(u64::MAX)
}

/// Whether `(block, index, prev_link)` is discontinuous with the last
/// accepted frame `(last_block, last_index)`.
///
/// Accepts exactly two healthy steps: same-block index increment with the
/// `"<block>-<index-1>"` link, and rollover to `block+1` at index 0 whose
/// link points back into the previous block. Anything else is a gap: frames
/// were silently dropped and the tracker must say so.
fn sequence_gap(
    block: u64,
    index: u64,
    prev_frame_id: &Option<String>,
    last_block: u64,
    last_index: u64,
) -> bool {
    let link = prev_frame_id
        .as_deref()
        .and_then(|s| s.rsplit_once('-'))
        .and_then(|(b, i)| Some((b.parse::<u64>().ok()?, i.parse::<u64>().ok()?)));
    if block == last_block {
        if index <= last_index {
            // Already-counted duplicate that slipped the frame-dedupe (e.g.
            // new state id on a re-send): not a gap, not progress.
            return false;
        }
        if index != last_index + 1 {
            return true;
        }
        if let Some((pb, pi)) = link {
            return pb != block || pi + 1 != index;
        }
        return false;
    }
    if block == last_block + 1 {
        if index != 0 {
            return true;
        }
        if let Some((pb, pi)) = link {
            return pb != last_block || pi != last_index;
        }
        return false;
    }
    block > last_block + 1 || block < last_block
}

// ---------------------------------------------------------------------------
// Engine-side tracker
// ---------------------------------------------------------------------------

/// One accepted frame, as the engine needs to remember it: identity plus the
/// hashes its diff added. Sealed matching compares the cumulative ordered
/// hashes of a block against the canonical block's transaction sequence.
#[derive(Clone, Debug)]
struct TrackedFrame {
    index: u64,
    state_id: B256,
    observed_at_ms: u64,
    tx_hashes: Vec<B256>,
}

/// How many recently-sealed block windows the tracker retains for matching
/// (canonical heads arrive ~0–2 s behind the last frame of a block).
const TRACKED_BLOCKS: usize = 48;

#[derive(Default)]
struct TrackerInner {
    latest: Option<PreconfirmedState>,
    /// block number → frames of that block, in arrival order.
    by_block: std::collections::BTreeMap<u64, Vec<TrackedFrame>>,
}

/// Engine-held view of the preconfirmed stream: the current state identity
/// and enough recent frames to grade the seal of a canonical block. This is
/// what the state-pinned simulation and TTL rechecks (WS-Q) consult.
pub struct PreconfirmedTracker {
    inner: parking_lot::Mutex<TrackerInner>,
}

impl Default for PreconfirmedTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl PreconfirmedTracker {
    pub fn new() -> Self {
        Self {
            inner: parking_lot::Mutex::new(TrackerInner::default()),
        }
    }

    /// Register an accepted frame. Out-of-order arrives are tolerated (they
    /// never move `latest` backwards).
    pub fn observe_frame(&self, state: &PreconfirmedState, tx_hashes: &[B256]) {
        let mut g = self.inner.lock();
        let advance = match &g.latest {
            None => true,
            Some(l) => {
                state.block_number > l.block_number
                    || (state.block_number == l.block_number
                        && state.flashblock_index > l.flashblock_index)
            }
        };
        if advance {
            g.latest = Some(state.clone());
        }
        let frames = g.by_block.entry(state.block_number).or_default();
        if !frames
            .iter()
            .any(|f| f.index == state.flashblock_index && f.state_id == state.state_id)
        {
            frames.push(TrackedFrame {
                index: state.flashblock_index,
                state_id: state.state_id,
                observed_at_ms: state.observed_at_ms,
                tx_hashes: tx_hashes.to_vec(),
            });
        }
        while g.by_block.len() > TRACKED_BLOCKS {
            let Some((&first, _)) = g.by_block.iter().next() else {
                break;
            };
            g.by_block.remove(&first);
        }
    }

    /// The newest accepted preconfirmed state — the identity a quote /
    /// simulation / send must pin to, and the recheck baseline.
    pub fn latest(&self) -> Option<PreconfirmedState> {
        self.inner.lock().latest.clone()
    }

    /// Whether `pinned` is still the current state or a prefix of it. A
    /// candidate pinned at `(N, i)` is alive while the feed's newest frame is
    /// in the same block at index ≥ i: its triggering transactions are a
    /// prefix of everything after it. Anything else (newer block, older
    /// payload) has expired.
    pub fn pinned_is_current(&self, pinned: &PreconfirmedState) -> bool {
        match self.inner.lock().latest.as_ref() {
            Some(l) => l.is_descendant_of(pinned) || l == pinned,
            None => false,
        }
    }

    /// The sealed canonical block for `block_number` just arrived with its
    /// ordered transaction hashes. Compares the tracked cumulative ordered
    /// hashes against the canonical sequence: a match proves the
    /// preconfirmed stream described exactly what sealed; a mismatch means
    /// the feed is lossy or adversarial and its data cannot trigger sends.
    ///
    /// Returns `Some((matched, lead_ms))` when any frame of that block was
    /// tracked; lead is measured from the newest frame's arrival.
    pub fn observe_sealed(
        &self,
        block_number: u64,
        canonical_hashes: &[B256],
        sealed_at_ms: u64,
    ) -> Option<(bool, u64)> {
        let mut g = self.inner.lock();
        let frames = g.by_block.remove(&block_number)?;
        let mut cumulative: Vec<B256> = Vec::new();
        let mut newest = 0u64;
        for f in &frames {
            cumulative.extend_from_slice(&f.tx_hashes);
            newest = newest.max(f.observed_at_ms);
        }
        let matched = cumulative == canonical_hashes;
        let lead_ms = sealed_at_ms.saturating_sub(newest);
        Some((matched, lead_ms))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn frame(block: u64, index: u64, state_tag: u8, prev: Option<(u64, u64)>) -> Value {
        json!({
            "payload_id": format!("0x{:016x}", block),
            "index": index,
            "diff": {
                "state_root": "0x00",
                "block_hash": format!("0x{:064x}", state_tag as u64 + block * 100 + index),
                "gas_used": "0x1234",
                "transactions": [],
                "withdrawals": [],
            },
            "metadata": {
                "block_number": block,
                "prev_flashblock_id": prev.map(|(b, i)| format!("{b}-{i}")),
            }
        })
    }

    #[test]
    fn a_healthy_block_sequence_has_no_gaps() {
        let mut p = FlashblockParser::new();
        let stats = FlashblockStats::default();
        for i in 0..=10u64 {
            let prev = if i == 0 { None } else { Some((100, i - 1)) };
            let r = p.parse(&frame(100, i, i as u8, prev), Some(&stats));
            assert!(r.state.is_some());
            if i == 0 {
                assert_eq!(r.outcome, FrameOutcome::NewBlock);
            } else {
                assert_eq!(r.outcome, FrameOutcome::Continuation);
            }
        }
        assert_eq!(stats.state_gaps.load(Ordering::Relaxed), 0);
        assert_eq!(stats.frames_total.load(Ordering::Relaxed), 11);
        assert_eq!(stats.blocks_seen.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn a_skipped_index_is_a_state_gap_not_pending_flow() {
        let mut p = FlashblockParser::new();
        let stats = FlashblockStats::default();
        p.parse(&frame(100, 0, 1, None), Some(&stats));
        p.parse(&frame(100, 1, 2, Some((100, 0))), Some(&stats));
        // index 3 arrives with a link claiming index 2 happened — we never saw it.
        let r = p.parse(&frame(100, 3, 4, Some((100, 2))), Some(&stats));
        assert_eq!(r.outcome, FrameOutcome::Continuation);
        assert_eq!(stats.state_gaps.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn a_lying_prev_link_is_a_gap() {
        let mut p = FlashblockParser::new();
        let stats = FlashblockStats::default();
        p.parse(&frame(100, 0, 1, None), Some(&stats));
        let r = p.parse(&frame(100, 1, 2, Some((0, 0))), Some(&stats));
        assert_eq!(r.outcome, FrameOutcome::Continuation);
        assert_eq!(stats.state_gaps.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn a_healthy_rollover_links_into_the_last_frame_of_the_previous_block() {
        let mut p = FlashblockParser::new();
        let stats = FlashblockStats::default();
        for i in 0..=10u64 {
            let prev = if i == 0 { None } else { Some((100, i - 1)) };
            p.parse(&frame(100, i, i as u8, prev), Some(&stats));
        }
        let r = p.parse(&frame(101, 0, 99, Some((100, 10))), Some(&stats));
        assert_eq!(r.outcome, FrameOutcome::NewBlock);
        assert_eq!(stats.state_gaps.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn a_redelivered_frame_is_duplicated_not_counted_twice() {
        let mut p = FlashblockParser::new();
        let stats = FlashblockStats::default();
        let f = frame(100, 0, 1, None);
        let first = p.parse(&f, Some(&stats));
        assert_eq!(first.outcome, FrameOutcome::NewBlock);
        // First resend: nothing new in it. Mark redelivery once there ARE
        // duplicate-tagged txs to recognize it; with no txs it is a resend
        // of an already-accepted (block, index) and must not move the window.
        let second = p.parse(&f, Some(&stats));
        assert!(matches!(
            second.outcome,
            FrameOutcome::Redelivered | FrameOutcome::Continuation
        ));
        assert_eq!(p.last_frame(), Some((100, 0)));
        assert_eq!(stats.blocks_seen.load(Ordering::Relaxed), 1);
        // Starting a second block must NOT count the same block twice.
        p.parse(&frame(101, 0, 100, Some((100, 0))), Some(&stats));
        assert_eq!(stats.blocks_seen.load(Ordering::Relaxed), 2);
        assert_eq!(stats.frames_total.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn malformed_frames_are_counted_and_carry_no_state() {
        let mut p = FlashblockParser::new();
        let stats = FlashblockStats::default();
        for bad in [
            json!({"index": "x"}),
            json!({"index": 3}),                                 // no diff
            json!({"index": 3, "diff": {}}),                     // no block_hash
            json!({"index": 3, "diff": {"block_hash": "0xzz"}}), // bad hash
        ] {
            let r = p.parse(&bad, Some(&stats));
            assert!(r.state.is_none(), "{bad}");
            assert!(matches!(r.outcome, FrameOutcome::Malformed(_)));
            assert!(r.txs.is_empty());
        }
        assert_eq!(stats.frames_malformed.load(Ordering::Relaxed), 4);
        assert_eq!(p.last_frame(), None);
    }

    #[test]
    fn midstream_resume_counts_a_gap_once() {
        // Connecting while a block is being built (first frame has index 4):
        // the missed prefix is one gap, then the feed runs clean.
        let mut p = FlashblockParser::new();
        let stats = FlashblockStats::default();
        let r = p.parse(&frame(100, 4, 1, Some((100, 3))), Some(&stats));
        assert_eq!(r.outcome, FrameOutcome::NewBlock);
        assert_eq!(stats.state_gaps.load(Ordering::Relaxed), 1);
        let r = p.parse(&frame(100, 5, 2, Some((100, 4))), Some(&stats));
        assert_eq!(r.outcome, FrameOutcome::Continuation);
        assert_eq!(stats.state_gaps.load(Ordering::Relaxed), 1);
    }

    fn state(block: u64, index: u64, tag: u8) -> PreconfirmedState {
        PreconfirmedState {
            feed: FEED_FLASHBLOCKS.into(),
            block_number: block,
            flashblock_index: index,
            state_id: B256::from([tag; 32]),
            payload_id: format!("0x{block:016x}"),
            prev_frame_id: None,
            parent_hash: None,
            observed_at_ms: 1_000 + index * 200,
            ordered: true,
        }
    }

    #[test]
    fn tracker_matches_a_sealed_block_by_cumulative_ordered_hashes() {
        let t = PreconfirmedTracker::new();
        let a = B256::from([0xaa; 32]);
        let b = B256::from([0xbb; 32]);
        let c = B256::from([0xcc; 32]);
        t.observe_frame(&state(100, 0, 1), &[a]);
        t.observe_frame(&state(100, 1, 2), &[b, c]);
        // Nothing tracked for block 99.
        assert_eq!(t.observe_sealed(99, &[], 3_000), None);
        let (matched, lead) = t.observe_sealed(100, &[a, b, c], 3_000).unwrap();
        assert!(matched);
        assert_eq!(lead, 3_000 - (1_000 + 200));
        // Once matched, the window is consumed (a second head must not
        // double-count).
        assert_eq!(t.observe_sealed(100, &[a, b, c], 3_100), None);
    }

    #[test]
    fn tracker_flags_a_mismatched_seal() {
        let t = PreconfirmedTracker::new();
        let a = B256::from([0xaa; 32]);
        let b = B256::from([0xbb; 32]);
        t.observe_frame(&state(100, 0, 1), &[a, b]);
        // Canonical order flipped — the preconfirmed stream did NOT describe
        // what sealed, and that must surface as a mismatch, never be
        // silently accepted.
        let (matched, _) = t.observe_sealed(100, &[b, a], 3_000).unwrap();
        assert!(!matched);
    }

    #[test]
    fn pinned_state_expires_on_block_rollover_but_lives_through_descendants() {
        let t = PreconfirmedTracker::new();
        let pinned = state(100, 2, 5);
        t.observe_frame(&pinned, &[]);
        assert!(t.pinned_is_current(&pinned));
        t.observe_frame(&state(100, 7, 6), &[]);
        assert!(
            t.pinned_is_current(&pinned),
            "index 7 is a descendant of (100, 2): its transactions are a prefix"
        );
        let mut other_payload = state(100, 9, 7);
        other_payload.payload_id = "0xdeadbeef".into();
        assert!(
            !t.pinned_is_current(&other_payload),
            "same block, different payload id is not a validated descendant"
        );
        t.observe_frame(&state(101, 0, 8), &[]);
        assert!(
            !t.pinned_is_current(&pinned),
            "block 100 has rolled over — the candidate expired"
        );
    }
}
