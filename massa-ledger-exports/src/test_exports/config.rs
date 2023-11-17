// Copyright (c) 2022 MASSA LABS <info@massa.net>
/// This file defines testing tools related to the configuration
use massa_models::{
    address::Address,
    config::{MAX_DATASTORE_KEY_LENGTH, MAX_DATASTORE_VALUE_LENGTH, THREAD_COUNT},
};
use std::collections::HashMap;
use std::io::Seek;
use tempfile::NamedTempFile;

use crate::{LedgerConfig, LedgerEntry};

/// Default value of `LedgerConfig` used for tests
impl Default for LedgerConfig {
    fn default() -> Self {
        LedgerConfig {
            // unused by the mock (you can use `LedgerConfig::sample()` to get
            // a NamedTempFile in addition)
            initial_ledger_path: "".into(),
            thread_count: THREAD_COUNT,
            max_key_length: MAX_DATASTORE_KEY_LENGTH,
            max_datastore_value_length: MAX_DATASTORE_VALUE_LENGTH,
        }
    }
}

impl LedgerConfig {
    /// get ledger and ledger configuration
    pub fn sample(ledger: &HashMap<Address, LedgerEntry>) -> (Self, NamedTempFile) {
        let initial_ledger = NamedTempFile::new().expect("cannot create temp file");
        serde_json::to_writer_pretty(initial_ledger.as_file(), &ledger)
            .expect("unable to write ledger file");
        initial_ledger
            .as_file()
            .seek(std::io::SeekFrom::Start(0))
            .expect("could not seek file");
        (
            Self {
                initial_ledger_path: initial_ledger.path().to_path_buf(),
                max_key_length: MAX_DATASTORE_KEY_LENGTH,
                thread_count: THREAD_COUNT,
                max_datastore_value_length: MAX_DATASTORE_VALUE_LENGTH,
            },
            initial_ledger,
        )
    }
}
