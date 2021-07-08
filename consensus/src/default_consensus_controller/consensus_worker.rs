use super::super::{
    block_graph::*, config::ConsensusConfig, consensus_controller::*, random_selector::*,
    timeslots::*,
};
use super::misc_collections::{DependencyWaitingBlocks, FutureIncomingBlocks};
use crate::error::{BlockAcknowledgeError, ConsensusError};
use communication::network::PeerInfo;
use communication::protocol::protocol_controller::{
    NodeId, ProtocolController, ProtocolEvent, ProtocolEventType,
};
use crypto::{hash::Hash, signature::PublicKey, signature::SignatureEngine};
use models::block::Block;
use std::{collections::HashMap, net::IpAddr};

use tokio::{
    sync::mpsc::{Receiver, Sender},
    time::sleep_until,
};

#[derive(Debug, Clone)]
pub enum ConsensusCommand {
    //return current blockgraph without block operations.
    GetBlockGraphStatus(Sender<BlockGraphExport>),
    //return full block with specified hash
    GetActiveBlock(Hash, Sender<Option<Block>>),
    GetPeers(Sender<HashMap<IpAddr, PeerInfo>>),
    GetSelectionDraws(
        (u64, u8),
        (u64, u8),
        Sender<Result<Vec<((u64, u8), PublicKey)>, ConsensusError>>,
    ),
}

pub struct ConsensusWorker<ProtocolControllerT: ProtocolController + 'static> {
    cfg: ConsensusConfig,
    genesis_public_key: PublicKey,
    protocol_controller: ProtocolControllerT,
    block_db: BlockGraph,
    controller_command_rx: Receiver<ConsensusCommand>,
    controller_event_tx: Sender<ConsensusEvent>,
    selector: RandomSelector,
    future_incoming_blocks: FutureIncomingBlocks,
    dependency_waiting_blocks: DependencyWaitingBlocks,
    current_slot: (u64, u8),
}

impl<ProtocolControllerT: ProtocolController + 'static> ConsensusWorker<ProtocolControllerT> {
    pub fn new(
        cfg: ConsensusConfig,
        protocol_controller: ProtocolControllerT,
        block_db: BlockGraph,
        controller_command_rx: Receiver<ConsensusCommand>,
        controller_event_tx: Sender<ConsensusEvent>,
    ) -> Result<ConsensusWorker<ProtocolControllerT>, ConsensusError> {
        let seed = vec![0u8; 32]; // TODO temporary (see issue #103)
        let participants_weights = vec![1u64; cfg.nodes.len()]; // TODO (see issue #104)
        let selector = RandomSelector::new(&seed, cfg.thread_count, participants_weights)?;
        let current_slot =
            get_current_latest_block_slot(cfg.thread_count, cfg.t0, cfg.genesis_timestamp)?
                .map_or(Ok((0u64, 0u8)), |s| {
                    get_next_block_slot(cfg.thread_count, s)
                })?;
        Ok(ConsensusWorker {
            cfg: cfg.clone(),
            genesis_public_key: SignatureEngine::new().derive_public_key(&cfg.genesis_key),
            protocol_controller,
            block_db,
            controller_command_rx,
            controller_event_tx,
            selector,
            future_incoming_blocks: FutureIncomingBlocks::new(cfg.max_future_processing_blocks),
            dependency_waiting_blocks: DependencyWaitingBlocks::new(cfg.max_dependency_blocks),
            current_slot,
        })
    }

    pub async fn run_loop(mut self) -> Result<(), ConsensusError> {
        let next_slot_timer = sleep_until(
            get_block_slot_timestamp(
                self.cfg.thread_count,
                self.cfg.t0,
                self.cfg.genesis_timestamp,
                self.current_slot,
            )?
            .estimate_instant()?,
        );
        tokio::pin!(next_slot_timer);
        loop {
            tokio::select! {
                // listen consensus commands
                res = self.controller_command_rx.recv() => match res {
                    Some(cmd) => self.process_consensus_command(cmd).await?,
                    None => break  // finished
                },

                // slot timer
                _ = &mut next_slot_timer => {
                    massa_trace!("slot_timer", {
                        "thread": self.current_slot.1,
                        "period": self.current_slot.0
                    });
                    let block_creator = self.selector.draw(self.current_slot);

                    // create a block if enabled and possible
                    if !self.cfg.disable_block_creation && self.current_slot.0 > 0 && block_creator == self.cfg.current_node_index {
                        let (hash, block) = self.block_db.create_block("block".to_string(), self.current_slot)?;
                        self.rec_acknowledge_block(hash, block).await?;
                    }

                    // process queued blocks
                    let popped_blocks = self.future_incoming_blocks.pop_until(self.current_slot)?;
                    for (hash, block) in popped_blocks.into_iter() {
                        self.rec_acknowledge_block(hash, block).await?;
                    }

                    // reset timer for next slot
                    self.current_slot = get_next_block_slot(self.cfg.thread_count, self.current_slot)?;
                    next_slot_timer.set(sleep_until(
                        get_block_slot_timestamp(
                            self.cfg.thread_count,
                            self.cfg.t0,
                            self.cfg.genesis_timestamp,
                            self.current_slot
                        )?
                        .estimate_instant()?,
                    ));
                }

                // listen protocol controller events
                evt = self.protocol_controller.wait_event() => match evt {
                    Ok(ProtocolEvent(source_node_id, event)) => self.process_protocol_event(source_node_id, event).await?,
                    Err(err) => return Err(ConsensusError::CommunicationError(err)) // in a loop
                }

            }
        }

        // end loop
        self.protocol_controller.stop().await?;
        Ok(())
    }

    async fn process_consensus_command(
        &mut self,
        cmd: ConsensusCommand,
    ) -> Result<(), ConsensusError> {
        match cmd {
            ConsensusCommand::GetBlockGraphStatus(response_tx) => response_tx
                .send(BlockGraphExport::from(&self.block_db))
                .await
                .map_err(|err| {
                    ConsensusError::SendChannelError(format!(
                        "could not send GetBlockGraphStatus answer:{:?}",
                        err
                    ))
                }),
            //return full block with specified hash
            ConsensusCommand::GetActiveBlock(hash, response_tx) => response_tx
                .send(self.block_db.get_active_block(hash).cloned())
                .await
                .map_err(|err| {
                    ConsensusError::SendChannelError(format!(
                        "could not send GetBlock answer:{:?}",
                        err
                    ))
                }),
            ConsensusCommand::GetPeers(response_tx) => response_tx
                .send(self.protocol_controller.get_peers().await?)
                .await
                .map_err(|err| {
                    ConsensusError::SendChannelError(format!(
                        "could not send GetPeers answer: {:?}",
                        err
                    ))
                }),
            ConsensusCommand::GetSelectionDraws(slot_start, slot_end, response_tx) => {
                let mut result: Result<Vec<((u64, u8), PublicKey)>, ConsensusError> =
                    Ok(Vec::new());
                let mut cur_slot = slot_start;
                while cur_slot < slot_end {
                    if let Ok(res) = result.as_mut() {
                        res.push((
                            cur_slot,
                            if cur_slot.0 == 0 {
                                self.genesis_public_key
                            } else {
                                self.cfg.nodes[self.selector.draw(cur_slot) as usize].0
                            },
                        ));
                    }
                    cur_slot = match get_next_block_slot(self.cfg.thread_count, cur_slot) {
                        Ok(next_slot) => next_slot,
                        Err(err) => {
                            result = Err(err);
                            break;
                        }
                    }
                }
                response_tx.send(result).await.map_err(|err| {
                    ConsensusError::SendChannelError(format!(
                        "could not send GetSelectionDraws response: {:?}",
                        err
                    ))
                })
            }
        }
    }

    // returns a new hashmap of blocks to re-acknowledge
    async fn acknowledge_block(
        &mut self,
        hash: Hash,
        block: Block,
    ) -> Result<HashMap<Hash, Block>, ConsensusError> {
        // if already in waiting structures, promote them if possible and quit
        {
            let (in_future, waiting_deps) = (
                self.future_incoming_blocks.contains(&hash),
                self.dependency_waiting_blocks.has_missing_deps(&hash),
            );
            if waiting_deps {
                self.dependency_waiting_blocks.promote(&hash)?;
            }
            if in_future || waiting_deps {
                return Ok(HashMap::new());
            }
        }

        info!("Add block hash:{}", hash);
        let res =
            self.block_db
                .acknowledge_block(hash, block, &mut self.selector, self.current_slot);
        if let Err(ref err) = res {
            let reason_str: String = err.to_string();
            massa_trace!(" consensus worker acknowledge_incoming_block error:", {
                "block hash ": hash,
                "error ": reason_str
            });
        }

        match res {
            // block is valid and was acknowledged
            Ok(discarded) => {
                // cancel discarded dependencies
                self.dependency_waiting_blocks
                    .cancel(discarded.keys().copied().collect())?;
                // cancel dependency_waiting_blocks for which the slot number is now inferior or equal to the latest final block in their thread
                let last_finals = self
                    .block_db
                    .get_latest_final_blocks_periods()
                    .iter()
                    .map(|(_hash, slot)| *slot)
                    .collect();
                let too_old = self.dependency_waiting_blocks.get_old(last_finals);
                self.dependency_waiting_blocks.cancel(too_old)?;

                // get block (if not discarded)
                if let Some(block) = self.block_db.get_active_block(hash) {
                    // propagate block
                    self.protocol_controller
                        .propagate_block(hash, &block)
                        .await?;

                    // unlock dependencies
                    self.dependency_waiting_blocks
                        .valid_block_obtained(&hash)?
                        .1
                        .into_iter()
                        .map(|h| {
                            Ok((
                                h,
                                self.dependency_waiting_blocks
                                    .get(&h)
                                    .ok_or(ConsensusError::ContainerInconsistency)?
                                    .clone(),
                            ))
                        })
                        .collect()
                } else {
                    Ok(HashMap::new())
                }
            }
            // block is in the future: queue it
            Err(BlockAcknowledgeError::InTheFuture(block)) => {
                if let Some((discarded_hash, _)) =
                    self.future_incoming_blocks.insert(hash, block)?
                {
                    // cancel dependency wait of canceled timeslot wait
                    self.dependency_waiting_blocks
                        .cancel(vec![discarded_hash].into_iter().collect())?;
                }
                Ok(HashMap::new())
            }
            Err(BlockAcknowledgeError::MissingDependencies(block, dependencies)) => {
                self.dependency_waiting_blocks
                    .insert(hash, block, dependencies)?;
                // TODO ask for dependencies that have not been asked yet
                //      but only if the dependency is not already in timeslot waiting line
                // (see issue #105)
                Ok(HashMap::new())
            }
            Err(BlockAcknowledgeError::TooMuchInTheFuture) => {
                // do nothing (DO NO DISCARD OR IT COULD BE USED TO PERFORM A FINALITY FORK)
                self.dependency_waiting_blocks
                    .cancel([hash].iter().copied().collect())?;
                Ok(HashMap::new())
            }
            Err(BlockAcknowledgeError::AlreadyAcknowledged) => {
                // do nothing: we already have this block
                Ok(HashMap::new())
            }
            Err(BlockAcknowledgeError::AlreadyDiscarded) => {
                //  do nothing: we already have discarded this block
                Ok(HashMap::new())
            }
            Err(BlockAcknowledgeError::WrongSignature) => {
                // the signature is wrong: ignore and do not cancel anything
                // TODO in the future, ban sender node
                // TODO re-ask ? (see issue #107)
                Ok(HashMap::new())
            }
            Err(BlockAcknowledgeError::InvalidFields) => {
                // do nothing: block is invalid
                self.dependency_waiting_blocks
                    .cancel([hash].iter().copied().collect())?;
                Ok(HashMap::new())
            }
            Err(BlockAcknowledgeError::DrawMismatch) => {
                // do nothing: wrong draw number
                self.dependency_waiting_blocks
                    .cancel([hash].iter().copied().collect())?;
                Ok(HashMap::new())
            }
            Err(BlockAcknowledgeError::InvalidParents(_)) => {
                // do nothing: invalid choice of parents
                self.dependency_waiting_blocks
                    .cancel([hash].iter().copied().collect())?;
                Ok(HashMap::new())
            }
            Err(BlockAcknowledgeError::TooOld) => {
                // do nothing: we already have discarded this block
                self.dependency_waiting_blocks
                    .cancel([hash].iter().copied().collect())?;
                Ok(HashMap::new())
            }
            Err(BlockAcknowledgeError::CryptoError(e)) => Err(ConsensusError::CryptoError(e)),
            Err(BlockAcknowledgeError::TimeError(e)) => Err(ConsensusError::TimeError(e)),
            Err(BlockAcknowledgeError::ConsensusError(e)) => Err(e),
            Err(BlockAcknowledgeError::ContainerInconsistency) => {
                Err(ConsensusError::ContainerInconsistency)
            }
        }
    }

    // recusrively acknowledge blocks while some are available
    async fn rec_acknowledge_block(
        &mut self,
        hash: Hash,
        block: Block,
    ) -> Result<(), ConsensusError> {
        // acknowledge incoming block
        let mut ack_map: HashMap<Hash, Block> = HashMap::new();
        ack_map.insert(hash, block);
        while let Some(bh) = ack_map.keys().next().cloned() {
            if let Some(b) = ack_map.remove(&bh) {
                ack_map.extend(self.acknowledge_block(bh, b).await?);
            }
        }
        Ok(())
    }

    async fn process_protocol_event(
        &mut self,
        source_node_id: NodeId,
        event: ProtocolEventType,
    ) -> Result<(), ConsensusError> {
        match event {
            ProtocolEventType::ReceivedBlock(block) => {
                self.rec_acknowledge_block(block.header.compute_hash()?, block)
                    .await?;
            }
            ProtocolEventType::ReceivedTransaction(_transaction) => {
                // todo (see issue #108)
            }
            ProtocolEventType::AskedBlock(block_hash) => {
                if let Some(_block) = self.block_db.get_active_block(block_hash) {
                    massa_trace!("sending_block", {"dest_node_id": source_node_id, "block": block_hash});
                    /*
                        TODO send full block
                        self.protocol_controller
                            .send_block(block, source_node_id)
                            .await?;
                    */ // (see issue #109)
                }
            }
        }
        Ok(())
    }
}
