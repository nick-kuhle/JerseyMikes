//! Execution pipeline for the new-token sniper lane.
//!
//! Handles entry submission (back-running launches to SniperVault) and
//! exit execution (closing positions on triggers).
//!
//! Global Invariant: Keeps its own submission path and NEVER imports or calls
//! bundle.rs, submission.rs, or qualification.rs.

use std::sync::Arc;

use alloy_primitives::{Address, U256};
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
}

impl SniperExecution {
    pub fn new(
        rpc: RpcClient,
        signer: Option<Signer>,
        store: Arc<Store>,
        lane: Arc<SniperLane>,
    ) -> Self {
        Self {
            rpc,
            signer,
            store,
            lane,
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
        let _ = self.store.record_sniper_verdict(
            &token_hex,
            chain_id,
            verdict_str,
            candidate.verdict.round_trip_bps(),
            "launch probe",
        );

        if matches!(candidate.verdict, super::gates::HoneypotVerdict::Honeypot) {
            self.lane.blacklist(candidate.token);
        }

        // Admit candidate through gates.
        let admission = match self.lane.admit(candidate, now_ms) {
            Ok(adm) => adm,
            Err(_) => return Ok(None),
        };

        let params = self.lane.params();
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
        let _ = self.store.upsert_sniper_position(&position);
        self.lane.upsert_position(position.clone());

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
            let _ = self.store.upsert_sniper_position(&position);
            self.lane.upsert_position(position.clone());
            return Ok(Some(position));
        }

        let signer = self.signer.as_ref().unwrap();

        // Query nonce for searcher key
        let nonce = self
            .rpc
            .get_transaction_count(signer.address(), head_block)
            .await
            .unwrap_or(0);
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
        let _ = self.store.upsert_sniper_position(&position);

        let raw_hex = format!("0x{}", hex::encode(&raw_tx));
        let send_res = self
            .rpc
            .call_raw("eth_sendRawTransaction", serde_json::json!([raw_hex]))
            .await;

        match send_res {
            Ok(_) => {
                // Entry confirmed: transition to Open and record fill
                position.state = PositionState::Open;
                position.entry_qty = expected_tokens_out;
                position.remaining_qty = expected_tokens_out;
                let fill_id = uuid::Uuid::new_v4().to_string();
                let _ = self.store.record_sniper_fill(
                    &fill_id,
                    &pos_id,
                    "buy",
                    "entry",
                    expected_tokens_out,
                    size_wei,
                    U256::ZERO,
                    Some(tx_hash_hex.clone()),
                    Some(head_block),
                );
                let _ = self.store.upsert_sniper_position(&position);
                self.lane.upsert_position(position.clone());
                tracing::info!(target: "sniper", id = %pos_id, token = ?candidate.token, "entry submitted and opened");
                Ok(Some(position))
            }
            Err(e) => {
                tracing::warn!(target: "sniper", id = %pos_id, error = %e, "entry submission failed");
                position.state = PositionState::Abandoned;
                position.notes = format!("submission error: {e}");
                let _ = self.store.upsert_sniper_position(&position);
                self.lane.upsert_position(position.clone());
                Ok(Some(position))
            }
        }
    }

    /// Evaluate exits for all live positions on block head.
    pub async fn process_block_exits(
        &self,
        weth: Address,
        head_block: u64,
        head_base_fee: U256,
        now_ms: u64,
    ) -> Vec<Position> {
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

            let armed = params.is_armed() && !self.lane.is_halted() && self.lane.boot_enabled();
            if !armed || self.signer.is_none() || vault_addr == Address::ZERO {
                tracing::info!(
                    target: "sniper",
                    id = %position.id,
                    reason = ?decision.reason,
                    "shadow mode exit decision — unsubmitted"
                );
                continue;
            }

            let signer = self.signer.as_ref().unwrap();
            let nonce = self
                .rpc
                .get_transaction_count(signer.address(), head_block)
                .await
                .unwrap_or(0);
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
                let _ = self.store.record_sniper_fill(
                    &fill_id,
                    &position.id,
                    "sell",
                    decision.reason.code(),
                    decision.qty,
                    weth_received,
                    U256::ZERO,
                    Some(tx_hash_hex.clone()),
                    Some(head_block),
                );
                let _ = self.store.upsert_sniper_position(&position);
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
    fn execution_struct_can_be_instantiated() {
        let rpc = RpcClient::new("http://localhost:8545").unwrap();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let lane = Arc::new(SniperLane::new(
            super::super::params::SniperParams::default(),
        ));

        let exec = SniperExecution::new(rpc, None, store, lane);
        assert!(exec.signer.is_none());
    }
}
