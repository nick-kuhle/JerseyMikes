//! Bundle construction: turning an [`Opportunity`] into calldata, into signed
//! transactions, and into the JSON payloads Flashbots understands.

use alloy_primitives::{keccak256, Address, Bytes, B256, U256};
use alloy_sol_types::{sol, SolCall};
use serde_json::{json, Value};

use crate::config::RiskConfig;
use crate::types::{now_ms, BundleRecord, BundleTx, Call, Opportunity};

sol! {
    interface IMevExecutor {
        struct Call {
            address target;
            uint256 value;
            bytes data;
        }

        struct Guard {
            address profitToken;
            uint256 minProfit;
            uint16 bribeBps;
            uint64 blockDeadline;
            uint256 maxBaseFee;
        }

        function execute(bytes32 tag, Call[] calldata calls, Guard calldata g) external payable returns (uint256);

        function flashExecute(
            bytes32 tag,
            address[] calldata tokens,
            uint256[] calldata amounts,
            Call[] calldata calls,
            Guard calldata g
        ) external;

        function quote(Call[] calldata calls, address profitToken) external payable returns (int256, uint256);
    }
}

fn to_sol_calls(calls: &[Call]) -> Vec<IMevExecutor::Call> {
    calls
        .iter()
        .map(|c| IMevExecutor::Call {
            target: c.target,
            value: c.value,
            data: Bytes::copy_from_slice(&c.data),
        })
        .collect()
}

fn tag_of(opp: &Opportunity, front: bool) -> B256 {
    let mut seed = opp.id.clone().into_bytes();
    seed.push(if front { 0 } else { 1 });
    keccak256(seed)
}

/// Encode a call to `MevExecutor.execute` (or `flashExecute` when the leg needs
/// borrowed capital).
///
/// The front leg of a sandwich is *expected* to end the transaction poorer in
/// the profit token — the profit only materialises on the back leg — so the
/// front leg's `minProfit` is zero while the back leg carries the real
/// requirement. For single-shot strategies (arb, liquidation) there is only a
/// front leg and it carries the requirement.
pub fn encode_execute(opp: &Opportunity, calls: &[Call], front: bool, risk: &RiskConfig) -> Vec<u8> {
    let single_leg = opp.back_calls.is_empty();
    let enforce = !front || single_leg;
    let min_profit = if enforce {
        risk.min_net_profit_wei
    } else {
        U256::ZERO
    };

    let guard = IMevExecutor::Guard {
        profitToken: opp.profit_token,
        minProfit: min_profit,
        // Only the profitable leg pays the builder.
        bribeBps: if enforce { risk.bribe_bps } else { 0 },
        blockDeadline: opp.target_block,
        maxBaseFee: risk.max_base_fee_wei,
    };

    if front && !opp.flash_tokens.is_empty() {
        IMevExecutor::flashExecuteCall {
            tag: tag_of(opp, front),
            tokens: opp.flash_tokens.clone(),
            amounts: opp.flash_amounts.clone(),
            calls: to_sol_calls(calls),
            g: guard,
        }
        .abi_encode()
    } else {
        IMevExecutor::executeCall {
            tag: tag_of(opp, front),
            calls: to_sol_calls(calls),
            g: guard,
        }
        .abi_encode()
    }
}

/// Read-only `quote` calldata, for `eth_call` sizing without any state changes.
pub fn encode_quote(calls: &[Call], profit_token: Address) -> Vec<u8> {
    IMevExecutor::quoteCall {
        calls: to_sol_calls(calls),
        profitToken: profit_token,
    }
    .abi_encode()
}

/// Everything needed to sign the bot's own legs of a bundle.
#[derive(Clone, Debug)]
pub struct BundleContext {
    pub chain_id: u64,
    pub executor: Address,
    pub nonce: u64,
    pub base_fee: U256,
    pub priority_fee: U256,
    pub gas_limit: u64,
}

/// Build the *unbroadcast* bundle: our signed legs plus the victim's raw
/// transactions in the correct order.
#[allow(clippy::too_many_arguments)]
pub fn build_bundle(
    opp: &Opportunity,
    victims_raw: &[Vec<u8>],
    ctx: &BundleContext,
    risk: &RiskConfig,
    signer: &crate::signer::Signer,
) -> BundleRecord {
    let mut txs: Vec<BundleTx> = Vec::new();
    let mut nonce = ctx.nonce;

    let sign_leg = |calls: &[Call], front: bool, nonce: u64| -> BundleTx {
        let tx = crate::signer::Eip1559Tx {
            chain_id: ctx.chain_id,
            nonce,
            max_priority_fee_per_gas: ctx.priority_fee,
            max_fee_per_gas: ctx.base_fee * U256::from(2u8) + ctx.priority_fee,
            gas_limit: ctx.gas_limit,
            to: Some(ctx.executor),
            value: U256::ZERO,
            data: encode_execute(opp, calls, front, risk),
        };
        let (raw, hash) = signer.sign_eip1559(&tx);
        BundleTx {
            hash: Some(hash),
            raw,
            can_revert: false,
            foreign: false,
        }
    };

    if !opp.front_calls.is_empty() {
        txs.push(sign_leg(&opp.front_calls, true, nonce));
        nonce += 1;
    }
    for raw in victims_raw {
        txs.push(BundleTx {
            hash: None,
            raw: raw.clone(),
            can_revert: true,
            foreign: true,
        });
    }
    if !opp.back_calls.is_empty() {
        txs.push(sign_leg(&opp.back_calls, false, nonce));
    }
    let _ = nonce;

    BundleRecord {
        id: uuid::Uuid::new_v4().to_string(),
        opportunity_id: opp.id.clone(),
        strategy: opp.strategy,
        target_block: opp.target_block,
        txs,
        submitted: false,
        included: None,
        created_at_ms: now_ms(),
    }
}

/// `eth_callBundle` parameters (Flashbots relay / builder RPCs).
pub fn call_bundle_params(bundle: &BundleRecord, state_block: &str) -> Value {
    json!([{
        "txs": bundle
            .txs
            .iter()
            .map(|t| format!("0x{}", hex::encode(&t.raw)))
            .collect::<Vec<_>>(),
        "blockNumber": format!("0x{:x}", bundle.target_block),
        "stateBlockNumber": state_block,
    }])
}

/// `eth_sendBundle` parameters. Built for completeness and for shape-parity in
/// the simulator; the engine refuses to invoke it unless live execution is on.
pub fn send_bundle_params(bundle: &BundleRecord) -> Value {
    json!([{
        "txs": bundle
            .txs
            .iter()
            .map(|t| format!("0x{}", hex::encode(&t.raw)))
            .collect::<Vec<_>>(),
        "blockNumber": format!("0x{:x}", bundle.target_block),
        "revertingTxHashes": bundle
            .txs
            .iter()
            .filter(|t| t.can_revert)
            .filter_map(|t| t.hash.map(|h| format!("{h:?}")))
            .collect::<Vec<_>>(),
    }])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Strategy;

    fn opp() -> Opportunity {
        Opportunity {
            id: "test".into(),
            strategy: Strategy::AtomicArb,
            victim_hashes: vec![],
            front_calls: vec![Call::new(Address::with_last_byte(7), vec![1, 2, 3])],
            back_calls: vec![],
            flash_tokens: vec![],
            flash_amounts: vec![],
            profit_token: Address::ZERO,
            expected_profit_wei: U256::from(1u8),
            notional_wei: U256::ZERO,
            target_block: 100,
            created_at_ms: 0,
            notes: String::new(),
        }
    }

    fn risk() -> RiskConfig {
        RiskConfig {
            min_net_profit_wei: U256::from(42u8),
            max_position_wei: U256::from(1u8),
            max_base_fee_wei: U256::from(1u8),
            bribe_bps: 9000,
            max_gas_per_bundle: 1_000_000,
            max_drawdown_wei: U256::ZERO,
            max_inflight_per_strategy: 1,
            max_revert_rate: 1.0,
        }
    }

    #[test]
    fn execute_selector_is_stable() {
        let data = encode_execute(&opp(), &opp().front_calls, true, &risk());
        assert_eq!(&data[..4], &IMevExecutor::executeCall::SELECTOR);
    }

    #[test]
    fn single_leg_enforces_min_profit() {
        // A single-leg opportunity must carry the profit requirement on the front leg,
        // otherwise an unprofitable bundle could land.
        let data = encode_execute(&opp(), &opp().front_calls, true, &risk());
        let decoded = IMevExecutor::executeCall::abi_decode(&data, true).unwrap();
        assert_eq!(decoded.g.minProfit, U256::from(42u8));
        assert_eq!(decoded.g.bribeBps, 9000);
    }

    #[test]
    fn sandwich_front_leg_does_not_require_profit() {
        let mut o = opp();
        o.strategy = Strategy::Sandwich;
        o.back_calls = vec![Call::new(Address::with_last_byte(8), vec![4])];
        let front = encode_execute(&o, &o.front_calls, true, &risk());
        let back = encode_execute(&o, &o.back_calls, false, &risk());
        let fd = IMevExecutor::executeCall::abi_decode(&front, true).unwrap();
        let bd = IMevExecutor::executeCall::abi_decode(&back, true).unwrap();
        assert_eq!(fd.g.minProfit, U256::ZERO);
        assert_eq!(bd.g.minProfit, U256::from(42u8));
    }

    #[test]
    fn flash_leg_uses_flash_execute() {
        let mut o = opp();
        o.flash_tokens = vec![Address::with_last_byte(3)];
        o.flash_amounts = vec![U256::from(5u8)];
        let data = encode_execute(&o, &o.front_calls, true, &risk());
        assert_eq!(&data[..4], &IMevExecutor::flashExecuteCall::SELECTOR);
    }
}
