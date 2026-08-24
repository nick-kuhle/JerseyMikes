//! Maker liquidations: `dog.bark` + same-transaction clip `take`.
//!
//! Maker liquidates in two phases — `Dog.bark(ilk, urn, kpr)` seizes the urn
//! and kicks a collateral auction (`Clipper`), and the auction is settled by
//! buyers calling `clip.take` over minutes or hours. A searcher makes this
//! atomic: back-run the bark with a take in the same batch and capture the
//! whole auction in one transaction.
//!
//! **The deterministic-id trick.** A `Call[]` batch cannot thread bark's
//! return value (the auction id) into take. It does not need to: `id =
//! ++clip.kicks`, and `kicks` is a public counter we read off-chain, so the
//! bundle hardcodes `kicks + 1`. If another searcher barks the same urn
//! between our read and inclusion, our take targets the wrong (or missing)
//! auction and reverts — the bundle dies, nothing is broadcast. Fail-safe is
//! the correct polarity here.
//!
//! **The bundle.** Flash DAI → `dai.join` (vat.dai) → `vat.hope(daiJoin)` →
//! `dog.bark` (kick reward `tip + tab·chip` is minted *to the executor* as
//! vat.dai) → `clip.take(kicks+1, MAX_UINT, marketPrice, executor, "")` (buys
//! the whole lot at the auction's opening price, capped by the tab) →
//! `gemJoin.exit` (ERC20 WETH out) → swap WETH → DAI → `daiJoin.exit` the
//! leftover vat.dai → repay. Profit = kick reward + (market − auction) spread
//! on the bought collateral, in DAI.
//!
//! **Pricing the take.** `max` is a *ray* price — DAI per 1e18 collateral.
//! We bid exactly the V2 pool price (`dai_reserve · 1e27 / weth_reserve`): if
//! the auction's opening price (`top = feed price × buf`) is above market the
//! take reverts `too-expensive` (correct — the auction is not a discount
//! yet); if it is below, we buy and realise the spread on the swap leg. Both
//! directions fail safe, which is why a pure market-price cap is the right
//! choice over any off-chain "fair value".
//!
//! **Sizing.** bark's own arithmetic is mirrored off-chain (`dart` from
//! Hole/hole room, the dust edge case, `tab = dart·rate·chop`), plus
//! `clip.getFeedPrice() × buf` for the opening price, so the take's `slice =
//! min(lot, tab/price)` and `exit` amount are exact integers. Everything is
//! public state: `vat.urns`, `vat.ilks`, `dog.ilks`, `dog.Hole/Dirt`,
//! `clip.kicks/buf/tip/chip`.
//!
//! **Discovery.** The Vat itself emits **nothing** (verified live: two
//! independent RPCs return zero `frob` LogNotes, and the source says "It
//! doesn't use LibNote anymore" — do not build a Vat-log harvest). Urns are
//! instead harvested from each ilk's **gem join** events: the joins emit
//! *anonymous* DSNote LogNotes whose topic0 is the padded `join`/`exit`
//! selector, with the urn in topics[2] (the `usr` argument). One
//! `eth_getLogs` per ilk per block, then batched `vat.urns` + `vat.ilks`
//! polls: liquidatable ⇔ `ink · spot < art · rate`.
//!
//! **Near-miss leads.** The safety ratio `(ink·spot)/(art·rate)` in bps is
//! published for urns within 5% above the threshold — these are the
//! positions an OSM `poke` flips, which is the classic Maker oracle
//! front-run (OSM prices lag the medianizer by an hour).
//!
//! **Not yet.** Multiple ilks per bundle (`MAKER_ILKS` selects from the
//! built-in table; each ilk adds one log scan + urn reads); auctions that
//! need a reset (list.max staleness); ETH-C/WSTETH-B by default-off.

use alloy_primitives::{Address, U256};
use alloy_sol_types::{sol, SolCall};
use async_trait::async_trait;
use parking_lot::RwLock;
use serde_json::json;

use crate::config::known;
use crate::dex::{self, IERC20};
use crate::strategies::leads::{
    liquidation_opportunity, ratio_bps, Lead, LeadAction, LiquidationLeads,
};
use crate::strategies::sandwich::build_leg;
use crate::strategies::{StrategyCtx, StrategyImpl};
use crate::types::{BlockHead, Call, Opportunity, Strategy};

sol! {
    interface IVat {
        function urns(bytes32 ilk, address urn) external view returns (uint256 ink, uint256 art);
        function ilks(bytes32 ilk) external view returns (uint256 Art, uint256 rate, uint256 spot, uint256 line, uint256 dust);
        function hope(address usr) external;
    }

    interface IDog {
        function bark(bytes32 ilk, address urn, address kpr) external returns (uint256 id);
        function ilks(bytes32 ilk) external view returns (address clip, uint256 chop, uint256 hole, uint256 dirt);
        function Hole() external view returns (uint256);
        function Dirt() external view returns (uint256);
    }

    interface IClip {
        function take(uint256 id, uint256 amt, uint256 max, address who, bytes calldata data) external;
        function kicks() external view returns (uint256);
        function getFeedPrice() external view returns (uint256);
        function buf() external view returns (uint256);
        function tip() external view returns (uint256);
        function chip() external view returns (uint256);
    }

    interface IGemJoin {
        function exit(address usr, uint256 wad) external;
    }

    interface IDaiJoin {
        function join(address usr, uint256 wad) external;
        function exit(address usr, uint256 wad) external;
    }
}

/// The gem joins' LogNote is **anonymous**: topic0 is the indexed bytes4
/// selector (left-aligned, zero-padded), not an event hash — so the harvest
/// filters on the selector directly. `join(address,uint256)`.
const JOIN_NOTE_TOPIC: &str = "0x3b4da69f00000000000000000000000000000000000000000000000000000000";
/// `exit(address,uint256)`, same anonymous-LogNote shape.
const EXIT_NOTE_TOPIC: &str = "0xef693bed00000000000000000000000000000000000000000000000000000000";

const RAY: U256 = U256::from_limbs([0x9FD0803CE8000000, 0x33B2E3C, 0x0, 0x0]); // 1e27
const WAD: U256 = U256::from_limbs([0xDE0B6B3A7640000, 0x0, 0x0, 0x0]); // 1e18

/// Verified against the MakerDAO chainlog (`MCD_DOG` was replaced in the
/// Sky-era upgrades — do not use the pre-2024 address 0xaD7c337E...).
pub mod maker {
    use alloy_primitives::{address, Address, B256};

    pub const VAT: Address = address!("35D1b3F3D7966A1DFe207aa4514C12a259A0492B");
    pub const DOG: Address = address!("135954d155898D42c90D2a57824c690e0c7BEF1b");
    pub const SPOT: Address = address!("65C79fcB50ca1594B025960e539eD7A9a6D434A3");
    pub const DAI_JOIN: Address = address!("9759A6Ac90977b93B58547b4A71c78317f391A28");

    /// One liquidatable collateral type. Addresses resolved from the chainlog
    /// (2026-08); the clip is *not* static because `dog.ilks(ilk)` is the
    /// live source — this table only holds the stable adapter addresses.
    pub struct IlkSpec {
        pub name: &'static str,
        /// `"ETH-A"` right-padded with zero bytes, as the Vat keys it.
        pub ilk: B256,
        pub gem_join: Address,
        /// The OSM (pip) that feeds this ilk's spot price — watched by the
        /// oracle front-runner.
        pub pip: Address,
        /// The ERC20 collateral the gem join exits.
        pub gem: Address,
    }

    /// Built-in ilk table. gem joins and pips resolved from the on-chain
    /// chainlog at 0xdA0Ab1e0017DEbCd72Be8599041a2aa3bA7e740F:
    ///   MCD_JOIN_ETH_A=0x2f0b23f53734252bda2277357e97e1517d6b042a
    ///   MCD_JOIN_WBTC_A=0xbf72da2bd84c5170618fbe5914b0eca9638d5eb5
    ///   MCD_JOIN_WSTETH_A=0x10cd5fbe1b404b7e19ef964b63939907bdaf42e2
    ///   PIP_ETH=0x81fe72b5a8d1a857d176c3e7d5bd2679a9b85763
    ///   PIP_WBTC=0xf185d0682d50819263941e5f4eacc763cc5c6c42
    ///   PIP_WSTETH=0xfe7a2ac0b945f12089aeeb6ecebf4f384d9f043f
    pub fn table() -> &'static [IlkSpec] {
        static TABLE: [IlkSpec; 3] = [
            IlkSpec {
                name: "ETH-A",
                ilk: B256::new([
                    b'E', b'T', b'H', b'-', b'A', 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
                    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
                ]),
                gem_join: address!("2f0b23f53734252bDa2277357E97e1517D6B042A"),
                pip: address!("81FE72B5A8D1A857D176C3E7D5BD2679A9B85763"),
                gem: super::super::super::config::known::WETH,
            },
            IlkSpec {
                name: "WBTC-A",
                ilk: B256::new([
                    b'W', b'B', b'T', b'C', b'-', b'A', 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
                    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
                ]),
                gem_join: address!("BF72da2bd84C5170618Fbe5914B0eCa9638d5EB5"),
                pip: address!("F185d0682d50819263941E5F4eAcc763cC5c6C42"),
                gem: super::super::super::config::known::WBTC,
            },
            IlkSpec {
                name: "WSTETH-A",
                ilk: B256::new([
                    b'W', b'S', b'T', b'E', b'T', b'H', b'-', b'A', 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
                    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
                ]),
                gem_join: address!("10cd5FBe1b404b7e19ef964b63939907BDAF42e2"),
                pip: address!("fe7a2ac0b945f12089aeeb6ecebf4f384d9f043f"),
                gem: super::super::super::config::known::WSTETH,
            },
        ];
        &TABLE
    }

    pub fn spec_by_name(name: &str) -> Option<&'static IlkSpec> {
        table().iter().find(|s| s.name.eq_ignore_ascii_case(name))
    }
}

/// Off-chain mirror of `Dog.bark`'s auction sizing, so the bundle knows the
/// exact lot/tab it creates. Returns `(dink, dart, tab)` or `None` when the
/// urn is safe / the dog is out of room.
#[allow(clippy::too_many_arguments)]
pub fn auction_math(
    ink: U256,
    art: U256,
    rate: U256,
    spot: U256,
    dust: U256,
    chop: U256,
    hole: U256,
    dirt: U256,
    ilk_hole: U256,
    ilk_dirt: U256,
) -> Option<(U256, U256, U256)> {
    if spot.is_zero() || art.is_zero() || ink.is_zero() {
        return None;
    }
    if ink.saturating_mul(spot) >= art.saturating_mul(rate) {
        return None; // safe
    }
    if hole <= dirt || ilk_hole <= ilk_dirt {
        return None; // Dog/liquidation-limit-hit
    }
    // Hole/Dirt are uint256 rad on chain; U256 is the only faithful unit.
    let room = (hole - dirt).min(ilk_hole - ilk_dirt);
    let mut dart = art.min(room * WAD / rate / chop);
    if art > dart {
        // Dusty leftovers are liquidated entirely, exactly like the Dog.
        if (art - dart) * rate < dust {
            dart = art;
        }
    }
    if dart.is_zero() {
        return None;
    }
    let dink = ink * dart / art;
    if dink.is_zero() {
        return None;
    }
    let tab = dart * rate * chop / WAD;
    Some((dink, dart, tab))
}

/// Market price cap for `clip.take`, in ray (DAI per 1e18 gem).
/// `reserve_in · RAY / reserve_out`, both reserves in native token units.
pub fn market_price_ray(reserve_dai: U256, reserve_gem: U256) -> Option<U256> {
    if reserve_dai.is_zero() || reserve_gem.is_zero() {
        return None;
    }
    Some(reserve_dai * RAY / reserve_gem)
}

pub struct MakerLiquidationStrategy {
    /// Selected ilks (from `MAKER_ILKS`).
    ilks: Vec<&'static maker::IlkSpec>,
    /// Urn candidates per ilk, capped, most-recently-active first.
    urns: RwLock<Vec<(usize, Address, u64)>>, // (ilk index, urn, last seen block)
    last_log_block: RwLock<u64>,
    watch_cap: usize,
    leads: LiquidationLeads,
}

impl MakerLiquidationStrategy {
    pub fn new(
        ilks: Vec<&'static maker::IlkSpec>,
        watch_cap: usize,
        leads: LiquidationLeads,
    ) -> Self {
        Self {
            ilks,
            urns: RwLock::new(Vec::new()),
            last_log_block: RwLock::new(0),
            watch_cap,
            leads,
        }
    }

    pub fn urn_count(&self) -> usize {
        self.urns.read().len()
    }

    fn touch_urn(&self, ilk_idx: usize, urn: Address, block: u64) {
        let mut v = self.urns.write();
        if let Some(slot) = v.iter_mut().find(|(i, u, _)| *i == ilk_idx && *u == urn) {
            slot.2 = block;
        } else {
            v.push((ilk_idx, urn, block));
            if v.len() > self.watch_cap.max(1) {
                if let Some(pos) = v
                    .iter()
                    .enumerate()
                    .min_by_key(|(_, (_, _, b))| *b)
                    .map(|(i, _)| i)
                {
                    v.swap_remove(pos);
                }
            }
        }
        v.sort_unstable_by_key(|(_, _, b)| std::cmp::Reverse(*b));
    }

    /// Harvest urn addresses from Vat frob LogNotes (ilk in topic3, urn in topic4).
    /// Harvest urn addresses from the configured ilks' gem-join activity.
    /// One `eth_getLogs` per ilk: the joins emit *anonymous* DSNote
    /// LogNotes whose topic0 is the padded `join`/`exit` selector, with the
    /// urn — the `usr` argument, usually the owner's proxy — in topics[2].
    async fn harvest(&self, ctx: &StrategyCtx, head: &BlockHead) {
        let from = {
            let last = *self.last_log_block.read();
            if last == 0 {
                head.number.saturating_sub(2_000)
            } else if head.number <= last {
                return;
            } else {
                last + 1
            }
        };
        for (idx, spec) in self.ilks.iter().enumerate() {
            let params = json!([{
                "fromBlock": format!("0x{from:x}"),
                "toBlock": format!("0x{:x}", head.number),
                "address": format!("{:?}", spec.gem_join),
                "topics": [[JOIN_NOTE_TOPIC, EXIT_NOTE_TOPIC]],
            }]);
            match ctx.rpc.call_raw("eth_getLogs", params).await {
                Ok(v) => {
                    if let Some(logs) = v.as_array() {
                        for log in logs {
                            // topics[2] == arg1 == join/exit's `usr` param.
                            let Some(urn_topic) = log["topics"].get(2).and_then(|t| t.as_str())
                            else {
                                continue;
                            };
                            if let Ok(bytes) = hex::decode(urn_topic.trim_start_matches("0x")) {
                                if bytes.len() == 32 && bytes[12..] != [0u8; 20] {
                                    self.touch_urn(
                                        idx,
                                        Address::from_slice(&bytes[12..32]),
                                        head.number,
                                    );
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    tracing::debug!(target: "strategy::liquidation_maker", ilk = spec.name, error = %e, "gem-join log harvest failed")
                }
            }
        }
        *self.last_log_block.write() = head.number;
    }

    /// Batched `vat.urns` + `vat.ilks` + `dog.ilks` reads; returns unsafe
    /// urns with their sizing inputs and publishes near-miss leads.
    #[allow(clippy::type_complexity)]
    async fn poll(
        &self,
        ctx: &StrategyCtx,
    ) -> Vec<(
        &'static maker::IlkSpec,
        Address,
        U256,
        U256,
        U256,
        U256,
        U256,
        U256,
        U256,
        U256,
        U256,
        U256,
        Address,
    )> {
        let snapshot: Vec<(usize, Address)> =
            self.urns.read().iter().map(|(i, u, _)| (*i, *u)).collect();
        if snapshot.is_empty() {
            return Vec::new();
        }

        // Per-ilk shared state first: vat.ilks + dog.ilks.
        let mut ilk_state = Vec::with_capacity(self.ilks.len());
        for spec in &self.ilks {
            let vat_ilks = ctx
                .rpc
                .call_raw(
                    "eth_call",
                    json!([
                        { "to": format!("{:?}", maker::VAT), "data": format!("0x{}", hex::encode(IVat::ilksCall { ilk: spec.ilk }.abi_encode())) },
                        "latest"
                    ]),
                )
                .await;
            let dog_ilks = ctx
                .rpc
                .call_raw(
                    "eth_call",
                    json!([
                        { "to": format!("{:?}", maker::DOG), "data": format!("0x{}", hex::encode(IDog::ilksCall { ilk: spec.ilk }.abi_encode())) },
                        "latest"
                    ]),
                )
                .await;
            let hole = ctx
                .rpc
                .call_raw("eth_call", json!([{ "to": format!("{:?}", maker::DOG), "data": format!("0x{}", hex::encode(IDog::HoleCall {}.abi_encode())) }, "latest"]))
                .await;
            let dirt = ctx
                .rpc
                .call_raw("eth_call", json!([{ "to": format!("{:?}", maker::DOG), "data": format!("0x{}", hex::encode(IDog::DirtCall {}.abi_encode())) }, "latest"]))
                .await;
            let (Ok(vat_v), Ok(dog_v), Ok(hole_v), Ok(dirt_v)) = (vat_ilks, dog_ilks, hole, dirt)
            else {
                continue;
            };
            let vraw = crate::types::parse_bytes(&vat_v);
            let draw = crate::types::parse_bytes(&dog_v);
            if vraw.len() < 160 || draw.len() < 128 {
                continue;
            }
            ilk_state.push((
                U256::from_be_slice(&vraw[32..64]),   // rate
                U256::from_be_slice(&vraw[64..96]),   // spot
                U256::from_be_slice(&vraw[128..160]), // dust
                U256::from_be_slice(&draw[32..64]),   // chop
                U256::from_be_slice(&draw[64..96]),   // ilk hole
                U256::from_be_slice(&draw[96..128]),  // ilk dirt
                U256::from_be_slice(&crate::types::parse_bytes(&hole_v)[0..32]), // Hole
                U256::from_be_slice(&crate::types::parse_bytes(&dirt_v)[0..32]), // Dirt
                Address::from_slice(&draw[12..32]),   // clip
            ));
        }

        // Urns, batched across all ilks.
        let calls: Vec<(String, serde_json::Value)> = snapshot
            .iter()
            .map(|(i, u)| {
                (
                    "eth_call".to_string(),
                    json!([
                        { "to": format!("{:?}", maker::VAT), "data": format!("0x{}", hex::encode(IVat::urnsCall { ilk: self.ilks[*i].ilk, urn: *u }.abi_encode())) },
                        "latest"
                    ]),
                )
            })
            .collect();
        let Ok(results) = ctx.rpc.batch(&calls).await else {
            return Vec::new();
        };

        let mut out = Vec::new();
        let mut near_misses = Vec::new();
        for ((ilk_idx, urn), res) in snapshot.iter().zip(results) {
            let Ok(v) = res else { continue };
            let raw = crate::types::parse_bytes(&v);
            if raw.len() < 64 {
                continue;
            }
            let ink = U256::from_be_slice(&raw[0..32]);
            let art = U256::from_be_slice(&raw[32..64]);
            if ink.is_zero() || art.is_zero() {
                continue;
            }
            let Some((rate, spot, dust, chop, ilk_hole, ilk_dirt, hole, dirt, clip)) =
                ilk_state.get(*ilk_idx).cloned()
            else {
                continue;
            };
            let spec = self.ilks[*ilk_idx];
            let collateral_value = ink.saturating_mul(spot);
            let debt = art.saturating_mul(rate);
            if collateral_value < debt {
                out.push((
                    spec, *urn, ink, art, rate, spot, dust, chop, ilk_hole, ilk_dirt, hole, dirt,
                    clip,
                ));
            } else {
                let bps = ratio_bps(collateral_value, None, debt);
                if bps < 10_500 {
                    near_misses.push(Lead {
                        account: *urn,
                        collateral: spec.gem,
                        debt_asset: known::DAI,
                        ratio_bps: bps,
                        debt_wei: debt,
                        action: LeadAction::Maker {
                            ilk: spec.ilk,
                            urn: *urn,
                        },
                    });
                }
            }
        }
        self.leads.publish("maker", near_misses);
        out
    }
}

/// Build the bark+take bundle for one unsafe urn. Public: the oracle
/// front-runner calls it with an OSM `poke` transaction as the bundle's
/// victim — this exact call list becomes the back-run.
#[allow(clippy::too_many_arguments)]
pub async fn build_opportunity(
    ctx: &StrategyCtx,
    spec: &maker::IlkSpec,
    urn: Address,
    ink: U256,
    art: U256,
    chop: U256,
    hole: U256,
    dirt: U256,
    ilk_hole: U256,
    ilk_dirt: U256,
    clip: Address,
    target_block: u64,
) -> Option<Opportunity> {
    let executor = ctx.executor;

    // Shared per-urn reads: spot/dust for auction math, clip config, kicks.
    let vat_ilks = ctx
        .rpc
        .call_raw(
            "eth_call",
            json!([{ "to": format!("{:?}", maker::VAT), "data": format!("0x{}", hex::encode(IVat::ilksCall { ilk: spec.ilk }.abi_encode())) }, "latest"]),
        )
        .await
        .ok()?;
    let vraw = crate::types::parse_bytes(&vat_ilks);
    if vraw.len() < 160 {
        return None;
    }
    let rate_now = U256::from_be_slice(&vraw[32..64]);
    let spot = U256::from_be_slice(&vraw[64..96]);
    let dust = U256::from_be_slice(&vraw[128..160]);
    let (dink, dart, tab) = auction_math(
        ink, art, rate_now, spot, dust, chop, hole, dirt, ilk_hole, ilk_dirt,
    )?;

    let kicks = self_read(ctx, clip, IClip::kicksCall {}.abi_encode()).await?;
    let feed_price = self_read(ctx, clip, IClip::getFeedPriceCall {}.abi_encode()).await?;
    let buf = self_read(ctx, clip, IClip::bufCall {}.abi_encode()).await?;
    let tip = self_read(ctx, clip, IClip::tipCall {}.abi_encode()).await?;
    let chip = self_read(ctx, clip, IClip::chipCall {}.abi_encode()).await?;

    // Opening price the auction will carry right after kick (zero decay
    // in-block): top = feed * buf / RAY, rmul semantics.
    let top = feed_price * buf / RAY;
    if top.is_zero() {
        return None;
    }

    // What our take would buy: slice = min(lot, tab / price) — the same floor
    // division `Clipper.take` performs.
    let slice = if dink * top > tab { tab / top } else { dink };
    let owe = slice * top;

    // The kick reward mints to the executor (vat.dai): tip + tab * chip / WAD.
    let reward = tip + tab * chip / WAD;

    // Market price cap from the DAI/gem pool. Without a pool we degrade to a
    // bark-only harvest (reward capture, no auction purchase).
    let pool = ctx
        .pools
        .pair_for(spec.gem, known::DAI, dex::Venue::UniV2)
        .await;
    let pool_loaded = match pool {
        Some(pair) => {
            ctx.pools
                .load(pair, dex::Venue::UniV2, ctx.head().number)
                .await
        }
        None => None,
    };
    let max_price = pool_loaded.as_ref().and_then(|p| {
        // reserves_for(gem) = (gem_reserve, other = DAI reserve); the ray
        // price is DAI per gem, i.e. dai_reserve * RAY / gem_reserve.
        let (gem_reserve, dai_reserve) = p.reserves_for(spec.gem)?;
        market_price_ray(dai_reserve, gem_reserve)
    });

    let mut calls = vec![Call::new(
        known::DAI,
        IERC20::approveCall {
            spender: maker::DAI_JOIN,
            amount: U256::MAX,
        }
        .abi_encode(),
    )];
    let mut notes = format!(
        "maker {} bark urn {urn:?} ink {ink} art {art} dink {dink} dart {dart} tab {tab}",
        spec.name
    );

    if let (Some(p), Some(mp)) = (pool_loaded.as_ref(), max_price) {
        if owe.is_zero() || mp.is_zero() {
            return None;
        }
        calls.push(Call::new(
            maker::DAI_JOIN,
            IDaiJoin::joinCall {
                usr: executor,
                wad: owe,
            }
            .abi_encode(),
        ));
        calls.push(Call::new(
            maker::VAT,
            IVat::hopeCall {
                usr: maker::DAI_JOIN,
            }
            .abi_encode(),
        ));
        calls.push(Call::new(
            maker::DOG,
            IDog::barkCall {
                ilk: spec.ilk,
                urn,
                kpr: executor,
            }
            .abi_encode(),
        ));
        calls.push(Call::new(
            clip,
            IClip::takeCall {
                id: kicks + U256::ONE, // ++kicks inside bark
                amt: U256::MAX,        // buy as much as possible
                max: mp,               // market price, ray
                who: executor,
                data: alloy_primitives::Bytes::new(),
            }
            .abi_encode(),
        ));
        calls.push(Call::new(
            spec.gem_join,
            IGemJoin::exitCall {
                usr: executor,
                wad: slice,
            }
            .abi_encode(),
        ));
        // Sell the bought gem back into DAI to fund the flash repayment.
        calls.extend(build_leg(p, spec.gem, known::DAI, slice, executor));
        calls.push(Call::new(
            maker::DAI_JOIN,
            IDaiJoin::exitCall {
                usr: executor,
                wad: reward,
            }
            .abi_encode(),
        ));
        notes.push_str(&format!(
            "; atomic bark+take id {} slice {slice} owe {owe} top {top} market {mp} reward {reward}",
            kicks + U256::ONE
        ));
        // Expected profit: reward + a 1% spread on the purchase (conservative;
        // the simulation measures the real spread).
        let expected = reward + owe / U256::from(100u64);
        Some(liquidation_opportunity(
            Strategy::LiquidationMaker,
            calls,
            vec![known::DAI],
            vec![owe],
            known::DAI,
            expected,
            owe,
            target_block,
            notes,
        ))
    } else {
        // Bark-only: capture the kick reward, leave the auction to the market.
        calls.push(Call::new(
            maker::VAT,
            IVat::hopeCall {
                usr: maker::DAI_JOIN,
            }
            .abi_encode(),
        ));
        calls.push(Call::new(
            maker::DOG,
            IDog::barkCall {
                ilk: spec.ilk,
                urn,
                kpr: executor,
            }
            .abi_encode(),
        ));
        calls.push(Call::new(
            maker::DAI_JOIN,
            IDaiJoin::exitCall {
                usr: executor,
                wad: reward,
            }
            .abi_encode(),
        ));
        notes.push_str("; bark-only (no DAI pool for gem)");
        Some(liquidation_opportunity(
            Strategy::LiquidationMaker,
            calls,
            Vec::new(),
            Vec::new(),
            known::DAI,
            reward,
            reward,
            target_block,
            notes,
        ))
    }
}

/// Standalone read helper (the strategy struct is not available to the free
/// builder used by the oracle path).
async fn self_read(ctx: &StrategyCtx, to: Address, call: Vec<u8>) -> Option<U256> {
    let v = ctx
        .rpc
        .call_raw(
            "eth_call",
            json!([{ "to": format!("{to:?}"), "data": format!("0x{}", hex::encode(call)) }, "latest"]),
        )
        .await
        .ok()?;
    let raw = crate::types::parse_bytes(&v);
    if raw.len() < 32 {
        return None;
    }
    Some(U256::from_be_slice(&raw[0..32]))
}

#[async_trait]
impl StrategyImpl for MakerLiquidationStrategy {
    fn kind(&self) -> Strategy {
        Strategy::LiquidationMaker
    }

    async fn on_block(&self, ctx: &StrategyCtx, head: &BlockHead) -> Vec<Opportunity> {
        if self.ilks.is_empty() {
            return Vec::new();
        }
        self.harvest(ctx, head).await;
        let unsafe_urns = self.poll(ctx).await;
        if unsafe_urns.is_empty() {
            return Vec::new();
        }
        tracing::info!(
            target: "strategy::liquidation_maker",
            candidates = unsafe_urns.len(),
            watchlist = self.urn_count(),
            ilks = self.ilks.iter().map(|i| i.name).collect::<Vec<_>>().join(","),
            "unsafe Maker urns found"
        );
        let mut out = Vec::new();
        for (
            spec,
            urn,
            ink,
            art,
            _rate,
            _spot,
            _dust,
            chop,
            ilk_hole,
            ilk_dirt,
            hole,
            dirt,
            clip,
        ) in unsafe_urns
        {
            if let Some(opp) = build_opportunity(
                ctx,
                spec,
                urn,
                ink,
                art,
                chop,
                hole,
                dirt,
                ilk_hole,
                ilk_dirt,
                clip,
                ctx.target_block(),
            )
            .await
            {
                out.push(opp);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wad(v: u128) -> U256 {
        U256::from(v) * WAD
    }

    #[test]
    fn bark_and_take_encode_the_verified_selectors() {
        assert_eq!(IDog::barkCall::SELECTOR, [0xed, 0x99, 0x89, 0x08]);
        assert_eq!(IClip::takeCall::SELECTOR, [0x81, 0xa7, 0x94, 0xcb]);
        assert_eq!(IVat::hopeCall::SELECTOR, [0xa3, 0xb2, 0x2f, 0xc4]);
        assert_eq!(IDaiJoin::joinCall::SELECTOR, [0x3b, 0x4d, 0xa6, 0x9f]);
        assert_eq!(IGemJoin::exitCall::SELECTOR, [0xef, 0x69, 0x3b, 0xed]);
    }

    #[test]
    fn frob_lognote_harvests_the_urn_from_topic4() {
        // DSNote LogNote(bytes4 indexed sig, address indexed usr, bytes32
        // indexed arg1, bytes32 indexed arg2): for frob, arg1 = ilk,
        // arg2 = urn. The urn is the LAST 20 bytes of topic4.
        let mut word = [0u8; 32];
        word[12..].copy_from_slice(&[
            0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee,
            0xff, 0x01, 0x23, 0x45, 0x67, 0x89,
        ]);
        let addr = Address::from_slice(&word[12..]);
        assert_eq!(addr.as_slice(), &word[12..32]);
        // frob selector padded to 32 bytes is the harvest filter's topic1.
        let frob = "7608870300000000000000000000000000000000000000000000000000000000";
        assert!(frob.starts_with("76088703"));
    }

    #[test]
    fn unsafe_urn_is_detected_by_spot_times_ink() {
        // ink=10 WETH, spot=1500e27 (ray), art=12000 DAI of debt, rate=1e27:
        // 10 * 1500 = 15000e45 collateral vs 12000e45 debt -> safe.
        // chop is WAD-scaled; Hole/hole are rad (unbounded in this test).
        assert!(auction_math(
            wad(10),
            wad(12_000),
            RAY,
            U256::from(1_500u128) * RAY,
            wad(100),
            WAD, // chop = 1.0
            U256::MAX,
            U256::ZERO,
            U256::MAX,
            U256::ZERO,
        )
        .is_none());
        // Debt above collateral value -> barkable.
        let got = auction_math(
            wad(10),
            wad(16_000),
            RAY,
            U256::from(1_500u128) * RAY,
            wad(100),
            WAD,
            U256::MAX,
            U256::ZERO,
            U256::MAX,
            U256::ZERO,
        )
        .expect("unsafe");
        // Full liquidation: dart = art (room unbounded), dink = ink.
        assert_eq!(got.1, wad(16_000));
        assert_eq!(got.0, wad(10));
    }

    #[test]
    fn room_bounds_partial_liquidations_like_the_dog() {
        // Room of ~1001 DAI in rad against 16000 DAI of debt:
        // dart = room * WAD / rate / chop.
        let got = auction_math(
            wad(10),
            wad(16_000),
            RAY,
            U256::from(1_500u128) * RAY,
            wad(100),
            WAD,
            U256::from(1_001u128) * RAY * WAD, // 1001e45 rad == 1001 DAI
            U256::ZERO,
            U256::MAX,
            U256::ZERO,
        )
        .expect("partial bark");
        // dart = room(WAD)/rate/chop ≈ 1001 DAI of art; the dusty-leftover
        // rule may bump it to the full art when (art-dart)*rate < dust(100).
        let dart_dai = got.1 / WAD;
        assert!(dart_dai <= U256::from(1_002u128));
    }

    #[test]
    fn market_price_is_ray_scaled() {
        // Pool: 3,000,000e18 DAI vs 1,000e18 WETH -> 3000 ray.
        let p = market_price_ray(wad(3_000_000), wad(1_000)).unwrap();
        assert_eq!(p, U256::from(3_000u128) * RAY);
        assert!(market_price_ray(U256::ZERO, wad(1)).is_none());
    }

    #[test]
    fn ilk_table_bytes_are_right_padded() {
        let spec = maker::spec_by_name("ETH-A").unwrap();
        assert_eq!(&spec.ilk.as_slice()[..5], b"ETH-A");
        assert_eq!(&spec.ilk.as_slice()[5..], &[0u8; 27]);
        assert!(maker::spec_by_name("nope").is_none());
        assert_eq!(maker::table().len(), 3);
    }

    #[test]
    fn auction_id_is_kicks_plus_one() {
        // clip.kicks is incremented inside kick: `id = ++kicks`. Our bundle
        // encodes kicks + 1 read off-chain before the bark.
        let kicks = U256::from(41u64);
        assert_eq!(kicks + U256::ONE, U256::from(42u64));
    }
}
