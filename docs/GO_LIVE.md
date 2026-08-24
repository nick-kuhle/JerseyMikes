# Going live, step by step (deploying MevExecutor to Ethereum mainnet)

> Coming from a fresh server? Start with
> [`DAY0_RUNBOOK.md`](DAY0_RUNBOOK.md) — the full Phase 0→4 path from bare
> host to the Day-7 money switch. This page covers just the contract
> deployment leg in depth.

Written for a first-time operator. It answers the two questions people ask
first — *do I deploy before arming the bot?* (yes — deploy is step 1, arming
is the very last thing and a separate decision) and *what do I need?*
(a funded wallet and about ten minutes). Nothing here is scary, but some of it
is irreversible, so read the whole page once before spending anything.

```text
your machine                       ethereum mainnet
────────────                       ───────────────
mev-bot (Rust)  ── simulates ──▶   MevExecutor  ◀── may hold ETH used as call value
   │  ▲                             owner = your wallet
   │  └── reads/writes via RPC      searchers = the bot's signer EOA
   └── broadcasts only after every independent go-live gate passes
```

> Already deployed, and just switching the bot over from simulation?
> See [`SIM_TO_LIVE.md`](SIM_TO_LIVE.md).

## What you are deploying

`MevExecutor` is the bot's on-chain hand: strategies are encoded off-chain as
an ordered list of calls, and the executor runs them atomically with a hard
profit guard — if the batch does not come out ahead by at least
`minProfit`, the whole transaction reverts. A reverting bundle sent through
private orderflow is simply dropped by the builder, which is why liberal
searching is safe to practise with.

This revision is **not ABI-compatible with older executor deployments**. The
current ABI uses `execute(bytes32,Call[],Guard)` and
`flashExecute(bytes32,address[],uint256[],Call[],Guard)`, with phase `0` for a
single-shot batch, `1` for an opening baseline, and `2` for same-block
settlement. `minProfit` is retained profit after builder payment. Redeploy from
this revision and update `EXECUTOR_ADDRESS`; do not reuse an older deployment.

Two facts that answer most confusion:

- **You do not need to deploy anything to simulate.** The simulator injects
  the compiled bytecode into its own local fork. A deployment only matters
  for the live path.
- **Deploying does not make the bot trade.** Relay submission additionally
  requires the funded transaction key, boot arming, `BROADCAST_ENABLED`,
  runtime live mode, risk and inventory approval, and a strategy-specific
  qualification `PASS`. All default values remain fail-closed.

## Prerequisites

| Need | What | Why |
| --- | --- | --- |
| A wallet | MetaMask / Rabby / anything EIP-1193 — the console's **connect wallet** button works with all of them | pays the deployment gas and becomes the contract owner |
| ETH for gas | ≥ 0.05 ETH recommended (deployment is typically ~0.002–0.01 ETH at normal base fees; the rest covers a few admin transactions and headroom) | mainnet gas |
| The chain | your wallet on Ethereum mainnet (chain 1) | the console's checklist detects and offers to switch |
| An RPC | set as `ETH_HTTP_URL` (Ethereum) or `BASE_HTTP_URL` (Base) — the console's reads ride the same one via its server-side proxy | contract reads / receipts |
| Optional: Etherscan API key | only for the CLI path's `--verify` | source verification |

## Path A — production wizard (easiest)

Open the console and scroll to **"Production Go-Live Wizard"**. The wizard is
five cards, in order: network and wallet, EOA/key separation, deployment of
both `MevExecutor` and `SniperVault`, funding/budget setup, then pre-flight and
independent live switches. A deployment never arms either lane.

The wizard also exposes a dynamic operator soak threshold. Set it to the
window you are actually willing to accept (for example 1, 24, or 168 hours).
The change is sent to the bot's authenticated control plane and re-runs the
normal evidence evaluation; it does not bypass continuity, sample, accuracy,
or independent-comparison gates. The atomic engine and directional sniper have
separate runtime switches, so the sniper may be enabled while atomic MEV stays
in simulation.

Base uses the same wizard with the Base WETH binding and the Base bot selected
by `CHAINS`. Its pre-flight accepts a raw/chain-block path rather than requiring
an Ethereum relay market. The canonical stream URLs are
`/api/stream?chain=ethereum` and `/api/stream?chain=base`.

The wizard deploys the exact creation bytecode for both contracts, and its
buttons are disabled until the wallet is on the selected chain and has gas.
`MevExecutor` uses the chain's Balancer V2 vault and WETH. `SniperVault` uses
that chain's WETH plus the daily and lifetime budget values entered in the
card. The owner wallet must explicitly allowlist the atomic and dedicated
sniper searcher addresses; the wizard verifies both mappings before reporting
success.

1. **Connect a wallet on the selected chain.** This wallet is the owner for
   browser deployments and pays gas.
2. **Verify the EOA separation.** The panel shows the atomic searcher and
   `SNIPER_SEARCHER_ADDRESS`; it never reads or displays either private key.
3. **Deploy or paste both contracts.** The UI waits for receipts and verifies
   bytecode, owner, WETH, and searcher allowlisting. The CLI alternatives are
   `script/Deploy.s.sol` and `script/DeploySniperVault.s.sol`.
4. **Set the vault budget and fund it with WETH.** The panel wraps ETH and
   transfers WETH to the selected vault or executor. `SniperVault` enforces
   its daily/total entry ceilings on-chain; drawdown remains a separate
   off-chain sniper control.
5. **Run pre-flight and make an explicit runtime choice.** RPC, chain feed,
   relay/raw path, and qualification state are shown. The atomic and sniper
   buttons are independent and require confirmation. Boot capability still
   comes only from the environment and a restart.
6. **Persist the generated lines.** Copy `EXECUTOR_ADDRESS`,
   `SEARCHER_ADDRESS`, `SNIPER_VAULT_ADDRESS`, and
   `SNIPER_SEARCHER_ADDRESS` into the appropriate chain env file before a
   restart. Runtime patches are not a replacement for durable configuration.

Costs you should expect on path A: one deployment (~2–3M gas), an optional funding
transfer (21k gas + the amount sent), one `setSearcher` (~50k gas). At 1–2
gwei base fee that is a few thousandths of an ETH total.

## Path B — Foundry CLI (scripted, verifiable on Etherscan)

For repeatability, or if you prefer not to load a hot wallet in a browser:

```bash
cd contracts
cp .env.example .env   # or edit your bot .env — same variables

# dry run against a fork first — costs nothing, no key needed:
forge script script/Deploy.s.sol --fork-url $ETH_HTTP_URL

# real deployment (uses DEPLOYER_PRIVATE_KEY from .env):
forge script script/Deploy.s.sol --rpc-url $ETH_HTTP_URL --broadcast --verify

# isolated directional vault; on Base use $BASE_HTTP_URL
forge script script/DeploySniperVault.s.sol --rpc-url $ETH_HTTP_URL --broadcast
```

`Deploy.s.sol` selects Base WETH automatically when the connected chain is
8453 (an explicit `WETH_ADDRESS` still wins), and — if `SEARCHER_ADDRESS` is
set — allowlists the atomic searcher in the same transaction batch.
`DeploySniperVault.s.sol` selects the chain WETH, uses the
`SNIPER_DAILY_BUDGET_WEI` / `SNIPER_TOTAL_BUDGET_WEI` values, and allowlists
`SNIPER_SEARCHER_ADDRESS` when supplied. With `--verify` and
`ETHERSCAN_API_KEY` set, the executor source is verified on Etherscan so anyone
can read it. Keep `DEPLOYER_PRIVATE_KEY` in the `.env` file only long enough to
deploy, then clear it.

## After deploying — verify before touching anything else

- **Etherscan:** the contract shows as a `MevExecutor` creation; source verified
  on path B.
- **Console → MevExecutor — on-chain control:** paste the new address and press
  **read** — `owner` is your wallet, the balance is what you funded, and
  **you are searcher** answers yes for the deployer.
- If you used the checklist, the panel already switched its target to the
  deployed address.

## What "arming" actually means (and why it comes last)

The bot's execution mode has three operator-controlled layers — see
[`RISK.md`](RISK.md):

1. **Broadcast capability** — `BROADCAST_ENABLED=true` permits relay transport.
2. **Arming at boot** — `LIVE_EXECUTION=true` and
   `I_UNDERSTAND_LIVE_RISK=yes`, then a restart. The console can never set it.
3. **The runtime switch** — on an armed process, the authenticated
   `SIMULATION ⇄ LIVE` control pauses/resumes live mode without a restart.

These do not override the per-strategy qualification, risk, inventory, nonce,
or exact-payload simulation gates.

The order that keeps you safe: deploy → fund → allowlist the address derived
from `SEARCHER_PRIVATE_KEY` → set `EXECUTOR_ADDRESS` → restart → collect at
least seven continuously observed days → require an explicit per-strategy
`PASS` → tighten risk → enable broadcast capability → arm. An unarmed process
cannot be flipped live through the UI.

**Live submission exists in this build.** `eth_sendBundle` is invoked across
configured relays when every gate passes. Treat arming as a money switch, and
follow the full evidence, nonce-recovery, finality, and rollback procedure in
[`SIM_TO_LIVE.md`](SIM_TO_LIVE.md).

> Before you arm, secure the bot's API. A network-reachable
> `API_BIND` with no `API_AUTH_TOKEN` lets anyone who can reach the port
> retune your risk envelope — including setting `bribeBps` to 100%. The bot
> refuses to start in that configuration; the fix and the reasoning are in
> [`SIM_TO_LIVE.md`](SIM_TO_LIVE.md).

## Safety notes

- **The owner key is the master key.** Whoever holds the deployer wallet can
  `setOwner`, allowlist searchers, and sweep everything out. Consider a
  dedicated deployment wallet rather than your main holdings.
- **The searcher allowlist is execution-critical.** A mismatch between
  `SEARCHER_ADDRESS` in the bot and the contract's `searchers` means live
  bundles would revert at the allowlist check.
- **A wrong constructor (vault/WETH) means broken flash loans** — leave the
  mainnet defaults unless you know why not to.
- **Deployment is permanent** (no proxy, no upgrade path — that is
  deliberate: less surface). If you mistype something, deploy again and
  treat the first contract as burnt gas; sweep its funds out first if you
  funded it.
- **Never put `DEPLOYER_PRIVATE_KEY` or bot keys in the frontend env**
  (`NEXT_PUBLIC_*` is visible to every browser).

## FAQ

**Do I need to deploy before arming?** Yes — and arming is itself still a
later decision (see above). Deployed executor first, configured bot second,
arming last.

**Can I run several executors?** Yes, deployments are cheap and stateless
apart from the allowlist and balance; one per experiment is a fine pattern.

**Which wallet should deploy?** One you keep for operations. Ownership can
be moved later with `setOwner`, but simplest is to start with the right
wallet.

**Does the console's wallet need to be the bot's searcher?** No. The
console wallet is the owner/operator; the searcher is the EOA the bot
signs bundles from (its `SEARCHER_ADDRESS`). They can be — and usually
should be — different.
