//! Evidence-backed matching of a decision-time opportunity to MEV that later
//! appeared in a canonical delivered block.
//!
//! This deliberately reports an *on-chain WETH delta less gas*, not an unknowable
//! competitor's total economic profit. Off-chain/CEX legs and inventory marks are
//! outside the chain. A high-confidence match requires the same actor to bracket
//! the victim and touch one of the same V2/V3 pools.

use std::collections::{HashMap, HashSet};

use alloy_primitives::{Address, B256, U256};
use serde_json::{json, Value};

use crate::rpc::RpcClient;
use crate::store::{ActualMevMatch, Store};
use crate::types::{parse_address, parse_b256, parse_u256, parse_u64, PendingTx};

const TRANSFER: &str = "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef";
const DEPOSIT: &str = "0xe1fffcc4923d04b559f4d29a8bfc6cda04eb5b0d3c460751c2402c5c5cc9109c";
const WITHDRAWAL: &str = "0x7fcf532c15f0a6db0bd6d0e038bea71d30d808c7d98cb3bf7268a95bf5081b65";
const V2_SWAP: &str = "0xd78ad95fa46c994b6551d0da85fc275fe613ce37657fb8d5e3d130840159d822";
const V3_SWAP: &str = "0xc42079f94a6350d7e6235f29174924f928cc2ac818eb64fed8004e115fbcca67";
const EXECUTED: &str = "0x920d3a9c5eb5759e8895809a65dae03c9336ebf6f554de8cdc90e3bcb4404121";
const WINDOW: usize = 5;

struct SelectedCandidate {
    indices: Vec<usize>,
    receipts: Vec<Value>,
    entity: HashSet<Address>,
    confidence: &'static str,
}

/// Exact reconciliation for our own submitted payloads. Unlike competitor
/// attribution, bundle boundaries and signer identity are known, so executor
/// events plus receipts produce exact on-chain retained WETH/ETH less gas.
pub async fn reconcile_own_submissions(
    store: &Store,
    rpc: &RpcClient,
    head: u64,
    finality_depth: u64,
    searcher: Address,
    executor: Address,
) {
    let Ok(bundles) = store.submitted_bundles_through(head) else {
        return;
    };
    for bundle in bundles {
        let mut found = Vec::new();
        for hash in &bundle.tx_hashes {
            let value = rpc
                .call_raw("eth_getTransactionReceipt", json!([format!("{hash:?}")]))
                .await
                .unwrap_or(Value::Null);
            if !value.is_null() {
                found.push((*hash, value));
            }
        }
        let found_hashes = found.iter().map(|(hash, _)| *hash).collect::<Vec<_>>();
        if found.len() != bundle.tx_hashes.len() {
            let state = if found.is_empty() {
                "pending"
            } else {
                "partial_unfinalized"
            };
            let _ = store.mark_bundle_observation(&bundle.bundle_id, state, &found_hashes);
            if head >= bundle.target_block.saturating_add(finality_depth) {
                if found.is_empty() {
                    let _ = store.mark_bundle_not_included(&bundle.bundle_id);
                } else {
                    let block = found
                        .first()
                        .map(|(_, receipt)| parse_u64(&receipt["blockNumber"]))
                        .filter(|value| *value != 0);
                    let _ = store.finalize_bundle_state(
                        &bundle.bundle_id,
                        "finalized_partial_inclusion",
                        false,
                        block,
                        &found_hashes,
                    );
                }
            }
            continue;
        }

        let receipts = found
            .iter()
            .map(|(_, receipt)| receipt.clone())
            .collect::<Vec<_>>();
        let included_block = parse_u64(&receipts[0]["blockNumber"]);
        let receipt_block_hash = receipts[0].get("blockHash").and_then(parse_b256);
        let coherent = included_block != 0
            && receipt_block_hash.is_some()
            && receipts.iter().all(|value| {
                parse_u64(&value["status"]) == 1
                    && parse_u64(&value["blockNumber"]) == included_block
                    && value.get("blockHash").and_then(parse_b256) == receipt_block_hash
            });
        if !coherent {
            if head >= bundle.target_block.saturating_add(finality_depth) {
                let _ = store.finalize_bundle_state(
                    &bundle.bundle_id,
                    "finalized_incoherent_inclusion",
                    false,
                    Some(included_block).filter(|value| *value != 0),
                    &found_hashes,
                );
            } else {
                let _ = store.mark_bundle_observation(
                    &bundle.bundle_id,
                    "incoherent_unfinalized",
                    &found_hashes,
                );
            }
            continue;
        }

        if head < included_block.saturating_add(finality_depth) {
            let _ = store.mark_bundle_observation(
                &bundle.bundle_id,
                "included_unfinalized",
                &found_hashes,
            );
            continue;
        }

        // Verify that the receipt block hash is still canonical at finality.
        let canonical = rpc
            .call_raw(
                "eth_getBlockByNumber",
                json!([format!("0x{included_block:x}"), false]),
            )
            .await
            .ok()
            .and_then(|block| block.get("hash").and_then(parse_b256))
            == receipt_block_hash;
        if !canonical {
            let _ = store.finalize_bundle_state(
                &bundle.bundle_id,
                "finalized_noncanonical",
                false,
                Some(included_block),
                &found_hashes,
            );
            continue;
        }

        let mut gross = U256::ZERO;
        let mut bribe = U256::ZERO;
        let mut retained = U256::ZERO;
        let mut gas_cost = U256::ZERO;
        let mut executed_events = 0u64;
        for value in &receipts {
            if value.get("from").and_then(parse_address) == Some(searcher) {
                gas_cost = gas_cost.saturating_add(
                    parse_u256(&value["effectiveGasPrice"])
                        .saturating_mul(U256::from(parse_u64(&value["gasUsed"]))),
                );
            }
            for log in value
                .get("logs")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if log.get("address").and_then(parse_address) != Some(executor)
                    || !first_topic(log)
                        .map(|topic| topic.eq_ignore_ascii_case(EXECUTED))
                        .unwrap_or(false)
                {
                    continue;
                }
                let data = log
                    .get("data")
                    .map(crate::types::parse_bytes)
                    .unwrap_or_default();
                if data.len() >= 96 {
                    executed_events = executed_events.saturating_add(1);
                    gross = gross.saturating_add(U256::from_be_slice(&data[0..32]));
                    bribe = bribe.saturating_add(U256::from_be_slice(&data[32..64]));
                    retained = retained.saturating_add(U256::from_be_slice(&data[64..96]));
                }
            }
        }
        if executed_events == 0 {
            let _ = store.finalize_bundle_state(
                &bundle.bundle_id,
                "finalized_missing_executor_evidence",
                true,
                Some(included_block),
                &found_hashes,
            );
            continue;
        }
        let net = crate::sim::anvil::to_i128(retained)
            .saturating_sub(crate::sim::anvil::to_i128(gas_cost));
        if let Err(error) = store.record_execution_outcome(
            &bundle,
            included_block,
            head,
            gross,
            bribe,
            retained,
            gas_cost,
            net,
        ) {
            tracing::debug!(target: "attribution", %error, bundle = %bundle.bundle_id, "persist own outcome failed");
        }
    }
}

pub async fn reconcile_block(
    store: &Store,
    rpc: &RpcClient,
    block_number: u64,
    txs: &[PendingTx],
    weth: Address,
) {
    let Ok(opportunities) = store.victim_opportunities_for_block(block_number) else {
        return;
    };
    if opportunities.is_empty() {
        return;
    }
    let by_hash: HashMap<String, usize> = txs
        .iter()
        .enumerate()
        .map(|(index, tx)| (format!("{:?}", tx.hash).to_ascii_lowercase(), index))
        .collect();
    let mut receipts: HashMap<B256, Value> = HashMap::new();

    for opportunity in opportunities {
        let Some(&victim_index) = by_hash.get(&opportunity.victim_hash.to_ascii_lowercase()) else {
            continue;
        };
        let Some(victim_receipt) = receipt(rpc, txs[victim_index].hash, &mut receipts).await else {
            continue;
        };
        let victim_pools = swap_pools(&victim_receipt);
        if victim_pools.is_empty() {
            continue;
        }

        let before_start = victim_index.saturating_sub(WINDOW);
        let after_end = (victim_index + WINDOW + 1).min(txs.len());
        let mut before = Vec::new();
        let mut after = Vec::new();
        for (index, candidate) in txs.iter().enumerate().take(victim_index).skip(before_start) {
            if let Some(candidate_receipt) = receipt(rpc, candidate.hash, &mut receipts).await {
                if !swap_pools(&candidate_receipt).is_disjoint(&victim_pools) {
                    before.push((index, candidate_receipt));
                }
            }
        }
        for (index, candidate) in txs
            .iter()
            .enumerate()
            .take(after_end)
            .skip(victim_index + 1)
        {
            if let Some(candidate_receipt) = receipt(rpc, candidate.hash, &mut receipts).await {
                if !swap_pools(&candidate_receipt).is_disjoint(&victim_pools) {
                    after.push((index, candidate_receipt));
                }
            }
        }

        let mut selected: Option<SelectedCandidate> = None;
        'pairs: for (front_index, front_receipt) in before.iter().rev() {
            for (back_index, back_receipt) in &after {
                let front = &txs[*front_index];
                let back = &txs[*back_index];
                if same_actor(front, back) {
                    let entity = entity_addresses(front, back);
                    selected = Some(SelectedCandidate {
                        indices: vec![*front_index, *back_index],
                        receipts: vec![front_receipt.clone(), back_receipt.clone()],
                        entity,
                        confidence: "high",
                    });
                    break 'pairs;
                }
            }
        }
        // Back-run-only match: one immediately following transaction touching
        // the same pool. This is evidence, but not enough to call a bundle
        // boundary exact, so it is explicitly medium confidence.
        if selected.is_none() {
            if let Some((index, candidate_receipt)) = after.first() {
                let tx = &txs[*index];
                selected = Some(SelectedCandidate {
                    indices: vec![*index],
                    receipts: vec![candidate_receipt.clone()],
                    entity: entity_addresses(tx, tx),
                    confidence: "medium",
                });
            }
        }
        let Some(selected) = selected else {
            continue;
        };
        let indices = selected.indices;
        let candidate_receipts = selected.receipts;
        let entity = selected.entity;
        let confidence = selected.confidence;

        let weth_delta = candidate_receipts.iter().fold(0i128, |sum, value| {
            sum.saturating_add(weth_delta(value, weth, &entity))
        });
        let (gas_used, gas_cost) =
            candidate_receipts
                .iter()
                .fold((0u64, U256::ZERO), |(used_sum, cost_sum), value| {
                    let used = parse_u64(&value["gasUsed"]);
                    let price = parse_u256(&value["effectiveGasPrice"]);
                    (
                        used_sum.saturating_add(used),
                        cost_sum.saturating_add(price.saturating_mul(U256::from(used))),
                    )
                });
        let net = weth_delta.saturating_sub(crate::sim::anvil::to_i128(gas_cost));
        // A non-positive WETH delta can be a liquidation in another token, but
        // it is not an attributable WETH profit and must not qualify a strategy.
        if net <= 0 {
            continue;
        }
        let actor = indices
            .first()
            .and_then(|index| txs[*index].to.or(txs[*index].from))
            .map(|address| format!("{address:?}"));
        let hashes = indices
            .iter()
            .map(|index| format!("{:?}", txs[*index].hash))
            .collect::<Vec<_>>();
        let matched = ActualMevMatch {
            opportunity_id: opportunity.opportunity_id,
            block_number,
            victim_hash: opportunity.victim_hash,
            mev_tx_hashes: hashes,
            actor,
            gross_weth_wei: U256::from(weth_delta as u128),
            gas_cost_wei: gas_cost,
            net_weth_wei: net,
            confidence: confidence.to_string(),
            confidence_score_bps: if confidence == "high" { 9_000 } else { 6_500 },
            completeness: json!({
                "transactionGas": "exact",
                "observableWethTransfers": "exact",
                "bundleBoundary": if confidence == "high" { "strongly_inferred" } else { "inferred" },
                "internalEthTransfers": "not_observed_without_traces",
                "offChainLegs": "not_observable",
                "inventoryMarkToMarket": "not_observable"
            }),
            evidence: json!({
                "kind": "on_chain_weth_delta_less_gas",
                "strategy": opportunity.strategy,
                "victimIndex": victim_index,
                "mevTxIndices": indices,
                "sharedPools": victim_pools.iter().map(|p| format!("{p:?}")).collect::<Vec<_>>(),
                "entity": entity.iter().map(|a| format!("{a:?}")).collect::<Vec<_>>(),
                "gasUsed": gas_used,
                "limitations": [
                    "bundle boundaries are inferred from ordering and swap logs",
                    "off-chain/CEX legs and inventory marks are not observable",
                    "internal ETH transfers require trace RPC and are not included"
                ]
            }),
        };
        if let Err(error) = store.record_actual_mev_match(&matched) {
            tracing::debug!(target: "attribution", %error, block_number, "persist match failed");
        }
    }
}

async fn receipt(rpc: &RpcClient, hash: B256, cache: &mut HashMap<B256, Value>) -> Option<Value> {
    if let Some(value) = cache.get(&hash) {
        return Some(value.clone());
    }
    let value = rpc
        .call_raw("eth_getTransactionReceipt", json!([format!("{hash:?}")]))
        .await
        .ok()?;
    if value.is_null() || parse_u64(&value["status"]) != 1 {
        return None;
    }
    cache.insert(hash, value.clone());
    Some(value)
}

fn first_topic(log: &Value) -> Option<&str> {
    log.get("topics")?.as_array()?.first()?.as_str()
}

fn swap_pools(receipt: &Value) -> HashSet<Address> {
    receipt
        .get("logs")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|log| {
            first_topic(log)
                .map(|topic| {
                    topic.eq_ignore_ascii_case(V2_SWAP) || topic.eq_ignore_ascii_case(V3_SWAP)
                })
                .unwrap_or(false)
        })
        .filter_map(|log| log.get("address").and_then(parse_address))
        .collect()
}

fn same_actor(a: &PendingTx, b: &PendingTx) -> bool {
    (a.from.is_some() && a.from == b.from) || (a.to.is_some() && a.to == b.to)
}

fn entity_addresses(a: &PendingTx, b: &PendingTx) -> HashSet<Address> {
    [a.from, a.to, b.from, b.to].into_iter().flatten().collect()
}

fn topic_address(value: Option<&Value>) -> Option<Address> {
    let hash = value.and_then(parse_b256)?;
    Some(Address::from_slice(&hash.as_slice()[12..]))
}

fn weth_delta(receipt: &Value, weth: Address, entity: &HashSet<Address>) -> i128 {
    let mut delta = 0i128;
    let Some(logs) = receipt.get("logs").and_then(Value::as_array) else {
        return 0;
    };
    for log in logs {
        if log.get("address").and_then(parse_address) != Some(weth) {
            continue;
        }
        let Some(topic) = first_topic(log) else {
            continue;
        };
        let topics = log.get("topics").and_then(Value::as_array);
        let amount = log.get("data").map(parse_u256).unwrap_or(U256::ZERO);
        let amount = crate::sim::anvil::to_i128(amount);
        if topic.eq_ignore_ascii_case(TRANSFER) {
            let from = topics.and_then(|values| topic_address(values.get(1)));
            let to = topics.and_then(|values| topic_address(values.get(2)));
            let from_us = from
                .map(|address| entity.contains(&address))
                .unwrap_or(false);
            let to_us = to.map(|address| entity.contains(&address)).unwrap_or(false);
            if from_us && !to_us {
                delta = delta.saturating_sub(amount);
            } else if to_us && !from_us {
                delta = delta.saturating_add(amount);
            }
        } else if topic.eq_ignore_ascii_case(DEPOSIT) {
            if topics
                .and_then(|values| topic_address(values.get(1)))
                .map(|address| entity.contains(&address))
                .unwrap_or(false)
            {
                delta = delta.saturating_add(amount);
            }
        } else if topic.eq_ignore_ascii_case(WITHDRAWAL)
            && topics
                .and_then(|values| topic_address(values.get(1)))
                .map(|address| entity.contains(&address))
                .unwrap_or(false)
        {
            delta = delta.saturating_sub(amount);
        }
    }
    delta
}

#[cfg(test)]
mod tests {
    use super::*;

    fn topic(address: Address) -> String {
        format!("0x{:064x}", U256::from_be_slice(address.as_slice()))
    }

    #[test]
    fn transfer_delta_is_exact_for_the_tracked_entity() {
        let weth = Address::with_last_byte(1);
        let entity_address = Address::with_last_byte(2);
        let stranger = Address::with_last_byte(3);
        let receipt = json!({"logs": [{
            "address": format!("{weth:?}"),
            "topics": [TRANSFER, topic(stranger), topic(entity_address)],
            "data": format!("0x{:064x}", U256::from(50u8))
        }, {
            "address": format!("{weth:?}"),
            "topics": [TRANSFER, topic(entity_address), topic(stranger)],
            "data": format!("0x{:064x}", U256::from(7u8))
        }]});
        assert_eq!(
            weth_delta(&receipt, weth, &HashSet::from([entity_address])),
            43
        );
    }

    #[test]
    fn swaps_are_attributed_to_the_log_address() {
        let pool = Address::with_last_byte(9);
        let receipt = json!({"logs": [{"address": format!("{pool:?}"), "topics": [V2_SWAP]}]});
        assert_eq!(swap_pools(&receipt), HashSet::from([pool]));
    }
}
