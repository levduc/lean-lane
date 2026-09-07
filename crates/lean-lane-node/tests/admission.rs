//! Pool-admission tests through the REAL reth `EthTransactionValidator`, fed by
//! the hand-written `LeanProvider` (over FlatState) instead of reth-provider's
//! MockEthProvider. THIS is the gate that proves the FlatState provider feeds
//! the validator correctly — the reason the storage subtree could be dropped.
//! (Moved here from lean-native/src/pool.rs: LeanProvider lives in this crate,
//! and keeping these out of lean-native leaves it provider-dependency-free.)

use alloy_eips::{Decodable2718, Encodable2718};
use alloy_primitives::{Address, U256};
use lean_lane_node::provider::LeanProvider;
use lean_lane_node::state::Acct;
use lean_native::envelope::{current_lane_domain, ArcTxEnvelope, LeanSigned, LEAN_TX_TYPE};
use lean_native::pool::ArcPooledTx;
use lean_native::{lean_gas, LeanTx, Output, MAX_OUTPUTS};
use reth_transaction_pool::blobstore::InMemoryBlobStore;
use reth_transaction_pool::validate::{EthTransactionValidator, EthTransactionValidatorBuilder};
use reth_transaction_pool::{
    CoinbaseTipOrdering, Pool, PoolTransaction, TransactionOrigin, TransactionPool,
    TransactionValidator,
};
use secp256k1::SecretKey;

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
    ArcPooledTx::new(alloy_consensus::transaction::Recovered::new_unchecked(env, signer), len)
}

fn balance_wei_of(tx: &ArcPooledTx) -> u128 {
    // cost() = fee + Σ output value (wei); fund exactly that unless overridden
    tx.cost().try_into().unwrap_or(u128::MAX)
}

fn validator(
    provider: LeanProvider,
    custom_type: bool,
) -> EthTransactionValidator<LeanProvider, ArcPooledTx, reth_evm_ethereum::EthEvmConfig> {
    let b = EthTransactionValidatorBuilder::new(provider, reth_evm_ethereum::EthEvmConfig::mainnet());
    let b = if custom_type { b.with_custom_tx_type(LEAN_TX_TYPE) } else { b };
    b.build(InMemoryBlobStore::default())
}

#[tokio::test]
async fn lean_tx_admitted_through_real_pool() {
    let tx = lean_pooled(3, 1, 0);
    let provider = LeanProvider::new();
    provider.upsert(tx.sender(), Acct { nonce: 0, balance: u128::MAX });
    let blob_store = InMemoryBlobStore::default();
    let v = EthTransactionValidatorBuilder::new(
        provider,
        reth_evm_ethereum::EthEvmConfig::mainnet(),
    )
    .with_custom_tx_type(LEAN_TX_TYPE)
    .build(blob_store.clone());

    let outcome = v.validate_one(TransactionOrigin::External, tx.clone());
    assert!(outcome.is_valid(), "lean tx must validate: {outcome:?}");

    let pool = Pool::new(v, CoinbaseTipOrdering::default(), blob_store, Default::default());
    pool.add_external_transaction(tx.clone()).await.expect("pool insert");
    assert!(pool.get(tx.hash()).is_some(), "lean tx must be retrievable from the pool");
}

#[tokio::test]
async fn lean_tx_rejected_without_custom_type_bit() {
    let tx = lean_pooled(1, 2, 0);
    let provider = LeanProvider::new();
    provider.upsert(tx.sender(), Acct { nonce: 0, balance: u128::MAX });
    let outcome = validator(provider, false).validate_one(TransactionOrigin::External, tx);
    assert!(!outcome.is_valid(), "0x50 must be rejected unless explicitly enabled");
}

#[tokio::test]
async fn lean_tx_rejected_on_stale_nonce() {
    let tx = lean_pooled(2, 3, 0);
    let provider = LeanProvider::new();
    provider.upsert(tx.sender(), Acct { nonce: 7, balance: u128::MAX }); // account nonce 7 > tx nonce 0
    let outcome = validator(provider, true).validate_one(TransactionOrigin::External, tx);
    assert!(!outcome.is_valid(), "stale nonce must be rejected");
}

#[tokio::test]
async fn lean_tx_rejected_on_insufficient_funds() {
    let tx = lean_pooled(2, 4, 0);
    let provider = LeanProvider::new();
    provider.upsert(tx.sender(), Acct { nonce: 0, balance: balance_wei_of(&tx) - 1 });
    let outcome = validator(provider, true).validate_one(TransactionOrigin::External, tx);
    assert!(!outcome.is_valid(), "insufficient funds must be rejected");
}

#[tokio::test]
async fn lean_tx_rejected_on_oversized_fanout() {
    // N = MAX_OUTPUTS => gas = 21000 + 5000*10_000 = 50.021M > mainnet block gas limit
    let tx = lean_pooled(MAX_OUTPUTS, 5, 0);
    assert!(lean_gas(MAX_OUTPUTS) > 45_000_000);
    let provider = LeanProvider::new();
    provider.upsert(tx.sender(), Acct { nonce: 0, balance: u128::MAX });
    let outcome = validator(provider, true).validate_one(TransactionOrigin::External, tx);
    assert!(!outcome.is_valid(), "oversized fan-out must exceed block gas limit");
}

#[test]
fn state_root_stub_errors_loudly() {
    // The commitment-scheme stub must fail, not fabricate a zero root.
    use reth_storage_api::StateRootProvider;
    use reth_trie_common::HashedPostState;
    let p = LeanProvider::new();
    assert!(p.state_root(HashedPostState::default()).is_err());
}
