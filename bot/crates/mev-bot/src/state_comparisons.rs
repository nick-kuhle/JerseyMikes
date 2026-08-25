//! Independent `state_comparisons` qualification producer (work order 3.1).
//!
//! For every atomic-arb candidate the engine admits, the exact sized route
//! is captured **at its source state** and re-quoted against the canonical
//! sealed block that realises that state. The predicted and realised gross
//! deltas land in `state_comparisons`, a population the qualification gate
//! reads independently of any own-execution row (`actual_mev_matches`).
//!
//! Correctness invariants:
//!
//! * **Unique.** Sample id is `sc:{opportunity_id}`; the store additionally
//!   enforces `UNIQUE(opportunity_id, source_state_id, route, direction,
//!   amount_in)` with `INSERT OR IGNORE`, so replayed duplicate frames
//!   cannot inflate counts.
//! * **Reorg-aware.** Rows key on the sealed block they were measured at;
//!   `Store::record_reorg` flips their `canonical` flag, which removes them
//!   from the qualification population.
//! * **No cross-matching.** A row exists only for its own (state, route,
//!   direction, amount) tuple — the unique identity — and the producer
//!   never writes execution rows, so a route match can satisfy only one
//!   population.
//! * **Fail closed.** Any fetch failure, unquotable venue, zero output, or
//!   missed seal drops the sample instead of writing evidence from
//!   fabricated state.

use std::collections::VecDeque;

use alloy_primitives::U256;
use tracing::debug;

use crate::rpc::RpcClient;
use crate::store::AsyncStore;
use crate::types::{Opportunity, RouteHop};

/// Oldest a pending sample may get relative to the head before its seal is
/// considered missed for good (the normal path settles on the very next
/// head).
pub const MAX_COMPARISON_AGE_BLOCKS: u64 = 64;
/// Bounded in-memory backlog. Well above per-block candidate volume on Base;
/// overflow drops the *new* candidate's sample rather than evicting proof
/// that is closer to settling.
pub const MAX_PENDING_COMPARISONS: usize = 256;

/// The exact re-quotable identity of one emitted arb candidate.
#[derive(Clone, Debug)]
pub struct PendingStateComparison {
    /// `sc:{opportunity_id}` — deterministic, so duplicate captures collapse.
    pub sample_id: String,
    pub opportunity_id: String,
    /// Source-state identity at measurement time: `pin:{state_id}` for
    /// preconfirmed-pinned candidates, `head:{hash}` for canonical ones.
    pub source_state_id: String,
    /// Expected net-free gross profit at the source state, in anchor wei.
    pub predicted_wei: i128,
    /// The sealed block whose canonical state must be re-quoted.
    pub target_block: u64,
    pub route: String,
    /// Ordered hop identities — the machine-readable route.
    pub hops: Vec<RouteHop>,
    /// Anchor input of the route, in wei.
    pub amount_in: U256,
    pub direction: String,
}

/// Capture the candidate's identity for later settlement. Only atomic-arb
/// candidates that carried a full machine-readable route qualify; anything
/// else is outside this population (work order 3.1 scopes it to atomic arb).
///
/// `head` is the canonical head the engine was at when the candidate was
/// admitted — the source state for candidates without a preconfirmed pin.
pub fn capture(
    opp: &Opportunity,
    head: &crate::types::BlockHead,
) -> Option<PendingStateComparison> {
    use crate::types::Strategy;
    if opp.strategy != Strategy::AtomicArb {
        return None;
    }
    let hops = &opp.provenance.route_hops;
    if hops.len() < 2 {
        return None;
    }
    let source_state_id = match &opp.provenance.source_state {
        Some(pin) => format!("pin:{:?}", pin.state_id),
        None => format!("head:{:?}", head.hash),
    };
    Some(PendingStateComparison {
        sample_id: format!("sc:{}", opp.id),
        opportunity_id: opp.id.clone(),
        source_state_id,
        predicted_wei: crate::sim::anvil::to_i128(opp.provenance.predicted_gross_wei),
        target_block: opp.target_block,
        route: opp.provenance.route.clone(),
        hops: hops.clone(),
        amount_in: opp.notional_wei,
        direction: opp.provenance.direction.clone(),
    })
}

/// Push into the bounded pending set. Returns false when the backlog is
/// full (the sample is dropped — never queue unbounded work off the feed).
pub fn push(pending: &mut VecDeque<PendingStateComparison>, item: PendingStateComparison) -> bool {
    if pending.iter().any(|p| p.sample_id == item.sample_id) {
        return true; // duplicate frame: already pending, no second proof request
    }
    if pending.len() >= MAX_PENDING_COMPARISONS {
        return false;
    }
    pending.push_back(item);
    true
}

/// How a settle pass ended for the pending set.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SettleOutcome {
    /// Rows written (or confirmed already written — the store dedupes).
    pub settled: usize,
    /// Samples dropped without writing (see module docs for the gates).
    pub dropped: usize,
    /// Samples carried to the next head (their seal has not landed).
    pub kept: usize,
}

/// Re-quote every pending sample whose seal has landed, at the sealed
/// canonical block, and record the comparison through the async writer.
///
/// Samples settle exactly once: on the first head at or past their target.
/// A head *past* target means the in-order head tick skipped (or restarted
/// across) its seal; re-quoting at `target_block` is still the correct
/// canonical state for that sample (source state `head:H` realises into
/// sealed `H+1` — `target_block` — regardless of when we look at it).
pub async fn settle(
    pending: &mut VecDeque<PendingStateComparison>,
    sealed: &crate::types::BlockHead,
    rpc: &RpcClient,
    aero_factory: Option<alloy_primitives::Address>,
    writes: &AsyncStore,
) -> SettleOutcome {
    let mut out = SettleOutcome::default();
    let mut kept = VecDeque::with_capacity(pending.len());
    for item in pending.drain(..) {
        if item.target_block > sealed.number {
            kept.push_back(item);
            continue;
        }
        if sealed.number.saturating_sub(item.target_block) > MAX_COMPARISON_AGE_BLOCKS {
            // The seal is too far back to trust the RPC for the historical
            // state on a light endpoint. Drop rather than approximate.
            out.dropped += 1;
            continue;
        }
        match requote(&item, rpc, aero_factory).await {
            Ok(realized) => {
                writes.record_state_comparison(
                    item.sample_id.clone(),
                    item.opportunity_id.clone(),
                    "arb",
                    item.source_state_id.clone(),
                    item.target_block,
                    format!("{:?}", sealed.hash),
                    item.route.clone(),
                    item.amount_in.to_string(),
                    item.direction.clone(),
                    item.predicted_wei,
                    realized,
                );
                debug!(
                    target: "engine",
                    sample = %item.sample_id,
                    predicted = item.predicted_wei,
                    realized,
                    "state comparison settled"
                );
                out.settled += 1;
            }
            Err(reason) => {
                debug!(
                    target: "engine",
                    sample = %item.sample_id,
                    %reason,
                    "state comparison dropped"
                );
                out.dropped += 1;
            }
        }
    }
    out.kept = kept.len();
    *pending = kept;
    out
}

/// Thread `amount_in` through the ordered hops at the canonical block.
/// Errors are descriptive strings: the caller counts them as drops.
async fn requote(
    item: &PendingStateComparison,
    rpc: &RpcClient,
    aero_factory: Option<alloy_primitives::Address>,
) -> Result<i128, String> {
    let mut amount = item.amount_in;
    if amount.is_zero() {
        return Err("zero amount".into());
    }
    for (i, hop) in item.hops.iter().enumerate() {
        let out = crate::dex::requote_hop(rpc, hop, aero_factory, amount, item.target_block)
            .await
            .map_err(|e| format!("hop {i} fetch at {}: {e:#}", item.target_block))?
            .ok_or_else(|| format!("hop {i} unquotable venue at {}", item.target_block))?;
        if out.is_zero() {
            return Err(format!("hop {i} quotes zero at {}", item.target_block));
        }
        amount = out;
    }
    Ok(crate::sim::anvil::to_i128(amount) - crate::sim::anvil::to_i128(item.amount_in))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::B256;
    use serde_json::{json, Value};
    use std::sync::Arc;

    use crate::config::known;
    use crate::dex::{AeroPool, Venue};

    /// Poll until the background writer has landed `want` comparison rows
    /// (bounded: a hung writer fails the test instead of looping forever).
    async fn wait_rows(
        store: &crate::store::Store,
        strategy: &str,
        want: usize,
    ) -> Vec<crate::store::StateComparisonRow> {
        for _ in 0..200 {
            let rows = store
                .recorded_state_comparisons_for_test(strategy)
                .expect("rows readable");
            if rows.len() >= want {
                return rows;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("writer never landed {want} rows")
    }

    /// Two pools diverged by ~1% (beyond the ~0.6% two-fee band): selling
    /// WETH for USDC on the V2 pool and buying WETH back on the aero pool
    /// nets a positive gross, so the pinned value is an honest profit case.
    /// The exact gross is *derived from the venue math helper*, never
    /// hand-pinned to a round number.
    const WETH_IN: u128 = 800_000_000_000_000_000; // 0.8 WETH
    const V2_R0: u128 = 1_000_000_000_000_000_000_000; // 1000 WETH
    const V2_R1: u128 = 2_000_000_000_000u128; // 2,000,000 USDC (2000/WETH)
    const AERO_R0: u128 = 1_000_000_000_000_000_000_000; // 1000 WETH
    const AERO_R1: u128 = 1_980_000_000_000u128; // 1,980,000 USDC (1980/WETH)

    /// Gross profit in WETH wei for the shaped fixture, from the exact same
    /// math helpers the strategy prices with.
    fn expected_gross() -> i128 {
        let aero = AeroPool {
            address: known::BASE_AERO_WETH_USDC_VOLATILE,
            token0: known::BASE_WETH,
            token1: known::BASE_USDC,
            reserve0: U256::from(AERO_R0),
            reserve1: U256::from(AERO_R1),
            fee_bps: 30,
            stable: false,
            block: 42,
        };
        let usdc_out = crate::dex::v2_amount_out(
            U256::from(WETH_IN),
            U256::from(V2_R0),
            U256::from(V2_R1),
            30,
        );
        let out = aero
            .amount_out(known::BASE_USDC, usdc_out)
            .expect("aero quotes");
        let gross = crate::sim::anvil::to_i128(U256::from(out)) - WETH_IN as i128;
        assert!(gross > 0, "fixture pools must yield a positive round trip");
        gross
    }

    fn word(v: u128) -> String {
        format!("0x{:064x}", v)
    }

    fn test_item() -> PendingStateComparison {
        let weth = known::BASE_WETH;
        let usdc = known::BASE_USDC;
        let aero = AeroPool {
            address: known::BASE_AERO_WETH_USDC_VOLATILE,
            token0: weth,
            token1: usdc,
            reserve0: U256::from(AERO_R0),
            reserve1: U256::from(AERO_R1),
            fee_bps: 30,
            stable: false,
            block: 42,
        };
        let gross = expected_gross();
        let hops = vec![
            RouteHop {
                venue: Venue::UniV2,
                pool: known::BASE_UNIV2_FACTORY, // used as the pair for the mock
                token_in: weth,
                fee_bps: 30,
            },
            crate::dex::hop_for_aero(&aero, usdc),
        ];
        PendingStateComparison {
            sample_id: "sc:test-1".into(),
            opportunity_id: "test-1".into(),
            source_state_id: "head:0xabc".into(),
            predicted_wei: gross,
            target_block: 43,
            route: "univ2:0x1 -> aerodrome:0x2".into(),
            hops,
            amount_in: U256::from(WETH_IN),
            direction: "forward".into(),
        }
    }

    /// Mock JSON-RPC server answering the exact calls `requote_hop` makes:
    /// token0/token1/getReserves for the V2 pair and token0/token1/
    /// getReserves + factory getFee for the aero pool.
    async fn mock_rpc(alter_aero_reserves: bool) -> (String, Arc<tokio::sync::Mutex<Vec<Value>>>) {
        let seen: Arc<tokio::sync::Mutex<Vec<Value>>> =
            Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let st = seen.clone();
        let app = axum::Router::new().route(
            "/",
            axum::routing::post(
                move |axum::extract::Json(req): axum::extract::Json<Value>| {
                    let st = st.clone();
                    async move {
                        st.lock().await.push(req.clone());
                        // The pool fetchers batch their descriptive calls into a
                        // single JSON-RPC array; answer element-wise.
                        let answer = |one: &Value| -> Value {
                            let method = one["method"].as_str().unwrap_or("");
                            let params = &one["params"];
                            let id = one["id"].clone();
                            match method {
                                "eth_call" => json!({
                                    "jsonrpc": "2.0", "id": id,
                                    "result": mock_eth_call(params, alter_aero_reserves)
                                }),
                                other => json!({
                                    "jsonrpc": "2.0", "id": id,
                                    "error": {"code": -32601, "message": format!("no {other}")}
                                }),
                            }
                        };
                        if let Some(batch) = req.as_array() {
                            let out: Vec<Value> = batch.iter().map(answer).collect();
                            return axum::Json(Value::Array(out));
                        }
                        axum::Json(answer(&req))
                    }
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{addr}"), seen)
    }

    fn mock_eth_call(params: &Value, alter_aero: bool) -> String {
        let to = params[0]["to"].as_str().unwrap_or_default().to_lowercase();
        let data = params[0]["data"].as_str().unwrap_or_default();
        let selector = if data.len() >= 10 { &data[..10] } else { data };
        let weth = format!("{:?}", known::BASE_WETH).to_lowercase();
        let usdc = format!("{:?}", known::BASE_USDC).to_lowercase();
        let aero_factory = format!("{:?}", known::BASE_AERODROME_FACTORY).to_lowercase();
        let aero_pool = format!("{:?}", known::BASE_AERO_WETH_USDC_VOLATILE).to_lowercase();
        match selector {
            // token0()
            "0x0dfe1681" => pad_addr(&weth),
            // token1()
            "0xd21220a7" => pad_addr(&usdc),
            // stable()
            "0x22be3de1" => word(0),
            // getReserves() — answer per callee: factory-as-V2-pair vs aero pool
            "0x0902f1ac" => {
                let (r0, r1) = if to == aero_pool {
                    if alter_aero {
                        // The aero pool got deeper in USDC → its WETH price
                        // moved *against* the round trip: the arb is gone at
                        // the canonical seal (realized < predicted).
                        (AERO_R0, AERO_R1 + AERO_R1 / 64)
                    } else {
                        (AERO_R0, AERO_R1)
                    }
                } else {
                    (V2_R0, V2_R1)
                };
                format!(
                    "{}{}{:064x}",
                    word(V2_R0)[2..].replacen(&word(V2_R0)[2..], &word(r0)[2..], 1)[..0]
                        .to_string()
                        + "",
                    0u64,
                    0u64
                )[..0]
                    .to_string();
                format!("0x{}{}{:064x}", &word(r0)[2..], &word(r1)[2..], 0u64)
            }
            // getFee(pool, stable) on the aero factory
            "0xcc56b2c5" => {
                assert_eq!(to, aero_factory, "getFee must target the aerodrome factory");
                word(30)
            }
            other => panic!("unmocked eth_call selector {other} to {to}"),
        }
    }

    fn pad_addr(hex_addr: &str) -> String {
        format!(
            "0x000000000000000000000000{}",
            hex_addr.trim_start_matches("0x")
        )
    }

    fn head(number: u64) -> crate::types::BlockHead {
        crate::types::BlockHead {
            number,
            hash: B256::with_last_byte(number as u8),
            parent_hash: B256::ZERO,
            timestamp: 1_700_000_000,
            base_fee_per_gas: U256::from(100u64),
            gas_used: 0,
            gas_limit: 30_000_000,
        }
    }

    #[tokio::test]
    async fn settles_at_the_exact_seal_and_records_realized() {
        let store = std::sync::Arc::new(crate::store::Store::open_in_memory().unwrap());
        let item = test_item();
        let (url, _seen) = mock_rpc(false).await;
        let rpc = crate::rpc::RpcClient::new(url).unwrap();
        let writes = crate::store::AsyncStore::spawn(store.clone(), 64);

        // Not yet sealed: kept.
        let mut pending = VecDeque::from([item.clone()]);
        let out = settle(&mut pending, &head(42), &rpc, None, &writes).await;
        assert_eq!(
            out,
            SettleOutcome {
                settled: 0,
                dropped: 0,
                kept: 1
            }
        );
        assert_eq!(pending.len(), 1);

        // Sealed: settle exactly once at the canonical block.
        let out = settle(
            &mut pending,
            &head(43),
            &rpc,
            Some(known::BASE_AERODROME_FACTORY),
            &writes,
        )
        .await;
        assert_eq!(out.settled, 1, "expected one write, got {out:?}");
        assert!(pending.is_empty());
        let rows = wait_rows(&store, "arb", 1).await;
        assert_eq!(rows.len(), 1);
        let gross = expected_gross();
        let r = &rows[0];
        assert_eq!(r.canonical_block, 43);
        assert_eq!(r.predicted_wei, gross);
        assert_eq!(r.realized_wei, gross);
        assert_eq!(r.amount_in, WETH_IN.to_string());
        assert_eq!(r.direction, "forward");

        // Settling the same head again cannot double-write (unique identity).
        let mut pending = VecDeque::from([item.clone()]);
        let out = settle(
            &mut pending,
            &head(43),
            &rpc,
            Some(known::BASE_AERODROME_FACTORY),
            &writes,
        )
        .await;
        assert_eq!(out.settled, 1);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let rows = store
            .recorded_state_comparisons_for_test("arb")
            .expect("rows readable");
        assert_eq!(rows.len(), 1, "duplicate frames cannot inflate counts");
    }

    #[tokio::test]
    async fn drops_irrecoverable_or_fractional_hops_and_expired_seals() {
        let store = std::sync::Arc::new(crate::store::Store::open_in_memory().unwrap());
        let item = test_item();
        let (url, _seen) = mock_rpc(true).await; // aero leg quotes thin at seal
        let rpc = crate::rpc::RpcClient::new(url).unwrap();
        let writes = crate::store::AsyncStore::spawn(store.clone(), 64);

        let mut pending = VecDeque::from([item.clone()]);
        let out = settle(
            &mut pending,
            &head(43),
            &rpc,
            Some(known::BASE_AERODROME_FACTORY),
            &writes,
        )
        .await;
        // Price moved against the route at seal: still quotes, just worse —
        // that's a real realized quote, not a failure. The drop path is exercised by
        // zero/V3 below. Crucially, the written row must show the *canonical*
        // state (realized diverges from the prediction made pre-alteration,
        // proving the producer never parrots its own forecast).
        assert_eq!(out.settled, 1);
        let rows = wait_rows(&store, "arb", 1).await;
        assert!(
            rows[0].realized_wei < rows[0].predicted_wei,
            "the decayed canonical price must show as a decayed realized quote: {:?}",
            rows[0]
        );

        // Unquotable hop (V3) must drop, never fabricate.
        let mut v3 = item.clone();
        v3.sample_id = "sc:test-v3".into();
        v3.opportunity_id = "test-v3".into();
        v3.hops[0].venue = Venue::UniV3;
        let mut pending = VecDeque::from([v3]);
        let out = settle(&mut pending, &head(43), &rpc, None, &writes).await;
        assert_eq!(out.dropped, 1);

        // Missed seal beyond the age bound drops instead of re-quoting.
        let mut stale = item.clone();
        stale.sample_id = "sc:test-stale".into();
        stale.opportunity_id = "test-stale".into();
        let mut pending = VecDeque::from([stale]);
        let out = settle(
            &mut pending,
            &head(43 + MAX_COMPARISON_AGE_BLOCKS + 1),
            &rpc,
            None,
            &writes,
        )
        .await;
        assert_eq!(out.dropped, 1);

        let rows = wait_rows(&store, "arb", 1).await;
        assert_eq!(rows.len(), 1, "only the first sample may exist");
    }

    #[test]
    fn capture_rejects_non_arb_and_routeless_candidates() {
        let item = test_item();
        let mut opp = crate::types::Opportunity {
            id: "test-1".into(),
            strategy: crate::types::Strategy::AtomicArb,
            victim_hashes: vec![],
            front_calls: vec![],
            back_calls: vec![],
            flash_tokens: vec![],
            flash_amounts: vec![],
            profit_token: known::BASE_WETH,
            expected_profit_wei: U256::from(expected_gross() as u128),
            notional_wei: U256::from(WETH_IN),
            target_block: 43,
            created_at_ms: crate::types::now_ms(),
            notes: String::new(),
            provenance: crate::types::Provenance {
                route: item.route.clone(),
                direction: item.direction.clone(),
                route_hops: item.hops.clone(),
                predicted_gross_wei: U256::from(expected_gross() as u128),
                ..Default::default()
            },
        };
        let head = head(42);
        let got = capture(&opp, &head).expect("arb candidate captures");
        assert_eq!(got.sample_id, "sc:test-1");
        assert_eq!(got.target_block, 43);
        assert!(got.source_state_id.starts_with("head:"));

        opp.strategy = crate::types::Strategy::Sandwich;
        assert!(
            capture(&opp, &head).is_none(),
            "sandwich is out of population"
        );

        opp.strategy = crate::types::Strategy::AtomicArb;
        opp.provenance.route_hops.clear();
        assert!(capture(&opp, &head).is_none(), "routeless cannot compare");
    }
}
