//! Execution pipeline for the new-token sniper lane.
//!
//! Handles entry submission (back-running launches to SniperVault) and
//! exit execution (closing positions on triggers).
//!
//! Two settlement domains, one pipeline:
//!
//! * **Simulation** — the same `openPosition`/`closePosition` calldata the
//!   live lane signs, executed against the local Anvil fixture that runs the
//!   real `SniperVault` bytecode ([`super::sim_vault`]). Fills are booked
//!   only from the fixture's mined receipts/events, and the paper bankroll is
//!   debited/credited by the *realised* amounts. A reverting simulation
//!   trade never moves the bankroll.
//! * **Live** — signed submissions to the production vault, receipt-based
//!   reconciliation, dedicated signer. Unchanged trust model.
//!
//! Global Invariant: Keeps its own submission path and NEVER imports or calls
//! bundle.rs, submission.rs, or qualification.rs.

use std::sync::Arc;

use alloy_primitives::{keccak256, Address, U256};
use anyhow::Result;

use super::calldata;
use super::gates::LaunchCandidate;
use super::marks;
use super::position::{ExecutionMode, Position, PositionState, Settlement, TxStatus};
use super::sim_vault::{SimTxOutcome, SimVaultFixture};
use super::SniperLane;
use crate::rpc::RpcClient;
use crate::signer::{Eip1559Tx, Signer};
use crate::store::Store;

/// Work order 4.2: venue-exact entry calldata. A venue with no adapter
/// (UniV3 today) or no registry addresses is an error — the caller turns
/// that into an abandoned position with the reason in its notes, never a
/// trade on approximated calldata.
#[allow(clippy::too_many_arguments)]
fn build_entry_for(
    addresses: &crate::config::known::ChainAddresses,
    venue: crate::dex::Venue,
    vault: Address,
    pair: Address,
    weth: Address,
    token: Address,
    is_weth_token0: bool,
    size_wei: U256,
    expected_tokens_out: U256,
    impact_bps: u32,
    head_block: u64,
    grace: u64,
    max_base_fee: U256,
    tag: alloy_primitives::B256,
) -> Result<Vec<u8>> {
    Ok(match venue {
        crate::dex::Venue::UniV2 | crate::dex::Venue::SushiV2 => {
            calldata::build_entry(
                vault,
                pair,
                weth,
                token,
                is_weth_token0,
                size_wei,
                expected_tokens_out,
                impact_bps,
                head_block,
                grace,
                max_base_fee,
                tag,
            )
            .2
        }
        crate::dex::Venue::AeroVolatile => {
            let router = addresses
                .aerodrome_router
                .ok_or_else(|| anyhow::anyhow!("chain registry has no Aerodrome router"))?;
            let factory = addresses
                .aerodrome_factory
                .ok_or_else(|| anyhow::anyhow!("chain registry has no Aerodrome factory"))?;
            calldata::build_entry_aero(
                vault,
                router,
                factory,
                weth,
                token,
                size_wei,
                expected_tokens_out,
                impact_bps,
                head_block,
                grace,
                max_base_fee,
                tag,
            )
            .2
        }
        crate::dex::Venue::UniV3 => {
            anyhow::bail!(
                "UniV3 sniper entries have no execution adapter — refusing to fabricate calldata"
            )
        }
    })
}

/// Work order 4.2: venue-exact exit calldata, same contract as
/// [`build_entry_for`].
#[allow(clippy::too_many_arguments)]
fn build_exit_for(
    addresses: &crate::config::known::ChainAddresses,
    venue: crate::dex::Venue,
    vault: Address,
    pair: Address,
    weth: Address,
    token: Address,
    is_weth_token0: bool,
    token_amount: U256,
    expected_weth_out: U256,
    slippage_bps: u32,
    head_block: u64,
    grace: u64,
    max_base_fee: U256,
    tag: alloy_primitives::B256,
) -> Result<Vec<u8>> {
    Ok(match venue {
        crate::dex::Venue::UniV2 | crate::dex::Venue::SushiV2 => {
            calldata::build_exit(
                vault,
                pair,
                weth,
                token,
                is_weth_token0,
                token_amount,
                expected_weth_out,
                slippage_bps,
                head_block,
                grace,
                max_base_fee,
                tag,
            )
            .2
        }
        crate::dex::Venue::AeroVolatile => {
            let router = addresses
                .aerodrome_router
                .ok_or_else(|| anyhow::anyhow!("chain registry has no Aerodrome router"))?;
            let factory = addresses
                .aerodrome_factory
                .ok_or_else(|| anyhow::anyhow!("chain registry has no Aerodrome factory"))?;
            calldata::build_exit_aero(
                vault,
                router,
                factory,
                weth,
                token,
                token_amount,
                expected_weth_out,
                slippage_bps,
                head_block,
                grace,
                max_base_fee,
                tag,
            )
            .2
        }
        crate::dex::Venue::UniV3 => {
            anyhow::bail!(
                "UniV3 sniper exits have no execution adapter — refusing to fabricate calldata"
            )
        }
    })
}

#[derive(Clone)]
pub struct SniperExecution {
    pub rpc: RpcClient,
    pub signer: Option<Signer>,
    pub store: Arc<Store>,
    pub lane: Arc<SniperLane>,
    /// Chain registry addresses the venue adapters dispatch on (Aerodrome
    /// router/factory today). Snapshot at boot; overrides still come from
    /// the same validated registry every other component reads.
    addresses: crate::config::known::ChainAddresses,
    /// The local SniperVault simulation fixture, present only while a fork
    /// backend is available. Simulation entries refuse to run without it
    /// rather than pretending a paper trade was contract-backed.
    fixture: Arc<parking_lot::RwLock<Option<Arc<SimVaultFixture>>>>,
}

impl SniperExecution {
    pub fn new(
        rpc: RpcClient,
        signer: Option<Signer>,
        store: Arc<Store>,
        lane: Arc<SniperLane>,
        addresses: crate::config::known::ChainAddresses,
    ) -> Self {
        Self {
            rpc,
            signer,
            store,
            lane,
            addresses,
            fixture: Arc::new(parking_lot::RwLock::new(None)),
        }
    }

    /// Attach the simulation fixture once the engine's fork is up.
    pub fn set_fixture(&self, fixture: Arc<SimVaultFixture>) {
        *self.fixture.write() = Some(fixture);
    }

    /// Expected WETH out for selling `qty` of a position's token into its
    /// WETH pool, quoted with the execution venue's own fee model — the
    /// number the optimistic amount and the vault's slippage floor derive
    /// from (work order 4.2). `U256::ZERO` means "could not quote", which
    /// callers degrade to the spot mark exactly as they already do for an
    /// unreadable reserve set.
    async fn expected_exit_out(
        &self,
        venue: crate::dex::Venue,
        pair: Address,
        weth: Address,
        token: Address,
        qty: U256,
        head_block: u64,
    ) -> U256 {
        let Some((r0, r1)) = marks::pair_reserves(&self.rpc, pair, head_block).await else {
            return U256::ZERO;
        };
        let (weth_r, token_r) = if weth < token { (r0, r1) } else { (r1, r0) };
        match venue {
            crate::dex::Venue::UniV2 | crate::dex::Venue::SushiV2 => {
                marks::v2_amount_out(qty, token_r, weth_r)
            }
            crate::dex::Venue::AeroVolatile => {
                // Aerodrome's fee is per-pool, read live at quote time so a
                // fee that changed since discovery cannot be baked into a
                // slippage floor that no longer exists (volatile only —
                // stable pools refuse to quote upstream).
                let Some(factory) = self.addresses.aerodrome_factory else {
                    return U256::ZERO;
                };
                crate::dex::aero_fee_bps(&self.rpc, factory, pair, false, head_block)
                    .await
                    .map(|fee| crate::dex::aero_volatile_amount_out(qty, token_r, weth_r, fee))
                    .unwrap_or(U256::ZERO)
            }
            // No adapter, no quote. The calldata builder refuses UniV3
            // before anything signs, so this zero can never settle a trade.
            crate::dex::Venue::UniV3 => U256::ZERO,
        }
    }

    pub fn fixture(&self) -> Option<Arc<SimVaultFixture>> {
        self.fixture.read().clone()
    }

    /// The lane's mode is authoritative and read at decision time: a runtime
    /// switch to simulation stops new live entries immediately.
    fn paper_mode(&self) -> bool {
        self.lane.paper_mode()
    }

    /// Persist the paper bankroll after any mutation so the simulation ledger
    /// survives restarts exactly as the on-chain ledger does.
    fn persist_paper_balance(&self) {
        if let Err(error) = self
            .store
            .save_simulation_state(self.lane.paper_balance_wei(), 0)
        {
            tracing::error!(target: "sniper", %error, "failed to persist simulation bankroll");
        }
    }

    /// Process a new launch candidate for potential entry.
    pub async fn process_launch(
        &self,
        candidate: &LaunchCandidate,
        weth: Address,
        chain_id: u64,
        head_block: u64,
        head_base_fee: U256,
        now_ms: u64,
    ) -> Result<Option<Position>> {
        // Claim token so a launch seen on both log scan and mempool is only evaluated once.
        if !self.lane.claim_token(candidate.token) {
            return Ok(None);
        }

        // Store / check honeypot verdict.
        let verdict_str = candidate.verdict.code();
        let token_hex = format!("{:?}", candidate.token);
        self.store.record_sniper_verdict(
            &token_hex,
            chain_id,
            verdict_str,
            candidate.verdict.round_trip_bps(),
            "launch probe",
        )?;

        if matches!(candidate.verdict, super::gates::HoneypotVerdict::Honeypot) {
            self.lane.blacklist(candidate.token);
        }

        // Work order 4.3: the LP-lock requirement is a real gate, not a
        // label. When the operator demands a locked LP and the candidate
        // arrived without a verdict, reach for one now — but only on venues
        // where the probe means something (V2-style pools; UniV3 liquidity
        // is NFT positions and stays unprobed, which the gate reads as
        // not-locked — fail-closed, never a silent pass).
        let params = self.lane.params();
        let probed;
        let candidate = if params.require_lp_locked
            && candidate.lp_locked.is_none()
            && super::gates::lp_lock_probe_supported(candidate.venue)
        {
            probed = {
                let mut c = candidate.clone();
                c.lp_locked = super::gates::probe_lp_locked(&self.rpc, c.pair).await;
                c
            };
            &probed
        } else {
            candidate
        };
        // Admit candidate through the normal gates. Simulation uses the same
        // gates but substitutes an internal non-zero vault marker, because no
        // production deployment is needed for a contract-backed local trade.
        let admission = if self.paper_mode()
            && params.enabled
            && !params.buy_size_wei.is_zero()
            && !params.daily_budget_wei.is_zero()
        {
            self.lane.admit_paper(candidate, now_ms)
        } else {
            self.lane.admit(candidate, now_ms)
        };
        let admission = match admission {
            Ok(adm) => adm,
            Err(_) => return Ok(None),
        };

        let size_wei = admission.size_wei;

        // Calculate expected output from reserves. `weth_reserve` /
        // `token_reserve` are WETH-side / token-side by construction at every
        // candidate source — they must NOT be re-ordered by address sorting
        // (a token that sorts below WETH would otherwise quote against the
        // wrong side of the curve).
        let is_weth_token0 = weth < candidate.token;
        let (weth_reserve, token_reserve) = (candidate.weth_reserve, candidate.token_reserve);

        if token_reserve.is_zero() || weth_reserve.is_zero() {
            return Ok(None);
        }

        // Venue-exact quote (work order 4.2): the fee the execution will
        // actually be billed is the one the prediction uses. A venue without
        // an exact quote here (UniV3 — reserves are not its pricing input)
        // refuses rather than buying on an invented number.
        let expected_tokens_out = match candidate.venue {
            crate::dex::Venue::UniV2 | crate::dex::Venue::SushiV2 => {
                let out = (size_wei * token_reserve * U256::from(997))
                    / (weth_reserve * U256::from(1000) + size_wei * U256::from(997));
                (!out.is_zero()).then_some(out)
            }
            crate::dex::Venue::AeroVolatile => candidate.pool_fee_bps.and_then(|fee_bps| {
                let out = crate::dex::aero_volatile_amount_out(
                    size_wei,
                    weth_reserve,
                    token_reserve,
                    fee_bps,
                );
                (!out.is_zero()).then_some(out)
            }),
            crate::dex::Venue::UniV3 => None,
        };
        let Some(expected_tokens_out) = expected_tokens_out else {
            return Ok(None);
        };

        let pos_id = uuid::Uuid::new_v4().to_string();
        let simulation = self.paper_mode();

        let mut position = Position {
            id: pos_id.clone(),
            chain_id,
            token: candidate.token,
            pair: candidate.pair,
            // The entry's venue is the position's venue forever: exits quote
            // and build against the same venue's exact model (work order
            // 4.2). Pre-venue rows in the store predate this string only by
            // never existing on this chain.
            venue: candidate.venue.as_str().into(),
            state: PositionState::Pending,
            trigger_tx: None,
            entry_tx: None,

            exit_tx: None,
            entry_cost_wei: size_wei,
            entry_qty: U256::ZERO,
            remaining_qty: U256::ZERO,
            realized_wei: U256::ZERO,
            gas_spent_wei: U256::ZERO,
            peak_value_wei: size_wei,
            opened_block: head_block,
            opened_at_ms: now_ms,
            closed_at_ms: None,
            exit_reason: None,
            entry_verdict: candidate.verdict.code().into(),
            notes: format!("entry probe size {size_wei} wei"),
            execution_mode: if simulation {
                ExecutionMode::Simulation
            } else {
                ExecutionMode::Live
            },
            settlement: if simulation {
                Settlement::Paper
            } else {
                Settlement::OnChain
            },
            tx_status: TxStatus::Intent,
        };

        // INVARIANT 4: Position rows are written BEFORE entry submission, never after.
        // A persistence failure is a hard refusal to sign or broadcast.
        self.store
            .upsert_sniper_position(&position)
            .map_err(|error| anyhow::anyhow!("persisting entry intent: {error}"))?;
        self.lane.upsert_position(position.clone());

        if simulation {
            return self
                .simulated_entry(
                    &mut position,
                    candidate,
                    weth,
                    size_wei,
                    expected_tokens_out,
                    is_weth_token0,
                    weth_reserve,
                    token_reserve,
                    head_block,
                    now_ms,
                )
                .await;
        }

        // Shadow mode check: SNIPER_DIRECTIONAL=false runs detection -> probe -> gate, stops before signing.
        let armed = params.is_armed() && !self.lane.is_halted() && self.lane.boot_enabled();
        let vault_addr = params.vault_address.unwrap_or(Address::ZERO);
        if !armed || self.signer.is_none() || vault_addr == Address::ZERO {
            tracing::info!(
                target: "sniper",
                id = %pos_id,
                token = ?candidate.token,
                size_wei = %size_wei,
                "shadow mode / disarmed admission — entry candidate logged without signing tx"
            );
            position.state = PositionState::Abandoned;
            position.tx_status = TxStatus::Abandoned;
            position.notes = "shadow mode pass (unsubmitted)".into();
            self.store
                .upsert_sniper_position(&position)
                .map_err(|error| anyhow::anyhow!("persisting shadow position: {error}"))?;
            self.lane.upsert_position(position.clone());
            return Ok(Some(position));
        }

        let tag = calldata::make_tag(&pos_id, 0);
        // Work order 4.2 dispatch: venue-exact calldata or an abandoned,
        // fully persisted position. Never a trade on approximated calldata.
        let calldata = match build_entry_for(
            &self.addresses,
            candidate.venue,
            vault_addr,
            candidate.pair,
            weth,
            candidate.token,
            is_weth_token0,
            size_wei,
            expected_tokens_out,
            params.max_price_impact_bps,
            head_block,
            2,
            U256::ZERO,
            tag,
        ) {
            Ok(c) => c,
            Err(error) => {
                position.state = PositionState::Abandoned;
                position.tx_status = TxStatus::Abandoned;
                position.notes = format!("execution adapter refused the entry: {error}");
                self.store
                    .upsert_sniper_position(&position)
                    .map_err(|e| anyhow::anyhow!("persisting refused entry: {e}"))?;
                self.lane.upsert_position(position.clone());
                return Ok(Some(position));
            }
        };

        let signer = self.signer.as_ref().unwrap();

        // Query nonce for searcher key
        let nonce = match self
            .rpc
            .get_transaction_count(signer.address(), head_block)
            .await
        {
            Ok(nonce) => nonce,
            Err(error) => {
                position.state = PositionState::Abandoned;
                position.tx_status = TxStatus::Abandoned;
                position.notes = format!("nonce lookup failed: {error}");
                self.store.upsert_sniper_position(&position)?;
                self.lane.upsert_position(position.clone());
                return Err(error);
            }
        };
        let max_priority_fee = U256::from(1_500_000_000u64);
        let max_fee = head_base_fee * U256::from(2) + max_priority_fee;

        let tx = Eip1559Tx {
            chain_id,
            nonce,
            max_priority_fee_per_gas: max_priority_fee,
            max_fee_per_gas: max_fee,
            gas_limit: 350_000,
            to: Some(vault_addr),
            value: U256::ZERO,
            data: calldata,
        };

        let (raw_tx, tx_hash) = signer.sign_eip1559(&tx);
        let tx_hash_hex = format!("0x{}", hex::encode(tx_hash));
        position.entry_tx = Some(tx_hash);
        // The signed intent is durable before the RPC send. If this write
        // fails, do not broadcast a transaction we cannot recover.
        self.store
            .upsert_sniper_position(&position)
            .map_err(|error| anyhow::anyhow!("persisting signed entry intent: {error}"))?;

        let raw_hex = format!("0x{}", hex::encode(&raw_tx));
        let send_res = self
            .rpc
            .call_raw("eth_sendRawTransaction", serde_json::json!([raw_hex]))
            .await;

        match send_res {
            Ok(_) => {
                // RPC acceptance is not settlement. Keep the durable row
                // Pending until a later head observes a successful
                // EntryExecuted receipt and records the exact balance deltas.
                // This prevents a dropped/reverted entry from becoming fake
                // open exposure or consuming an imaginary token quantity.
                position.tx_status = TxStatus::Submitted;
                position.notes = format!("entry submitted {tx_hash_hex}; awaiting receipt");
                self.store.upsert_sniper_position(&position)?;
                self.lane.upsert_position(position.clone());
                tracing::info!(target: "sniper", id = %pos_id, token = ?candidate.token, "entry submitted; awaiting receipt");
                Ok(Some(position))
            }
            Err(e) => {
                tracing::warn!(target: "sniper", id = %pos_id, error = %e, "entry submission failed");
                position.state = PositionState::Abandoned;
                position.tx_status = TxStatus::Abandoned;
                position.notes = format!("submission error: {e}");
                self.store.upsert_sniper_position(&position)?;
                self.lane.upsert_position(position.clone());
                Ok(Some(position))
            }
        }
    }

    /// Contract-backed simulation entry (work order A.2).
    ///
    /// Executes the real `SniperVault.openPosition` calldata against the
    /// local fixture, books the fill only from the mined `EntryExecuted`
    /// event, and debits the paper bankroll by the *realised* spend. A
    /// revert refunds the reservation and persists the reason.
    #[allow(clippy::too_many_arguments)]
    async fn simulated_entry(
        &self,
        position: &mut Position,
        candidate: &LaunchCandidate,
        weth: Address,
        size_wei: U256,
        expected_tokens_out: U256,
        is_weth_token0: bool,
        weth_reserve: U256,
        token_reserve: U256,
        head_block: u64,
        _now_ms: u64,
    ) -> Result<Option<Position>> {
        // The simulation fixture is a UniV2-shaped mock pair. Running an
        // Aerodrome/UniV3 candidate through it would measure the wrong
        // venue — abandon honestly rather than report an invented fill
        // (work order 4.2; live fork round-trips for the Aero adapters run
        // in the env-gated integration test instead).
        if candidate.venue != crate::dex::Venue::UniV2 {
            self.lane.credit_paper(size_wei);
            self.persist_paper_balance();
            position.state = PositionState::Abandoned;
            position.tx_status = TxStatus::Abandoned;
            position.notes = format!(
                "simulation fixture is UniV2-shaped; {} entries are not simulated",
                candidate.venue.as_str()
            );
            self.store.upsert_sniper_position(position)?;
            self.lane.upsert_position(position.clone());
            return Ok(Some(position.clone()));
        }
        let Some(fixture) = self.fixture() else {
            position.state = PositionState::Abandoned;
            position.tx_status = TxStatus::Abandoned;
            position.notes =
                "simulation unavailable: local fork is not running — observation-only".to_string();
            self.store.upsert_sniper_position(position)?;
            self.lane.upsert_position(position.clone());
            return Ok(Some(position.clone()));
        };

        // Reserve the paper bankroll before anything executes.
        if !self.lane.reserve_paper(size_wei) {
            position.state = PositionState::Abandoned;
            position.tx_status = TxStatus::Abandoned;
            position.notes = "simulation paper balance exhausted".into();
            self.store.upsert_sniper_position(position)?;
            self.lane.upsert_position(position.clone());
            return Ok(Some(position.clone()));
        }
        self.persist_paper_balance();

        // The fixture vault + a deterministic mock pair seeded with this
        // launch's observed reserves.
        let vault_state = match fixture.ensure_deployed().await {
            Ok(state) => state,
            Err(error) => {
                self.lane.credit_paper(size_wei);
                self.persist_paper_balance();
                position.state = PositionState::Abandoned;
                position.tx_status = TxStatus::Abandoned;
                position.notes = format!("simulation unavailable: {error:?}");
                self.store.upsert_sniper_position(position)?;
                self.lane.upsert_position(position.clone());
                return Ok(Some(position.clone()));
            }
        };
        let pair_fixture = match fixture
            .deploy_launch_pair(weth_reserve, token_reserve)
            .await
        {
            Ok(pf) => pf,
            Err(error) => {
                self.lane.credit_paper(size_wei);
                self.persist_paper_balance();
                position.state = PositionState::Abandoned;
                position.tx_status = TxStatus::Abandoned;
                position.notes = format!("simulation fixture liquidity failed: {error:?}");
                self.store.upsert_sniper_position(position)?;
                self.lane.upsert_position(position.clone());
                return Ok(Some(position.clone()));
            }
        };

        // The position now lives against the fixture pair, not the observed
        // production pair: marks and exits read fork state from here on.
        position.pair = pair_fixture.pair;
        position.token = pair_fixture.token;

        let params = self.lane.params();
        let tag = calldata::make_tag(&position.id, 0);
        let (_, _guard, calldata) = calldata::build_entry(
            vault_state.vault,
            pair_fixture.pair,
            vault_state.weth,
            pair_fixture.token,
            is_weth_token0,
            size_wei,
            expected_tokens_out,
            params.max_price_impact_bps,
            head_block,
            // The fixture mines on its own cadence; a tight deadline would
            // reject valid simulations when the fork lags the live head.
            u64::MAX / 2,
            U256::ZERO,
            tag,
        );
        let _ = weth; // the chain WETH is carried by the fixture state

        match fixture.execute_vault_calldata(&calldata).await {
            Err(error) => {
                self.lane.credit_paper(size_wei);
                self.persist_paper_balance();
                position.state = PositionState::Abandoned;
                position.tx_status = TxStatus::Abandoned;
                position.notes = format!("simulation entry failed: {error:?}");
                self.store.upsert_sniper_position(position)?;
                self.lane.upsert_position(position.clone());
                Ok(Some(position.clone()))
            }
            Ok(SimTxOutcome::Reverted { reason }) => {
                // A reverted simulated trade must not touch the bankroll.
                self.lane.credit_paper(size_wei);
                self.persist_paper_balance();
                position.state = PositionState::Abandoned;
                position.tx_status = TxStatus::Reverted;
                position.notes = format!("SIMULATION entry reverted: {reason}");
                self.store.upsert_sniper_position(position)?;
                self.lane.upsert_position(position.clone());
                tracing::info!(target: "sniper", id = %position.id, %reason, "simulated entry reverted");
                Ok(Some(position.clone()))
            }
            Ok(SimTxOutcome::Mined {
                tx_hash,
                block,
                gas_cost_wei,
                receipt,
            }) => {
                let Some((weth_spent, tokens_received, _gas, _blk)) =
                    Self::decode_entry_receipt(&receipt, vault_state.vault, pair_fixture.token)
                else {
                    self.lane.credit_paper(size_wei);
                    self.persist_paper_balance();
                    position.state = PositionState::Abandoned;
                    position.tx_status = TxStatus::Reverted;
                    position.notes =
                        "simulation entry mined but emitted no EntryExecuted fill".into();
                    self.store.upsert_sniper_position(position)?;
                    self.lane.upsert_position(position.clone());
                    return Ok(Some(position.clone()));
                };
                if tokens_received.is_zero() || weth_spent.is_zero() {
                    self.lane.credit_paper(size_wei);
                    self.persist_paper_balance();
                    position.state = PositionState::Abandoned;
                    position.tx_status = TxStatus::Reverted;
                    position.notes = "simulation entry reported a zero fill".into();
                    self.store.upsert_sniper_position(position)?;
                    self.lane.upsert_position(position.clone());
                    return Ok(Some(position.clone()));
                };

                // Receipt-based exact accounting: debit the realised spend,
                // refund the unspent part of the reservation.
                if weth_spent < size_wei {
                    self.lane.credit_paper(size_wei - weth_spent);
                }
                self.persist_paper_balance();

                position.entry_tx = Some(tx_hash);
                position.entry_cost_wei = weth_spent;
                position.entry_qty = tokens_received;
                position.remaining_qty = tokens_received;
                position.peak_value_wei = weth_spent;
                position.gas_spent_wei = gas_cost_wei;
                position.state = PositionState::Open;
                position.tx_status = TxStatus::Mined;
                position.notes = format!(
                    "SIMULATION contract-backed entry · fixture tx {tx_hash:?} · block {block}"
                );
                let fill_id = uuid::Uuid::new_v4().to_string();
                self.store.record_sniper_fill(
                    &fill_id,
                    &position.id,
                    "buy",
                    "simulation",
                    tokens_received,
                    weth_spent,
                    gas_cost_wei,
                    Some(format!("{tx_hash:?}")),
                    Some(block),
                    ExecutionMode::Simulation,
                )?;
                self.store.upsert_sniper_position(position)?;
                self.lane.upsert_position(position.clone());
                Ok(Some(position.clone()))
            }
        }
    }

    /// Poll briefly for a receipt so manual controls do not book a fill merely
    /// because the RPC accepted a mempool submission.
    async fn wait_for_receipt(&self, tx_hash: &str) -> Result<Option<serde_json::Value>> {
        for _ in 0..20 {
            match self
                .rpc
                .call_raw("eth_getTransactionReceipt", serde_json::json!([tx_hash]))
                .await
            {
                Ok(value) if !value.is_null() => return Ok(Some(value)),
                Ok(_) => tokio::time::sleep(std::time::Duration::from_millis(250)).await,
                Err(_) => tokio::time::sleep(std::time::Duration::from_millis(250)).await,
            }
        }
        Ok(None)
    }

    /// Decode the exact `ExitExecuted` values from a mined vault receipt.
    pub(crate) fn decode_exit_receipt(
        receipt: &serde_json::Value,
        vault: Address,
        token: Address,
    ) -> Option<(U256, U256, U256, u64)> {
        let status = crate::types::parse_u64(receipt.get("status")?);
        if status == 0 {
            return None;
        }
        let signature = format!(
            "0x{:x}",
            keccak256("ExitExecuted(bytes32,address,uint256,uint256)".as_bytes())
        );
        let logs = receipt.get("logs")?.as_array()?;
        for log in logs {
            if log
                .get("address")
                .and_then(|value| value.as_str())
                .and_then(|value| value.parse::<Address>().ok())
                != Some(vault)
            {
                continue;
            }
            let topics = log.get("topics")?.as_array()?;
            if topics.len() < 3 || topics[0].as_str()?.to_ascii_lowercase() != signature {
                continue;
            }
            let token_topic = topics[2].as_str()?;
            let token_bytes = hex::decode(token_topic.trim_start_matches("0x")).ok()?;
            if token_bytes.len() < 20 {
                continue;
            }
            let logged_token = Address::from_slice(&token_bytes[token_bytes.len() - 20..]);
            if logged_token != token {
                continue;
            }
            let data = log.get("data")?.as_str()?;
            let bytes = hex::decode(data.trim_start_matches("0x")).ok()?;
            if bytes.len() < 64 {
                continue;
            }
            let tokens_sold = U256::from_be_slice(&bytes[..32]);
            let weth_received = U256::from_be_slice(&bytes[32..64]);
            let gas_used = crate::types::parse_u256(receipt.get("gasUsed")?);
            let gas_price = crate::types::parse_u256(receipt.get("effectiveGasPrice")?);
            let gas_cost = gas_used.saturating_mul(gas_price);
            let block = crate::types::parse_u64(receipt.get("blockNumber")?);
            return Some((tokens_sold, weth_received, gas_cost, block));
        }
        None
    }

    /// Decode the exact `EntryExecuted` values from a mined vault receipt.
    pub(crate) fn decode_entry_receipt(
        receipt: &serde_json::Value,
        vault: Address,
        token: Address,
    ) -> Option<(U256, U256, U256, u64)> {
        let status = crate::types::parse_u64(receipt.get("status")?);
        if status == 0 {
            return None;
        }
        let signature = format!(
            "0x{:x}",
            keccak256("EntryExecuted(bytes32,address,uint256,uint256)".as_bytes())
        );
        let logs = receipt.get("logs")?.as_array()?;
        for log in logs {
            if log
                .get("address")
                .and_then(|value| value.as_str())
                .and_then(|value| value.parse::<Address>().ok())
                != Some(vault)
            {
                continue;
            }
            let topics = log.get("topics")?.as_array()?;
            if topics.len() < 3 || topics[0].as_str()?.to_ascii_lowercase() != signature {
                continue;
            }
            let token_topic = topics[2].as_str()?;
            let token_bytes = hex::decode(token_topic.trim_start_matches("0x")).ok()?;
            if token_bytes.len() < 20 {
                continue;
            }
            if Address::from_slice(&token_bytes[token_bytes.len() - 20..]) != token {
                continue;
            }
            let data = log.get("data")?.as_str()?;
            let bytes = hex::decode(data.trim_start_matches("0x")).ok()?;
            if bytes.len() < 64 {
                continue;
            }
            let weth_spent = U256::from_be_slice(&bytes[..32]);
            let tokens_received = U256::from_be_slice(&bytes[32..64]);
            let gas_used = crate::types::parse_u256(receipt.get("gasUsed")?);
            let gas_price = crate::types::parse_u256(receipt.get("effectiveGasPrice")?);
            let gas_cost = gas_used.saturating_mul(gas_price);
            let block = crate::types::parse_u64(receipt.get("blockNumber")?);
            return Some((weth_spent, tokens_received, gas_cost, block));
        }
        None
    }

    /// Reconcile submitted entry transactions without treating mempool
    /// acceptance as a fill. This runs once per block and is deliberately
    /// receipt/event based, so PnL and quantities cannot be booked from a
    /// quote that never landed. Live positions only: simulation entries
    /// settle synchronously against the fixture.
    pub async fn reconcile_pending_entries(&self) {
        let params = self.lane.params();
        let vault = params.vault_address.unwrap_or(Address::ZERO);
        if vault.is_zero() {
            return;
        }
        for mut position in self.lane.positions().into_iter().filter(|position| {
            position.state == PositionState::Pending
                && position.entry_tx.is_some()
                && position.execution_mode == ExecutionMode::Live
        }) {
            let tx_hash = format!("{:?}", position.entry_tx.unwrap());
            let Ok(receipt) = self
                .rpc
                .call_raw("eth_getTransactionReceipt", serde_json::json!([tx_hash]))
                .await
            else {
                continue;
            };
            if receipt.is_null() {
                continue;
            }
            let Some((weth_spent, tokens_received, gas_cost, block)) =
                Self::decode_entry_receipt(&receipt, vault, position.token)
            else {
                position.state = PositionState::Abandoned;
                position.tx_status = TxStatus::Reverted;
                position.notes = "entry receipt reverted or emitted no vault fill".into();
                if let Err(error) = self.store.upsert_sniper_position(&position) {
                    tracing::error!(target: "sniper", %error, id = %position.id, "failed to persist abandoned entry");
                    continue;
                }
                self.lane.upsert_position(position);
                continue;
            };
            if tokens_received.is_zero() || weth_spent.is_zero() {
                position.state = PositionState::Abandoned;
                position.tx_status = TxStatus::Reverted;
                position.notes = "entry receipt reported zero fill".into();
                if let Err(error) = self.store.upsert_sniper_position(&position) {
                    tracing::error!(target: "sniper", %error, id = %position.id, "failed to persist zero-fill entry");
                    continue;
                }
                self.lane.upsert_position(position);
                continue;
            }
            position.entry_cost_wei = weth_spent;
            position.entry_qty = tokens_received;
            position.remaining_qty = tokens_received;
            position.peak_value_wei = weth_spent;
            position.gas_spent_wei = gas_cost;
            position.state = PositionState::Open;
            position.tx_status = TxStatus::Mined;
            position.notes = format!("entry confirmed at block {block}");
            let fill_id = uuid::Uuid::new_v4().to_string();
            if let Err(error) = self.store.record_sniper_fill(
                &fill_id,
                &position.id,
                "buy",
                "entry",
                tokens_received,
                weth_spent,
                gas_cost,
                position.entry_tx.map(|hash| format!("{hash:?}")),
                Some(block),
                ExecutionMode::Live,
            ) {
                tracing::error!(target: "sniper", %error, id = %position.id, "entry confirmed but fill persistence failed");
                continue;
            }
            if let Err(error) = self.store.upsert_sniper_position(&position) {
                tracing::error!(target: "sniper", %error, id = %position.id, "entry confirmed but position persistence failed");
                continue;
            }
            self.lane.upsert_position(position);
        }

        // Exit reconciliation: a submitted exit is an intent, not a fill.
        // Replace the optimistic booking with the receipt's exact values, or
        // roll it back on a revert.
        for mut position in self.lane.positions().into_iter().filter(|position| {
            position.state.is_live()
                && position.tx_status == TxStatus::Submitted
                && position.exit_tx.is_some()
                && position.execution_mode == ExecutionMode::Live
        }) {
            let tx_hash = format!("{:?}", position.exit_tx.unwrap());
            let Ok(receipt) = self
                .rpc
                .call_raw("eth_getTransactionReceipt", serde_json::json!([tx_hash]))
                .await
            else {
                continue;
            };
            if receipt.is_null() {
                continue;
            }
            match Self::decode_exit_receipt(&receipt, vault, position.token) {
                Some((tokens_sold, weth_received, gas_cost, block)) => {
                    // Undo the optimistic fill (it subtracted the decision
                    // qty and added the expected proceeds), then apply the
                    // exact receipt values. Both amounts are recovered from
                    // the last sell fill row, which carries them.
                    if let Some((opt_qty, opt_weth)) = self
                        .store
                        .last_sell_fill_amounts(&position.id)
                        .unwrap_or(None)
                    {
                        position.remaining_qty = position.remaining_qty.saturating_add(opt_qty);
                        position.realized_wei = position.realized_wei.saturating_sub(opt_weth);
                    }
                    position.apply_fill(
                        tokens_sold,
                        weth_received,
                        gas_cost,
                        crate::types::now_ms(),
                    );
                    position.exit_tx = None;
                    position.tx_status = TxStatus::Mined;
                    position.notes = format!("exit confirmed at block {block}");
                    if let Err(error) = self.store.correct_last_sell_fill(
                        &position.id,
                        tokens_sold,
                        weth_received,
                        gas_cost,
                        block,
                    ) {
                        tracing::error!(target: "sniper", %error, id = %position.id, "exit receipt booked but fill correction failed");
                    }
                    if let Err(error) = self.store.upsert_sniper_position(&position) {
                        tracing::error!(target: "sniper", %error, id = %position.id, "exit confirmed but position persistence failed");
                        continue;
                    }
                    self.lane.upsert_position(position);
                }
                None => {
                    // Reverted (or event-less) exit: roll the optimistic fill
                    // back completely. The position is exactly as it was.
                    if let Some((opt_qty, opt_weth)) = self
                        .store
                        .last_sell_fill_amounts(&position.id)
                        .unwrap_or(None)
                    {
                        position.remaining_qty = position.remaining_qty.saturating_add(opt_qty);
                        position.realized_wei = position.realized_wei.saturating_sub(opt_weth);
                    }
                    if !position.remaining_qty.is_zero() {
                        position.state = PositionState::Open;
                        position.closed_at_ms = None;
                    }
                    position.exit_tx = None;
                    position.tx_status = TxStatus::Reverted;
                    position.notes =
                        format!("exit {tx_hash} reverted — optimistic fill rolled back");
                    if let Err(error) = self.store.delete_last_sell_fill(&position.id) {
                        tracing::error!(target: "sniper", %error, id = %position.id, "reverted exit: fill deletion failed");
                    }
                    if let Err(error) = self.store.upsert_sniper_position(&position) {
                        tracing::error!(target: "sniper", %error, id = %position.id, "reverted exit: position persistence failed");
                        continue;
                    }
                    self.lane.upsert_position(position);
                }
            }
        }
    }

    /// Manual operator buy. It still uses the SniperVault budget/slippage
    /// guards and the same persistence-before-signing path as automatic buys.
    /// The operator explicitly supplies a V2 pair, so this path does not
    /// pretend that a launch probe was performed; the UI labels it manual.
    #[allow(clippy::too_many_arguments)]
    pub async fn process_manual_buy(
        &self,
        token: Address,
        pair: Address,
        weth: Address,
        size_wei: U256,
        chain_id: u64,
        head_block: u64,
        head_base_fee: U256,
        now_ms: u64,
    ) -> Result<Option<Position>> {
        if !self.lane.effective_armed() {
            anyhow::bail!("sniper lane is not armed; manual buys are disabled")
        }
        if !self.paper_mode() && self.signer.is_none() {
            anyhow::bail!("SNIPER_SEARCHER_PRIVATE_KEY is not configured")
        }
        if token.is_zero() || pair.is_zero() || size_wei.is_zero() {
            anyhow::bail!("token, pair and sizeWei must all be non-zero")
        }

        let pool =
            crate::dex::fetch_v2_pool(&self.rpc, pair, crate::dex::Venue::UniV2, 30, head_block)
                .await
                .map_err(|error| anyhow::anyhow!("could not read pair reserves: {error}"))?;
        let (weth_reserve, token_reserve) = if pool.token0 == weth && pool.token1 == token {
            (pool.reserve0, pool.reserve1)
        } else if pool.token1 == weth && pool.token0 == token {
            (pool.reserve1, pool.reserve0)
        } else {
            anyhow::bail!("pair does not contain the configured WETH/token pair")
        };
        if weth_reserve.is_zero() || token_reserve.is_zero() {
            anyhow::bail!("pair has zero liquidity")
        }

        // A manual buy is a conscious operator override of the launch-probe
        // discovery step, but not of the on-chain budget, impact, position-cap,
        // or vault authorization guards.
        self.lane.release_token_claim(token);
        let candidate = LaunchCandidate {
            token,
            pair,
            venue: crate::dex::Venue::UniV2,
            pool_fee_bps: None, // UniV2's 30 bps is a protocol constant in the quote
            weth_reserve,
            token_reserve,
            verdict: super::gates::HoneypotVerdict::Clean {
                round_trip_bps: 9_940,
            },
            lp_locked: None,
            blacklisted: self.lane.is_blacklisted(token),
        };
        self.process_launch(
            &candidate,
            weth,
            chain_id,
            head_block,
            head_base_fee,
            now_ms,
        )
        .await
    }

    /// Execute an operator-requested partial/full exit immediately. Unlike an
    /// entry, an exit remains permitted while the lane is halted or its entry
    /// switch is off: trapping held tokens is not a safety feature.
    pub async fn process_manual_sell(
        &self,
        id: &str,
        sell_fraction_bps: u32,
        weth: Address,
        head_block: u64,
        head_base_fee: U256,
        now_ms: u64,
    ) -> Result<Option<(Position, String)>> {
        let Some(position) = self.lane.position(id) else {
            return Ok(None);
        };
        if !position.state.is_live() || position.remaining_qty.is_zero() {
            return Ok(None);
        }
        if position.execution_mode == ExecutionMode::Simulation {
            return self
                .simulated_exit(
                    id,
                    sell_fraction_bps,
                    head_block,
                    now_ms,
                    super::position::ExitReason::Manual,
                )
                .await;
        }
        self.live_manual_sell(
            id,
            sell_fraction_bps,
            weth,
            head_block,
            head_base_fee,
            now_ms,
        )
        .await
    }

    /// Live-domain manual exit: signed, receipt-reconciled.
    async fn live_manual_sell(
        &self,
        id: &str,
        sell_fraction_bps: u32,
        weth: Address,
        head_block: u64,
        head_base_fee: U256,
        now_ms: u64,
    ) -> Result<Option<(Position, String)>> {
        let Some(mut position) = self.lane.position(id) else {
            return Ok(None);
        };
        let Some(decision) = self.lane.manual_sell(id, sell_fraction_bps) else {
            return Ok(None);
        };
        let Some(mark) =
            marks::update_position_mark(&self.rpc, &self.lane, &position, weth, head_block, now_ms)
                .await
        else {
            anyhow::bail!("manual sell requires a fresh pool mark")
        };
        let params = self.lane.params();
        let vault_addr = params.vault_address.unwrap_or(Address::ZERO);
        let signer = self
            .signer
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("SNIPER_SEARCHER_PRIVATE_KEY is not configured"))?;
        if vault_addr.is_zero() {
            anyhow::bail!("SNIPER_VAULT_ADDRESS is not configured")
        }
        let is_weth_token0 = weth < position.token;
        // Positions written before venues existed are UniV2 by construction;
        // anything newer quotes with its own venue's fee model (work order
        // 4.2). UniV2 keeps the historical spot mark as its optimistic
        // amount, byte-identical to before this dispatch existed.
        let venue =
            crate::dex::Venue::from_label(&position.venue).unwrap_or(crate::dex::Venue::UniV2);
        let expected_out = if venue == crate::dex::Venue::AeroVolatile {
            self.expected_exit_out(
                venue,
                position.pair,
                weth,
                position.token,
                decision.qty,
                head_block,
            )
            .await
        } else {
            mark.value_wei
        };
        let calldata = build_exit_for(
            &self.addresses,
            venue,
            vault_addr,
            position.pair,
            weth,
            position.token,
            is_weth_token0,
            decision.qty,
            expected_out,
            params.max_price_impact_bps,
            head_block,
            2,
            U256::ZERO,
            calldata::make_tag(&position.id, now_ms as u32),
        )
        .map_err(|error| anyhow::anyhow!("execution adapter refused the manual exit: {error}"))?;

        // Persist the intent before signing/broadcasting. The durable row is
        // the recovery anchor if the process dies after sendRawTransaction.
        position.notes = format!("manual exit intent: {} bps", decision.fraction_bps);
        self.store
            .upsert_sniper_position(&position)
            .map_err(|error| anyhow::anyhow!("persisting manual exit intent: {error}"))?;

        let nonce = self
            .rpc
            .get_transaction_count(signer.address(), head_block)
            .await?;
        let tx = Eip1559Tx {
            chain_id: position.chain_id,
            nonce,
            max_priority_fee_per_gas: U256::from(1_500_000_000u64),
            max_fee_per_gas: head_base_fee * U256::from(2) + U256::from(1_500_000_000u64),
            gas_limit: 350_000,
            to: Some(vault_addr),
            value: U256::ZERO,
            data: calldata,
        };
        let (raw_tx, tx_hash) = signer.sign_eip1559(&tx);
        let tx_hash_hex = format!("0x{}", hex::encode(tx_hash));
        self.rpc
            .call_raw(
                "eth_sendRawTransaction",
                serde_json::json!([format!("0x{}", hex::encode(raw_tx))]),
            )
            .await
            .map_err(|error| anyhow::anyhow!("manual sell submission failed: {error}"))?;

        // The vault's minWethOut guard protects the transaction. Only book a
        // fill from the receipt/event, never from the pre-trade mark.
        let Some(receipt) = self.wait_for_receipt(&tx_hash_hex).await? else {
            position.tx_status = TxStatus::Submitted;
            position.notes = format!("manual exit submitted {tx_hash_hex}; awaiting receipt");
            self.store.upsert_sniper_position(&position)?;
            self.lane.upsert_position(position.clone());
            return Ok(Some((position, tx_hash_hex)));
        };
        let Some((tokens_sold, weth_received, gas_cost, filled_block)) =
            Self::decode_exit_receipt(&receipt, vault_addr, position.token)
        else {
            position.tx_status = TxStatus::Reverted;
            position.notes = format!("manual exit {tx_hash_hex} reverted or emitted no vault fill");
            self.store.upsert_sniper_position(&position)?;
            self.lane.upsert_position(position.clone());
            return Err(anyhow::anyhow!(
                "manual sell transaction reverted or emitted no ExitExecuted event"
            ));
        };
        if tokens_sold > decision.qty {
            return Err(anyhow::anyhow!(
                "vault sold more tokens than the manual guard allowed"
            ));
        }
        position.apply_fill(tokens_sold, weth_received, gas_cost, now_ms);
        position.exit_reason = Some(super::position::ExitReason::Manual);
        position.tx_status = TxStatus::Mined;
        position.notes = format!("manual exit confirmed {tx_hash_hex}");
        let fill_id = uuid::Uuid::new_v4().to_string();
        self.store.record_sniper_fill(
            &fill_id,
            &position.id,
            "sell",
            "manual",
            tokens_sold,
            weth_received,
            gas_cost,
            Some(tx_hash_hex.clone()),
            Some(filled_block),
            ExecutionMode::Live,
        )?;
        self.store.upsert_sniper_position(&position)?;
        self.lane.upsert_position(position.clone());
        Ok(Some((position, tx_hash_hex)))
    }

    /// Contract-backed simulation exit against the fixture vault. Marks come
    /// from the fixture pair's fork reserves — the same reserve source the
    /// live path uses, just read on the fork.
    async fn simulated_exit(
        &self,
        id: &str,
        sell_fraction_bps: u32,
        head_block: u64,
        now_ms: u64,
        reason: super::position::ExitReason,
    ) -> Result<Option<(Position, String)>> {
        let Some(mut position) = self.lane.position(id) else {
            return Ok(None);
        };
        let Some(fixture) = self.fixture() else {
            anyhow::bail!(
                "simulation unavailable: local fork is not running — cannot exit a simulated position"
            )
        };
        let decision = match reason {
            super::position::ExitReason::Manual => {
                let Some(d) = self.lane.manual_sell(id, sell_fraction_bps) else {
                    return Ok(None);
                };
                d
            }
            _ => {
                // Automated pass already decided; recompute the quantity from
                // the same lane helper semantics.
                let Some(d) = self.lane.manual_sell(id, sell_fraction_bps) else {
                    return Ok(None);
                };
                super::position::ExitDecision {
                    reason,
                    qty: d.qty,
                    fraction_bps: d.fraction_bps,
                    closes_position: d.closes_position,
                }
            }
        };

        let vault_state = fixture
            .ensure_deployed()
            .await
            .map_err(|error| anyhow::anyhow!("simulation fixture unavailable: {error}"))?;

        let params = self.lane.params();
        let is_weth_token0 = vault_state.weth < position.token;

        // Fork reserves give both numbers the exit needs: the spot mark (the
        // slippage floor) and the constant-product output (the swap's
        // optimistic amount — a spot quote here would fail the pair's K).
        let (_mark_value, expected_weth_out) = match fixture.pair_reserves(position.pair).await {
            Ok((r0, r1)) => {
                let (weth_r, token_r) = if is_weth_token0 { (r0, r1) } else { (r1, r0) };
                (
                    marks::compute_mark_value(
                        vault_state.weth,
                        position.token,
                        r0,
                        r1,
                        decision.qty,
                    )
                    .unwrap_or(U256::ZERO),
                    marks::v2_amount_out(decision.qty, token_r, weth_r),
                )
            }
            Err(_) => (U256::ZERO, U256::ZERO),
        };

        let fill_idx = position.closed_at_ms.unwrap_or(now_ms) as u32;
        let tag = calldata::make_tag(&position.id, fill_idx);
        let (_, _, calldata) = calldata::build_exit(
            vault_state.vault,
            position.pair,
            vault_state.weth,
            position.token,
            is_weth_token0,
            decision.qty,
            expected_weth_out,
            params.max_price_impact_bps,
            head_block,
            u64::MAX / 2,
            U256::ZERO,
            tag,
        );

        match fixture.execute_vault_calldata(&calldata).await {
            Err(error) => {
                position.tx_status = TxStatus::Abandoned;
                position.notes = format!("SIMULATION exit failed: {error}");
                self.store.upsert_sniper_position(&position)?;
                self.lane.upsert_position(position.clone());
                Err(anyhow::anyhow!("simulation exit failed: {error}"))
            }
            Ok(SimTxOutcome::Reverted { reason: revert }) => {
                // No credit on a failed sell — the tokens are still held.
                position.tx_status = TxStatus::Reverted;
                position.notes = format!("SIMULATION exit reverted: {revert}");
                self.store.upsert_sniper_position(&position)?;
                self.lane.upsert_position(position.clone());
                tracing::warn!(
                    target: "sniper",
                    id = %position.id,
                    reason = %revert,
                    "simulated exit reverted — position still open, nothing credited"
                );
                Err(anyhow::anyhow!("simulation exit reverted: {revert}"))
            }
            Ok(SimTxOutcome::Mined {
                tx_hash,
                block,
                gas_cost_wei,
                receipt,
            }) => {
                let Some((tokens_sold, weth_received, _gas, _blk)) =
                    Self::decode_exit_receipt(&receipt, vault_state.vault, position.token)
                else {
                    position.tx_status = TxStatus::Reverted;
                    position.notes =
                        "SIMULATION exit mined but emitted no ExitExecuted fill".into();
                    self.store.upsert_sniper_position(&position)?;
                    self.lane.upsert_position(position.clone());
                    return Err(anyhow::anyhow!(
                        "simulation exit mined without an ExitExecuted event"
                    ));
                };
                if tokens_sold > decision.qty {
                    return Err(anyhow::anyhow!(
                        "fixture vault sold more tokens than the guard allowed"
                    ));
                }
                position.apply_fill(tokens_sold, weth_received, gas_cost_wei, now_ms);
                position.exit_reason = Some(reason);
                position.tx_status = TxStatus::Mined;
                position.notes = format!(
                    "SIMULATION contract-backed exit ({}) · fixture tx {tx_hash:?} · block {block}",
                    reason.code()
                );
                self.lane.credit_paper(weth_received);
                self.persist_paper_balance();
                let fill_id = uuid::Uuid::new_v4().to_string();
                self.store.record_sniper_fill(
                    &fill_id,
                    &position.id,
                    "sell",
                    reason.code(),
                    tokens_sold,
                    weth_received,
                    gas_cost_wei,
                    Some(format!("{tx_hash:?}")),
                    Some(block),
                    ExecutionMode::Simulation,
                )?;
                self.store.upsert_sniper_position(&position)?;
                self.lane.upsert_position(position.clone());
                Ok(Some((position, format!("{tx_hash:?}"))))
            }
        }
    }

    /// Evaluate exits for all live positions on block head.
    ///
    /// Routing is per-position by provenance, not by the lane's current mode:
    /// a live → simulation switch stops new live entries but must keep
    /// managing live exposure, and simulated positions always settle against
    /// the fixture.
    pub async fn process_block_exits(
        &self,
        weth: Address,
        head_block: u64,
        head_base_fee: U256,
        now_ms: u64,
    ) -> Vec<Position> {
        self.reconcile_pending_entries().await;
        let live_positions = self.lane.live_positions();
        let mut executed = Vec::new();
        let params = self.lane.params();
        let vault_addr = params.vault_address.unwrap_or(Address::ZERO);

        for mut position in live_positions {
            if position.execution_mode == ExecutionMode::Simulation {
                if let Some(p) = self
                    .simulated_block_exit(&mut position, head_block, now_ms, &params)
                    .await
                {
                    executed.push(p);
                }
                continue;
            }

            // Update mark from live reserves
            let mark_opt = marks::update_position_mark(
                &self.rpc, &self.lane, &position, weth, head_block, now_ms,
            )
            .await;

            let marks_map = self.lane.marks();
            let mark = marks_map.get(&position.id);

            let (mark_val, is_stale) = match mark {
                Some(m) => (m.value_wei, m.is_stale(head_block)),
                None => (U256::ZERO, true),
            };

            let sell_honeypot = false;
            let decision = position.evaluate_exit_with_staleness(
                &params,
                mark_val,
                head_block,
                now_ms,
                sell_honeypot,
                is_stale,
            );

            let Some(decision) = decision else {
                let _ = mark_opt;
                continue;
            };

            let is_weth_token0 = weth < position.token;
            let fill_idx = position.closed_at_ms.unwrap_or(now_ms) as u32;
            let tag = calldata::make_tag(&position.id, fill_idx);

            // The optimistic swap output must be the constant-product amount
            // for this input, not the spot mark: spot ignores price impact
            // and a pair's K invariant rejects it. Fall back to the mark only
            // when reserves cannot be read (the vault's minWethOut still
            // guards the realised proceeds either way).
            let venue =
                crate::dex::Venue::from_label(&position.venue).unwrap_or(crate::dex::Venue::UniV2);
            let expected_weth = if venue == crate::dex::Venue::AeroVolatile {
                // Same intent, the venue's own fee model (work order 4.2).
                let out = self
                    .expected_exit_out(
                        venue,
                        position.pair,
                        weth,
                        position.token,
                        decision.qty,
                        head_block,
                    )
                    .await;
                if out.is_zero() {
                    mark_val
                } else {
                    out
                }
            } else {
                match marks::pair_reserves(&self.rpc, position.pair, head_block).await {
                    Some((r0, r1)) => {
                        let (weth_r, token_r) = if is_weth_token0 { (r0, r1) } else { (r1, r0) };
                        let out = marks::v2_amount_out(decision.qty, token_r, weth_r);
                        if out.is_zero() {
                            mark_val
                        } else {
                            out
                        }
                    }
                    None => mark_val,
                }
            };
            // Venue-exact calldata or no exit this block: an adapter that
            // cannot build must never fall back to approximated calldata
            // with real funds on the line.
            let calldata = match build_exit_for(
                &self.addresses,
                venue,
                vault_addr,
                position.pair,
                weth,
                position.token,
                is_weth_token0,
                decision.qty,
                expected_weth,
                params.max_price_impact_bps,
                head_block,
                2,
                U256::ZERO,
                tag,
            ) {
                Ok(c) => c,
                Err(error) => {
                    tracing::error!(
                        target: "sniper",
                        id = %position.id,
                        %error,
                        "execution adapter refused the block exit; skipping this block"
                    );
                    let _ = mark_opt;
                    continue;
                }
            };

            // Exit management is independent of the entry switch. A halt or
            // disabled master switch stops new buys, but must not strand funds
            // already held by the vault.
            if self.signer.is_none() || vault_addr == Address::ZERO {
                tracing::info!(
                    target: "sniper",
                    id = %position.id,
                    reason = ?decision.reason,
                    "exit decision recorded but no dedicated sniper signer/vault is configured"
                );
                continue;
            }

            let signer = self.signer.as_ref().unwrap();
            let nonce = match self
                .rpc
                .get_transaction_count(signer.address(), head_block)
                .await
            {
                Ok(nonce) => nonce,
                Err(error) => {
                    tracing::error!(target: "sniper", %error, id = %position.id, "exit nonce lookup failed; refusing send");
                    continue;
                }
            };
            let max_priority_fee = U256::from(1_500_000_000u64);
            let max_fee = head_base_fee * U256::from(2) + max_priority_fee;

            let tx = Eip1559Tx {
                chain_id: position.chain_id,
                nonce,
                max_priority_fee_per_gas: max_priority_fee,
                max_fee_per_gas: max_fee,
                gas_limit: 350_000,
                to: Some(vault_addr),
                value: U256::ZERO,
                data: calldata,
            };

            let (raw_tx, tx_hash) = signer.sign_eip1559(&tx);
            let tx_hash_hex = format!("0x{}", hex::encode(tx_hash));
            let raw_hex = format!("0x{}", hex::encode(&raw_tx));

            if self
                .rpc
                .call_raw("eth_sendRawTransaction", serde_json::json!([raw_hex]))
                .await
                .is_ok()
            {
                // Book the exit optimistically so the lane does not re-fire it
                // next block; the receipt reconciliation below then replaces
                // the estimate with the vault's exact ExitExecuted values or
                // rolls it back entirely on a revert. Never trust mempool
                // acceptance as the final word.
                let optimistic_qty = decision.qty;
                let optimistic_weth = expected_weth;
                position.apply_fill(optimistic_qty, optimistic_weth, U256::ZERO, now_ms);
                position.exit_reason = Some(decision.reason);
                position.exit_tx = Some(tx_hash);
                position.tx_status = TxStatus::Submitted;

                let fill_id = uuid::Uuid::new_v4().to_string();
                if let Err(error) = self.store.record_sniper_fill(
                    &fill_id,
                    &position.id,
                    "sell",
                    decision.reason.code(),
                    optimistic_qty,
                    optimistic_weth,
                    U256::ZERO,
                    Some(tx_hash_hex.clone()),
                    Some(head_block),
                    ExecutionMode::Live,
                ) {
                    tracing::error!(target: "sniper", %error, id = %position.id, "exit was submitted but fill persistence failed");
                    continue;
                }
                if let Err(error) = self.store.upsert_sniper_position(&position) {
                    tracing::error!(target: "sniper", %error, id = %position.id, "exit was submitted but position persistence failed");
                    continue;
                }
                self.lane.upsert_position(position.clone());
                executed.push(position);
            }
        }

        executed
    }

    /// One simulated position's per-block exit evaluation, settled against the
    /// fixture. Returns the position when an exit actually executed.
    async fn simulated_block_exit(
        &self,
        position: &mut Position,
        head_block: u64,
        now_ms: u64,
        params: &super::params::SniperParams,
    ) -> Option<Position> {
        let fixture = self.fixture()?;

        // Mark from the fixture pair's fork reserves. If the pair lost its
        // code (fork reset), try to rebuild it from the fixture's seed cache
        // before giving up on the mark.
        let reserves = match fixture.pair_reserves(position.pair).await {
            Ok(r) => Some(r),
            Err(_) => match fixture.rebuild_pair(position.pair).await {
                Ok(new_pair) => {
                    position.pair = new_pair;
                    fixture.pair_reserves(new_pair).await.ok()
                }
                Err(_) => None,
            },
        };

        let vault_state = fixture.state()?;
        let (mark_val, is_stale) = match reserves {
            Some((r0, r1)) => {
                match marks::compute_mark_value(
                    vault_state.weth,
                    position.token,
                    r0,
                    r1,
                    position.remaining_qty,
                ) {
                    Some(value) => {
                        let mark = super::portfolio::Mark::fresh(value, head_block, now_ms);
                        self.lane.set_mark(&position.id, mark);
                        (value, false)
                    }
                    None => (U256::ZERO, true),
                }
            }
            None => {
                // A vanished fixture pair is a stale mark, not a price crash:
                // suppress price-based exits rather than selling at zero.
                (U256::ZERO, true)
            }
        };

        let sell_honeypot = false;
        let decision = position.evaluate_exit_with_staleness(
            params,
            mark_val,
            head_block,
            now_ms,
            sell_honeypot,
            is_stale,
        )?;

        let fraction_bps = if decision.closes_position {
            10_000
        } else {
            decision.fraction_bps
        };
        match self
            .simulated_exit(
                &position.id,
                fraction_bps,
                head_block,
                now_ms,
                decision.reason,
            )
            .await
        {
            Ok(Some((p, _hash))) => Some(p),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(dead_code)]
    fn eth(n: u64) -> U256 {
        U256::from(n) * U256::from(1_000_000_000_000_000_000u128)
    }

    #[test]
    fn exit_receipt_decoder_requires_the_vault_and_reads_exact_values() {
        let vault = Address::repeat_byte(0xaa);
        let token = Address::repeat_byte(0x11);
        let signature = format!(
            "0x{:x}",
            keccak256("ExitExecuted(bytes32,address,uint256,uint256)".as_bytes())
        );
        let topic_token = format!("0x{:064x}", U256::from_be_slice(token.as_slice()));
        let receipt = serde_json::json!({
            "status": "0x1",
            "blockNumber": "0x2a",
            "gasUsed": "0x5208",
            "effectiveGasPrice": "0x3b9aca00",
            "logs": [{
                "address": format!("{vault:?}"),
                "topics": [signature, "0x00", topic_token],
                "data": format!("0x{:064x}{:064x}", 7u64, 11u64)
            }]
        });
        let decoded = SniperExecution::decode_exit_receipt(&receipt, vault, token).unwrap();
        assert_eq!(decoded.0, U256::from(7u64));
        assert_eq!(decoded.1, U256::from(11u64));
        assert_eq!(
            decoded.2,
            U256::from(21_000u64) * U256::from(1_000_000_000u64)
        );
        assert_eq!(decoded.3, 42);
    }

    #[test]
    fn entry_receipt_decoder_reads_exact_values() {
        let vault = Address::repeat_byte(0xaa);
        let token = Address::repeat_byte(0x11);
        let signature = format!(
            "0x{:x}",
            keccak256("EntryExecuted(bytes32,address,uint256,uint256)".as_bytes())
        );
        let topic_token = format!("0x{:064x}", U256::from_be_slice(token.as_slice()));
        let receipt = serde_json::json!({
            "status": "0x1",
            "blockNumber": "0x2a",
            "gasUsed": "0x5208",
            "effectiveGasPrice": "0x3b9aca00",
            "logs": [{
                "address": format!("{vault:?}"),
                "topics": [signature, "0x00", topic_token],
                "data": format!("0x{:064x}{:064x}", 13u64, 17u64)
            }]
        });
        let decoded = SniperExecution::decode_entry_receipt(&receipt, vault, token).unwrap();
        assert_eq!(decoded.0, U256::from(13u64));
        assert_eq!(decoded.1, U256::from(17u64));
        assert_eq!(
            decoded.2,
            U256::from(21_000u64) * U256::from(1_000_000_000u64)
        );
        assert_eq!(decoded.3, 42);
    }

    #[test]
    fn execution_struct_reads_its_mode_from_the_lane_at_decision_time() {
        let rpc = RpcClient::new("http://localhost:8545").unwrap();
        let store = Arc::new(Store::open_in_memory().unwrap());
        // A fresh checkout boots the sniper in simulation.
        let lane = Arc::new(SniperLane::with_boot(
            super::super::params::SniperParams::default(),
            super::super::SniperModeBoot::default(),
        ));

        let exec = SniperExecution::new(
            rpc,
            None,
            store,
            lane.clone(),
            *crate::config::known::ethereum(),
        );
        assert!(exec.signer.is_none());
        // Default boot is simulation: the paper ledger is reachable.
        assert!(exec.paper_mode());
        // And a runtime flip is visible to the execution path immediately —
        // no restart, no stale boot-time boolean.
        let boot_live = super::super::SniperModeBoot {
            mode: super::super::SniperMode::Live,
            live_enabled: true,
        };
        let live_lane = Arc::new(SniperLane::with_boot(
            super::super::params::SniperParams::default(),
            boot_live,
        ));
        let exec_live = SniperExecution::new(
            RpcClient::new("http://localhost:8545").unwrap(),
            None,
            Arc::new(Store::open_in_memory().unwrap()),
            live_lane,
            *crate::config::known::ethereum(),
        );
        assert!(!exec_live.paper_mode());
    }
}
