# Risk & safety

## Why nothing can be broadcast

1. **No submission call site.** The engine records `BundleRecord`s and stops.
   `bundle::send_bundle_params` exists so the payload shape is exercised and
   testable, but no code path passes it to a transport.
2. **Two-key switch.** `Config::live_execution` is only true when
   `LIVE_EXECUTION=true` **and** `I_UNDERSTAND_LIVE_RISK=yes`. Neither is set by
   `.env.example`.
3. **Read-only relay calls.** The only relay method used is `eth_callBundle`,
   which simulates and returns; it never enqueues.
4. **Simulation happens on a local fork.** `anvil --fork-url` is a separate
   process bound to `127.0.0.1`; every simulation RPC goes there, not to
   mainnet.

## Why a failed opportunity costs nothing

`MevExecutor` measures the balance of the profit token before and after the
batch and reverts with `Unprofitable(realised, required)` if the delta is below
`minProfit`. Bundles are submitted through private orderflow, so a bundle whose
transactions revert is **dropped by the builder and never included** — no block
space, no gas. The gas-burn risk that exists for public-mempool bots does not
exist here.

Additional on-chain guards, all optional per bundle:

| Guard | Effect |
| --- | --- |
| `minProfit` | reverts unless the realised delta clears it |
| `blockDeadline` | reverts if the bundle slips to a later block |
| `maxBaseFee` | reverts if base fee spiked since we sized the trade |
| `bribeBps` | pays the builder a share **of realised profit**, so a losing bundle pays nothing |
| `searchers` allowlist | only approved addresses can call `execute` |
| transient-storage guards | reentrancy, flash-loan callback, V3 mint callback |

## Off-chain risk parameters

Set in the environment; see `.env.example`.

| Variable | Default | Meaning |
| --- | --- | --- |
| `MIN_NET_PROFIT_WEI` | `1` | Record anything that is not a loss |
| `MAX_POSITION_WEI` | `100 ETH` | Cap on notional per bundle |
| `MAX_BASE_FEE_WEI` | `500 gwei` | Refuse to play in a gas spike |
| `BRIBE_BPS` | `9000` | 90% of gross to the builder |
| `MAX_GAS_PER_BUNDLE` | `3,000,000` | Bundle gas ceiling |
| `MAX_DRAWDOWN_WEI` | `0` (off) | Cumulative simulated loss that trips the kill switch |
| `MAX_INFLIGHT_PER_STRATEGY` | `32` | Concurrent simulations per strategy |

These start **deliberately liberal**. The first run's job is to measure how much
MEV is reachable and where the losses come from, not to be profitable. Suggested
tightening order once there is data:

1. Raise `MIN_NET_PROFIT_WEI` above the observed noise floor (start around
   0.002 ETH — roughly two blocks of failed-inclusion opportunity cost).
2. Lower `MAX_POSITION_WEI` per strategy to the size where the realised-vs-
   predicted profit error stops growing.
3. Turn on `MAX_DRAWDOWN_WEI`.
4. Drop `BRIBE_BPS` and watch inclusion rate — this is the parameter with the
   most money in it.

## Known limitations

- **Sizing assumes our bundle is the whole block.** Competing searchers'
  bundles, and any transaction between ours and the victim's, are not modelled.
  Real-world profit will be lower than simulated profit.
- **Victim replay needs raw bytes.** If the RPC does not serve
  `eth_getRawTransactionByHash`, sandwich and JIT opportunities are skipped
  rather than guessed at.
- **MEV-Share hints are usually redacted.** Most private orderflow yields only
  a function selector and log topics; strategies that need calldata cannot act
  on those.
- **Liquidation sizing assumes a USDC debt leg and a 5% bonus.** Correct
  per-reserve configuration lookup is a to-do.
- **No re-org handling.** A re-orged block's simulations stay in the database.

## Operational notes

- The database is append-only; delete `data/mev.sqlite` to reset P/L.
- `mev-bot doctor` verifies every endpoint before a run.
- The kill switch is per-process; restarting clears it unless the drawdown is
  recomputed from the database.
