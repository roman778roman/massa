use massa_consensus_exports::{
    block_graph_export::BlockGraphExport, block_status::BlockStatus,
    bootstrapable_graph::BootstrapableGraph, error::ConsensusError,
    export_active_block::ExportActiveBlock, ConsensusController,
};
use massa_models::{
    api::BlockGraphStatus,
    block::{BlockHeader, BlockId},
    clique::Clique,
    slot::Slot,
    stats::ConsensusStats,
    streaming_step::StreamingStep,
    wrapped::Wrapped,
};
use massa_storage::Storage;
use parking_lot::RwLock;
use std::sync::{mpsc::SyncSender, Arc};
use tracing::debug;

use crate::{commands::ConsensusCommand, state::ConsensusState};

/// The retrieval of data is made using a shared state and modifications are asked by sending message to a channel.
/// This is done mostly to be able to:
///
/// - send commands through the channel without waiting for them to be processed from the point of view of the sending thread, and channels are very much optimal for that (much faster than locks)
/// - still be able to read the current state of the graph as processed so far (for this we need a shared state)
///
/// Note that sending commands and reading the state is done from different, mutually-asynchronous tasks and they can have data that are not sync yet.
#[derive(Clone)]
pub struct ConsensusControllerImpl {
    command_sender: SyncSender<ConsensusCommand>,
    shared_state: Arc<RwLock<ConsensusState>>,
}

impl ConsensusControllerImpl {
    pub fn new(
        command_sender: SyncSender<ConsensusCommand>,
        shared_state: Arc<RwLock<ConsensusState>>,
    ) -> Self {
        Self {
            command_sender,
            shared_state,
        }
    }
}

impl ConsensusController for ConsensusControllerImpl {
    /// Get a block graph export in a given period.
    ///
    /// # Arguments:
    /// * `start_slot`: the start slot
    /// * `end_slot`: the end slot
    ///
    /// # Returns:
    /// An export of the block graph in this period
    fn get_block_graph_status(
        &self,
        start_slot: Option<Slot>,
        end_slot: Option<Slot>,
    ) -> Result<BlockGraphExport, ConsensusError> {
        self.shared_state
            .read()
            .extract_block_graph_part(start_slot, end_slot)
    }

    /// Get statuses of blocks present in the graph
    ///
    /// # Arguments:
    /// * `block_ids`: the block ids to get the status of
    ///
    /// # Returns:
    /// A vector of statuses sorted by the order of the block ids
    fn get_block_statuses(&self, ids: &[BlockId]) -> Vec<BlockGraphStatus> {
        let read_shared_state = self.shared_state.read();
        ids.iter()
            .map(|id| read_shared_state.get_block_status(id))
            .collect()
    }

    /// Get all the cliques possible in the block graph.
    ///
    /// # Returns:
    /// A vector of cliques
    fn get_cliques(&self) -> Vec<Clique> {
        self.shared_state.read().max_cliques.clone()
    }

    /// Get a part of the graph to send to a node so that he can setup his graph.
    /// Used for bootstrap.
    ///
    /// # Returns:
    /// A portion of the graph
    fn get_bootstrap_part(
        &self,
        mut cursor: StreamingStep<Slot>,
        execution_cursor: StreamingStep<Slot>,
    ) -> Result<(BootstrapableGraph, StreamingStep<Slot>), ConsensusError> {
        if cursor.finished() {
            return Ok((
                BootstrapableGraph {
                    final_blocks: Vec::new(),
                },
                StreamingStep::Finished(None),
            ));
        }

        let read_shared_state = self.shared_state.read();
        let mut required_final_blocks: Vec<_> =
            read_shared_state.list_required_active_blocks()?;
        required_final_blocks.retain(|b_id| {
            if let Some(BlockStatus::Active { a_block, .. }) =
                read_shared_state.block_statuses.get(b_id)
            {
                if a_block.is_final {
                    match cursor {
                        StreamingStep::Started => return true,
                        StreamingStep::Ongoing(slot) if a_block.slot > slot => return true,
                        _ => return false,
                    }
                }
            }
            false
        });
        let mut final_blocks: Vec<ExportActiveBlock> = Vec::new();

        debug!("CONSENSUS get_bootstrap_part START");

        for b_id in &required_final_blocks {
            if let Some(BlockStatus::Active { a_block, storage }) =
                read_shared_state.block_statuses.get(b_id)
            {
                // IMPORTANT TODO: use a config parameter
                if final_blocks.len() >= 100 {
                    break;
                }
                final_blocks.push(ExportActiveBlock::from_active_block(a_block, storage));
                if let StreamingStep::Finished(Some(slot)) = execution_cursor {
                    if slot == a_block.slot {
                        cursor = StreamingStep::Finished(Some(a_block.slot));
                        break;
                    }
                }
                cursor = StreamingStep::Ongoing(a_block.slot);
            } else {
                return Err(ConsensusError::ContainerInconsistency(format!(
                    "block {} was expected to be active but wasn't on bootstrap graph export",
                    b_id
                )));
            }
        }

        // if final_blocks.is_empty() {
        //     cursor = StreamingStep::Finished(None);
        // }

        debug!("CONSENSUS get_bootstrap_part END");

        Ok((BootstrapableGraph { final_blocks }, cursor))
    }

    /// Get the stats of the consensus
    fn get_stats(&self) -> Result<ConsensusStats, ConsensusError> {
        self.shared_state.read().get_stats()
    }

    /// Get the current best parents for a block creation
    ///
    /// # Returns:
    /// A block id and a period for each thread of the graph
    fn get_best_parents(&self) -> Vec<(BlockId, u64)> {
        self.shared_state.read().best_parents.clone()
    }

    /// Get the block, that is in the blockclique, at a given slot.
    ///
    /// # Arguments:
    /// * `slot`: the slot to get the block at
    ///
    /// # Returns:
    /// The block id of the block at the given slot if exists
    fn get_blockclique_block_at_slot(&self, slot: Slot) -> Option<BlockId> {
        self.shared_state
            .read()
            .get_blockclique_block_at_slot(&slot)
    }

    /// Get the latest block, that is in the blockclique, in the thread of the given slot and before this `slot`.
    ///
    /// # Arguments:
    /// * `slot`: the slot that will give us the thread and the upper bound
    ///
    /// # Returns:
    /// The block id of the latest block in the thread of the given slot and before this slot
    fn get_latest_blockclique_block_at_slot(&self, slot: Slot) -> BlockId {
        self.shared_state
            .read()
            .get_latest_blockclique_block_at_slot(&slot)
    }

    fn register_block(&self, block_id: BlockId, slot: Slot, block_storage: Storage, created: bool) {
        let _ = self
            .command_sender
            .try_send(ConsensusCommand::RegisterBlock(
                block_id,
                slot,
                block_storage,
                created,
            ));
    }

    fn register_block_header(&self, block_id: BlockId, header: Wrapped<BlockHeader, BlockId>) {
        let _ = self
            .command_sender
            .try_send(ConsensusCommand::RegisterBlockHeader(block_id, header));
    }

    fn mark_invalid_block(&self, block_id: BlockId, header: Wrapped<BlockHeader, BlockId>) {
        let _ = self
            .command_sender
            .try_send(ConsensusCommand::MarkInvalidBlock(block_id, header));
    }

    fn clone_box(&self) -> Box<dyn ConsensusController> {
        Box::new(self.clone())
    }
}
