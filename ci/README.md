# CI

`github-actions-ci.yml` is the CI pipeline for this repo. It lives here instead
of `.github/workflows/` because the automation that opened this PR does not hold
the GitHub `workflows` permission and pushes containing workflow files are
rejected.

To enable it (one command, from the repo root):

```bash
mkdir -p .github/workflows && git mv ci/github-actions-ci.yml .github/workflows/ci.yml
git commit -m "ci: enable GitHub Actions" && git push
```

## What it runs

| Job | Commands |
| --- | --- |
| `contracts` | `forge build --sizes`, `forge test -vvv` (`forge fmt --check` advisory) |
| `artifact-drift` | recompiles with solc-js and fails if `bot/crates/mev-bot/artifacts` or `contracts/abi` drifted from the sources |
| `bot` | `cargo clippy --all-targets`, `cargo test --all` (`cargo fmt --check` advisory) |
| `frontend` | `tsc --noEmit`, `next build` |
