Going live, step by step (deploying MevExecutor to Ethereum mainnet)
Written for a first-time operator. It answers the two questions people ask
first — do I deploy before arming the bot? (yes — deploy is step 1, arming
is the very last thing and a separate decision) and what do I need?
(a funded wallet and about ten minutes). Nothing here is scary, but some of it
is irreversible, so read the whole page once before spending anything.

text

your machine                       ethereum mainnet
────────────                       ───────────────
mev-bot (Rust)  ── simulates ──▶   MevExecutor  ◀── holds a little ETH for gas
   │  ▲                             owner = your wallet
   │  └── reads/writes via RPC      searchers = the bot's signer EOA
   └── until Phase 3: never broadcasts anything
What you are deploying
MevExecutor is the bot's on-chain hand: strategies are encoded off-chain as
an ordered list of calls, and the executor runs them atomically with a hard
profit guard — if the batch does not come out ahead by at least
minProfit, the whole transaction reverts. A reverting bundle sent through
private orderflow is simply dropped by the builder, which is why liberal
searching is safe to practise with.

Two facts that answer most confusion:

You do not need to deploy anything to simulate. The simulator injects
the compiled bytecode into its own local fork. A deployment only matters
for the live path.
Deploying does not make the bot trade. The bot broadcasts nothing until
it is restarted with both live-execution keys set (and even then, this
build records bundles rather than sending them — actual submission is
Phase 3 of ROADMAP.md).
Prerequisites
Need	What	Why
A wallet	MetaMask / Rabby / anything EIP-1193 — the console's connect wallet button works with all of them	pays the deployment gas and becomes the contract owner
ETH for gas	≥ 0.05 ETH recommended (deployment is typically ~0.002–0.01 ETH at normal base fees; the rest covers a few admin transactions and headroom)	mainnet gas
The chain	your wallet on Ethereum mainnet (chain 1)	the console's checklist detects and offers to switch
An RPC	already set in the bot's .env (ETH_HTTP_URL) — the console's reads ride the same one via its server-side proxy	contract reads / receipts
Optional: Etherscan API key	only for the CLI path's --verify	source verification
Path A — the console's checklist (easiest)
Open the console and scroll to "Go live — deploy MevExecutor to mainnet".
The six steps are the rest of this section; each step is disabled until the
ones before it make sense. (The panel deploys the exact creation bytecode
the bot simulates against — a CI-checked copy of the bot artifact — with the
mainnet Balancer V2 vault and WETH9 constructor arguments prefilled, and
prefills the bot's SEARCHER_ADDRESS for the allowlist step.)

Connect a wallet on chain 1. If your wallet sits on another network the
panel offers a one-click switch. This wallet becomes the executor's owner.
Confirm it holds gas money. The panel shows the balance; ≥ 0.05 ETH is
the recommended buffer.
Deploy. Press estimate cost first (a free eth_estimateGas call —
no wallet popup), then deploy and confirm in your wallet. The panel
waits for the receipt, shows the new address with an Etherscan link, and
remembers it. The constructor is MevExecutor(balancerVault, weth) — the
prefilled values are mainnet's Balancer V2 vault and WETH9; leave them.
Your deployer wallet is now owner and an allowed searcher.
Fund the executor. Send 0.02–0.05 ETH to the new address. This is the
gas the executor will spend when it eventually executes bundles. It is not
trading capital — swap capital comes from zero-fee Balancer flash loans.
The owner can always sweep it back.
Allowlist the bot's searcher. The field is prefilled from the bot's
SEARCHER_ADDRESS (or enter it manually) and calls setSearcher(addr, true). The executor only accepts bundles from allowlisted addresses, so
this must match the EOA the bot signs from.
Point the bot at the executor and restart it. Copy the generated env
lines (EXECUTOR_ADDRESS=0x…, and SEARCHER_ADDRESS if you changed it),
add them to the bot's .env, restart. The console's MevExecutor —
on-chain control panel now reads your deployed contract, and simulations
use it instead of the injected placeholder.
Costs you should expect on path A: one deployment (~2–3M gas), one funding
transfer (21k gas + the amount sent), one setSearcher (~50k gas). At 1–2
gwei base fee that is a few thousandths of an ETH total.

Path B — Foundry CLI (scripted, verifiable on Etherscan)
For repeatability, or if you prefer not to load a hot wallet in a browser:

Bash

cd contracts
cp .env.example .env   # or edit your bot .env — same variables

# dry run against a fork first — costs nothing, no key needed:
forge script script/Deploy.s.sol --fork-url $ETH_HTTP_URL

# real deployment (uses DEPLOYER_PRIVATE_KEY from .env):
forge script script/Deploy.s.sol --rpc-url $ETH_HTTP_URL --broadcast --verify
Deploy.s.sol deploys with mainnet's vault/WETH defaults, and — if
SEARCHER_ADDRESS is set — allowlists the bot's searcher in the same
transaction batch (saving you step 5 of path A). With --verify and
ETHERSCAN_API_KEY set, the source is verified on Etherscan so anyone can
read it. Keep DEPLOYER_PRIVATE_KEY in the .env file only long enough to
deploy, then clear it.

After deploying — verify before touching anything else
Etherscan: the contract shows as a MevExecutor creation; source verified
on path B.
Console → MevExecutor — on-chain control: paste the new address and press
read — owner is your wallet, the balance is what you funded, and
you are searcher answers yes for the deployer.
If you used the checklist, the panel already switched its target to the
deployed address.
What "arming" actually means (and why it comes last)
The bot's execution mode has two layers — see docs/RISK.md:

Arming at boot — LIVE_EXECUTION=true and
I_UNDERSTAND_LIVE_RISK=yes in the bot's .env, then a restart. This is
the deliberate two-key switch; the console's mode toggle can never set it.
The runtime switch — once a bot is armed, the console's
SIMULATION ⇄ LIVE badge pauses/resumes live mode without a restart.
The order that keeps you safe: deploy → fund → allowlist searcher →
EXECUTOR_ADDRESS → restart → watch it in simulation for days — and treat
arming as its own decision, made only when the funnel says the bot is finding
real, profitable bundles. An unarmed bot cannot be flipped live by anyone
through the UI.

Honest reminder: in the current build, even an armed bot only marks
profitable bundles as submitted — the eth_sendBundle transport is Phase 3
and does not exist yet. Arming today is preparation, not a money switch.

Safety notes
The owner key is the master key. Whoever holds the deployer wallet can
setOwner, allowlist searchers, and sweep everything out. Consider a
dedicated deployment wallet rather than your main holdings.
The searcher allowlist is execution-critical. A mismatch between
SEARCHER_ADDRESS in the bot and the contract's searchers means live
bundles would revert at the allowlist check.
A wrong constructor (vault/WETH) means broken flash loans — leave the
mainnet defaults unless you know why not to.
Deployment is permanent (no proxy, no upgrade path — that is
deliberate: less surface). If you mistype something, deploy again and
treat the first contract as burnt gas; sweep its funds out first if you
funded it.
Never put DEPLOYER_PRIVATE_KEY or bot keys in the frontend env
(NEXT_PUBLIC_* is visible to every browser).
FAQ
Do I need to deploy before arming? Yes — and arming is itself still a
later decision (see above). Deployed executor first, configured bot second,
arming last.
Can I run several executors? Yes, deployments are cheap and stateless
apart from the allowlist and balance; one per experiment is a fine pattern.
Which wallet should deploy? One you keep for operations. Ownership can
be moved later with setOwner, but simplest is to start with the right
wallet.
Does the console's wallet need to be the bot's searcher? No. The
console wallet is the owner/operator; the searcher is the EOA the bot
signs bundles from (its SEARCHER_ADDRESS). They can be — and usually
should be — different.
