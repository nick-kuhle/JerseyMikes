# Setup

End-to-end walkthrough for getting the JerseyMikes pipeline running locally:
the **Rust** searcher/bot, the **Foundry** contracts (and the `anvil` simulation
engine), and the **Node/Next.js** console. Every command below is also wrapped by
a `make` target — see the [Quick reference](#quick-reference) at the bottom.

> First run, or something is broken? Jump to
> [`make doctor`](#sanity-check-make-doctor) and then
> [Troubleshooting](#troubleshooting). `make doctor` prints a ✓ / · / ! / ✗ for
> every dependency so you can see exactly what is still missing.

---

## Prerequisites

| Tool | Version | Why you need it | Install |
| --- | --- | --- | --- |
| **Rust** (rustc + cargo) | **1.79+** | The searcher, simulator and API (`bot/`) | [rustup](https://rustup.rs) |
| **Foundry** (`forge`, `cast`, `anvil`) | latest | Contracts build/tests, **`anvil` is the simulation engine** | [getfoundry.sh](https://getfoundry.sh) |
| **Node.js** (+ npm) | **20+** | The console (`frontend/`); also the no-Foundry compile fallback | [nodejs.org](https://nodejs.org) / [nvm](https://github.com/nvm-sh/nvm) |
| **Git** | any recent | Clone with submodules | system package |
| **An Ethereum RPC** | archive-capable, mainnet | Live state + historical `eth_call`; see [Configure `.env`](#configure-env) | Alchemy / QuickNode / Erigon / Reth |

Each row is expanded in [Install the toolchains](#install-the-toolchains).

---

## Install the toolchains

You need all three (Rust, Foundry, Node). The fastest path is to install them,
then run [`make setup`](#clone--first-time-setup), which pulls submodules and
dependencies for you.

### Rust (the bot)

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"      # or open a new shell
rustc --version                # expect 1.79.0 or newer
cargo --version
```

The crate targets `rust-version = "1.79"` and `edition = "2021"`
(see `bot/Cargo.toml`); older toolchains will refuse to build.

### Foundry (contracts + `anvil`)

```bash
curl -L https://foundry.paradigm.dev | bash
foundryup                       # installs forge, cast, anvil, chisel
forge --version
anvil --version                 # the simulation engine — must be runnable
```

`anvil` is what replays each candidate bundle `front → victim → back` inside a
forked mainnet. The bot finds it via the `ANVIL_BIN` env var (default `anvil`),
so if you install it under a non-standard name or path, point `ANVIL_BIN` at it.

### Node.js (the console)

```bash
# with nvm:
nvm install 20 && nvm use 20
# or use the official installer / your system package manager
node --version                 # expect v20.x or newer
npm --version
```

Node also powers the no-Foundry fallback for the contracts: if you cannot install
Foundry, `contracts/script/compile-check.js` does a solc-only type check
(`solc` is pulled in as an npm dependency of `contracts/`).

---

## Clone & first-time setup

Always clone with submodules — `contracts/lib/forge-std` is a submodule and
`forge build` will fail without it:

```bash
git clone --recurse-submodules <this repo> && cd JerseyMikes
# already cloned without submodules? recover with:
git submodule update --init --recursive
```

Then let the Makefile finish the install:

```bash
make setup    # submodules + contracts npm deps + frontend npm deps + copy .env
```

`make setup` runs `git submodule update --init --recursive`, installs npm
dependencies in both `contracts/` and `frontend/`, and copies `.env.example` to
`.env` (only if `.env` does not already exist).

## Configure `.env`

`make setup` creates `.env` from `.env.example`. Open it and set the two
required variables:

```ini
ETH_HTTP_URL=https://eth-mainnet.g.alchemy.com/v2/<key>   # archive-capable RPC
ETH_WS_URL=wss://eth-mainnet.g.alchemy.com/v2/<key>       # newHeads + pending txs
```

- **`ETH_HTTP_URL`** (required) — must support `eth_call` at historical blocks
  and ideally `eth_getRawTransactionByHash` (needed to faithfully replay
  sandwich/JIT victims).
- **`ETH_WS_URL`** (required for real opportunity flow) — without it the bot
  falls back to HTTP head polling and **sees no mempool**, so no opportunities
  appear. This is the #1 cause of "nothing is happening".
- Everything else is optional and has safe defaults; see the annotated
  [`.env.example`](../.env.example).

---

## Sanity check: `make doctor`

Before a real run, verify every endpoint and binary answers:

```bash
make doctor
```

It builds the bot (if needed) and runs `mev-bot doctor`, which prints one line
per check using three markers:

| Marker | Meaning |
| --- | --- |
| `✓` | passing / present |
| `·` | informational (configured but not strictly required) |
| `!` | warning — degraded mode (bot runs but loses capability) |
| `✗` | hard failure — this dependency is broken/missing |

Checks, in order:

1. **http rpc** — `eth_blockNumber` on `ETH_HTTP_URL` (`✓` head block / `✗`)
2. **chain id** — `eth_chainId` (`✓` / `✗`)
3. **raw tx access** — `eth_getRawTransactionByHash` support
   (`✓`, or `!` → sandwich/JIT sims will be skipped)
4. **websocket** — whether `ETH_WS_URL` is set
   (`·` configured / `!` not set → no mempool)
5. **flashbots key** — `FLASHBOTS_SIGNER_KEY`
   (`·` configured / `!` not set → relay cross-checks use an ephemeral key)
6. **anvil** — `ANVIL_BIN --version` runs (`✓` / `✗`)
7. **relay data** — each URL in `RELAY_DATA_URLS` answers
   (`·` status / `!` error), one line per relay

…and finishes with the run mode (`simulation` or `LIVE`). Aim for **no `✗` and
as few `!` as possible** before starting a run. A missing `ETH_WS_URL` shows up
as a `!` on line 4 — fix that first or nothing will happen.

---

## Build & run

Each layer independently:

```bash
make contracts       # forge build --sizes + forge test -vvv
make bot-build       # cargo build --release
make front-build     # next build
```

Then run the stack:

```bash
make bot-run         # searcher + API on :8080 (simulation mode by default)
make front-dev        # console on :3000
```

The dashboard renders against `/api/bot/*` (proxied to `BOT_API_URL`). If the
bot API is unreachable it shows generated data behind a **DEMO DATA** badge, so
the console works before the bot does.

---

## Troubleshooting

### `forge: not found`

Foundry isn't installed or isn't on your `PATH`.

```bash
curl -L https://foundry.paradigm.dev | bash
foundryup
# open a new shell, or: source ~/.foundry/bin/foundryup && export PATH="$HOME/.foundry/bin:$PATH"
forge --version
```

If `anvil` is present but under a different name/path, set `ANVIL_BIN` in `.env`.

### `cargo: not found`

Rust isn't installed or the toolchain is too old (needs 1.79+).

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
rustc --version          # must be ≥ 1.79.0
```

### Submodule / `forge build` error (`File "lib/forge-std/src/..." not found`)

You cloned without `--recurse-submodules`, so `contracts/lib/forge-std` is
empty. Fix it with:

```bash
git submodule update --init --recursive
```

(`make setup` does this for you on a fresh clone.) Verify
`contracts/lib/forge-std/src/` is now populated.

### Missing `.env` / `Config::from_env` error at startup

The bot loads `.env` via the `--env-file` flag (`make` passes
`--env-file ../.env`). If it is absent the run aborts on the missing
`ETH_HTTP_URL`. Create it:

```bash
cp .env.example .env
$EDITOR .env           # set ETH_HTTP_URL and ETH_WS_URL
```

### No opportunities appearing

Almost always a **missing `ETH_WS_URL`**. The bot reads the mempool from the
websocket (`newPendingTransactions`); with only an HTTP URL it falls back to
head polling and sees no pending transactions, so sandwich/JIT/arb never fire.

```bash
make doctor            # look for "! websocket  not set" on line 4
```

Set `ETH_WS_URL` in `.env` and re-run. Other things to check: confirm the RPC
is mainnet (`chain id` should be `1`), that raw-tx access is supported
(otherwise sandwich/JIT sims are skipped), and that the forked `anvil` started
(`make doctor` line 6).

---

## Quick reference

| What | Command |
| --- | --- |
| Full one-time setup | `make setup` |
| Connectivity pre-flight | `make doctor` |
| Build everything | `make build` |
| Contracts build + test | `make contracts` |
| Bot build / test | `make bot-build` / `make bot-test` |
| Run searcher + API (`:8080`) | `make bot-run` |
| Replay stored sims vs relay traces | `make replay` |
| Run console (`:3000`) | `make front-dev` |
| Simulate a deployment against a fork | `make deploy-dry` |
| No-Foundry contracts check | `make contracts-check` |
| Format everything | `make fmt` |
