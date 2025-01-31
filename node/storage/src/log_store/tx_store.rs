use crate::error::Error;
use crate::log_store::log_manager::{
    data_to_merkle_leaves, sub_merkle_tree, COL_BLOCK_PROGRESS, COL_MISC, COL_TX, COL_TX_COMPLETED,
    COL_TX_DATA_ROOT_INDEX, ENTRY_SIZE, PORA_CHUNK_SIZE,
};
use crate::log_store::metrics;
use crate::{try_option, LogManager, ZgsKeyValueDB};
use anyhow::{anyhow, Result};
use append_merkle::{AppendMerkleTree, MerkleTreeRead, Sha3Algorithm};
use ethereum_types::H256;
use merkle_light::merkle::log2_pow2;
use shared_types::{DataRoot, Transaction};
use ssz::{Decode, Encode};
use std::cmp;
use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tracing::{error, instrument};

const LOG_SYNC_PROGRESS_KEY: &str = "log_sync_progress";
const NEXT_TX_KEY: &str = "next_tx_seq";
const LOG_LATEST_BLOCK_NUMBER_KEY: &str = "log_latest_block_number_key";

#[derive(Debug)]
pub enum TxStatus {
    Finalized,
    Pruned,
}

impl From<TxStatus> for u8 {
    fn from(value: TxStatus) -> Self {
        match value {
            TxStatus::Finalized => 0,
            TxStatus::Pruned => 1,
        }
    }
}

impl TryFrom<u8> for TxStatus {
    type Error = anyhow::Error;

    fn try_from(value: u8) -> std::result::Result<Self, Self::Error> {
        match value {
            0 => Ok(TxStatus::Finalized),
            1 => Ok(TxStatus::Pruned),
            _ => Err(anyhow!("invalid value for tx status {}", value)),
        }
    }
}

#[derive(Clone, Debug)]
pub struct BlockHashAndSubmissionIndex {
    pub block_hash: H256,
    pub first_submission_index: Option<u64>,
}

pub struct TransactionStore {
    flow_kvdb: Arc<dyn ZgsKeyValueDB>,
    data_kvdb: Arc<dyn ZgsKeyValueDB>,
    /// This is always updated before writing the database to ensure no intermediate states.
    next_tx_seq: AtomicU64,
}

impl TransactionStore {
    pub fn new(
        flow_kvdb: Arc<dyn ZgsKeyValueDB>,
        data_kvdb: Arc<dyn ZgsKeyValueDB>,
    ) -> Result<Self> {
        let next_tx_seq = flow_kvdb
            .get(COL_TX, NEXT_TX_KEY.as_bytes())?
            .map(|a| decode_tx_seq(&a))
            .unwrap_or(Ok(0))?;
        Ok(Self {
            flow_kvdb,
            data_kvdb,
            next_tx_seq: AtomicU64::new(next_tx_seq),
        })
    }

    #[instrument(skip(self))]
    /// Return `Ok(Some(tx_seq))` if a previous transaction has the same tx root.
    pub fn put_tx(&self, mut tx: Transaction) -> Result<Vec<u64>> {
        let start_time = Instant::now();

        let old_tx_seq_list = self.get_tx_seq_list_by_data_root(&tx.data_merkle_root)?;
        if old_tx_seq_list.last().is_some_and(|seq| *seq == tx.seq) {
            // The last tx is inserted again, so no need to process it.
            self.next_tx_seq.store(tx.seq + 1, Ordering::SeqCst);
            return Ok(old_tx_seq_list);
        }

        let mut db_tx = self.flow_kvdb.transaction();
        if !tx.data.is_empty() {
            tx.size = tx.data.len() as u64;
            let mut padded_data = tx.data.clone();
            let extra = tx.data.len() % ENTRY_SIZE;
            if extra != 0 {
                padded_data.append(&mut vec![0u8; ENTRY_SIZE - extra]);
            }
            let data_root = sub_merkle_tree(&padded_data)?.root();
            tx.data_merkle_root = data_root.into();
        }

        db_tx.put(COL_TX, &tx.seq.to_be_bytes(), &tx.as_ssz_bytes());
        db_tx.put(COL_TX, NEXT_TX_KEY.as_bytes(), &(tx.seq + 1).to_be_bytes());
        // The list is sorted, and we always call `put_tx` in order.
        assert!(old_tx_seq_list
            .last()
            .map(|last| *last < tx.seq)
            .unwrap_or(true));
        let mut new_tx_seq_list = old_tx_seq_list.clone();
        new_tx_seq_list.push(tx.seq);
        db_tx.put(
            COL_TX_DATA_ROOT_INDEX,
            tx.data_merkle_root.as_bytes(),
            &new_tx_seq_list.as_ssz_bytes(),
        );
        self.next_tx_seq.store(tx.seq + 1, Ordering::SeqCst);
        self.flow_kvdb.write(db_tx)?;
        metrics::TX_STORE_PUT.update_since(start_time);
        Ok(old_tx_seq_list)
    }

    pub fn get_tx_by_seq_number(&self, seq: u64) -> Result<Option<Transaction>> {
        let start_time = Instant::now();
        if seq >= self.next_tx_seq() {
            return Ok(None);
        }
        let value = try_option!(self.flow_kvdb.get(COL_TX, &seq.to_be_bytes())?);
        let tx = Transaction::from_ssz_bytes(&value).map_err(Error::from)?;
        metrics::TX_BY_SEQ_NUMBER.update_since(start_time);
        Ok(Some(tx))
    }

    pub fn remove_tx_after(&self, min_seq: u64) -> Result<Vec<Transaction>> {
        let mut removed_txs = Vec::new();
        let max_seq = self.next_tx_seq();
        let mut flow_db_tx = self.flow_kvdb.transaction();
        let mut data_db_tx = self.data_kvdb.transaction();
        let mut modified_merkle_root_map = HashMap::new();
        for seq in min_seq..max_seq {
            let Some(tx) = self.get_tx_by_seq_number(seq)? else {
                error!(?seq, ?max_seq, "Transaction missing before the end");
                break;
            };
            flow_db_tx.delete(COL_TX, &seq.to_be_bytes());
            data_db_tx.delete(COL_TX_COMPLETED, &seq.to_be_bytes());
            // We only remove tx when the blockchain reorgs.
            // If a tx is reverted, all data after it will also be reverted, so we call remove
            // all indices after it.
            let tx_seq_list = match modified_merkle_root_map.entry(tx.data_merkle_root) {
                Entry::Occupied(e) => e.into_mut(),
                Entry::Vacant(e) => {
                    e.insert(self.get_tx_seq_list_by_data_root(&tx.data_merkle_root)?)
                }
            };
            tx_seq_list.retain(|e| *e < seq);
            removed_txs.push(tx);
        }
        for (merkle_root, tx_seq_list) in modified_merkle_root_map {
            if tx_seq_list.is_empty() {
                flow_db_tx.delete(COL_TX_DATA_ROOT_INDEX, merkle_root.as_bytes());
            } else {
                flow_db_tx.put(
                    COL_TX_DATA_ROOT_INDEX,
                    merkle_root.as_bytes(),
                    &tx_seq_list.as_ssz_bytes(),
                );
            }
        }
        flow_db_tx.put(COL_TX, NEXT_TX_KEY.as_bytes(), &min_seq.to_be_bytes());
        self.next_tx_seq.store(min_seq, Ordering::SeqCst);
        self.data_kvdb.write(data_db_tx)?;
        self.flow_kvdb.write(flow_db_tx)?;
        Ok(removed_txs)
    }

    pub fn get_tx_seq_list_by_data_root(&self, data_root: &DataRoot) -> Result<Vec<u64>> {
        let value = match self
            .flow_kvdb
            .get(COL_TX_DATA_ROOT_INDEX, data_root.as_bytes())?
        {
            Some(v) => v,
            None => return Ok(Vec::new()),
        };
        Ok(Vec::<u64>::from_ssz_bytes(&value).map_err(Error::from)?)
    }

    #[instrument(skip(self))]
    pub fn finalize_tx(&self, tx_seq: u64) -> Result<()> {
        Ok(self.data_kvdb.put(
            COL_TX_COMPLETED,
            &tx_seq.to_be_bytes(),
            &[TxStatus::Finalized.into()],
        )?)
    }

    #[instrument(skip(self))]
    pub fn prune_tx(&self, tx_seq: u64) -> Result<()> {
        Ok(self.data_kvdb.put(
            COL_TX_COMPLETED,
            &tx_seq.to_be_bytes(),
            &[TxStatus::Pruned.into()],
        )?)
    }

    pub fn get_tx_status(&self, tx_seq: u64) -> Result<Option<TxStatus>> {
        let value = try_option!(self
            .data_kvdb
            .get(COL_TX_COMPLETED, &tx_seq.to_be_bytes())?);
        match value.first() {
            Some(v) => Ok(Some(TxStatus::try_from(*v)?)),
            None => Ok(None),
        }
    }

    pub fn check_tx_completed(&self, tx_seq: u64) -> Result<bool> {
        let start_time = Instant::now();
        let status = self.get_tx_status(tx_seq)?;

        metrics::CHECK_TX_COMPLETED.update_since(start_time);
        Ok(matches!(status, Some(TxStatus::Finalized)))
    }

    pub fn check_tx_pruned(&self, tx_seq: u64) -> Result<bool> {
        let status = self.get_tx_status(tx_seq)?;
        Ok(matches!(status, Some(TxStatus::Pruned)))
    }

    pub fn next_tx_seq(&self) -> u64 {
        self.next_tx_seq.load(Ordering::SeqCst)
    }

    #[instrument(skip(self))]
    pub fn put_progress(&self, progress: (u64, H256, Option<Option<u64>>)) -> Result<()> {
        let mut items = vec![(
            COL_MISC,
            LOG_SYNC_PROGRESS_KEY.as_bytes().to_vec(),
            (progress.0, progress.1).as_ssz_bytes(),
        )];

        if let Some(p) = progress.2 {
            items.push((
                COL_BLOCK_PROGRESS,
                progress.0.to_be_bytes().to_vec(),
                (progress.1, p).as_ssz_bytes(),
            ));
        }
        Ok(self.flow_kvdb.puts(items)?)
    }

    #[instrument(skip(self))]
    pub fn get_progress(&self) -> Result<Option<(u64, H256)>> {
        Ok(Some(
            <(u64, H256)>::from_ssz_bytes(&try_option!(self
                .flow_kvdb
                .get(COL_MISC, LOG_SYNC_PROGRESS_KEY.as_bytes())?))
            .map_err(Error::from)?,
        ))
    }

    #[instrument(skip(self))]
    pub fn put_log_latest_block_number(&self, block_number: u64) -> Result<()> {
        Ok(self.flow_kvdb.put(
            COL_MISC,
            LOG_LATEST_BLOCK_NUMBER_KEY.as_bytes(),
            &block_number.as_ssz_bytes(),
        )?)
    }

    #[instrument(skip(self))]
    pub fn get_log_latest_block_number(&self) -> Result<Option<u64>> {
        Ok(Some(
            <u64>::from_ssz_bytes(&try_option!(self
                .flow_kvdb
                .get(COL_MISC, LOG_LATEST_BLOCK_NUMBER_KEY.as_bytes())?))
            .map_err(Error::from)?,
        ))
    }

    pub fn get_block_hash_by_number(
        &self,
        block_number: u64,
    ) -> Result<Option<(H256, Option<u64>)>> {
        Ok(Some(
            <(H256, Option<u64>)>::from_ssz_bytes(&try_option!(self
                .flow_kvdb
                .get(COL_BLOCK_PROGRESS, &block_number.to_be_bytes())?))
            .map_err(Error::from)?,
        ))
    }

    pub fn get_block_hashes(&self) -> Result<Vec<(u64, BlockHashAndSubmissionIndex)>> {
        let mut block_numbers = vec![];
        for r in self.flow_kvdb.iter(COL_BLOCK_PROGRESS) {
            let (key, val) = r?;
            let block_number =
                u64::from_be_bytes(key.as_ref().try_into().map_err(|e| anyhow!("{:?}", e))?);
            let val = <(H256, Option<u64>)>::from_ssz_bytes(val.as_ref()).map_err(Error::from)?;

            block_numbers.push((
                block_number,
                BlockHashAndSubmissionIndex {
                    block_hash: val.0,
                    first_submission_index: val.1,
                },
            ));
        }

        Ok(block_numbers)
    }

    pub fn delete_block_hash_by_number(&self, block_number: u64) -> Result<()> {
        Ok(self
            .flow_kvdb
            .delete(COL_BLOCK_PROGRESS, &block_number.to_be_bytes())?)
    }

    /// Build the merkle tree at `pora_chunk_index` with the data before (including) `tx_seq`.
    /// This first rebuild the tree with the tx root nodes lists by repeatedly checking previous
    /// until we reach the start of this chunk.
    ///
    /// Note that this can only be called with the last chunk after some transaction is committed,
    /// otherwise the start of this chunk might be within some tx subtree and this will panic.
    // TODO(zz): Fill the last chunk with data.
    pub fn rebuild_last_chunk_merkle(
        &self,
        pora_chunk_index: usize,
        mut tx_seq: u64,
    ) -> Result<AppendMerkleTree<H256, Sha3Algorithm>> {
        let last_chunk_start_index = pora_chunk_index as u64 * PORA_CHUNK_SIZE as u64;
        let mut tx_list = Vec::new();
        // Find the first tx within the last chunk.
        loop {
            let tx = self.get_tx_by_seq_number(tx_seq)?.expect("tx not removed");
            match tx.start_entry_index.cmp(&last_chunk_start_index) {
                cmp::Ordering::Greater => {
                    tx_list.push((tx_seq, tx.merkle_nodes));
                    if tx.start_entry_index >= last_chunk_start_index + PORA_CHUNK_SIZE as u64 {
                        break;
                    }
                }
                cmp::Ordering::Equal => {
                    tx_list.push((tx_seq, tx.merkle_nodes));
                    break;
                }
                cmp::Ordering::Less => {
                    // The transaction data crosses a chunk, so we need to find the subtrees
                    // within the last chunk.
                    let mut start_index = tx.start_entry_index;
                    let mut first_index = None;
                    for (i, (depth, _)) in tx.merkle_nodes.iter().enumerate() {
                        start_index += 1 << (depth - 1);
                        if start_index == last_chunk_start_index {
                            first_index = Some(i + 1);
                            break;
                        }
                    }
                    // Some means some subtree ends at the chunk boundary.
                    // None means there are padding data between the tx data and the boundary,
                    // so no data belongs to the last chunk.
                    if let Some(first_index) = first_index {
                        if first_index != tx.merkle_nodes.len() {
                            tx_list.push((tx_seq, tx.merkle_nodes[first_index..].to_vec()));
                        } else {
                            // If the last subtree ends at the chunk boundary, we also do not need
                            // to add data of this tx to the last chunk.
                            // This is only possible if the last chunk is empty, because otherwise
                            // we should have entered the `Equal` condition before and
                            // have broken the loop.
                            assert!(tx_list.is_empty());
                        }
                    }
                    break;
                }
            }
            if tx_seq == 0 {
                break;
            } else {
                tx_seq -= 1;
            }
        }
        let mut merkle = if last_chunk_start_index == 0 {
            // The first entry hash is initialized as zero.
            AppendMerkleTree::<H256, Sha3Algorithm>::new_with_depth(vec![H256::zero()], 1, None)
        } else {
            AppendMerkleTree::<H256, Sha3Algorithm>::new_with_depth(
                vec![],
                log2_pow2(PORA_CHUNK_SIZE) + 1,
                None,
            )
        };
        for (tx_seq, subtree_list) in tx_list.into_iter().rev() {
            // Pad the tx. After the first subtree is padded, other subtrees should be aligned.
            let first_subtree = 1 << (subtree_list[0].0 - 1);
            if merkle.leaves() % first_subtree != 0 {
                let pad_len =
                    cmp::min(first_subtree, PORA_CHUNK_SIZE) - (merkle.leaves() % first_subtree);
                merkle.append_list(data_to_merkle_leaves(&LogManager::padding_raw(pad_len))?);
            }
            // Since we are building the last merkle with a given last tx_seq, it's ensured
            // that appending subtrees will not go beyond the max size.
            merkle.append_subtree_list(subtree_list)?;
            merkle.commit(Some(tx_seq));
        }
        Ok(merkle)
    }
}

fn decode_tx_seq(data: &[u8]) -> Result<u64> {
    Ok(u64::from_be_bytes(
        data.try_into().map_err(|e| anyhow!("{:?}", e))?,
    ))
}
