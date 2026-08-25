.DEFAULT_GOAL := help
SHELL := /bin/bash

# One process per chain; each has its own untracked runtime env file.
ENV_MAINNET ?= .env
ENV_BASE ?= .env.base

help: ## show this help
	@grep -hE '^[a-zA-Z_-]+:.*?## ' $(MAKEFILE_LIST) | awk 'BEGIN{FS=":.*?## "}{printf "  \033[36m%-18s\033[0m %s\n", $$1, $$2}'

setup: ## first-run bootstrap: tools, submodules, deps, both env files, bot/data/
	@missing=""; \
	for tool in cargo forge anvil node npm; do \
		command -v $$tool >/dev/null 2>&1 || missing="$$missing $$tool"; \
	done; \
	if [ -n "$$missing" ]; then \
		echo "✗ missing required tools:$$missing"; \
		echo "  install Rust (rustup.rs), Foundry (getfoundry.sh), Node 22+ (nodejs.org) and re-run"; \
		exit 1; \
	fi; \
	echo "✓ toolchain: $$(rustc --version), $$(forge --version | head -1), node $$(node --version)"
	git submodule update --init --recursive
	cd contracts && npm install --no-audit --no-fund
	cd frontend && npm install --no-audit --no-fund
	@if [ -f $(ENV_MAINNET) ]; then \
		echo "· $(ENV_MAINNET) exists — left untouched"; \
	else \
		install -m 0600 .env.example $(ENV_MAINNET); \
		echo "✓ created $(ENV_MAINNET) from .env.example (mode 0600, gitignored)"; \
	fi
	@if [ -f $(ENV_BASE) ]; then \
		echo "· $(ENV_BASE) exists — left untouched"; \
	else \
		install -m 0600 .env.example.base $(ENV_BASE); \
		echo "✓ created $(ENV_BASE) from .env.example.base (mode 0600, gitignored)"; \
	fi
	@install -d -m 0700 bot/data
	@echo "✓ bot/data/ ready (mode 0700, local SQLite state)"
	@echo ""
	@echo "→ next required action: set ETH_HTTP_URL in $(ENV_MAINNET) (Ethereum RPC) and"
	@echo "  ETH_HTTP_URL in $(ENV_BASE) (Base archive RPC), then run: make doctor"

build: contracts bot-build front-build ## build everything

contracts: ## forge build + tests
	cd contracts && forge build --sizes && forge test -vvv

contracts-check: ## solc-only compile check (no foundry needed); regenerates artifacts
	cd contracts && node script/compile-check.js

bot-build: ## cargo build --release
	cd bot && cargo build --release

bot-check: ## fast Rust type-check (all targets)
	cd bot && cargo check --all-targets

bot-test: ## cargo test
	cd bot && cargo test --all

# ── dual-chain operation ─────────────────────────────────────────────────────
# The dashboard with only one reachable bot is indistinguishable from a Base
# demo fallback, so `doctor` and `bot-run` deliberately operate on BOTH chain
# profiles at once and fail until both are configured.

doctor: doctor-mainnet doctor-base ## preflight BOTH profiles (.env + .env.base)

doctor-mainnet: ## doctor the Ethereum profile from $(ENV_MAINNET)
	@test -f $(ENV_MAINNET) || { echo "✗ $(ENV_MAINNET) missing — run make setup"; exit 1; }
	@echo "── ethereum ($(ENV_MAINNET)) ──────────────────────────────────────────"
	cd bot && cargo run --release --bin mev-bot -- --env-file ../$(ENV_MAINNET) doctor

doctor-base: ## doctor the Base profile from $(ENV_BASE) — must report chain 0x2105
	@test -f $(ENV_BASE) || { echo "✗ $(ENV_BASE) missing — run make setup"; exit 1; }
	@echo "── base ($(ENV_BASE)) ─────────────────────────────────────────────────"
	@cd bot && output="$$(cargo run --release --bin mev-bot -- --env-file ../$(ENV_BASE) doctor)"; \
	status=$$?; \
	printf '%s\n' "$$output"; \
	if [ $$status -ne 0 ]; then \
		echo "✗ Base doctor failed — a wrong-chain RPC is a stop condition"; \
		exit $$status; \
	fi; \
	if ! printf '%s' "$$output" | grep -q "0x2105"; then \
		echo "✗ Base doctor did not report chain ID 0x2105 (8453) — stop"; \
		exit 1; \
	fi; \
	echo "✓ Base profile reports chain ID 0x2105 (8453)"

bot-run: ## supervise BOTH isolated processes; either exiting stops both (fails)
	@test -f $(ENV_MAINNET) || { echo "✗ $(ENV_MAINNET) missing — run make setup"; exit 1; }
	@test -f $(ENV_BASE) || { echo "✗ $(ENV_BASE) missing — run make setup"; exit 1; }
	cd bot && cargo build --release
	@cd bot && { \
		./target/release/mev-bot --env-file ../$(ENV_MAINNET) run & main_pid=$$!; \
		./target/release/mev-bot --env-file ../$(ENV_BASE) run & base_pid=$$!; \
		trap 'kill $$main_pid $$base_pid 2>/dev/null' INT TERM; \
		wait -n $$main_pid $$base_pid; code=$$?; \
		kill $$main_pid $$base_pid 2>/dev/null; \
		wait $$main_pid $$base_pid 2>/dev/null; \
		echo "✗ a bot process exited (status $$code) — the other instance was stopped too; run them alone only via make bot-run-mainnet / make bot-run-base" >&2; \
		exit $${code:-1}; \
	}

bot-run-mainnet: ## run ONLY the Ethereum searcher + API (developer command)
	@test -f $(ENV_MAINNET) || { echo "✗ $(ENV_MAINNET) missing — run make setup"; exit 1; }
	cd bot && cargo run --release --bin mev-bot -- --env-file ../$(ENV_MAINNET) run

bot-run-base: ## run ONLY the Base searcher + API (developer command)
	@test -f $(ENV_BASE) || { echo "✗ $(ENV_BASE) missing — run make setup"; exit 1; }
	cd bot && cargo run --release --bin mev-bot -- --env-file ../$(ENV_BASE) run

replay: ## compare stored simulations against relay bid traces
	cd bot && cargo run --release --bin mev-bot -- --env-file ../$(ENV_MAINNET) replay

front-build: ## next build
	cd frontend && npm run build

front-dev: ## next dev on :3000
	cd frontend && npm run dev

deploy-dry: ## simulate the MevExecutor deployment against a fork
	cd contracts && forge script script/Deploy.s.sol --fork-url $$ETH_HTTP_URL -vvv

fmt: ## format everything
	cd contracts && forge fmt
	cd bot && cargo fmt --all

.PHONY: help setup build contracts contracts-check bot-build bot-check bot-test doctor doctor-mainnet doctor-base bot-run bot-run-mainnet bot-run-base replay front-build front-dev deploy-dry fmt
