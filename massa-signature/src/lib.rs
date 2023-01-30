// Copyright (c) 2022 MASSA LABS <info@massa.net>
//! Signature management

#![warn(missing_docs)]
#![warn(unused_crate_dependencies)]
mod error;
mod signature_impl;

#[macro_use]
mod multi_v2;

pub use error::MassaSignatureError;
/*pub use signature_impl::{
    verify_signature_batch, KeyPair, PublicKey, PublicKeyDeserializer, Signature,
    SignatureDeserializer, PUBLIC_KEY_SIZE_BYTES, SECRET_KEY_BYTES_SIZE, SIGNATURE_SIZE_BYTES,
};*/
pub use multi_v2::{
    verify_signature_batch, KeyPair, PublicKey, PublicKeyDeserializer, Signature,
    SignatureDeserializer,
};

pub const PUBLIC_KEY_SIZE_BYTES_V1: usize = 32;
pub const SIGNATURE_SIZE_BYTES_V1: usize = 32;
