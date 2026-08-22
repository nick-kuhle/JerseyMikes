# Where to take this next

A prioritisation memo, written after reading the codebase rather than the
roadmap. The context: there is pressure to go live soon, and a competing
instinct to make the bot good first. Both are right, and the way to satisfy
both is to stop treating "go live" as one switch.

**The recommendation in one line:** do not ship live trading next. Ship
*measurement* next — you are a couple of days of work plus a week of patience from proving,
with real numbers, whether this bot would make money. That answer is worth more
than an early live run, and it is also the fastest thing you can show anyone.

---

## What already exists (do not rebuild any of this)

The repo is in better shape than a roadmap read suggests. Concretely:

| Capability | State | Where |
| --- | --- | --- |
| Ten strategies | implemented, ~7,300 lines | `strategies/` |
| Per-strategy funnel | 9 counters, live + replay lanes | `engine.rs::FunnelCounters` |
| Latency histograms | 6 stages, 150 ms budget baked in | `latency.rs` |
| Competition model | logistic of our bribe vs. the block's realised clearing price | `competition.rs` |
| Reconciliation | per block, marks `true_positive` / `false_positive` / `would_outbid`; persisted to a `reconciliations` table and summarised at `GET /api/competition` | `replay.rs`, `store.rs` |
| Fork simulation | anvil, executor injected via `anvil_setCode` | `sim/anvil.rs` |
| Builder cross-check | `eth_callBundle` against the relay | `sim/relay.rs` |
| Orderflow ingest | public mempool, MEV-Share, MEV Blocker, bloXroute, relay blocks | `ingest.rs` |
| Risk envelope | boot + runtime, kill switch, drawdown | `risk.rs` |
| Ops | Prometheus, alerts, systemd, compose | `metrics.rs`, `alerts.rs` |

That is the expensive half of a searcher, and it is done.

## The one thing that is not done

**Nothing is ever sent anywhere.**

`engine.rs:1300` is the end of the pipeline:

```rust
bundle.submitted = self.mode.live();
```

That sets a **boolean column in SQLite**. `bundle::send_bundle_params` builds a
correct `eth_sendBundle` payload, and its only caller
(`store.rs:474`) serialises it into the database for inspection. No code path
opens a connection to a relay and submits.

So "going live" today would change a flag from `false` to `true` and trade
exactly as much as it does now: nothing. The pressure you are under is to flip
a switch that is not wired to anything.

That is not a criticism of the build — the two-key arming, the risk envelope
and the console were all built to make that last step safe. But it does mean
**the live-submission path is simultaneously the least-written and
least-exercised code in the repo**, and it is the only part that can lose money.

---

## Why "measure first" is also the fastest way to show results

The instinct under deadline pressure is that measurement is the slow, careful
option and shipping is the fast one. Here it is the reverse, because of what
`replay.rs` already computes.

Every reconciled block already produces, per opportunity:

- `sim_success` — would the on-chain profit guard have passed
- `victim_landed` — was the target actually in the block
- `our_bribe_wei` vs `winning_bid_wei` — the real clearing price that block
- `would_outbid`, `inclusion_p`, `true_positive`, `false_positive`

Aggregate a week of those rows and you can state, with evidence:

> "Over N blocks we found X profitable bundles. We would have outbid the
> winning builder on Y of them. Expected capture: Z ETH. Our p50 latency is
> L ms against a 150 ms budget."

**That is a far stronger result to show than "it is live."** It is defensible,
it is quantitative, it tells you which strategies to keep, and it costs zero
ETH to produce. A live bot with no inclusions is a worse story than a
simulating bot with a measured edge.

This is also exactly what `docs/W6_MEMO.md` was designed for — it is a go/no-go
gate that says *gather seven days of funnel data, then decide*. That instinct
was right. Extend it from one decision to the whole programme.

---

## The plan

### Phase A — prove the edge (about 1 sprint, zero ETH at risk)

**A1. Ship the scoreboard.** *(1–2 days — smaller than it sounds)*
Most of this exists. `GET /api/competition` already returns
`store.rs::competition_summary()`: row count, true positives, false positives,
`wouldOutbid`, victims landed, mean inclusion probability, reorg-aware.

Three things are missing, and they are the three that drive decisions:

| Missing | Why it matters |
| --- | --- |
| **Per-strategy breakdown** | the summary is a single all-time row, so it cannot tell you *which* of the ten strategies to keep |
| **A time window** | all-time totals blur a bad first week into a good third one; you need "last 7 days" to see a trend |
| **Expected value in ETH** | `SUM(sim_net_wei)` over true positives, weighted by `inclusion_p` — the number an executive actually asks for |

Add `GROUP BY strategy`, a block-range filter, and the weighted-EV column to
that one query, then surface it as the dashboard's top card. It is a SQL change
and a view over data you are already writing every block.

**A2. Run seven days on one good feed.** *(1 week, mostly waiting)*
`ETH_WS_URL` is empty by default and the bot falls back to HTTP head polling.
For a real measurement you want a websocket mempool feed plus the MEV-Share SSE
default that is already on. Do nothing else during this window; changing
strategy code mid-measurement invalidates the sample.

**A3. Fill in the W6 memo from real numbers.** *(half a day)*
It already computes decode pressure and empty pressure. That single memo
decides whether UniversalRouter / 1inch / 0x decoding is worth building, or
whether the public mempool is too thin to bother — which would redirect the
entire strategy roadmap. **Do not build decoders before this memo says to.**

**Exit criteria for Phase A:** you can state expected weekly capture, per
strategy, with a measured inclusion probability. If that number is
unattractive, you have saved yourself a live deployment; if it is attractive,
you have the mandate for Phase B and something concrete to show.

### Phase B — earn the right to submit (about 1 sprint)

**B1. Shadow submission.** *(3–4 days — do this before any real submission)*
Write the relay transport, and use it with `minProfit` set so high the bundle
cannot land. You exercise signing, relay auth, bundle shape, error handling and
rate limits against the real endpoint, with a profit guard that makes inclusion
arithmetically impossible. Every failure mode surfaces at zero risk.

This is the single highest-value engineering task in the repo, because it is
the only untested code that can cost money — and shadow mode removes the risk
from testing it.

**B2. Fix the private-orderflow gap.** *(2–3 days)*
`engine.rs:1213` drops any sandwich/JIT opportunity whose victim raw bytes are
unavailable, bumping `missing_victim_raw`. Private flow (MEV-Share hints, MEV
Blocker) never has raw bytes — by design. But MEV Blocker's protocol wants
`txs[0]` to be the victim's **hash**, not its bytes, and `send_bundle_params`
currently hex-encodes raw for every entry.

So there is a class of opportunity the bot ingests, decodes, and then discards
on a precondition that does not apply to that source. Check `missing_victim_raw`
in your Phase A data before sizing this — if it is large, this is free
opportunity you are already paying to find.

**B3. Multi-relay submission.** *(already on the roadmap)*
Only after B1 works against one relay.

### Phase C — go live small

Canary: one strategy — whichever Phase A ranked highest — with
`MAX_POSITION_WEI` at a fraction of the envelope, a hard daily loss cap, and
the kill switch wired to an alert you will actually see. Scale on evidence.

---

## What I would *not* do next

- **Do not add strategies or chains.** Ten strategies with unmeasured
  conversion is not better than three with known conversion. Phase 4 is a
  distraction until Phase A tells you which of the ten earn their keep.
- **Do not build router decoders yet.** That is W6, and it is explicitly gated
  on the memo. Building 1inch/0x decoding before knowing decode pressure is how
  you spend a sprint on flow that is already private.
- **Do not tune the simulator for speed yet.** `sim/anvil.rs:68` serialises
  every simulation behind one mutex and `REPLAY_LANES` defaults to 1, which
  *looks* like an obvious bottleneck. But the latency histograms will tell you
  whether simulation is actually your p95 problem. Optimise it if the data says
  so, not because it looks slow. (If it does: `REPLAY_LANES` is already the
  designed knob, and `eth_callBundle` is already a faster alternative backend.)

---

## The honest framing for the pressure

Two things are true at once, and saying both is stronger than picking one:

1. **The bot cannot lose money today**, because the submission path does not
   exist. Anyone worried about risk should be reassured.
2. **The bot cannot make money today, for the same reason.** Anyone impatient
   for revenue should understand that flipping `LIVE_EXECUTION=true` this week
   would produce exactly zero trades and zero information.

The fastest path to *either* goal runs through Phase A, because it is the only
work that tells you whether the remaining work is worth doing. A day or two to
the scoreboard, a week of data, and you will be arguing from evidence instead
of from instinct — including the argument about whether to go live at all.

If you need something visible sooner than that: the scoreboard card itself is
the demo. A dashboard showing "we would have outbid the winning builder on 34%
of the sandwiches we found this week" is a better artefact than a live bot with
an empty trade history.
