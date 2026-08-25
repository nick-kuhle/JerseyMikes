# Base preconfirmation feed — provider notes and wire format

**Work order:** WS-O / Workstream 1 of `BASE_FULL_SCOPE_WORK_ORDER.md`.
**Status:** analysed 2026-08-24 against *live* public Base endpoints; parser,
fixtures, dedupe, gap tracking and counters implemented in
`bot/crates/mev-bot/src/flashblocks.rs` + `tests/flashblocks_fixtures.rs`.

## Provider decision record

| Option | What it is | Verdict |
| --- | --- | --- |
| Base public endpoints (`mainnet.base.org`, `sepolia.base.org`, HTTP + WSS) | Flashblocks-enabled RPC: all standard methods support the `"pending"` tag (preconfirmed state), plus `eth_simulateV1`, `base_transactionStatus`, and WSS `newHeads` (200 ms), `newFlashblocks`, `newFlashblockTransactions`, `pendingLogs` | **Supported and used for development.** Public, no auth; rate limits make it a dev/shadow endpoint, not a measured-soak one. |
| Raw infrastructure stream (`wss://mainnet.flashblocks.base.org/ws`) | The sequencer's own feed: brotli-compressed JSON frames, ~200 ms cadence. Documented as **node-operator-only** — "applications should not connect directly" | Used here *only* to capture real fixture frames (no auth needed). Same payload shape the RPC `newFlashblocks` subscription delivers as JSON text. |
| Flashblocks-aware third-party RPC (QuickNode/Alchemy et al.) | Paid, authed endpoints exposing the same subscriptions + higher rate limits + SLAs | **Required for the 48 h/168 h measurement windows.** Selection needs an operator account and written confirmation of rate limits, retention/redistribution terms and simulation support — cannot be completed from this dev environment. Endpoint credentials must never be committed (land in `FLASHBLOCKS_WS_URL`). |

Official reference: <https://docs.base.org/base-chain/api-reference/flashblocks-api/flashblocks-api-overview>

## Verified mechanics (live, 2026-08-24)

- 120 s capture = 658 frames / 60 blocks / **zero sequence gaps**; ~11 frames per
  block (`index` 0..=10), i.e. one frame per ~182 ms against the 2 s block time.
- Frame identity trio: `payload_id` (per block build), `index`, and
  `diff.block_hash` — **unique per frame**. `diff.block_hash` changes as the diff
  appends transactions and **never equals the sealed canonical block hash**
  (verified across several blocks). It is therefore the correct identity for a
  *preconfirmed state*, and sealed-block matching must use transaction content
  (below), not the hash.
- `metadata` = `{ block_number (decimal), prev_flashblock_id: "<block>-<index>" }`.
  The prev link chains frames across rollover (`50393250-10` → block 50393251
  index 0). `metadata` is marked unstable upstream, so it corroborates but never
  substitutes the block number (also in `base` at index 0).
- Cumulative ordered transaction hashes across a block's frames **equal the
  sealed block's ordered transaction list** (verified 487/487 incl. order on
  block 50393253). This is the sealed-match invariant the engine measures and
  counts (`sealedMatches` / `sealedMismatches`).
- `diff.transactions` are **raw RLP signed bytes** (types `0x02`, `0x7e`
  deposits, …). Deposits are OP-stack system transactions: no signature, no
  sender — skipped silently, never actionable, counted separately.
- RPC behaves exactly as documented: reads accept `"pending"`; preconfirmed
  hashes are **not** individually addressable (`eth_call {blockHash:
  <diff.block_hash>}` → *block not found*); `eth_simulateV1(.., "pending")`
  works. Consequence: the *latest* frame's identity is the current preconfirmed
  state's identity; a candidate stays alive while newer frames remain its
  descendants in the same block/payload, and dies at rollover.

## Bot-facing contract (implemented)

- `FLASHBLOCKS_WS_URL=wss://…` enables `eth_subscribe(["newFlashblocks"])`.
- Every accepted frame emits `IngestEvent::PreconfirmedState` (identity) before
  its transactions; each transaction carries `PendingTx.preconfirmed` with the
  full `PreconfirmedState`.
- Dedupe key: `(feed, state_id, tx_hash)` over a 32-block bounded window.
- Frame sequence enforced on block/index continuity plus the
  `prev_flashblock_id` link; discontinuities raise `stateGaps` (never silently
  patched).
- Counters on `/api/status.flashblocks`: `framesTotal`, `framesMalformed`,
  `txsTotal`, `txsDuplicate`, `txsMalformed`, `txsDeposit`, `stateGaps`,
  `reconnects`, `lastFrameMs`, `lastBlockNumber`, `lastIndex`,
  `lastSealedLeadMs`, `sealedMatches`, `sealedMismatches`.
- Malformed frames/entries are dropped and counted — never relabelled as
  pending flow.

## Explicitly out of scope (stop conditions honoured)

- Orders placed *ahead* of a preconfirmed transaction. Marked
  `backrun_only()` end to end; sandwich/JIT/front-run from this feed are
  rejected by the engine, and a future provider lane that *could* offer earlier
  ordering requires separate review before any use.
- 48 h/168 h capture windows, provider SLAs, and any execution use: all gated
  on the paid-provider step and the fresh qualification clock.
