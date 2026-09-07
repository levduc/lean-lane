//! `LeanProvider` — a hand-written state view over the lean `FlatState`, in
//! place of reth-provider's `MockEthProvider`. This is what lets the tree drop
//! reth-provider and the entire db/trie/mdbx storage subtree: a payment
//! account is (nonce, balance) with NO code and NO storage, so the pool
//! validator's `StateProviderFactory` needs only `basic_account`.
//!
//! WHAT IS REAL (the validator's actual read path — `validate_stateful` →
//! `basic_account` for nonce+balance; EOAs skip the bytecode path):
//!   * `AccountReader::basic_account` — nonce + balance from the mirror,
//!     `bytecode_hash = None` (payment accounts are EOAs by construction).
//!   * `ChainSpecProvider::chain_spec` — mainnet spec (fork activation +
//!     chain-id source; mirrors what MockEthProvider returned by default).
//!   * `BlockNumReader` chain_info/best/last/block_number — head number.
//!
//! WHAT IS AN HONEST STUB (never reached on the pool-validation path; a lean
//! account has no code, storage, or trie — there is no meaningful answer, and
//! calling them is a programming error, so they fail loudly):
//!   * `BytecodeReader::bytecode_by_hash`, `StateProvider::storage` → `Ok(None)`
//!     (there is genuinely none).
//!   * `StateRootProvider` / `StorageRootProvider` / `StateProofProvider` →
//!     `Err(UnsupportedProvider)` — the lean lane commits via the block
//!     commitment (keccak chain), NOT an MPT root; these are meaningless here.
//!   * `HashedPostStateProvider` → empty `HashedPostState`.
//!   * `BlockHashReader`, historical/pending `StateProviderFactory` variants →
//!     empty/`latest` (the pool only ever asks for `latest`).

use crate::state::{Acct, FlatState};
use alloy_eips::{BlockHashOrNumber, BlockId, BlockNumHash, BlockNumberOrTag};
use alloy_primitives::{Address, BlockNumber, Bytes, B256, U256};
use reth_chainspec::{ChainInfo, ChainSpec, ChainSpecProvider, MAINNET};
use reth_primitives_traits::{Account, Bytecode};
use reth_storage_api::{
    AccountReader, BlockHashReader, BlockIdReader, BlockNumReader, BytecodeReader,
    HashedPostStateProvider, StateProofProvider, StateProvider, StateProviderBox,
    StateProviderFactory, StateRootProvider, StorageRootProvider,
};
use reth_storage_errors::provider::{ProviderError, ProviderResult};
use reth_trie_common::{
    updates::TrieUpdates, AccountProof, HashedPostState, HashedStorage, MultiProof,
    MultiProofTargets, StorageMultiProof, StorageProof, TrieInput,
};
use revm_database::BundleState;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

/// Shared account mirror the pool validator reads. Kept in lockstep with the
/// node's authoritative `FlatState` (the node upserts on genesis-seed and after
/// every committed block, exactly where it previously called `add_account`).
#[derive(Clone, Debug, Default)]
pub struct LeanProvider {
    accounts: Arc<RwLock<HashMap<Address, Acct>>>,
    head: Arc<RwLock<u64>>,
}

impl LeanProvider {
    pub fn new() -> Self {
        Self::default()
    }

    /// Seed/replace one account (mirror of `MockEthProvider::add_account`).
    pub fn upsert(&self, addr: Address, acct: Acct) {
        self.accounts.write().unwrap().insert(addr, acct);
    }

    /// Bulk-load from a recovered `FlatState` (open/recovery path).
    pub fn load_from(&self, state: &FlatState) {
        let mut g = self.accounts.write().unwrap();
        for (a, acct) in &state.accounts {
            g.insert(*a, *acct);
        }
    }

    pub fn set_head(&self, number: u64) {
        *self.head.write().unwrap() = number;
    }

    fn account(&self, addr: &Address) -> Option<Account> {
        self.accounts.read().unwrap().get(addr).map(|a| Account {
            nonce: a.nonce,
            balance: U256::from(a.balance),
            bytecode_hash: None,
        })
    }
}

// ---- the reader half (also the boxed `StateProvider` returned by `latest`) --

impl AccountReader for LeanProvider {
    fn basic_account(&self, address: &Address) -> ProviderResult<Option<Account>> {
        Ok(self.account(address))
    }
}

impl BytecodeReader for LeanProvider {
    fn bytecode_by_hash(&self, _code_hash: &B256) -> ProviderResult<Option<Bytecode>> {
        Ok(None) // payment accounts have no code
    }
}

impl BlockHashReader for LeanProvider {
    fn block_hash(&self, _number: BlockNumber) -> ProviderResult<Option<B256>> {
        Ok(None)
    }
    fn canonical_hashes_range(
        &self,
        _start: BlockNumber,
        _end: BlockNumber,
    ) -> ProviderResult<Vec<B256>> {
        Ok(Vec::new())
    }
}

impl StateProvider for LeanProvider {
    fn storage(
        &self,
        _account: Address,
        _storage_key: alloy_primitives::StorageKey,
    ) -> ProviderResult<Option<alloy_primitives::StorageValue>> {
        Ok(None) // no contract storage on the lane
    }
}

// The commitment-scheme stubs: the lean lane does not use an MPT, so a state
// root / proof is not just unimplemented but semantically absent. Fail loudly
// rather than fabricate a zero root.
impl StateRootProvider for LeanProvider {
    fn state_root(&self, _s: HashedPostState) -> ProviderResult<B256> {
        Err(ProviderError::UnsupportedProvider)
    }
    fn state_root_from_nodes(&self, _i: TrieInput) -> ProviderResult<B256> {
        Err(ProviderError::UnsupportedProvider)
    }
    fn state_root_with_updates(
        &self,
        _s: HashedPostState,
    ) -> ProviderResult<(B256, TrieUpdates)> {
        Err(ProviderError::UnsupportedProvider)
    }
    fn state_root_from_nodes_with_updates(
        &self,
        _i: TrieInput,
    ) -> ProviderResult<(B256, TrieUpdates)> {
        Err(ProviderError::UnsupportedProvider)
    }
}

impl StorageRootProvider for LeanProvider {
    fn storage_root(
        &self,
        _address: Address,
        _hashed_storage: HashedStorage,
    ) -> ProviderResult<B256> {
        Err(ProviderError::UnsupportedProvider)
    }
    fn storage_proof(
        &self,
        _address: Address,
        _slot: B256,
        _hashed_storage: HashedStorage,
    ) -> ProviderResult<StorageProof> {
        Err(ProviderError::UnsupportedProvider)
    }
    fn storage_multiproof(
        &self,
        _address: Address,
        _slots: &[B256],
        _hashed_storage: HashedStorage,
    ) -> ProviderResult<StorageMultiProof> {
        Err(ProviderError::UnsupportedProvider)
    }
}

impl StateProofProvider for LeanProvider {
    fn proof(
        &self,
        _input: TrieInput,
        _address: Address,
        _slots: &[B256],
    ) -> ProviderResult<AccountProof> {
        Err(ProviderError::UnsupportedProvider)
    }
    fn multiproof(
        &self,
        _input: TrieInput,
        _targets: MultiProofTargets,
    ) -> ProviderResult<MultiProof> {
        Err(ProviderError::UnsupportedProvider)
    }
    fn witness(
        &self,
        _input: TrieInput,
        _target: HashedPostState,
        _mode: reth_trie_common::ExecutionWitnessMode,
    ) -> ProviderResult<Vec<Bytes>> {
        Err(ProviderError::UnsupportedProvider)
    }
}

impl HashedPostStateProvider for LeanProvider {
    fn hashed_post_state(&self, _bundle_state: &BundleState) -> HashedPostState {
        HashedPostState::default()
    }
}

// ---- the factory half ------------------------------------------------------

impl ChainSpecProvider for LeanProvider {
    type ChainSpec = ChainSpec;
    fn chain_spec(&self) -> Arc<ChainSpec> {
        MAINNET.clone()
    }
}

impl BlockNumReader for LeanProvider {
    fn chain_info(&self) -> ProviderResult<ChainInfo> {
        Ok(ChainInfo { best_hash: B256::ZERO, best_number: *self.head.read().unwrap() })
    }
    fn best_block_number(&self) -> ProviderResult<BlockNumber> {
        Ok(*self.head.read().unwrap())
    }
    fn last_block_number(&self) -> ProviderResult<BlockNumber> {
        Ok(*self.head.read().unwrap())
    }
    fn block_number(&self, _hash: B256) -> ProviderResult<Option<BlockNumber>> {
        Ok(None)
    }
}

impl BlockIdReader for LeanProvider {
    fn pending_block_num_hash(&self) -> ProviderResult<Option<BlockNumHash>> {
        Ok(None)
    }
    fn safe_block_num_hash(&self) -> ProviderResult<Option<BlockNumHash>> {
        Ok(None)
    }
    fn finalized_block_num_hash(&self) -> ProviderResult<Option<BlockNumHash>> {
        Ok(None)
    }
}

impl StateProviderFactory for LeanProvider {
    fn latest(&self) -> ProviderResult<StateProviderBox> {
        Ok(Box::new(self.clone()))
    }
    fn state_by_block_number_or_tag(
        &self,
        _number_or_tag: BlockNumberOrTag,
    ) -> ProviderResult<StateProviderBox> {
        Ok(Box::new(self.clone())) // single-state lane: only "latest" is meaningful
    }
    fn history_by_block_number(&self, _block: BlockNumber) -> ProviderResult<StateProviderBox> {
        Ok(Box::new(self.clone()))
    }
    fn history_by_block_hash(&self, _block: B256) -> ProviderResult<StateProviderBox> {
        Ok(Box::new(self.clone()))
    }
    fn state_by_block_hash(&self, _block: B256) -> ProviderResult<StateProviderBox> {
        Ok(Box::new(self.clone()))
    }
    fn pending(&self) -> ProviderResult<StateProviderBox> {
        Ok(Box::new(self.clone()))
    }
    fn pending_state_by_hash(
        &self,
        _block_hash: B256,
    ) -> ProviderResult<Option<StateProviderBox>> {
        Ok(Some(Box::new(self.clone())))
    }
    fn maybe_pending(&self) -> ProviderResult<Option<StateProviderBox>> {
        Ok(None)
    }
}

// Silence unused-import lints for aliases only used in signatures on some paths.
#[allow(unused_imports)]
use {BlockHashOrNumber as _BHoN, BlockId as _BId};
