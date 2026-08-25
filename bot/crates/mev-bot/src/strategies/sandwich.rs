//! Sandwich strategy.
//!
//! Looks for a router swap in the mempool that moves a constant-product pool
//! enough for a front-run/back-run pair to be profitable *after* gas, sizes the
//! front-run optimally, and emits the two call batches.
//!
//! Two properties matter and both are enforced here:
//!   1. we never sandwich a victim whose slippage bound would be violated —
//!      their revert would leave us holding the inventory,
//!   2. the sizing is capped by `max_position_wei`, and by the pool's depth.

use alloy_primitives::{Address, U256};
use alloy_sol_types::SolCall;
use async_trait::async_trait;

use crate::dex;
use crate::strategies::{decode_router, StrategyCtx, StrategyImpl};
use crate::types::{now_ms, Call, Opportunity, PendingTx, Strategy};

pub struct SandwichStrategy;

#[async_trait]
impl StrategyImpl for SandwichStrategy {
    fn kind(&self) -> Strategy {
        Strategy::Sandwich
    }

    async fn on_pending(&self, ctx: &StrategyCtx, tx: &PendingTx) -> Vec<Opportunity> {
        // A sandwich is a front-run. Unsigned / sequencer / Flashblock
        // sources cannot be placed in front of — refuse here so a toggle
        // left on on Base cannot emit an undeliverable candidate.
        if tx.source.backrun_only() {
            return Vec::new();
        }
        let weth = ctx.cfg.chain.weth;
        let Some(intent) = decode_router(
            tx,
            weth,
            ctx.cfg.decode_universal_router,
            ctx.cfg.addresses.universal_router,
        ) else {
            return Vec::new();
        };
        // Only single-hop paths for now: multi-hop sandwiches need the whole
        // path simulated, which the V3/aggregator work will bring.
        if intent.path.len() != 2 {
            return Vec::new();
        }
        // We front-run with WETH inventory, so the victim must be buying with WETH.
        if intent.token_in != weth {
            return Vec::new();
        }

        let head = ctx.head();
        // Live flow prices against the head; a replayed transaction prices
        // against the parent of the block it landed in, which is the state it
        // actually executed against.
        let state_block = tx.state_block(&head);
        let base_fee = tx.base_fee(&head);
        let target_block = tx.target_block(&head, ctx.cfg.sim.target_block_offset);

        let mut out = Vec::new();
        // Every V2 venue the chain's registry knows about (two on mainnet,
        // one on Base).
        for (venue, _) in ctx.cfg.addresses.pair_factories() {
            let Some(pair) = ctx
                .pools
                .pair_for(intent.token_in, intent.token_out, venue)
                .await
            else {
                continue;
            };
            let Some(pool) = ctx.pool_at(pair, venue, state_block).await else {
                continue;
            };
            let Some(sizing) = dex::optimal_sandwich_in(
                &pool,
                intent.token_in,
                intent.amount_in,
                intent.min_out,
                ctx.max_position(),
            ) else {
                continue;
            };

            // Rough gas model: front leg ~140k, back leg ~130k.
            let gas_estimate = U256::from(270_000u64);
            let gas_cost = gas_estimate * (base_fee + U256::from(1_000_000_000u64));
            if sizing.gross_profit <= gas_cost {
                tracing::trace!(
                    target: "strategy::sandwich",
                    gross = %sizing.gross_profit,
                    gas = %gas_cost,
                    "skipping: gas eats the edge"
                );
                continue;
            }

            let front = build_leg(
                &pool,
                intent.token_in,
                intent.token_out,
                sizing.amount_in,
                ctx.executor,
            );
            let back = build_leg(
                &pool,
                intent.token_out,
                intent.token_in,
                sizing.front_out,
                ctx.executor,
            );

            out.push(Opportunity {
                id: uuid::Uuid::new_v4().to_string(),
                strategy: Strategy::Sandwich,
                victim_hashes: vec![tx.hash],
                front_calls: front,
                back_calls: back,
                flash_tokens: Vec::new(),
                flash_amounts: Vec::new(),
                profit_token: weth,
                expected_profit_wei: sizing.gross_profit.saturating_sub(gas_cost),
                notional_wei: sizing.amount_in,
                target_block,
                created_at_ms: now_ms(),
                notes: format!(
                    "sandwich {} on {} pair {:?}: victim in {} min_out {} -> front {} back {} gross {}",
                    intent.token_out,
                    venue.as_str(),
                    pool.address,
                    intent.amount_in,
                    intent.min_out,
                    sizing.amount_in,
                    sizing.back_out,
                    sizing.gross_profit
                ),
            provenance: Default::default(),
        });
        }
        out
    }
}

/// A raw V2 swap leg: move the input token into the pair, then call `swap`.
///
/// Going straight to the pair (instead of through the router) saves ~40k gas
/// and removes the router's deadline/slippage checks, which we do not need
/// because the executor enforces profit atomically.
pub fn build_leg(
    pool: &dex::V2Pool,
    token_in: Address,
    token_out: Address,
    amount_in: U256,
    recipient: Address,
) -> Vec<Call> {
    let amount_out = pool.amount_out(token_in, amount_in).unwrap_or(U256::ZERO);
    let zero_for_one = token_in == pool.token0;
    let (amount0_out, amount1_out) = if zero_for_one {
        (U256::ZERO, amount_out)
    } else {
        (amount_out, U256::ZERO)
    };
    let _ = token_out;

    vec![
        Call::new(
            token_in,
            dex::IERC20::transferCall {
                to: pool.address,
                amount: amount_in,
            }
            .abi_encode(),
        ),
        Call::new(
            pool.address,
            dex::IUniswapV2Pair::swapCall {
                amount0Out: amount0_out,
                amount1Out: amount1_out,
                to: recipient,
                data: alloy_primitives::Bytes::new(),
            }
            .abi_encode(),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dex::V2Pool;
    use crate::dex::Venue;

    fn pool() -> V2Pool {
        V2Pool {
            address: Address::with_last_byte(0xaa),
            token0: Address::with_last_byte(1),
            token1: Address::with_last_byte(2),
            reserve0: U256::from(1_000_000u128) * U256::from(10u128.pow(18)),
            reserve1: U256::from(1_000_000u128) * U256::from(10u128.pow(18)),
            fee_bps: 30,
            venue: Venue::UniV2,
            block: 1,
        }
    }

    #[test]
    fn leg_transfers_then_swaps() {
        let p = pool();
        let calls = build_leg(
            &p,
            p.token0,
            p.token1,
            U256::from(1000u64),
            Address::with_last_byte(9),
        );
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].target, p.token0);
        assert_eq!(calls[1].target, p.address);
        assert_eq!(
            &calls[1].data[..4],
            &dex::IUniswapV2Pair::swapCall::SELECTOR
        );
    }

    #[test]
    fn leg_sets_the_correct_output_side() {
        let p = pool();
        let calls = build_leg(&p, p.token0, p.token1, U256::from(1000u64), Address::ZERO);
        let decoded = dex::IUniswapV2Pair::swapCall::abi_decode(&calls[1].data, true).unwrap();
        // Selling token0 must request token1 out.
        assert_eq!(decoded.amount0Out, U256::ZERO);
        assert!(decoded.amount1Out > U256::ZERO);
    }
}
