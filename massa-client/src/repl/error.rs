//!Contains all error generated by repl module

use thiserror::Error;

#[derive(Error, Debug)]
pub enum ReplError {
    #[error("error:{0}")]
    GeneralError(String),
    #[error("Error during command parsing")]
    ParseCommandError,
    #[error("Error command:{0} not found")]
    CommandNotFoundError(String),
    #[error("Node connection error err:{0}")]
    NodeConnectionError(#[from] reqwest::Error),
    #[error("Bad input parameter : {0}")]
    BadCommandParameter(String),
    #[error("Error can't create address from specifed hash cause:: {0}")]
    AddressCreationError(String),
    #[error("IO error err:{0}")]
    IOError(#[from] std::io::Error),
    #[error("JSON error err:{0}")]
    JSONError(#[from] serde_json::Error),
}
