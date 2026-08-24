//! State-pinned simulation at the provider's preconfirmed (`"pending"`) state.
//!
//! Work order 2.4: an anvil fork at the sealed head is not proof for a
//! 200 ms opportunity — the state it prices is up to a full block behind the
//! preconfirmation that produced the candidate. `eth_simulateV1` at the
//! `"pending"` tag executes against the state the sequencer is currently
//! building on (verified live against Base endpoints 2026-08-24:
//! `eth_simulateV1` accepts `"pending"`, and the preconfirmed tail is visible
//! there while `eth_getBlockByNumber("pending")` still hashes zero).
//!
//! The constructor-equivalent executor fixture is injected through
//! `eth_simulateV1`'s state overrides — the same runtime patching and
//! slot-3 WETH inventory as the anvil fixture
//! ([`executor_fixture_runtime`], one shared routine so the two cannot
//! drift; Base WETH's `balanceOf` mapping is slot 3, verified live against
//! the canonical predeploy 0x4200…0006 2026-08-24). No deployment is
//! therefore needed in shadow mode, and the simulated call is the
//! byte-identical payload the raw send would sign.
//!
//! Fail-closed rules:
//! - the simulation is three calls in one simulated block
//!   (`balanceOf(pre) → exact execute/flashExecute payload → balanceOf(post)`);
//!   any RPC error, non-success status or malformed response is an `Err`,
//!   never an interpolated profit;
//! - a native-ETH profit token is unsupported (no balance-slot trick) and
//!   returns an `Err` — no current strategy pins such a candidate;
//! - identity/TTL validity is the engine's job
//!   ([`crate::flashblocks::check_pin`]) before and after this call.

use std::sync::Arc;

use alloy_primitives::{Address, U256};
use alloy_sol_types::SolCall;
use anyhow::{anyhow, Context, Result};
use serde_json::json;

use crate::config::Config;
use crate::rpc::RpcClient;
use crate::types::{now_ms, Opportunity, SimBackend, SimulationResult};

use super::anvil::{self, executor_fixture_runtime, SIM_EXECUTOR};

/// Simulation funding for the fixture accounts, mirroring the anvil fixture:
/// 30 000 ETH of gas money for the searcher, 10 000 WETH inventory for the
/// executor. Simulation balances do not gate capital: notional limits live
/// in the risk envelope and the guard.
const SIM_ETH_BALANCE: u128 = 30_000u128 * 10u128.pow(18);
const SIM_WETH_INVENTORY: u128 = 10_000u128 * 10u128.pow(18);

/// State-pinned simulation via `eth_simulateV1` on the chain's primary RPC.
#[derive(Clone)]
pub struct PendingSim {
    cfg: Arc<Config>,
    rpc: RpcClient,
    risk: crate::risk::RuntimeRisk,
}

impl PendingSim {
    pub fn new(cfg: Arc<Config>, rpc: RpcClient, risk: crate::risk::RuntimeRisk) -> Self {
        Self { cfg, rpc, risk }
    }

    /// Simulate the candidate's own legs against the preconfirmed state.
    ///
    /// `base_fee` is the head's base fee, used for the gas-cost column of
    /// the result (the same accounting the anvil lane produces).
    pub async fn simulate_pinned(
        &self,
        opp: &Opportunity,
        base_fee: U256,
        started: std::time::Instant,
    ) -> Result<SimulationResult> {
        let pin = opp
            .provenance
            .source_state
            .as_ref()
            .context("pending sim requires a pinned source state")?;
        if opp.profit_token == Address::ZERO {
            return Err(anyhow!(
                "pending sim: native-ETH profit token unsupported (no balance slot)"
            ));
        }
        // The victim's transaction is already inside the preconfirmed
        // state, so only our own legs are simulated — exactly one leg.
        // Two-leg victims-in-the-middle (sandwich) cannot be modeled
        // post-preconfirmation and are structural errors here.
        let (legs, front) = match (opp.front_calls.is_empty(), opp.back_calls.is_empty()) {
            (false, true) => (&opp.front_calls, true),
            (true, false) => (&opp.back_calls, false),
            (true, true) => return Err(anyhow!("pending sim: opportunity has no calls")),
            (false, false) => {
                return Err(anyhow!(
                    "pending sim: victim-in-the-middle candidates are not simulatable \
                     against a post-victim state"
                ))
            }
        };
        let weth = self.cfg.chain.weth;
        let searcher = self.cfg.endpoints.searcher_address;
        let fixture_code = executor_fixture_runtime(&self.cfg)?;

        // Byte-identical to the signed bundle's payload:
        // `build_bundle` calls `encode_execute(opp, legs, front, risk)` with
        // the same runtime risk snapshot.
        let data = crate::bundle::encode_execute(opp, legs, front, &self.risk.risk());
        let balance_data = crate::dex::IERC20::balanceOfCall {
            account: SIM_EXECUTOR,
        }
        .abi_encode();

        let calls = json!([
            { "to": format!("{weth:?}"), "data": format!("0x{}", hex::encode(&balance_data)) },
            {
                "from": format!("{searcher:?}"),
                "to": format!("{SIM_EXECUTOR:?}"),
                "data": format!("0x{}", hex::encode(&data)),
                "maxFeePerGas": format!("{:#x}", base_fee.saturating_mul(U256::from(2u8)) + self.cfg.priority_fee_wei),
                "maxPriorityFeePerGas": format!("{:#x}", self.cfg.priority_fee_wei),
            },
            { "to": format!("{weth:?}"), "data": format!("0x{}", hex::encode(&balance_data)) },
        ]);

        let params = json!([{
            "blockStateCalls": [{
                "stateOverrides": fixture_overrides(weth, searcher, &fixture_code),
                "calls": calls,
            }],
            "traceTransfers": false,
            "validation": false,
        }, "pending"]);

        let response = self
            .rpc
            .call_raw("eth_simulateV1", params)
            .await
            .context("eth_simulateV1 RPC failed")?;

        let block = response
            .as_array()
            .and_then(|blocks| blocks.first())
            .ok_or_else(|| anyhow!("eth_simulateV1: empty block list"))?;
        let results = block["calls"]
            .as_array()
            .ok_or_else(|| anyhow!("eth_simulateV1: missing calls array"))?;
        if results.len() != 3 {
            return Err(anyhow!(
                "eth_simulateV1: expected 3 calls, got {}",
                results.len()
            ));
        }
        let status_ok = |i: usize| results[i]["status"].as_str() == Some("0x1");
        if !status_ok(0) || !status_ok(2) {
            return Err(anyhow!(
                "eth_simulateV1: balanceOf measurement calls failed"
            ));
        }

        let before = parse_u256_return(&results[0]["returnData"])?;
        let after = parse_u256_return(&results[2]["returnData"])?;
        let gas_used = parse_u64_hex(&results[1]["gasUsed"]).unwrap_or(0);

        let (success, revert_reason) = if status_ok(1) {
            (true, None)
        } else {
            let reason = results[1]["error"]["data"].as_str().map(|hex_data| {
                let raw =
                    crate::types::parse_bytes(&serde_json::Value::String(hex_data.to_string()));
                anvil::decode_revert_data(&raw)
            });
            (
                false,
                Some(
                    reason
                        .or_else(|| results[1]["error"]["message"].as_str().map(str::to_string))
                        .unwrap_or_else(|| "execution reverted".to_string()),
                ),
            )
        };

        // Profit = fixture executor's `profit_token` balance delta — the
        // same accounting source as the anvil lane.
        let gross = after.saturating_sub(before);
        let gas_price = base_fee.saturating_add(self.cfg.priority_fee_wei);
        let gas_cost = U256::from(gas_used).saturating_mul(gas_price);
        let net = crate::sim::anvil::to_i128(gross) - crate::sim::anvil::to_i128(gas_cost);

        let result = SimulationResult {
            opportunity_id: opp.id.clone(),
            strategy: opp.strategy,
            backend: SimBackend::EthSimulateV1,
            success,
            gross_profit_wei: gross,
            gas_used,
            gas_price_wei: gas_price,
            gas_cost_wei: gas_cost,
            bribe_wei: U256::ZERO,
            net_profit_wei: net,
            victim_predicted_out_wei: None,
            revert_reason,
            target_block: opp.target_block,
            sim_latency_ms: started.elapsed().as_millis() as u64,
            created_at_ms: now_ms(),
        };

        tracing::debug!(
            target: "sim",
            state = ?pin.state_id,
            block = pin.block_number,
            index = pin.flashblock_index,
            success = result.success,
            gross = %result.gross_profit_wei,
            gas = result.gas_used,
            "state-pinned simulation complete"
        );
        Ok(result)
    }
}

/// The `eth_simulateV1` `stateOverrides` object for the fixture: executor
/// code + fresh storage with the owner slot set, searcher gas balance, and
/// a single-slot WETH inventory patch on the token (never a whole-storage
/// replacement — that would wipe total supply and every holder row).
///
/// `pub` so the env-gated live fork test overrides the exact same state the
/// engine simulates with — one fixture definition, no drift.
pub fn fixture_overrides(
    weth: Address,
    searcher: Address,
    fixture_code: &[u8],
) -> serde_json::Value {
    // owner (storage slot 0) := searcher — `onlySearcher` accepts the owner,
    // same as the anvil fixture.
    let mut owner_word = [0u8; 32];
    owner_word[12..].copy_from_slice(searcher.as_slice());
    // keccak(executor ‖ slot 3) — Base WETH stores balanceOf at slot 3,
    // identical to mainnet WETH9 (verified live 2026-08-24).
    let mut key = [0u8; 64];
    key[12..32].copy_from_slice(SIM_EXECUTOR.as_slice());
    key[63] = 3;
    let slot = alloy_primitives::keccak256(key);

    json!({
        format!("{SIM_EXECUTOR:?}"): {
            "code": format!("0x{}", hex::encode(fixture_code)),
            "state": {
                "0x0000000000000000000000000000000000000000000000000000000000000000":
                    format!("0x{}", hex::encode(owner_word)),
            },
        },
        format!("{searcher:?}"): {
            "balance": format!("0x{:x}", U256::from(SIM_ETH_BALANCE)),
        },
        format!("{weth:?}"): {
            "stateDiff": {
                format!("{slot:?}"): format!("0x{:064x}", U256::from(SIM_WETH_INVENTORY)),
            },
        },
    })
}

/// Decode a 32-byte return word as U256.
fn parse_u256_return(v: &serde_json::Value) -> Result<U256> {
    let raw = crate::types::parse_bytes(v);
    if raw.len() != 32 {
        return Err(anyhow!(
            "returnData is {} bytes, expected a 32-byte word",
            raw.len()
        ));
    }
    Ok(U256::from_be_slice(&raw))
}

fn parse_u64_hex(v: &serde_json::Value) -> Option<u64> {
    let s = v.as_str()?;
    u64::from_str_radix(s.strip_prefix("0x").unwrap_or(s), 16).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Call, Provenance, Strategy};

    #[test]
    fn fixture_overrides_patch_owner_code_balance_and_one_weth_slot() {
        let weth = Address::with_last_byte(0xAA);
        let searcher = Address::with_last_byte(0x42);
        let code = vec![0x60, 0x80, 0x60, 0x40];
        let ov = fixture_overrides(weth, searcher, &code);

        // Executor: code + fresh storage with one owner slot.
        let exec = &ov[format!("{SIM_EXECUTOR:?}")];
        assert_eq!(exec["code"].as_str(), Some("0x60806040"));
        let owner = exec["state"]
            ["0x0000000000000000000000000000000000000000000000000000000000000000"]
            .as_str()
            .expect("owner slot");
        let owner_bytes = crate::types::parse_bytes(&serde_json::Value::String(owner.to_string()));
        assert_eq!(Address::from_slice(&owner_bytes[12..32]), searcher);

        // Searcher has gas money.
        assert!(ov[format!("{searcher:?}")]["balance"].as_str().is_some());

        // WETH: stateDiff (never a full `state` replacement), exactly one
        // slot, holding the fixture inventory.
        let w = &ov[format!("{weth:?}")];
        assert!(w.get("state").is_none(), "must never wipe token storage");
        let slots = w["stateDiff"].as_object().expect("stateDiff");
        assert_eq!(slots.len(), 1);
        let (slot, value) = slots.iter().next().unwrap();
        let mut key = [0u8; 64];
        key[12..32].copy_from_slice(SIM_EXECUTOR.as_slice());
        key[63] = 3;
        assert_eq!(slot, &format!("{:?}", alloy_primitives::keccak256(key)));
        assert_eq!(
            value.as_str().unwrap(),
            format!("0x{:064x}", U256::from(SIM_WETH_INVENTORY))
        );
    }

    #[test]
    fn parses_simulate_results_exactly() {
        let good =
            serde_json::json!("0x0000000000000000000000000000000000000000000000000de0b6b3a7640000");
        assert_eq!(
            parse_u256_return(&good).unwrap(),
            U256::from(1_000_000_000_000_000_000u128)
        );
        // Not a word -> hard error, never a guess.
        assert!(parse_u256_return(&serde_json::json!("0x00")).is_err());
        assert!(parse_u256_return(&serde_json::json!("0x")).is_err());
        assert_eq!(parse_u64_hex(&serde_json::json!("0x5208")), Some(21_000));
        assert_eq!(parse_u64_hex(&serde_json::json!(42)), None);
    }

    fn base_cfg() -> Arc<Config> {
        // Same shape as risk.rs's test config, on the Base chain profile.
        Arc::new(Config {
            chain: crate::config::ChainConfig {
                chain_id: 8453,
                name: "base".into(),
                weth: crate::config::known::BASE_WETH,
                usd_stable: crate::config::known::BASE_USDC,
                block_time_ms: 2_000,
            },
            addresses: *crate::config::known::base(),
            priority_fee_wei: U256::from(1_000_000_000u64),
            token_valuation: false,
            valuation_haircut_bps: crate::valuation::DEFAULT_HAIRCUT_BPS,
            raw_cancel_bump_bps: 1_250,
            raw_cancel_max_fee_wei: U256::from(500_000_000_000u64),
            submission_mode: crate::config::SubmissionMode::Raw,
            qualification_backend: crate::config::QualificationBackend::Sequencer,
            chain_block_ingest: false,
            endpoints: crate::config::Endpoints {
                http_url: "http://localhost:8545".into(),
                ws_url: None,
                mev_share_sse: String::new(),
                relay_url: String::new(),
                bundle_relay_urls: vec![],
                relay_data_urls: vec![],
                bloxroute_relay_url: String::new(),
                sequencer_feed: None,
                flashblocks_ws: None,
                extra_mempool_ws: vec![],
                mev_blocker_ws: None,
                flashbots_signer_key: None,
                searcher_private_key: None,
                sniper_searcher_private_key: None,
                executor: None,
                searcher_address: Address::with_last_byte(0x42),
                sniper_searcher_address: Address::ZERO,
            },
            risk: crate::config::RiskConfig {
                min_net_profit_wei: U256::from(1u8),
                max_position_wei: U256::from(1_000u64),
                max_base_fee_wei: U256::from(u64::MAX),
                bribe_bps: 0,
                max_gas_per_bundle: 1_000_000,
                max_drawdown_wei: U256::ZERO,
                max_inflight_per_strategy: 2,
                max_revert_rate: 1.0,
            },
            strategies: crate::config::StrategyToggles {
                sandwich: false,
                sandwich_v3: false,
                jit: false,
                atomic_arb: true,
                liquidation: false,
                liquidation_compound: false,
                liquidation_morpho: false,
                liquidation_maker: false,
                oracle_frontrun: false,
                sniper: false,
            },
            sim: crate::config::SimConfig {
                anvil_bin: "anvil".into(),
                anvil_port: 8548,
                anvil_replay_port: 8549,
                replay_fork: false,
                refork_every_blocks: 1,
                use_call_bundle: false,
                target_block_offset: 1,
                timeout: std::time::Duration::from_millis(1_000),
            },
            sniper_mode: crate::sniper::SniperModeBoot::default(),
            liquidation: crate::config::LiquidationConfig {
                watch_cap: 8,
                morpho_market_cap: 4,
                morpho_borrower_cap: 4,
                maker_ilks: vec![],
            },
            oracle: crate::config::OracleConfig {
                watch_feeds: vec![],
                max_leads: 3,
            },
            alerts: crate::config::AlertsConfig::default(),
            api: crate::config::ApiConfig {
                bind: "127.0.0.1:0".into(),
                db_path: ":memory:".into(),
                feed_capacity: 10,
                write_queue_capacity: 1_024,
                auth_token: None,
                allowed_origins: vec![],
            },
            pool_discovery: true,
            pool_discovery_v3: false,
            decode_universal_router: false,
            dex_univ3_arb: false,
            dex_aerodrome_arb: false,
            dex_aerodrome_stable: false,
            arb_max_cycle_len: 2,
            relay_tx_ingest: false,
            relay_tx_concurrency: 4,
            strategy_concurrency: 64,
            replay_lanes: 1,
            replay_queue_depth: 4,
            pool_discovery_interval_blocks: 1,
            inventory_refresh_blocks: 1,
            arb_enumeration_budget: std::time::Duration::from_millis(25),
            arb_max_pools: 200,
            inventory_gate: false,
            live_execution: false,
            broadcast_enabled: false,
            qualification_hours: 168,
            qualification_min_samples: 30,
            qualification_min_relay_comparisons: 30,
            qualification_min_actual_matches: 30,
            qualification_max_error_bps: 2_000,
            qualification_min_accuracy_bps: 8_000,
            qualification_max_gap_secs: 120,
            finality_depth: 12,
            preconfirmed_ttl_ms: 1_000,
            submission_retry_ms: 250,
            submission_max_attempts: 2,
            live_smoke_max: 0,
            live_smoke_max_gas_cost_wei: U256::ZERO,
        })
    }

    fn pinned_opp(profit_token: Address) -> Opportunity {
        Opportunity {
            id: "x".into(),
            strategy: Strategy::AtomicArb,
            victim_hashes: vec![alloy_primitives::B256::with_last_byte(9)],
            front_calls: vec![],
            back_calls: vec![Call::new(Address::with_last_byte(1), vec![1])],
            flash_tokens: vec![],
            flash_amounts: vec![],
            profit_token,
            expected_profit_wei: U256::ZERO,
            notional_wei: U256::ZERO,
            target_block: 1,
            created_at_ms: now_ms(),
            notes: String::new(),
            provenance: Provenance {
                source_state: Some(crate::types::PreconfirmedState {
                    feed: "test".into(),
                    block_number: 1,
                    flashblock_index: 3,
                    state_id: alloy_primitives::B256::ZERO,
                    payload_id: "p".into(),
                    prev_frame_id: None,
                    parent_hash: None,
                    observed_at_ms: now_ms(),
                    ordered: true,
                }),
                ttl_ms: Some(1_000),
                requires_foreign_payload: false,
                ..Default::default()
            },
        }
    }

    #[tokio::test]
    async fn native_profit_token_fails_closed_before_any_rpc() {
        let cfg = base_cfg();
        // Dead endpoint: must never be reached — the refusal precedes it.
        let rpc = RpcClient::new("http://127.0.0.1:9".to_string()).unwrap();
        let risk = crate::risk::RuntimeRisk::new(cfg.risk.clone(), cfg.strategies.clone());
        let sim = PendingSim::new(cfg, rpc, risk);
        let err = sim
            .simulate_pinned(
                &pinned_opp(Address::ZERO),
                U256::ZERO,
                std::time::Instant::now(),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("native-ETH"), "{err}");
    }

    #[tokio::test]
    async fn fixture_runtime_matches_the_anvil_patch() {
        // One routine serves both fixtures: assert the artifact carries the
        // chain's WETH binding after patching (Base profile).
        let cfg = base_cfg();
        let code = executor_fixture_runtime(&cfg).expect("patched runtime");
        let weth = super::super::anvil::EXECUTOR_RUNTIME_BYTECODE
            .trim()
            .trim_start_matches("0x");
        let raw_len = weth.len() / 2;
        assert_eq!(code.len(), raw_len, "patching must not resize the bytecode");
        // The WETH immutable must appear somewhere in the patched runtime.
        let weth_bytes = crate::config::known::BASE_WETH;
        let found = code.windows(20).any(|w| w == weth_bytes.as_slice());
        assert!(found, "patched runtime must bind the chain WETH immutable");
    }
}
