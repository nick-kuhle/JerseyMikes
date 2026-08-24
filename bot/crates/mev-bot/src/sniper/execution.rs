//! Execution pipeline for the new-token sniper lane.
//!
//! Handles entry submission (back-running launches to SniperVault) and
//! exit execution (closing positions on triggers).
//!
//! Global Invariant: Keeps its own submission path and NEVER imports or calls
//! bundle.rs, submission.rs, or qualification.rs.

use std::sync::Arc;

use alloy_primitives::{keccak256, Address, U256};
use anyhow::Result;

use super::calldata;
use super::gates::LaunchCandidate;
use super::marks;
use super::position::{Position, PositionState};
use super::SniperLane;
use crate::rpc::RpcClient;
use crate::signer::{Eip1559Tx, Signer};
use crate::store::Store;

#[derive(Clone)]
pub struct SniperExecution {
    pub rpc: RpcClient,
    pub signer: Option<Signer>,
    pub store: Arc<Store>,
    pub lane: Arc<SniperLane>,
    /// True when the process was not boot-armed for live execution. This is
    /// the only branch allowed to touch the virtual 1 ETH paper ledger.
    pub paper_mode: bool,
}

impl SniperExecution {
    pub fn new(
        rpc: RpcClient,
        signer: Option<Signer>,
        store: Arc<Store>,
        lane: Arc<SniperLane>,
        paper_mode: bool,
    ) -> Self {
        Self {
            rpc,
            signer,
            store,
            lane,
            paper_mode,
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

        // Admit candidate through the normal gates. Paper mode uses the same
        // gates but substitutes an internal non-zero vault marker, because no
        // on-chain deployment is needed for a virtual trade.
        let params = self.lane.params();
        let admission = if self.paper_mode
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

        // Calculate expected output from reserves
        let is_weth_token0 = weth < candidate.token;
        let (weth_reserve, token_reserve) = if is_weth_token0 {
            (candidate.weth_reserve, candidate.token_reserve)
        } else {
            (candidate.token_reserve, candidate.weth_reserve)
        };

        if token_reserve.is_zero() || weth_reserve.is_zero() {
            return Ok(None);
        }

        let expected_tokens_out = (size_wei * token_reserve * U256::from(997))
            / (weth_reserve * U256::from(1000) + size_wei * U256::from(997));

        if expected_tokens_out.is_zero() {
            return Ok(None);
        }

        let pos_id = uuid::Uuid::new_v4().to_string();
        let tag = calldata::make_tag(&pos_id, 0);

        let vault_addr = params.vault_address.unwrap_or(Address::ZERO);

        let (_, _guard, calldata) = calldata::build_entry(
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
        );

        let mut position = Position {
            id: pos_id.clone(),
            chain_id,
            token: candidate.token,
            pair: candidate.pair,
            venue: "univ2".into(),
            state: PositionState::Pending,
            trigger_tx: None,
            entry_tx: None,
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
        };

        // INVARIANT 4: Position rows are written BEFORE entry submission, never after.
        // A persistence failure is a hard refusal to sign or broadcast.
        self.store
            .upsert_sniper_position(&position)
            .map_err(|error| anyhow::anyhow!("persisting entry intent: {error}"))?;
        self.lane.upsert_position(position.clone());

        let paper_ready = self.paper_mode
            && params.enabled
            && !params.buy_size_wei.is_zero()
            && !params.daily_budget_wei.is_zero();
        if paper_ready {
            if !self.lane.reserve_paper(size_wei) {
                position.state = PositionState::Abandoned;
                position.notes = "simulation paper balance exhausted".into();
                self.store.upsert_sniper_position(&position)?;
                self.lane.upsert_position(position.clone());
                return Ok(Some(position));
            }
            position.state = PositionState::Open;
            position.entry_qty = expected_tokens_out;
            position.remaining_qty = expected_tokens_out;
            position.notes = "SIMULATION paper entry".into();
            let fill_id = uuid::Uuid::new_v4().to_string();
            self.store.record_sniper_fill(
                &fill_id,
                &pos_id,
                "buy",
                "simulation",
                expected_tokens_out,
                size_wei,
                U256::ZERO,
                None,
                Some(head_block),
            )?;
            self.store.upsert_sniper_position(&position)?;
            self.lane.upsert_position(position.clone());
            return Ok(Some(position));
        }

        // Shadow mode check: SNIPER_DIRECTIONAL=false runs detection -> probe -> gate, stops before signing.
        let armed = params.is_armed() && !self.lane.is_halted() && self.lane.boot_enabled();
        if !armed || self.signer.is_none() || vault_addr == Address::ZERO {
            tracing::info!(
                target: "sniper",
                id = %pos_id,
                token = ?candidate.token,
                size_wei = %size_wei,
                "shadow mode / disarmed admission — entry candidate logged without signing tx"
            );
            position.state = PositionState::Abandoned;
            position.notes = "shadow mode pass (unsubmitted)".into();
            self.store
                .upsert_sniper_position(&position)
                .map_err(|error| anyhow::anyhow!("persisting shadow position: {error}"))?;
            self.lane.upsert_position(position.clone());
            return Ok(Some(position));
        }

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
                position.notes = format!("entry submitted {tx_hash_hex}; awaiting receipt");
                self.store.upsert_sniper_position(&position)?;
                self.lane.upsert_position(position.clone());
                tracing::info!(target: "sniper", id = %pos_id, token = ?candidate.token, "entry submitted; awaiting receipt");
                Ok(Some(position))
            }
            Err(e) => {
                tracing::warn!(target: "sniper", id = %pos_id, error = %e, "entry submission failed");
                position.state = PositionState::Abandoned;
                position.notes = format!("submission error: {e}");
                self.store.upsert_sniper_position(&position)?;
                self.lane.upsert_position(position.clone());
                Ok(Some(position))
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
    fn decode_exit_receipt(
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
    fn decode_entry_receipt(
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
    /// quote that never landed.
    pub async fn reconcile_pending_entries(&self) {
        let params = self.lane.params();
        let vault = params.vault_address.unwrap_or(Address::ZERO);
        if vault.is_zero() {
            return;
        }
        for mut position in self.lane.positions().into_iter().filter(|position| {
            position.state == PositionState::Pending && position.entry_tx.is_some()
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
                position.entry_tx.map(|hash| format!("{:?}", hash)),
                Some(block),
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
        if !self.paper_mode && self.signer.is_none() {
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
        let Some(mut position) = self.lane.position(id) else {
            return Ok(None);
        };
        if !position.state.is_live() || position.remaining_qty.is_zero() {
            return Ok(None);
        }
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
        let (_, _, calldata) = calldata::build_exit(
            vault_addr,
            position.pair,
            weth,
            position.token,
            is_weth_token0,
            decision.qty,
            mark.value_wei,
            params.max_price_impact_bps,
            head_block,
            2,
            U256::ZERO,
            calldata::make_tag(&position.id, now_ms as u32),
        );

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
            position.notes = format!("manual exit submitted {tx_hash_hex}; awaiting receipt");
            self.store.upsert_sniper_position(&position)?;
            self.lane.upsert_position(position.clone());
            return Ok(Some((position, tx_hash_hex)));
        };
        let Some((tokens_sold, weth_received, gas_cost, filled_block)) =
            Self::decode_exit_receipt(&receipt, vault_addr, position.token)
        else {
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
        )?;
        self.store.upsert_sniper_position(&position)?;
        self.lane.upsert_position(position.clone());
        Ok(Some((position, tx_hash_hex)))
    }

    /// Evaluate exits for all live positions on block head.
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

            if self.paper_mode {
                // Simulation exits settle against the same reserve-derived mark
                // shown in the portfolio. No signer, vault or native balance is
                // touched; the virtual bankroll is credited with the simulated
                // WETH proceeds and the exact paper fill is persisted.
                position.apply_fill(decision.qty, mark_val, U256::ZERO, now_ms);
                position.exit_reason = Some(decision.reason);
                position.notes = format!("SIMULATION paper exit: {}", decision.reason.code());
                self.lane.credit_paper(mark_val);
                let fill_id = uuid::Uuid::new_v4().to_string();
                if let Err(error) = self.store.record_sniper_fill(
                    &fill_id,
                    &position.id,
                    "sell",
                    decision.reason.code(),
                    decision.qty,
                    mark_val,
                    U256::ZERO,
                    None,
                    Some(head_block),
                ) {
                    tracing::error!(target: "sniper", %error, id = %position.id, "paper exit fill persistence failed");
                    continue;
                }
                if let Err(error) = self.store.upsert_sniper_position(&position) {
                    tracing::error!(target: "sniper", %error, id = %position.id, "paper exit position persistence failed");
                    continue;
                }
                self.lane.upsert_position(position.clone());
                executed.push(position);
                continue;
            }

            let is_weth_token0 = weth < position.token;
            let fill_idx = position.closed_at_ms.unwrap_or(now_ms) as u32;
            let tag = calldata::make_tag(&position.id, fill_idx);

            let expected_weth = mark_val;
            let (_, _, calldata) = calldata::build_exit(
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
            );

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
                let weth_received = expected_weth;
                position.apply_fill(decision.qty, weth_received, U256::ZERO, now_ms);
                position.exit_reason = Some(decision.reason);

                let fill_id = uuid::Uuid::new_v4().to_string();
                if let Err(error) = self.store.record_sniper_fill(
                    &fill_id,
                    &position.id,
                    "sell",
                    decision.reason.code(),
                    decision.qty,
                    weth_received,
                    U256::ZERO,
                    Some(tx_hash_hex.clone()),
                    Some(head_block),
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
    fn execution_struct_can_be_instantiated() {
        let rpc = RpcClient::new("http://localhost:8545").unwrap();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let lane = Arc::new(SniperLane::new(
            super::super::params::SniperParams::default(),
        ));

        let exec = SniperExecution::new(rpc, None, store, lane, true);
        assert!(exec.signer.is_none());
        assert!(exec.paper_mode);
    }
}
