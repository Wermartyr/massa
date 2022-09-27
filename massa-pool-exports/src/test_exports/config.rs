// Copyright (c) 2022 MASSA LABS <info@massa.net>

use massa_models::config::{
    ENDORSEMENT_COUNT, GENESIS_TIMESTAMP, MAX_BLOCK_SIZE, MAX_GAS_PER_BLOCK,
    OPERATION_VALIDITY_PERIODS, ROLL_PRICE, T0, THREAD_COUNT,
};
use massa_time::MassaTime;

use crate::PoolConfig;

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            thread_count: THREAD_COUNT,
            operation_validity_periods: OPERATION_VALIDITY_PERIODS,
            max_block_gas: MAX_GAS_PER_BLOCK,
            roll_price: ROLL_PRICE,
            max_block_size: MAX_BLOCK_SIZE,
            max_operation_pool_size_per_thread: 1000,
            max_endorsements_pool_size_per_thread: 1000,
            max_block_endorsement_count: ENDORSEMENT_COUNT,
            protection_time: MassaTime::from_millis(0),
            protection_batch_size: 0,
            t0: T0,
            genesis_timestamp: *GENESIS_TIMESTAMP,
        }
    }
}
