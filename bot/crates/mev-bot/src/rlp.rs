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

/// Fee envelope carried by a signed EIP-1559 (type-2) transaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Eip1559Envelope {
    pub nonce: u64,
    pub max_priority_fee_per_gas: U256,
    pub max_fee_per_gas: U256,
    pub gas_limit: u64,
}

/// Decode the nonce and fee envelope of a signed EIP-1559 transaction.
///
/// Layout: `0x02 || rlp([chain_id, nonce, max_priority_fee, max_fee, gas,
/// to, value, data, access_list, y_parity, r, s])`. Returns `None` for
/// anything that is not a well-formed type-2 payload. Cancellation uses the
/// original caps as the replacement-price floor; smoke accounting uses
/// `gas_limit × max_fee_per_gas` as the worst-case amount at risk.
pub fn decode_eip1559_envelope(raw: &[u8]) -> Option<Eip1559Envelope> {
    let payload = raw.strip_prefix(&[0x02])?;
    let items = decode_list(payload)?;
    if items.len() < 5 {
        return None;
    }
    Some(Eip1559Envelope {
        nonce: decode_u64(&items[1])?,
        max_priority_fee_per_gas: decode_u256(&items[2])?,
        max_fee_per_gas: decode_u256(&items[3])?,
        gas_limit: decode_u64(&items[4])?,
    })
}

/// Decode only the nonce for callers that do not need fee information.
pub fn decode_eip1559_nonce(raw: &[u8]) -> Option<u64> {
    decode_eip1559_envelope(raw).map(|envelope| envelope.nonce)
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

fn decode_u256(raw: &[u8]) -> Option<U256> {
    if raw.is_empty() {
        return Some(U256::ZERO);
    }
    if raw.len() > 32 || raw[0] == 0 {
        return None;
    }
    Some(U256::from_be_slice(raw))
}

/// A signed transaction decoded from its raw wire bytes.
///
/// Feeds that push *objects* (`newPendingTransactions` with
/// `includeTransactions`) hand us fields directly but usually cannot supply
/// `raw`. Flashblocks is the mirror image: it pushes the signed payload, which
/// is strictly more useful — the raw bytes are exactly what the bundle
/// transport requires in order to carry a victim transaction, and every field
/// below is recovered from the same bytes we would resubmit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecodedTx {
    pub hash: alloy_primitives::B256,
    pub from: Option<alloy_primitives::Address>,
    pub to: Option<alloy_primitives::Address>,
    pub value: U256,
    pub nonce: u64,
    pub gas_limit: u64,
    pub max_fee_per_gas: U256,
    pub max_priority_fee_per_gas: U256,
    pub input: Vec<u8>,
}

/// Decode a signed transaction from raw wire bytes, recovering the sender.
///
/// Handles legacy, EIP-2930 (`0x01`), EIP-1559 (`0x02`) and EIP-4844 (`0x03`)
/// envelopes — the shapes an OP-stack chain actually carries. Deposit
/// transactions (`0x7e`) are intentionally rejected: they are system
/// transactions with no recoverable signature and nothing to back-run.
///
/// Returns `None` for anything malformed rather than guessing, so a feed that
/// changes shape degrades to "no transactions" instead of to wrong ones.
pub fn decode_raw_transaction(raw: &[u8]) -> Option<DecodedTx> {
    use alloy_primitives::keccak256;

    if raw.is_empty() {
        return None;
    }
    let hash = keccak256(raw);

    // (items, signature offset, chain-id-in-payload)
    let (items, sig_start, typed) = match raw[0] {
        // Legacy: rlp([nonce, gasPrice, gas, to, value, data, v, r, s])
        b if b >= 0xc0 => (decode_list(raw)?, 6usize, false),
        0x01 => (decode_list(&raw[1..])?, 8, true),
        0x02 => (decode_list(&raw[1..])?, 9, true),
        0x03 => (decode_list(&raw[1..])?, 11, true),
        _ => return None,
    };
    if items.len() < sig_start + 3 {
        return None;
    }

    // Field positions differ only by the leading chain_id and the gas-price
    // split; normalise both here.
    let (nonce_i, tip_i, cap_i, gas_i, to_i, val_i, data_i) = match raw[0] {
        b if b >= 0xc0 => (0usize, 1usize, 1usize, 2usize, 3usize, 4usize, 5usize),
        _ => (1, 2, 3, 4, 5, 6, 7),
    };

    let to = {
        let b = &items[to_i];
        if b.is_empty() {
            None // contract creation
        } else if b.len() == 20 {
            Some(alloy_primitives::Address::from_slice(b))
        } else {
            return None;
        }
    };

    let sighash = {
        // Re-encode the unsigned prefix and hash it the way the signer did.
        let unsigned: Vec<Vec<u8>> = items[..sig_start]
            .iter()
            .map(|i| encode_bytes(i))
            .collect::<Vec<_>>();
        // Nested lists (access list, blob hashes) were returned pre-encoded by
        // `decode_list`, so re-encoding their bytes as a string would corrupt
        // them. Rebuild using the original encoding for list items.
        let mut parts: Vec<Vec<u8>> = Vec::with_capacity(sig_start);
        for (idx, item) in items[..sig_start].iter().enumerate() {
            // The access list (and, for 4844, the blob-hash list) is the only
            // nested list before the signature; `decode_list` handed those
            // back already encoded.
            let is_list = matches!(
                (raw[0], idx),
                (0x01, 7) | (0x02, 8) | (0x03, 9) | (0x03, 10)
            );
            if is_list {
                parts.push(item.clone());
            } else {
                parts.push(unsigned[idx].clone());
            }
        }
        let body = encode_list(&parts);
        if raw[0] >= 0xc0 {
            // EIP-155 legacy signing hash needs [chain_id, 0, 0] appended,
            // which we cannot know without the chain id; recover below only
            // for typed transactions and skip `from` for legacy.
            keccak256(&body)
        } else {
            let mut buf = Vec::with_capacity(body.len() + 1);
            buf.push(raw[0]);
            buf.extend_from_slice(&body);
            keccak256(&buf)
        }
    };

    let from = if typed {
        recover_sender(
            &sighash,
            &items[sig_start],
            &items[sig_start + 1],
            &items[sig_start + 2],
        )
    } else {
        // Legacy senders need the chain id to rebuild the signing hash; the
        // strategies that matter here only need `to`/`input`/fees, and an
        // absent `from` is already an accepted shape upstream.
        None
    };

    Some(DecodedTx {
        hash,
        from,
        to,
        value: decode_u256(&items[val_i])?,
        nonce: decode_u64(&items[nonce_i])?,
        gas_limit: decode_u64(&items[gas_i])?,
        max_fee_per_gas: decode_u256(&items[cap_i])?,
        max_priority_fee_per_gas: decode_u256(&items[tip_i])?,
        input: items[data_i].clone(),
    })
}

/// Recover the signing address from a `(y_parity, r, s)` triple.
fn recover_sender(
    sighash: &alloy_primitives::B256,
    y_parity: &[u8],
    r: &[u8],
    s: &[u8],
) -> Option<alloy_primitives::Address> {
    use k256::ecdsa::{RecoveryId, Signature, VerifyingKey};

    if r.len() > 32 || s.len() > 32 {
        return None;
    }
    let mut sig = [0u8; 64];
    sig[32 - r.len()..32].copy_from_slice(r);
    sig[64 - s.len()..].copy_from_slice(s);
    let signature = Signature::from_slice(&sig).ok()?;
    let parity = match y_parity {
        [] => 0u8,
        [v] => *v,
        _ => return None,
    };
    let rec = RecoveryId::from_byte(parity)?;
    let vk = VerifyingKey::recover_from_prehash(sighash.as_slice(), &signature, rec).ok()?;
    let point = vk.to_encoded_point(false);
    let h = alloy_primitives::keccak256(&point.as_bytes()[1..]);
    Some(alloy_primitives::Address::from_slice(&h[12..]))
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
            assert_eq!(
                decode_eip1559_envelope(&raw),
                Some(Eip1559Envelope {
                    nonce,
                    max_priority_fee_per_gas: U256::from(1u64),
                    max_fee_per_gas: U256::from(2u64),
                    gas_limit: 21_000,
                })
            );
        }
        assert_eq!(decode_eip1559_nonce(&[]), None);
        assert_eq!(decode_eip1559_nonce(&[0x01, 0xc0]), None);
    }

    #[test]
    fn decodes_a_signed_1559_transaction_and_recovers_the_signer() {
        // Sign with the real signer, then decode the bytes back. This pins the
        // decoder against the encoder rather than against a hand-copied
        // fixture, so a change to either side has to break the round trip.
        use crate::signer::{Eip1559Tx, Signer};
        let signer = Signer::simulation();
        let to = alloy_primitives::Address::repeat_byte(0xab);
        let tx = Eip1559Tx {
            chain_id: 8453,
            nonce: 42,
            max_priority_fee_per_gas: U256::from(1_000_000u64),
            max_fee_per_gas: U256::from(50_000_000u64),
            gas_limit: 210_000,
            to: Some(to),
            value: U256::from(1_234_567_890u64),
            data: vec![0xde, 0xad, 0xbe, 0xef, 0x01, 0x02],
        };
        let (raw, hash) = signer.sign_eip1559(&tx);

        let d = decode_raw_transaction(&raw).expect("decodes");
        assert_eq!(d.hash, hash, "hash must match the signer's");
        assert_eq!(d.from, Some(signer.address()), "sender must be recovered");
        assert_eq!(d.to, Some(to));
        assert_eq!(d.nonce, 42);
        assert_eq!(d.gas_limit, 210_000);
        assert_eq!(d.max_fee_per_gas, U256::from(50_000_000u64));
        assert_eq!(d.max_priority_fee_per_gas, U256::from(1_000_000u64));
        assert_eq!(d.value, U256::from(1_234_567_890u64));
        assert_eq!(d.input, vec![0xde, 0xad, 0xbe, 0xef, 0x01, 0x02]);
    }

    #[test]
    fn decodes_a_contract_creation_as_no_recipient() {
        use crate::signer::{Eip1559Tx, Signer};
        let signer = Signer::simulation();
        let tx = Eip1559Tx {
            chain_id: 1,
            nonce: 0,
            max_priority_fee_per_gas: U256::from(1u64),
            max_fee_per_gas: U256::from(2u64),
            gas_limit: 100_000,
            to: None,
            value: U256::ZERO,
            data: vec![0x60, 0x80],
        };
        let (raw, _) = signer.sign_eip1559(&tx);
        let d = decode_raw_transaction(&raw).expect("decodes");
        assert_eq!(d.to, None, "creation has no recipient");
        assert_eq!(d.from, Some(signer.address()));
    }

    #[test]
    fn rejects_malformed_and_system_payloads() {
        assert!(decode_raw_transaction(&[]).is_none());
        // OP-stack deposit transactions have no recoverable signature.
        assert!(decode_raw_transaction(&[0x7e, 0xc0]).is_none());
        // Truncated typed envelope.
        assert!(decode_raw_transaction(&[0x02, 0xc0]).is_none());
        // Unknown envelope type.
        assert!(decode_raw_transaction(&[0x09, 0xc0]).is_none());
    }
}
