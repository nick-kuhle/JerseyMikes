//! Nonce and inventory manager.
//!
//! The bundle nonce used to be a hardcoded `0`. That is fine for anvil
//! impersonation (the fork sets the sender's nonce itself) but it produces a
//! nonsense signed bundle: a two-leg sandwich would sign both legs at nonce 0,
//! and `eth_callBundle` would reject the back-run. This module is the source
//! of truth for
//!
//! * the searcher's next nonce (refreshed from chain each block),
//! * how many consecutive nonces a bundle consumes,
//! * ETH / WETH balances, used as a *gate* only when explicitly enabled
//!   (simulation-only runs observe with a dummy searcher that has no mainnet
//!   inventory; gating that would silence the tape).

use std::sync::atomic::{AtomicU64, Ordering};

use alloy_primitives::{Address, U256};
use anyhow::Result;
use parking_lot::RwLock;
use serde_json::{json, Value};

use crate::rpc::RpcClient;
use crate::types::Opportunity;

pub struct Inventory {
    /// Next nonce available to this process, including private reservations.
    nonce: AtomicU64,
    /// Broadcasting remains blocked through this height when startup recovery
    /// could not prove that every prior private bundle was cancelled.
    blocked_until_block: AtomicU64,
    eth_wei: RwLock<U256>,
    weth_wei: RwLock<U256>,
    /// When true, opportunities that need more notional than we hold are
    /// skipped. Off by default; flipped on for live execution.
    pub gate: bool,
}

impl Inventory {
    pub fn new(gate: bool) -> Self {
        Self {
            nonce: AtomicU64::new(0),
            blocked_until_block: AtomicU64::new(0),
            eth_wei: RwLock::new(U256::ZERO),
            weth_wei: RwLock::new(U256::ZERO),
            gate,
        }
    }

    pub fn nonce(&self) -> u64 {
        self.nonce.load(Ordering::Relaxed)
    }

    pub fn set_nonce(&self, n: u64) {
        self.nonce.store(n, Ordering::Relaxed);
    }

    /// Chain refreshes may advance the nonce but must never erase a private
    /// reservation that is not visible in the public pending transaction pool.
    pub fn advance_chain_nonce(&self, n: u64) {
        self.nonce.fetch_max(n, Ordering::SeqCst);
    }

    pub fn reserve_nonces(&self, count: u64) -> u64 {
        self.nonce.fetch_add(count, Ordering::SeqCst)
    }

    /// Release only the newest reservation. The submission semaphore makes
    /// this the normal case; compare-exchange prevents creating a nonce gap if
    /// a future caller violates that ordering.
    pub fn release_nonces(&self, start: u64, count: u64) -> bool {
        self.nonce
            .compare_exchange(
                start.saturating_add(count),
                start,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_ok()
    }

    pub fn block_broadcast_until(&self, block: u64) {
        self.blocked_until_block.fetch_max(block, Ordering::SeqCst);
    }

    pub fn broadcast_available(&self, head: u64) -> bool {
        head > self.blocked_until_block.load(Ordering::Relaxed)
    }

    pub fn eth(&self) -> U256 {
        *self.eth_wei.read()
    }

    pub fn weth(&self) -> U256 {
        *self.weth_wei.read()
    }

    pub fn available(&self) -> U256 {
        self.eth().saturating_add(self.weth())
    }

    /// How many of *our* transactions a bundle contains. Victims are foreign
    /// and do not consume our nonce.
    pub fn legs(opp: &Opportunity) -> u64 {
        let mut n = 0u64;
        if !opp.front_calls.is_empty() {
            n += 1;
        }
        if !opp.back_calls.is_empty() {
            n += 1;
        }
        n
    }

    /// Starting nonce for this bundle. The back-run, if any, is `start + 1`.
    ///
    /// We do **not** increment on reservation: in simulation mode nothing is
    /// sent, so the on-chain nonce does not move, and two concurrent
    /// simulations are alternative futures that both start from the same
    /// nonce. The live lane commits separately: `Engine::submit_live_candidate`
    /// reserves and advances the nonce (`reserve_nonces`) only on the
    /// serialized submission path, re-simulating the exact reserved-nonce
    /// payload before anything is signed and sent.
    pub fn nonce_for(&self, _opp: &Opportunity) -> u64 {
        self.nonce()
    }

    /// The signer pays gas in ETH; non-flash strategies spend WETH held by the
    /// executor. Treating the searcher's ETH+WETH as one interchangeable pool
    /// let the live gate pass while the account that actually performs the
    /// transfer was empty.
    pub fn can_fund(&self, opp: &Opportunity) -> bool {
        if !self.gate {
            return true;
        }
        if self.eth().is_zero() {
            return false;
        }
        if !opp.flash_tokens.is_empty() {
            return true;
        }
        opp.notional_wei <= self.weth()
    }

    /// Pull nonce + balances from the execution node. Failures are logged and
    /// leave the last known values in place: a stale nonce is better than a
    /// panic on a flaky RPC, and the next block will try again.
    pub async fn refresh(
        &self,
        http: &RpcClient,
        searcher: Address,
        weth: Address,
        executor: Option<Address>,
    ) -> Result<()> {
        let who = format!("{searcher:?}");
        if let Ok(v) = http
            .call_raw(
                "eth_getTransactionCount",
                serde_json::json!([who.clone(), "pending"]),
            )
            .await
        {
            self.advance_chain_nonce(crate::types::parse_u64(&v));
        }
        if let Ok(v) = http
            .call_raw("eth_getBalance", serde_json::json!([who.clone(), "latest"]))
            .await
        {
            *self.eth_wei.write() = crate::types::parse_u256(&v);
        }
        if let Some(executor) = executor {
            if let Ok(bal) = erc20_balance(http, weth, executor).await {
                *self.weth_wei.write() = bal;
            }
        } else {
            *self.weth_wei.write() = U256::ZERO;
        }
        Ok(())
    }

    pub fn snapshot(&self) -> Value {
        json!({
            "nonce": self.nonce(),
            "broadcastBlockedUntilBlock": self.blocked_until_block.load(Ordering::Relaxed),
            "searcherGasEthWei": self.eth().to_string(),
            "executorWethWei": self.weth().to_string(),
            // Backward-compatible aliases for the current dashboard.
            "ethWei": self.eth().to_string(),
            "wethWei": self.weth().to_string(),
            "availableWei": self.available().to_string(),
            "gate": self.gate,
        })
    }
}

async fn erc20_balance(http: &RpcClient, token: Address, account: Address) -> Result<U256> {
    use alloy_sol_types::SolCall;
    let data = crate::dex::IERC20::balanceOfCall { account }.abi_encode();
    let v = http
        .call_raw(
            "eth_call",
            serde_json::json!([
                {"to": format!("{token:?}"), "data": format!("0x{}", hex::encode(data))},
                "latest"
            ]),
        )
        .await?;
    Ok(crate::types::parse_u256(&v))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{now_ms, Call, Strategy};

    fn opp(flash: bool, notional: u64, back: bool) -> Opportunity {
        Opportunity {
            id: "x".into(),
            strategy: Strategy::Sandwich,
            victim_hashes: vec![],
            front_calls: vec![Call::new(Address::ZERO, vec![1])],
            back_calls: if back {
                vec![Call::new(Address::ZERO, vec![2])]
            } else {
                vec![]
            },
            flash_tokens: if flash {
                vec![Address::with_last_byte(1)]
            } else {
                vec![]
            },
            flash_amounts: vec![],
            profit_token: Address::ZERO,
            expected_profit_wei: U256::ZERO,
            notional_wei: U256::from(notional),
            target_block: 1,
            created_at_ms: now_ms(),
            notes: String::new(),
            provenance: Default::default(),
        }
    }

    #[test]
    fn two_leg_bundle_consumes_two_nonces_but_starts_at_the_chain_nonce() {
        let inv = Inventory::new(false);
        inv.set_nonce(7);
        let o = opp(false, 1, true);
        assert_eq!(Inventory::legs(&o), 2);
        assert_eq!(inv.nonce_for(&o), 7);
        // Simulation does not burn the nonce.
        assert_eq!(inv.nonce(), 7);
    }

    #[test]
    fn single_leg_is_one_nonce() {
        assert_eq!(Inventory::legs(&opp(true, 1, false)), 1);
    }

    #[test]
    fn gating_is_off_by_default_even_with_empty_pockets() {
        let inv = Inventory::new(false);
        assert!(inv.can_fund(&opp(false, 1_000, false)));
    }

    #[test]
    fn gated_inventory_rejects_oversized_non_flash_trades() {
        let inv = Inventory::new(true);
        *inv.eth_wei.write() = U256::from(10u64);
        *inv.weth_wei.write() = U256::from(5u64);
        // Non-flash principal must already be held by the executor; gas ETH
        // cannot be counted as interchangeable WETH capital.
        assert!(inv.can_fund(&opp(false, 5, false)));
        assert!(!inv.can_fund(&opp(false, 6, false)));
        // Flash-funded trades skip the check.
        assert!(inv.can_fund(&opp(true, 1_000, false)));
    }

    #[test]
    fn nonce_reservations_release_only_from_the_tip() {
        let inv = Inventory::new(false);
        inv.set_nonce(10);
        let first = inv.reserve_nonces(2);
        assert_eq!(first, 10);
        assert_eq!(inv.nonce(), 12);
        assert!(inv.release_nonces(first, 2));
        assert_eq!(inv.nonce(), 10);

        let a = inv.reserve_nonces(1);
        let _b = inv.reserve_nonces(1);
        assert!(!inv.release_nonces(a, 1), "must not create a nonce gap");
        assert_eq!(inv.nonce(), 12);
    }

    #[test]
    fn unresolved_recovery_blocks_through_the_target() {
        let inv = Inventory::new(false);
        inv.block_broadcast_until(100);
        assert!(!inv.broadcast_available(99));
        assert!(!inv.broadcast_available(100));
        assert!(inv.broadcast_available(101));
    }

    #[test]
    fn snapshot_exports_the_dashboard_fields() {
        let inv = Inventory::new(true);
        inv.set_nonce(3);
        *inv.eth_wei.write() = U256::from(9u64);
        let s = inv.snapshot();
        assert_eq!(s["nonce"], 3);
        assert_eq!(s["ethWei"], "9");
        assert_eq!(s["gate"], true);
    }
}
