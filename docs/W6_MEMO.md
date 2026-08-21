# W6 — public-mempool gap memo

**Purpose.** `PHASE_2_HANDOFF.md` W6 (UniversalRouter calldata decoding) stays
off until a *written* memo like this one exists. This file is the template and
the decision record: fill it from the dashboard's funnel card (the
**W6 go/no-go** block computes every number below and can copy them pre-filled)
after the bot has run on one feed for **at least seven days**.

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
- [ ] **W6 stays off** — evidence: _thin `pendingSeen` (flow is private) or
  healthy `candidatesEmitted` (flow is already decodable)._ 1inch v6 / 0x v2
  remain closed.

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
