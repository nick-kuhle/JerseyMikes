//! Base launch discovery (work order 4.1).
//!
//! Watches the chain's factory contracts for pool-creation events and turns
//! each WETH-paired creation into a typed, provenance-carrying launch
//! observation. Three verified event shapes:
//!
//! - UniV2/SushiV2 `PairCreated(address,address,address,uint256)` (two
//!   indexed tokens, pair in word 1 of data);
//! - Aerodrome `PoolCreated(address,address,bool,address,uint256)` (indexed
//!   `stable` — stable pools are creation events the sniper cannot quote
//!   and are discarded, never relabelled as volatile);
//! - UniswapV3 `PoolCreated(address,address,uint24,int24,address)` (three
//!   indexed, pool in word 1 of data) — discovery only: no execution
//!   adapter exists for it yet, so V3 events persist as observations and
//!   are never fed to admission.
//!
//! Source and state provenance are part of the event: venue, block, tx
//! hash and log index travel with it, so a row in `sniper_launches` always
//! answers "which exact log on which chain said this exists".
//!
//! Canonical-only by construction: `eth_getLogs` reads sealed blocks, so
//! anything this module emits already happened on chain. Per the work
//! order that is **observation, not a competitive launch entry** — the
//! admission path keeps its own gates (verdict, probe, liquidity), and
//! nothing here claims front-running capability.

use alloy_primitives::{Address, B256};
use serde_json::Value;

use crate::dex::Venue;
use crate::rpc::RpcClient;

/// One decoded pool-creation log with full provenance.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LaunchEvent {
    pub venue: Venue,
    /// The non-WETH side of the new pool — the buyable token.
    pub token: Address,
    pub pair: Address,
    pub block_number: u64,
    pub tx_hash: B256,
    pub log_index: u64,
}

/// The non-WETH side of a token couple, or `None` when the pool is not a
/// WETH market (token–token pools are not launch buys for this lane, and a
/// WETH/WETH pool is nonsense rather than a launch).
fn non_weth(token0: Address, token1: Address, weth: Address) -> Option<Address> {
    match (token0 == weth, token1 == weth) {
        (true, false) => Some(token1),
        (false, true) => Some(token0),
        _ => None,
    }
}

/// Decode one UniV2 `PairCreated` log with launch provenance.
fn from_v2_log(log: &Value, factories: &[(Venue, Address)], weth: Address) -> Option<LaunchEvent> {
    let topics = log["topics"].as_array()?;
    if !topics
        .first()?
        .as_str()?
        .eq_ignore_ascii_case(crate::strategies::V2_PAIR_CREATED_TOPIC)
        || topics.len() < 3
    {
        return None;
    }
    let venue = crate::strategies::venue_from_factory(&log["address"], factories)?;
    let t0 = crate::types::parse_bytes(&topics[1]);
    let t1 = crate::types::parse_bytes(&topics[2]);
    if t0.len() < 32 || t1.len() < 32 {
        return None;
    }
    let token0 = Address::from_slice(&t0[12..32]);
    let token1 = Address::from_slice(&t1[12..32]);
    let data = crate::types::parse_bytes(&log["data"]);
    if data.len() < 32 {
        return None;
    }
    Some(LaunchEvent {
        venue,
        token: non_weth(token0, token1, weth)?,
        pair: Address::from_slice(&data[12..32]),
        block_number: crate::types::parse_u64(&log["blockNumber"]),
        tx_hash: crate::types::parse_b256(&log["transactionHash"])?,
        log_index: crate::types::parse_u64(&log["logIndex"]),
    })
}

/// Decode one Aerodrome `PoolCreated` log. Volatile pools only: a stable
/// pool's invariant is a different price function the sniper has no quote
/// for, so the event is skipped entirely rather than marked "volatile".
fn from_aero_log(log: &Value, weth: Address) -> Option<LaunchEvent> {
    let seed = crate::strategies::decode_aero_pool_created(log)?;
    if seed.stable {
        return None;
    }
    Some(LaunchEvent {
        venue: Venue::AeroVolatile,
        token: non_weth(seed.token0, seed.token1, weth)?,
        pair: seed.address,
        block_number: crate::types::parse_u64(&log["blockNumber"]),
        tx_hash: crate::types::parse_b256(&log["transactionHash"])?,
        log_index: crate::types::parse_u64(&log["logIndex"]),
    })
}

/// Decode one UniswapV3 `PoolCreated` log with launch provenance.
fn from_v3_log(log: &Value, weth: Address) -> Option<LaunchEvent> {
    let topics = log["topics"].as_array()?;
    if !topics
        .first()?
        .as_str()?
        .eq_ignore_ascii_case(crate::strategies::V3_POOL_CREATED_TOPIC)
        || topics.len() < 4
    {
        return None;
    }
    let t0 = crate::types::parse_bytes(&topics[1]);
    let t1 = crate::types::parse_bytes(&topics[2]);
    if t0.len() < 32 || t1.len() < 32 {
        return None;
    }
    let token0 = Address::from_slice(&t0[12..32]);
    let token1 = Address::from_slice(&t1[12..32]);
    // Pool address: second word of data, right-aligned (first word is tickSpacing).
    // Layout: topics = token0, token1, fee (indexed); data = tickSpacing, pool.
    // Verified against `strategies::decode_pool_created` which uses data[44..64].
    let data = crate::types::parse_bytes(&log["data"]);
    if data.len() < 64 {
        return None;
    }
    Some(LaunchEvent {
        venue: Venue::UniV3,
        token: non_weth(token0, token1, weth)?,
        pair: Address::from_slice(&data[44..64]),
        block_number: crate::types::parse_u64(&log["blockNumber"]),
        tx_hash: crate::types::parse_b256(&log["transactionHash"])?,
        log_index: crate::types::parse_u64(&log["logIndex"]),
    })
}

/// Scan `[from, to]` for pool-creation logs on every registered factory,
/// once per event family (three `eth_getLogs` max, only for factories the
/// chain actually has). `None` means one of the RPC calls failed — the
/// caller must not advance its scan cursor past a range it could not read.
pub async fn scan_launch_events(
    rpc: &RpcClient,
    from: u64,
    to: u64,
    pair_factories: &[(Venue, Address)],
    v3_factory: Option<Address>,
    aero_factory: Option<Address>,
    weth: Address,
) -> Option<Vec<LaunchEvent>> {
    let mut out = Vec::new();

    if !pair_factories.is_empty() {
        let addresses: Vec<Address> = pair_factories.iter().map(|(_, a)| *a).collect();
        let logs = crate::strategies::scan_factory_logs(
            rpc,
            &addresses,
            crate::strategies::V2_PAIR_CREATED_TOPIC,
            from,
            to,
        )
        .await?;
        out.extend(
            logs.iter()
                .filter_map(|log| from_v2_log(log, pair_factories, weth)),
        );
    }

    if let Some(factory) = v3_factory {
        let logs = crate::strategies::scan_factory_logs(
            rpc,
            &[factory],
            crate::strategies::V3_POOL_CREATED_TOPIC,
            from,
            to,
        )
        .await?;
        out.extend(logs.iter().filter_map(|log| from_v3_log(log, weth)));
    }

    if let Some(factory) = aero_factory {
        let logs = crate::strategies::scan_factory_logs(
            rpc,
            &[factory],
            crate::dex::AERO_POOL_CREATED_TOPIC,
            from,
            to,
        )
        .await?;
        out.extend(logs.iter().filter_map(|log| from_aero_log(log, weth)));
    }

    // Deterministic emission order: chain order, as the logs landed.
    out.sort_by_key(|e| (e.block_number, e.log_index));
    Some(out)
}

/// How far behind the head a first-ever scan reaches — the bounded
/// "canonical-block catch-up for completeness and research" from the work
/// order. Never the chain's whole history.
pub const SCAN_BACKFILL_BLOCKS: u64 = 16;

/// The largest range a single pass will read. A pass that fell further
/// behind (RPC outage, restart) clamps to the newest window rather than
/// fire-hosing `eth_getLogs`; the cursor still advances past what it read.
pub const SCAN_MAX_RANGE_BLOCKS: u64 = 64;

/// Sentinel for "no successful scan pass yet" (engine-side cursor init).
pub const CURSOR_NEVER: u64 = u64::MAX;

/// The block range the next canonical scan pass must read, or `None` when
/// the cursor already covers `head`. The cursor is the highest block a
/// previous pass *successfully* read — a failed pass never advances it,
/// so its range is retried rather than silently skipped.
pub fn scan_window(cursor: u64, head: u64) -> Option<(u64, u64)> {
    if cursor == CURSOR_NEVER {
        return Some((head.saturating_sub(SCAN_BACKFILL_BLOCKS), head));
    }
    if cursor >= head {
        return None;
    }
    Some((
        head.saturating_sub(SCAN_MAX_RANGE_BLOCKS).max(cursor + 1),
        head,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const WETH: Address = known::BASE_WETH;
    use crate::config::known;

    /// Real Base log, block 50424273 (0x30169d1): BaseSwap factory created
    /// WETH / 0x9630…32ad pair 0xc175…9e97 in tx 0x716f…5a68, log 0x108.
    fn real_v2_log() -> Value {
        serde_json::json!({
            "address": "0x8909dc15e40173ff4699343b6eb8132c65e18ec6",
            "blockNumber": "0x30169d1",
            "data": "0x000000000000000000000000c175960f630044a5a74627b78d39d202d4ee9e9700000000000000000000000000000000000000000000000000000000002e7dac",
            "logIndex": "0x108",
            "topics": [
                "0x0d3648bd0f6ba80134a33ba9275ac585d9d315f0ad8355cddefde31afa28d0e9",
                "0x0000000000000000000000004200000000000000000000000000000000000006",
                "0x0000000000000000000000009630ececf6db99c2fbee57415c9002dcee8a32ad"
            ],
            "transactionHash": "0x716f3985a730f4946f7a5b1ade6576bf3224605c0ecafe93b9267567ad8a5a68"
        })
    }

    /// Real Base log, block 50423380 (0x3016654): Aerodrome factory created
    /// the volatile 0x747e…ecd6 / 0xd9aa…b6ca pool 0xa5c9…fb8f (stable
    /// topic is zero) in tx 0xa209…7d77, log 0x25d.
    fn real_aero_log() -> Value {
        serde_json::json!({
            "address": "0x420dd381b31aef6683db6b902084cb0ffece40da",
            "blockNumber": "0x3016654",
            "data": "0x000000000000000000000000a5c9fcf73d96ed646c61598137df5125977ffb8f0000000000000000000000000000000000000000000000000000000000006fe6",
            "logIndex": "0x25d",
            "topics": [
                "0x2128d88d14c80cb081c1252a5acff7a264671bf199ce226b53788fb26065005e",
                "0x000000000000000000000000747e3705e030fe7afc19a3e8c103d3ac3ad8ecd6",
                "0x000000000000000000000000d9aaec86b65d86f6a7b5b1b0c42ffa531710b6ca",
                "0x0000000000000000000000000000000000000000000000000000000000000000"
            ],
            "transactionHash": "0xa20934cbe492ecb868bc8b74708272010a4449685d8b912a1199882cd5e27d77"
        })
    }

    /// Real Base log, block 50424058 (0x30168fa): UniV3 factory created the
    /// 1% WETH / 0xda3d…5450 pool 0xfed0…960c in tx 0x765e…89e6b, log 0x8d.
    fn real_v3_log() -> Value {
        serde_json::json!({
            "address": "0x33128a8fc17869897dce68ed026d694621f6fdfd",
            "blockNumber": "0x30168fa",
            "data": "0x00000000000000000000000000000000000000000000000000000000000000c8000000000000000000000000fed0f301d4d2c91558cc60b791163d952b04960c",
            "logIndex": "0x8d",
            "topics": [
                "0x783cca1c0412dd0d695e784568c96da2e9c22ff989357a2e8b1d9b2b4e6b7118",
                "0x0000000000000000000000004200000000000000000000000000000000000006",
                "0x000000000000000000000000da3dc35fa2a848b642e75169812ae6aca5645450",
                "0x0000000000000000000000000000000000000000000000000000000000002710"
            ],
            "transactionHash": "0x765e8dceb389ee64a6cf182cf280e27c77b976bd20dae4c24d7fce3282b89e6b"
        })
    }

    fn b256(hex_str: &str) -> B256 {
        let raw = hex::decode(hex_str.trim_start_matches("0x")).unwrap();
        B256::from_slice(&raw)
    }

    #[test]
    fn a_real_v2_pair_created_carries_launch_provenance() {
        let factories = [(Venue::UniV2, known::BASE_UNIV2_FACTORY)];
        let ev = from_v2_log(&real_v2_log(), &factories, WETH).expect("decodes");
        assert_eq!(ev.venue, Venue::UniV2);
        // WETH was token0, so the token is the other side.
        assert_eq!(
            ev.token,
            "0x9630ececf6db99c2fbee57415c9002dcee8a32ad"
                .parse::<Address>()
                .unwrap()
        );
        assert_eq!(
            ev.pair,
            "0xc175960f630044a5a74627b78d39d202d4ee9e97"
                .parse::<Address>()
                .unwrap()
        );
        // 0x30169d1 = 50424273 (fixture hex is source of truth).
        assert_eq!(ev.block_number, 50_424_273);
        assert_eq!(
            ev.tx_hash,
            b256("0x716f3985a730f4946f7a5b1ade6576bf3224605c0ecafe93b9267567ad8a5a68")
        );
        assert_eq!(ev.log_index, 0x108);
    }

    #[test]
    fn a_real_aero_pool_created_is_volatile_only_with_provenance() {
        let ev = from_aero_log(&real_aero_log(), WETH);
        // Neither side is WETH on this real log: the launch filter must drop
        // it, not force it into the funnel.
        assert!(ev.is_none());

        // Same shape, token1 swapped for WETH: keeps every provenance field.
        let mut log = real_aero_log();
        log["topics"][2] =
            serde_json::json!("0x0000000000000000000000004200000000000000000000000000000000000006");
        let ev = from_aero_log(&log, WETH).expect("WETH market decodes");
        assert_eq!(ev.venue, Venue::AeroVolatile);
        assert_eq!(
            ev.token,
            "0x747e3705e030fe7afc19a3e8c103d3ac3ad8ecd6"
                .parse::<Address>()
                .unwrap()
        );
        assert_eq!(
            ev.pair,
            "0xa5c9fcf73d96ed646c61598137df5125977ffb8f"
                .parse::<Address>()
                .unwrap()
        );
        // 0x3016654 = 50423380 (fixture hex is source of truth).
        assert_eq!(ev.block_number, 50_423_380);
        assert_eq!(ev.log_index, 0x25d);

        // A stable pool is a different price function the sniper cannot
        // quote: the event is dropped, never relabelled volatile.
        log["topics"][3] =
            serde_json::json!("0x0000000000000000000000000000000000000000000000000000000000000001");
        assert!(from_aero_log(&log, WETH).is_none());
    }

    #[test]
    fn a_real_v3_pool_created_decodes_as_an_observation() {
        let ev = from_v3_log(&real_v3_log(), WETH).expect("decodes");
        assert_eq!(ev.venue, Venue::UniV3);
        assert_eq!(
            ev.token,
            "0xda3dc35fa2a848b642e75169812ae6aca5645450"
                .parse::<Address>()
                .unwrap()
        );
        assert_eq!(
            ev.pair,
            "0xfed0f301d4d2c91558cc60b791163d952b04960c"
                .parse::<Address>()
                .unwrap()
        );
        assert_eq!(ev.block_number, 50_424_058);
        assert_eq!(
            ev.tx_hash,
            b256("0x765e8dceb389ee64a6cf182cf280e27c77b976bd20dae4c24d7fce3282b89e6b")
        );
        assert_eq!(ev.log_index, 0x8d);
    }

    #[test]
    fn non_weth_pools_and_nonsense_pools_are_not_launches() {
        let a = Address::with_last_byte(1);
        let b = Address::with_last_byte(2);
        assert_eq!(non_weth(WETH, a, WETH), Some(a));
        assert_eq!(non_weth(a, WETH, WETH), Some(a));
        assert_eq!(
            non_weth(a, b, WETH),
            None,
            "token-token is not a launch buy"
        );
        assert_eq!(non_weth(WETH, WETH, WETH), None, "nonsense, not a launch");
    }

    #[test]
    fn logs_of_the_wrong_event_family_are_ignored() {
        // Aero log through the V2 decoder (and vice versa) must not decode:
        // cross-family misreads are how a V3 pool once landed in a V2 cache.
        let factories = [(Venue::UniV2, known::BASE_UNIV2_FACTORY)];
        assert!(from_v2_log(&real_aero_log(), &factories, WETH).is_none());
        assert!(from_aero_log(&real_v2_log(), WETH).is_none());
        assert!(from_v3_log(&real_v2_log(), WETH).is_none());
        // And a V2 log from an unregistered factory is nobody's launch.
        let wrong = [(Venue::UniV2, Address::with_last_byte(9))];
        assert!(from_v2_log(&real_v2_log(), &wrong, WETH).is_none());
    }

    #[test]
    fn scan_window_bounds_backfill_retries_and_catch_up() {
        // Never scanned: a bounded window just behind the head.
        assert_eq!(
            scan_window(CURSOR_NEVER, 100),
            Some((100 - SCAN_BACKFILL_BLOCKS, 100))
        );
        // Genesis-safe: saturating, never underflowing.
        assert_eq!(scan_window(CURSOR_NEVER, 3), Some((0, 3)));
        // Steady state: exactly the blocks since the last good pass.
        assert_eq!(scan_window(100, 101), Some((101, 101)));
        assert_eq!(scan_window(100, 103), Some((101, 103)));
        // Nothing new: a repeated or reorged head reads nothing.
        assert_eq!(scan_window(100, 100), None);
        assert_eq!(scan_window(100, 99), None);
        // Far behind (RPC outage): clamp to the newest range, no firehose.
        assert_eq!(
            scan_window(10, 1_000),
            Some((1_000 - SCAN_MAX_RANGE_BLOCKS, 1_000))
        );
    }
}
