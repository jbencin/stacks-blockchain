// Copyright (C) 2013-2020 Blockstack PBC, a public benefit corporation
// Copyright (C) 2020 Stacks Open Internet Foundation
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.

use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::ThreadId;
use std::{cmp, fs, mem};

use clarity::vm::analysis::{CheckError, CheckErrors};
use clarity::vm::ast::errors::ParseErrors;
use clarity::vm::ast::ASTRules;
use clarity::vm::clarity::TransactionConnection;
use clarity::vm::costs::ExecutionCost;
use clarity::vm::database::BurnStateDB;
use clarity::vm::errors::Error as InterpreterError;
use clarity::vm::types::TypeSignature;
use serde::Deserialize;
use stacks_common::codec::{
    read_next, write_next, Error as CodecError, StacksMessageCodec, MAX_PAYLOAD_LEN,
};
use stacks_common::types::chainstate::{
    BlockHeaderHash, BurnchainHeaderHash, ConsensusHash, StacksAddress, StacksBlockId, TrieHash,
};
use stacks_common::types::StacksPublicKeyBuffer;
use stacks_common::util::get_epoch_time_ms;
use stacks_common::util::hash::{Hash160, MerkleTree, Sha512Trunc256Sum};
use stacks_common::util::secp256k1::{MessageSignature, Secp256k1PrivateKey};

use crate::burnchains::{PrivateKey, PublicKey};
use crate::chainstate::burn::db::sortdb::{SortitionDB, SortitionDBConn, SortitionHandleTx};
use crate::chainstate::burn::operations::*;
use crate::chainstate::burn::*;
use crate::chainstate::nakamoto::{
    MaturedMinerRewards, NakamotoBlock, NakamotoBlockHeader, NakamotoChainState, SetupBlockResult,
};
use crate::chainstate::stacks::address::StacksAddressExtensions;
use crate::chainstate::stacks::db::accounts::MinerReward;
use crate::chainstate::stacks::db::blocks::MemPoolRejection;
use crate::chainstate::stacks::db::transactions::{
    handle_clarity_runtime_error, ClarityRuntimeTxError,
};
use crate::chainstate::stacks::db::{
    ChainstateTx, ClarityTx, MinerRewardInfo, StacksChainState, StacksHeaderInfo,
    MINER_REWARD_MATURITY,
};
use crate::chainstate::stacks::events::{StacksTransactionEvent, StacksTransactionReceipt};
use crate::chainstate::stacks::miner::{
    BlockBuilder, BlockBuilderSettings, BlockLimitFunction, TransactionError,
    TransactionProblematic, TransactionResult, TransactionSkipped,
};
use crate::chainstate::stacks::{Error, StacksBlockHeader, *};
use crate::clarity_vm::clarity::{ClarityConnection, ClarityInstance};
use crate::core::mempool::*;
use crate::core::*;
use crate::cost_estimates::metrics::CostMetric;
use crate::cost_estimates::CostEstimator;
use crate::monitoring::{
    set_last_mined_block_transaction_count, set_last_mined_execution_cost_observed,
};
use crate::net::relay::Relayer;
use crate::util_lib::db::Error as DBError;

/// Nakamaoto tenure information
pub struct NakamotoTenureInfo {
    /// Coinbase tx, if this is a new tenure
    pub coinbase_tx: Option<StacksTransaction>,
    /// Tenure change transaction from Stackers
    pub tenure_change_tx: Option<StacksTransaction>,
}

impl NakamotoTenureInfo {
    pub fn cause(&self) -> Option<TenureChangeCause> {
        self.tenure_change_tx
            .as_ref()
            .map(|tx| tx.try_as_tenure_change().map(|payload| payload.cause))
            .flatten()
    }

    pub fn tenure_change_tx(&self) -> Option<&StacksTransaction> {
        self.tenure_change_tx.as_ref()
    }

    pub fn coinbase_tx(&self) -> Option<&StacksTransaction> {
        self.coinbase_tx.as_ref()
    }
}

pub struct NakamotoBlockBuilder {
    /// if this is building atop an epoch 2 block, then this is that block's header
    epoch2_parent_header: Option<(StacksBlockHeader, ConsensusHash)>,
    /// if this is building atop an epoch 3 block, then this is that block's header
    nakamoto_parent_header: Option<NakamotoBlockHeader>,
    /// Signed coinbase tx, if starting a new tenure
    coinbase_tx: Option<StacksTransaction>,
    /// Tenure change tx, if starting or extending a tenure
    tenure_tx: Option<StacksTransaction>,
    /// Total burn this block represents
    total_burn: u64,
    /// parent block-commit hash value
    parent_commit_hash_value: BlockHeaderHash,
    /// Matured miner rewards to process, if any.
    matured_miner_rewards_opt: Option<MaturedMinerRewards>,
    /// bytes of space consumed so far
    bytes_so_far: u64,
    /// transactions selected
    txs: Vec<StacksTransaction>,
    /// header we're filling in
    header: NakamotoBlockHeader,
}

pub struct MinerTenureInfo<'a> {
    pub chainstate_tx: ChainstateTx<'a>,
    pub clarity_instance: &'a mut ClarityInstance,
    pub burn_tip: BurnchainHeaderHash,
    /// This is the expected burn tip height (i.e., the current burnchain tip + 1)
    ///  of the mined block
    pub burn_tip_height: u32,
    pub mainnet: bool,
    pub parent_consensus_hash: ConsensusHash,
    pub parent_header_hash: BlockHeaderHash,
    pub parent_stacks_block_height: u64,
    pub parent_burn_block_height: u32,
    pub coinbase_height: u64,
    pub cause: Option<TenureChangeCause>,
}

impl NakamotoBlockBuilder {
    /// Make a block builder atop a Nakamoto parent for a new tenure
    pub fn new_tenure_from_nakamoto_parent(
        parent_tenure_id: &StacksBlockId,
        parent: &NakamotoBlockHeader,
        tenure_id_consensus_hash: &ConsensusHash,
        total_burn: u64,
        tenure_change: &StacksTransaction,
        coinbase: &StacksTransaction,
    ) -> NakamotoBlockBuilder {
        let parent_commit_hash_value = BlockHeaderHash(parent_tenure_id.0.clone());
        NakamotoBlockBuilder {
            epoch2_parent_header: None,
            nakamoto_parent_header: Some(parent.clone()),
            total_burn,
            coinbase_tx: Some(coinbase.clone()),
            tenure_tx: Some(tenure_change.clone()),
            parent_commit_hash_value,
            matured_miner_rewards_opt: None,
            bytes_so_far: 0,
            txs: vec![],
            header: NakamotoBlockHeader::from_parent_empty(
                parent.chain_length + 1,
                total_burn,
                tenure_id_consensus_hash.clone(),
                parent.block_id(),
            ),
        }
    }

    /// Make a block builder atop a Nakamoto parent for a new block within a tenure
    pub fn continue_tenure_from_nakamoto_parent(
        parent: &NakamotoBlockHeader,
        tenure_id_consensus_hash: &ConsensusHash,
        total_burn: u64,
        tenure_extend: Option<&StacksTransaction>,
    ) -> NakamotoBlockBuilder {
        let parent_commit_hash_value = BlockHeaderHash(parent.block_id().0.clone());
        NakamotoBlockBuilder {
            epoch2_parent_header: None,
            nakamoto_parent_header: Some(parent.clone()),
            total_burn,
            coinbase_tx: None,
            tenure_tx: tenure_extend.cloned(),
            parent_commit_hash_value,
            matured_miner_rewards_opt: None,
            bytes_so_far: 0,
            txs: vec![],
            header: NakamotoBlockHeader::from_parent_empty(
                parent.chain_length + 1,
                total_burn,
                tenure_id_consensus_hash.clone(),
                parent.block_id(),
            ),
        }
    }

    /// Make a block builder atop an epoch 2 parent for a new tenure
    pub fn new_tenure_from_epoch2_parent(
        parent: &StacksBlockHeader,
        parent_tenure_id_consensus_hash: &ConsensusHash,
        tenure_id_consensus_hash: &ConsensusHash,
        total_burn: u64,
        tenure_change: &StacksTransaction,
        coinbase: &StacksTransaction,
    ) -> NakamotoBlockBuilder {
        NakamotoBlockBuilder {
            epoch2_parent_header: Some((parent.clone(), parent_tenure_id_consensus_hash.clone())),
            nakamoto_parent_header: None,
            total_burn,
            coinbase_tx: Some(coinbase.clone()),
            tenure_tx: Some(tenure_change.clone()),
            parent_commit_hash_value: parent.block_hash(),
            matured_miner_rewards_opt: None,
            bytes_so_far: 0,
            txs: vec![],
            header: NakamotoBlockHeader::from_parent_empty(
                parent.total_work.work + 1,
                total_burn,
                tenure_id_consensus_hash.clone(),
                StacksBlockId::new(parent_tenure_id_consensus_hash, &parent.block_hash()),
            ),
        }
    }

    /// Make a block builder from genesis (testing only)
    pub fn new_tenure_from_genesis(
        tenure_change: &StacksTransaction,
        coinbase: &StacksTransaction,
    ) -> NakamotoBlockBuilder {
        NakamotoBlockBuilder {
            epoch2_parent_header: None,
            nakamoto_parent_header: None,
            total_burn: 0,
            coinbase_tx: Some(coinbase.clone()),
            tenure_tx: Some(tenure_change.clone()),
            parent_commit_hash_value: FIRST_STACKS_BLOCK_HASH.clone(),
            matured_miner_rewards_opt: None,
            bytes_so_far: 0,
            txs: vec![],
            header: NakamotoBlockHeader::genesis(),
        }
    }

    /// Make a Nakamoto block builder appropriate for building atop the given block header
    pub fn new_from_parent(
        // tenure ID -- this is the index block hash of the start block of the last tenure (i.e.
        // the data we committed to in the block-commit).  If this is an epoch 2.x parent, then
        // this is just the index block hash of the parent Stacks block.
        parent_tenure_id: &StacksBlockId,
        // Stacks header we're building off of.
        parent_stacks_header: &StacksHeaderInfo,
        // consensus hash of this tenure's burnchain block. This is the consensus hash that goes
        // into the block header.
        tenure_id_consensus_hash: &ConsensusHash,
        // total BTC burn so far
        total_burn: u64,
        // tenure change, if we're starting or extending a tenure
        tenure_change: Option<&StacksTransaction>,
        // coinbase, if we're starting a new tenure
        coinbase: Option<&StacksTransaction>,
    ) -> Result<NakamotoBlockBuilder, Error> {
        let builder = if let Some(parent_nakamoto_header) =
            parent_stacks_header.anchored_header.as_stacks_nakamoto()
        {
            // building atop a nakamoto block
            // new tenure?
            if coinbase.is_some() && tenure_change.is_some() {
                NakamotoBlockBuilder::new_tenure_from_nakamoto_parent(
                    parent_tenure_id,
                    parent_nakamoto_header,
                    tenure_id_consensus_hash,
                    total_burn,
                    tenure_change.ok_or(Error::ExpectedTenureChange)?,
                    coinbase.ok_or(Error::ExpectedTenureChange)?,
                )
            } else {
                NakamotoBlockBuilder::continue_tenure_from_nakamoto_parent(
                    parent_nakamoto_header,
                    tenure_id_consensus_hash,
                    total_burn,
                    tenure_change,
                )
            }
        } else if let Some(parent_epoch2_header) =
            parent_stacks_header.anchored_header.as_stacks_epoch2()
        {
            // building atop a stacks 2.x block.
            // we are necessarily starting a new tenure
            if tenure_change.is_some() && coinbase.is_some() {
                NakamotoBlockBuilder::new_tenure_from_epoch2_parent(
                    parent_epoch2_header,
                    &parent_stacks_header.consensus_hash,
                    tenure_id_consensus_hash,
                    total_burn,
                    tenure_change.ok_or(Error::ExpectedTenureChange)?,
                    coinbase.ok_or(Error::ExpectedTenureChange)?,
                )
            } else {
                // not allowed
                warn!("Failed to start a Nakamoto tenure atop a Stacks 2.x block -- missing a coinbase and/or tenure");
                return Err(Error::ExpectedTenureChange);
            }
        } else {
            // not reachable -- no other choices
            return Err(Error::InvalidStacksBlock(
                "Parent is neither a Nakamoto block nor a Stacks 2.x block".into(),
            ));
        };

        Ok(builder)
    }

    /// This function should be called before `tenure_begin`.
    /// It creates a MinerTenureInfo struct which owns connections to the chainstate and sortition
    /// DBs, so that block-processing is guaranteed to terminate before the lives of these handles
    /// expire.
    pub fn load_tenure_info<'a>(
        &self,
        chainstate: &'a mut StacksChainState,
        burn_dbconn: &'a SortitionDBConn,
        cause: Option<TenureChangeCause>,
    ) -> Result<MinerTenureInfo<'a>, Error> {
        debug!("Nakamoto miner tenure begin");

        let burn_tip = SortitionDB::get_canonical_chain_tip_bhh(burn_dbconn.conn())?;
        let burn_tip_height = u32::try_from(
            SortitionDB::get_canonical_burn_chain_tip(burn_dbconn.conn())?.block_height,
        )
        .expect("block height overflow");

        let mainnet = chainstate.config().mainnet;

        let (chain_tip, parent_tenure_id_consensus_hash, parent_header_hash) =
            if let Some(nakamoto_parent_header) = self.nakamoto_parent_header.as_ref() {
                // parent is a nakamoto block
                let parent_header_info = NakamotoChainState::get_block_header(
                    chainstate.db(),
                    &StacksBlockId::new(
                        &nakamoto_parent_header.consensus_hash,
                        &nakamoto_parent_header.block_hash(),
                    ),
                )?
                .ok_or(Error::NoSuchBlockError)
                .map_err(|e| {
                    warn!(
                        "No such Nakamoto parent block {}/{} ({})",
                        &nakamoto_parent_header.consensus_hash,
                        &nakamoto_parent_header.block_hash(),
                        &nakamoto_parent_header.block_id()
                    );
                    e
                })?;

                (
                    parent_header_info,
                    nakamoto_parent_header.consensus_hash.clone(),
                    nakamoto_parent_header.block_hash(),
                )
            } else if let Some((stacks_header, consensus_hash)) = self.epoch2_parent_header.as_ref()
            {
                // parent is a Stacks epoch2 block
                let parent_header_info = NakamotoChainState::get_block_header(
                    chainstate.db(),
                    &StacksBlockId::new(consensus_hash, &stacks_header.block_hash()),
                )?
                .ok_or(Error::NoSuchBlockError)
                .map_err(|e| {
                    warn!(
                        "No such Stacks 2.x parent block {}/{} ({})",
                        &consensus_hash,
                        &stacks_header.block_hash(),
                        &StacksBlockId::new(&consensus_hash, &stacks_header.block_hash())
                    );
                    e
                })?;

                (
                    parent_header_info,
                    consensus_hash.clone(),
                    stacks_header.block_hash(),
                )
            } else {
                // parent is genesis (testing only)
                (
                    StacksHeaderInfo::regtest_genesis(),
                    FIRST_BURNCHAIN_CONSENSUS_HASH.clone(),
                    FIRST_STACKS_BLOCK_HASH.clone(),
                )
            };

        let coinbase_height = if let Ok(Some(parent_coinbase_height)) =
            NakamotoChainState::get_coinbase_height(
                chainstate.db(),
                &StacksBlockId::new(&parent_tenure_id_consensus_hash, &parent_header_hash),
            ) {
            parent_coinbase_height
                .checked_add(1)
                .expect("Blockchain overflow")
        } else {
            0
        };

        // data won't be committed, so do a concurrent transaction
        let (chainstate_tx, clarity_instance) = chainstate.chainstate_tx_begin()?;

        Ok(MinerTenureInfo {
            chainstate_tx,
            clarity_instance,
            burn_tip,
            burn_tip_height,
            mainnet,
            parent_consensus_hash: parent_tenure_id_consensus_hash,
            parent_header_hash,
            parent_stacks_block_height: chain_tip.stacks_block_height,
            parent_burn_block_height: chain_tip.burn_header_height,
            cause,
            coinbase_height,
        })
    }

    /// Begin/resume mining a tenure's transactions.
    /// Returns an open ClarityTx for mining the block.
    /// NOTE: even though we don't yet know the block hash, the Clarity VM ensures that a
    /// transaction can't query information about the _current_ block (i.e. information that is not
    /// yet known).
    pub fn tenure_begin<'a, 'b>(
        &mut self,
        burn_dbconn: &'a SortitionDBConn,
        info: &'b mut MinerTenureInfo<'a>,
    ) -> Result<ClarityTx<'b, 'b>, Error> {
        let SetupBlockResult {
            clarity_tx,
            matured_miner_rewards_opt,
            ..
        } = NakamotoChainState::setup_block(
            &mut info.chainstate_tx,
            info.clarity_instance,
            burn_dbconn,
            &burn_dbconn.context.pox_constants,
            info.parent_consensus_hash,
            info.parent_header_hash,
            info.parent_stacks_block_height,
            info.parent_burn_block_height,
            info.burn_tip,
            info.burn_tip_height,
            info.cause == Some(TenureChangeCause::BlockFound),
            info.coinbase_height,
            info.cause == Some(TenureChangeCause::Extended),
        )?;
        self.matured_miner_rewards_opt = matured_miner_rewards_opt;
        Ok(clarity_tx)
    }

    /// Finish up mining an epoch's transactions.
    /// Return the ExecutionCost consumed so far.
    pub fn tenure_finish(self, tx: ClarityTx) -> ExecutionCost {
        let new_consensus_hash = MINER_BLOCK_CONSENSUS_HASH.clone();
        let new_block_hash = MINER_BLOCK_HEADER_HASH.clone();

        let index_block_hash =
            StacksBlockHeader::make_index_block_hash(&new_consensus_hash, &new_block_hash);

        // write out the trie...
        let consumed = tx.commit_mined_block(&index_block_hash);

        test_debug!("\n\nFinished mining. Trie is in mined_blocks table.\n",);

        consumed
    }

    /// Finish constructing a Nakamoto block.
    /// The block will not be signed yet.
    /// Returns the unsigned Nakamoto block
    fn finalize_block(&mut self, clarity_tx: &mut ClarityTx) -> NakamotoBlock {
        // done!  Calculate state root and tx merkle root
        let txid_vecs = self
            .txs
            .iter()
            .map(|tx| tx.txid().as_bytes().to_vec())
            .collect();

        let merkle_tree = MerkleTree::<Sha512Trunc256Sum>::new(&txid_vecs);
        let tx_merkle_root = merkle_tree.root();
        let state_root_hash = clarity_tx.seal();

        self.header.tx_merkle_root = tx_merkle_root;
        self.header.state_index_root = state_root_hash;

        let block = NakamotoBlock {
            header: self.header.clone(),
            txs: self.txs.clone(),
        };

        test_debug!(
            "\n\nMined Nakamoto block {}, {} transactions, state root is {}\n",
            block.header.block_hash(),
            block.txs.len(),
            state_root_hash
        );

        debug!(
            "Miner: mined Nakamoto block";
            "consensus_hash" => %block.header.consensus_hash,
            "block_hash" => %block.header.block_hash(),
            "block_height" => block.header.chain_length,
            "num_txs" => block.txs.len(),
            "parent_block" => %block.header.parent_block_id,
            "state_root" => %state_root_hash
        );

        block
    }

    /// Finish building the Nakamoto block
    pub fn mine_nakamoto_block(&mut self, clarity_tx: &mut ClarityTx) -> NakamotoBlock {
        NakamotoChainState::finish_block(clarity_tx, self.matured_miner_rewards_opt.as_ref())
            .expect("FATAL: call to `finish_block` failed");
        self.finalize_block(clarity_tx)
    }

    /// Given access to the mempool, mine a nakamoto block.
    /// It will not be signed.
    pub fn build_nakamoto_block(
        // not directly used; used as a handle to open other chainstates
        chainstate_handle: &StacksChainState,
        burn_dbconn: &SortitionDBConn,
        mempool: &mut MemPoolDB,
        // tenure ID -- this is the index block hash of the start block of the last tenure (i.e.
        // the data we committed to in the block-commit)
        parent_tenure_id: &StacksBlockId,
        // Stacks header we're building off of.
        parent_stacks_header: &StacksHeaderInfo,
        // tenure ID consensus hash of this block
        tenure_id_consensus_hash: &ConsensusHash,
        // the burn so far on the burnchain (i.e. from the last burnchain block)
        total_burn: u64,
        tenure_info: NakamotoTenureInfo,
        settings: BlockBuilderSettings,
        event_observer: Option<&dyn MemPoolEventDispatcher>,
    ) -> Result<(NakamotoBlock, ExecutionCost, u64), Error> {
        let (tip_consensus_hash, tip_block_hash, tip_height) = (
            parent_stacks_header.consensus_hash.clone(),
            parent_stacks_header.anchored_header.block_hash(),
            parent_stacks_header.stacks_block_height,
        );

        debug!(
            "Build Nakamoto block off of {}/{} height {}",
            &tip_consensus_hash, &tip_block_hash, tip_height
        );

        let (mut chainstate, _) = chainstate_handle.reopen()?;

        let mut builder = NakamotoBlockBuilder::new_from_parent(
            parent_tenure_id,
            parent_stacks_header,
            tenure_id_consensus_hash,
            total_burn,
            tenure_info.tenure_change_tx(),
            tenure_info.coinbase_tx(),
        )?;

        let ts_start = get_epoch_time_ms();

        let mut miner_tenure_info =
            builder.load_tenure_info(&mut chainstate, burn_dbconn, tenure_info.cause())?;
        let mut tenure_tx = builder.tenure_begin(burn_dbconn, &mut miner_tenure_info)?;

        let block_limit = tenure_tx
            .block_limit()
            .expect("Failed to obtain block limit from miner's block connection");

        let initial_txs: Vec<_> = [
            tenure_info.tenure_change_tx.clone(),
            tenure_info.coinbase_tx.clone(),
        ]
        .into_iter()
        .filter_map(|x| x)
        .collect();
        let (blocked, tx_events) = match StacksBlockBuilder::select_and_apply_transactions(
            &mut tenure_tx,
            &mut builder,
            mempool,
            parent_stacks_header.stacks_block_height,
            &initial_txs,
            settings,
            event_observer,
            ASTRules::PrecheckSize,
        ) {
            Ok(x) => x,
            Err(e) => {
                warn!("Failure building block: {}", e);
                tenure_tx.rollback_block();
                return Err(e);
            }
        };

        if blocked {
            debug!(
                "Miner: block transaction selection aborted (child of {})",
                &parent_stacks_header.anchored_header.block_hash()
            );
            return Err(Error::MinerAborted);
        }

        // save the block so we can build microblocks off of it
        let block = builder.mine_nakamoto_block(&mut tenure_tx);
        let size = builder.bytes_so_far;
        let consumed = builder.tenure_finish(tenure_tx);

        let ts_end = get_epoch_time_ms();

        if let Some(observer) = event_observer {
            observer.mined_nakamoto_block_event(
                SortitionDB::get_canonical_burn_chain_tip(burn_dbconn.conn())?.block_height + 1,
                &block,
                size,
                &consumed,
                tx_events,
            );
        }

        set_last_mined_block_transaction_count(block.txs.len() as u64);
        set_last_mined_execution_cost_observed(&consumed, &block_limit);

        info!(
            "Miner: mined Nakamoto block";
            "block_hash" => %block.header.block_hash(),
            "block_id" => %block.header.block_id(),
            "height" => block.header.chain_length,
            "tx_count" => block.txs.len(),
            "parent_block_id" => %block.header.parent_block_id,
            "block_size" => size,
            "execution_consumed" => %consumed,
            "%-full" => block_limit.proportion_largest_dimension(&consumed),
            "assembly_time_ms" => ts_end.saturating_sub(ts_start),
        );

        Ok((block, consumed, size))
    }

    #[cfg(test)]
    pub fn make_nakamoto_block_from_txs(
        mut self,
        chainstate_handle: &StacksChainState,
        burn_dbconn: &SortitionDBConn,
        mut txs: Vec<StacksTransaction>,
    ) -> Result<(NakamotoBlock, u64, ExecutionCost), Error> {
        debug!("Build Nakamoto block from {} transactions", txs.len());
        let (mut chainstate, _) = chainstate_handle.reopen()?;

        let tenure_cause = txs.iter().find_map(|tx| match &tx.payload {
            TransactionPayload::TenureChange(tc) => Some(tc.cause),
            _ => None,
        });

        let mut miner_tenure_info =
            self.load_tenure_info(&mut chainstate, burn_dbconn, tenure_cause)?;
        let mut tenure_tx = self.tenure_begin(burn_dbconn, &mut miner_tenure_info)?;
        for tx in txs.drain(..) {
            let tx_len = tx.tx_len();
            match self.try_mine_tx_with_len(
                &mut tenure_tx,
                &tx,
                tx_len,
                &BlockLimitFunction::NO_LIMIT_HIT,
                ASTRules::PrecheckSize,
            ) {
                TransactionResult::Success(..) => {
                    debug!("Included {}", &tx.txid());
                }
                TransactionResult::Skipped(TransactionSkipped { error, .. })
                | TransactionResult::ProcessingError(TransactionError { error, .. }) => {
                    match error {
                        Error::BlockTooBigError => {
                            // done mining -- our execution budget is exceeded.
                            // Make the block from the transactions we did manage to get
                            debug!("Block budget exceeded on tx {}", &tx.txid());
                        }
                        Error::InvalidStacksTransaction(_emsg, true) => {
                            // if we have an invalid transaction that was quietly ignored, don't warn here either
                            test_debug!(
                                "Failed to apply tx {}: InvalidStacksTransaction '{:?}'",
                                &tx.txid(),
                                &_emsg
                            );
                            continue;
                        }
                        Error::ProblematicTransaction(txid) => {
                            test_debug!("Encountered problematic transaction. Aborting");
                            return Err(Error::ProblematicTransaction(txid));
                        }
                        e => {
                            warn!("Failed to apply tx {}: {:?}", &tx.txid(), &e);
                            continue;
                        }
                    }
                }
                TransactionResult::Problematic(TransactionProblematic { tx, .. }) => {
                    // drop from the mempool
                    debug!("Encountered problematic transaction {}", &tx.txid());
                    return Err(Error::ProblematicTransaction(tx.txid()));
                }
            }
        }
        let block = self.mine_nakamoto_block(&mut tenure_tx);
        let size = self.bytes_so_far;
        let cost = self.tenure_finish(tenure_tx);
        Ok((block, size, cost))
    }

    pub fn get_bytes_so_far(&self) -> u64 {
        self.bytes_so_far
    }
}

impl BlockBuilder for NakamotoBlockBuilder {
    /// Append a transaction if doing so won't exceed the epoch data size.
    /// Errors out if we exceed budget, or the transaction is invalid.
    fn try_mine_tx_with_len(
        &mut self,
        clarity_tx: &mut ClarityTx,
        tx: &StacksTransaction,
        tx_len: u64,
        limit_behavior: &BlockLimitFunction,
        ast_rules: ASTRules,
    ) -> TransactionResult {
        if self.bytes_so_far + tx_len >= MAX_EPOCH_SIZE.into() {
            return TransactionResult::skipped_due_to_error(&tx, Error::BlockTooBigError);
        }

        match limit_behavior {
            BlockLimitFunction::CONTRACT_LIMIT_HIT => {
                match &tx.payload {
                    TransactionPayload::ContractCall(cc) => {
                        // once we've hit the runtime limit once, allow boot code contract calls, but do not try to eval
                        //   other contract calls
                        if !cc.address.is_boot_code_addr() {
                            return TransactionResult::skipped(
                                &tx,
                                "BlockLimitFunction::CONTRACT_LIMIT_HIT".to_string(),
                            );
                        }
                    }
                    TransactionPayload::SmartContract(..) => {
                        return TransactionResult::skipped(
                            &tx,
                            "BlockLimitFunction::CONTRACT_LIMIT_HIT".to_string(),
                        );
                    }
                    _ => {}
                }
            }
            BlockLimitFunction::LIMIT_REACHED => {
                return TransactionResult::skipped(
                    &tx,
                    "BlockLimitFunction::LIMIT_REACHED".to_string(),
                )
            }
            BlockLimitFunction::NO_LIMIT_HIT => {}
        };

        let quiet = !cfg!(test);
        let result = {
            // preemptively skip problematic transactions
            if let Err(e) = Relayer::static_check_problematic_relayed_tx(
                clarity_tx.config.mainnet,
                clarity_tx.get_epoch(),
                &tx,
                ast_rules,
            ) {
                info!(
                    "Detected problematic tx {} while mining; dropping from mempool",
                    tx.txid()
                );
                return TransactionResult::problematic(&tx, Error::NetError(e));
            }
            let (fee, receipt) = match StacksChainState::process_transaction(
                clarity_tx, tx, quiet, ast_rules,
            ) {
                Ok((fee, receipt)) => (fee, receipt),
                Err(e) => {
                    let (is_problematic, e) =
                        TransactionResult::is_problematic(&tx, e, clarity_tx.get_epoch());
                    if is_problematic {
                        return TransactionResult::problematic(&tx, e);
                    } else {
                        match e {
                            Error::CostOverflowError(cost_before, cost_after, total_budget) => {
                                clarity_tx.reset_cost(cost_before.clone());
                                if total_budget.proportion_largest_dimension(&cost_before)
                                    < TX_BLOCK_LIMIT_PROPORTION_HEURISTIC
                                {
                                    warn!(
                                            "Transaction {} consumed over {}% of block budget, marking as invalid; budget was {}",
                                            tx.txid(),
                                            100 - TX_BLOCK_LIMIT_PROPORTION_HEURISTIC,
                                            &total_budget
                                        );
                                    return TransactionResult::error(
                                        &tx,
                                        Error::TransactionTooBigError,
                                    );
                                } else {
                                    warn!(
                                        "Transaction {} reached block cost {}; budget was {}",
                                        tx.txid(),
                                        &cost_after,
                                        &total_budget
                                    );
                                    return TransactionResult::skipped_due_to_error(
                                        &tx,
                                        Error::BlockTooBigError,
                                    );
                                }
                            }
                            _ => return TransactionResult::error(&tx, e),
                        }
                    }
                }
            };
            info!("Include tx";
                  "tx" => %tx.txid(),
                  "payload" => tx.payload.name(),
                  "origin" => %tx.origin_address());

            // save
            self.txs.push(tx.clone());
            TransactionResult::success(&tx, fee, receipt)
        };

        self.bytes_so_far += tx_len;
        result
    }
}

/// This enum is used to supply a `reason_code` for validation
///  rejection responses. This is serialized as an enum with string
///  type (in jsonschema terminology).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ValidateRejectCode {
    BadBlockHash,
    BadTransaction,
    InvalidBlock,
    ChainstateError,
    UnknownParent,
}

/// A response for block proposal validation
///  that the stacks-node thinks should be rejected.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BlockValidateReject {
    pub reason: String,
    pub reason_code: ValidateRejectCode,
}

/// A response for block proposal validation
///  that the stacks-node thinks is acceptable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BlockValidateOk {
    pub block: NakamotoBlock,
    pub cost: ExecutionCost,
    pub size: u64,
}

/// This enum is used for serializing the response to block
/// proposal validation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "Result")]
pub enum BlockValidateResponse {
    Ok(BlockValidateOk),
    Reject(BlockValidateReject),
}

impl From<Result<BlockValidateOk, BlockValidateReject>> for BlockValidateResponse {
    fn from(value: Result<BlockValidateOk, BlockValidateReject>) -> Self {
        match value {
            Ok(o) => BlockValidateResponse::Ok(o),
            Err(e) => BlockValidateResponse::Reject(e),
        }
    }
}

impl From<Error> for BlockValidateReject {
    fn from(value: Error) -> Self {
        BlockValidateReject {
            reason: format!("Chainstate Error: {value}"),
            reason_code: ValidateRejectCode::ChainstateError,
        }
    }
}

impl From<DBError> for BlockValidateReject {
    fn from(value: DBError) -> Self {
        Error::from(value).into()
    }
}

/// Represents a block proposed to the `v2/block_proposal` endpoint for validation
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NakamotoBlockProposal {
    /// Proposed block
    pub block: NakamotoBlock,
    // tenure ID -- this is the index block hash of the start block of the last tenure (i.e.
    // the data we committed to in the block-commit).  If this is an epoch 2.x parent, then
    // this is just the index block hash of the parent Stacks block.
    pub tenure_start_block: StacksBlockId,
    /// Identifies which chain block is for (Mainnet, Testnet, etc.)
    pub chain_id: u32,
}

impl StacksMessageCodec for NakamotoBlockProposal {
    fn consensus_serialize<W: Write>(&self, fd: &mut W) -> Result<(), CodecError> {
        write_next(fd, &self.block)?;
        write_next(fd, &self.tenure_start_block)?;
        write_next(fd, &self.chain_id)
    }

    fn consensus_deserialize<R: Read>(fd: &mut R) -> Result<Self, CodecError> {
        Ok(Self {
            block: read_next(fd)?,
            tenure_start_block: read_next(fd)?,
            chain_id: read_next(fd)?,
        })
    }
}

impl NakamotoBlockProposal {
    /// Test this block proposal against the current chain state and
    /// either accept or reject the proposal
    ///
    /// This is done in 2 steps:
    /// - Static validation of the block, which checks the following:
    ///   - Block is well-formed
    ///   - Transactions are well-formed
    ///   - Miner signature is valid
    /// - Validation of transactions by executing them agains current chainstate.
    ///   This is resource intensive, and therefore done only if previous checks pass
    pub fn validate(
        &self,
        sortdb: &SortitionDB,
        chainstate: &mut StacksChainState, // not directly used; used as a handle to open other chainstates
    ) -> Result<BlockValidateOk, BlockValidateReject> {
        let ts_start = get_epoch_time_ms();
        // Measure time from start of function
        let time_elapsed = || get_epoch_time_ms().saturating_sub(ts_start);

        let mainnet = self.chain_id == CHAIN_ID_MAINNET;
        if self.chain_id != chainstate.chain_id || mainnet != chainstate.mainnet {
            return Err(BlockValidateReject {
                reason_code: ValidateRejectCode::InvalidBlock,
                reason: "Wrong netowrk/chain_id".into(),
            });
        }

        let burn_dbconn = sortdb.index_conn();
        let sort_tip = SortitionDB::get_canonical_sortition_tip(sortdb.conn())?;
        let mut db_handle = sortdb.index_handle(&sort_tip);
        let expected_burn =
            NakamotoChainState::get_expected_burns(&mut db_handle, chainstate.db(), &self.block)?;

        // Static validation checks
        NakamotoChainState::validate_nakamoto_block_burnchain(
            &db_handle,
            expected_burn,
            &self.block,
            mainnet,
            self.chain_id,
        )?;

        // Validate block txs against chainstate
        let parent_stacks_header = NakamotoChainState::get_block_header(
            chainstate.db(),
            &self.block.header.parent_block_id,
        )?
        .ok_or_else(|| BlockValidateReject {
            reason_code: ValidateRejectCode::InvalidBlock,
            reason: "Invalid parent block".into(),
        })?;
        let tenure_change = self
            .block
            .txs
            .iter()
            .find(|tx| matches!(tx.payload, TransactionPayload::TenureChange(..)));
        let coinbase = self
            .block
            .txs
            .iter()
            .find(|tx| matches!(tx.payload, TransactionPayload::Coinbase(..)));
        let tenure_cause = tenure_change.and_then(|tx| match &tx.payload {
            TransactionPayload::TenureChange(tc) => Some(tc.cause),
            _ => None,
        });

        let mut builder = NakamotoBlockBuilder::new_from_parent(
            &self.tenure_start_block,
            &parent_stacks_header,
            &self.block.header.consensus_hash,
            self.block.header.burn_spent,
            tenure_change,
            coinbase,
        )?;

        let mut miner_tenure_info =
            builder.load_tenure_info(chainstate, &burn_dbconn, tenure_cause)?;
        let mut tenure_tx = builder.tenure_begin(&burn_dbconn, &mut miner_tenure_info)?;

        for (i, tx) in self.block.txs.iter().enumerate() {
            let tx_len = tx.tx_len();
            let tx_result = builder.try_mine_tx_with_len(
                &mut tenure_tx,
                &tx,
                tx_len,
                &BlockLimitFunction::NO_LIMIT_HIT,
                ASTRules::PrecheckSize,
            );
            let err = match tx_result {
                TransactionResult::Success(_) => Ok(()),
                TransactionResult::Skipped(s) => Err(format!("tx {i} skipped: {}", s.error)),
                TransactionResult::ProcessingError(e) => {
                    Err(format!("Error processing tx {i}: {}", e.error))
                }
                TransactionResult::Problematic(p) => {
                    Err(format!("Problematic tx {i}: {}", p.error))
                }
            };
            if let Err(reason) = err {
                warn!(
                    "Rejected block proposal";
                    "reason" => %reason,
                    "tx" => ?tx,
                );
                return Err(BlockValidateReject {
                    reason,
                    reason_code: ValidateRejectCode::BadTransaction,
                });
            }
        }

        let mut block = builder.mine_nakamoto_block(&mut tenure_tx);
        let size = builder.get_bytes_so_far();
        let cost = builder.tenure_finish(tenure_tx);

        // Clone signatures from block proposal
        // These have already been validated by `validate_nakamoto_block_burnchain()``
        block.header.miner_signature = self.block.header.miner_signature.clone();
        block.header.signer_signature = self.block.header.signer_signature.clone();

        // Assuming `tx_nerkle_root` has been checked we don't need to hash the whole block
        let expected_block_header_hash = self.block.header.block_hash();
        let computed_block_header_hash = block.header.block_hash();

        if computed_block_header_hash != expected_block_header_hash {
            warn!(
                "Rejected block proposal";
                "reason" => "Block hash is not as expected",
                "expected_block_header_hash" => %expected_block_header_hash,
                "computed_block_header_hash" => %computed_block_header_hash,
                //"expected_block" => %serde_json::to_string(&serde_json::to_value(&self.block).unwrap()).unwrap(),
                //"computed_block" => %serde_json::to_string(&serde_json::to_value(&block).unwrap()).unwrap(),
            );
            return Err(BlockValidateReject {
                reason: "Block hash is not as expected".into(),
                reason_code: ValidateRejectCode::BadBlockHash,
            });
        }

        info!(
            "Participant: validated anchored block";
            "block_header_hash" => %computed_block_header_hash,
            "height" => block.header.chain_length,
            "tx_count" => block.txs.len(),
            "parent_stacks_block_id" => %block.header.parent_block_id,
            "block_size" => size,
            "execution_cost" => %cost,
            "validation_time_ms" => time_elapsed(),
            "tx_fees_microstacks" => block.txs.iter().fold(0, |agg: u64, tx| {
                agg.saturating_add(tx.get_tx_fee())
            })
        );

        Ok(BlockValidateOk { block, cost, size })
    }
}
