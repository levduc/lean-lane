//! Pool integration (increment 2a): `ArcPooledTx` — the lean-aware pool
//! transaction, usable with reth's UNMODIFIED `EthTransactionValidator` and
//! `Pool` (the validator accepts type `0x50` via its existing
//! `with_custom_tx_type` hook; funds/nonce/gas checks run through the generic
//! accessors, which the lean variant answers synthetically — see
//! `envelope.rs`).
//!
//! `Pooled == Consensus == ArcTxEnvelope`: the lane carries no blob sidecars,
//! so the pooled and consensus wire forms coincide and the std reflexive
//! `From`/`TryFrom` impls provide the conversions. Trade-off (documented): an
//! EIP-4844 tx is representable in the pooled position without its sidecar;
//! the validator rejects 4844 on the lane, and the blob-specific
//! `EthPoolTransaction` methods below answer "not a blob transaction".

use crate::envelope::ArcTxEnvelope;
use alloy_consensus::transaction::{Recovered, TxHashRef};
use alloy_consensus::{InMemorySize, Transaction};
use alloy_eips::eip2718::{Encodable2718, Typed2718};
use alloy_eips::eip2930::AccessList;
use alloy_eips::eip4844::env_settings::KzgSettings;
use alloy_eips::eip4844::BlobTransactionValidationError;
use alloy_eips::eip7594::BlobTransactionSidecarVariant;
use alloy_eips::eip7702::SignedAuthorization;
use alloy_primitives::{Address, Bytes, TxHash, TxKind, B256, U256};
use reth_transaction_pool::{
    EthBlobTransactionSidecar, EthPoolTransaction, EthPooledTransaction, PoolTransaction,
};
use std::sync::Arc;

/// Lean-aware pool transaction: reth's own `EthPooledTransaction` cache struct
/// instantiated over the lane envelope, newtyped so the pool traits can be
/// implemented here (orphan rules).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArcPooledTx(pub EthPooledTransaction<ArcTxEnvelope>);

impl ArcPooledTx {
    pub fn new(transaction: Recovered<ArcTxEnvelope>, encoded_length: usize) -> Self {
        Self(EthPooledTransaction::new(transaction, encoded_length))
    }
}

// --- alloy Transaction + friends by delegation to the envelope --------------

macro_rules! delegate {
    ($self:ident . $m:ident ( $($arg:expr),* )) => {
        $self.0.transaction.inner().$m($($arg),*)
    };
}

impl Typed2718 for ArcPooledTx {
    fn ty(&self) -> u8 {
        delegate!(self.ty())
    }
}

impl Transaction for ArcPooledTx {
    fn chain_id(&self) -> Option<alloy_primitives::ChainId> {
        delegate!(self.chain_id())
    }
    fn nonce(&self) -> u64 {
        delegate!(self.nonce())
    }
    fn gas_limit(&self) -> u64 {
        delegate!(self.gas_limit())
    }
    fn gas_price(&self) -> Option<u128> {
        delegate!(self.gas_price())
    }
    fn max_fee_per_gas(&self) -> u128 {
        delegate!(self.max_fee_per_gas())
    }
    fn max_priority_fee_per_gas(&self) -> Option<u128> {
        delegate!(self.max_priority_fee_per_gas())
    }
    fn max_fee_per_blob_gas(&self) -> Option<u128> {
        delegate!(self.max_fee_per_blob_gas())
    }
    fn priority_fee_or_price(&self) -> u128 {
        delegate!(self.priority_fee_or_price())
    }
    fn effective_gas_price(&self, base_fee: Option<u64>) -> u128 {
        delegate!(self.effective_gas_price(base_fee))
    }
    fn is_dynamic_fee(&self) -> bool {
        delegate!(self.is_dynamic_fee())
    }
    fn kind(&self) -> TxKind {
        delegate!(self.kind())
    }
    fn is_create(&self) -> bool {
        delegate!(self.is_create())
    }
    fn value(&self) -> U256 {
        delegate!(self.value())
    }
    fn input(&self) -> &Bytes {
        delegate!(self.input())
    }
    fn access_list(&self) -> Option<&AccessList> {
        delegate!(self.access_list())
    }
    fn blob_versioned_hashes(&self) -> Option<&[B256]> {
        delegate!(self.blob_versioned_hashes())
    }
    fn authorization_list(&self) -> Option<&[SignedAuthorization]> {
        delegate!(self.authorization_list())
    }
}

impl InMemorySize for ArcPooledTx {
    fn size(&self) -> usize {
        self.0.transaction.inner().size()
    }
}

// --- PoolTransaction ---------------------------------------------------------

impl PoolTransaction for ArcPooledTx {
    type TryFromConsensusError = std::convert::Infallible;
    type Consensus = ArcTxEnvelope;
    type Pooled = ArcTxEnvelope;

    fn try_from_consensus(
        tx: Recovered<Self::Consensus>,
    ) -> Result<Self, Self::TryFromConsensusError> {
        let encoded_length = tx.encode_2718_len();
        Ok(Self::new(tx, encoded_length))
    }

    fn consensus_ref(&self) -> Recovered<&Self::Consensus> {
        Recovered::new_unchecked(self.0.transaction.inner(), self.0.transaction.signer())
    }

    fn into_consensus(self) -> Recovered<Self::Consensus> {
        self.0.transaction
    }

    fn from_pooled(pooled: Recovered<Self::Pooled>) -> Self {
        let encoded_length = pooled.encode_2718_len();
        Self::new(pooled, encoded_length)
    }

    fn hash(&self) -> &TxHash {
        self.0.transaction.inner().tx_hash()
    }

    fn sender(&self) -> Address {
        self.0.transaction.signer()
    }

    fn sender_ref(&self) -> &Address {
        self.0.transaction.signer_ref()
    }

    fn cost(&self) -> &U256 {
        &self.0.cost
    }

    fn encoded_length(&self) -> usize {
        self.0.encoded_length
    }
}

// --- EthPoolTransaction (blob surface: the lane has none) --------------------

impl EthPoolTransaction for ArcPooledTx {
    fn take_blob(&mut self) -> EthBlobTransactionSidecar {
        std::mem::replace(&mut self.0.blob_sidecar, EthBlobTransactionSidecar::None)
    }

    fn try_into_pooled_eip4844(
        self,
        _sidecar: Arc<BlobTransactionSidecarVariant>,
    ) -> Option<Recovered<Self::Pooled>> {
        // The lane's pooled form cannot carry a sidecar; 4844 is not supported.
        None
    }

    fn try_from_eip4844(
        _tx: Recovered<Self::Consensus>,
        _sidecar: BlobTransactionSidecarVariant,
    ) -> Option<Self> {
        None
    }

    fn validate_blob(
        &self,
        _blob: &BlobTransactionSidecarVariant,
        _settings: &KzgSettings,
    ) -> Result<(), BlobTransactionValidationError> {
        Err(BlobTransactionValidationError::NotBlobTransaction(self.ty()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::envelope::{current_lane_domain, LeanSigned, LEAN_TX_TYPE};
    use crate::{lean_fee, LeanTx, Output};
    use alloy_eips::Decodable2718;
    use secp256k1::SecretKey;

    // NOTE: the provider-backed pool ADMISSION tests (funded-accept + stale-nonce/
    // insufficient-funds/oversized-N/unconfigured-type rejects, real
    // Pool::add_external_transaction round-trip) live in
    // `lean-lane-node/tests/admission.rs` — they need a StateProviderFactory, and
    // `LeanProvider` lives in that crate. Keeping them out of lean-native is what
    // lets lean-native stay free of any provider dependency.

    fn sk(i: u64) -> SecretKey {
        let mut b = [0u8; 32];
        b[..8].copy_from_slice(&(i + 1).to_be_bytes());
        b[31] = 1;
        SecretKey::from_slice(&b).unwrap()
    }

    fn lean_pooled(n: usize, key: u64, nonce: u32) -> ArcPooledTx {
        let outs = (0..n)
            .map(|i| Output { to: Address::with_last_byte((i % 250) as u8), amount: 100 + i as u64 })
            .collect();
        let env = ArcTxEnvelope::Lean(LeanSigned::new(LeanTx::sign(
            nonce,
            outs,
            &sk(key),
            &current_lane_domain(),
        )));
        let signer = reth_primitives_traits::SignerRecoverable::recover_signer(&env).unwrap();
        let len = env.encode_2718_len();
        ArcPooledTx::new(Recovered::new_unchecked(env, signer), len)
    }

    #[test]
    fn malformed_lean_2718_rejected() {
        // zero outputs
        let mut zero = vec![LEAN_TX_TYPE];
        zero.extend_from_slice(&0u32.to_le_bytes());
        zero.extend_from_slice(&0u16.to_le_bytes());
        zero.extend_from_slice(&[0u8; crate::SIG_LEN]);
        assert!(ArcTxEnvelope::decode_2718(&mut zero.as_slice()).is_err());
        // garbage recovery id -> decodes, but signer recovery must fail
        let good = lean_pooled(1, 6, 0);
        let mut bytes = Vec::new();
        good.0.transaction.inner().encode_2718(&mut bytes);
        *bytes.last_mut().unwrap() = 9; // recovery id out of range
        let env = ArcTxEnvelope::decode_2718(&mut bytes.as_slice()).unwrap();
        assert!(reth_primitives_traits::SignerRecoverable::recover_signer(&env).is_err());
    }

    #[test]
    fn pool_cost_is_fee_plus_value() {
        let tx = lean_pooled(3, 7, 0);
        let expected = U256::from(lean_fee(3)) +
            U256::from((100u128 + 101 + 102) * crate::AMOUNT_UNIT);
        assert_eq!(*tx.cost(), expected);
    }
}
