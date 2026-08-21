//! Local secp256k1 signing: EIP-1559 transactions and the Flashbots
//! `X-Flashbots-Signature` header.
//!
//! In simulation mode the key is only ever used for
//!   * `eth_callBundle` authentication (read-only), and
//!   * producing *unbroadcast* raw transactions so the simulated bundle has the
//!     exact same shape and gas cost as a real one.
//!
//! There is no code path in this crate that sends a signed transaction to a
//! public node or relay unless `live_execution` is enabled, which additionally
//! requires an explicit acknowledgement env var.

use alloy_primitives::{keccak256, Address, B256, U256};
use anyhow::{anyhow, Result};
use k256::{
    ecdsa::{hazmat::SignPrimitive, RecoveryId, Signature as EcdsaSignature, SigningKey},
    elliptic_curve::sec1::ToEncodedPoint,
};

use crate::rlp;

#[derive(Clone)]
pub struct Signer {
    key: SigningKey,
    address: Address,
}

impl std::fmt::Debug for Signer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Signer({:?})", self.address)
    }
}

impl Signer {
    pub fn from_hex(key: &str) -> Result<Self> {
        let clean = key.trim().strip_prefix("0x").unwrap_or(key.trim());
        let bytes = hex::decode(clean).map_err(|e| anyhow!("bad private key hex: {e}"))?;
        if bytes.len() != 32 {
            return Err(anyhow!("private key must be 32 bytes, got {}", bytes.len()));
        }
        let key = SigningKey::from_slice(&bytes).map_err(|e| anyhow!("bad private key: {e}"))?;
        let address = public_key_to_address(&key);
        Ok(Self { key, address })
    }

    /// Deterministic throwaway key. Used when the operator has not supplied one:
    /// simulation still needs *a* sender, but it never touches funds.
    pub fn ephemeral() -> Self {
        let mut seed = [0u8; 32];
        seed[..8].copy_from_slice(&crate::types::now_ms().to_be_bytes());
        seed[31] = 1; // guarantee non-zero
        let key = SigningKey::from_slice(&keccak256(seed).0).expect("valid key");
        let address = public_key_to_address(&key);
        Self { key, address }
    }

    pub fn address(&self) -> Address {
        self.address
    }

    fn sign_hash(&self, hash: B256) -> (u8, [u8; 32], [u8; 32]) {
        let (sig, recid): (EcdsaSignature, RecoveryId) = self
            .key
            .sign_prehash_recoverable(hash.as_slice())
            .expect("prehash is 32 bytes");
        let r: [u8; 32] = sig.r().to_bytes().into();
        let s: [u8; 32] = sig.s().to_bytes().into();
        (recid.to_byte(), r, s)
    }

    /// `X-Flashbots-Signature: <address>:<sig>` over the EIP-191 hash of the
    /// hex-encoded keccak of the request body.
    pub fn flashbots_header(&self, body: &[u8]) -> String {
        let body_hash = keccak256(body);
        let message = format!("0x{}", hex::encode(body_hash.0));
        let eip191 = eip191_hash(message.as_bytes());
        let (v, r, s) = self.sign_hash(eip191);
        format!(
            "{:?}:0x{}{}{:02x}",
            self.address,
            hex::encode(r),
            hex::encode(s),
            v + 27
        )
    }

    /// Sign an EIP-1559 (type 2) transaction, returning `(raw_tx, tx_hash)`.
    pub fn sign_eip1559(&self, tx: &Eip1559Tx) -> (Vec<u8>, B256) {
        let unsigned = tx.encode_for_signing();
        let sighash = keccak256(&unsigned);
        let (v, r, s) = self.sign_hash(sighash);
        let raw = tx.encode_signed(v, &r, &s);
        let hash = keccak256(&raw);
        (raw, hash)
    }
}

fn public_key_to_address(key: &SigningKey) -> Address {
    let vk = key.verifying_key();
    let point = vk.to_encoded_point(false);
    // Strip the 0x04 prefix; keccak of the 64-byte public key, last 20 bytes.
    let hash = keccak256(&point.as_bytes()[1..]);
    Address::from_slice(&hash[12..])
}

pub fn eip191_hash(message: &[u8]) -> B256 {
    let mut buf = format!("\x19Ethereum Signed Message:\n{}", message.len()).into_bytes();
    buf.extend_from_slice(message);
    keccak256(buf)
}

/// Minimal EIP-1559 transaction. Access lists are always empty: MEV bundles that
/// need one are rare and it keeps the encoder trivial to audit.
#[derive(Clone, Debug)]
pub struct Eip1559Tx {
    pub chain_id: u64,
    pub nonce: u64,
    pub max_priority_fee_per_gas: U256,
    pub max_fee_per_gas: U256,
    pub gas_limit: u64,
    pub to: Option<Address>,
    pub value: U256,
    pub data: Vec<u8>,
}

impl Eip1559Tx {
    fn body(&self) -> Vec<Vec<u8>> {
        vec![
            rlp::encode_u64(self.chain_id),
            rlp::encode_u64(self.nonce),
            rlp::encode_u256(self.max_priority_fee_per_gas),
            rlp::encode_u256(self.max_fee_per_gas),
            rlp::encode_u64(self.gas_limit),
            match self.to {
                Some(a) => rlp::encode_bytes(a.as_slice()),
                None => rlp::encode_bytes(&[]),
            },
            rlp::encode_u256(self.value),
            rlp::encode_bytes(&self.data),
            rlp::encode_list(&[]), // empty access list
        ]
    }

    /// `0x02 || rlp([chain_id, nonce, ..., access_list])`
    pub fn encode_for_signing(&self) -> Vec<u8> {
        let mut out = vec![0x02u8];
        out.extend_from_slice(&rlp::encode_list(&self.body()));
        out
    }

    /// `0x02 || rlp([..., y_parity, r, s])`
    pub fn encode_signed(&self, y_parity: u8, r: &[u8; 32], s: &[u8; 32]) -> Vec<u8> {
        let mut items = self.body();
        items.push(rlp::encode_u64(y_parity as u64));
        items.push(rlp::encode_bytes(trim_leading_zeros(r)));
        items.push(rlp::encode_bytes(trim_leading_zeros(s)));
        let mut out = vec![0x02u8];
        out.extend_from_slice(&rlp::encode_list(&items));
        out
    }
}

fn trim_leading_zeros(b: &[u8]) -> &[u8] {
    let mut i = 0;
    while i < b.len() && b[i] == 0 {
        i += 1;
    }
    &b[i..]
}

#[cfg(test)]
mod tests {
    use super::*;

    // Well-known test key (hardhat/anvil account #0).
    const KEY: &str = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
    const ADDR: &str = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266";

    #[test]
    fn derives_the_right_address() {
        let s = Signer::from_hex(KEY).unwrap();
        assert_eq!(format!("{:?}", s.address()).to_lowercase(), ADDR.to_lowercase());
    }

    #[test]
    fn signs_a_transaction_deterministically() {
        let s = Signer::from_hex(KEY).unwrap();
        let tx = Eip1559Tx {
            chain_id: 1,
            nonce: 0,
            max_priority_fee_per_gas: U256::from(1_000_000_000u64),
            max_fee_per_gas: U256::from(20_000_000_000u64),
            gas_limit: 21_000,
            to: Some(Address::with_last_byte(1)),
            value: U256::from(1u64),
            data: vec![],
        };
        let (raw1, h1) = s.sign_eip1559(&tx);
        let (raw2, h2) = s.sign_eip1559(&tx);
        assert_eq!(raw1, raw2, "RFC6979 signing must be deterministic");
        assert_eq!(h1, h2);
        assert_eq!(raw1[0], 0x02, "must be a typed transaction");
    }

    #[test]
    fn flashbots_header_is_well_formed() {
        let s = Signer::from_hex(KEY).unwrap();
        let h = s.flashbots_header(b"{\"jsonrpc\":\"2.0\"}");
        let (addr, sig) = h.split_once(':').unwrap();
        assert_eq!(addr.to_lowercase(), ADDR.to_lowercase());
        assert_eq!(sig.len(), 2 + 130, "0x + r(64) + s(64) + v(2)");
    }
}
