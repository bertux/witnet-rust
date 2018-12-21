//! # ChainManager actor
//!
//! This module contains the ChainManager actor which is in charge
//! of managing the blocks and transactions of the Witnet blockchain
//! received through the protocol, and also encapsulates the logic of the
//! _unspent transaction outputs_.
//!
//! Among its responsabilities are the following:
//!
//! * Initializing the chain info upon running the node for the first time and persisting it into storage [StorageManager](actors::storage_manager::StorageManager)
//! * Recovering the chain info from storage and keeping it in its state.
//! * Validating block candidates as they come from a session.
//! * Consolidating multiple block candidates for the same checkpoint into a single valid block.
//! * Putting valid blocks into storage by sending them to the inventory manager actor.
//! * Having a method for letting other components get blocks by *hash* or *checkpoint*.
//! * Having a method for letting other components get the epoch of the current tip of the
//! blockchain (e.g. the last epoch field required for the handshake in the Witnet network
//! protocol).
//! * Validating transactions as they come from any [Session](actors::session::Session). This includes:
//!     - Iterating over its inputs, adding the value of the inputs to calculate the value of the transaction.
//!     - Running the output scripts, expecting them all to return `TRUE` and leave an empty stack.
//!     - Verifying that the sum of all inputs is greater than or equal to the sum of all the outputs.
//! * Keeping valid transactions into memory. This in-memory transaction pool is what we call the _mempool_. Valid transactions are immediately appended to the mempool.
//! * Keeping every unspent transaction output (UTXO) in the block chain in memory. This is called the _UTXO set_.
//! * Updating the UTXO set with valid transactions that have already been anchored into a valid block. This includes:
//!     - Removing the UTXOs that the transaction spends as inputs.
//!     - Adding a new UTXO for every output in the transaction.
use actix::{
    ActorFuture, Context, ContextFutureSpawner, Supervised, System, SystemService, WrapFuture,
};

use crate::actors::{
    chain_manager::messages::InventoryEntriesResult,
    inventory_manager::{messages::AddItem, InventoryManager},
    storage_keys::CHAIN_KEY,
    storage_manager::{messages::Put, StorageManager},
};

use log::{debug, error, info};
use std::collections::HashMap;
use std::collections::HashSet;
use witnet_data_structures::chain::{
    Block, BlockHeader, ChainInfo, Epoch, Hash, Hashable, InventoryEntry, InventoryItem,
    Transaction, TransactionsPool,
};

use crate::actors::session::messages::AnnounceItems;
use crate::actors::sessions_manager::{messages::Broadcast, SessionsManager};

use witnet_storage::error::StorageError;

use crate::actors::chain_manager::messages::BuildBlock;
use crate::validations::block_reward;
use crate::validations::merkle_tree_root;
use witnet_util::error::WitnetError;

mod actor;
mod handlers;

/// Messages for ChainManager
pub mod messages;

/// Possible errors when interacting with ChainManager
#[derive(Debug)]
pub enum ChainManagerError {
    /// A block being processed was already known to this node
    BlockAlreadyExists,
    /// A block does not exist
    BlockDoesNotExist,
    /// StorageError
    StorageError(WitnetError<StorageError>),
}

impl From<WitnetError<StorageError>> for ChainManagerError {
    fn from(x: WitnetError<StorageError>) -> Self {
        ChainManagerError::StorageError(x)
    }
}

////////////////////////////////////////////////////////////////////////////////////////
// ACTOR BASIC STRUCTURE
////////////////////////////////////////////////////////////////////////////////////////
/// ChainManager actor
#[derive(Default)]
pub struct ChainManager {
    /// Blockchain information data structure
    chain_info: Option<ChainInfo>,
    /// Map that relates an epoch with the hashes of the blocks for that epoch
    // One epoch can have more than one block
    epoch_to_block_hash: HashMap<Epoch, HashSet<Hash>>,
    /// Map that stores blocks by their hash
    blocks: HashMap<Hash, Block>,
    /// Current Epoch
    current_epoch: Option<Epoch>,
    /// Transactions Pool
    transactions_pool: TransactionsPool,
    /// Block candidate to update chain_info in the next epoch
    block_candidate: Option<Block>,
}

/// Required trait for being able to retrieve ChainManager address from registry
impl Supervised for ChainManager {}

/// Required trait for being able to retrieve ChainManager address from registry
impl SystemService for ChainManager {}

/// Auxiliary methods for ChainManager actor
impl ChainManager {
    /// Method to persist chain_info into storage
    fn persist_chain_info(&self, ctx: &mut Context<Self>) {
        // Get StorageManager address
        let storage_manager_addr = System::current().registry().get::<StorageManager>();

        let chain_info = match self.chain_info.as_ref() {
            Some(x) => x,
            None => {
                error!("Trying to persist a None value");
                return;
            }
        };

        // Persist chain_info into storage. `AsyncContext::wait` registers
        // future within context, but context waits until this future resolves
        // before processing any other events.
        let msg = Put::from_value(CHAIN_KEY, chain_info).unwrap();
        storage_manager_addr
            .send(msg)
            .into_actor(self)
            .then(|res, _act, _ctx| {
                match res {
                    Ok(Ok(_)) => {
                        info!("ChainManager successfully persisted chain_info into storage")
                    }
                    _ => {
                        error!("ChainManager failed to persist chain_info into storage");
                        // FIXME(#72): handle errors
                    }
                }
                actix::fut::ok(())
            })
            .wait(ctx);
    }

    /// Method to Send an Item to Inventory Manager
    fn persist_item(&self, ctx: &mut Context<Self>, item: InventoryItem) {
        // Get InventoryManager address
        let inventory_manager_addr = System::current().registry().get::<InventoryManager>();

        // Persist block into storage through InventoryManager. `AsyncContext::wait` registers
        // future within context, but context waits until this future resolves
        // before processing any other events.
        inventory_manager_addr
            .send(AddItem { item })
            .into_actor(self)
            .then(|res, _act, _ctx| match res {
                // Process the response from InventoryManager
                Err(e) => {
                    // Error when sending message
                    error!("Unsuccessful communication with InventoryManager: {}", e);
                    actix::fut::err(())
                }
                Ok(res) => match res {
                    Err(e) => {
                        // InventoryManager error
                        error!("Error while getting block from InventoryManager: {}", e);
                        actix::fut::err(())
                    }
                    Ok(_) => actix::fut::ok(()),
                },
            })
            .wait(ctx)
    }

    fn process_new_block(&mut self, block: Block) -> Result<Hash, ChainManagerError> {
        // Calculate the hash of the block
        let hash: Hash = block.hash();

        // Check if we already have a block with that hash
        if let Some(_block) = self.blocks.get(&hash) {
            Err(ChainManagerError::BlockAlreadyExists)
        } else {
            // This is a new block, insert it into the internal maps
            {
                // Insert the new block into the map that relates epochs to block hashes
                let beacon = &block.block_header.beacon;
                let hash_set = &mut self
                    .epoch_to_block_hash
                    .entry(beacon.checkpoint)
                    .or_insert_with(HashSet::new);
                hash_set.insert(hash);

                debug!(
                    "Checkpoint {} has {} blocks",
                    beacon.checkpoint,
                    hash_set.len()
                );
            }

            // Insert the new block into the map of known blocks
            self.blocks.insert(hash, block);

            Ok(hash)
        }
    }

    fn broadcast_block(&mut self, hash: Hash) {
        // Get SessionsManager's address
        let sessions_manager_addr = System::current().registry().get::<SessionsManager>();

        // Tell SessionsManager to announce the new block through every consolidated Session
        let items = vec![InventoryEntry::Block(hash)];
        sessions_manager_addr.do_send(Broadcast {
            command: AnnounceItems { items },
        });
    }

    fn try_to_get_block(&mut self, hash: Hash) -> Result<Block, ChainManagerError> {
        // Check if we have a block with that hash
        self.blocks.get(&hash).map_or_else(
            || Err(ChainManagerError::BlockDoesNotExist),
            |block| Ok(block.clone()),
        )
    }

    fn build_block(&self, msg: &BuildBlock) -> Block {
        // Get all the unspent transactions and calculate the sum of their fees
        let mut transaction_fees = 0;
        let transactions: Vec<Transaction> = self
            .transactions_pool
            .iter()
            .map(|t| {
                // TODO: t.fee()
                transaction_fees += 1;
                *t
            })
            .collect();
        let epoch = msg.beacon.checkpoint;
        let _reward = block_reward(epoch) + transaction_fees;
        // TODO: push coinbase transaction
        let beacon = msg.beacon;
        let hash_merkle_root = merkle_tree_root(&transactions);
        let block_header = BlockHeader {
            version: 0,
            beacon,
            hash_merkle_root,
        };
        let proof = msg.leadership_proof;

        Block {
            block_header,
            proof,
            txns: transactions,
        }
    }

    fn discard_existing_inventory_entries(
        &mut self,
        inv_entries: Vec<InventoryEntry>,
    ) -> InventoryEntriesResult {
        // Missing inventory entries
        let missing_inv_entries = inv_entries
            .into_iter()
            .filter(|inv_entry| {
                // Get hash from inventory vector
                let hash = match inv_entry {
                    InventoryEntry::Error(hash)
                    | InventoryEntry::Block(hash)
                    | InventoryEntry::Tx(hash)
                    | InventoryEntry::DataRequest(hash)
                    | InventoryEntry::DataResult(hash) => hash,
                };

                // Add the inventory vector to the missing vectors if it is not found
                self.blocks.get(&hash).is_none()
            })
            .collect();

        Ok(missing_inv_entries)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_block() {
        let mut bm = ChainManager::default();

        // Build hardcoded block
        let checkpoint = 2;
        let block_a = build_hardcoded_block(checkpoint, 99999);

        // Add block to ChainManager
        let hash_a = bm.process_new_block(block_a.clone()).unwrap();

        // Check the block is added into the blocks map
        assert_eq!(bm.blocks.len(), 1);
        assert_eq!(bm.blocks.get(&hash_a).unwrap(), &block_a);

        // Check the block is added into the epoch-to-hash map
        assert_eq!(bm.epoch_to_block_hash.get(&checkpoint).unwrap().len(), 1);
        assert_eq!(
            bm.epoch_to_block_hash
                .get(&checkpoint)
                .unwrap()
                .iter()
                .next()
                .unwrap(),
            &hash_a
        );
    }

    #[test]
    fn add_same_block_twice() {
        let mut bm = ChainManager::default();

        // Build hardcoded block
        let block = build_hardcoded_block(2, 99999);

        // Only the first block will be inserted
        assert!(bm.process_new_block(block.clone()).is_ok());
        assert!(bm.process_new_block(block).is_err());
        assert_eq!(bm.blocks.len(), 1);
    }

    #[test]
    fn add_blocks_same_epoch() {
        let mut bm = ChainManager::default();

        // Build hardcoded blocks
        let checkpoint = 2;
        let block_a = build_hardcoded_block(checkpoint, 99999);
        let block_b = build_hardcoded_block(checkpoint, 12345);

        // Add blocks to the ChainManager
        let hash_a = bm.process_new_block(block_a).unwrap();
        let hash_b = bm.process_new_block(block_b).unwrap();
        assert_ne!(hash_a, hash_b);

        // Check that both blocks are stored in the same epoch
        assert_eq!(bm.epoch_to_block_hash.get(&checkpoint).unwrap().len(), 2);
        assert!(bm
            .epoch_to_block_hash
            .get(&checkpoint)
            .unwrap()
            .contains(&hash_a));
        assert!(bm
            .epoch_to_block_hash
            .get(&checkpoint)
            .unwrap()
            .contains(&hash_b));
    }

    #[test]
    fn get_existing_block() {
        // Create empty ChainManager
        let mut bm = ChainManager::default();

        // Create a hardcoded block
        let block_a = build_hardcoded_block(2, 99999);

        // Add the block to the ChainManager
        let hash_a = bm.process_new_block(block_a.clone()).unwrap();

        // Try to get the block from the ChainManager
        let stored_block = bm.try_to_get_block(hash_a).unwrap();

        assert_eq!(stored_block, block_a);
    }

    #[test]
    fn get_non_existent_block() {
        // Create empty ChainManager
        let mut bm = ChainManager::default();

        // Try to get a block with an invented hash
        let result = bm.try_to_get_block(Hash::SHA256([1; 32]));

        // Check that an error was obtained
        assert!(result.is_err());
    }

    #[test]
    fn discard_all() {
        // Create empty ChainManager
        let mut bm = ChainManager::default();

        // Build blocks
        let block_a = build_hardcoded_block(2, 99999);
        let block_b = build_hardcoded_block(1, 10000);
        let block_c = build_hardcoded_block(3, 72138);

        // Add blocks to the ChainManager
        let hash_a = bm.process_new_block(block_a.clone()).unwrap();
        let hash_b = bm.process_new_block(block_b.clone()).unwrap();
        let hash_c = bm.process_new_block(block_c.clone()).unwrap();

        // Build vector of inventory entries from hashes
        let mut inv_entries = Vec::new();
        inv_entries.push(InventoryEntry::Block(hash_a));
        inv_entries.push(InventoryEntry::Block(hash_b));
        inv_entries.push(InventoryEntry::Block(hash_c));

        // Filter inventory entries
        let missing_inv_entries = bm.discard_existing_inventory_entries(inv_entries).unwrap();

        // Check there is no missing inventory entry
        assert!(missing_inv_entries.is_empty());
    }

    #[test]
    fn discard_some() {
        // Create empty ChainManager
        let mut bm = ChainManager::default();

        // Build blocks
        let block_a = build_hardcoded_block(2, 99999);
        let block_b = build_hardcoded_block(1, 10000);
        let block_c = build_hardcoded_block(3, 72138);

        // Add blocks to the ChainManager
        let hash_a = bm.process_new_block(block_a.clone()).unwrap();
        let hash_b = bm.process_new_block(block_b.clone()).unwrap();
        let hash_c = bm.process_new_block(block_c.clone()).unwrap();

        // Missing inventory vector
        let missing_inv_entries = InventoryEntry::Block(Hash::SHA256([1; 32]));

        // Build vector of inventory vectors from hashes
        let mut inv_entries = Vec::new();
        inv_entries.push(InventoryEntry::Block(hash_a));
        inv_entries.push(InventoryEntry::Block(hash_b));
        inv_entries.push(InventoryEntry::Block(hash_c));
        inv_entries.push(missing_inv_entries.clone());

        // Filter inventory vectors
        let expected_missing_inv_entries =
            bm.discard_existing_inventory_entries(inv_entries).unwrap();

        // Check the expected missing inventory vectors
        assert_eq!(vec![missing_inv_entries], expected_missing_inv_entries);
    }

    #[test]
    fn discard_none() {
        // Create empty ChainManager
        let mut bm = ChainManager::default();

        // Build blocks
        let block_a = build_hardcoded_block(2, 99999);
        let block_b = build_hardcoded_block(1, 10000);
        let block_c = build_hardcoded_block(3, 72138);

        // Add blocks to the ChainManager
        bm.process_new_block(block_a.clone()).unwrap();
        bm.process_new_block(block_b.clone()).unwrap();
        bm.process_new_block(block_c.clone()).unwrap();

        // Missing inventory vector
        let missing_inv_entries_1 = InventoryEntry::Block(Hash::SHA256([1; 32]));
        let missing_inv_entries_2 = InventoryEntry::Block(Hash::SHA256([2; 32]));
        let missing_inv_entries_3 = InventoryEntry::Block(Hash::SHA256([3; 32]));

        // Build vector of missing inventory vectors from hashes
        let mut inv_entries = Vec::new();
        inv_entries.push(missing_inv_entries_1);
        inv_entries.push(missing_inv_entries_2);
        inv_entries.push(missing_inv_entries_3);

        // Filter inventory vectors
        let missing_inv_entries = bm
            .discard_existing_inventory_entries(inv_entries.clone())
            .unwrap();

        // Check there is no missing inventory vector
        assert_eq!(missing_inv_entries, inv_entries);
    }

    #[cfg(test)]
    fn build_hardcoded_block(checkpoint: u32, influence: u64) -> Block {
        use witnet_data_structures::chain::*;
        let signed_beacon_hash = [4; 32];
        // Currently, every hash is valid
        // Fake signature which will be accepted anyway
        let signature = Signature::Secp256k1(Secp256k1Signature {
            r: signed_beacon_hash,
            s: signed_beacon_hash,
            v: 0,
        });
        let proof = LeadershipProof {
            block_sig: Some(signature),
            influence,
        };

        Block {
            block_header: BlockHeader {
                version: 1,
                beacon: CheckpointBeacon {
                    checkpoint,
                    hash_prev_block: Hash::SHA256([111; 32]),
                },
                hash_merkle_root: Hash::SHA256([222; 32]),
            },
            proof,
            txns: vec![Transaction],
        }
    }

    #[test]
    fn block_storable() {
        use witnet_data_structures::chain::*;
        use witnet_storage::storage::Storable;

        let b = InventoryItem::Block(build_hardcoded_block(0, 0));
        let msp = b.to_bytes().unwrap();
        assert_eq!(InventoryItem::from_bytes(&msp).unwrap(), b);

        println!("{:?}", b);
        println!("{:?}", msp);
        /*
        use witnet_data_structures::chain::Hash::SHA256;
        use witnet_data_structures::chain::Signature::Secp256k1;
        let mined_block = InventoryItem::Block(Block {
            block_header: BlockHeader {
                version: 0,
                beacon: CheckpointBeacon {
                    checkpoint: 400,
                    hash_prev_block: SHA256([
                        47, 17, 139, 130, 7, 164, 151, 185, 64, 43, 88, 183, 53, 213, 38, 89, 76,
                        66, 231, 53, 78, 216, 230, 217, 245, 184, 150, 33, 182, 15, 111, 38,
                    ]),
                },
                hash_merkle_root: SHA256([
                    227, 176, 196, 66, 152, 252, 28, 20, 154, 251, 244, 200, 153, 111, 185, 36, 39,
                    174, 65, 228, 100, 155, 147, 76, 164, 149, 153, 27, 120, 82, 184, 85,
                ]),
            },
            proof: LeadershipProof {
                block_sig: Some(Secp256k1(Secp256k1Signature {
                    r: [
                        128, 205, 5, 48, 74, 223, 4, 72, 223, 231, 60, 90, 128, 196, 37, 74, 225,
                        60, 123, 112, 167, 2, 28, 201, 210, 41, 9, 128, 136, 223, 228, 35,
                    ],
                    s: [
                        128, 205, 5, 48, 74, 223, 4, 72, 223, 231, 60, 90, 128, 196, 37, 74, 225,
                        60, 123, 112, 167, 2, 28, 201, 210, 41, 9, 128, 136, 223, 228, 35,
                    ],
                    v: 0,
                })),
                influence: 0,
            },
            txns: vec![],
        });
        let raw_block = [146, 1, 145, 147, 147, 0, 146, 205, 1, 144, 146, 0, 145, 220, 0, 32, 47, 17, 204, 139, 204, 130, 7, 204, 164, 204, 151, 204, 185, 64, 43, 88, 204, 183, 53, 204, 213, 38, 89, 76, 66, 204, 231, 53, 78, 204, 216, 204, 230, 204, 217, 204, 245, 204, 184, 204, 150, 33, 204, 182, 15, 111, 38, 146, 0, 145, 220, 0, 32, 204, 227, 204, 176, 204, 196, 66, 204, 152, 204, 252, 28, 20, 204, 154, 204, 251, 204, 244, 204, 200, 204, 153, 111, 204, 185, 36, 39, 204, 174, 65, 204, 228, 100, 204, 155, 204, 147, 76, 204, 164, 204, 149, 204, 153, 27, 120, 82, 204, 184, 85, 146, 146, 0, 145, 147, 220, 0, 32, 204, 128, 204, 205, 5, 48, 74, 204, 223, 4, 72, 204, 223, 204, 231, 60, 90, 204, 128, 204, 196, 37, 74, 204, 225, 60, 123, 112, 204, 167, 2, 28, 204, 201, 204, 210, 41, 9, 204, 128, 204, 136, 204, 223, 204, 228, 35, 220, 0, 32, 204, 128, 204, 205, 5, 48, 74, 204, 223, 4, 72, 204, 223, 204, 231, 60, 90, 204, 128, 204, 196, 37, 74, 204, 225, 60, 123, 112, 204, 167, 2, 28, 204, 201, 204, 210, 41, 9, 204, 128, 204, 136, 204, 223, 204, 228, 35, 0, 0, 144];
        println!("{:?}", mined_block);
        println!("Mined block to bytes:");
        println!("{:?}", mined_block.to_bytes());
        println!("Mined block bytes from storage:");
        println!("{:?}", &raw_block[..]);
        assert_eq!(InventoryItem::from_bytes(&raw_block).unwrap(), mined_block);
        */
    }

    #[test]
    fn block_storable_fail() {
        use witnet_data_structures::chain::Hash::SHA256;
        use witnet_data_structures::chain::Signature::Secp256k1;
        use witnet_data_structures::chain::*;
        use witnet_storage::storage::Storable;

        let mined_block = InventoryItem::Block(Block {
            block_header: BlockHeader {
                version: 0,
                beacon: CheckpointBeacon {
                    checkpoint: 400,
                    hash_prev_block: SHA256([
                        47, 17, 139, 130, 7, 164, 151, 185, 64, 43, 88, 183, 53, 213, 38, 89, 76,
                        66, 231, 53, 78, 216, 230, 217, 245, 184, 150, 33, 182, 15, 111, 38,
                    ]),
                },
                hash_merkle_root: SHA256([
                    227, 176, 196, 66, 152, 252, 28, 20, 154, 251, 244, 200, 153, 111, 185, 36, 39,
                    174, 65, 228, 100, 155, 147, 76, 164, 149, 153, 27, 120, 82, 184, 85,
                ]),
            },
            proof: LeadershipProof {
                block_sig: Some(Secp256k1(Secp256k1Signature {
                    r: [
                        128, 205, 5, 48, 74, 223, 4, 72, 223, 231, 60, 90, 128, 196, 37, 74, 225,
                        60, 123, 112, 167, 2, 28, 201, 210, 41, 9, 128, 136, 223, 228, 35,
                    ],
                    s: [
                        128, 205, 5, 48, 74, 223, 4, 72, 223, 231, 60, 90, 128, 196, 37, 74, 225,
                        60, 123, 112, 167, 2, 28, 201, 210, 41, 9, 128, 136, 223, 228, 35,
                    ],
                    v: 0,
                })),
                influence: 0,
            },
            txns: vec![],
        });
        let msp = mined_block.to_bytes().unwrap();

        assert_eq!(InventoryItem::from_bytes(&msp).unwrap(), mined_block);
    }

    #[test]
    fn leadership_storable() {
        use witnet_data_structures::chain::*;
        use witnet_storage::storage::Storable;
        let signed_beacon_hash = [4; 32];

        let signature = Signature::Secp256k1(Secp256k1Signature {
            r: signed_beacon_hash,
            s: signed_beacon_hash,
            v: 0,
        });
        let a = LeadershipProof {
            block_sig: Some(signature),
            influence: 0,
        };

        let msp = a.to_bytes().unwrap();

        assert_eq!(LeadershipProof::from_bytes(&msp).unwrap(), a);
    }

    #[test]
    fn signature_storable() {
        use witnet_data_structures::chain::*;
        use witnet_storage::storage::Storable;
        let signed_beacon_hash = [4; 32];

        let a = Some(Signature::Secp256k1(Secp256k1Signature {
            r: signed_beacon_hash,
            s: signed_beacon_hash,
            v: 0,
        }));
        let msp = a.to_bytes().unwrap();

        assert_eq!(Option::<Signature>::from_bytes(&msp).unwrap(), a);
    }

    #[test]
    fn som_de_uno() {
        use witnet_storage::storage::Storable;

        let a = Some(Some(1u8));
        let msp = a.to_bytes().unwrap();
        assert_eq!(Option::<Option<u8>>::from_bytes(&msp).unwrap(), a);
    }
}