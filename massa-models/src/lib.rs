// Copyright (c) 2021 MASSA LABS <info@massa.net>

#[macro_use]
extern crate lazy_static;

pub use address::Address;
pub use amount::{Amount, AMOUNT_ZERO};
pub use block::{Block, BlockHeader, BlockHeaderContent, BlockId};
pub use composite::{
    OperationSearchResult, OperationSearchResultBlockStatus, OperationSearchResultStatus,
    StakersCycleProductionStats,
};
pub use context::{
    get_serialization_context, init_serialization_context, with_serialization_context,
    SerializationContext,
};
pub use endorsement::{Endorsement, EndorsementContent, EndorsementId};
pub use error::ModelsError;
pub use operation::{Operation, OperationContent, OperationId, OperationType};
pub use serialization::{
    array_from_slice, u8_from_slice, DeserializeCompact, DeserializeMinBEInt, DeserializeVarInt,
    SerializeCompact, SerializeMinBEInt, SerializeVarInt,
};
pub use settings::CompactConfig;

pub use slot::Slot;
pub use version::Version;

pub mod address;
pub mod amount;
pub mod api;
mod block;
pub mod clique;
pub mod composite;
mod context;
mod endorsement;
pub mod error;
pub mod execution;
pub mod ledger_models;
pub mod node;
pub mod operation;
pub mod output_event;
pub mod prehash;
mod serialization;
mod settings;
pub mod slot;
pub mod stats;
pub mod timeslots;
mod version;

pub mod active_block;
pub mod rolls;

mod constants;
pub use constants::{
    thread_count, ADDRESS_SIZE_BYTES, AMOUNT_DECIMAL_FACTOR, BLOCK_ID_SIZE_BYTES,
    ENDORSEMENT_ID_SIZE_BYTES, OPERATION_ID_SIZE_BYTES, SLOT_KEY_SIZE,
};

#[cfg(feature = "test-utils")]
pub use constants::{reset_config, set_thread_count};
