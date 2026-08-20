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
}
