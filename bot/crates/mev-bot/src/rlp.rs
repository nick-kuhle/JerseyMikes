//! A tiny RLP encoder — the only part of the Ethereum wire format the bot needs.
//!
//! Hand-rolled (≈60 lines) instead of pulling a dependency so that the exact
//! bytes we sign are auditable in one screen.

use alloy_primitives::U256;

/// Encode a byte string.
pub fn encode_bytes(b: &[u8]) -> Vec<u8> {
    if b.len() == 1 && b[0] < 0x80 {
        return vec![b[0]];
    }
    let mut out = encode_length(b.len(), 0x80);
    out.extend_from_slice(b);
    out
}

/// Encode a list whose items are *already encoded*.
pub fn encode_list(items: &[Vec<u8>]) -> Vec<u8> {
    let payload: Vec<u8> = items.iter().flat_map(|i| i.iter().copied()).collect();
    let mut out = encode_length(payload.len(), 0xc0);
    out.extend_from_slice(&payload);
    out
}

/// Integers are encoded as big-endian byte strings with no leading zeros;
/// zero itself is the empty string.
pub fn encode_u64(v: u64) -> Vec<u8> {
    if v == 0 {
        return encode_bytes(&[]);
    }
    let be = v.to_be_bytes();
    let start = be.iter().position(|&b| b != 0).unwrap_or(be.len());
    encode_bytes(&be[start..])
}

pub fn encode_u256(v: U256) -> Vec<u8> {
    if v.is_zero() {
        return encode_bytes(&[]);
    }
    let be: [u8; 32] = v.to_be_bytes();
    let start = be.iter().position(|&b| b != 0).unwrap_or(be.len());
    encode_bytes(&be[start..])
}

fn encode_length(len: usize, offset: u8) -> Vec<u8> {
    if len < 56 {
        vec![offset + len as u8]
    } else {
        let be = len.to_be_bytes();
        let start = be.iter().position(|&b| b != 0).unwrap_or(be.len());
        let len_bytes = &be[start..];
        let mut out = vec![offset + 55 + len_bytes.len() as u8];
        out.extend_from_slice(len_bytes);
        out
    }
}

/// Decode the nonce of a signed EIP-1559 (type-2) transaction.
///
/// Layout: `0x02 || rlp([chain_id, nonce, max_priority_fee, max_fee, gas,
/// to, value, data, access_list, y_parity, r, s])`. Returns `None` for
/// anything that is not a well-formed type-2 payload. The fork pins to this
/// number so a stale inventory nonce cannot produce "nonce too low".
pub fn decode_eip1559_nonce(raw: &[u8]) -> Option<u64> {
    let payload = raw.strip_prefix(&[0x02])?;
    let items = decode_list(payload)?;
    // Need at least chain_id + nonce.
    if items.len() < 2 {
        return None;
    }
    decode_u64(&items[1])
}

/// Decode a single top-level RLP list into its items. String items are the
/// payload bytes (so integers can be decoded with [`decode_u64`]); nested
/// lists are returned encoded so the walker can skip them.
fn decode_list(input: &[u8]) -> Option<Vec<Vec<u8>>> {
    let (payload, rest) = take_list(input)?;
    if !rest.is_empty() {
        return None;
    }
    let mut out = Vec::new();
    let mut cur = payload;
    while !cur.is_empty() {
        let (item, next) = take_item(cur)?;
        out.push(item);
        cur = next;
    }
    Some(out)
}

fn take_list(input: &[u8]) -> Option<(&[u8], &[u8])> {
    let first = *input.first()?;
    if (0xc0..=0xf7).contains(&first) {
        let len = (first - 0xc0) as usize;
        if input.len() < 1 + len {
            return None;
        }
        Some((&input[1..1 + len], &input[1 + len..]))
    } else if first >= 0xf8 {
        let len_of_len = (first - 0xf7) as usize;
        if input.len() < 1 + len_of_len {
            return None;
        }
        let len = decode_be_usize(&input[1..1 + len_of_len])?;
        let start = 1 + len_of_len;
        if input.len() < start + len {
            return None;
        }
        Some((&input[start..start + len], &input[start + len..]))
    } else {
        None
    }
}

/// Consume one RLP item. Strings return their payload; lists return the
/// encoded item (prefix included) so a nested access-list can be skipped.
fn take_item(input: &[u8]) -> Option<(Vec<u8>, &[u8])> {
    let first = *input.first()?;
    if first < 0x80 {
        Some((vec![first], &input[1..]))
    } else if first <= 0xb7 {
        let len = (first - 0x80) as usize;
        if input.len() < 1 + len {
            return None;
        }
        Some((input[1..1 + len].to_vec(), &input[1 + len..]))
    } else if first <= 0xbf {
        let len_of_len = (first - 0xb7) as usize;
        if input.len() < 1 + len_of_len {
            return None;
        }
        let len = decode_be_usize(&input[1..1 + len_of_len])?;
        let start = 1 + len_of_len;
        if input.len() < start + len {
            return None;
        }
        Some((input[start..start + len].to_vec(), &input[start + len..]))
    } else if first <= 0xf7 {
        let len = (first - 0xc0) as usize;
        if input.len() < 1 + len {
            return None;
        }
        Some((input[..1 + len].to_vec(), &input[1 + len..]))
    } else {
        let len_of_len = (first - 0xf7) as usize;
        if input.len() < 1 + len_of_len {
            return None;
        }
        let len = decode_be_usize(&input[1..1 + len_of_len])?;
        let start = 1 + len_of_len;
        if input.len() < start + len {
            return None;
        }
        Some((input[..start + len].to_vec(), &input[start + len..]))
    }
}

fn decode_be_usize(bytes: &[u8]) -> Option<usize> {
    if bytes.is_empty() || bytes.len() > 8 || bytes[0] == 0 {
        return None;
    }
    let mut n = 0usize;
    for b in bytes {
        n = n.checked_mul(256)?.checked_add(*b as usize)?;
    }
    Some(n)
}

fn decode_u64(raw: &[u8]) -> Option<u64> {
    if raw.is_empty() {
        return Some(0);
    }
    if raw.len() > 8 || raw[0] == 0 {
        return None;
    }
    let mut n = 0u64;
    for b in raw {
        n = n.checked_shl(8)?.checked_add(*b as u64)?;
    }
    Some(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_the_yellow_paper_vectors() {
        assert_eq!(encode_bytes(b"dog"), vec![0x83, b'd', b'o', b'g']);
        assert_eq!(encode_bytes(&[]), vec![0x80]);
        assert_eq!(encode_bytes(&[0x00]), vec![0x00]);
        assert_eq!(encode_bytes(&[0x0f]), vec![0x0f]);
        assert_eq!(encode_bytes(&[0x04, 0x00]), vec![0x82, 0x04, 0x00]);
        assert_eq!(
            encode_list(&[encode_bytes(b"cat"), encode_bytes(b"dog")]),
            vec![0xc8, 0x83, b'c', b'a', b't', 0x83, b'd', b'o', b'g']
        );
        assert_eq!(encode_list(&[]), vec![0xc0]);
        assert_eq!(encode_u64(0), vec![0x80]);
        assert_eq!(encode_u64(15), vec![0x0f]);
        assert_eq!(encode_u64(1024), vec![0x82, 0x04, 0x00]);
    }

    #[test]
    fn long_strings_use_the_length_prefix() {
        let s = vec![b'a'; 56];
        let enc = encode_bytes(&s);
        assert_eq!(enc[0], 0xb8);
        assert_eq!(enc[1], 56);
        assert_eq!(enc.len(), 58);
    }

    #[test]
    fn u256_drops_leading_zeros() {
        assert_eq!(encode_u256(U256::ZERO), vec![0x80]);
        assert_eq!(encode_u256(U256::from(1024u64)), vec![0x82, 0x04, 0x00]);
    }

    #[test]
    fn eip1559_nonce_round_trips_the_values_we_sign() {
        // Pin against the same encoder the bot uses to sign searcher legs.
        // A drift here would reintroduce "nonce too low" on the fork:
        // we would setNonce to a number that is not in the signed bytes.
        use crate::signer::{Eip1559Tx, Signer};
        use alloy_primitives::Address;
        let s = Signer::from_hex(Signer::SIMULATION_KEY).unwrap();
        for nonce in [0u64, 1, 15, 1024, 1_000_000] {
            let tx = Eip1559Tx {
                chain_id: 1,
                nonce,
                max_priority_fee_per_gas: U256::from(1u64),
                max_fee_per_gas: U256::from(2u64),
                gas_limit: 21_000,
                to: Some(Address::with_last_byte(1)),
                value: U256::ZERO,
                data: vec![],
            };
            let (raw, _) = s.sign_eip1559(&tx);
            assert_eq!(
                decode_eip1559_nonce(&raw),
                Some(nonce),
                "nonce {nonce} did not survive encode→decode"
            );
        }
        assert_eq!(decode_eip1559_nonce(&[]), None);
        assert_eq!(decode_eip1559_nonce(&[0x01, 0xc0]), None);
    }
}
