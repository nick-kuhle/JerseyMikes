# Local build & sandbox notes

## What CI verifies

The intended GitHub Actions pipeline is
[`ci/github-actions-ci.yml`](../ci/github-actions-ci.yml). It remains parked
outside `.github/workflows/` until a maintainer with GitHub `workflows`
permission enables it.

| Job | Commands |
| --- | --- |
| `contracts` | `forge fmt --check`, `forge build --sizes`, `forge test -vvv` (`forge fmt --check` is currently advisory) |
| `artifact-drift` | recompiles with solc-js and fails if `bot/crates/mev-bot/artifacts` or `contracts/abi` drifted from the sources |
| `bot` | `cargo fmt --check`, `cargo clippy --all-targets`, `cargo test --all` (`cargo fmt --check` is currently advisory) |
| `frontend` | `npm ci`, `tsc --noEmit`, `next build` |

## Verification status for the Phase 2 handoff

The maintainer reports the following local commands passing on 2026-08-21:

```bash
make bot-check
make bot-test
make contracts
```

The frontend checks were also run in the authoring sandbox:

```bash
cd frontend && npx tsc --noEmit && npm run build
```

The authoring sandbox itself still has no Rust or Foundry binaries, so it could
not independently reproduce the maintainer's Rust and Forge runs. Remote CI is
not yet available because the GitHub App push is rejected without the
`workflows` permission. Treat the local results as verification of the current
W1–W4 implementation, not as a substitute for a required green PR check.

The contracts-only fallback is independently reproducible here:

```bash
cd contracts && node script/compile-check.js
```

It compiled 28 sources with zero errors and confirmed the embedded
`MevExecutor` runtime at 9,618 bytes. Solc reports the existing transient-storage
and test-contract-size warnings; those are warnings, not compile failures.

## Independent re-verification + security hardening (automation session, 2026-08-21)

An automation session (this PR) re-ran the checks it can run — the sandbox still
has no Rust/Foundry binaries, so the bot and forge runs remain
maintainer-verified only — and recorded clean results:

```bash
cd contracts && node script/compile-check.js   # 28 sources, 5 deployables, MevExecutor runtime 9,618 B
git diff --exit-code -- bot/crates/mev-bot/artifacts contracts/abi   # artifact-drift step: no drift
cd frontend && npx tsc --noEmit && npm run build   # clean
```

Two findings came out of that pass, one of which is a code change:

- **Frontend dependency security bump.** `npm install` reported that
  `next@15.5.4` (App Router) is vulnerable to **CVE-2025-66478** — a CVSS 10.0
  remote-code-execution via the React Server Components protocol, with public
  PoCs. Bumped `next` to `15.5.7` and `react`/`react-dom` to `19.1.2` (the
  patched versions for this release line per the advisory). `tsc --noEmit` and
  `npm run build` are green after the bump. A long tail of lower-severity
  Next/PostCSS/sharp advisories remains (mostly DoS/SSRF in image-optimizer,
  middleware and server-action paths this dashboard does not use); clearing
  those needs `next@15.5.23`+ and was deliberately left out of this PR to keep
  the bump minimal and reviewable.
- **CI (W0) re-confirmed blocked.** The exact enable step
  (`git mv ci/github-actions-ci.yml .github/workflows/ci.yml`) was attempted and
  the push was rejected again with:
  `refusing to allow a GitHub App to create or update workflow
  '.github/workflows/ci.yml' without 'workflows' permission`. Remote CI still
  requires a human with GitHub `workflows` permission.

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
