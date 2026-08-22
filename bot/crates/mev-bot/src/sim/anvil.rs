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
pub const SIM_EXECUTOR: Address =
    alloy_primitives::address!("00000000000000000000000000000000000e0000");

/// Ceiling for the *signed* bundle transactions (the relay `eth_callBundle`
/// path, which has no fork to read a live limit from): comfortably below any
/// L1 block gas limit, past or present, so a misconfigured
/// `MAX_GAS_PER_BUNDLE` can never get a bundle rejected before it runs.
pub const MAX_TX_GAS_CEILING: u64 = 30_000_000;

/// Clamp a configured bundle gas limit to 95% of the block gas limit (5%
/// headroom for the victim transactions sharing the mined block). A limit of
/// 0 means "unknown" and leaves the configured value untouched.
fn clamp_gas(configured: u64, block_gas_limit: u64) -> u64 {
    if block_gas_limit == 0 {
        return configured;
    }
    let capped = block_gas_limit
        .saturating_sub(block_gas_limit / 20)
        .max(21_000);
    configured.min(capped)
}

pub struct AnvilSim {
    cfg: Arc<Config>,
    rpc: RpcClient,
    child: Mutex<Option<tokio::process::Child>>,
    /// Block the fork is currently pinned to.
    forked_at: Mutex<u64>,
    executor: Address,
    searcher: Address,
    /// Block gas limit of the forked chain, read after every (re)fork. The
    /// executor transactions are clamped below it — see [`Self::executor_gas_limit`].
    block_gas_limit: Mutex<u64>,
    /// Warns once per process when `MAX_GAS_PER_BUNDLE` had to be clamped.
    clamp_warned: std::sync::atomic::AtomicBool,
    /// The runtime risk envelope — `minProfit`/`bribeBps` guards and the gas
    /// cap follow dashboard changes immediately, not at the next restart.
    risk: crate::risk::RuntimeRisk,
    /// Serialises access: one simulation at a time per fork.
    lock: Mutex<()>,
}

/// A transaction the simulator injected into the fork, kept around so a
/// status-0 receipt can be re-executed via `eth_call` and its revert data
/// decoded. Victim replay targets (`eth_sendRawTransaction`) have no local
/// call params, so they carry `replay: None` and get the plain label.
struct SentTx {
    /// "front-run" | "back-run" | "victim"
    label: &'static str,
    /// `eth_call` params reproducing the transaction (executor legs only).
    replay: Option<Value>,
}

/// Decode revert bytes into something a human can triage: the executor's own
/// guards (`Unprofitable(...)`), the protocols' known rejections, Solidity
/// `Error(string)`/`Panic`, or a named-as-hex custom error.
///
/// Selector constants come from `sol!` (compile-time keccak), not from a
/// hand-copied table — an exact-selector typo here would silently turn every
/// revert back into "custom error 0x…" and the whole point of the decoder
/// would be lost.
pub fn decode_revert_data(data: &[u8]) -> String {
    decode_revert_data_at(data, 0)
}

/// Depth-bounded worker — `CallFailed` carries the failed leg's own revert
/// data, which is decoded recursively; three levels is deeper than anything
/// real protocols produce.
fn decode_revert_data_at(data: &[u8], depth: u8) -> String {
    use alloy_sol_types::{sol, SolError};
    sol! {
        interface MevExecutorErrors {
            error NotOwner();
            error NotSearcher();
            error Reentrancy();
            error Deadline();
            error BaseFeeTooHigh();
            error Unprofitable(uint256 realised, uint256 required);
            error CallFailed(uint256 index, bytes returndata);
            error BadFlashCallback();
            error BadBribe();
        }
        interface KnownProtocolErrors {
            // Morpho Blue (ErrorsLib)
            error HEALTHY_POSITION();
            error INCONSISTENT_INPUT();
            error MARKET_NOT_CREATED();
            // Compound V3 (Comet)
            error TooMuchSlippage();
            error NotForSale();
            error NotLiquidatable();
            error Paused();
            // Aave V3
            error LiquidationCallFailed();
        }
    }
    const ERROR_STRING: [u8; 4] = [0x08, 0xc3, 0x79, 0xa0];
    const PANIC: [u8; 4] = [0x4e, 0x48, 0x7b, 0x71];

    if data.len() < 4 {
        return "no revert data".to_string();
    }
    let selector: [u8; 4] = [data[0], data[1], data[2], data[3]];
    let words = |from: usize| -> Vec<U256> {
        data[4 + from * 32..]
            .chunks_exact(32)
            .map(U256::from_be_slice)
            .collect()
    };
    if selector == ERROR_STRING {
        // Error(string): offset, length, bytes — decode the message text.
        if data.len() >= 4 + 64 {
            let len = words(1).first().copied().unwrap_or_default();
            let len = len.min(U256::from(512u32)).to::<usize>();
            let start = 4 + 64;
            if data.len() >= start + len {
                let text = String::from_utf8_lossy(&data[start..start + len]);
                return format!("Error(\"{text}\")");
            }
        }
        return "Error(<malformed>)".to_string();
    }
    if selector == PANIC {
        let code = words(0).first().copied().unwrap_or_default();
        return format!("Panic({code})");
    }
    let named = |sel: [u8; 4], name: &str| {
        if selector == sel {
            Some(name.to_string())
        } else {
            None
        }
    };
    let plain = [
        (MevExecutorErrors::NotOwner::SELECTOR, "NotOwner()"),
        (MevExecutorErrors::NotSearcher::SELECTOR, "NotSearcher()"),
        (MevExecutorErrors::Reentrancy::SELECTOR, "Reentrancy()"),
        (MevExecutorErrors::Deadline::SELECTOR, "Deadline()"),
        (
            MevExecutorErrors::BaseFeeTooHigh::SELECTOR,
            "BaseFeeTooHigh()",
        ),
        (
            MevExecutorErrors::BadFlashCallback::SELECTOR,
            "BadFlashCallback()",
        ),
        (MevExecutorErrors::BadBribe::SELECTOR, "BadBribe()"),
        (
            KnownProtocolErrors::HEALTHY_POSITION::SELECTOR,
            "HEALTHY_POSITION() (position healthy at execution)",
        ),
        (
            KnownProtocolErrors::INCONSISTENT_INPUT::SELECTOR,
            "INCONSISTENT_INPUT()",
        ),
        (
            KnownProtocolErrors::MARKET_NOT_CREATED::SELECTOR,
            "MARKET_NOT_CREATED()",
        ),
        (
            KnownProtocolErrors::TooMuchSlippage::SELECTOR,
            "TooMuchSlippage()",
        ),
        (
            KnownProtocolErrors::NotForSale::SELECTOR,
            "NotForSale() (reserves >= targetReserves)",
        ),
        (
            KnownProtocolErrors::NotLiquidatable::SELECTOR,
            "NotLiquidatable()",
        ),
        (KnownProtocolErrors::Paused::SELECTOR, "Paused()"),
        (
            KnownProtocolErrors::LiquidationCallFailed::SELECTOR,
            "LiquidationCallFailed()",
        ),
    ]
    .iter()
    .find_map(|(sel, name)| named(*sel, name));
    if let Some(name) = plain {
        return name.to_string();
    }
    if selector == MevExecutorErrors::Unprofitable::SELECTOR {
        let w = words(0);
        return format!(
            "Unprofitable(realised={}, required={}) — the profit guard: gross did not clear minProfit",
            w.first().copied().unwrap_or_default(),
            w.get(1).copied().unwrap_or_default()
        );
    }
    if selector == MevExecutorErrors::CallFailed::SELECTOR {
        let w = words(0);
        let index = w.first().copied().unwrap_or_default();
        return match decode_callfailed_inner(data) {
            Some(inner) if inner.is_empty() => format!(
                "CallFailed(index={index}): the leg reverted bare (a require() without a message)"
            ),
            Some(inner) if inner.len() >= 4 && depth < 3 => format!(
                "CallFailed(index={}): {}",
                index,
                decode_revert_data_at(&inner, depth + 1)
            ),
            Some(inner) => format!(
                "CallFailed(index={}): raw 0x{}…",
                index,
                hex::encode(&inner[..inner.len().min(8)])
            ),
            // Malformed length/offset — unreachable from the contract's own
            // encoding, but a decoder must never panic on hostile bytes.
            None => format!("CallFailed(index={index}) — a leg reverted inside the batch"),
        };
    }
    let w = words(0);
    let args = if w.is_empty() {
        String::new()
    } else {
        format!(
            "({})",
            w.iter()
                .take(4)
                .map(|v| v.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    format!("custom error 0x{}{args}", hex::encode(selector))
}

/// Pull the `bytes returndata` argument out of a `CallFailed(uint256, bytes)`
/// payload. Layout after the 4-byte selector (a standard tuple head):
/// word0 = index, word1 = byte offset of the bytes data (relative to the
/// head start), then at that offset: a length word and the bytes themselves.
/// The executor reverts with the leg's *raw* revert bytes here
/// (`revert CallFailed(i, ret)`), so the payload is itself decodable.
fn decode_callfailed_inner(data: &[u8]) -> Option<Vec<u8>> {
    // selector(4) + index(32) + offset(32), then at least the length word.
    if data.len() < 4 + 64 + 32 {
        return None;
    }
    let offset = usize::try_from(U256::from_be_slice(&data[4 + 32..4 + 64])).ok()?;
    let len = usize::try_from(U256::from_be_slice(&data[4 + 64..4 + 96])).ok()?;
    // The bytes offset is relative to the tuple head and the encoder emits
    // it right after the two head words; anything else is malformed.
    if offset != 64 {
        return None;
    }
    let start = 4usize.checked_add(offset)?.checked_add(32)?;
    let end = start.checked_add(len)?;
    if end > data.len() {
        return None;
    }
    Some(data[start..end].to_vec())
}

impl AnvilSim {
    /// Start a local anvil forked at `block` on the configured primary port.
    pub async fn spawn(
        cfg: Arc<Config>,
        block: u64,
        risk: crate::risk::RuntimeRisk,
    ) -> Result<Self> {
        let port = cfg.sim.anvil_port;
        Self::spawn_on(cfg, block, port, risk).await
    }

    /// Start a local anvil forked at `block` on an explicit port.
    ///
    /// A second instance on a second port is what keeps replay work off the
    /// live fork: the two pin to different heights and would otherwise reset
    /// each other on every alternating simulation.
    pub async fn spawn_on(
        cfg: Arc<Config>,
        block: u64,
        port: u16,
        risk: crate::risk::RuntimeRisk,
    ) -> Result<Self> {
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

        let child = cmd.spawn().with_context(|| {
            format!(
                "failed to spawn `{}` — is Foundry installed?",
                cfg.sim.anvil_bin
            )
        })?;

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
            block_gas_limit: Mutex::new(0),
            clamp_warned: std::sync::atomic::AtomicBool::new(false),
            risk,
            lock: Mutex::new(()),
        };
        sim.prepare_state().await?;
        Ok(sim)
    }

    /// Attach to an already-running anvil (useful in CI / docker-compose).
    pub async fn attach(
        cfg: Arc<Config>,
        url: &str,
        risk: crate::risk::RuntimeRisk,
    ) -> Result<Self> {
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
            block_gas_limit: Mutex::new(0),
            clamp_warned: std::sync::atomic::AtomicBool::new(false),
            risk,
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
                    json!([
                        format!("{:?}", self.executor),
                        format!("0x{}", code.trim_start_matches("0x"))
                    ]),
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
        // WETH inventory for the fixture executor. Sandwich fronts, JIT
        // mint callbacks and sniper buys all *spend* WETH the batch must
        // already hold — those strategies are deliberately not flash-funded
        // — and WETH9's `transfer` is a bare `require(balanceOf >= wad)`,
        // so an unfunded executor shows up as exactly
        // `CallFailed(index=0): the leg reverted bare`. A real
        // `EXECUTOR_ADDRESS` is NOT topped up: its forked balance is the
        // honest picture of live readiness (fund it for real before live
        // sandwich/JIT — see docs/GO_LIVE.md).
        if self.cfg.endpoints.executor.is_none() {
            use alloy_sol_types::{sol, SolCall};
            sol! {
                interface IWETH9 {
                    function deposit() external payable;
                }
            }
            // The deposit's value plus its gas must fit the balance: bump the
            // executor first so depositing the full `big` cannot fail its
            // `gas * price + value` check (probed against a live anvil —
            // depositing exactly the funded balance is rejected).
            let _ = self
                .rpc
                .call_raw(
                    "anvil_setBalance",
                    json!([
                        format!("{:?}", self.executor),
                        format!(
                            "0x{:x}",
                            U256::from(30_000u64) * U256::from(1_000_000_000_000_000_000u128)
                        )
                    ]),
                )
                .await;
            let _ = self
                .rpc
                .call_raw(
                    "eth_sendTransaction",
                    json!([{
                        "from": format!("{:?}", self.executor),
                        "to": format!("{:?}", self.cfg.chain.weth),
                        "value": big,
                        "data": format!("0x{}", hex::encode(IWETH9::depositCall {}.abi_encode())),
                    }]),
                )
                .await;
        }
        self.refresh_block_gas_limit().await;
        Ok(())
    }

    /// Read the fork's block gas limit and warn when the configured bundle
    /// gas cannot fit inside it. An executor tx with `gas > block gas
    /// limit` is rejected by anvil before it ever runs — surfaced as the
    /// cryptic `intrinsic gas too high -- tx.gas_limit > env.block.gas_limit`
    /// on **every** simulation — so this is checked once per (re)fork rather
    /// than discovered one back-run at a time.
    async fn refresh_block_gas_limit(&self) {
        let limit = self
            .rpc
            .call_raw("eth_getBlockByNumber", json!(["latest", false]))
            .await
            .ok()
            .and_then(|b| b.get("gasLimit").cloned())
            .map(|g| parse_u64(&g))
            .unwrap_or(0);
        *self.block_gas_limit.lock().await = limit;
        let configured = self.risk.risk().max_gas_per_bundle;
        if limit > 0 && configured > limit.saturating_sub(limit / 20) {
            tracing::warn!(
                target: "sim",
                configured = configured,
                fork_limit = limit,
                "MAX_GAS_PER_BUNDLE is at/above the fork's block gas limit — executor txs are clamped to 95% of the limit; lower MAX_GAS_PER_BUNDLE if you did not intend this"
            );
        }
    }

    /// Gas for an injected executor tx: the configured bundle gas, clamped to
    /// 95% of the fork's block gas limit so the tx is always admissible (and
    /// leaves headroom for the victim txs sharing the same mined block).
    async fn executor_gas_limit(&self) -> u64 {
        let limit = *self.block_gas_limit.lock().await;
        clamp_gas(self.cfg.risk.max_gas_per_bundle, limit)
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
        self.simulate_locked(opp, victims_raw, victim_sender_nonce, base_fee)
            .await
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
        self.simulate_locked(opp, victims_raw, victim_sender_nonce, base_fee)
            .await
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
        let mut sent: Vec<SentTx> = Vec::new();

        // 1. front-run
        if !opp.front_calls.is_empty() {
            match self
                .send_executor_tx(opp, &opp.front_calls, base_fee, true)
                .await
            {
                Ok((h, replay)) => {
                    tx_hashes.push(h);
                    sent.push(SentTx {
                        label: "front-run",
                        replay: Some(replay),
                    });
                }
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
                let _ = self
                    .rpc
                    .call_raw(
                        "anvil_setNonce",
                        json!([format!("{sender:?}"), format!("0x{nonce:x}")]),
                    )
                    .await;
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
                            sent.push(SentTx {
                                label: "victim",
                                replay: None,
                            });
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
            match self
                .send_executor_tx(opp, &opp.back_calls, base_fee, false)
                .await
            {
                Ok((h, replay)) => {
                    tx_hashes.push(h);
                    sent.push(SentTx {
                        label: "back-run",
                        replay: Some(replay),
                    });
                }
                Err(e) => {
                    ok = false;
                    revert_reason = Some(format!("back-run rejected: {e}"));
                }
            }
        }

        let _ = self.rpc.call_raw("evm_mine", json!([])).await;
        let _ = self.rpc.call_raw("evm_setAutomine", json!([true])).await;

        // Collect receipts. Status-0 txs are re-executed via eth_call against
        // the still-live fork state (we are inside the snapshot) so the revert
        // data can be decoded — "reverted" without a reason is untriageable.
        for (h, what) in tx_hashes.iter().zip(&sent) {
            let receipt: Value = self
                .rpc
                .call_raw("eth_getTransactionReceipt", json!([h]))
                .await
                .unwrap_or(Value::Null);
            if receipt.is_null() {
                ok = false;
                revert_reason.get_or_insert_with(|| format!("{} {h} was not mined", what.label));
                continue;
            }
            gas_used += parse_u64(&receipt["gasUsed"]);
            if parse_u64(&receipt["status"]) != 1 {
                ok = false;
                let reason = match &what.replay {
                    Some(replay) => self.revert_reason_of(replay).await,
                    None => format!(
                        "victim {h} reverted — the target's own protection fired (slippage / pause / guard); the bundle is invalid by design"
                    ),
                };
                revert_reason.get_or_insert_with(|| format!("{} reverted: {reason}", what.label));
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
    ) -> Result<(String, Value)> {
        let risk = self.risk.risk();
        let data = crate::bundle::encode_execute(opp, calls, front, &risk);
        let gas = self.executor_gas_limit().await;
        if gas < risk.max_gas_per_bundle
            && !self
                .clamp_warned
                .swap(true, std::sync::atomic::Ordering::Relaxed)
        {
            tracing::warn!(
                target: "sim",
                configured = self.cfg.risk.max_gas_per_bundle,
                clamped_to = gas,
                "clamping executor tx gas below the fork's block gas limit (see MAX_GAS_PER_BUNDLE)"
            );
        }
        let tx = json!([{
            "from": format!("{:?}", self.searcher),
            "to": format!("{:?}", self.executor),
            "data": format!("0x{}", hex::encode(data)),
            "gas": format!("0x{gas:x}"),
            "maxFeePerGas": format!("0x{:x}", base_fee * U256::from(2u8) + U256::from(2_000_000_000u64)),
            "maxPriorityFeePerGas": format!("0x{:x}", 1_000_000_000u64),
        }]);
        let replay = tx[0].clone();
        let h: Value = self.rpc.call_raw("eth_sendTransaction", tx).await?;
        h.as_str()
            .map(|s| (s.to_string(), replay))
            .ok_or_else(|| anyhow!("eth_sendTransaction returned no hash"))
    }

    async fn balance_of(&self, token: Address) -> Result<U256> {
        if token == Address::ZERO {
            let v = self
                .rpc
                .call_raw(
                    "eth_getBalance",
                    json!([format!("{:?}", self.executor), "latest"]),
                )
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

impl AnvilSim {
    /// Re-execute an injected tx via `eth_call` against the current fork
    /// state and decode the revert. Called while the simulation snapshot is
    /// still live: the reverted tx's own state changes were discarded, and
    /// everything before it (the victim) is in place, so the call reverts
    /// the same way it did when it was mined.
    async fn revert_reason_of(&self, call: &Value) -> String {
        match self
            .rpc
            .call_raw_with_error("eth_call", json!([call, "latest"]))
            .await
        {
            // A revert comes back as the JSON-RPC error object; anvil puts
            // the raw revert bytes in `data` (and any Error(string) text in
            // `message`).
            Err(err) => {
                let data = err.get("data").and_then(|d| d.as_str()).unwrap_or("");
                let msg = err.get("message").and_then(|m| m.as_str()).unwrap_or("");
                if let Ok(bytes) = hex::decode(data.trim_start_matches("0x")) {
                    if bytes.len() >= 4 {
                        return decode_revert_data(&bytes);
                    }
                }
                match msg.strip_prefix("execution reverted") {
                    Some(rest) => {
                        let text = rest.trim_start_matches(':').trim();
                        if text.is_empty() {
                            "reverted (no data)".to_string()
                        } else {
                            text.to_string()
                        }
                    }
                    None => "reverted (no data)".to_string(),
                }
            }
            // The re-execution succeeded: the revert depended on tx-level
            // context we cannot reproduce with eth_call (nonce-driven
            // guards, block properties). Rare; the label stays generic.
            Ok(_) => "reverted (context-dependent; not reproducible via eth_call)".to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_sol_types::{sol, SolError};

    #[test]
    fn clamp_leaves_sane_configurations_alone() {
        // Default config on today's 60M mainnet limit: untouched.
        assert_eq!(clamp_gas(3_000_000, 60_000_000), 3_000_000);
        // Unknown limit (0): untouched — the live fork reads it right after
        // spawning, so 0 only ever means "not yet measured".
        assert_eq!(clamp_gas(3_000_000, 0), 3_000_000);
    }

    #[test]
    fn clamp_caps_above_limit_configurations() {
        // The reproduced failure: configured > fork limit. 95% of the limit,
        // leaving headroom for the victim txs in the same mined block.
        assert_eq!(clamp_gas(70_000_000, 60_000_000), 57_000_000);
        // An attached anvil started with a small --gas-limit.
        assert_eq!(clamp_gas(3_000_000, 2_000_000), 1_900_000);
        // Absurd configuration on an old 30M-limit fork.
        assert_eq!(clamp_gas(300_000_000, 30_000_000), 28_500_000);
    }

    #[test]
    fn clamp_never_goes_below_intrinsic_floor() {
        assert!(clamp_gas(u64::MAX, 30_000_000) < 30_000_000);
        assert!(clamp_gas(u64::MAX, 100_000) >= 21_000);
    }

    #[test]
    fn decodes_solidity_error_string() {
        // Error("nope") — selector + abi-encoded (offset, len, bytes).
        let mut data = vec![0x08, 0xc3, 0x79, 0xa0];
        data.extend_from_slice(&[0u8; 32]); // string offset (unused by decoder)
        let mut len = [0u8; 32];
        len[31] = 4;
        data.extend_from_slice(&len);
        data.extend_from_slice(b"nope");
        assert_eq!(decode_revert_data(&data), "Error(\"nope\")");
    }

    #[test]
    fn decodes_panic_with_code() {
        let mut data = vec![0x4e, 0x48, 0x7b, 0x71];
        let mut code = [0u8; 32];
        code[31] = 0x11; // arithmetic overflow
        data.extend_from_slice(&code);
        assert_eq!(decode_revert_data(&data), "Panic(17)");
    }

    #[test]
    fn decodes_the_executor_profit_guard() {
        sol! {
            interface E {
                error Unprofitable(uint256 realised, uint256 required);
            }
        }
        let data = E::Unprofitable {
            realised: U256::from(5u8),
            required: U256::from(1u8),
        }
        .abi_encode();
        let text = decode_revert_data(&data);
        assert!(
            text.contains("Unprofitable(realised=5, required=1)"),
            "{text}"
        );
        assert!(text.contains("profit guard"), "{text}");
    }

    #[test]
    fn decodes_known_protocol_rejections_by_selector() {
        sol! {
            interface E {
                error HEALTHY_POSITION();
                error NotForSale();
            }
        }
        let text = decode_revert_data(&E::HEALTHY_POSITION {}.abi_encode());
        assert!(text.contains("HEALTHY_POSITION"), "{text}");
        let text = decode_revert_data(&E::NotForSale {}.abi_encode());
        assert!(text.contains("NotForSale"), "{text}");
    }

    #[test]
    fn unknown_selectors_show_as_custom_with_args() {
        let mut data = vec![0xde, 0xad, 0xbe, 0xef];
        let mut word = [0u8; 32];
        word[31] = 7;
        data.extend_from_slice(&word);
        let text = decode_revert_data(&data);
        assert!(text.starts_with("custom error 0xdeadbeef"), "{text}");
        assert!(text.contains("(7)"), "{text}");
    }

    /// `Error("UniswapV2: K")` — the classic constant-product sentinel.
    fn error_string(message: &str) -> Vec<u8> {
        use alloy_sol_types::SolType;
        let mut out = vec![0x08, 0xc3, 0x79, 0xa0];
        out.extend_from_slice(&[0u8; 32]); // string offset
        let mut len = [0u8; 32];
        len[24..].copy_from_slice(&(message.len() as u64).to_be_bytes());
        out.extend_from_slice(&len);
        out.extend_from_slice(message.as_bytes());
        let _ = alloy_sol_types::sol_data::String::abi_encode_packed(&message.to_string());
        out
    }

    #[test]
    fn decodes_callfailed_with_inner_error_string() {
        use alloy_sol_types::{sol, SolError};
        sol! {
            interface E {
                error CallFailed(uint256 index, bytes returndata);
            }
        }
        let data = E::CallFailed {
            index: U256::ZERO,
            returndata: alloy_primitives::Bytes::from(error_string("UniswapV2: K")),
        }
        .abi_encode();
        let text = decode_revert_data(&data);
        assert!(text.contains("CallFailed(index=0)"), "{text}");
        assert!(text.contains("UniswapV2: K"), "{text}");
    }

    #[test]
    fn decodes_callfailed_with_inner_custom_error() {
        use alloy_sol_types::{sol, SolError};
        sol! {
            interface E {
                error CallFailed(uint256 index, bytes returndata);
                error HEALTHY_POSITION();
            }
        }
        let data = E::CallFailed {
            index: U256::from(2u8),
            returndata: alloy_primitives::Bytes::from(E::HEALTHY_POSITION {}.abi_encode()),
        }
        .abi_encode();
        let text = decode_revert_data(&data);
        assert!(text.contains("CallFailed(index=2)"), "{text}");
        assert!(text.contains("HEALTHY_POSITION"), "{text}");
    }

    #[test]
    fn decodes_callfailed_with_bare_and_short_inner_reverts() {
        use alloy_sol_types::{sol, SolError};
        sol! {
            interface E {
                error CallFailed(uint256 index, bytes returndata);
            }
        }
        let bare = E::CallFailed {
            index: U256::ZERO,
            returndata: alloy_primitives::Bytes::new(),
        }
        .abi_encode();
        let text = decode_revert_data(&bare);
        assert!(text.contains("bare"), "{text}");

        let short = E::CallFailed {
            index: U256::from(1u8),
            returndata: alloy_primitives::Bytes::from(vec![0xde, 0xad]),
        }
        .abi_encode();
        let text = decode_revert_data(&short);
        assert!(text.contains("raw 0xdead"), "{text}");
    }

    #[test]
    fn callfailed_with_malformed_inner_bytes_never_panics() {
        // Right selector, truncated/trash tail — the decoder must produce a
        // string, not a panic, whatever a hostile contract returns.
        let mut data = vec![0x5c, 0x0d, 0xee, 0x00]; // wrong-case selector is fine, ignored
        data.extend_from_slice(&[0u8; 128]);
        let mut bogus_offset = [0u8; 32];
        bogus_offset[24..].copy_from_slice(&999u64.to_be_bytes());
        data[4 + 32..4 + 64].copy_from_slice(&bogus_offset);
        let _ = decode_revert_data(&data); // must not panic
                                           // Exact selector with a length that overruns the payload.
        use alloy_sol_types::{sol, SolError};
        sol! {
            interface E {
                error CallFailed(uint256 index, bytes returndata);
            }
        }
        let mut overrun = E::CallFailed {
            index: U256::ZERO,
            returndata: alloy_primitives::Bytes::new(),
        }
        .abi_encode();
        // length word sits at head offset 64 + 4: blow it up
        let len_pos = 4 + 64;
        let mut bogus_len = [0u8; 32];
        bogus_len[24..].copy_from_slice(&(1u64 << 40).to_be_bytes());
        overrun[len_pos..len_pos + 32].copy_from_slice(&bogus_len);
        let text = decode_revert_data(&overrun);
        assert!(text.contains("CallFailed"), "{text}");
    }

    #[test]
    fn short_or_empty_data_is_reported_not_panicked() {
        assert_eq!(decode_revert_data(&[]), "no revert data");
        assert_eq!(decode_revert_data(&[0x08]), "no revert data");
    }
}
