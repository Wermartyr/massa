use std::{
    collections::{hash_map::Entry, HashMap, HashSet},
    num::NonZeroUsize,
    thread::JoinHandle,
    time::Instant,
};

use crate::{
    handlers::{
        endorsement_handler::cache::SharedEndorsementCache,
        operation_handler::cache::SharedOperationCache,
        peer_handler::models::{PeerManagementCmd, PeerMessageTuple},
    },
    messages::MessagesSerializer,
    sig_verifier::verify_sigs_batch,
};
use crossbeam::{
    channel::{at, Receiver, Sender},
    select,
};
use lru::LruCache;
use massa_consensus_exports::ConsensusController;
use massa_hash::{Hash, HASH_SIZE_BYTES};
use massa_logging::massa_trace;
use massa_models::{
    block::{Block, BlockSerializer},
    block_header::SecuredHeader,
    block_id::BlockId,
    endorsement::SecureShareEndorsement,
    operation::{OperationId, SecureShareOperation},
    prehash::{CapacityAllocator, PreHashMap, PreHashSet},
    secure_share::{Id, SecureShare},
};
use massa_pool_exports::PoolController;
use massa_protocol_exports_2::{ProtocolConfig, ProtocolError};
use massa_serialization::{DeserializeError, Deserializer, Serializer};
use massa_storage::Storage;
use massa_time::TimeError;
use peernet::{network_manager::SharedActiveConnections, peer_id::PeerId};
use tracing::warn;

use super::{
    cache::SharedBlockCache,
    commands_propagation::BlockHandlerCommand,
    commands_retrieval::BlockHandlerRetrievalCommand,
    messages::{
        AskForBlocksInfo, BlockInfoReply, BlockMessage, BlockMessageDeserializer,
        BlockMessageDeserializerArgs,
    },
    BlockMessageSerializer,
};

static BLOCK_HEADER: &str = "protocol.protocol_worker.on_network_event.received_block_header";

/// Info about a block we've seen
#[derive(Debug, Clone)]
pub(crate) struct BlockInfo {
    /// The header of the block.
    pub(crate) header: Option<SecuredHeader>,
    /// Operations ids. None if not received yet
    pub(crate) operation_ids: Option<Vec<OperationId>>,
    /// Operations and endorsements contained in the block,
    /// if we've received them already, and none otherwise.
    pub(crate) storage: Storage,
    /// Full operations size in bytes
    pub(crate) operations_size: usize,
}

impl BlockInfo {
    fn new(header: Option<SecuredHeader>, storage: Storage) -> Self {
        BlockInfo {
            header,
            operation_ids: None,
            storage,
            operations_size: 0,
        }
    }
}

pub struct RetrievalThread {
    active_connections: SharedActiveConnections,
    consensus_controller: Box<dyn ConsensusController>,
    pool_controller: Box<dyn PoolController>,
    receiver_network: Receiver<PeerMessageTuple>,
    _internal_sender: Sender<BlockHandlerCommand>,
    receiver: Receiver<BlockHandlerRetrievalCommand>,
    block_message_serializer: MessagesSerializer,
    block_wishlist: PreHashMap<BlockId, BlockInfo>,
    asked_blocks: HashMap<PeerId, PreHashMap<BlockId, Instant>>,
    peer_cmd_sender: Sender<PeerManagementCmd>,
    endorsement_cache: SharedEndorsementCache,
    operation_cache: SharedOperationCache,
    next_timer_ask_block: Instant,
    cache: SharedBlockCache,
    config: ProtocolConfig,
    storage: Storage,
}

impl RetrievalThread {
    fn run(&mut self) {
        //TODO: Add real values
        let mut block_message_deserializer =
            BlockMessageDeserializer::new(BlockMessageDeserializerArgs {
                thread_count: 32,
                endorsement_count: 10000,
                block_infos_length_max: 10000,
                max_operations_per_block: 10000,
                max_datastore_value_length: 10000,
                max_function_name_length: 10000,
                max_parameters_size: 10000,
                max_op_datastore_entry_count: 10000,
                max_op_datastore_key_length: 100,
                max_op_datastore_value_length: 10000,
            });
        loop {
            select! {
                recv(self.receiver_network) -> msg => {
                    match msg {
                        Ok((peer_id, message_id, message)) => {
                            block_message_deserializer.set_message_id(message_id);
                            let (rest, message) = block_message_deserializer
                                .deserialize::<DeserializeError>(&message)
                                .unwrap();
                            if !rest.is_empty() {
                                println!("Error: message not fully consumed");
                                return;
                            }
                            match message {
                                BlockMessage::AskForBlocks(block_infos) => {
                                    if let Err(err) = self.on_asked_for_blocks_received(peer_id.clone(), block_infos) {
                                        warn!("Error in on_asked_for_blocks_received: {:?}", err);
                                    }
                                }
                                BlockMessage::ReplyForBlocks(block_infos) => {
                                    for (block_id, block_info) in block_infos.into_iter() {
                                        if let Err(err) = self.on_block_info_received(peer_id.clone(), block_id, block_info) {
                                            warn!("Error in on_block_info_received: {:?}", err);
                                        }
                                    }
                                    if let Err(err) = self.update_ask_block() {
                                        warn!("Error in update_ask_blocks: {:?}", err);
                                    }
                                }
                                BlockMessage::BlockHeader(header) => {
                                    massa_trace!(BLOCK_HEADER, { "peer_id": peer_id, "header": header});
                                    if let Ok(Some((block_id, is_new))) =
                                        self.note_header_from_peer(&header, &peer_id)
                                    {
                                        if is_new {
                                            self.consensus_controller
                                                .register_block_header(block_id, header);
                                        }
                                        if let Err(err) = self.update_ask_block() {
                                            warn!("Error in update_ask_blocks: {:?}", err);
                                        }
                                    } else {
                                        warn!(
                                            "peer {} sent us critically incorrect header, \
                                            which may be an attack attempt by the remote peer \
                                            or a loss of sync between us and the remote peer",
                                            peer_id,
                                        );
                                        let _ = self.ban_node(&peer_id);
                                    }
                                }
                            }
                        },
                        Err(err) => {
                            println!("Error in retrieval block handler: {:?}", err);
                            return;
                        }
                    }
                },
                recv(self.receiver) -> msg => {
                    match msg {
                        Ok(command) => {
                            match command {
                                BlockHandlerRetrievalCommand::WishlistDelta { new, remove } => {
                                    massa_trace!("protocol.protocol_worker.process_command.wishlist_delta.begin", { "new": new, "remove": remove });
                                    for (block_id, header) in new.into_iter() {
                                        self.block_wishlist.insert(
                                            block_id,
                                            BlockInfo::new(header, self.storage.clone_without_refs()),
                                        );
                                    }
                                    // Remove the knowledge that we asked this block to nodes.
                                    if let Err(err) = self.remove_asked_blocks_of_node(&remove) {
                                        warn!("Error in remove_asked_blocks_of_node: {:?}", err);
                                    }

                                    // Remove from the wishlist.
                                    for block_id in remove.iter() {
                                        self.block_wishlist.remove(block_id);
                                    }
                                    if let Err(err) = self.update_ask_block() {
                                        warn!("Error in update_ask_blocks: {:?}", err);
                                    }
                                    massa_trace!(
                                        "protocol.protocol_worker.process_command.wishlist_delta.end",
                                        {}
                                    );
                                }
                            }
                        },
                        Err(err) => {
                            println!("Error in retrieval block handler channel command: {:?}", err);
                            return;
                        }
                    }
                },
                recv(at(self.next_timer_ask_block)) -> _ => {
                    if let Err(err) = self.update_ask_block() {
                        warn!("Error in ask_blocks: {:?}", err);
                    }
                }
            }
        }
    }

    /// Network ask the local node for blocks
    ///
    /// React on another node asking for blocks information. We can forward the operation ids if
    /// the foreign node asked for `AskForBlocksInfo::Info` or the full operations if he asked for
    /// the missing operations in his storage with `AskForBlocksInfo::Operations`
    ///
    /// Forward the reply to the network.
    fn on_asked_for_blocks_received(
        &mut self,
        from_peer_id: PeerId,
        list: Vec<(BlockId, AskForBlocksInfo)>,
    ) -> Result<(), ProtocolError> {
        let mut all_blocks_info = vec![];
        for (hash, info_wanted) in &list {
            let (header, operations_ids) = match self.storage.read_blocks().get(hash) {
                Some(signed_block) => (
                    signed_block.content.header.clone(),
                    signed_block.content.operations.clone(),
                ),
                None => {
                    // let the node know we don't have the block.
                    all_blocks_info.push((*hash, BlockInfoReply::NotFound));
                    continue;
                }
            };
            let block_info = match info_wanted {
                AskForBlocksInfo::Header => BlockInfoReply::Header(header),
                AskForBlocksInfo::Info => BlockInfoReply::Info(operations_ids),
                AskForBlocksInfo::Operations(op_ids) => {
                    // Mark the node as having the block.
                    {
                        let mut cache_write = self.cache.write();
                        cache_write.insert_blocks_known(
                            &from_peer_id,
                            &[*hash],
                            true,
                            Instant::now(),
                        );
                    }
                    // Send only the missing operations that are in storage.
                    let needed_ops = {
                        let operations = self.storage.read_operations();
                        operations_ids
                            .into_iter()
                            .filter(|id| op_ids.contains(id))
                            .filter_map(|id| operations.get(&id))
                            .cloned()
                            .collect()
                    };
                    BlockInfoReply::Operations(needed_ops)
                }
            };
            all_blocks_info.push((*hash, block_info));
        }
        // Clean shared cache if peers do not exist anymore
        {
            let mut cache_write = self.cache.write();
            let peers: Vec<PeerId> = cache_write
                .blocks_known_by_peer
                .iter()
                .map(|(id, _)| id.clone())
                .collect();
            {
                let active_connections_read = self.active_connections.read();
                for peer_id in peers {
                    if !active_connections_read.connections.contains_key(&peer_id) {
                        cache_write.blocks_known_by_peer.pop(&peer_id);
                        self.asked_blocks.remove(&peer_id);
                    }
                }
            }
        }
        {
            let active_connections_read = self.active_connections.read();
            let connection = active_connections_read
                .connections
                .get(&from_peer_id)
                .ok_or(ProtocolError::SendError(format!(
                    "Send block info peer {} isn't connected anymore",
                    &from_peer_id
                )))?;
            connection
                .send_channels
                .send(
                    &self.block_message_serializer,
                    BlockMessage::ReplyForBlocks(all_blocks_info).into(),
                    true,
                )
                .map_err(|err| {
                    ProtocolError::SendError(format!("Send block info error: {:?}", err))
                })
        }
    }

    fn on_block_info_received(
        &mut self,
        from_peer_id: PeerId,
        block_id: BlockId,
        info: BlockInfoReply,
    ) -> Result<(), ProtocolError> {
        match info {
            BlockInfoReply::Header(header) => {
                // Verify and Send it consensus
                self.on_block_header_received(from_peer_id, block_id, header)
            }
            BlockInfoReply::Info(operation_list) => {
                // Ask for missing operations ids and print a warning if there is no header for
                // that block.
                // Ban the node if the operation ids hash doesn't match with the hash contained in
                // the block_header.
                self.on_block_operation_list_received(from_peer_id, block_id, operation_list)
            }
            BlockInfoReply::Operations(operations) => {
                // Send operations to pool,
                // before performing the below checks,
                // and wait for them to have been procesed(i.e. added to storage).
                self.on_block_full_operations_received(from_peer_id, block_id, operations)
            }
            BlockInfoReply::NotFound => {
                {
                    let mut cache_write = self.cache.write();
                    cache_write.insert_blocks_known(
                        &from_peer_id,
                        &[block_id],
                        false,
                        Instant::now(),
                    );
                }
                Ok(())
            }
        }
    }

    /// On block header received from a node.
    /// If the header is new, we propagate it to the consensus.
    /// We pass the state of `block_wishlist` to ask for information about the block.
    fn on_block_header_received(
        &mut self,
        from_peer_id: PeerId,
        block_id: BlockId,
        header: SecuredHeader,
    ) -> Result<(), ProtocolError> {
        if let Some(info) = self.block_wishlist.get(&block_id) {
            if info.header.is_some() {
                warn!(
                    "Peer {} sent us header for block id {} but we already received it.",
                    from_peer_id, block_id
                );
                if let Some(asked_blocks) = self.asked_blocks.get_mut(&from_peer_id) {
                    if asked_blocks.contains_key(&block_id) {
                        asked_blocks.remove(&block_id);
                        {
                            let mut cache_write = self.cache.write();
                            cache_write.insert_blocks_known(
                                &from_peer_id,
                                &[block_id],
                                false,
                                Instant::now(),
                            );
                        }
                    }
                }

                return Ok(());
            }
        }
        if let Err(err) = self.note_header_from_peer(&header, &from_peer_id) {
            warn!(
                "peer {} sent us critically incorrect header through protocol, \
                which may be an attack attempt by the remote node \
                or a loss of sync between us and the remote node. Err = {}",
                from_peer_id, err
            );
            let _ = self.ban_node(&from_peer_id);
            return Ok(());
        };
        if let Some(info) = self.block_wishlist.get_mut(&block_id) {
            info.header = Some(header);
        }

        // Update ask block
        // Maybe this code is useless as it's been done just above but in a condition that should cover all cases where it's useful
        // to do this. But maybe it's still trigger there it need verifications.
        let mut set = PreHashSet::<BlockId>::with_capacity(1);
        set.insert(block_id);
        self.remove_asked_blocks_of_node(&set)?;
        Ok(())
    }

    /// Perform checks on a header,
    /// and if valid update the node's view of the world.
    ///
    /// Returns a boolean representing whether the header is new.
    ///
    /// Does not ban the source node if the header is invalid.
    ///
    /// Checks performed on Header:
    /// - Not genesis.
    /// - Can compute a `BlockId`.
    /// - Valid signature.
    /// - Absence of duplicate endorsements.
    ///
    /// Checks performed on endorsements:
    /// - Unique indices.
    /// - Slot matches that of the block.
    /// - Block matches that of the block.
    pub(crate) fn note_header_from_peer(
        &mut self,
        header: &SecuredHeader,
        from_peer_id: &PeerId,
    ) -> Result<Option<(BlockId, bool)>, ProtocolError> {
        // refuse genesis blocks
        if header.content.slot.period == 0 || header.content.parents.is_empty() {
            return Ok(None);
        }

        // compute ID
        let block_id = header.id;

        // check if this header was already verified
        {
            let mut cache_write = self.cache.write();
            if let Some(block_header) = cache_write.checked_headers.get(&block_id).cloned() {
                cache_write.insert_blocks_known(&from_peer_id, &[block_id], false, Instant::now());
                cache_write.insert_blocks_known(
                    &from_peer_id,
                    &block_header.content.parents,
                    false,
                    Instant::now(),
                );
                {
                    let mut endorsement_cache_write = self.endorsement_cache.write();
                    let endorsement_ids = endorsement_cache_write
                        .endorsements_known_by_peer
                        .get_or_insert_mut(from_peer_id.clone(), || {
                            LruCache::new(
                                NonZeroUsize::new(self.config.max_node_known_blocks_size)
                                    .expect("max_node_known_blocks_size in config must be > 0"),
                            )
                        });
                    for endorsement_id in block_header.content.endorsements.iter().map(|e| e.id) {
                        endorsement_ids.put(endorsement_id, ());
                    }
                }
                return Ok(Some((block_id, false)));
            }
        }

        if let Err(err) =
            self.note_endorsements_from_peer(header.content.endorsements.clone(), from_peer_id)
        {
            warn!(
                "node {} sent us a header containing critically incorrect endorsements: {}",
                from_peer_id, err
            );
            return Ok(None);
        };

        // check header signature
        if let Err(err) = header.verify_signature() {
            massa_trace!("protocol.protocol_worker.check_header.err_signature", { "header": header, "err": format!("{}", err)});
            return Ok(None);
        };

        // check endorsement in header integrity
        let mut used_endorsement_indices: HashSet<u32> =
            HashSet::with_capacity(header.content.endorsements.len());
        for endorsement in header.content.endorsements.iter() {
            // check index reuse
            if !used_endorsement_indices.insert(endorsement.content.index) {
                massa_trace!("protocol.protocol_worker.check_header.err_endorsement_index_reused", { "header": header, "endorsement": endorsement});
                return Ok(None);
            }
            // check slot
            if endorsement.content.slot != header.content.slot {
                massa_trace!("protocol.protocol_worker.check_header.err_endorsement_invalid_slot", { "header": header, "endorsement": endorsement});
                return Ok(None);
            }
            // check endorsed block
            if endorsement.content.endorsed_block
                != header.content.parents[header.content.slot.thread as usize]
            {
                massa_trace!("protocol.protocol_worker.check_header.err_endorsement_invalid_endorsed_block", { "header": header, "endorsement": endorsement});
                return Ok(None);
            }
        }
        {
            let mut cache_write = self.cache.write();
            cache_write.checked_headers.put(block_id, header.clone());
            cache_write.insert_blocks_known(&from_peer_id, &[block_id], false, Instant::now());
            cache_write.insert_blocks_known(
                &from_peer_id,
                &header.content.parents,
                false,
                Instant::now(),
            );
            {
                let mut endorsement_cache_write = self.endorsement_cache.write();
                let endorsement_ids = endorsement_cache_write
                    .endorsements_known_by_peer
                    .get_or_insert_mut(from_peer_id.clone(), || {
                        LruCache::new(
                            NonZeroUsize::new(self.config.max_node_known_blocks_size)
                                .expect("max_node_known_blocks_size in config must be > 0"),
                        )
                    });
                for endorsement_id in header.content.endorsements.iter().map(|e| e.id) {
                    endorsement_ids.put(endorsement_id, ());
                }
            }
        }
        massa_trace!("protocol.protocol_worker.note_header_from_node.ok", { "node": from_peer_id, "block_id": block_id, "header": header});
        Ok(Some((block_id, true)))
    }

    /// send a ban peer command to the peer handler
    fn ban_node(&mut self, peer_id: &PeerId) -> Result<(), ProtocolError> {
        massa_trace!("ban node from retrieval thread", { "peer_id": peer_id.to_string() });
        self.peer_cmd_sender
            .send(PeerManagementCmd::Ban(peer_id.clone()))
            .map_err(|err| ProtocolError::SendError(err.to_string()))
    }

    /// Remove the given blocks from the local wishlist
    pub(crate) fn remove_asked_blocks_of_node(
        &mut self,
        remove_hashes: &PreHashSet<BlockId>,
    ) -> Result<(), ProtocolError> {
        massa_trace!("protocol.protocol_worker.remove_asked_blocks_of_node", {
            "remove": remove_hashes
        });
        for asked_blocks in self.asked_blocks.values_mut() {
            asked_blocks.retain(|h, _| !remove_hashes.contains(h));
        }
        Ok(())
    }

    /// Note endorsements coming from a given node,
    /// and propagate them when they were received outside of a header.
    ///
    /// Caches knowledge of valid ones.
    ///
    /// Does not ban if the endorsement is invalid
    ///
    /// Checks performed:
    /// - Valid signature.
    pub(crate) fn note_endorsements_from_peer(
        &mut self,
        endorsements: Vec<SecureShareEndorsement>,
        from_peer_id: &PeerId,
    ) -> Result<(), ProtocolError> {
        massa_trace!("protocol.protocol_worker.note_endorsements_from_node", { "node": from_peer_id, "endorsements": endorsements});
        let length = endorsements.len();
        let mut new_endorsements = PreHashMap::with_capacity(length);
        let mut endorsement_ids = PreHashSet::with_capacity(length);
        for endorsement in endorsements.into_iter() {
            let endorsement_id = endorsement.id;
            endorsement_ids.insert(endorsement_id);
            // check endorsement signature if not already checked
            {
                let read_cache = self.endorsement_cache.read();
                if read_cache.checked_endorsements.contains(&endorsement_id) {
                    new_endorsements.insert(endorsement_id, endorsement);
                }
            }
        }

        // Batch signature verification
        // optimized signature verification
        verify_sigs_batch(
            &new_endorsements
                .iter()
                .map(|(endorsement_id, endorsement)| {
                    (
                        *endorsement_id.get_hash(),
                        endorsement.signature,
                        endorsement.content_creator_pub_key,
                    )
                })
                .collect::<Vec<_>>(),
        )?;

        {
            let mut cache_write = self.endorsement_cache.write();
            // add to verified signature cache
            for endorsement_id in endorsement_ids.iter() {
                cache_write.checked_endorsements.put(*endorsement_id, ());
            }
            // add to known endorsements for source node.
            let endorsements = cache_write.endorsements_known_by_peer.get_or_insert_mut(
                from_peer_id.clone(),
                || {
                    LruCache::new(
                        NonZeroUsize::new(self.config.max_node_known_endorsements_size)
                            .expect("max_node_known_endorsements_size in config should be > 0"),
                    )
                },
            );
            for endorsement_id in endorsement_ids.iter() {
                endorsements.put(*endorsement_id, ());
            }
        }

        if !new_endorsements.is_empty() {
            let mut endorsements = self.storage.clone_without_refs();
            endorsements.store_endorsements(new_endorsements.into_values().collect());
            // Add to pool
            self.pool_controller.add_endorsements(endorsements);
        }

        Ok(())
    }

    /// On block information received, manage when we get a list of operations.
    /// Ask for the missing operations that are not in the `checked_operations` cache variable.
    ///
    /// # Ban
    /// Start compute the operations serialized total size with the operation we know.
    /// Ban the node if the operations contained in the block overflow the max size. We don't
    /// forward the block to the consensus in that case.
    ///
    /// # Parameters:
    /// - `from_peer_id`: Node which sent us the information.
    /// - `BlockId`: ID of the related operations we received.
    /// - `operation_ids`: IDs of the operations contained by the block.
    ///
    /// # Result
    /// return an error if stopping asking block failed. The error should be forwarded at the
    /// root. todo: check if if make panic.
    fn on_block_operation_list_received(
        &mut self,
        from_peer_id: PeerId,
        block_id: BlockId,
        operation_ids: Vec<OperationId>,
    ) -> Result<(), ProtocolError> {
        // All operation ids sent into a set
        let operation_ids_set: PreHashSet<OperationId> = operation_ids.iter().cloned().collect();

        // add to known ops
        {
            let mut cache_write = self.operation_cache.write();
            let known_ops =
                cache_write
                    .ops_known_by_peer
                    .get_or_insert_mut(from_peer_id.clone(), || {
                        LruCache::new(
                            NonZeroUsize::new(self.config.max_node_known_ops_size)
                                .expect("max_node_known_ops_size in config should be > 0"),
                        )
                    });
            for op_id in operation_ids_set.iter() {
                known_ops.put(op_id.prefix(), ());
            }
        }

        let info = if let Some(info) = self.block_wishlist.get_mut(&block_id) {
            info
        } else {
            warn!(
                "Peer {} sent us an operation list but we don't have block id {} in our wishlist.",
                from_peer_id, block_id
            );
            if let Some(asked_blocks) = self.asked_blocks.get_mut(&from_peer_id) && asked_blocks.contains_key(&block_id) {
                asked_blocks.remove(&block_id);
                {
                    let mut cache_write = self.cache.write();
                    cache_write.insert_blocks_known(&from_peer_id, &[block_id], false, Instant::now());
                }
            }
            return Ok(());
        };

        let header = if let Some(header) = &info.header {
            header
        } else {
            warn!("Peer {} sent us an operation list but we don't have receive the header of block id {} yet.", from_peer_id, block_id);
            if let Some(asked_blocks) = self.asked_blocks.get_mut(&from_peer_id) && asked_blocks.contains_key(&block_id) {
                asked_blocks.remove(&block_id);
                {
                    let mut cache_write = self.cache.write();
                    cache_write.insert_blocks_known(&from_peer_id, &[block_id], false, Instant::now());
                }
            }
            return Ok(());
        };

        if info.operation_ids.is_some() {
            warn!(
                "Peer {} sent us an operation list for block id {} but we already received it.",
                from_peer_id, block_id
            );
            if let Some(asked_blocks) = self.asked_blocks.get_mut(&from_peer_id) && asked_blocks.contains_key(&block_id) {
                asked_blocks.remove(&block_id);
                {
                    let mut cache_write = self.cache.write();
                    cache_write.insert_blocks_known(&from_peer_id, &[block_id], false, Instant::now());
                }
            }
            return Ok(());
        }

        let mut total_hash: Vec<u8> =
            Vec::with_capacity(operation_ids.len().saturating_mul(HASH_SIZE_BYTES));
        operation_ids.iter().for_each(|op_id| {
            let op_hash = op_id.get_hash().into_bytes();
            total_hash.extend(op_hash);
        });

        // Check operation_list against expected operations hash from header.
        if header.content.operation_merkle_root == Hash::compute_from(&total_hash) {
            if operation_ids.len() > self.config.max_operations_per_block as usize {
                warn!("Peer id {} sent us an operations list for block id {} that contains more operations than the max allowed for a block.", from_peer_id, block_id);
                let _ = self.ban_node(&from_peer_id);
                return Ok(());
            }

            // Add the ops of info.
            info.operation_ids = Some(operation_ids.clone());
            let known_operations = info.storage.claim_operation_refs(&operation_ids_set);

            // get the total size of known ops
            info.operations_size =
                Self::get_total_operations_size(&self.storage, &known_operations);

            // mark ops as checked
            {
                let mut cache_ops_write = self.operation_cache.write();
                for operation_id in known_operations.iter() {
                    cache_ops_write.insert_checked_operation(*operation_id);
                }
            }

            if info.operations_size > self.config.max_serialized_operations_size_per_block {
                warn!("Peer id {} sent us a operation list for block id {} but the operations we already have in our records exceed max size.", from_peer_id, block_id);
                let _ = self.ban_node(&from_peer_id);
                return Ok(());
            }

            // Update ask block
            let mut set = PreHashSet::<BlockId>::with_capacity(1);
            set.insert(block_id);
            self.remove_asked_blocks_of_node(&set)?;

            // If the block is empty, go straight to processing the full block info.
            if operation_ids.is_empty() {
                return self.on_block_full_operations_received(
                    from_peer_id,
                    block_id,
                    Default::default(),
                );
            }
        } else {
            warn!("Peer id {} sent us a operation list for block id {} but the hash in header doesn't match.", from_peer_id, block_id);
            let _ = self.ban_node(&from_peer_id);
        }
        Ok(())
    }
    /// Return the sum of all operation's serialized sizes in the `Set<Id>`
    fn get_total_operations_size(
        storage: &Storage,
        operation_ids: &PreHashSet<OperationId>,
    ) -> usize {
        let op_reader = storage.read_operations();
        let mut total: usize = 0;
        operation_ids.iter().for_each(|id| {
            if let Some(op) = op_reader.get(id) {
                total = total.saturating_add(op.serialized_size());
            }
        });
        total
    }

    /// Checks full block operations that we asked. (Because their was missing in the
    /// `checked_operations` cache variable, refer to `on_block_operation_list_received`)
    ///
    /// # Ban
    /// Ban the node if it doesn't fill the requirement. Forward to the graph with a
    /// `ProtocolEvent::ReceivedBlock` if the operations are under a max size.
    ///
    /// - thread incorrect for an operation
    /// - wanted operations doesn't match
    /// - duplicated operation
    /// - full operations serialized size overflow
    ///
    /// We received these operation because we asked for the missing operation
    fn on_block_full_operations_received(
        &mut self,
        from_peer_id: PeerId,
        block_id: BlockId,
        mut operations: Vec<SecureShareOperation>,
    ) -> Result<(), ProtocolError> {
        if let Err(err) = self.note_operations_from_peer(operations.clone(), &from_peer_id) {
            warn!(
                "Peer id {} sent us operations for block id {} but they failed at verifications. Err = {}",
                from_peer_id, block_id, err
            );
            let _ = self.ban_node(&from_peer_id);
            return Ok(());
        }

        match self.block_wishlist.entry(block_id) {
            Entry::Occupied(mut entry) => {
                let info = entry.get_mut();
                let header = if let Some(header) = &info.header {
                    header.clone()
                } else {
                    warn!("Peer {} sent us full operations but we don't have receive the header of block id {} yet.", from_peer_id, block_id);
                    if let Some(asked_blocks) = self.asked_blocks.get_mut(&from_peer_id) && asked_blocks.contains_key(&block_id) {
                        asked_blocks.remove(&block_id);
                        {
                            let mut cache_write = self.cache.write();
                            cache_write.insert_blocks_known(&from_peer_id, &[block_id], false, Instant::now());
                        }
                    }
                    return Ok(());
                };
                let block_operation_ids = if let Some(operations) = &info.operation_ids {
                    operations
                } else {
                    warn!("Peer id {} sent us full operations but we don't have received the operation list of block id {} yet.", from_peer_id, block_id);
                    if let Some(asked_blocks) = self.asked_blocks.get_mut(&from_peer_id) && asked_blocks.contains_key(&block_id) {
                        asked_blocks.remove(&block_id);
                        {
                            let mut cache_write = self.cache.write();
                            cache_write.insert_blocks_known(&from_peer_id, &[block_id], false, Instant::now());
                        }
                    }
                    return Ok(());
                };
                operations.retain(|op| block_operation_ids.contains(&op.id));
                // add operations to local storage and claim ref
                info.storage.store_operations(operations);
                let block_ids_set = block_operation_ids.clone().into_iter().collect();
                let known_operations = info.storage.claim_operation_refs(&block_ids_set);

                // Ban the node if:
                // - mismatch with asked operations (asked operations are the one that are not in storage) + operations already in storage and block operations
                // - full operations serialized size overflow
                let full_op_size: usize = {
                    let stored_operations = info.storage.read_operations();
                    known_operations
                        .iter()
                        .map(|id| stored_operations.get(id).unwrap().serialized_size())
                        .sum()
                };
                if full_op_size > self.config.max_serialized_operations_size_per_block {
                    warn!("Peer id {} sent us full operations for block id {} but they exceed max size.", from_peer_id, block_id);
                    let _ = self.ban_node(&from_peer_id);
                    self.block_wishlist.remove(&block_id);
                    self.consensus_controller
                        .mark_invalid_block(block_id, header);
                } else {
                    if known_operations != block_ids_set {
                        warn!(
                            "Peer id {} didn't sent us all the full operations for block id {}.",
                            from_peer_id, block_id
                        );

                        if let Some(asked_blocks) = self.asked_blocks.get_mut(&from_peer_id) && asked_blocks.contains_key(&block_id) {
                            asked_blocks.remove(&block_id);
                            {
                                let mut cache_write = self.cache.write();
                                cache_write.insert_blocks_known(&from_peer_id, &[block_id], false, Instant::now());
                            }
                        }
                        return Ok(());
                    }

                    // Re-constitute block.
                    let block = Block {
                        header: header.clone(),
                        operations: block_operation_ids.clone(),
                    };

                    let mut content_serialized = Vec::new();
                    BlockSerializer::new() // todo : keep the serializer in the struct to avoid recreating it
                        .serialize(&block, &mut content_serialized)
                        .unwrap();

                    // wrap block
                    let signed_block = SecureShare {
                        signature: header.signature,
                        content_creator_pub_key: header.content_creator_pub_key,
                        content_creator_address: header.content_creator_address,
                        id: block_id,
                        content: block,
                        serialized_data: content_serialized,
                    };

                    // create block storage (without parents)
                    let mut block_storage = entry.remove().storage;
                    // add endorsements to local storage and claim ref
                    // TODO change this if we make endorsements separate from block header
                    block_storage.store_endorsements(
                        signed_block.content.header.content.endorsements.clone(),
                    );
                    let slot = signed_block.content.header.content.slot;
                    // add block to local storage and claim ref
                    block_storage.store_block(signed_block);

                    // Send to consensus
                    self.consensus_controller
                        .register_block(block_id, slot, block_storage, false);
                }
            }
            Entry::Vacant(_) => {
                warn!("Peer {} sent us full operations but we don't have the block id {} in our wishlist.", from_peer_id, block_id);
                if let Some(asked_blocks) = self.asked_blocks.get_mut(&from_peer_id) && asked_blocks.contains_key(&block_id) {
                    asked_blocks.remove(&block_id);
                    {
                        let mut cache_write = self.cache.write();
                        cache_write.insert_blocks_known(&from_peer_id, &[block_id], false, Instant::now());
                    }
                }
                return Ok(());
            }
        };

        // Update ask block
        let remove_hashes = vec![block_id].into_iter().collect();
        self.remove_asked_blocks_of_node(&remove_hashes)
    }

    fn note_operations_from_peer(
        &mut self,
        operations: Vec<SecureShareOperation>,
        source_peer_id: &PeerId,
    ) -> Result<(), ProtocolError> {
        massa_trace!("protocol.protocol_worker.note_operations_from_peer", { "peer": source_peer_id, "operations": operations });
        let length = operations.len();
        let mut new_operations = PreHashMap::with_capacity(length);
        for operation in operations {
            let operation_id = operation.id;
            if operation.serialized_size() > self.config.max_serialized_operations_size_per_block {
                return Err(ProtocolError::InvalidOperationError(format!(
                    "Operation {} exceeds max block size,  maximum authorized {} bytes but found {} bytes",
                    operation_id,
                    operation.serialized_size(),
                    self.config.max_serialized_operations_size_per_block
                )));
            };

            // Check operation signature only if not already checked.
            if !self
                .operation_cache
                .read()
                .checked_operations
                .contains(&operation_id)
            {
                // check signature if the operation wasn't in `checked_operation`
                new_operations.insert(operation_id, operation);
            };
        }
        // optimized signature verification
        verify_sigs_batch(
            &new_operations
                .iter()
                .map(|(op_id, op)| (*op_id.get_hash(), op.signature, op.content_creator_pub_key))
                .collect::<Vec<_>>(),
        )?;
        {
            // add to checked operations
            let mut cache_write = self.operation_cache.write();
            for op_id in new_operations.keys().copied() {
                cache_write.insert_checked_operation(op_id);
            }
        }
        //TODO: Send to operation propagation thread
        Ok(())
    }

    pub(crate) fn update_ask_block(&mut self) -> Result<(), ProtocolError> {
        massa_trace!("protocol.protocol_worker.update_ask_block.begin", {});
        let now = Instant::now();

        // init timer
        let mut next_tick = now
            .checked_add(self.config.ask_block_timeout.into())
            .ok_or(TimeError::TimeOverflowError)?;

        // list blocks to re-ask and gather candidate nodes to ask from
        let mut candidate_nodes: PreHashMap<BlockId, Vec<_>> = Default::default();
        let mut ask_block_list: HashMap<PeerId, Vec<(BlockId, AskForBlocksInfo)>> =
            Default::default();

        // list blocks to re-ask and from whom
        for (hash, block_info) in self.block_wishlist.iter() {
            let required_info = if block_info.header.is_none() {
                AskForBlocksInfo::Header
            } else if block_info.operation_ids.is_none() {
                AskForBlocksInfo::Info
            } else {
                let already_stored_operations = block_info.storage.get_op_refs();
                // Unwrap safety: Check if `operation_ids` is none just above
                AskForBlocksInfo::Operations(
                    block_info
                        .operation_ids
                        .as_ref()
                        .unwrap()
                        .iter()
                        .filter(|id| !already_stored_operations.contains(id))
                        .copied()
                        .collect(),
                )
            };
            let mut needs_ask = true;
            {
                let mut cache_write = self.cache.write();
                // Clean old peers that aren't active anymore
                let peers_connected: HashSet<PeerId> = self
                    .active_connections
                    .read()
                    .connections
                    .keys()
                    .cloned()
                    .collect();
                let peers_in_cache: Vec<PeerId> = cache_write
                    .blocks_known_by_peer
                    .iter()
                    .map(|(peer_id, _)| peer_id.clone())
                    .collect();
                for peer_id in peers_in_cache {
                    if !peers_connected.contains(&peer_id) {
                        cache_write.blocks_known_by_peer.pop(&peer_id);
                    }
                }
                let peers_in_asked_blocks: Vec<PeerId> =
                    self.asked_blocks.keys().cloned().collect();
                for peer_id in peers_in_asked_blocks {
                    if !peers_connected.contains(&peer_id) {
                        self.asked_blocks.remove(&peer_id);
                    }
                }
                // Add new peers
                for peer_id in peers_connected {
                    if !cache_write.blocks_known_by_peer.contains(&peer_id) {
                        //TODO: Change to detect the connection before
                        cache_write.blocks_known_by_peer.put(
                            peer_id.clone(),
                            (
                                LruCache::new(
                                    NonZeroUsize::new(self.config.max_node_known_blocks_size)
                                        .expect("max_node_known_blocks_size in config must be > 0"),
                                ),
                                Instant::now(),
                            ),
                        );
                    } else {
                        cache_write.blocks_known_by_peer.promote(&peer_id);
                    }

                    if !self.asked_blocks.contains_key(&peer_id) {
                        self.asked_blocks
                            .insert(peer_id.clone(), PreHashMap::default());
                    }
                }
                for (peer_id, (blocks_known, _)) in cache_write.blocks_known_by_peer.iter_mut() {
                    // map to remove the borrow on asked_blocks. Otherwise can't call insert_known_blocks
                    let ask_time_opt = self
                        .asked_blocks
                        .get(peer_id)
                        .and_then(|asked_blocks| asked_blocks.get(hash).copied());
                    let (timeout_at_opt, timed_out) = if let Some(ask_time) = ask_time_opt {
                        let t = ask_time
                            .checked_add(self.config.ask_block_timeout.into())
                            .ok_or(TimeError::TimeOverflowError)?;
                        (Some(t), t <= now)
                    } else {
                        (None, false)
                    };
                    let knows_block = blocks_known.get(hash);

                    // check if the peer recently told us it doesn't have the block
                    if let Some((false, info_time)) = knows_block {
                        let info_expires = info_time
                            .checked_add(self.config.ask_block_timeout.into())
                            .ok_or(TimeError::TimeOverflowError)?;
                        if info_expires > now {
                            next_tick = std::cmp::min(next_tick, info_expires);
                            continue; // ignore candidate peer
                        }
                    }

                    let candidate = match (timed_out, timeout_at_opt, knows_block) {
                        // not asked yet
                        (_, None, knowledge) => match knowledge {
                            Some((true, _)) => (0u8, None),
                            None => (1u8, None),
                            Some((false, _)) => (2u8, None),
                        },
                        // not timed out yet (note: recent DONTHAVBLOCK checked before the match)
                        (false, Some(timeout_at), _) => {
                            next_tick = std::cmp::min(next_tick, timeout_at);
                            needs_ask = false; // no need to re ask
                            continue; // not a candidate
                        }
                        // timed out, supposed to have it
                        (true, Some(timeout_at), Some((true, info_time))) => {
                            if info_time < &timeout_at {
                                // info less recent than timeout: mark as not having it
                                blocks_known.put(*hash, (false, timeout_at));
                                (2u8, ask_time_opt)
                            } else {
                                // told us it has it after a timeout: good candidate again
                                (0u8, ask_time_opt)
                            }
                        }
                        // timed out, supposed to not have it
                        (true, Some(timeout_at), Some((false, info_time))) => {
                            if info_time < &timeout_at {
                                // info less recent than timeout: update info time
                                blocks_known.put(*hash, (false, timeout_at));
                            }
                            (2u8, ask_time_opt)
                        }
                        // timed out but don't know if has it: mark as not having it
                        (true, Some(timeout_at), None) => {
                            blocks_known.put(*hash, (false, timeout_at));
                            (2u8, ask_time_opt)
                        }
                    };

                    // add candidate peer
                    candidate_nodes.entry(*hash).or_insert_with(Vec::new).push((
                        candidate,
                        peer_id.clone(),
                        required_info.clone(),
                    ));
                }

                // remove if doesn't need to be asked
                if !needs_ask {
                    candidate_nodes.remove(hash);
                }
            }
        }

        // count active block requests per node
        let mut active_block_req_count: HashMap<PeerId, usize> = self
            .asked_blocks
            .iter()
            .map(|(peer_id, blocks)| {
                (
                    peer_id.clone(),
                    blocks
                        .iter()
                        .filter(|(_h, ask_t)| {
                            ask_t
                                .checked_add(self.config.ask_block_timeout.into())
                                .map_or(false, |timeout_t| timeout_t > now)
                        })
                        .count(),
                )
            })
            .collect();

        for (hash, criteria) in candidate_nodes.into_iter() {
            // find the best node
            if let Some((_knowledge, best_node, required_info)) = criteria
                .into_iter()
                .filter(|(_knowledge, peer_id, _)| {
                    // filter out nodes with too many active block requests
                    *active_block_req_count.get(peer_id).unwrap_or(&0)
                        <= self.config.max_simultaneous_ask_blocks_per_node
                })
                .min_by_key(|(knowledge, peer_id, _)| {
                    (
                        *knowledge,                                         // block knowledge
                        *active_block_req_count.get(peer_id).unwrap_or(&0), // active requests
                        self.cache
                            .read()
                            .blocks_known_by_peer
                            .peek(peer_id)
                            .unwrap()
                            .1, // node age (will not panic, already checked)
                        peer_id.clone(),                                    // node ID
                    )
                })
            {
                let asked_blocks = self.asked_blocks.get_mut(&best_node).unwrap(); // will not panic, already checked
                asked_blocks.insert(hash, now);
                if let Some(cnt) = active_block_req_count.get_mut(&best_node) {
                    *cnt += 1; // increase the number of actively asked blocks
                }

                ask_block_list
                    .entry(best_node.clone())
                    .or_insert_with(Vec::new)
                    .push((hash, required_info.clone()));

                let timeout_at = now
                    .checked_add(self.config.ask_block_timeout.into())
                    .ok_or(TimeError::TimeOverflowError)?;
                next_tick = std::cmp::min(next_tick, timeout_at);
            }
        }

        // send AskBlockEvents
        if !ask_block_list.is_empty() {
            for (peer_id, list) in ask_block_list.iter() {
                {
                    let active_connections_read = self.active_connections.read();
                    if let Some(peer) = active_connections_read.connections.get(peer_id) {
                        if let Err(err) = peer.send_channels.send(
                            &self.block_message_serializer,
                            BlockMessage::AskForBlocks(list.clone()).into(),
                            true,
                        ) {
                            warn!(
                                "Failed to send AskForBlocks to peer {} err: {}",
                                peer_id, err
                            );
                        }
                    }
                }
            }
        }

        self.next_timer_ask_block = next_tick;
        Ok(())
    }
}

pub fn start_retrieval_thread(
    active_connections: SharedActiveConnections,
    consensus_controller: Box<dyn ConsensusController>,
    pool_controller: Box<dyn PoolController>,
    receiver_network: Receiver<PeerMessageTuple>,
    receiver: Receiver<BlockHandlerRetrievalCommand>,
    _internal_sender: Sender<BlockHandlerCommand>,
    peer_cmd_sender: Sender<PeerManagementCmd>,
    config: ProtocolConfig,
    endorsement_cache: SharedEndorsementCache,
    operation_cache: SharedOperationCache,
    cache: SharedBlockCache,
    storage: Storage,
) -> JoinHandle<()> {
    let block_message_serializer =
        MessagesSerializer::new().with_block_message_serializer(BlockMessageSerializer::new());
    std::thread::spawn(move || {
        let mut retrieval_thread = RetrievalThread {
            active_connections,
            consensus_controller,
            pool_controller,
            next_timer_ask_block: Instant::now() + config.ask_block_timeout.to_duration(),
            block_wishlist: PreHashMap::default(),
            asked_blocks: HashMap::default(),
            peer_cmd_sender,
            receiver_network,
            block_message_serializer,
            receiver,
            _internal_sender,
            cache,
            endorsement_cache,
            operation_cache,
            config,
            storage,
        };
        retrieval_thread.run();
    })
}
