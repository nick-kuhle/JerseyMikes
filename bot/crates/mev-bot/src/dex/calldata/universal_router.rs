//! Uniswap UniversalRouter `execute` decoder.
//!
//! Scope is the single mainnet router at [`known::UNIVERSAL_ROUTER`]. We walk
//! the command byte string and the parallel `inputs` array and emit a swap
//! intent for the first `V3_SWAP_EXACT_IN` or `V2_SWAP_EXACT_IN` we find.
//! Everything else (permits, sweeps, V4, 1inch, 0x) is ignored.
//!
//! Command bytes may have the high bit set (`ALLOW_REVERT = 0x80`); the
//! command id is the low 6 bits.

use alloy_primitives::{Address, Bytes, U256};
use alloy_sol_types::{sol, SolCall, SolValue};

use crate::config::known;

// Two overloads of `execute` must live in *separate* `sol!` blocks.
// In one block alloy generates a single `executeCall` type and the last
// definition wins, so both `IUniversalRouter::executeCall::SELECTOR` and
// `IUniversalRouterWithDeadline::executeCall::SELECTOR` become the 3-arg
// selector `0x24856bc3`. Then every 2-arg `execute(bytes,bytes[])` on
// mainnet (`0x3593564c`) is invisible. MAINTAINING.md §5.
sol! {
    interface IUniversalRouter {
        function execute(bytes calldata commands, bytes[] calldata inputs) external payable;
    }
}

sol! {
    interface IUniversalRouterWithDeadline {
        function execute(bytes calldata commands, bytes[] calldata inputs, uint256 deadline) external payable;
    }
}

/// `execute(bytes,bytes[])` — published UniversalRouter selector.
pub const SEL_EXECUTE: [u8; 4] = [0x35, 0x93, 0x56, 0x4c];
/// `execute(bytes,bytes[],uint256)` — published UniversalRouter selector.
pub const SEL_EXECUTE_DEADLINE: [u8; 4] = [0x24, 0x85, 0x6b, 0xc3];

/// `V3_SWAP_EXACT_IN`
pub const CMD_V3_SWAP_EXACT_IN: u8 = 0x00;
/// `V2_SWAP_EXACT_IN`
pub const CMD_V2_SWAP_EXACT_IN: u8 = 0x08;
/// `WRAP_ETH` — often precedes a swap that spends `CONTRACT_BALANCE`.
pub const CMD_WRAP_ETH: u8 = 0x0b;

/// Low 6 bits are the command; the high bit is `ALLOW_REVERT`.
const COMMAND_TYPE_MASK: u8 = 0x3f;

/// Sentinel used by UniversalRouter for "use the contract's whole balance".
fn contract_balance() -> U256 {
    U256::from(1u8) << 255
}

/// A decoded UniversalRouter swap, before it is lifted into `SwapIntent`.
#[derive(Clone, Debug)]
pub struct UrSwap {
    pub token_in: Address,
    pub token_out: Address,
    pub amount_in: U256,
    pub min_out: U256,
    pub path: Vec<Address>,
    pub native_in: bool,
}

/// Decode one UniversalRouter transaction into a swap. `None` on anything
/// we do not understand — never panics on malformed input.
pub fn decode(to: Address, input: &[u8], value: U256, weth: Address) -> Option<UrSwap> {
    if to != known::UNIVERSAL_ROUTER {
        return None;
    }
    if input.len() < 4 {
        return None;
    }
    let sel: [u8; 4] = [input[0], input[1], input[2], input[3]];
    let (commands, inputs) = if sel == IUniversalRouter::executeCall::SELECTOR {
        let c = IUniversalRouter::executeCall::abi_decode(input, false).ok()?;
        (c.commands, c.inputs)
    } else if sel == IUniversalRouterWithDeadline::executeCall::SELECTOR {
        let c = IUniversalRouterWithDeadline::executeCall::abi_decode(input, false).ok()?;
        (c.commands, c.inputs)
    } else {
        return None;
    };
    walk_commands(commands.as_ref(), &inputs, value, weth)
}

fn walk_commands(commands: &[u8], inputs: &[Bytes], value: U256, weth: Address) -> Option<UrSwap> {
    if commands.len() != inputs.len() {
        return None;
    }
    let mut wrapped: Option<U256> = None;
    for (cmd, input) in commands.iter().zip(inputs.iter()) {
        match *cmd & COMMAND_TYPE_MASK {
            CMD_WRAP_ETH => {
                // (address recipient, uint256 amount)
                if let Some((_, amount)) = decode_wrap(input) {
                    if !amount.is_zero() && amount != contract_balance() {
                        wrapped = Some(amount);
                    } else if !value.is_zero() {
                        wrapped = Some(value);
                    }
                }
            }
            CMD_V2_SWAP_EXACT_IN => {
                if let Some(s) = decode_v2_exact_in(input, value, wrapped, weth) {
                    return Some(s);
                }
            }
            CMD_V3_SWAP_EXACT_IN => {
                if let Some(s) = decode_v3_exact_in(input, value, wrapped, weth) {
                    return Some(s);
                }
            }
            _ => {}
        }
    }
    None
}

fn decode_wrap(input: &[u8]) -> Option<(Address, U256)> {
    <(Address, U256)>::abi_decode(input, false).ok()
}

fn decode_v2_exact_in(
    input: &[u8],
    tx_value: U256,
    wrapped: Option<U256>,
    weth: Address,
) -> Option<UrSwap> {
    // (address recipient, uint256 amountIn, uint256 amountOutMin, address[] path, bool payerIsUser)
    let (_recipient, amount_in, min_out, path, _payer): (Address, U256, U256, Vec<Address>, bool) =
        SolValue::abi_decode(input, false).ok()?;
    if path.len() < 2 {
        return None;
    }
    let token_in = *path.first()?;
    let token_out = *path.last()?;
    if token_in == token_out {
        return None;
    }
    let (amount_in, native_in) = resolve_amount(amount_in, tx_value, wrapped, token_in, weth)?;
    Some(UrSwap {
        token_in,
        token_out,
        amount_in,
        min_out,
        path,
        native_in,
    })
}

fn decode_v3_exact_in(
    input: &[u8],
    tx_value: U256,
    wrapped: Option<U256>,
    weth: Address,
) -> Option<UrSwap> {
    // (address recipient, uint256 amountIn, uint256 amountOutMin, bytes path, bool payerIsUser)
    let (_recipient, amount_in, min_out, path, _payer): (Address, U256, U256, Bytes, bool) =
        SolValue::abi_decode(input, false).ok()?;
    let tokens = decode_v3_path(path.as_ref())?;
    if tokens.len() < 2 {
        return None;
    }
    let token_in = tokens[0];
    let token_out = *tokens.last()?;
    if token_in == token_out {
        return None;
    }
    let (amount_in, native_in) = resolve_amount(amount_in, tx_value, wrapped, token_in, weth)?;
    Some(UrSwap {
        token_in,
        token_out,
        amount_in,
        min_out,
        path: tokens,
        native_in,
    })
}

fn resolve_amount(
    amount_in: U256,
    tx_value: U256,
    wrapped: Option<U256>,
    token_in: Address,
    weth: Address,
) -> Option<(U256, bool)> {
    if !amount_in.is_zero() && amount_in != contract_balance() {
        return Some((amount_in, false));
    }
    if let Some(w) = wrapped {
        if !w.is_zero() {
            return Some((w, token_in == weth));
        }
    }
    if !tx_value.is_zero() && token_in == weth {
        return Some((tx_value, true));
    }
    None
}

/// Packed V3 path: `token (20) || fee (3) || token (20) [|| fee || token …]`.
pub fn decode_v3_path(path: &[u8]) -> Option<Vec<Address>> {
    if path.len() < 43 {
        return None;
    }
    // 20 + n*(3+20) == len, n >= 1
    if (path.len() - 20) % 23 != 0 {
        return None;
    }
    let mut tokens = Vec::new();
    tokens.push(Address::from_slice(&path[0..20]));
    let mut i = 20;
    while i + 23 <= path.len() {
        i += 3;
        tokens.push(Address::from_slice(&path[i..i + 20]));
        i += 20;
    }
    if tokens.len() < 2 {
        return None;
    }
    Some(tokens)
}

/// Encode an `execute(commands, inputs)` payload. Test/fixture helper.
pub fn encode_execute(commands: Vec<u8>, inputs: Vec<Vec<u8>>) -> Vec<u8> {
    IUniversalRouter::executeCall {
        commands: Bytes::from(commands),
        inputs: inputs.into_iter().map(Bytes::from).collect(),
    }
    .abi_encode()
}

/// Pack a single-hop V3 path. Used by tests to build fixtures.
pub fn encode_v3_path(token_in: Address, fee: u32, token_out: Address) -> Vec<u8> {
    let mut out = Vec::with_capacity(43);
    out.extend_from_slice(token_in.as_slice());
    out.push(((fee >> 16) & 0xff) as u8);
    out.push(((fee >> 8) & 0xff) as u8);
    out.push((fee & 0xff) as u8);
    out.extend_from_slice(token_out.as_slice());
    out
}

/// Pack a two-hop V3 path.
pub fn encode_v3_path_two_hop(
    a: Address,
    fee_ab: u32,
    b: Address,
    fee_bc: u32,
    c: Address,
) -> Vec<u8> {
    let mut out = encode_v3_path(a, fee_ab, b);
    out.push(((fee_bc >> 16) & 0xff) as u8);
    out.push(((fee_bc >> 8) & 0xff) as u8);
    out.push((fee_bc & 0xff) as u8);
    out.extend_from_slice(c.as_slice());
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::known;
    use std::time::Instant;

    fn encode_execute_deadline(commands: Vec<u8>, inputs: Vec<Vec<u8>>) -> Vec<u8> {
        IUniversalRouterWithDeadline::executeCall {
            commands: Bytes::from(commands),
            inputs: inputs.into_iter().map(Bytes::from).collect(),
            deadline: U256::from(1_900_000_000u64),
        }
        .abi_encode()
    }

    fn v2_input(amount_in: U256, min_out: U256, path: Vec<Address>) -> Vec<u8> {
        (Address::with_last_byte(9), amount_in, min_out, path, true).abi_encode()
    }

    fn v3_input(amount_in: U256, min_out: U256, path: Vec<u8>) -> Vec<u8> {
        (
            Address::with_last_byte(9),
            amount_in,
            min_out,
            Bytes::from(path),
            true,
        )
            .abi_encode()
    }

    fn wrap_input(amount: U256) -> Vec<u8> {
        (Address::with_last_byte(9), amount).abi_encode()
    }

    #[test]
    fn selectors_match_the_published_universal_router() {
        // Published 4byte.directory / UniversalRouter.sol values. If these
        // two `sol!` types ever collapse into one again, *both* equals
        // fail (they would share 0x24856bc3) rather than silently swapping.
        assert_ne!(
            IUniversalRouter::executeCall::SELECTOR,
            IUniversalRouterWithDeadline::executeCall::SELECTOR,
            "the two execute overloads must not share a sol! type"
        );
        // execute(bytes,bytes[])
        assert_eq!(IUniversalRouter::executeCall::SELECTOR, SEL_EXECUTE);
        assert_eq!(SEL_EXECUTE, [0x35, 0x93, 0x56, 0x4c]);
        // execute(bytes,bytes[],uint256)
        assert_eq!(
            IUniversalRouterWithDeadline::executeCall::SELECTOR,
            SEL_EXECUTE_DEADLINE
        );
        assert_eq!(SEL_EXECUTE_DEADLINE, [0x24, 0x85, 0x6b, 0xc3]);
    }

    #[test]
    fn encode_execute_uses_the_two_arg_selector() {
        let data = encode_execute(vec![CMD_V2_SWAP_EXACT_IN], vec![vec![0u8; 4]]);
        assert_eq!(&data[..4], &SEL_EXECUTE);
    }

    #[test]
    fn encode_execute_deadline_uses_the_three_arg_selector() {
        let data = encode_execute_deadline(vec![CMD_V2_SWAP_EXACT_IN], vec![vec![0u8; 4]]);
        assert_eq!(&data[..4], &SEL_EXECUTE_DEADLINE);
    }

    #[test]
    fn fixture_v2_exact_in_weth_usdc() {
        let path = vec![known::WETH, known::USDC];
        let data = encode_execute(
            vec![CMD_V2_SWAP_EXACT_IN],
            vec![v2_input(U256::from(10u128.pow(18)), U256::from(1_000u64), path.clone())],
        );
        let s = decode(known::UNIVERSAL_ROUTER, &data, U256::ZERO, known::WETH).unwrap();
        assert_eq!(s.token_in, known::WETH);
        assert_eq!(s.token_out, known::USDC);
        assert_eq!(s.amount_in, U256::from(10u128.pow(18)));
        assert_eq!(s.min_out, U256::from(1_000u64));
        assert_eq!(s.path, path);
        assert!(!s.native_in);
    }

    #[test]
    fn fixture_v3_exact_in_single_hop() {
        let path = encode_v3_path(known::WETH, 3_000, known::USDC);
        let data = encode_execute(
            vec![CMD_V3_SWAP_EXACT_IN],
            vec![v3_input(U256::from(5u128.pow(18)), U256::from(42u64), path)],
        );
        let s = decode(known::UNIVERSAL_ROUTER, &data, U256::ZERO, known::WETH).unwrap();
        assert_eq!(s.token_in, known::WETH);
        assert_eq!(s.token_out, known::USDC);
        assert_eq!(s.amount_in, U256::from(5u128.pow(18)));
        assert_eq!(s.min_out, U256::from(42u64));
        assert_eq!(s.path.len(), 2);
    }

    #[test]
    fn fixture_wrap_eth_then_v3_uses_the_wrap_amount() {
        let path = encode_v3_path(known::WETH, 500, known::USDC);
        let data = encode_execute(
            vec![CMD_WRAP_ETH, CMD_V3_SWAP_EXACT_IN],
            vec![
                wrap_input(U256::from(3u128.pow(18))),
                // amountIn = 0 → CONTRACT_BALANCE / wrap
                v3_input(U256::ZERO, U256::from(7u64), path),
            ],
        );
        let s = decode(
            known::UNIVERSAL_ROUTER,
            &data,
            U256::from(3u128.pow(18)),
            known::WETH,
        )
        .unwrap();
        assert_eq!(s.amount_in, U256::from(3u128.pow(18)));
        assert!(s.native_in);
        assert_eq!(s.token_in, known::WETH);
        assert_eq!(s.min_out, U256::from(7u64));
    }

    #[test]
    fn fixture_v3_two_hop_keeps_first_and_last() {
        let path = encode_v3_path_two_hop(known::WETH, 500, known::USDC, 100, known::DAI);
        let data = encode_execute(
            vec![CMD_V3_SWAP_EXACT_IN],
            vec![v3_input(U256::from(2u128.pow(18)), U256::from(9u64), path)],
        );
        let s = decode(known::UNIVERSAL_ROUTER, &data, U256::ZERO, known::WETH).unwrap();
        assert_eq!(s.path.len(), 3);
        assert_eq!(s.token_in, known::WETH);
        assert_eq!(s.token_out, known::DAI);
        assert_eq!(s.path[1], known::USDC);
    }

    #[test]
    fn fixture_execute_with_deadline_decodes() {
        let path = vec![known::WETH, known::USDT];
        let data = encode_execute_deadline(
            vec![CMD_V2_SWAP_EXACT_IN],
            vec![v2_input(U256::from(11u64), U256::from(10u64), path)],
        );
        let s = decode(known::UNIVERSAL_ROUTER, &data, U256::ZERO, known::WETH).unwrap();
        assert_eq!(s.token_out, known::USDT);
        assert_eq!(s.amount_in, U256::from(11u64));
    }

    #[test]
    fn wrong_router_is_rejected() {
        let path = vec![known::WETH, known::USDC];
        let data = encode_execute(
            vec![CMD_V2_SWAP_EXACT_IN],
            vec![v2_input(U256::from(1u64), U256::from(1u64), path)],
        );
        assert!(decode(known::UNIV2_ROUTER, &data, U256::ZERO, known::WETH).is_none());
    }

    #[test]
    fn malformed_inputs_return_none_instead_of_panicking() {
        assert!(decode(known::UNIVERSAL_ROUTER, &[], U256::ZERO, known::WETH).is_none());
        assert!(decode(
            known::UNIVERSAL_ROUTER,
            &[0xde, 0xad, 0xbe, 0xef],
            U256::ZERO,
            known::WETH
        )
        .is_none());
        // commands / inputs length mismatch
        let data = encode_execute(vec![CMD_V2_SWAP_EXACT_IN, CMD_WRAP_ETH], vec![vec![0u8; 4]]);
        assert!(decode(known::UNIVERSAL_ROUTER, &data, U256::ZERO, known::WETH).is_none());
        // empty command list
        let data = encode_execute(vec![], vec![]);
        assert!(decode(known::UNIVERSAL_ROUTER, &data, U256::ZERO, known::WETH).is_none());
        // unknown command only
        let data = encode_execute(vec![0x21], vec![vec![0u8; 32]]);
        assert!(decode(known::UNIVERSAL_ROUTER, &data, U256::ZERO, known::WETH).is_none());
        // truncated V3 path
        let data = encode_execute(
            vec![CMD_V3_SWAP_EXACT_IN],
            vec![v3_input(U256::from(1u64), U256::from(1u64), vec![0u8; 10])],
        );
        assert!(decode(known::UNIVERSAL_ROUTER, &data, U256::ZERO, known::WETH).is_none());
    }

    #[test]
    fn v3_path_round_trips() {
        let p = encode_v3_path(known::WETH, 10_000, known::WBTC);
        let tokens = decode_v3_path(&p).unwrap();
        assert_eq!(tokens, vec![known::WETH, known::WBTC]);
        assert!(decode_v3_path(&p[..20]).is_none());
        assert!(decode_v3_path(&[]).is_none());
    }

    #[test]
    fn decoder_is_well_under_a_millisecond() {
        // Pure calldata parse; the acceptance criterion is < 1 ms per pending
        // tx. A 100-iteration loop on a CI runner still has to land well
        // inside 100 ms or the decoder has grown a surprise allocation.
        let path = encode_v3_path(known::WETH, 3_000, known::USDC);
        let data = encode_execute(
            vec![CMD_V3_SWAP_EXACT_IN],
            vec![v3_input(U256::from(10u128.pow(18)), U256::from(1u64), path)],
        );
        let started = Instant::now();
        for _ in 0..100 {
            assert!(decode(known::UNIVERSAL_ROUTER, &data, U256::ZERO, known::WETH).is_some());
        }
        let elapsed = started.elapsed();
        assert!(
            elapsed.as_millis() < 100,
            "100 decodes took {elapsed:?} (budget 1 ms each)"
        );
    }
}
