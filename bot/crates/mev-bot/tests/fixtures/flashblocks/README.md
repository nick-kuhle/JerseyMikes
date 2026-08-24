# Flashblocks fixtures

Real Flashblock payload frames, captured on **Base mainnet 2026-08-24** from the
public Flashblocks stream (`wss://mainnet.flashblocks.base.org/ws`, brotli-compressed
JSON), covering canonical blocks ~50393250–50393310. Application-facing
Flashblocks-aware RPCs deliver the same JSON shape via
`eth_subscribe(["newFlashblocks"])` — see `docs/BASE_FEED.md`.

Capture facts that pin the parser's assumptions (measured on 120 s / 658 frames /
60 blocks, zero index gaps):

- every frame: `payload_id`, `index`, `diff`, `metadata`; `base` only at `index = 0`;
- `diff` keys: `state_root`, `block_hash`, `gas_used`, `blob_gas_used`,
  `transactions`, `withdrawals`, `receipts_root`, `logs_bloom`, `withdrawals_root`;
- `diff.transactions` = raw RLP-encoded signed transactions for THIS diff (incremental);
- `diff.block_hash` is unique per frame and is the preconfirmed-state identity — it
  changes with every frame and never equals the sealed canonical block hash;
- `metadata.block_number` (decimal) = block being built; `metadata.prev_flashblock_id`
  = `"<block_number>-<index>"` chain link to the previous frame;
- `base` (index 0 only) = block header fields: `parent_hash`, `block_number` (hex),
  `timestamp`, `base_fee_per_gas`, `gas_limit`, `fee_recipient`, `prev_randao`,
  `extra_data`, `parent_beacon_block_root`;
- ~11 frames per 2 s block (indices 0..10); the cumulative transaction-hash sequence
  across indices equals the canonical sealed block's transaction list, order included
  (verified 487/487 on block 50393253).

## Files

| File | Shape it covers |
| --- | --- |
| `index0_base.json` | `index = 0` rollover frame: full `base` header object + deposit tx |
| `light_diff.json` | single-transaction diff (`index = 2`, no `base`) |
| `multi_tx.json` | multi-transaction diff (`index = 1`, 3 real txs kept) |
| `rollover_prev.json` / `rollover_next.json` | last frame of block N (`index = 10`) followed by `index = 0` of block N+1 — same-chain sequence the parser must accept without a gap flag |
| `malformed_bad_tx_hex.json` | two undecodable tx entries amid one valid tx — valid one must parse, junk must be dropped |
| `gap_wrong_prev_link.json` | `metadata.prev_flashblock_id` lying about the chain — must raise a state-gap signal, never a silent accept |
| `malformed_no_diff.json` | no `diff`/`transactions` — a notification, not pending flow: zero transactions, counted malformed |

Transactions inside fixtures were truncated to the first few per frame, and are
public Base-mainnet data. Do not replace these files with synthetic fabrications:
the point of the suite is that a payload shaped unlike reality breaks a test.
