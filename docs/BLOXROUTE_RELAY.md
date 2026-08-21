# bloXroute Max Profit relay integration

## What it is

The bloXroute **Max Profit** relay (`https://bloxroute.max-profit.blxrbdn.com`)
is a MEV-Boost relay: block builders submit blocks to it, and it hands the
highest-bid block to the proposer. It is one of the relays the bot already
polls for its **value benchmark** — the `proposer_payload_delivered` data API
reports, per slot, how much the winning builder paid the proposer. That number
is the market price of a block's MEV, and the yardstick the simulated bundles
are judged against.

What the data API does **not** return is the winning block's *transactions*.
Those live on the execution layer and are fetched separately. This integration
closes that gap:

1. Poll `proposer_payload_delivered` on the Max Profit relay (once per block).
2. For each newly delivered block, fetch its transactions with
   `eth_getBlockByHash(hash, true)` on the execution RPC.
3. Persist the block metadata (`relay_blocks`) and every transaction
   (`relay_block_txs`, calldata included).
4. Route every transaction through the existing
   `strategy → risk → anvil-fork/relay simulation` funnel with
   `TxSource::RelayDelivered`, so the bot records whether value was extractable
   from the exact transactions that actually landed.
5. Stream a `relay_block` feed event and expose `/api/relay-blocks` and
   `/api/relay-txs`; the dashboard shows a dedicated panel.

Nothing in this path submits anything. It is read-only against a public data
API plus the operator's own RPC, and is controlled independently of the
two-key live-execution switch.

## Why it matters

- **Benchmarking.** The `value` field is the realised builder payment. Combined
  with our simulated net P/L on the same transactions, we can answer "would our
  bundle have been competitive?" — the core of Phase 1 replay validation.
- **Competitor intel.** The delivered block contains the *winning* searcher's
  bundles. Storing them makes the shape of the competition visible.
- **Coverage.** Transactions that went through private orderflow (never in our
  public mempool) still show up here after landing, so we see flow we would
  otherwise have no record of.

## Configuration

| Variable | Default | Meaning |
| --- | --- | --- |
| `BLOXROUTE_MAX_PROFIT_URL` | `https://bloxroute.max-profit.blxrbdn.com` | Relay whose delivered blocks are pulled |
| `RELAY_TX_INGEST` | `true` | Fetch delivered blocks + transactions and score them |
| `RELAY_TX_CONCURRENCY` | `16` | How many delivered transactions are scored at once |

The `relay_data_urls` list (which already contains the Max Profit relay) keeps
feeding the value-only `relay_bids` table unchanged. The two are independent
polls of the same endpoint; the per-block cost is two tiny GETs.

## Data model

- `relay_blocks` — one row per delivered block: relay, slot, block number/hash,
  builder, `value_wei`, gas used, transaction count. Deduplicated per
  `(relay, slot)`.
- `relay_block_txs` — one row per transaction: block number, in-block index,
  hash, from/to, value, nonce, gas, function selector, full calldata.
  Deduplicated per `(block_number, hash)`.

## Endpoints

| Endpoint | Returns |
| --- | --- |
| `GET /api/relay-blocks?limit=` | delivered blocks, newest first |
| `GET /api/relay-txs?blockNumber=&limit=` | stored transactions (optionally filtered to one block) |
| `GET /api/stream` | live `relay_block` events |

## Known limitations

- **Replay fidelity.** A delivered transaction is already mined. The fork
  replay is a *post-mortem* — the victim's raw bytes are replayed via
  `eth_getRawTransactionByHash`, but the fork is pinned to the current head, so
  a transaction from an older block can fail replay with a nonce error. The
  cleanest replay (fork at the block *before* the delivered block) is the Phase 1
  replay harness; the funnel already reports these cases as `simulationsFailed`
  / reverts rather than silently mis-scoring them.
- **Reorgs.** A re-org can replace the block at a given slot after it was
  recorded. Rows are keyed by `(relay, slot)`, so the first-seen block wins.
- **Node requirements.** `eth_getBlockByHash(…, true)` and
  `eth_getRawTransactionByHash` are needed for full fidelity; on providers that
  do not expose raw transactions the sandwich/JIT legs are skipped (the
  strategies still record the candidate and the reason).
- **Volume, and why it is bounded.** A busy block carries ~300–400
  transactions and each one fans out to a task per strategy, so scoring a block
  unbounded queues well over a thousand tasks every 12 seconds. The pre-filters
  are not uniformly cheap either — several reach for pool state over RPC before
  they can reject a transaction. Left unbounded that starves the
  latency-critical mempool path and trips provider rate limits, which then
  breaks pool discovery and quoting elsewhere in the bot.

  Replay has no deadline (the block is already mined), so it runs through
  `Engine::evaluate_awaited` behind a `RELAY_TX_CONCURRENCY` semaphore. Every
  transaction is still scored; at most N are in flight. The steady-state cost
  is one `eth_getBlockByHash` per block plus a bounded, trailing simulation
  load.

## How this shows up in the funnel

Delivered-block transactions are scored through the same strategies as live
flow, but they are **counted in a separate funnel lane**. `TxSource::RelayDelivered`
maps to `FunnelLane::Replay`; everything actionable maps to `FunnelLane::Live`.

| Surface | Live flow | Replay flow |
| --- | --- | --- |
| `GET /api/funnel`, `/api/status` | `stats.funnel` | `stats.funnelReplay` |
| Dashboard | funnel panel, "live" tab | funnel panel, "replay (mined)" tab |

They are kept apart because they answer different questions and one would
otherwise swamp the other: ~150 already-mined transactions per block against
whatever the mempool feed happens to deliver. Live counters answer *should I
change a gate?*; replay counters answer *what was extractable from the blocks
that landed?* Reading a conversion rate across the two is meaningless — the
denominator is a post-mortem population that was never winnable in real time.

`relayBlocksSeen` and `relayTxsSeen` in `/api/status` stay as raw ingestion
counters and are unaffected by the split.
