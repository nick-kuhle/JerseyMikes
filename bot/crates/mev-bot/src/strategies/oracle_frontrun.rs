//! Oracle-update front-running: back-run the price change with liquidations.
//!
//! Most liquidation strategies watch *state* (health factors) and react a
//! block late. The real edge is watching the *event that changes state*: when
//! a lending protocol's collateral price feed updates downward, every
//! position priced at the stale higher value can flip underwater the moment
//! the update lands — and the first searcher behind that transaction in the
//! block captures the liquidation.
//!
//! This strategy sits on the pending path and classifies mempool transactions
//! against a watched set of oracle write paths:
//!
//! | source | transaction shape | what it moves |
//! | --- | --- | --- |
//! | Chainlink OCR2 | `transmit(bytes)` to the **aggregator** (resolved from the proxy at boot, refreshed periodically — the proxy address is what protocols read, the aggregator is what the update hits) | every Aave / Compound reserve priced off that feed |
//! | Chainlink legacy | `submit(uint256,int256,uint256,uint256,address)` | same |
//! | Maker OSM | `poke()` to the pip, `poke(bytes32)` via OsmMom | the ilk's spot price an hour later |
//! | Maker Spot | `poke(bytes32)` | all ilk spot values now |
//!
//! On a match we look up near-miss positions for the affected collateral in
//! the shared [`LiquidationLeads`] registry (published by the health-polling
//! strategies each block), rebuild each liquidation with the owning
//! strategy's own builder, and emit it as a **back-run bundle**: the oracle
//! transaction is the victim, our calls run directly behind it. The fork
//! simulation replays victim → back and decides whether the position really
//! flipped; if the update was upward or too small, the liquidation call
//! reverts, the bundle dies, and — private orderflow — nothing is broadcast.
//!
//! **Why Chainlink updates are winnable at all.** A feed update is a private
//! `transmit` from the OCR transmitter, but it lands in the *public* mempool
//! unless routed through Flashbots Protect; when it is public, anyone can
//! bundle behind it. Downward ETH moves are exactly when searchers compete
//! hardest here; being simulation-only, we measure how often the pattern
//! even exists before caring about winning it.
//!
//! **Honesty.** We do not decode the new price out of the OCR report (the
//! bytes are offchain-consensus encoded; the shapes drift): we emit for
//! every in-band near-miss and let the simulation filter. That costs a
//! simulation per lead per update, bounded by `ORACLE_FRONTRUN_MAX_LEADS`
//! (default 3). Medianizer `poke`s (the Maker price source itself, an hour
//! ahead of the OSM) are watched only if their addresses are added to the
//! feed list.
//!
//! **Not yet.** Chainlink updates submitted through aggregators we have not
//! resolved (aggregate->proposed round transitions), Redstone/Api3 oracle
//! families, L2 sequencer feed oracle graces.

use alloy_primitives::{Address, U256};
use alloy_sol_types::{sol, SolCall};
use async_trait::async_trait;
use parking_lot::RwLock;
use std::collections::HashMap;

use crate::config::known;
use crate::strategies::leads::LiquidationLeads;
use crate::strategies::liquidation::{build_opportunity as build_aave, compose as compose_aave};
use crate::strategies::liquidation_maker as maker;
use crate::strategies::liquidation_morpho as morpho;
use crate::strategies::{StrategyCtx, StrategyImpl};
use crate::types::{now_ms, BlockHead, Opportunity, PendingTx, Strategy};

sol! {
    interface IChainlinkProxy {
        function aggregator() external view returns (address);
    }
}

/// `transmit(bytes)` — Chainlink OCR2.
const TRANSMIT_SELECTOR: [u8; 4] = [0x6b, 0x0b, 0xac, 0x97];
/// `submit(uint256,int256,uint256,uint256,address)` — Chainlink OCR1/Flux.
const SUBMIT_SELECTOR: [u8; 4] = [0xc5, 0x2f, 0xd0, 0x19];
/// `poke()` — Maker OSM.
const OSM_POKE_SELECTOR: [u8; 4] = [0x18, 0x17, 0x83, 0x58];
/// `poke(bytes32)` — Maker Spot ter and OsmMom.
const SPOT_POKE_SELECTOR: [u8; 4] = [0x15, 0x04, 0x46, 0x0f];

/// A watched price source and the collateral it reprices.
#[derive(Clone, Copy, Debug)]
struct Feed {
    /// Address whose transactions we classify (aggregator or OSM).
    target: Address,
    /// Collateral token the feed prices.
    collateral: Address,
    kind: FeedKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FeedKind {
    ChainlinkAggregator,
    MakerOsm,
    MakerSpot,
}

pub struct OracleFrontrunStrategy {
    /// Proxy → (collateral, resolved aggregator), aggregator resolved lazily.
    chainlink_proxies: RwLock<HashMap<Address, (Address, Option<Address>)>>,
    /// Fully resolved watch set (target address → feed).
    watched: RwLock<HashMap<Address, Feed>>,
    /// Block at which the aggregator resolution was last refreshed.
    resolved_at_block: RwLock<u64>,
    max_leads: usize,
    leads: LiquidationLeads,
    /// Aave reserve cache for trigger-time position composition.
    cache_aave: crate::strategies::liquidation::AaveCache,
}

impl OracleFrontrunStrategy {
    /// `watch_feeds`: (Chainlink proxy, collateral token) pairs — the proxy
    /// is resolved to its live aggregator at runtime.
    pub fn new(
        watch_feeds: Vec<(Address, Address)>,
        max_leads: usize,
        leads: LiquidationLeads,
    ) -> Self {
        let mut proxies = HashMap::new();
        for (proxy, collateral) in watch_feeds {
            proxies.insert(proxy, (collateral, None));
        }
        Self {
            chainlink_proxies: RwLock::new(proxies),
            watched: RwLock::new(HashMap::new()),
            resolved_at_block: RwLock::new(0),
            max_leads: max_leads.max(1),
            leads,
            cache_aave: crate::strategies::liquidation::AaveCache::default(),
        }
    }

    pub fn watched_count(&self) -> usize {
        self.watched.read().len()
    }

    /// Resolve proxy → aggregator (Chainlink) and build the static Maker
    /// entries. Cheap (one `aggregator()` per proxy, every ~50 blocks) —
    /// aggregators are upgraded rarely, but when they are, a stale address
    /// would silently stop seeing updates.
    async fn refresh_watch_set(&self, ctx: &StrategyCtx, head: &BlockHead) {
        {
            let last = *self.resolved_at_block.read();
            if !self.watched.read().is_empty() && head.number.saturating_sub(last) < 50 {
                return;
            }
        }
        let proxies = self.chainlink_proxies.read().clone();
        let mut resolved = HashMap::new();
        for (proxy, (collateral, _)) in proxies {
            let agg = match ctx
                .rpc
                .call_raw(
                    "eth_call",
                    serde_json::json!([
                        { "to": format!("{proxy:?}"), "data": format!("0x{}", hex::encode(IChainlinkProxy::aggregatorCall {}.abi_encode())) },
                        "latest"
                    ]),
                )
                .await
            {
                Ok(v) => {
                    let raw = crate::types::parse_bytes(&v);
                    if raw.len() >= 32 && raw[12..32] != [0u8; 20] {
                        Some(Address::from_slice(&raw[12..32]))
                    } else {
                        None
                    }
                }
                Err(_) => None,
            };
            if let Some(a) = agg {
                resolved.insert(
                    a,
                    Feed {
                        target: a,
                        collateral,
                        kind: FeedKind::ChainlinkAggregator,
                    },
                );
            }
            self.chainlink_proxies
                .write()
                .insert(proxy, (collateral, agg));
        }

        // Maker: every ilk's OSM pip, plus the OsmMom and Spot ter, which
        // poke pips in one transaction.
        for spec in maker::maker::table() {
            resolved.insert(
                spec.pip,
                Feed {
                    target: spec.pip,
                    collateral: spec.gem,
                    kind: FeedKind::MakerOsm,
                },
            );
        }
        if let Some(mom) = maker_osm_mom() {
            resolved.insert(
                mom,
                Feed {
                    target: mom,
                    collateral: Address::ZERO,
                    kind: FeedKind::MakerSpot,
                },
            );
        }
        resolved.insert(
            maker::maker::SPOT,
            Feed {
                target: maker::maker::SPOT,
                collateral: Address::ZERO,
                kind: FeedKind::MakerSpot,
            },
        );

        *self.watched.write() = resolved;
        *self.resolved_at_block.write() = head.number;
    }

    /// Classify a pending transaction: which feed does it update, and which
    /// collateral does that reprice? `None` for everything else.
    fn classify(&self, tx: &PendingTx) -> Option<Feed> {
        let to = tx.to?;
        let feed = *self.watched.read().get(&to)?;
        let sel = tx.selector()?;
        match feed.kind {
            FeedKind::ChainlinkAggregator => {
                if sel == TRANSMIT_SELECTOR || sel == SUBMIT_SELECTOR {
                    Some(feed)
                } else {
                    None
                }
            }
            FeedKind::MakerOsm => {
                (sel == OSM_POKE_SELECTOR || sel == SPOT_POKE_SELECTOR).then_some(feed)
            }
            FeedKind::MakerSpot => (sel == SPOT_POKE_SELECTOR).then_some(feed),
        }
    }

    /// Rebuild the liquidation for one lead using the owning strategy's
    /// builder, so protocol logic lives in exactly one place.
    async fn rebuild(
        &self,
        ctx: &StrategyCtx,
        lead: &crate::strategies::leads::Lead,
    ) -> Option<Opportunity> {
        use crate::strategies::leads::LeadAction;
        match &lead.action {
            LeadAction::AaveV3 { user } => {
                // Re-read the composition at trigger time — the position may
                // have moved since the lead was published; a healthy
                // position reverts and the bundle dies honestly.
                let hf_one = U256::from(1_000_000_000_000_000_000u128);
                let health =
                    U256::from(lead.ratio_bps as u128) * U256::from(1_000_000_000_000_000u64);
                let pos = compose_aave(ctx, &self.cache_aave, *user, health.max(hf_one)).await?;
                build_aave(ctx, &pos).await
            }
            LeadAction::Morpho {
                market_id,
                loan_token,
                collateral_token,
                oracle,
                irm,
                lltv,
                borrower,
                borrow_shares,
                total_borrow_assets,
                total_borrow_shares,
            } => {
                let params = morpho::IMorpho::MarketParams {
                    loanToken: *loan_token,
                    collateralToken: *collateral_token,
                    oracle: *oracle,
                    irm: *irm,
                    lltv: *lltv,
                };
                let price = morpho::oracle_price(ctx, *oracle).await?;
                morpho::build_opportunity(
                    ctx,
                    &params,
                    *market_id,
                    *borrower,
                    *borrow_shares,
                    (*total_borrow_assets, *total_borrow_shares),
                    price,
                    ctx.target_block(),
                )
                .await
            }
            LeadAction::Maker { ilk, urn } => {
                let spec = maker::maker::table().iter().find(|s| s.ilk == *ilk)?;
                // Re-read the urn and dog state now: leads carry ratios, not
                // the full sizing inputs.
                let (ink, art, chop, hole, dirt, ilk_hole, ilk_dirt, clip) =
                    read_maker_inputs(ctx, spec, *urn).await?;
                maker::build_opportunity(
                    ctx,
                    spec,
                    *urn,
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
            }
        }
    }
}

/// Maker sizing inputs for the oracle path (the block path carries them in
/// its own poll).
async fn read_maker_inputs(
    ctx: &StrategyCtx,
    spec: &maker::maker::IlkSpec,
    urn: Address,
) -> Option<(U256, U256, U256, U256, U256, U256, U256, Address)> {
    use alloy_sol_types::SolCall as _;
    let read = |to: Address, data: Vec<u8>| async move {
        let v = ctx
            .rpc
            .call_raw(
                "eth_call",
                serde_json::json!([{ "to": format!("{to:?}"), "data": format!("0x{}", hex::encode(data)) }, "latest"]),
            )
            .await
            .ok()?;
        Some(crate::types::parse_bytes(&v))
    };
    let urns = read(
        maker::maker::VAT,
        maker::IVat::urnsCall { ilk: spec.ilk, urn }.abi_encode(),
    )
    .await?;
    let dog_ilks = read(
        maker::maker::DOG,
        maker::IDog::ilksCall { ilk: spec.ilk }.abi_encode(),
    )
    .await?;
    if urns.len() < 64 || dog_ilks.len() < 128 {
        return None;
    }
    let ink = U256::from_be_slice(&urns[0..32]);
    let art = U256::from_be_slice(&urns[32..64]);
    let clip = Address::from_slice(&dog_ilks[12..32]);
    let chop = U256::from_be_slice(&dog_ilks[32..64]);
    let ilk_hole = U256::from_be_slice(&dog_ilks[64..96]);
    let ilk_dirt = U256::from_be_slice(&dog_ilks[96..128]);
    let hole = read(maker::maker::DOG, maker::IDog::HoleCall {}.abi_encode()).await?;
    let dirt = read(maker::maker::DOG, maker::IDog::DirtCall {}.abi_encode()).await?;
    if hole.len() < 32 || dirt.len() < 32 {
        return None;
    }
    Some((
        ink,
        art,
        chop,
        U256::from_be_slice(&hole[0..32]),
        U256::from_be_slice(&dirt[0..32]),
        ilk_hole,
        ilk_dirt,
        clip,
    ))
}

/// OsmMom address (chainlog `OSM_MOM`). Separate so a missing entry degrades
/// to "not watched" rather than breaking the watch set build.
fn maker_osm_mom() -> Option<Address> {
    Some(alloy_primitives::address!(
        "76416A4d5190d071BFed309861527431304Aa14f"
    ))
}

#[async_trait]
impl StrategyImpl for OracleFrontrunStrategy {
    fn kind(&self) -> Strategy {
        Strategy::OracleFrontrun
    }

    async fn on_block(&self, ctx: &StrategyCtx, head: &BlockHead) -> Vec<Opportunity> {
        self.refresh_watch_set(ctx, head).await;
        Vec::new()
    }

    async fn on_pending(&self, ctx: &StrategyCtx, tx: &PendingTx) -> Vec<Opportunity> {
        if self.watched.read().is_empty() {
            return Vec::new();
        }
        let Some(feed) = self.classify(tx) else {
            return Vec::new();
        };
        // Spot pokes reprice every ilk at once; OSM/Chainlink pokes reprice
        // one collateral.
        let affected: Vec<Address> = if feed.collateral == Address::ZERO {
            known::collateral_universe().to_vec()
        } else {
            vec![feed.collateral]
        };
        let mut leads = Vec::new();
        for asset in affected {
            leads.extend(self.leads.near_misses_for(asset, self.max_leads));
        }
        if leads.is_empty() {
            tracing::debug!(
                target: "strategy::oracle_frontrun",
                tx = ?tx.hash,
                "oracle update seen but no near-miss leads match"
            );
            return Vec::new();
        }
        tracing::info!(
            target: "strategy::oracle_frontrun",
            tx = ?tx.hash,
            leads = leads.len(),
            kind = ?feed.kind,
            "oracle update classified — building back-run liquidations"
        );

        let mut out = Vec::new();
        for lead in leads {
            let protocol = lead.action.protocol();
            let Some(mut opp) = self.rebuild(ctx, &lead).await else {
                continue;
            };
            // Back-run shape: nothing in front, everything behind the oracle tx.
            opp.strategy = Strategy::OracleFrontrun;
            opp.victim_hashes = vec![tx.hash];
            opp.back_calls = std::mem::take(&mut opp.front_calls);
            opp.created_at_ms = now_ms();
            opp.notes = format!(
                "oracle-update back-run of {:?} via {:?} ({} near-miss, ratio_bps {}); {}",
                tx.hash, feed.target, protocol, lead.ratio_bps, opp.notes
            );
            out.push(opp);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::strategies::leads::{NEAR_MISS_MAX_BPS, NEAR_MISS_MIN_BPS};
    use crate::types::TxSource;

    fn tx_to(to: Address, selector: [u8; 4]) -> PendingTx {
        PendingTx {
            hash: alloy_primitives::B256::ZERO,
            from: None,
            to: Some(to),
            value: U256::ZERO,
            gas: 300_000,
            max_fee_per_gas: U256::ZERO,
            max_priority_fee_per_gas: U256::ZERO,
            nonce: 0,
            input: selector.to_vec(),
            raw: None,
            source: TxSource::PublicMempool,
            mined_at: None,
            seen_at_ms: now_ms(),
        }
    }

    fn strategy_with(feeds: Vec<(Address, Address)>) -> OracleFrontrunStrategy {
        let s = OracleFrontrunStrategy::new(feeds.clone(), 3, LiquidationLeads::new());
        // Pre-seed the watch set the way refresh_watch_set would after
        // resolving the aggregator, plus the Maker statics.
        let mut watched = HashMap::new();
        for (agg, collat) in feeds {
            watched.insert(
                agg,
                Feed {
                    target: agg,
                    collateral: collat,
                    kind: FeedKind::ChainlinkAggregator,
                },
            );
        }
        for spec in maker::maker::table() {
            watched.insert(
                spec.pip,
                Feed {
                    target: spec.pip,
                    collateral: spec.gem,
                    kind: FeedKind::MakerOsm,
                },
            );
        }
        watched.insert(
            maker::maker::SPOT,
            Feed {
                target: maker::maker::SPOT,
                collateral: Address::ZERO,
                kind: FeedKind::MakerSpot,
            },
        );
        *s.watched.write() = watched;
        s
    }

    #[test]
    fn classifies_chainlink_transmit_to_a_watched_aggregator() {
        let agg = Address::with_last_byte(7);
        let s = strategy_with(vec![(agg, known::WETH)]);
        let feed = s
            .classify(&tx_to(agg, TRANSMIT_SELECTOR))
            .expect("transmit is an update");
        assert_eq!(feed.collateral, known::WETH);
        assert_eq!(feed.kind, FeedKind::ChainlinkAggregator);
        // Legacy submit also counts.
        assert!(s.classify(&tx_to(agg, SUBMIT_SELECTOR)).is_some());
        // Any other selector on the aggregator is not an update.
        assert!(s.classify(&tx_to(agg, [0xde, 0xad, 0xbe, 0xef])).is_none());
        // Unknown target is ignored.
        assert!(s
            .classify(&tx_to(Address::with_last_byte(9), TRANSMIT_SELECTOR))
            .is_none());
    }

    #[test]
    fn classifies_maker_osm_and_spot_pokes() {
        let s = strategy_with(vec![]);
        let eth = maker::maker::spec_by_name("ETH-A").unwrap();
        let feed = s
            .classify(&tx_to(eth.pip, OSM_POKE_SELECTOR))
            .expect("osm poke");
        assert_eq!(feed.collateral, known::WETH);
        assert_eq!(feed.kind, FeedKind::MakerOsm);
        let spot_feed = s
            .classify(&tx_to(maker::maker::SPOT, SPOT_POKE_SELECTOR))
            .expect("spot poke");
        assert_eq!(spot_feed.kind, FeedKind::MakerSpot);
        // OSM poke selector on the Spot ter is not a thing.
        assert!(s
            .classify(&tx_to(maker::maker::SPOT, OSM_POKE_SELECTOR))
            .is_none());
    }

    #[test]
    fn near_miss_band_is_narrow_by_design() {
        assert_eq!(NEAR_MISS_MIN_BPS, 10_000);
        assert_eq!(NEAR_MISS_MAX_BPS, 10_500);
    }
}
