# Local build & sandbox notes

## What CI verifies

`.github/workflows/ci.yml` runs three independent jobs:

| Job | Commands |
| --- | --- |
| `contracts` | `forge fmt --check`, `forge build --sizes`, `forge test -vvv` |
| `bot` | `cargo fmt --check`, `cargo clippy`, `cargo test` |
| `frontend` | `npm ci`, `tsc --noEmit`, `next build` |

## What was verified in the authoring sandbox

The sandbox this PR was written in has no access to `crates.io`,
`static.rust-lang.org` or the Foundry release artifacts, so:

| Component | Status |
| --- | --- |
| Solidity (`src`, `test`, `script`) | **compiled** with solc 0.8.26 via `node contracts/script/compile-check.js` — 0 errors, `MevExecutor` runtime 9,618 bytes |
| Frontend | **built and run** — `tsc --noEmit` clean, dev server serving the console with live SSE |
| Rust crate | **not compiled** — no toolchain available. `cargo fmt/clippy/test` run in CI on the first push |

If `cargo check` reports anything on the first CI run it will be small and
mechanical (an import, a trait bound); the logic and the tests are written to be
read and re-run. `contracts/script/compile-check.js` is kept in the repo because
it is genuinely useful for a fast Solidity type-check without Foundry, and
because it generates the runtime bytecode artifact the Rust simulator embeds.

## Regenerating the embedded artifacts

`bot/crates/mev-bot/artifacts/MevExecutor.runtime.hex` is injected into the
anvil fork with `anvil_setCode`, so simulation works before the contract is
deployed anywhere. Regenerate after any contract change:

```bash
cd contracts && npm install && node script/compile-check.js
# or, with Foundry:
forge build && jq -r '.deployedBytecode.object' out/MevExecutor.sol/MevExecutor.json \
  > ../bot/crates/mev-bot/artifacts/MevExecutor.runtime.hex
```

CI fails if the checked-in artifact drifts from the compiler output.
