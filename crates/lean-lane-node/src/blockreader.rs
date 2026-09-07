//! Minimal `BlockReaderIdExt` on `LeanProvider` — the LAST thing standing
//! between the lean node and dropping reth-provider. The pool validator's
//! `EthTransactionValidatorBuilder::new` fetches `header_by_id(latest)` once to
//! build an EVM env (base fee / number / timestamp for intrinsic-gas checks);
//! it never reads a block, receipt, or historical header. So every method here
//! is an empty stub EXCEPT `header_by_id(latest)`, which returns a synthetic
//! default header. The lane has no Ethereum blocks/receipts to serve — its
//! blocks live in the append-only log as lean commitments, not reth types.
//!
//! This is all-stub by necessity, not laziness: serving a real reth `Block`/
//! `Receipt` here would mean re-importing the storage stack this whole prune
//! exists to remove.

use crate::provider::LeanProvider;
use alloy_consensus::Header;
use alloy_eips::{BlockHashOrNumber, BlockId, BlockNumberOrTag};
use alloy_primitives::{Address, BlockHash, BlockNumber, TxHash, TxNumber};
use alloy_consensus::transaction::TransactionMeta;
use reth_db_models::StoredBlockBodyIndices;
use reth_ethereum_primitives::{Block, Receipt, TransactionSigned};
use reth_storage_api::{
    BlockBodyIndicesProvider, BlockReader, BlockReaderIdExt, BlockSource, HeaderProvider,
    ReceiptProvider, ReceiptProviderIdExt, TransactionVariant, TransactionsProvider,
};
use reth_storage_errors::provider::ProviderResult;

impl HeaderProvider for LeanProvider {
    type Header = Header;
    fn header(&self, _block_hash: BlockHash) -> ProviderResult<Option<Header>> {
        Ok(None)
    }
    fn header_by_number(&self, _num: u64) -> ProviderResult<Option<Header>> {
        Ok(None)
    }
    fn headers_range(
        &self,
        _range: impl core::ops::RangeBounds<BlockNumber>,
    ) -> ProviderResult<Vec<Header>> {
        Ok(Vec::new())
    }
    fn sealed_header(
        &self,
        _number: BlockNumber,
    ) -> ProviderResult<Option<reth_primitives_traits::SealedHeader<Header>>> {
        Ok(None)
    }
    fn sealed_headers_while(
        &self,
        _range: impl core::ops::RangeBounds<BlockNumber>,
        _predicate: impl FnMut(&reth_primitives_traits::SealedHeader<Header>) -> bool,
    ) -> ProviderResult<Vec<reth_primitives_traits::SealedHeader<Header>>> {
        Ok(Vec::new())
    }
}

impl BlockBodyIndicesProvider for LeanProvider {
    fn block_body_indices(&self, _num: u64) -> ProviderResult<Option<StoredBlockBodyIndices>> {
        Ok(None)
    }
    fn block_body_indices_range(
        &self,
        _range: core::ops::RangeInclusive<BlockNumber>,
    ) -> ProviderResult<Vec<StoredBlockBodyIndices>> {
        Ok(Vec::new())
    }
}

impl TransactionsProvider for LeanProvider {
    type Transaction = TransactionSigned;
    fn transaction_id(&self, _tx_hash: TxHash) -> ProviderResult<Option<TxNumber>> {
        Ok(None)
    }
    fn transaction_by_id(&self, _id: TxNumber) -> ProviderResult<Option<TransactionSigned>> {
        Ok(None)
    }
    fn transaction_by_id_unhashed(
        &self,
        _id: TxNumber,
    ) -> ProviderResult<Option<TransactionSigned>> {
        Ok(None)
    }
    fn transaction_by_hash(&self, _hash: TxHash) -> ProviderResult<Option<TransactionSigned>> {
        Ok(None)
    }
    fn transaction_by_hash_with_meta(
        &self,
        _hash: TxHash,
    ) -> ProviderResult<Option<(TransactionSigned, TransactionMeta)>> {
        Ok(None)
    }
    fn transactions_by_block(
        &self,
        _block: BlockHashOrNumber,
    ) -> ProviderResult<Option<Vec<TransactionSigned>>> {
        Ok(None)
    }
    fn transactions_by_block_range(
        &self,
        _range: impl core::ops::RangeBounds<BlockNumber>,
    ) -> ProviderResult<Vec<Vec<TransactionSigned>>> {
        Ok(Vec::new())
    }
    fn transactions_by_tx_range(
        &self,
        _range: impl core::ops::RangeBounds<TxNumber>,
    ) -> ProviderResult<Vec<TransactionSigned>> {
        Ok(Vec::new())
    }
    fn senders_by_tx_range(
        &self,
        _range: impl core::ops::RangeBounds<TxNumber>,
    ) -> ProviderResult<Vec<Address>> {
        Ok(Vec::new())
    }
    fn transaction_sender(&self, _id: TxNumber) -> ProviderResult<Option<Address>> {
        Ok(None)
    }
}

impl ReceiptProvider for LeanProvider {
    type Receipt = Receipt;
    fn receipt(&self, _id: TxNumber) -> ProviderResult<Option<Receipt>> {
        Ok(None)
    }
    fn receipt_by_hash(&self, _hash: TxHash) -> ProviderResult<Option<Receipt>> {
        Ok(None)
    }
    fn receipts_by_block(
        &self,
        _block: BlockHashOrNumber,
    ) -> ProviderResult<Option<Vec<Receipt>>> {
        Ok(None)
    }
    fn receipts_by_tx_range(
        &self,
        _range: impl core::ops::RangeBounds<TxNumber>,
    ) -> ProviderResult<Vec<Receipt>> {
        Ok(Vec::new())
    }
    fn receipts_by_block_range(
        &self,
        _block_range: core::ops::RangeInclusive<BlockNumber>,
    ) -> ProviderResult<Vec<Vec<Receipt>>> {
        Ok(Vec::new())
    }
}

impl BlockReader for LeanProvider {
    type Block = Block;
    fn block(&self, _id: BlockHashOrNumber) -> ProviderResult<Option<Block>> {
        Ok(None)
    }
    fn pending_block(
        &self,
    ) -> ProviderResult<Option<reth_primitives_traits::RecoveredBlock<Block>>> {
        Ok(None)
    }
    fn pending_block_and_receipts(
        &self,
    ) -> ProviderResult<Option<(reth_primitives_traits::RecoveredBlock<Block>, Vec<Receipt>)>> {
        Ok(None)
    }
    fn recovered_block(
        &self,
        _id: BlockHashOrNumber,
        _transaction_kind: TransactionVariant,
    ) -> ProviderResult<Option<reth_primitives_traits::RecoveredBlock<Block>>> {
        Ok(None)
    }
    fn sealed_block_with_senders(
        &self,
        _id: BlockHashOrNumber,
        _transaction_kind: TransactionVariant,
    ) -> ProviderResult<Option<reth_primitives_traits::RecoveredBlock<Block>>> {
        Ok(None)
    }
    fn block_range(
        &self,
        _range: core::ops::RangeInclusive<BlockNumber>,
    ) -> ProviderResult<Vec<Block>> {
        Ok(Vec::new())
    }
    fn block_with_senders_range(
        &self,
        _range: core::ops::RangeInclusive<BlockNumber>,
    ) -> ProviderResult<Vec<reth_primitives_traits::RecoveredBlock<Block>>> {
        Ok(Vec::new())
    }
    fn recovered_block_range(
        &self,
        _range: core::ops::RangeInclusive<BlockNumber>,
    ) -> ProviderResult<Vec<reth_primitives_traits::RecoveredBlock<Block>>> {
        Ok(Vec::new())
    }
    fn block_by_transaction_id(&self, _id: TxNumber) -> ProviderResult<Option<BlockNumber>> {
        Ok(None)
    }
    fn find_block_by_hash(
        &self,
        _hash: alloy_primitives::B256,
        _source: BlockSource,
    ) -> ProviderResult<Option<Block>> {
        Ok(None)
    }
}

impl ReceiptProviderIdExt for LeanProvider {}

impl BlockReaderIdExt for LeanProvider {
    fn block_by_id(&self, _id: BlockId) -> ProviderResult<Option<Block>> {
        Ok(None)
    }
    fn header_by_id(&self, id: BlockId) -> ProviderResult<Option<Header>> {
        // The ONE meaningful call: `new()` asks for the latest header to build
        // an EVM env. A default header (number 0, base fee None) is a correct
        // "empty tip" for intrinsic-gas checks on a fresh lane.
        Ok(match id {
            BlockId::Number(BlockNumberOrTag::Latest | BlockNumberOrTag::Pending) => {
                Some(Header::default())
            }
            _ => None,
        })
    }
    fn sealed_header_by_id(
        &self,
        _id: BlockId,
    ) -> ProviderResult<Option<reth_primitives_traits::SealedHeader<Header>>> {
        Ok(None)
    }
    fn header_by_number_or_tag(
        &self,
        _id: BlockNumberOrTag,
    ) -> ProviderResult<Option<Header>> {
        Ok(None)
    }
    fn sealed_header_by_number_or_tag(
        &self,
        _id: BlockNumberOrTag,
    ) -> ProviderResult<Option<reth_primitives_traits::SealedHeader<Header>>> {
        Ok(None)
    }
}
