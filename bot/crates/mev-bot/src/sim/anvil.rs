//! Forked-mainnet simulation backend.
//!
//! Spawns (or attaches to) an `anvil` process forked from the live chain at the
//! current head and replays `front-run → victim → back-run` inside it. Because
//! anvil executes the real EVM against real mainnet state, the balance delta it
//! reports is the ground truth for "would this bundle have made money".
//!
//! Nothing here can touch mainnet: every RPC call goes to the local fork.

use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use alloy_primitives::{Address, U256};
use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use tokio::sync::Mutex;

use crate::config::Config;
use crate::rpc::RpcClient;
use crate::types::{now_ms, parse_u64, Call, Opportunity, SimBackend, SimulationResult};

/// Runtime bytecode of `MevExecutor`, produced by `contracts/script/compile-check.js`.
/// Injected into the fork with `anvil_setCode` so simulation works before the
/// contract is deployed anywhere.
pub const EXECUTOR_RUNTIME_BYTECODE: &str = include_str!("../../artifacts/MevExecutor.runtime.hex");

/// Address the executor is mounted at inside the fork when no real deployment exists.
pub const SIM_EXECUTOR: Address = alloy_primitives::address!("00000000000000000000000000000000000e0000");

pub struct AnvilSim {
    cfg: Arc<Config>,
    rpc: RpcClient,
    child: Mutex<Option<tokio::process::Child>>,
    /// Block the fork is currently pinned to.
    forked_at: Mutex<u64>,
    executor: Address,
    searcher: Address,
    /// Serialises access: one simulation at a time per fork.
    lock: Mutex<()>,
}

impl AnvilSim {
    /// Start a local anvil forked at `block` on the configured primary port.
    pub async fn spawn(cfg: Arc<Config>, block: u64) -> Result<Self> {
        let port = cfg.sim.anvil_port;
        Self::spawn_on(cfg, block, port).await
    }

    /// Start a local anvil forked at `block` on an explicit port.
    ///
    /// A second instance on a second port is what keeps replay work off the
    /// live fork: the two pin to different heights and would otherwise reset
    /// each other on every alternating simulation.
    pub async fn spawn_on(cfg: Arc<Config>, block: u64, port: u16) -> Result<Self> {
        let mut cmd = tokio::process::Command::new(&cfg.sim.anvil_bin);
        cmd.arg("--fork-url")
            .arg(&cfg.endpoints.http_url)
            .arg("--fork-block-number")
            .arg(block.to_string())
            .arg("--port")
            .arg(port.to_string())
            .arg("--host")
            .arg("127.0.0.1")
            .arg("--auto-impersonate")
            .arg("--no-rate-limit")
            .arg("--silent")
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let child = cmd
            .spawn()
            .with_context(|| format!("failed to spawn `{}` — is Foundry installed?", cfg.sim.anvil_bin))?;

        let rpc = RpcClient::new(format!("http://127.0.0.1:{port}"))?;

        // Wait for the fork to answer.
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            if rpc.call_raw("eth_blockNumber", json!([])).await.is_ok() {
                break;
            }
            if Instant::now() > deadline {
                bail!("anvil did not become ready within 60s");
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }

        let executor = cfg.endpoints.executor.unwrap_or(SIM_EXECUTOR);
        let sim = Self {
            searcher: cfg.endpoints.searcher_address,
            cfg,
            rpc,
            child: Mutex::new(Some(child)),
            forked_at: Mutex::new(block),
            executor,
            lock: Mutex::new(()),
        };
        sim.prepare_state().await?;
        Ok(sim)
    }

    /// Attach to an already-running anvil (useful in CI / docker-compose).
    pub async fn attach(cfg: Arc<Config>, url: &str) -> Result<Self> {
        let rpc = RpcClient::new(url.to_string())?;
        let block = parse_u64(&rpc.call_raw("eth_blockNumber", json!([])).await?);
        let executor = cfg.endpoints.executor.unwrap_or(SIM_EXECUTOR);
        let sim = Self {
            searcher: cfg.endpoints.searcher_address,
            cfg,
            rpc,
            child: Mutex::new(None),
            forked_at: Mutex::new(block),
            executor,
            lock: Mutex::new(()),
        };
        sim.prepare_state().await?;
        Ok(sim)
    }

    pub fn executor(&self) -> Address {
        self.executor
    }

    /// Install the executor bytecode, make the searcher the owner and give both
    /// accounts a spending balance inside the fork.
    async fn prepare_state(&self) -> Result<()> {
        let code = EXECUTOR_RUNTIME_BYTECODE.trim();
        if self.cfg.endpoints.executor.is_none() && !code.is_empty() {
            self.rpc
                .call_raw(
                    "anvil_setCode",
                    json!([format!("{:?}", self.executor), format!("0x{}", code.trim_start_matches("0x"))]),
                )
                .await
                .context("anvil_setCode for the simulated executor")?;
            // storage slot 0 == `owner`
            self.rpc
                .call_raw(
                    "anvil_setStorageAt",
                    json!([
                        format!("{:?}", self.executor),
                        "0x0",
                        format!("0x{:064x}", U256::from_be_slice(self.searcher.as_slice()))
                    ]),
                )
                .await
                .context("anvil_setStorageAt owner")?;
        }
        let big = "0x21e19e0c9bab2400000"; // 10_000 ETH
        for who in [self.searcher, self.executor] {
            let _ = self
                .rpc
                .call_raw("anvil_setBalance", json!([format!("{who:?}"), big]))
                .await;
        }
        Ok(())
    }

    /// Re-fork at `block` if we have drifted.
    /// Pin the fork to exactly `block`, resetting in **either** direction.
    ///
    /// [`ensure_fork_at`] only ever moves forward, which is right for the live
    /// fork — it should track the head and never rewind. Replay needs the
    /// opposite: it must land on the parent of a specific historical block,
    /// which is almost always behind the head. Calling this on the live fork
    /// would rewind it under the mempool path, so it belongs to the dedicated
    /// replay instance.
    pub async fn ensure_fork_exact(&self, block: u64) -> Result<()> {
        let _guard = self.lock.lock().await;
        self.ensure_fork_exact_locked(block).await
    }

    async fn ensure_fork_exact_locked(&self, block: u64) -> Result<()> {
        let mut at = self.forked_at.lock().await;
        if *at == block {
            return Ok(());
        }
        self.rpc
            .call_raw(
                "anvil_reset",
                json!([{"forking": {"jsonRpcUrl": self.cfg.endpoints.http_url, "blockNumber": block}}]),
            )
            .await
            .context("anvil_reset (replay)")?;
        *at = block;
        drop(at);
        self.prepare_state().await
    }

    /// The block this fork is currently pinned to.
    pub async fn forked_at(&self) -> u64 {
        *self.forked_at.lock().await
    }

    pub async fn ensure_fork_at(&self, block: u64) -> Result<()> {
        let mut at = self.forked_at.lock().await;
        if block <= *at || block - *at < self.cfg.sim.refork_every_blocks {
            return Ok(());
        }
        self.rpc
            .call_raw(
                "anvil_reset",
                json!([{"forking": {"jsonRpcUrl": self.cfg.endpoints.http_url, "blockNumber": block}}]),
            )
            .await
            .context("anvil_reset")?;
        *at = block;
        drop(at);
        self.prepare_state().await
    }

    /// Simulate one opportunity end to end.
    ///
    /// Ordering inside the fork mirrors the bundle we would submit:
    ///   `[front-run] → [victim tx…] → [back-run]`
    pub async fn simulate(
        &self,
        opp: &Opportunity,
        victims_raw: &[Vec<u8>],
        victim_sender_nonce: Option<(Address, u64)>,
        base_fee: U256,
    ) -> Result<SimulationResult> {
        self.simulate_locked(opp, victims_raw, victim_sender_nonce, base_fee).await
    }

    /// Pin and simulate while holding the same mutex. This prevents a reset
    /// from invalidating the snapshot between pinning and execution.
    pub async fn simulate_at(
        &self,
        block: u64,
        opp: &Opportunity,
        victims_raw: &[Vec<u8>],
        victim_sender_nonce: Option<(Address, u64)>,
        base_fee: U256,
    ) -> Result<SimulationResult> {
        let _guard = self.lock.lock().await;
        self.ensure_fork_exact_locked(block).await?;
        self.simulate_locked(opp, victims_raw, victim_sender_nonce, base_fee).await
    }

    async fn simulate_locked(
        &self,
        opp: &Opportunity,
        victims_raw: &[Vec<u8>],
        victim_sender_nonce: Option<(Address, u64)>,
        base_fee: U256,
    ) -> Result<SimulationResult> {
        let started = Instant::now();
        let snapshot: Value = self.rpc.call_raw("evm_snapshot", json!([])).await?;
        let result = self
            .simulate_inner(opp, victims_raw, victim_sender_nonce, base_fee, started)
            .await;
        // Always roll the fork back, even if the simulation blew up.
        let _ = self.rpc.call_raw("evm_revert", json!([snapshot])).await;
        result
    }

    async fn simulate_inner(
        &self,
        opp: &Opportunity,
        victims_raw: &[Vec<u8>],
        victim_sender_nonce: Option<(Address, u64)>,
        base_fee: U256,
        started: Instant,
    ) -> Result<SimulationResult> {
        // Manual mining so the whole bundle lands in one block, like a real bundle.
        let _ = self.rpc.call_raw("evm_setAutomine", json!([false])).await;

        let before = self.balance_of(opp.profit_token).await?;
        let mut gas_used = 0u64;
        let mut revert_reason: Option<String> = None;
        let mut ok = true;
        let mut tx_hashes: Vec<String> = Vec::new();

        // 1. front-run
        if !opp.front_calls.is_empty() {
            match self.send_executor_tx(opp, &opp.front_calls, base_fee, true).await {
                Ok(h) => tx_hashes.push(h),
                Err(e) => {
                    ok = false;
                    revert_reason = Some(format!("front-run rejected: {e}"));
                }
            }
        }

        // 2. victim transactions, replayed verbatim
        if ok {
            // A pending transaction may have landed between observation and
            // forking. Reset the impersonated sender's nonce so Anvil can
            // replay the signed victim at the nonce it carried when observed.
            // This is fork-local state and is reverted with the snapshot below.
            if let Some((sender, nonce)) = victim_sender_nonce {
                let _ = self.rpc.call_raw(
                    "anvil_setNonce",
                    json!([format!("{sender:?}"), format!("0x{nonce:x}")]),
                ).await;
            }
            for raw in victims_raw {
                let hex_raw = format!("0x{}", hex::encode(raw));
                match self
                    .rpc
                    .call_raw("eth_sendRawTransaction", json!([hex_raw]))
                    .await
                {
                    Ok(h) => {
                        if let Some(s) = h.as_str() {
                            tx_hashes.push(s.to_string());
                        }
                    }
                    Err(e) => {
                        // A victim we cannot replay (missing raw bytes, nonce gap,
                        // already mined) invalidates the whole observation.
                        ok = false;
                        revert_reason = Some(format!("victim replay failed: {e}"));
                        break;
                    }
                }
            }
        }

        // 3. back-run
        if ok && !opp.back_calls.is_empty() {
            match self.send_executor_tx(opp, &opp.back_calls, base_fee, false).await {
                Ok(h) => tx_hashes.push(h),
                Err(e) => {
                    ok = false;
                    revert_reason = Some(format!("back-run rejected: {e}"));
                }
            }
        }

        let _ = self.rpc.call_raw("evm_mine", json!([])).await;
        let _ = self.rpc.call_raw("evm_setAutomine", json!([true])).await;

        // Collect receipts.
        for h in &tx_hashes {
            let receipt: Value = self
                .rpc
                .call_raw("eth_getTransactionReceipt", json!([h]))
                .await
                .unwrap_or(Value::Null);
            if receipt.is_null() {
                ok = false;
                revert_reason.get_or_insert_with(|| format!("tx {h} was not mined"));
                continue;
            }
            gas_used += parse_u64(&receipt["gasUsed"]);
            if parse_u64(&receipt["status"]) != 1 {
                ok = false;
                revert_reason.get_or_insert_with(|| format!("tx {h} reverted"));
            }
        }

        let after = self.balance_of(opp.profit_token).await?;
        let gross = after.saturating_sub(before);
        let gas_price = base_fee + U256::from(1_000_000_000u64); // +1 gwei tip floor
        let gas_cost = gas_price * U256::from(gas_used);
        let bribe = gross * U256::from(self.cfg.risk.bribe_bps) / U256::from(10_000u32);
        let net = to_i128(gross) - to_i128(gas_cost) - to_i128(bribe);

        Ok(SimulationResult {
            opportunity_id: opp.id.clone(),
            strategy: opp.strategy,
            backend: SimBackend::AnvilFork,
            success: ok && gross > U256::ZERO,
            gross_profit_wei: gross,
            gas_used,
            gas_price_wei: gas_price,
            gas_cost_wei: gas_cost,
            bribe_wei: bribe,
            net_profit_wei: net,
            revert_reason,
            target_block: opp.target_block,
            sim_latency_ms: started.elapsed().as_millis() as u64,
            created_at_ms: now_ms(),
        })
    }

    async fn send_executor_tx(
        &self,
        opp: &Opportunity,
        calls: &[Call],
        base_fee: U256,
        front: bool,
    ) -> Result<String> {
        let data = crate::bundle::encode_execute(opp, calls, front, &self.cfg.risk);
        let tx = json!([{
            "from": format!("{:?}", self.searcher),
            "to": format!("{:?}", self.executor),
            "data": format!("0x{}", hex::encode(data)),
            "gas": format!("0x{:x}", self.cfg.risk.max_gas_per_bundle),
            "maxFeePerGas": format!("0x{:x}", base_fee * U256::from(2u8) + U256::from(2_000_000_000u64)),
            "maxPriorityFeePerGas": format!("0x{:x}", 1_000_000_000u64),
        }]);
        let h: Value = self.rpc.call_raw("eth_sendTransaction", tx).await?;
        h.as_str()
            .map(|s| s.to_string())
            .ok_or_else(|| anyhow!("eth_sendTransaction returned no hash"))
    }

    async fn balance_of(&self, token: Address) -> Result<U256> {
        if token == Address::ZERO {
            let v = self
                .rpc
                .call_raw("eth_getBalance", json!([format!("{:?}", self.executor), "latest"]))
                .await?;
            return Ok(crate::types::parse_u256(&v));
        }
        let data = crate::dex::IERC20::balanceOfCall {
            account: self.executor,
        };
        use alloy_sol_types::SolCall;
        let v = self
            .rpc
            .call_raw(
                "eth_call",
                json!([
                    {"to": format!("{token:?}"), "data": format!("0x{}", hex::encode(data.abi_encode()))},
                    "latest"
                ]),
            )
            .await?;
        Ok(crate::types::parse_u256(&v))
    }

    pub async fn shutdown(&self) {
        if let Some(mut child) = self.child.lock().await.take() {
            let _ = child.kill().await;
        }
    }
}

pub fn to_i128(v: U256) -> i128 {
    // Values beyond i128 are nonsense for PnL purposes; saturate.
    if v > U256::from(i128::MAX as u128) {
        i128::MAX
    } else {
        v.to::<u128>() as i128
    }
}
