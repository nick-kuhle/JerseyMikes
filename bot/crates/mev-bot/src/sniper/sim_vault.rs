//! The SniperVault **simulation fixture**.
//!
//! Work package A of the sim/live handoff: simulation must run the *real*
//! `SniperVault` bytecode on a local Anvil fork — not a paper stand-in. This
//! module owns that fixture's lifecycle:
//!
//! 1. Deploy the exact compiled `SniperVault.sol` creation bytecode with the
//!    chain-specific WETH and the configured simulation budget.
//! 2. Authorize a deterministic **simulation searcher** derived from the
//!    built-in simulation signer — never `SNIPER_SEARCHER_PRIVATE_KEY`.
//! 3. Fund the vault with simulated WETH (never real funds, never a real RPC).
//! 4. Deploy deterministic mock ERC-20 + V2 liquidity per simulated launch so
//!    the same `openPosition`/`closePosition` calldata the live lane signs
//!    executes against the fixture.
//!
//! Every RPC call here goes to the local fork transport. A missing or broken
//! fork is reported as an explicit blocker; the fixture never pretends that
//! contract-backed simulation succeeded when it could not run.

use alloy_primitives::{keccak256, Address, B256, U256};
use alloy_sol_types::{sol, SolCall, SolValue};
use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};

use crate::rpc::RpcClient;
use crate::signer::Signer;
use crate::types::parse_u64;

/// Creation bytecode of the real `SniperVault`, emitted by
/// `contracts/script/compile-check.js` from the same solc run the
/// artifact-drift gate checks. Deploying it runs the real constructor.
pub const SNIPER_VAULT_CREATION_HEX: &str =
    include_str!("../../artifacts/SniperVault.creation.hex");
/// Deterministic mock liquidity, test-only contracts, never for production.
pub const MOCK_ERC20_CREATION_HEX: &str = include_str!("../../artifacts/MockERC20.creation.hex");
pub const SIM_V2_PAIR_CREATION_HEX: &str = include_str!("../../artifacts/SimV2Pair.creation.hex");
pub const MOCK_WETH_CREATION_HEX: &str = include_str!("../../artifacts/MockWETH.creation.hex");

/// WETH funding for the fixture vault: enough for a long simulation session
/// without ever touching a real balance.
pub const FIXTURE_VAULT_WETH_WEI: U256 = U256::from_limbs([10_000_000_000_000_000_000, 0, 0, 0]); // 10 ETH

sol! {
    interface ISimVaultAdmin {
        struct Call {
            address target;
            uint256 value;
            bytes data;
        }
        function setSearcher(address searcher, bool allowed) external;
        function ownerCall(Call[] calldata calls) external payable returns (bytes[] memory);
        function WETH() external view returns (address);
        function owner() external view returns (address);
        function dailyBudget() external view returns (uint256);
        function totalBudget() external view returns (uint256);
        function spendableRemaining() external view returns (uint256);
        function searchers(address who) external view returns (bool);
    }

    interface IWETH9 {
        function deposit() external payable;
        function transfer(address to, uint256 value) external returns (bool);
        function balanceOf(address who) external view returns (uint256);
    }

    interface IMockERC20 {
        function mint(address to, uint256 amount) external;
        function setBlockedSender(address from, bool blocked) external;
        function balanceOf(address who) external view returns (uint256);
    }

    interface IMockWETH {
        function mint(address to, uint256 amount) external;
    }

    interface ISimV2Pair {
        function sync() external;
        function getReserves() external view returns (uint112 reserve0, uint112 reserve1, uint32 ts);
        function token0() external view returns (address);
        function token1() external view returns (address);
    }

    // Constructor argument tuples, ABI-encoded exactly like Solidity's
    // `new C(a, b, ...)` appends them to the creation bytecode.
    struct VaultCtorArgs {
        address weth;
        uint256 dailyBudget;
        uint256 totalBudget;
    }
    struct TokenCtorArgs {
        string name;
        string symbol;
    }
    struct PairCtorArgs {
        address t0;
        address t1;
    }

    interface SniperVaultErrors {
        error NotOwner();
        error NotSearcher();
        error Reentrancy();
        error Deadline();
        error BaseFeeTooHigh();
        error CallFailed(uint256 index, bytes returndata);
        error SweepFailed();
        error TransferFailed();
        error ZeroToken();
        error OverSpend(uint256 spent, uint256 allowed);
        error OverSell(uint256 sold, uint256 allowed);
        error InsufficientTokens(uint256 received, uint256 required);
        error InsufficientWeth(uint256 received, uint256 required);
        error DailyBudgetExceeded(uint256 spent, uint256 budget);
        error TotalBudgetExceeded(uint256 spent, uint256 budget);
    }
}

/// Deterministic simulation identities.
///
/// The owner **is** the built-in simulation signer; the searcher is derived
/// from it by hashing in a domain tag, so both are stable across restarts
/// and neither has anything to do with `SNIPER_SEARCHER_PRIVATE_KEY`.
pub fn sim_owner_address() -> Address {
    Signer::simulation().address()
}

pub fn sim_searcher_signer() -> Signer {
    let mut seed = Vec::with_capacity(64);
    seed.extend_from_slice(
        &hex::decode(Signer::SIMULATION_KEY.trim_start_matches("0x"))
            .expect("the built-in simulation key is valid hex"),
    );
    seed.extend_from_slice(b"jerseymikes/sniper-sim-searcher");
    let key = keccak256(&seed);
    Signer::from_hex(&format!("0x{}", hex::encode(key)))
        .expect("a keccak256 digest is always a valid secp256k1 scalar")
}

/// Decode a revert surfaced by the fork into a human-readable guard name.
/// The fixture's whole value is that a blocked entry says *which* contract
/// guard blocked it, so the operator (and the paper ledger's notes column)
/// sees `DailyBudgetExceeded(...)` rather than a hex blob.
pub fn decode_vault_revert(err: &anyhow::Error) -> String {
    let msg = err.to_string();
    // Best source: the JSON-RPC error's `data` field carries the full revert
    // bytes. Prefer it explicitly, then fall back to the longest hex run at
    // any `0x` — anvil also repeats the selector inside the human-readable
    // `message`, and that short copy must never win over the real payload.
    let mut candidates: Vec<Vec<u8>> = Vec::new();
    if let Some(start) = msg.find("\"data\":\"0x") {
        let hexish: String = msg[start + 9..]
            .chars()
            .skip(1)
            .take_while(|c| c.is_ascii_hexdigit())
            .collect();
        if let Ok(bytes) = hex::decode(&hexish) {
            candidates.push(bytes);
        }
    }
    let mut search_from = 0;
    while let Some(pos) = msg[search_from..].find("0x") {
        let abs = search_from + pos;
        let hexish: String = msg[abs..]
            .chars()
            .take_while(|c| c.is_ascii_hexdigit())
            .collect();
        if hexish.len() >= 8 {
            if let Ok(bytes) = hex::decode(&hexish) {
                candidates.push(bytes);
            }
        }
        search_from = abs + 2;
    }
    // Longest payload first: the full revert data beats a bare selector.
    candidates.sort_by_key(|b| std::cmp::Reverse(b.len()));
    for bytes in candidates {
        let text = decode_revert_bytes(&bytes);
        if !text.starts_with("revert") {
            return text;
        }
    }
    msg.lines().next().unwrap_or("revert").to_string()
}

fn decode_revert_bytes(data: &[u8]) -> String {
    use alloy_sol_types::SolError;
    if data.len() < 4 {
        return "revert (no data)".to_string();
    }
    // Error(string) / Panic — the mocks use require() strings.
    if data.starts_with(&[0x08, 0xc3, 0x79, 0xa0]) && data.len() >= 4 + 64 {
        let len_word = U256::from_be_slice(&data[36..68.min(data.len())]);
        if let Ok(len) = usize::try_from(len_word) {
            if len <= 512 && data.len() >= 68 + len {
                let text = String::from_utf8_lossy(&data[68..68 + len]);
                return format!("Error(\"{text}\")");
            }
        }
    }
    let named: &[([u8; 4], &str)] = &[
        (SniperVaultErrors::NotOwner::SELECTOR, "NotOwner()"),
        (SniperVaultErrors::NotSearcher::SELECTOR, "NotSearcher()"),
        (SniperVaultErrors::Reentrancy::SELECTOR, "Reentrancy()"),
        (SniperVaultErrors::Deadline::SELECTOR, "Deadline()"),
        (
            SniperVaultErrors::BaseFeeTooHigh::SELECTOR,
            "BaseFeeTooHigh()",
        ),
        (SniperVaultErrors::ZeroToken::SELECTOR, "ZeroToken()"),
        (SniperVaultErrors::SweepFailed::SELECTOR, "SweepFailed()"),
        (
            SniperVaultErrors::TransferFailed::SELECTOR,
            "TransferFailed()",
        ),
    ];
    let selector: [u8; 4] = [data[0], data[1], data[2], data[3]];
    for (sel, name) in named {
        if *sel == selector {
            return (*name).to_string();
        }
    }
    let words = |from: usize| -> Vec<U256> {
        data[4 + from * 32..]
            .as_chunks::<32>()
            .0
            .iter()
            .map(|c| U256::from_be_slice(c))
            .collect()
    };
    if selector == SniperVaultErrors::OverSpend::SELECTOR {
        let w = words(0);
        return format!(
            "OverSpend(spent={}, allowed={})",
            w.first().copied().unwrap_or_default(),
            w.get(1).copied().unwrap_or_default()
        );
    }
    if selector == SniperVaultErrors::OverSell::SELECTOR {
        let w = words(0);
        return format!(
            "OverSell(sold={}, allowed={})",
            w.first().copied().unwrap_or_default(),
            w.get(1).copied().unwrap_or_default()
        );
    }
    if selector == SniperVaultErrors::InsufficientTokens::SELECTOR {
        let w = words(0);
        return format!(
            "InsufficientTokens(received={}, required={})",
            w.first().copied().unwrap_or_default(),
            w.get(1).copied().unwrap_or_default()
        );
    }
    if selector == SniperVaultErrors::InsufficientWeth::SELECTOR {
        let w = words(0);
        return format!(
            "InsufficientWeth(received={}, required={})",
            w.first().copied().unwrap_or_default(),
            w.get(1).copied().unwrap_or_default()
        );
    }
    if selector == SniperVaultErrors::DailyBudgetExceeded::SELECTOR {
        let w = words(0);
        return format!(
            "DailyBudgetExceeded(spent={}, budget={})",
            w.first().copied().unwrap_or_default(),
            w.get(1).copied().unwrap_or_default()
        );
    }
    if selector == SniperVaultErrors::TotalBudgetExceeded::SELECTOR {
        let w = words(0);
        return format!(
            "TotalBudgetExceeded(spent={}, budget={})",
            w.first().copied().unwrap_or_default(),
            w.get(1).copied().unwrap_or_default()
        );
    }
    if selector == SniperVaultErrors::CallFailed::SELECTOR {
        // Recurse one level into the wrapped leg's revert data.
        if data.len() >= 4 + 96 {
            let offset = U256::from_be_slice(&data[4 + 32..4 + 64]).to::<usize>();
            let len = U256::from_be_slice(&data[4 + 64..4 + 96]).to::<usize>();
            let start = 4 + offset + 32;
            if offset == 64 && len <= 512 && data.len() >= start + len {
                return format!(
                    "CallFailed: {}",
                    decode_revert_bytes(&data[start..start + len])
                );
            }
        }
        return "CallFailed (a fixture swap leg reverted)".to_string();
    }
    format!("custom error 0x{}", hex::encode(selector))
}

/// The fixture's durable description once deployed. Addresses are stable
/// across `anvil_reset` cycles because deployment always resets the owner's
/// nonce first — the CREATE address sequence is deterministic.
#[derive(Clone, Debug)]
pub struct SimVaultState {
    pub vault: Address,
    pub owner: Address,
    pub searcher: Address,
    pub weth: Address,
    pub chain_id: u64,
    pub daily_budget_wei: U256,
    pub total_budget_wei: U256,
    pub funded_weth_wei: U256,
    pub deployed_at_block: u64,
    /// True when the fixture deployed its own MockWETH (local-anvil tests
    /// without a fork). A fork fixture always binds the chain's real WETH.
    pub weth_is_mock: bool,
}

/// One simulated launch's liquidity: a fresh mock token + V2 pair seeded with
/// the observed launch reserves.
#[derive(Clone, Debug)]
pub struct SimPairFixture {
    pub token: Address,
    pub pair: Address,
    pub weth_reserve_wei: U256,
    pub token_reserve: U256,
    pub created_block: u64,
}

/// Result of executing a fixture transaction.
#[derive(Clone, Debug)]
pub enum SimTxOutcome {
    Mined {
        tx_hash: B256,
        block: u64,
        gas_cost_wei: U256,
        receipt: Value,
    },
    Reverted {
        reason: String,
    },
}

impl SimTxOutcome {
    pub fn is_mined(&self) -> bool {
        matches!(self, SimTxOutcome::Mined { .. })
    }
}

pub struct SimVaultFixture {
    /// The local fork transport. Every call in this module goes here — this
    /// handle must never be a production RPC.
    rpc: RpcClient,
    weth: Address,
    chain_id: u64,
    daily_budget_wei: U256,
    total_budget_wei: U256,
    funded_weth_wei: U256,
    state: parking_lot::RwLock<Option<SimVaultState>>,
    /// Seed reserves of every deployed launch pair, so a pair lost to an
    /// `anvil_reset` can be rebuilt with the same liquidity instead of
    /// silently marking its positions against a dead curve.
    pair_seeds: parking_lot::RwLock<std::collections::HashMap<Address, (U256, U256)>>,
    /// Serialises fixture transactions: one at a time, and never interleaved
    /// with the atomic engine's automine-off simulation window when a shared
    /// lock is provided.
    lock: tokio::sync::Mutex<()>,
    shared_lock: Option<ArcSharedLock>,
}

/// The atomic fork's simulation mutex, shared so fixture transactions cannot
/// land inside a bundle replay's snapshot/revert window.
pub type ArcSharedLock = std::sync::Arc<tokio::sync::Mutex<()>>;

impl SimVaultFixture {
    pub fn new(
        rpc: RpcClient,
        weth: Address,
        chain_id: u64,
        daily_budget_wei: U256,
        total_budget_wei: U256,
    ) -> Self {
        Self {
            rpc,
            weth,
            chain_id,
            daily_budget_wei,
            total_budget_wei,
            funded_weth_wei: FIXTURE_VAULT_WETH_WEI,
            state: parking_lot::RwLock::new(None),
            pair_seeds: parking_lot::RwLock::new(std::collections::HashMap::new()),
            lock: tokio::sync::Mutex::new(()),
            shared_lock: None,
        }
    }

    /// Share the atomic fork's serialization lock (see `AnvilSim::sim_lock`).
    pub fn with_shared_lock(mut self, lock: ArcSharedLock) -> Self {
        self.shared_lock = Some(lock);
        self
    }

    pub fn rpc(&self) -> &RpcClient {
        &self.rpc
    }

    pub fn state(&self) -> Option<SimVaultState> {
        self.state.read().clone()
    }

    pub fn chain_weth(&self) -> Address {
        self.weth
    }

    pub fn budgets(&self) -> (U256, U256) {
        (self.daily_budget_wei, self.total_budget_wei)
    }

    async fn guard<'a>(&'a self) -> GuardHandle<'a> {
        let shared = match &self.shared_lock {
            Some(l) => Some(l.lock().await),
            None => None,
        };
        let own = self.lock.lock().await;
        GuardHandle {
            _shared: shared,
            _own: own,
        }
    }

    /// Deploy (or re-deploy after an `anvil_reset`) the whole fixture.
    ///
    /// Idempotent: when the vault address still has code on the fork the
    /// cached state is returned untouched.
    pub async fn ensure_deployed(&self) -> Result<SimVaultState> {
        let _guard = self.guard().await;
        let cached = self.state.read().clone();
        if let Some(state) = cached {
            if self.code_exists(state.vault).await.unwrap_or(false) {
                return Ok(state);
            }
            tracing::warn!(
                target: "sniper",
                vault = ?state.vault,
                "simulation fixture lost its code (fork reset) — redeploying at the deterministic address"
            );
        }
        let owner = sim_owner_address();
        let searcher = sim_searcher_signer().address();

        // A clean nonce sequence makes every deployment address deterministic,
        // so DB rows and open positions keep pointing at valid fixture code
        // across reforks.
        self.rpc
            .call_raw("anvil_setNonce", json!([format!("{owner:?}"), "0x0"]))
            .await
            .context("reset simulation owner nonce")?;
        self.rpc
            .call_raw(
                "anvil_setBalance",
                json!([
                    format!("{owner:?}"),
                    format!(
                        "0x{:x}",
                        U256::from(1_000u64) * U256::from(1_000_000_000_000_000_000u128)
                    )
                ]),
            )
            .await
            .context("fund simulation owner")?;
        // The simulation searcher pays gas for openPosition/closePosition on
        // the fork. Fork-local funding only — this address never holds real
        // funds anywhere.
        self.rpc
            .call_raw(
                "anvil_setBalance",
                json!([
                    format!("{searcher:?}"),
                    format!(
                        "0x{:x}",
                        U256::from(100u64) * U256::from(1_000_000_000_000_000_000u128)
                    )
                ]),
            )
            .await
            .context("fund simulation searcher")?;

        // With no configured WETH (a bare local anvil, e.g. CI integration
        // tests) the fixture mints its own. A forked chain always uses the
        // chain's real WETH — never mainnet's address on Base or vice versa.
        let (weth, weth_is_mock) = if self.weth.is_zero() {
            let (addr, _) = self
                .deploy(owner, MOCK_WETH_CREATION_HEX, &[])
                .await
                .context("deploy MockWETH")?;
            (addr, true)
        } else {
            (self.weth, false)
        };

        let constructor_args = VaultCtorArgs {
            weth,
            dailyBudget: self.daily_budget_wei,
            totalBudget: self.total_budget_wei,
        }
        .abi_encode();
        let (vault, deployed_block) = self
            .deploy(owner, SNIPER_VAULT_CREATION_HEX, &constructor_args)
            .await
            .context("deploy SniperVault fixture")?;

        // Authorize the deterministic simulation searcher EOA.
        self.send(
            owner,
            vault,
            &ISimVaultAdmin::setSearcherCall {
                searcher,
                allowed: true,
            }
            .abi_encode(),
            U256::ZERO,
        )
        .await
        .context("authorize simulation searcher")?;

        // Fund with simulated WETH. MockWETH mints; a fork's real WETH is
        // wrapped from owner ETH and transferred — portable across WETH9
        // implementations without assuming storage layout.
        if weth_is_mock {
            self.send(
                owner,
                weth,
                &IMockWETH::mintCall {
                    to: vault,
                    amount: self.funded_weth_wei,
                }
                .abi_encode(),
                U256::ZERO,
            )
            .await
            .context("mint fixture WETH into the vault")?;
        } else {
            // owner already funded above; wrap and move.
            self.send(
                owner,
                weth,
                &IWETH9::depositCall {}.abi_encode(),
                self.funded_weth_wei,
            )
            .await
            .context("wrap fixture WETH")?;
            self.send(
                owner,
                weth,
                &IWETH9::transferCall {
                    to: vault,
                    value: self.funded_weth_wei,
                }
                .abi_encode(),
                U256::ZERO,
            )
            .await
            .context("fund fixture vault with WETH")?;
        }

        // Verify the constructor bound the chain WETH. A mismatch here would
        // silently simulate against the wrong reserve asset.
        let bound = self
            .read_view(vault, ISimVaultAdmin::WETHCall {}.abi_encode(), |bytes| {
                ISimVaultAdmin::WETHCall::abi_decode_returns(bytes, true)
                    .map(|v| v._0)
                    .ok()
            })
            .await?
            .ok_or_else(|| anyhow!("fixture vault returned no WETH binding"))?;
        if bound != weth {
            anyhow::bail!("fixture vault bound WETH {bound:?} but the chain WETH is {weth:?}");
        }

        let state = SimVaultState {
            vault,
            owner,
            searcher,
            weth,
            chain_id: self.chain_id,
            daily_budget_wei: self.daily_budget_wei,
            total_budget_wei: self.total_budget_wei,
            funded_weth_wei: self.funded_weth_wei,
            deployed_at_block: deployed_block,
            weth_is_mock,
        };
        *self.state.write() = Some(state.clone());
        tracing::info!(
            target: "sniper",
            vault = ?vault,
            searcher = ?searcher,
            weth = ?weth,
            daily_budget_wei = %self.daily_budget_wei,
            "simulation SniperVault fixture ready (local anvil only)"
        );
        Ok(state)
    }

    /// Deploy deterministic mock liquidity for one simulated launch, seeded
    /// with the launch's observed reserves so the contract-backed simulation
    /// trades the same curve the paper ledger quoted.
    pub async fn deploy_launch_pair(
        &self,
        weth_reserve_wei: U256,
        token_reserve: U256,
    ) -> Result<SimPairFixture> {
        let state = self.ensure_deployed().await?;
        let _guard = self.guard().await;
        let owner = state.owner;

        let (token, token_block) = self
            .deploy(
                owner,
                MOCK_ERC20_CREATION_HEX,
                &TokenCtorArgs {
                    name: "Sim Launch".to_string(),
                    symbol: "SIM".to_string(),
                }
                .abi_encode(),
            )
            .await
            .context("deploy mock launch token")?;
        let (pair, _) = self
            .deploy(
                owner,
                SIM_V2_PAIR_CREATION_HEX,
                &PairCtorArgs {
                    t0: token,
                    t1: state.weth,
                }
                .abi_encode(),
            )
            .await
            .context("deploy mock launch pair")?;

        // Token side: mint the reserve straight into the pair.
        self.send(
            owner,
            token,
            &IMockERC20::mintCall {
                to: pair,
                amount: token_reserve,
            }
            .abi_encode(),
            U256::ZERO,
        )
        .await
        .context("seed mock token reserve")?;

        // WETH side.
        if state.weth_is_mock {
            self.send(
                owner,
                state.weth,
                &IMockWETH::mintCall {
                    to: pair,
                    amount: weth_reserve_wei,
                }
                .abi_encode(),
                U256::ZERO,
            )
            .await
            .context("seed mock pair WETH reserve")?;
        } else {
            self.send(
                owner,
                state.weth,
                &IWETH9::depositCall {}.abi_encode(),
                weth_reserve_wei,
            )
            .await
            .context("wrap pair WETH reserve")?;
            self.send(
                owner,
                state.weth,
                &IWETH9::transferCall {
                    to: pair,
                    value: weth_reserve_wei,
                }
                .abi_encode(),
                U256::ZERO,
            )
            .await
            .context("seed pair WETH reserve")?;
        }

        self.send(
            owner,
            pair,
            &ISimV2Pair::syncCall {}.abi_encode(),
            U256::ZERO,
        )
        .await
        .context("sync mock pair reserves")?;

        self.pair_seeds
            .write()
            .insert(pair, (weth_reserve_wei, token_reserve));
        Ok(SimPairFixture {
            token,
            pair,
            weth_reserve_wei,
            token_reserve,
            created_block: token_block,
        })
    }

    /// Rebuild a launch pair whose code was wiped by an `anvil_reset`.
    /// Returns the *new* pair address (deterministic redeployment creates a
    /// fresh address once the nonce sequence has advanced); the caller must
    /// re-point its position at it. Unknown pairs are an error, never a
    /// fabricated pool.
    pub async fn rebuild_pair(&self, old_pair: Address) -> Result<Address> {
        let seeds = self.pair_seeds.read().clone();
        let Some((weth_reserve, token_reserve)) = seeds.get(&old_pair).copied() else {
            anyhow::bail!("no seed reserves recorded for fixture pair {old_pair:?}");
        };
        if self.code_exists(old_pair).await.unwrap_or(false) {
            return Ok(old_pair);
        }
        let rebuilt = self.deploy_launch_pair(weth_reserve, token_reserve).await?;
        Ok(rebuilt.pair)
    }

    /// Flip a launch fixture into a honeypot: the token blocks transfers out
    /// of the vault, so `closePosition` reverts exactly like a hostile token.
    pub async fn set_honeypot(&self, fixture: &SimPairFixture) -> Result<()> {
        let state = self
            .state()
            .ok_or_else(|| anyhow!("fixture not deployed"))?;
        let _guard = self.guard().await;
        self.send(
            state.owner,
            fixture.token,
            &IMockERC20::setBlockedSenderCall {
                from: state.vault,
                blocked: true,
            }
            .abi_encode(),
            U256::ZERO,
        )
        .await
        .context("block vault sells on mock token")
    }

    /// Execute vault calldata **as the simulation searcher** and return the
    /// mined receipt or the decoded revert. Never touches a real signer.
    pub async fn execute_vault_calldata(&self, data: &[u8]) -> Result<SimTxOutcome> {
        let state = self
            .state()
            .ok_or_else(|| anyhow!("simulation fixture is not deployed"))?;
        let _guard = self.guard().await;
        self.execute_as(state.searcher, state.vault, data).await
    }

    /// Read a simulated position's pair reserves from the fork — the mark
    /// source for contract-backed simulation, mirroring the live reserve
    /// reads.
    pub async fn pair_reserves(&self, pair: Address) -> Result<(U256, U256)> {
        let res = self
            .read_view(pair, ISimV2Pair::getReservesCall {}.abi_encode(), |bytes| {
                ISimV2Pair::getReservesCall::abi_decode_returns(bytes, true).ok()
            })
            .await?
            .ok_or_else(|| anyhow!("pair {pair:?} returned no reserves"))?;
        Ok((U256::from(res.reserve0), U256::from(res.reserve1)))
    }

    /// Live-style status of the fixture vault for the console.
    pub async fn vault_status(&self) -> Result<Value> {
        let state = self
            .state()
            .ok_or_else(|| anyhow!("simulation fixture is not deployed"))?;
        let weth_balance = self
            .read_view(
                state.weth,
                IWETH9::balanceOfCall { who: state.vault }.abi_encode(),
                |bytes| {
                    IWETH9::balanceOfCall::abi_decode_returns(bytes, true)
                        .map(|v| v._0)
                        .ok()
                },
            )
            .await?
            .unwrap_or(U256::ZERO);
        let spendable = self
            .read_view(
                state.vault,
                ISimVaultAdmin::spendableRemainingCall {}.abi_encode(),
                |bytes| {
                    ISimVaultAdmin::spendableRemainingCall::abi_decode_returns(bytes, true)
                        .map(|v| v._0)
                        .ok()
                },
            )
            .await?
            .unwrap_or(U256::ZERO);
        let searcher_allowed = self
            .read_view(
                state.vault,
                ISimVaultAdmin::searchersCall {
                    who: state.searcher,
                }
                .abi_encode(),
                |bytes| {
                    ISimVaultAdmin::searchersCall::abi_decode_returns(bytes, true)
                        .map(|v| v._0)
                        .ok()
                },
            )
            .await?
            .unwrap_or(false);
        Ok(json!({
            "ready": true,
            "kind": "simulation_fixture",
            "label": "Simulation vault · local Anvil only",
            "address": format!("{:?}", state.vault),
            "chainId": state.chain_id,
            "weth": format!("{:?}", state.weth),
            "owner": format!("{:?}", state.owner),
            "searcher": format!("{:?}", state.searcher),
            "searcherAllowlisted": searcher_allowed,
            "vaultWethBalanceWei": weth_balance.to_string(),
            "spendableRemainingWei": spendable.to_string(),
            "dailyBudgetWei": state.daily_budget_wei.to_string(),
            "totalBudgetWei": state.total_budget_wei.to_string(),
            "fundedWethWei": state.funded_weth_wei.to_string(),
            "deployedAtBlock": state.deployed_at_block,
        }))
    }

    // --- internals ---------------------------------------------------------

    async fn code_exists(&self, addr: Address) -> Result<bool> {
        let v = self
            .rpc
            .call_raw("eth_getCode", json!([format!("{addr:?}"), "latest"]))
            .await?;
        Ok(v.as_str().map(|s| s != "0x").unwrap_or(false))
    }

    async fn deploy(
        &self,
        from: Address,
        creation_hex: &str,
        args: &[u8],
    ) -> Result<(Address, u64)> {
        let data = format!(
            "0x{}{}",
            creation_hex.trim().trim_start_matches("0x"),
            hex::encode(args)
        );
        // CREATE into an address that already holds code (a fork that
        // re-pinned past an upstream deployment at the same deterministic
        // address) fails silently: status 0, no contractAddress. Bump the
        // deployer nonce and retry — the fixture stays self-healing across
        // reforks instead of wedging the simulation lane.
        let mut last_error = anyhow!("fixture deployment failed");
        for attempt in 0..8u64 {
            if attempt > 0 {
                let nonce = self
                    .rpc
                    .get_transaction_count(from, 0)
                    .await
                    .unwrap_or(attempt);
                let _ = self
                    .rpc
                    .call_raw(
                        "anvil_setNonce",
                        json!([format!("{from:?}"), format!("0x{:x}", nonce + 1)]),
                    )
                    .await;
            }
            let hash = match self
                .rpc
                .call_raw(
                    "eth_sendTransaction",
                    json!([{
                        "from": format!("{from:?}"),
                        "data": data,
                        "gas": "0x1000000"
                    }]),
                )
                .await
            {
                Ok(h) => h,
                Err(error) => {
                    last_error = error.context("fixture deployment send");
                    continue;
                }
            };
            let receipt = self.wait_receipt(&hash).await?;
            if parse_u64(&receipt["status"]) != 1 {
                last_error = anyhow!("fixture deployment reverted");
                continue;
            }
            let Some(addr) = receipt
                .get("contractAddress")
                .and_then(Value::as_str)
                .and_then(|s| s.parse::<Address>().ok())
            else {
                last_error = anyhow!("fixture deployment receipt has no contractAddress");
                continue;
            };
            return Ok((addr, parse_u64(&receipt["blockNumber"])));
        }
        Err(last_error)
    }

    async fn send(&self, from: Address, to: Address, data: &[u8], value: U256) -> Result<()> {
        let mut params = json!({
            "from": format!("{from:?}"),
            "to": format!("{to:?}"),
            "data": format!("0x{}", hex::encode(data)),
            "gas": "0x1000000"
        });
        if !value.is_zero() {
            params["value"] = json!(format!("0x{value:x}"));
        }
        let hash = self
            .rpc
            .call_raw("eth_sendTransaction", json!([params]))
            .await?;
        let receipt = self.wait_receipt(&hash).await?;
        if parse_u64(&receipt["status"]) != 1 {
            anyhow::bail!("fixture setup transaction reverted");
        }
        Ok(())
    }

    /// Preflight with `eth_call`, then send. A revert surfaces as
    /// `SimTxOutcome::Reverted` with the decoded guard, never as a booked fill.
    async fn execute_as(&self, from: Address, to: Address, data: &[u8]) -> Result<SimTxOutcome> {
        // Preflight with eth_call: a deterministic revert surfaces here with
        // its decodable guard data, before anything is mined.
        let call_params = json!([{
            "from": format!("{from:?}"),
            "to": format!("{to:?}"),
            "data": format!("0x{}", hex::encode(data)),
            "gas": "0x1000000"
        }, "latest"]);
        if let Err(err_value) = self.rpc.call_raw_with_error("eth_call", call_params).await {
            let error = anyhow::anyhow!("rpc error: {err_value}");
            return Ok(SimTxOutcome::Reverted {
                reason: decode_vault_revert(&error),
            });
        }
        // Anvil builds differ on how a reverting automined send is surfaced:
        // some reject the send with the revert data, others mine it at
        // status 0. Handle both — a revert is a revert, and it must never
        // book a fill or move the bankroll.
        let hash = match self
            .rpc
            .call_raw(
                "eth_sendTransaction",
                json!([{
                    "from": format!("{from:?}"),
                    "to": format!("{to:?}"),
                    "data": format!("0x{}", hex::encode(data)),
                    "gas": "0x1000000"
                }]),
            )
            .await
        {
            Ok(h) => h,
            Err(error) => {
                return Ok(SimTxOutcome::Reverted {
                    reason: decode_vault_revert(&error),
                });
            }
        };
        let hash_str = hash
            .as_str()
            .ok_or_else(|| anyhow!("fixture send returned no hash"))?;
        let receipt = self.wait_receipt(&hash).await?;
        if parse_u64(&receipt["status"]) != 1 {
            return Ok(SimTxOutcome::Reverted {
                reason: "mined reverted (no event)".to_string(),
            });
        }
        let gas_used = crate::types::parse_u256(&receipt["gasUsed"]);
        let gas_price = crate::types::parse_u256(&receipt["effectiveGasPrice"]);
        let tx_hash = hash_str
            .parse::<B256>()
            .map_err(|e| anyhow!("bad fixture tx hash {hash_str}: {e}"))?;
        Ok(SimTxOutcome::Mined {
            tx_hash,
            block: parse_u64(&receipt["blockNumber"]),
            gas_cost_wei: gas_used.saturating_mul(gas_price),
            receipt,
        })
    }

    async fn read_view<T>(
        &self,
        to: Address,
        data: Vec<u8>,
        decode: impl Fn(&[u8]) -> Option<T>,
    ) -> Result<Option<T>> {
        let res = self
            .rpc
            .call_raw(
                "eth_call",
                json!([{
                    "to": format!("{to:?}"),
                    "data": format!("0x{}", hex::encode(data))
                }, "latest"]),
            )
            .await?;
        let hex_str = res.as_str().unwrap_or("0x");
        let bytes = hex::decode(hex_str.trim_start_matches("0x")).unwrap_or_default();
        Ok(decode(&bytes))
    }

    async fn wait_receipt(&self, hash: &Value) -> Result<Value> {
        for _ in 0..40 {
            let receipt = self
                .rpc
                .call_raw("eth_getTransactionReceipt", json!([hash]))
                .await
                .unwrap_or(Value::Null);
            if !receipt.is_null() {
                return Ok(receipt);
            }
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        }
        Err(anyhow!("fixture transaction was never mined"))
    }
}

pub struct GuardHandle<'a> {
    _shared: Option<tokio::sync::MutexGuard<'a, ()>>,
    _own: tokio::sync::MutexGuard<'a, ()>,
}

#[cfg(test)]
mod integration_tests {
    //! End-to-end fixture tests against a real local `anvil` (no fork, no
    //! network). They skip gracefully when the binary is absent, but CI
    //! installs Foundry, so they run wherever `forge` does.

    use super::*;
    use crate::sniper::calldata;

    struct LocalAnvil {
        child: std::process::Child,
        rpc: RpcClient,
    }

    async fn spawn_local_anvil() -> Option<LocalAnvil> {
        let bin = std::env::var("ANVIL_BIN").unwrap_or_else(|_| "anvil".into());
        // Grab a free port.
        let port = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").ok()?;
            listener.local_addr().ok()?.port()
        };
        let child = std::process::Command::new(&bin)
            .args([
                "--port",
                &port.to_string(),
                "--host",
                "127.0.0.1",
                "--auto-impersonate",
                "--silent",
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .ok()?;
        let rpc = RpcClient::new(format!("http://127.0.0.1:{port}")).ok()?;
        for _ in 0..80 {
            if rpc
                .call_raw("eth_blockNumber", serde_json::json!([]))
                .await
                .is_ok()
            {
                return Some(LocalAnvil { child, rpc });
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        None
    }

    impl Drop for LocalAnvil {
        fn drop(&mut self) {
            let _ = self.child.kill();
        }
    }

    fn eth(n: u64) -> U256 {
        U256::from(n) * U256::from(1_000_000_000_000_000_000u128)
    }

    /// The V2 output formula (0.3% fee) — same math as the pair's K check.
    fn v2_out(amount_in: U256, reserve_in: U256, reserve_out: U256) -> U256 {
        let in_with_fee = amount_in * U256::from(997u64);
        (in_with_fee * reserve_out) / (reserve_in * U256::from(1000u64) + in_with_fee)
    }

    /// Deterministic simulation identities must never equal a production key.
    #[test]
    fn sim_identities_are_deterministic_and_distinct() {
        let owner_a = sim_owner_address();
        let owner_b = sim_owner_address();
        let searcher_a = sim_searcher_signer().address();
        let searcher_b = sim_searcher_signer().address();
        assert_eq!(owner_a, owner_b, "the sim owner must be stable");
        assert_eq!(searcher_a, searcher_b, "the sim searcher must be stable");
        assert_ne!(owner_a, searcher_a, "owner and searcher stay separate");
        // And neither is derived from the dedicated sniper key (which is not
        // even configured in tests) — the derivation uses only the built-in
        // simulation key + a domain tag.
    }

    #[test]
    fn vault_revert_decoder_names_the_contract_guards() {
        use alloy_sol_types::SolError;
        let text = decode_revert_bytes(
            &SniperVaultErrors::DailyBudgetExceeded {
                spent: eth(2),
                budget: eth(1),
            }
            .abi_encode(),
        );
        assert!(text.contains("DailyBudgetExceeded"), "{text}");
        assert!(text.contains("budget="), "{text}");

        let text = decode_revert_bytes(&SniperVaultErrors::Deadline {}.abi_encode());
        assert!(text.contains("Deadline"), "{text}");

        let text = decode_revert_bytes(&SniperVaultErrors::BaseFeeTooHigh {}.abi_encode());
        assert!(text.contains("BaseFeeTooHigh"), "{text}");

        let text = decode_revert_bytes(
            &SniperVaultErrors::InsufficientTokens {
                received: U256::from(5u8),
                required: U256::from(9u8),
            }
            .abi_encode(),
        );
        assert!(
            text.contains("InsufficientTokens(received=5, required=9)"),
            "{text}"
        );
    }

    #[tokio::test]
    async fn fixture_deploys_the_real_vault_with_chain_weth_binding() {
        let Some(anvil) = spawn_local_anvil().await else {
            eprintln!("anvil not available — skipping fixture integration test");
            return;
        };
        // weth = ZERO makes the fixture deploy its own MockWETH (the bare
        // local-anvil path). A forked deployment always binds the chain WETH.
        let fixture = SimVaultFixture::new(anvil.rpc.clone(), Address::ZERO, 1, eth(1), eth(5));
        let state = fixture.ensure_deployed().await.expect("fixture deploys");

        assert!(
            state.weth_is_mock,
            "local anvil uses the fixture's own WETH"
        );
        assert_ne!(state.vault, Address::ZERO);
        assert_eq!(state.daily_budget_wei, eth(1));
        assert_eq!(state.total_budget_wei, eth(5));

        // The constructor bound the WETH given at deploy — chain-specific, not
        // a shared constant.
        let bound = fixture
            .read_view(
                state.vault,
                ISimVaultAdmin::WETHCall {}.abi_encode(),
                |bytes| {
                    ISimVaultAdmin::WETHCall::abi_decode_returns(bytes, true)
                        .map(|v| v._0)
                        .ok()
                },
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(bound, state.weth);

        // The deterministic searcher — and only it — is allowlisted.
        let allowed = fixture
            .read_view(
                state.vault,
                ISimVaultAdmin::searchersCall {
                    who: state.searcher,
                }
                .abi_encode(),
                |bytes| {
                    ISimVaultAdmin::searchersCall::abi_decode_returns(bytes, true)
                        .map(|v| v._0)
                        .ok()
                },
            )
            .await
            .unwrap()
            .unwrap();
        assert!(
            allowed,
            "the sim searcher must be authorized on the fixture"
        );

        // And the vault holds its simulated WETH — never real funds.
        let status = fixture.vault_status().await.unwrap();
        assert_eq!(status["ready"], serde_json::json!(true));
        assert_eq!(status["kind"], serde_json::json!("simulation_fixture"));
        assert_eq!(
            status["vaultWethBalanceWei"],
            serde_json::json!(FIXTURE_VAULT_WETH_WEI.to_string())
        );

        // Re-deploy is a no-op while the code is there (idempotent).
        let again = fixture.ensure_deployed().await.unwrap();
        assert_eq!(again.vault, state.vault);
    }

    #[tokio::test]
    async fn fixture_open_close_books_exact_event_values() {
        let Some(anvil) = spawn_local_anvil().await else {
            eprintln!("anvil not available — skipping fixture integration test");
            return;
        };
        let fixture = SimVaultFixture::new(anvil.rpc.clone(), Address::ZERO, 1, eth(2), U256::ZERO);
        let state = fixture.ensure_deployed().await.unwrap();
        let pair = fixture
            .deploy_launch_pair(eth(10), U256::from(1_000_000u64) * eth(1))
            .await
            .unwrap();

        let size = eth(1) / U256::from(10u64); // 0.1 ETH
        let expected_out = (size * pair.token_reserve * U256::from(997))
            / (pair.weth_reserve_wei * U256::from(1000) + size * U256::from(997));
        let is_weth_token0 = state.weth < pair.token;
        let tag = calldata::make_tag("sim-pos", 0);
        let (_, _, entry_data) = calldata::build_entry(
            state.vault,
            pair.pair,
            state.weth,
            pair.token,
            is_weth_token0,
            size,
            expected_out,
            300,
            u64::MAX / 2,
            2,
            U256::ZERO,
            tag,
        );

        match fixture.execute_vault_calldata(&entry_data).await.unwrap() {
            SimTxOutcome::Reverted { reason } => panic!("entry reverted: {reason}"),
            SimTxOutcome::Mined { receipt, block, .. } => {
                let (weth_spent, tokens_received, _, evt_block) =
                    crate::sniper::execution::SniperExecution::decode_entry_receipt(
                        &receipt,
                        state.vault,
                        pair.token,
                    )
                    .expect("EntryExecuted must be decodable");
                assert_eq!(evt_block, block);
                assert_eq!(weth_spent, size, "realised spend equals the transfer");
                assert_eq!(
                    tokens_received, expected_out,
                    "tokensReceived must match the V2 curve output"
                );

                // Exit everything and decode the ExitExecuted values.
                // Reserves after the entry moved the curve.
                let (r0, r1) = fixture.pair_reserves(pair.pair).await.unwrap();
                let (weth_reserve_now, token_reserve_now) =
                    if is_weth_token0 { (r0, r1) } else { (r1, r0) };
                let expected_weth_back =
                    v2_out(tokens_received, token_reserve_now, weth_reserve_now);
                let (_, _, exit_data) = calldata::build_exit(
                    state.vault,
                    pair.pair,
                    state.weth,
                    pair.token,
                    is_weth_token0,
                    tokens_received,
                    expected_weth_back,
                    0,
                    u64::MAX / 2,
                    2,
                    U256::ZERO,
                    calldata::make_tag("sim-pos", 1),
                );
                match fixture.execute_vault_calldata(&exit_data).await.unwrap() {
                    SimTxOutcome::Reverted { reason } => panic!("exit reverted: {reason}"),
                    SimTxOutcome::Mined { receipt, .. } => {
                        let (tokens_sold, weth_received, _, _) =
                            crate::sniper::execution::SniperExecution::decode_exit_receipt(
                                &receipt,
                                state.vault,
                                pair.token,
                            )
                            .expect("ExitExecuted must be decodable");
                        assert_eq!(tokens_sold, tokens_received);
                        // A round trip through a flat pool loses the fee twice.
                        assert!(weth_received < size);
                        assert!(!weth_received.is_zero());
                    }
                }
            }
        }
    }

    #[tokio::test]
    async fn fixture_guards_revert_with_named_reasons_and_move_nothing() {
        let Some(anvil) = spawn_local_anvil().await else {
            eprintln!("anvil not available — skipping fixture integration test");
            return;
        };
        // Daily budget of 0.05 ETH: a 0.1 ETH entry must hit the ceiling.
        let fixture = SimVaultFixture::new(
            anvil.rpc.clone(),
            Address::ZERO,
            1,
            eth(1) / U256::from(20u64),
            U256::ZERO,
        );
        let state = fixture.ensure_deployed().await.unwrap();
        let pair = fixture
            .deploy_launch_pair(eth(10), U256::from(1_000_000u64) * eth(1))
            .await
            .unwrap();

        let weth_before = fixture
            .read_view(
                state.weth,
                IWETH9::balanceOfCall { who: state.vault }.abi_encode(),
                |bytes| {
                    IWETH9::balanceOfCall::abi_decode_returns(bytes, true)
                        .map(|v| v._0)
                        .ok()
                },
            )
            .await
            .unwrap()
            .unwrap();

        let is_weth_token0 = state.weth < pair.token;
        // 1) Daily budget ceiling.
        let oversize = eth(1) / U256::from(10u64);
        let expected = (oversize * pair.token_reserve * U256::from(997))
            / (pair.weth_reserve_wei * U256::from(1000) + oversize * U256::from(997));
        let (_, _, data) = calldata::build_entry(
            state.vault,
            pair.pair,
            state.weth,
            pair.token,
            is_weth_token0,
            oversize,
            expected,
            300,
            u64::MAX / 2,
            2,
            U256::ZERO,
            calldata::make_tag("guard-1", 0),
        );
        match fixture.execute_vault_calldata(&data).await.unwrap() {
            SimTxOutcome::Reverted { reason } => {
                assert!(reason.contains("DailyBudgetExceeded"), "{reason}");
            }
            SimTxOutcome::Mined { .. } => panic!("oversized entry must revert"),
        }

        // 2) Slippage: demanding twice the curve's fair output fails the
        // pair's K invariant inside the batch. The vault wraps the leg's
        // revert as CallFailed — the entry still never settles. (The
        // guard-level InsufficientTokens floor is covered in the Foundry
        // suite, where the guard fields are set directly.)
        let ok_size = eth(1) / U256::from(100u64); // 0.01 ETH < budget
        let expected = (ok_size * pair.token_reserve * U256::from(997))
            / (pair.weth_reserve_wei * U256::from(1000) + ok_size * U256::from(997));
        let (_, guard, data) = calldata::build_entry(
            state.vault,
            pair.pair,
            state.weth,
            pair.token,
            is_weth_token0,
            ok_size,
            expected * U256::from(2u64), // impossible swap output
            300,
            u64::MAX / 2,
            2,
            U256::ZERO,
            calldata::make_tag("guard-2", 0),
        );
        assert!(guard.minTokensOut > U256::ZERO);
        match fixture.execute_vault_calldata(&data).await.unwrap() {
            SimTxOutcome::Reverted { reason } => {
                assert!(
                    reason.contains("CallFailed") || reason.contains("K"),
                    "{reason}"
                );
            }
            SimTxOutcome::Mined { .. } => panic!("impossible swap output must revert"),
        }

        // 3) Block deadline in the past.
        let (_, _, data) = calldata::build_entry(
            state.vault,
            pair.pair,
            state.weth,
            pair.token,
            is_weth_token0,
            ok_size,
            expected,
            300,
            1, // long past
            0, // no grace
            U256::ZERO,
            calldata::make_tag("guard-3", 0),
        );
        match fixture.execute_vault_calldata(&data).await.unwrap() {
            SimTxOutcome::Reverted { reason } => assert!(reason.contains("Deadline"), "{reason}"),
            SimTxOutcome::Mined { .. } => panic!("expired deadline must revert"),
        }

        // A failed simulated trade moved no WETH at all.
        let weth_after = fixture
            .read_view(
                state.weth,
                IWETH9::balanceOfCall { who: state.vault }.abi_encode(),
                |bytes| {
                    IWETH9::balanceOfCall::abi_decode_returns(bytes, true)
                        .map(|v| v._0)
                        .ok()
                },
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            weth_before, weth_after,
            "reverted simulation entries must not move the vault's WETH"
        );
    }

    #[tokio::test]
    async fn fixture_honeypot_blocks_the_exit() {
        let Some(anvil) = spawn_local_anvil().await else {
            eprintln!("anvil not available — skipping fixture integration test");
            return;
        };
        let fixture = SimVaultFixture::new(anvil.rpc.clone(), Address::ZERO, 1, eth(1), U256::ZERO);
        let state = fixture.ensure_deployed().await.unwrap();
        let pair = fixture
            .deploy_launch_pair(eth(10), U256::from(1_000_000u64) * eth(1))
            .await
            .unwrap();

        let size = eth(1) / U256::from(100u64);
        let expected = (size * pair.token_reserve * U256::from(997))
            / (pair.weth_reserve_wei * U256::from(1000) + size * U256::from(997));
        let is_weth_token0 = state.weth < pair.token;
        let (_, _, data) = calldata::build_entry(
            state.vault,
            pair.pair,
            state.weth,
            pair.token,
            is_weth_token0,
            size,
            expected,
            300,
            u64::MAX / 2,
            2,
            U256::ZERO,
            calldata::make_tag("hp", 0),
        );
        assert!(
            fixture
                .execute_vault_calldata(&data)
                .await
                .unwrap()
                .is_mined(),
            "the entry lands before the trap"
        );

        fixture.set_honeypot(&pair).await.unwrap();

        let held = fixture
            .read_view(
                pair.token,
                IMockERC20::balanceOfCall { who: state.vault }.abi_encode(),
                |bytes| {
                    IMockERC20::balanceOfCall::abi_decode_returns(bytes, true)
                        .map(|v| v._0)
                        .ok()
                },
            )
            .await
            .unwrap()
            .unwrap();
        let (r0, r1) = fixture.pair_reserves(pair.pair).await.unwrap();
        let (weth_reserve_now, token_reserve_now) =
            if is_weth_token0 { (r0, r1) } else { (r1, r0) };
        let expected_weth_back = v2_out(held, token_reserve_now, weth_reserve_now);
        let (_, _, exit_data) = calldata::build_exit(
            state.vault,
            pair.pair,
            state.weth,
            pair.token,
            is_weth_token0,
            held,
            expected_weth_back,
            0,
            u64::MAX / 2,
            2,
            U256::ZERO,
            calldata::make_tag("hp", 1),
        );
        match fixture.execute_vault_calldata(&exit_data).await.unwrap() {
            SimTxOutcome::Reverted { reason } => {
                assert!(
                    reason.contains("blocked") || reason.contains("CallFailed"),
                    "{reason}"
                );
            }
            SimTxOutcome::Mined { .. } => panic!("a honeypot sell must revert"),
        }
        // The vault still holds every token — nothing was credited or moved.
        let still_held = fixture
            .read_view(
                pair.token,
                IMockERC20::balanceOfCall { who: state.vault }.abi_encode(),
                |bytes| {
                    IMockERC20::balanceOfCall::abi_decode_returns(bytes, true)
                        .map(|v| v._0)
                        .ok()
                },
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(held, still_held);
    }
}
