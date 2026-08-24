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
use crate::types::{now_ms, parse_u64, Opportunity, SimBackend, SimulationResult};

/// Runtime bytecode of `MevExecutor`, produced by `contracts/script/compile-check.js`.
/// Injected into the fork with `anvil_setCode` so simulation works before the
/// contract is deployed anywhere.
pub const EXECUTOR_RUNTIME_BYTECODE: &str = include_str!("../../artifacts/MevExecutor.runtime.hex");
/// Compiler-emitted immutable byte ranges. They let the simulator patch the
/// runtime fixture with constructor-equivalent values without mining setup
/// transactions that would move the fork past the target block.
pub const EXECUTOR_IMMUTABLE_REFS: &str =
    include_str!("../../artifacts/MevExecutor.immutables.json");

/// Address where the constructor-equivalent fixture is mounted.
pub const SIM_EXECUTOR: Address =
    alloy_primitives::address!("00000000000000000000000000000000000e0000");
const EXECUTED_TOPIC: &str = "0x920d3a9c5eb5759e8895809a65dae03c9336ebf6f554de8cdc90e3bcb4404121";

/// Ceiling for the *signed* bundle transactions (the relay `eth_callBundle`
/// path, which has no fork to read a live limit from). Since Fusaka
/// (2025-12-03), EIP-7825 enforces a protocol-level cap of 16,777,216 (2^24)
/// gas on *any single transaction*: a tx signed above it is invalid no
/// matter what the block gas limit is (60M since EIP-7935, also Fusaka).
/// Clamping here keeps a misconfigured `MAX_GAS_PER_BUNDLE` from ever
/// signing a bundle that every builder must reject before it runs.
pub const MAX_TX_GAS_CEILING: u64 = 16_777_216;

pub struct AnvilSim {
    cfg: Arc<Config>,
    rpc: RpcClient,
    child: Mutex<Option<tokio::process::Child>>,
    /// Block the fork is currently pinned to.
    forked_at: Mutex<u64>,
    executor: parking_lot::RwLock<Address>,
    searcher: Address,
    /// Serialises access: one simulation at a time per fork. Shared with the
    /// sniper simulation fixture so its fixture transactions can never land
    /// inside a bundle replay's snapshot/revert window (and never race the
    /// automine-off phase).
    lock: Arc<Mutex<()>>,
    /// Block-pinned prices for non-native profit tokens. Keyed by
    /// `(token, block)` so a quote can never outlive its block.
    valuation: crate::valuation::ValuationCache,
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
            error SweepFailed();
            error BribeFailed();
            error TransferFailed(address token, address to, uint256 amount);
            error QuoteIsEthCallOnly();
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
            .as_chunks::<32>()
            .0
            .iter()
            .map(|c| U256::from_be_slice(c))
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
            MevExecutorErrors::SweepFailed::SELECTOR,
            "SweepFailed() (the ETH sweep recipient rejected the transfer)",
        ),
        (
            MevExecutorErrors::BribeFailed::SELECTOR,
            "BribeFailed() (block.coinbase rejected the bribe transfer)",
        ),
        (
            MevExecutorErrors::QuoteIsEthCallOnly::SELECTOR,
            "QuoteIsEthCallOnly() (call quote() with no `from`, or use quoteFrom())",
        ),
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
    if selector == MevExecutorErrors::TransferFailed::SELECTOR {
        // Same word-decoding style as `Unprofitable` above: the three args are
        // (address token, address to, uint256 amount), each one word wide.
        let w = words(0);
        let as_addr = |v: Option<U256>| {
            v.map(|x| format!("0x{}", hex::encode(&x.to_be_bytes::<32>()[12..])))
                .unwrap_or_else(|| "?".to_string())
        };
        return format!(
            "TransferFailed(token={}, to={}, amount={}) — the ERC20 returned false or reverted",
            as_addr(w.first().copied()),
            as_addr(w.get(1).copied()),
            w.get(2).copied().unwrap_or_default()
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
            executor: parking_lot::RwLock::new(executor),
            lock: Arc::new(Mutex::new(())),
            valuation: crate::valuation::ValuationCache::new(),
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
            executor: parking_lot::RwLock::new(executor),
            lock: Arc::new(Mutex::new(())),
            valuation: crate::valuation::ValuationCache::new(),
        };
        sim.prepare_state().await?;
        Ok(sim)
    }

    pub fn executor(&self) -> Address {
        *self.executor.read()
    }

    /// The fork's serialization lock, shared with the sniper simulation
    /// fixture. A fixture transaction holding this lock cannot interleave
    /// with a bundle simulation's snapshot/mine/revert cycle, and vice versa.
    pub fn sim_lock(&self) -> Arc<Mutex<()>> {
        self.lock.clone()
    }

    /// The fork's RPC transport. The sniper simulation fixture reuses the
    /// same anvil process rather than spawning a fork per click.
    pub fn rpc(&self) -> &RpcClient {
        &self.rpc
    }

    /// Install the executor bytecode, make the searcher the owner and give both
    /// accounts a spending balance inside the fork.
    async fn prepare_state(&self) -> Result<()> {
        if let Some(executor) = self.cfg.endpoints.executor {
            *self.executor.write() = executor;
            return Ok(());
        }
        let runtime = executor_fixture_runtime(&self.cfg)?;
        self.rpc
            .call_raw(
                "anvil_setCode",
                json!([
                    format!("{SIM_EXECUTOR:?}"),
                    format!("0x{}", hex::encode(runtime))
                ]),
            )
            .await
            .context("install constructor-equivalent executor fixture")?;
        *self.executor.write() = SIM_EXECUTOR;

        // Storage slot 0 is owner. The owner is also accepted by
        // `onlySearcher`, so no mapping slot needs to be fabricated.
        self.rpc
            .call_raw(
                "anvil_setStorageAt",
                json!([
                    format!("{SIM_EXECUTOR:?}"),
                    "0x0",
                    format!("0x{:064x}", U256::from_be_slice(self.searcher.as_slice()))
                ]),
            )
            .await
            .context("set fixture owner/searcher")?;

        let rich = U256::from(30_000u64) * U256::from(1_000_000_000_000_000_000u128);
        for who in [self.searcher, SIM_EXECUTOR] {
            self.rpc
                .call_raw(
                    "anvil_setBalance",
                    json!([format!("{who:?}"), format!("0x{rich:x}")]),
                )
                .await
                .with_context(|| format!("fund fixture account {who:?}"))?;
        }

        // Mainnet WETH9's `balanceOf` mapping is storage slot 3. Setting
        // the fixture executor's entry directly avoids mining a setup
        // deposit and therefore keeps the fork pinned to the exact parent
        // block the opportunity targets.
        let mut key = [0u8; 64];
        key[12..32].copy_from_slice(SIM_EXECUTOR.as_slice());
        key[63] = 3;
        let slot = alloy_primitives::keccak256(key);
        let weth_inventory = U256::from(10_000u64) * U256::from(1_000_000_000_000_000_000u128);
        self.rpc
            .call_raw(
                "anvil_setStorageAt",
                json!([
                    format!("{:?}", self.cfg.chain.weth),
                    format!("{slot:?}"),
                    format!("0x{weth_inventory:064x}")
                ]),
            )
            .await
            .context("fund fixture executor with WETH")?;
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
        let _guard = self.lock.lock().await;
        self.ensure_fork_at_locked(block).await
    }

    async fn ensure_fork_at_locked(&self, block: u64) -> Result<()> {
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

    /// Execute the exact signed bytes that would be sent to a relay. The old
    /// simulator independently rebuilt impersonated transactions, which could
    /// pass locally while the signed payload failed nonce, sender or allowlist
    /// checks at the relay.
    pub async fn simulate_at_head(
        &self,
        block: u64,
        opp: &Opportunity,
        bundle: &crate::types::BundleRecord,
        victim_sender_nonce: Option<(Address, u64)>,
    ) -> Result<SimulationResult> {
        let _guard = self.lock.lock().await;
        self.ensure_fork_at_locked(block).await?;
        self.simulate_locked(opp, bundle, victim_sender_nonce).await
    }

    /// Pin and simulate while holding the same mutex. This prevents a reset
    /// from invalidating the snapshot between pinning and execution.
    pub async fn simulate_at(
        &self,
        block: u64,
        opp: &Opportunity,
        bundle: &crate::types::BundleRecord,
        victim_sender_nonce: Option<(Address, u64)>,
    ) -> Result<SimulationResult> {
        let _guard = self.lock.lock().await;
        self.ensure_fork_exact_locked(block).await?;
        self.simulate_locked(opp, bundle, victim_sender_nonce).await
    }

    async fn simulate_locked(
        &self,
        opp: &Opportunity,
        bundle: &crate::types::BundleRecord,
        victim_sender_nonce: Option<(Address, u64)>,
    ) -> Result<SimulationResult> {
        let started = Instant::now();
        let snapshot: Value = self.rpc.call_raw("evm_snapshot", json!([])).await?;
        let result = self
            .simulate_inner(opp, bundle, victim_sender_nonce, started)
            .await;
        // Always roll the fork back, even if the simulation blew up.
        let _ = self.rpc.call_raw("evm_revert", json!([snapshot])).await;
        result
    }

    async fn simulate_inner(
        &self,
        opp: &Opportunity,
        bundle: &crate::types::BundleRecord,
        victim_sender_nonce: Option<(Address, u64)>,
        started: Instant,
    ) -> Result<SimulationResult> {
        let _ = self.rpc.call_raw("evm_setAutomine", json!([false])).await;
        let before = self.balance_of(opp.profit_token).await?;
        // The victim's own balance, measured when the bundle actually
        // replays a foreign transaction: this is the fork's prediction of
        // what the victim's trade does to their `profit_token` balance.
        let victim = match victim_sender_nonce {
            Some((sender, _)) if bundle.txs.iter().any(|t| t.foreign) => {
                Some(self.balance_of_account(opp.profit_token, sender).await?)
            }
            _ => None,
        };
        let mut ok = true;
        let mut revert_reason: Option<String> = None;
        let mut sent: Vec<(String, bool)> = Vec::with_capacity(bundle.txs.len());

        // A pending transaction can race the fork head. Restoring the nonce it
        // carried when observed is fork-local and keeps verbatim replay valid.
        if let Some((sender, nonce)) = victim_sender_nonce {
            let _ = self
                .rpc
                .call_raw(
                    "anvil_setNonce",
                    json!([format!("{sender:?}"), format!("0x{nonce:x}")]),
                )
                .await;
        }

        // Same restore for *our* legs. Inventory starts at 0 and is only
        // advanced by a successful `eth_getTransactionCount`; a used searcher
        // key, a failed refresh, or a previous sim whose `evm_revert` did not
        // land all produce "searcher tx 0 rejected: nonce too low" against
        // the fork's real nonce. The signed bytes are the source of truth —
        // pin the fork to them, exactly as we do for the victim.
        if let Some(nonce) = bundle
            .txs
            .iter()
            .find(|tx| !tx.foreign)
            .and_then(|tx| crate::rlp::decode_eip1559_nonce(&tx.raw))
        {
            let _ = self
                .rpc
                .call_raw(
                    "anvil_setNonce",
                    json!([format!("{:?}", self.searcher), format!("0x{nonce:x}")]),
                )
                .await;
        }

        for (index, tx) in bundle.txs.iter().enumerate() {
            let raw = format!("0x{}", hex::encode(&tx.raw));
            match self
                .rpc
                .call_raw("eth_sendRawTransaction", json!([raw]))
                .await
            {
                Ok(hash) => match hash.as_str() {
                    Some(hash) => sent.push((hash.to_string(), tx.foreign)),
                    None => {
                        ok = false;
                        revert_reason = Some(format!("bundle tx {index} returned no hash"));
                        break;
                    }
                },
                Err(error) => {
                    ok = false;
                    revert_reason = Some(format!(
                        "{} tx {index} rejected before mining: {error}",
                        if tx.foreign { "victim" } else { "searcher" }
                    ));
                    break;
                }
            }
        }

        let _ = self.rpc.call_raw("evm_mine", json!([])).await;
        let _ = self.rpc.call_raw("evm_setAutomine", json!([true])).await;

        let mut gas_used = 0u64;
        let mut gas_cost = U256::ZERO;
        let mut event_gross = U256::ZERO;
        let mut event_bribe = U256::ZERO;
        for (hash, foreign) in &sent {
            let receipt = self
                .rpc
                .call_raw("eth_getTransactionReceipt", json!([hash]))
                .await
                .unwrap_or(Value::Null);
            if receipt.is_null() {
                ok = false;
                revert_reason.get_or_insert_with(|| format!("tx {hash} was not mined"));
                continue;
            }
            let used = parse_u64(&receipt["gasUsed"]);
            if !*foreign {
                let price = crate::types::parse_u256(&receipt["effectiveGasPrice"]);
                gas_used = gas_used.saturating_add(used);
                gas_cost = gas_cost.saturating_add(price.saturating_mul(U256::from(used)));
                if let Some(logs) = receipt.get("logs").and_then(Value::as_array) {
                    for log in logs {
                        let Some(address) =
                            log.get("address").and_then(crate::types::parse_address)
                        else {
                            continue;
                        };
                        if address != self.executor() {
                            continue;
                        }
                        let topics = log.get("topics").and_then(Value::as_array);
                        let is_executed = topics
                            .and_then(|v| v.first())
                            .and_then(Value::as_str)
                            .map(|t| t.eq_ignore_ascii_case(EXECUTED_TOPIC))
                            .unwrap_or(false);
                        if !is_executed {
                            continue;
                        }
                        let data = log
                            .get("data")
                            .map(crate::types::parse_bytes)
                            .unwrap_or_default();
                        if data.len() >= 64 {
                            event_gross =
                                event_gross.saturating_add(U256::from_be_slice(&data[0..32]));
                            event_bribe =
                                event_bribe.saturating_add(U256::from_be_slice(&data[32..64]));
                        }
                    }
                }
            }
            if parse_u64(&receipt["status"]) != 1 {
                ok = false;
                revert_reason.get_or_insert_with(|| {
                    format!(
                        "{} {hash} reverted while executing the exact signed payload",
                        if *foreign { "victim" } else { "searcher" }
                    )
                });
            }
        }

        let after = self.balance_of(opp.profit_token).await?;
        let victim_after = match victim {
            Some(before) => {
                let (sender, _) = victim_sender_nonce.unwrap();
                Some((
                    before,
                    self.balance_of_account(opp.profit_token, sender).await?,
                ))
            }
            None => None,
        };
        let victim_predicted = match victim_after {
            Some((before, after)) => {
                // Signed fork-predicted victim delta (negative when the
                // victim spent the profit token). Wrapping is safe: both
                // sides are bounded by total supply, far inside i128.
                let delta = to_i128(after).wrapping_sub(to_i128(before));
                Some(delta.to_string())
            }
            None => None,
        };
        let retained = after.saturating_sub(before);
        let gross = if event_gross.is_zero() {
            retained.saturating_add(event_bribe)
        } else {
            event_gross
        };
        let gas_price = if gas_used == 0 {
            U256::ZERO
        } else {
            gas_cost / U256::from(gas_used)
        };

        // Gas is ETH-denominated, profit is denominated in `profit_token`.
        // Netting one against the other needs a price, and the price has to be
        // read at the block the profit was measured at.
        //
        // Native and wrapped-native need no conversion. Everything else is
        // priced by `crate::valuation` against the fork itself, at the block
        // the fork is pinned to — i.e. *pre-bundle* state, so our own
        // transactions cannot move the price they are then valued at. When
        // valuation is disabled or no route exists, this falls back to exactly
        // the previous conservative behaviour: uncertified, not submittable.
        let native_accounting =
            opp.profit_token == Address::ZERO || opp.profit_token == self.cfg.chain.weth;
        let mut certified = native_accounting;
        let net = if native_accounting {
            to_i128(retained) - to_i128(gas_cost)
        } else if self.cfg.token_valuation {
            let block = *self.forked_at.lock().await;
            match crate::valuation::value_in_native(
                &self.rpc,
                &self.cfg,
                &self.valuation,
                opp.profit_token,
                retained,
                block,
                self.cfg.valuation_haircut_bps,
            )
            .await
            {
                Some(v) => {
                    certified = true;
                    to_i128(v.wei) - to_i128(gas_cost)
                }
                None => {
                    ok = false;
                    revert_reason.get_or_insert_with(|| {
                        format!(
                            "uncertified accounting: no block-pinned route from {:?} to WETH at block {block}",
                            opp.profit_token
                        )
                    });
                    0
                }
            }
        } else {
            ok = false;
            revert_reason.get_or_insert_with(|| {
                "uncertified accounting: non-WETH profit cannot be netted against ETH gas without a block-pinned valuation (set TOKEN_VALUATION=true)"
                    .to_string()
            });
            0
        };

        Ok(SimulationResult {
            opportunity_id: opp.id.clone(),
            strategy: opp.strategy,
            backend: SimBackend::AnvilFork,
            success: ok && retained > U256::ZERO && certified,
            gross_profit_wei: gross,
            gas_used,
            gas_price_wei: gas_price,
            gas_cost_wei: gas_cost,
            bribe_wei: event_bribe,
            net_profit_wei: net,
            victim_predicted_out_wei: victim_predicted,
            revert_reason,
            target_block: opp.target_block,
            sim_latency_ms: started.elapsed().as_millis() as u64,
            created_at_ms: now_ms(),
        })
    }

    async fn balance_of(&self, token: Address) -> Result<U256> {
        self.balance_of_account(token, self.executor()).await
    }

    /// `profit_token` balance of an arbitrary account in the fork. The
    /// executor's balance is the accounting source; the victim sender's
    /// balance change is the fork's prediction of the victim's own trade
    /// (the sequencer qualification backend compares it against the
    /// victim's realised delta in the canonical block).
    async fn balance_of_account(&self, token: Address, account: Address) -> Result<U256> {
        if token == Address::ZERO {
            let v = self
                .rpc
                .call_raw("eth_getBalance", json!([format!("{account:?}"), "latest"]))
                .await?;
            return Ok(crate::types::parse_u256(&v));
        }
        let data = crate::dex::IERC20::balanceOfCall { account };
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

/// The constructor-equivalent executor fixture: runtime bytecode with the
/// chain's immutable parameters (Balancer vault, WETH from the registry)
/// patched in. Shared by the anvil fork's `anvil_setCode` and the
/// state-pinned `eth_simulateV1` state override — one patch routine, so the
/// two fixtures can never drift apart.
///
/// `pub` so the env-gated live fork test patches the same runtime the
/// engine simulates with.
pub fn executor_fixture_runtime(cfg: &Config) -> Result<Vec<u8>> {
    #[derive(serde::Deserialize)]
    struct ImmutableRef {
        start: usize,
        length: usize,
    }
    let refs: std::collections::HashMap<String, Vec<ImmutableRef>> =
        serde_json::from_str(EXECUTOR_IMMUTABLE_REFS)
            .context("decode executor immutable references")?;
    let mut runtime = hex::decode(EXECUTOR_RUNTIME_BYTECODE.trim().trim_start_matches("0x"))
        .context("decode embedded executor runtime bytecode")?;
    // Immutables from the chain's registry. A chain without a Balancer
    // vault (the flash-loan funding source) gets a fixture whose
    // flashExecute path is dead — the non-flash strategies still work.
    let mut immutable_values: Vec<(&str, Address)> = Vec::new();
    if let Some(vault) = cfg.addresses.balancer_vault {
        immutable_values.push(("BALANCER_VAULT", vault));
    }
    immutable_values.push(("WETH", cfg.chain.weth));
    for (name, value) in immutable_values {
        let positions = refs
            .get(name)
            .ok_or_else(|| anyhow!("compiler artifact has no {name} immutable references"))?;
        let mut word = [0u8; 32];
        word[12..].copy_from_slice(value.as_slice());
        for position in positions {
            if position.length > 32 || position.start + position.length > runtime.len() {
                bail!("invalid immutable range for {name}");
            }
            runtime[position.start..position.start + position.length]
                .copy_from_slice(&word[32 - position.length..]);
        }
    }
    Ok(runtime)
}

pub fn to_i128(v: U256) -> i128 {
    // Values beyond i128 are nonsense for PnL purposes; saturate.
    if v > U256::from(i128::MAX as u128) {
        i128::MAX
    } else {
        v.to::<u128>() as i128
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_sol_types::{sol, SolError};

    #[test]
    fn tx_gas_ceiling_is_the_eip7825_protocol_cap() {
        // EIP-7825 (live since Fusaka, 2025-12-03): no single transaction may
        // specify more than 2^24 gas, independent of the block gas limit
        // (60M since EIP-7935). Raising this const above that cap would let
        // a misconfigured MAX_GAS_PER_BUNDLE sign bundles that are
        // protocol-invalid — rejected by every txpool, builder and relay.
        assert_eq!(MAX_TX_GAS_CEILING, 16_777_216);
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
    fn decodes_the_new_executor_custom_errors() {
        // These replaced `require("sweep failed")` / `require("bribe failed")`
        // / `require("transfer failed")` in MevExecutor. If the decoder is not
        // kept in step, the console degrades from a named reason to a raw
        // selector — which is exactly the diagnostic the funnel depends on.
        sol! {
            interface E {
                error SweepFailed();
                error BribeFailed();
                error QuoteIsEthCallOnly();
                error TransferFailed(address token, address to, uint256 amount);
            }
        }
        assert!(decode_revert_data(&E::SweepFailed {}.abi_encode()).contains("SweepFailed()"));
        assert!(decode_revert_data(&E::BribeFailed {}.abi_encode()).contains("BribeFailed()"));
        assert!(decode_revert_data(&E::QuoteIsEthCallOnly {}.abi_encode())
            .contains("QuoteIsEthCallOnly()"));

        let token = alloy_primitives::Address::with_last_byte(0xAA);
        let to = alloy_primitives::Address::with_last_byte(0xBB);
        let text = decode_revert_data(
            &E::TransferFailed {
                token,
                to,
                amount: U256::from(1_234u64),
            }
            .abi_encode(),
        );
        assert!(text.contains("TransferFailed("), "{text}");
        assert!(text.contains("amount=1234"), "{text}");
        // The address words must render as addresses, not as huge integers.
        assert!(text.to_lowercase().contains("aa"), "{text}");
        assert!(text.to_lowercase().contains("bb"), "{text}");
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
