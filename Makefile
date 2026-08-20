.DEFAULT_GOAL := help
SHELL := /bin/bash

help: ## show this help
	@grep -hE '^[a-zA-Z_-]+:.*?## ' $(MAKEFILE_LIST) | awk 'BEGIN{FS=":.*?## "}{printf "  \033[36m%-18s\033[0m %s\n", $$1, $$2}'

setup: ## install everything (foundry submodules, npm deps)
	git submodule update --init --recursive
	cd contracts && npm install --no-audit --no-fund
	cd frontend && npm install --no-audit --no-fund
	@test -f .env || cp .env.example .env
	@echo "→ edit .env and set ETH_HTTP_URL / ETH_WS_URL"

build: contracts bot-build front-build ## build everything

contracts: ## forge build + tests
	cd contracts && forge build --sizes && forge test -vvv

contracts-check: ## solc-only compile check (no foundry needed); regenerates artifacts
	cd contracts && node script/compile-check.js

bot-build: ## cargo build --release
	cd bot && cargo build --release

bot-test: ## cargo test
	cd bot && cargo test --all

bot-run: ## run the searcher + API
	cd bot && cargo run --release --bin mev-bot -- --env-file ../.env run

doctor: ## check every configured endpoint
	cd bot && cargo run --release --bin mev-bot -- --env-file ../.env doctor

front-build: ## next build
	cd frontend && npm run build

front-dev: ## next dev on :3000
	cd frontend && npm run dev

deploy-dry: ## simulate the MevExecutor deployment against a fork
	cd contracts && forge script script/Deploy.s.sol --fork-url $$ETH_HTTP_URL -vvv

fmt: ## format everything
	cd contracts && forge fmt
	cd bot && cargo fmt --all

.PHONY: help setup build contracts contracts-check bot-build bot-test bot-run doctor front-build front-dev deploy-dry fmt
