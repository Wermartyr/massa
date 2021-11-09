// Copyright (c) 2021 MASSA LABS <info@massa.net>

use consensus::ConsensusError;
use crypto::CryptoError;
use displaydoc::Display;
use models::ModelsError;
use network::NetworkError;
use pool::PoolError;
use thiserror::Error;
use time::TimeError;

#[non_exhaustive]
#[derive(Display, Error, Debug)]
pub enum ApiError {
    /// pool error: {0}
    PoolError(#[from] PoolError),
    /// too many arguments error: {0}
    TooManyArguments(String),
    /// send channel error: {0}
    SendChannelError(String),
    /// receive channel error: {0}
    ReceiveChannelError(String),
    /// crypto error: {0}
    CryptoError(#[from] CryptoError),
    /// consensus error: {0}
    ConsensusError(#[from] ConsensusError),
    /// network error: {0}
    NetworkError(#[from] NetworkError),
    /// models error: {0}
    ModelsError(#[from] ModelsError),
    /// time error: {0}
    TimeError(#[from] TimeError),
    /// not found
    NotFound,
    /// inconsistency: {0}
    InconsistencyError(String),
    /// missing command sender {0}
    MissingCommandSender(String),
    /// missing config {0}
    MissingConfig(String),
    /// the wrong API (either Public or Private) was called
    WrongAPI,
}

impl From<ApiError> for jsonrpc_core::Error {
    fn from(err: ApiError) -> Self {
        jsonrpc_core::Error {
            code: jsonrpc_core::ErrorCode::ServerError(500),
            message: err.to_string(),
            data: None,
        }
    }
}
