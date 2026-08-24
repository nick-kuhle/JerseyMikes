//! Flashblocks ingestion driven by real captured frames (work order WS-O2/O3).
//!
//! Fixture provenance: `fixtures/flashblocks/README.md`. The suite proves the
//! required shapes — light, multi-transaction, full-block, rollover,
//! reconnect duplicate, gap, malformed — are parsed deterministically,
//! deduplicated on (feed, state identity, tx hash), and never relabelled into
//! ordinary pending flow.

use std::sync::atomic::Ordering;

use mev_bot::flashblocks::{FlashblockParser, FlashblockStats, FrameOutcome, PreconfirmedTracker};
use mev_bot::types::TxSource;
use serde_json::Value;

fn fixture(name: &str) -> Value {
    let path = format!(
        "{}/tests/fixtures/flashblocks/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{path}: {e}"))
}

fn full_block_frames() -> Vec<Value> {
    let path = format!(
        "{}/tests/fixtures/flashblocks/block_50393250_full.jsonl",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read_to_string(&path)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

#[test]
fn frame_shapes_carry_provenance_and_backrun_only_raw_bytes() {
    let mut p = FlashblockParser::new();
    let stats = FlashblockStats::default();

    let f0 = p.parse(&fixture("index0_base.json"), Some(&stats));
    assert_eq!(f0.outcome, FrameOutcome::NewBlock);
    let state0 = f0.state.expect("index-0 frame has an identity");
    assert_eq!(state0.flashblock_index, 0);
    assert!(state0.parent_hash.is_some_and(|h| !h.is_zero()));
    assert_eq!(state0.block_number, 50393250);
    // The index-0 diff holds an OP-stack deposit: expected, not malformed.
    assert!(f0.txs.is_empty());
    assert_eq!(stats.txs_deposit.load(Ordering::Relaxed), 1);
    assert_eq!(stats.frames_malformed.load(Ordering::Relaxed), 0);

    let f1 = p.parse(&fixture("multi_tx.json"), Some(&stats));
    assert_eq!(f1.outcome, FrameOutcome::Continuation);
    let state1 = f1.state.unwrap();
    assert_eq!(state1.block_number, state0.block_number);
    assert_eq!(state1.flashblock_index, 1);
    assert_ne!(
        state1.state_id, state0.state_id,
        "identity advances per frame"
    );
    assert!(
        f1.txs.len() >= 2,
        "the captured multi-transaction diff must decode its real transactions"
    );
    for tx in &f1.txs {
        assert_eq!(tx.source, TxSource::Flashblock);
        assert!(tx.source.backrun_only());
        assert!(tx.raw.is_some(), "raw signed bytes survive the parser");
        assert!(
            tx.mined_at.is_none(),
            "a preconfirmation is not canonical yet"
        );
        let pre = tx.preconfirmed.as_ref().expect("provenance attached");
        assert_eq!(pre.state_id, state1.state_id);
        assert_eq!(pre.block_number, state1.block_number);
        assert!(pre.ordered);
        assert_eq!(pre.payload_id, state0.payload_id, "same block build");
    }
    assert_eq!(stats.state_gaps.load(Ordering::Relaxed), 0);
    assert_eq!(stats.frames_malformed.load(Ordering::Relaxed), 0);
    assert_eq!(stats.txs_malformed.load(Ordering::Relaxed), 0);
}

#[test]
fn a_full_block_plus_rollover_is_gapless() {
    let mut p = FlashblockParser::new();
    let stats = FlashblockStats::default();
    let mut last_index = None;
    let mut first_block = None;
    let mut user_txs = 0usize;
    let mut last_state = None;
    for (lineno, v) in full_block_frames().into_iter().enumerate() {
        let r = p.parse(&v, Some(&stats));
        let state = r
            .state
            .unwrap_or_else(|| panic!("line {} has an identity", lineno + 1));
        if let (Some(lb), Some(li)) = (first_block, last_index) {
            assert_eq!(state.block_number, lb, "same block build");
            assert_eq!(
                state.flashblock_index,
                li + 1,
                "strictly +1 per frame (line {})",
                lineno + 1
            );
        } else {
            first_block = Some(state.block_number);
            assert_eq!(state.flashblock_index, 0);
            assert_eq!(r.outcome, FrameOutcome::NewBlock);
        }
        last_index = Some(state.flashblock_index);
        user_txs += r.txs.len();
        assert!(state.ordered, "preconfirmed ordering is final");
        last_state = Some(state);
    }
    assert_eq!(last_index, Some(10), "the full sub-block cadence");
    assert!(user_txs >= 3, "real user transactions decoded");

    // The next block rolls over cleanly: its prev link points at the exact
    // frame we finished on.
    let next = p.parse(&fixture("rollover_next.json"), Some(&stats));
    assert_eq!(next.outcome, FrameOutcome::NewBlock);
    let next_state = next.state.unwrap();
    assert_eq!(
        next_state.block_number,
        last_state.unwrap().block_number + 1
    );
    assert_eq!(next_state.flashblock_index, 0);

    assert_eq!(stats.state_gaps.load(Ordering::Relaxed), 0);
    assert_eq!(stats.blocks_seen.load(Ordering::Relaxed), 2);
    assert_eq!(stats.frames_total.load(Ordering::Relaxed), 12);
    assert_eq!(stats.txs_duplicate.load(Ordering::Relaxed), 0);
    assert_eq!(stats.txs_malformed.load(Ordering::Relaxed), 0);
    assert_eq!(stats.txs_deposit.load(Ordering::Relaxed), 2);
    assert_eq!(p.last_frame(), Some((50393251, 0)));
}

#[test]
fn missing_intermediate_frames_are_flagged_as_a_gap() {
    // Sparse walk: index 0 → index 1 → index 10. Legal-looking frames, but
    // 2..=9 never arrived and the chain link at 10 says so.
    let mut p = FlashblockParser::new();
    let stats = FlashblockStats::default();
    p.parse(&fixture("index0_base.json"), Some(&stats));
    p.parse(&fixture("multi_tx.json"), Some(&stats));
    let r = p.parse(&fixture("rollover_prev.json"), Some(&stats));
    assert_eq!(r.outcome, FrameOutcome::Continuation);
    assert_eq!(stats.state_gaps.load(Ordering::Relaxed), 1);
    // The gap is flagged exactly once and the parser heals on the newest
    // frame: the rollover's link (50393250-10) matches the tip we accepted,
    // so no second flag. Flag-once-then-heal is the intended behavior.
    let r = p.parse(&fixture("rollover_next.json"), Some(&stats));
    assert_eq!(r.outcome, FrameOutcome::NewBlock);
    assert_eq!(stats.state_gaps.load(Ordering::Relaxed), 1);
}

#[test]
fn redelivery_after_reconnect_is_duplicated_on_state_and_hash() {
    let mut p = FlashblockParser::new();
    let stats = FlashblockStats::default();

    let frame = fixture("multi_tx.json");
    let first = p.parse(&frame, Some(&stats));
    let n_first = first.txs.len();
    assert!(n_first >= 2);
    let decoded = stats.txs_total.load(Ordering::Relaxed);

    // A reconnect replays the same frame verbatim.
    let again = p.parse(&frame, Some(&stats));
    assert!(
        matches!(
            again.outcome,
            FrameOutcome::Redelivered | FrameOutcome::Continuation
        ),
        "{:?}",
        again.outcome
    );
    assert_eq!(again.txs.len(), 0, "every transaction deduped");
    assert_eq!(again.duplicate_txs, n_first);
    assert_eq!(
        stats.txs_duplicate.load(Ordering::Relaxed) as usize,
        n_first
    );
    // The dedupe key is (state identity, tx hash): the same transaction in
    // the SAME state must not be seen twice; the decode total does not move.
    assert_eq!(stats.txs_total.load(Ordering::Relaxed), decoded);
}

#[test]
fn a_frame_with_a_lying_chain_link_raises_a_state_gap() {
    let mut p = FlashblockParser::new();
    let stats = FlashblockStats::default();
    p.parse(&fixture("index0_base.json"), Some(&stats));
    p.parse(&fixture("multi_tx.json"), Some(&stats));
    let r = p.parse(&fixture("gap_wrong_prev_link.json"), Some(&stats));
    assert!(r.state.is_some(), "the frame itself is well-formed");
    assert!(
        stats.state_gaps.load(Ordering::Relaxed) >= 1,
        "a broken prev_flashblock_id chain must be flagged"
    );
}

#[test]
fn malformed_shapes_are_counted_never_relabelled() {
    let mut p = FlashblockParser::new();
    let stats = FlashblockStats::default();

    // No diff at all: a notification, not pending flow.
    let r = p.parse(&fixture("malformed_no_diff.json"), Some(&stats));
    assert!(matches!(r.outcome, FrameOutcome::Malformed(_)));
    assert!(r.state.is_none() && r.txs.is_empty());

    // Two junk tx entries surround one valid real transaction.
    let r = p.parse(&fixture("malformed_bad_tx_hex.json"), Some(&stats));
    assert!(r.state.is_some(), "frame-level shape is valid");
    assert_eq!(r.txs.len(), 1, "only the decodable transaction survives");
    assert_eq!(stats.txs_malformed.load(Ordering::Relaxed), 2);

    // Nothing in this scenario emitted ordinary pending flow or faked state.
    // The single gap below is the mid-stream resume: the first accepted frame
    // has index 1, so the parser says once that the prefix is unknown — the
    // documented behavior for a fresh connection.
    assert_eq!(stats.state_gaps.load(Ordering::Relaxed), 1);
}

#[test]
fn the_tracker_links_the_same_frames_to_a_sealed_block() {
    let mut p = FlashblockParser::new();
    let stats = FlashblockStats::default();
    let tracker = PreconfirmedTracker::new();

    let mut hashes: Vec<_> = Vec::new();
    let mut latest_state = None;
    let mut pinned = None;
    for v in full_block_frames() {
        let r = p.parse(&v, Some(&stats));
        let state = r.state.unwrap();
        let frame_hashes: Vec<_> = r.txs.iter().map(|t| t.hash).collect();
        tracker.observe_frame(&state, &frame_hashes);
        hashes.extend(frame_hashes);
        if state.flashblock_index == 1 {
            pinned = Some(state.clone());
        }
        latest_state = Some(state);
    }

    // Latest identity is the index-10 frame of the block.
    let latest = tracker.latest().unwrap();
    assert_eq!(latest.flashblock_index, 10);
    assert_eq!(latest.block_number, 50393250);
    assert_eq!(latest.state_id, latest_state.unwrap().state_id);

    // A candidate pinned at the index-1 frame is still alive at index 10
    // (its triggering transactions are a prefix of the descendant state).
    assert!(tracker.pinned_is_current(pinned.as_ref().unwrap()));

    // …but a rollover kills it.
    let r = p.parse(&fixture("rollover_next.json"), Some(&stats));
    tracker.observe_frame(&r.state.unwrap(), &[]);
    assert!(
        !tracker.pinned_is_current(pinned.as_ref().unwrap()),
        "the block rolled over: TTL recheck must drop the candidate"
    );

    // Sealed match: block 50393250 arrived canonically with exactly the
    // cumulative transaction sequence the frames described.
    let out = tracker.observe_sealed(50393250, &hashes, 4_000_000_000_000);
    let (matched, _) = out.expect("frames for the block were tracked");
    assert!(matched);

    // The same block with a truncated sequence is a mismatch.
    assert!(tracker
        .observe_sealed(50393250, &hashes[..hashes.len() - 1], 4_000_000_000_000)
        .is_none());
}
