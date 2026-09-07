//! EIP-2718 integration: `ArcTxEnvelope` — a first-class consensus transaction
//! type that carries every Ethereum tx variant PLUS the lean fan-out transfer
//! as tx type `0x50`.
//!
//! Design (increment 2a): the alloy `EthereumTxEnvelope` is a closed enum, so
//! the lean type rides in a fork-local wrapper enum that delegates all trait
//! surface to alloy for the Ethereum variants and implements it natively for
//! the lean variant. The wrapper implements everything reth's
//! `SignedTransaction` demands, which makes it usable as the `Consensus` AND
//! `Pooled` type of a pool transaction (the blanket `From<T> for T` gives the
//! pooled<->consensus conversions for free — the lean lane has no blob
//! sidecars, so pooled and consensus forms coincide; documented trade-off:
//! a 4844 tx without a sidecar is representable in the pooled position, and
//! it is the validator's job to reject it, as it already does).
//!
//! Type byte `0x50`: outside the standard 0x00..=0x04 range, distinct from
//! op-stack's 0x7e deposit, and below the 0x80 ceiling for typed-envelope
//! first bytes.
//!
//! Chain binding: the wire format carries no chain id; the signature commits
//! to `lane_domain(chain_id)`. In generic reth trait methods (`recover_signer`
//! has no chain context) the domain comes from a process-wide lane id, set
//! once at node startup via [`set_lane_chain_id`] (default 1338 — the Arc
//! payment lane).

use crate::{lane_domain, lean_gas, LeanTx, AMOUNT_UNIT, GAS_PRICE};
use alloy_consensus::crypto::RecoveryError;
use alloy_consensus::transaction::SignerRecoverable;
use alloy_consensus::{InMemorySize, Transaction, TxEip4844};
use alloy_eips::eip2718::{Eip2718Error, Eip2718Result, Encodable2718, IsTyped2718, Typed2718};
use alloy_eips::eip2930::AccessList;
use alloy_eips::eip7702::SignedAuthorization;
use alloy_eips::Decodable2718;
use alloy_primitives::{keccak256, Address, Bytes, TxKind, B256, U256};
use std::sync::atomic::{AtomicU64, Ordering};

/// EIP-2718 transaction type byte of the lean fan-out transfer (re-exported
/// from the wire module: the type byte IS the first canonical byte).
pub use crate::LEAN_TX_TYPE;

/// The Ethereum envelope the wrapper delegates to (reth's `TransactionSigned`).
pub type EthEnvelope = alloy_consensus::EthereumTxEnvelope<TxEip4844>;

static LANE_CHAIN_ID: AtomicU64 = AtomicU64::new(1338);

/// Set the process-wide lane chain id used to derive the lean signing domain.
/// Call once at startup, before any lean tx is decoded or recovered.
pub fn set_lane_chain_id(chain_id: u64) {
    LANE_CHAIN_ID.store(chain_id, Ordering::Relaxed);
}

/// The lane's current signing domain (see [`crate::lane_domain`]).
pub fn current_lane_domain() -> B256 {
    lane_domain(LANE_CHAIN_ID.load(Ordering::Relaxed))
}

/// A signed lean tx that RETAINS its canonical bytes: decoded once, the raw
/// form is reused verbatim for every later encode (2718, RLP payload, gossip,
/// storage) and the hash is computed once (`keccak256(raw)`). No role in the
/// pipeline ever re-encodes field-by-field.
#[derive(Clone, Debug)]
pub struct LeanSigned {
    /// Full canonical bytes, leading 2718 type byte included.
    raw: Bytes,
    /// Parsed view of `raw`.
    tx: LeanTx,
    hash: B256,
    /// Cached sum of outputs in wei (the `value()` accessor must return `U256`).
    value: U256,
    /// Empty calldata, held so `input()` can hand out a reference.
    empty_input: Bytes,
}

impl LeanSigned {
    fn from_parts(raw: Bytes, tx: LeanTx) -> Self {
        let hash = keccak256(&raw);
        let value = tx.outputs.iter().fold(U256::ZERO, |acc, o| {
            acc.saturating_add(U256::from(o.amount as u128 * AMOUNT_UNIT))
        });
        Self { raw, tx, hash, value, empty_input: Bytes::new() }
    }

    /// Signing-side constructor: encodes ONCE (the only encode in the tx's
    /// entire life) and retains the bytes.
    pub fn new(tx: LeanTx) -> Self {
        let raw = Bytes::from(tx.encode());
        Self::from_parts(raw, tx)
    }

    /// Decode-side constructor: parse the canonical bytes (type byte
    /// included), retaining them. The single decode of the tx's life.
    pub fn from_raw(raw: Bytes) -> Result<Self, crate::DecodeError> {
        let tx = LeanTx::decode(&raw)?;
        Ok(Self::from_parts(raw, tx))
    }

    pub fn tx(&self) -> &LeanTx {
        &self.tx
    }

    pub fn raw(&self) -> &Bytes {
        &self.raw
    }

    pub fn hash(&self) -> &B256 {
        &self.hash
    }
}

impl PartialEq for LeanSigned {
    fn eq(&self, other: &Self) -> bool {
        self.tx == other.tx
    }
}
impl Eq for LeanSigned {}
impl std::hash::Hash for LeanSigned {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.hash.hash(state)
    }
}

/// First-class consensus envelope for the lane: Ethereum variants + lean.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ArcTxEnvelope {
    Eth(EthEnvelope),
    Lean(LeanSigned),
}

impl From<EthEnvelope> for ArcTxEnvelope {
    fn from(tx: EthEnvelope) -> Self {
        Self::Eth(tx)
    }
}
impl From<LeanSigned> for ArcTxEnvelope {
    fn from(tx: LeanSigned) -> Self {
        Self::Lean(tx)
    }
}

// --- Typed2718 / IsTyped2718 ------------------------------------------------

impl Typed2718 for ArcTxEnvelope {
    fn ty(&self) -> u8 {
        match self {
            Self::Eth(t) => t.ty(),
            Self::Lean(_) => LEAN_TX_TYPE,
        }
    }
}

impl IsTyped2718 for ArcTxEnvelope {
    fn is_type(type_id: u8) -> bool {
        type_id == LEAN_TX_TYPE || <EthEnvelope as IsTyped2718>::is_type(type_id)
    }
}

// --- EIP-2718 encode / decode ----------------------------------------------

impl Encodable2718 for ArcTxEnvelope {
    fn encode_2718_len(&self) -> usize {
        match self {
            Self::Eth(t) => t.encode_2718_len(),
            Self::Lean(t) => t.raw.len(),
        }
    }

    fn encode_2718(&self, out: &mut dyn alloy_rlp::BufMut) {
        match self {
            Self::Eth(t) => t.encode_2718(out),
            // Pure passthrough of the retained canonical bytes — no re-encode.
            Self::Lean(t) => out.put_slice(&t.raw),
        }
    }

    fn trie_hash(&self) -> B256 {
        match self {
            Self::Eth(t) => t.trie_hash(),
            Self::Lean(t) => t.hash,
        }
    }
}

impl Decodable2718 for ArcTxEnvelope {
    fn typed_decode(ty: u8, buf: &mut &[u8]) -> Eip2718Result<Self> {
        if ty == LEAN_TX_TYPE {
            // Parse once; retain the canonical bytes (one memcpy from the
            // network buffer, never a field-by-field re-encode).
            let before = *buf;
            let tx = LeanTx::parse_after_type(buf)
                .map_err(|_| Eip2718Error::RlpError(alloy_rlp::Error::Custom("bad lean tx")))?;
            let consumed = before.len() - buf.len();
            let mut raw = Vec::with_capacity(1 + consumed);
            raw.push(LEAN_TX_TYPE);
            raw.extend_from_slice(&before[..consumed]);
            return Ok(Self::Lean(LeanSigned::from_parts(Bytes::from(raw), tx)));
        }
        Ok(Self::Eth(EthEnvelope::typed_decode(ty, buf)?))
    }

    fn fallback_decode(buf: &mut &[u8]) -> Eip2718Result<Self> {
        Ok(Self::Eth(EthEnvelope::fallback_decode(buf)?))
    }
}

// --- RLP (network / block-body) form ---------------------------------------

impl alloy_rlp::Encodable for ArcTxEnvelope {
    fn encode(&self, out: &mut dyn alloy_rlp::BufMut) {
        match self {
            Self::Eth(t) => t.encode(out),
            Self::Lean(_) => {
                // Typed txs travel as an RLP string of the opaque 2718 bytes.
                alloy_rlp::Header { list: false, payload_length: self.encode_2718_len() }
                    .encode(out);
                self.encode_2718(out);
            }
        }
    }

    fn length(&self) -> usize {
        match self {
            Self::Eth(t) => t.length(),
            Self::Lean(_) => {
                let payload = self.encode_2718_len();
                alloy_rlp::Header { list: false, payload_length: payload }.length() + payload
            }
        }
    }
}

impl alloy_rlp::Decodable for ArcTxEnvelope {
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        let first = *buf.first().ok_or(alloy_rlp::Error::InputTooShort)?;
        if first >= 0xc0 {
            // Legacy transaction: a bare RLP list.
            return Ok(Self::Eth(alloy_rlp::Decodable::decode(buf)?));
        }
        let header = alloy_rlp::Header::decode(buf)?;
        if buf.len() < header.payload_length {
            return Err(alloy_rlp::Error::InputTooShort);
        }
        let mut payload = &buf[..header.payload_length];
        let this = Self::decode_2718(&mut payload)
            .map_err(|_| alloy_rlp::Error::Custom("2718 decode failed"))?;
        if !payload.is_empty() {
            return Err(alloy_rlp::Error::UnexpectedLength);
        }
        *buf = &buf[header.payload_length..];
        Ok(this)
    }
}

// --- alloy Transaction (synthetic accessors for the lean variant) -----------
//
// The lean variant answers the generic accessors so that reth's UNMODIFIED
// pool/validator logic does the right thing:
//   gas_limit  = lean_gas(N)     (packing weight; oversized N -> natural
//                                 ExceedsGasLimit rejection)
//   *fee*      = GAS_PRICE flat  (protocol-fixed; is_dynamic_fee = false and
//                                 chain_id = None skip the fee-market and
//                                 chain-id checks that don't apply)
//   value      = sum(outputs)    (so generic cost = fee*gas + value is exact)
//   kind       = Call(first recipient)

impl Transaction for ArcTxEnvelope {
    fn chain_id(&self) -> Option<alloy_primitives::ChainId> {
        match self {
            Self::Eth(t) => t.chain_id(),
            Self::Lean(_) => None,
        }
    }

    fn nonce(&self) -> u64 {
        match self {
            Self::Eth(t) => t.nonce(),
            Self::Lean(t) => t.tx.nonce as u64,
        }
    }

    fn gas_limit(&self) -> u64 {
        match self {
            Self::Eth(t) => t.gas_limit(),
            Self::Lean(t) => lean_gas(t.tx.outputs.len()),
        }
    }

    fn gas_price(&self) -> Option<u128> {
        match self {
            Self::Eth(t) => t.gas_price(),
            Self::Lean(_) => Some(GAS_PRICE),
        }
    }

    fn max_fee_per_gas(&self) -> u128 {
        match self {
            Self::Eth(t) => t.max_fee_per_gas(),
            Self::Lean(_) => GAS_PRICE,
        }
    }

    fn max_priority_fee_per_gas(&self) -> Option<u128> {
        match self {
            Self::Eth(t) => t.max_priority_fee_per_gas(),
            Self::Lean(_) => None,
        }
    }

    fn max_fee_per_blob_gas(&self) -> Option<u128> {
        match self {
            Self::Eth(t) => t.max_fee_per_blob_gas(),
            Self::Lean(_) => None,
        }
    }

    fn priority_fee_or_price(&self) -> u128 {
        match self {
            Self::Eth(t) => t.priority_fee_or_price(),
            Self::Lean(_) => GAS_PRICE,
        }
    }

    fn effective_gas_price(&self, base_fee: Option<u64>) -> u128 {
        match self {
            Self::Eth(t) => t.effective_gas_price(base_fee),
            Self::Lean(_) => GAS_PRICE,
        }
    }

    fn is_dynamic_fee(&self) -> bool {
        match self {
            Self::Eth(t) => t.is_dynamic_fee(),
            Self::Lean(_) => false,
        }
    }

    fn kind(&self) -> TxKind {
        match self {
            Self::Eth(t) => t.kind(),
            Self::Lean(t) => TxKind::Call(t.tx.outputs[0].to),
        }
    }

    fn is_create(&self) -> bool {
        match self {
            Self::Eth(t) => t.is_create(),
            Self::Lean(_) => false,
        }
    }

    fn value(&self) -> U256 {
        match self {
            Self::Eth(t) => t.value(),
            Self::Lean(t) => t.value,
        }
    }

    fn input(&self) -> &Bytes {
        match self {
            Self::Eth(t) => t.input(),
            Self::Lean(t) => &t.empty_input,
        }
    }

    fn access_list(&self) -> Option<&AccessList> {
        match self {
            Self::Eth(t) => t.access_list(),
            Self::Lean(_) => None,
        }
    }

    fn blob_versioned_hashes(&self) -> Option<&[B256]> {
        match self {
            Self::Eth(t) => t.blob_versioned_hashes(),
            Self::Lean(_) => None,
        }
    }

    fn authorization_list(&self) -> Option<&[SignedAuthorization]> {
        match self {
            Self::Eth(t) => t.authorization_list(),
            Self::Lean(_) => None,
        }
    }
}

// --- signer recovery / hash / size -----------------------------------------

impl SignerRecoverable for ArcTxEnvelope {
    fn recover_signer(&self) -> Result<Address, RecoveryError> {
        match self {
            Self::Eth(t) => t.recover_signer(),
            Self::Lean(t) => t
                .tx
                .recover_sender(&current_lane_domain())
                .map_err(|_| RecoveryError::new()),
        }
    }

    fn recover_signer_unchecked(&self) -> Result<Address, RecoveryError> {
        match self {
            Self::Eth(t) => t.recover_signer_unchecked(),
            Self::Lean(t) => t
                .tx
                .recover_sender(&current_lane_domain())
                .map_err(|_| RecoveryError::new()),
        }
    }
}

impl alloy_consensus::transaction::TxHashRef for ArcTxEnvelope {
    fn tx_hash(&self) -> &B256 {
        match self {
            Self::Eth(t) => t.tx_hash(),
            Self::Lean(t) => &t.hash,
        }
    }
}

impl InMemorySize for ArcTxEnvelope {
    fn size(&self) -> usize {
        match self {
            Self::Eth(t) => t.size(),
            Self::Lean(t) => core::mem::size_of::<LeanSigned>() + t.tx.outputs.len() * 36,
        }
    }
}

// --- serde (2718 hex bytes) -------------------------------------------------

impl serde::Serialize for ArcTxEnvelope {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut bytes = Vec::with_capacity(self.encode_2718_len());
        self.encode_2718(&mut bytes);
        Bytes::from(bytes).serialize(s)
    }
}

impl<'de> serde::Deserialize<'de> for ArcTxEnvelope {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let bytes = Bytes::deserialize(d)?;
        Self::decode_2718(&mut bytes.as_ref()).map_err(serde::de::Error::custom)
    }
}

// reth's `SignedTransaction` arrives via its blanket impl (all supertraits
// implemented above) — an explicit impl would conflict with it.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Output;
    use secp256k1::SecretKey;

    fn sk(i: u64) -> SecretKey {
        let mut b = [0u8; 32];
        b[..8].copy_from_slice(&(i + 1).to_be_bytes());
        b[31] = 1;
        SecretKey::from_slice(&b).unwrap()
    }

    fn lean(n: usize, key: u64) -> ArcTxEnvelope {
        let outs = (0..n)
            .map(|i| Output { to: Address::with_last_byte((i % 250) as u8), amount: 1 + i as u64 })
            .collect();
        ArcTxEnvelope::Lean(LeanSigned::new(LeanTx::sign(
            0,
            outs,
            &sk(key),
            &current_lane_domain(),
        )))
    }

    #[test]
    fn envelope_2718_roundtrip_lean() {
        let tx = lean(3, 1);
        let mut bytes = Vec::new();
        tx.encode_2718(&mut bytes);
        assert_eq!(bytes[0], LEAN_TX_TYPE);
        assert_eq!(bytes.len(), tx.encode_2718_len());
        let back = ArcTxEnvelope::decode_2718(&mut bytes.as_slice()).unwrap();
        assert_eq!(back, tx);
        use alloy_consensus::transaction::TxHashRef;
        assert_eq!(back.tx_hash(), tx.tx_hash());
    }

    #[test]
    fn envelope_rlp_roundtrip_lean_and_eth_shape() {
        let tx = lean(2, 2);
        let mut rlp = Vec::new();
        alloy_rlp::Encodable::encode(&tx, &mut rlp);
        assert_eq!(rlp.len(), alloy_rlp::Encodable::length(&tx));
        let back: ArcTxEnvelope = alloy_rlp::Decodable::decode(&mut rlp.as_slice()).unwrap();
        assert_eq!(back, tx);
    }

    #[test]
    fn recovery_through_trait_matches_direct() {
        let tx = lean(4, 3);
        let ArcTxEnvelope::Lean(ref inner) = tx else { unreachable!() };
        let direct = inner.tx().recover_sender(&current_lane_domain()).unwrap();
        assert_eq!(tx.recover_signer().unwrap(), direct);
    }

    #[test]
    fn synthetic_accessors() {
        let tx = lean(10, 4);
        assert_eq!(tx.ty(), LEAN_TX_TYPE);
        assert_eq!(tx.gas_limit(), lean_gas(10));
        assert_eq!(tx.max_fee_per_gas(), GAS_PRICE);
        assert!(!tx.is_dynamic_fee());
        assert_eq!(tx.chain_id(), None);
        assert_eq!(tx.value(), U256::from(55u128 * AMOUNT_UNIT));
        assert!(tx.input().is_empty());
    }

    #[test]
    fn serde_roundtrip() {
        let tx = lean(2, 5);
        let json = serde_json::to_string(&tx).unwrap();
        let back: ArcTxEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(back, tx);
    }
}
