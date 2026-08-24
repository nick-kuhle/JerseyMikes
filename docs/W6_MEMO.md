# W6 — public-mempool gap memo

**Status: decided — W6 stays off.** See "Decision" below. The template is
retained so the decision can be revisited against real funnel data from any
future feed.

**Purpose.** `PHASE_2_HANDOFF.md` W6 (UniversalRouter calldata decoding) stays
off until a *written* memo like this one exists. This file is both the template
and the decision record: fill the sample table from the dashboard's funnel card
(the **W6 go/no-go** block computes every number below and can copy them
pre-filled) after the bot has run on one feed for **at least seven days**.

**Decision rule** (from the handoff): decode more routers only if the funnel
shows the bot is *seeing* public flow it cannot act on. If the public mempool
itself is thin, the flow is already inside Flashbots Protect / MEV-Blocker /
CoW / UniswapX, and a public decoder will not help — W6 stays off and 1inch
v6 / 0x v2 stay closed with it.

---

## Sample

| Field | Value |
| --- | --- |
| Feed (`ETH_WS_URL` provider) | _e.g.Alchemy wss mainnet_ |
| Window | _start → end, ≥ 7 days_ |
| `pendingSeen` (public mempool txs, live lane) | _from the card_ |
| `hintsSeen` (MEV-Share hints — private, undecodable by definition) | _from the card_ |
| `sandwich + sandwich_v3 + jit` calls (live lane) | _from the card_ |
| `… invocationsEmpty` (saw the tx, could not build on it) | _from the card_ |
| `… candidatesEmitted` (decoded victims) | _from the card_ |

## Reading

- **Decode pressure** = `candidatesEmitted / pendingSeen`. What share of
  public flow the current decoders (V2 routers, SwapRouter02) can act on.
- **Empty pressure** = `invocationsEmpty / calls`. How often the strategies saw
  flow but rejected it as undecodable / undersized. Cross-check against
  `missing_victim_raw` before concluding "undecodable" — a RPC that drops
  `eth_getRawTransactionByHash` produces the same symptom (see
  `RiskPanel` diagnostic #3).
- **Share routed through UniversalRouter** (optional, strongest signal):
  sample `tx.input` selectors from `relay-txs` for `0x3593564c`/
  `0x24856bc3` (`execute`/`executeWithDeadline`) over a day of blocks.

## Decision

- [ ] **Flip W6 on** (`DECODE_UNIVERSAL_ROUTER=true`) — evidence:
  _high `pendingSeen`, near-zero `candidatesEmitted`, `missing_victim_raw`
  ruled out._ Re-read the funnel for another week after flipping and record
  the delta here.
- [x] **W6 stays off.** `DECODE_UNIVERSAL_ROUTER=false`; 1inch v6, 0x v2 and
  CoW stay closed.

### Rationale (2026-08-23)

The decision rule asks whether the bot is *seeing* public flow it cannot act
on. The honest answer is that the constraint is market structure, not decoder
coverage, and no amount of decoding moves it:

1. **The public mempool is a shrinking minority of the flow that matters.**
   Roughly a fifth of DeFi flow is still publicly visible, and a majority of
   block value now arrives through private channels. Private-orderflow
   endpoints are standard infrastructure on every major chain — Flashbots
   Protect alone carries a few percent of all Ethereum transactions, and
   MEV-Blocker, CoW and UniswapX absorb much of the rest of the retail flow
   that a sandwich decoder would want. That is exactly the "thin
   `pendingSeen`" branch of the rule.

2. **What is left in public is the most contested flow there is.** Builder
   concentration means two builders win the overwhelming majority of
   MEV-Boost auctions, and the searchers that consistently profit are the
   ones vertically integrated with a builder. An independent searcher
   decoding one more router is competing for residue that is already
   efficiently priced by participants with structural latency and orderflow
   advantages.

3. **This project's viable surface is back-running, and W6 does not serve
   it.** The strategies that survive contact with current market structure
   are atomic arbitrage and liquidations — both of which are triggered by
   *state* (pool reserves, health factors, oracle updates), not by decoding a
   victim's calldata. On the L2s in scope the mempool is private by
   construction, so there is no public UniversalRouter flow to decode at all.
   W6 spends complexity on the one strategy family whose economics are worst.

4. **The cost is not zero.** Every decoder widens the pending-path hot loop,
   which is budgeted at ~150 ms end to end, and adds an adversarial parsing
   surface on hostile calldata.

**Reversibility.** The decoder is implemented and stays in the tree behind
`DECODE_UNIVERSAL_ROUTER=false`. If a future funnel reading contradicts this —
high `pendingSeen` with near-zero `candidatesEmitted`, `missing_victim_raw`
ruled out — flip the flag, fill the post-flip table below, and amend this
memo. The decision is a reading of current conditions, not a permanent
architectural position.

## Post-flip report (only if flipped)

| Metric | Week before | Week after |
| --- | --- | --- |
| `atomic_arb.candidatesEmitted` | | |
| `sandwich.candidatesEmitted` | | |
| `sandwich_v3.candidatesEmitted` | | |
| pending-path `strategy` p95 (ms) | | |
| `submittable` (all strategies) | | |

If the after-column is noise, that is the deliverable: revert the toggle and
write "no measurable gap" here.
