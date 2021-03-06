use super::*;
use std::sync::Arc;
use witnet_data_structures::chain::EpochConstants;

/// Wallet state snapshot after indexing a block
#[derive(Clone, Debug)]
pub struct StateSnapshot {
    /// Current wallet balance (including pending movements)
    pub balance: model::BalanceInfo,
    /// Block beacon
    pub beacon: model::Beacon,
    /// Next transaction identifier of the wallet
    pub transaction_next_id: u32,
    /// Current UTXO set (including pending movements)
    pub utxo_set: model::UtxoSet,
}

/// A single wallet state. It includes:
///  - fields required to operate wallet accounts (e.g. derive addresses)
///  - on-memory state after indexing pending block transactions
#[derive(Debug)]
pub struct State {
    /// Current account index
    pub account: u32,
    /// Available account indices
    pub available_accounts: Vec<u32>,
    /// Current wallet balance (including pending movements)
    pub balance: model::WalletBalance,
    /// Wallet caption
    pub caption: Option<String>,
    /// List of already existing DB balance movements that need to be updated upon superblock
    /// confirmation
    pub db_movements_to_update: HashMap<String, Vec<model::BalanceMovement>>,
    /// Epoch constants
    pub epoch_constants: EpochConstants,
    /// Keychains used to derive addresses
    pub keychains: [types::ExtendedSK; 2],
    /// Beacon of last block confirmed by superblock (or during sync process)
    pub last_confirmed: CheckpointBeacon,
    /// Beacon of the last block received during synchronization
    pub last_sync: CheckpointBeacon,
    /// List of local pending balance movements derived from transaction submissions by wallet clients
    /// (they have not yet been indexed in blocks)
    pub local_movements: HashMap<types::Hash, model::BalanceMovement>,
    /// Wallet name
    pub name: Option<String>,
    /// Next external index used to derive addresses
    pub next_external_index: u32,
    /// Next internal index used to derive addresses
    pub next_internal_index: u32,
    /// List of pending address infos indexed by block hash, waiting to be confirmed with a superblock
    ///  This is a hashmap from pending_block_hash to Vec<addresses>.
    pub pending_addresses_by_block: HashMap<String, Vec<Arc<model::Address>>>,
    /// List of pending address infos indexed by key path, waiting to be confirmed with a superblock
    pub pending_addresses_by_path: HashMap<String, Arc<model::Address>>,
    /// List of pending blocks with state snapshots waiting to be confirmed
    ///  This is a hashmap from pending_block_hash to StateSnapshot.
    pub pending_blocks: HashMap<String, StateSnapshot>,
    /// List of pending dr movements, waiting to be confirmed with a superblock
    /// This is a hashmap from dr_pointer to (pending_block_hash, index).
    pub pending_dr_movements: HashMap<String, (Hash, usize)>,
    /// List of pending balance movements, waiting to be confirmed with a superblock
    ///  This is a hashmap from pending_block_hash to (Vec<BalanceMovement).
    pub pending_movements: HashMap<String, Vec<model::BalanceMovement>>,
    /// Next transaction identifier of the wallet
    pub transaction_next_id: u32,
    /// Current UTXO set (including pending movements)
    pub utxo_set: model::UtxoSet,
    /// Transient internal addresses
    pub transient_internal_addresses: HashMap<types::PublicKeyHash, model::Address>,
    /// Transient external addresses
    pub transient_external_addresses: HashMap<types::PublicKeyHash, model::Address>,
}
