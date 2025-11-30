//! # moonpool-traits
//!
//! Traits and common types for the moonpool simulation framework.
//!
//! This crate provides the core abstractions that allow moonpool to be extended:
//!
//! - [`MessageCodec`] - Pluggable serialization format (JSON, bincode, protobuf, etc.)
//! - [`JsonCodec`] - Default JSON codec using serde_json
//!
//! ## Pluggable Serialization
//!
//! moonpool uses the [`MessageCodec`] trait to abstract over serialization formats.
//! This allows users to bring their own serialization while moonpool provides
//! [`JsonCodec`] as a sensible default for debugging and development.
//!
//! ```rust
//! use moonpool_traits::{MessageCodec, JsonCodec};
//! use serde::{Serialize, Deserialize};
//!
//! #[derive(Serialize, Deserialize, PartialEq, Debug)]
//! struct MyMessage { value: i32 }
//!
//! let codec = JsonCodec;
//! let msg = MyMessage { value: 42 };
//!
//! let bytes = codec.encode(&msg).unwrap();
//! let decoded: MyMessage = codec.decode(&bytes).unwrap();
//!
//! assert_eq!(msg, decoded);
//! ```
//!
//! ## Custom Codec Example
//!
//! To use a different serialization format, implement [`MessageCodec`]:
//!
//! ```rust,ignore
//! use moonpool_traits::{MessageCodec, CodecError};
//!
//! #[derive(Clone)]
//! struct BincodeCodec;
//!
//! impl MessageCodec for BincodeCodec {
//!     fn encode<T: serde::Serialize>(&self, msg: &T) -> Result<Vec<u8>, CodecError> {
//!         bincode::serialize(msg).map_err(|e| CodecError::Encode(e.into()))
//!     }
//!
//!     fn decode<T: serde::de::DeserializeOwned>(&self, buf: &[u8]) -> Result<T, CodecError> {
//!         bincode::deserialize(buf).map_err(|e| CodecError::Decode(e.into()))
//!     }
//! }
//! ```

mod codec;

pub use codec::{CodecError, JsonCodec, MessageCodec};
