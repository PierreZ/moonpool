//! Pluggable message serialization for moonpool.
//!
//! The [`MessageCodec`] trait allows users to bring their own serialization format
//! (JSON, bincode, protobuf, messagepack, etc.) while moonpool provides a default
//! [`JsonCodec`] for debugging and getting started quickly.
//!
//! # Example
//!
//! ```rust
//! use moonpool_core::{MessageCodec, JsonCodec, CodecError};
//! use serde::{Serialize, Deserialize};
//!
//! #[derive(Serialize, Deserialize, Debug, PartialEq)]
//! struct MyMessage {
//!     id: u32,
//!     content: String,
//! }
//!
//! let codec = JsonCodec;
//! let msg = MyMessage { id: 42, content: "hello".to_string() };
//!
//! // Encode
//! let bytes = codec.encode(&msg).unwrap();
//!
//! // Decode
//! let decoded: MyMessage = codec.decode(&bytes).unwrap();
//! assert_eq!(msg, decoded);
//! ```
//!
//! # Implementing Custom Codecs
//!
//! ```rust
//! use moonpool_core::{MessageCodec, CodecError};
//! use serde::{Serialize, de::DeserializeOwned};
//!
//! #[derive(Clone, Default)]
//! struct BincodeCodec;
//!
//! // impl MessageCodec for BincodeCodec {
//! //     fn encode<T: Serialize>(&self, msg: &T) -> Result<Vec<u8>, CodecError> {
//! //         bincode::serialize(msg).map_err(|e| CodecError::Encode(e.into()))
//! //     }
//! //
//! //     fn decode<T: DeserializeOwned>(&self, buf: &[u8]) -> Result<T, CodecError> {
//! //         bincode::deserialize(buf).map_err(|e| CodecError::Decode(e.into()))
//! //     }
//! // }
//! ```

use std::fmt;

use serde::Serialize;
use serde::de::DeserializeOwned;

/// Error type for codec operations.
#[derive(Debug)]
pub enum CodecError {
    /// Failed to encode a message to bytes.
    Encode(Box<dyn std::error::Error + Send + Sync>),
    /// Failed to decode bytes to a message.
    Decode(Box<dyn std::error::Error + Send + Sync>),
}

impl fmt::Display for CodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CodecError::Encode(e) => write!(f, "encode error: {}", e),
            CodecError::Decode(e) => write!(f, "decode error: {}", e),
        }
    }
}

impl std::error::Error for CodecError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            CodecError::Encode(e) => Some(e.as_ref()),
            CodecError::Decode(e) => Some(e.as_ref()),
        }
    }
}

/// Pluggable message serialization format.
///
/// Implement this trait to use custom serialization formats (bincode, protobuf, etc.).
/// The trait requires `Clone + 'static` so codec instances can be stored in queues
/// and transports.
///
/// # Serde Dependency
///
/// This trait uses serde's `Serialize` and `DeserializeOwned` bounds, which means
/// your message types must derive or implement serde traits. If you need completely
/// custom serialization without serde, you can implement your own queue type.
pub trait MessageCodec: Clone + 'static {
    /// Encode a serializable message to bytes.
    ///
    /// # Errors
    ///
    /// Returns `CodecError::Encode` if serialization fails.
    fn encode<T: Serialize>(&self, msg: &T) -> Result<Vec<u8>, CodecError>;

    /// Decode bytes to a deserializable message.
    ///
    /// # Errors
    ///
    /// Returns `CodecError::Decode` if deserialization fails.
    fn decode<T: DeserializeOwned>(&self, buf: &[u8]) -> Result<T, CodecError>;
}

/// JSON codec using serde_json.
///
/// This is the default codec provided by moonpool. It's great for debugging
/// (human-readable output) but not the most efficient for production use.
///
/// # Example
///
/// ```rust
/// use moonpool_core::{MessageCodec, JsonCodec};
/// use serde::{Serialize, Deserialize};
///
/// #[derive(Serialize, Deserialize, PartialEq, Debug)]
/// struct Ping { seq: u32 }
///
/// let codec = JsonCodec;
/// let ping = Ping { seq: 1 };
///
/// let bytes = codec.encode(&ping).unwrap();
/// assert_eq!(&bytes, br#"{"seq":1}"#);
///
/// let decoded: Ping = codec.decode(&bytes).unwrap();
/// assert_eq!(decoded, ping);
/// ```
#[derive(Clone, Default, Debug, Copy)]
pub struct JsonCodec;

impl MessageCodec for JsonCodec {
    fn encode<T: Serialize>(&self, msg: &T) -> Result<Vec<u8>, CodecError> {
        serde_json::to_vec(msg).map_err(|e| CodecError::Encode(Box::new(e)))
    }

    fn decode<T: DeserializeOwned>(&self, buf: &[u8]) -> Result<T, CodecError> {
        serde_json::from_slice(buf).map_err(|e| CodecError::Decode(Box::new(e)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};

    #[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
    struct TestMessage {
        id: u32,
        content: String,
    }

    #[derive(Serialize, Deserialize, Debug, PartialEq)]
    struct NestedMessage {
        outer: String,
        inner: TestMessage,
    }

    #[test]
    fn test_json_codec_roundtrip() {
        let codec = JsonCodec;
        let msg = TestMessage {
            id: 42,
            content: "hello world".to_string(),
        };

        let bytes = codec.encode(&msg).expect("encode should succeed");
        let decoded: TestMessage = codec.decode(&bytes).expect("decode should succeed");

        assert_eq!(msg, decoded);
    }

    #[test]
    fn test_json_codec_nested() {
        let codec = JsonCodec;
        let msg = NestedMessage {
            outer: "outer".to_string(),
            inner: TestMessage {
                id: 1,
                content: "inner".to_string(),
            },
        };

        let bytes = codec.encode(&msg).expect("encode should succeed");
        let decoded: NestedMessage = codec.decode(&bytes).expect("decode should succeed");

        assert_eq!(msg, decoded);
    }

    #[test]
    fn test_json_codec_primitives() {
        let codec = JsonCodec;

        // String
        let s = "test string".to_string();
        let bytes = codec.encode(&s).expect("encode should succeed");
        let decoded: String = codec.decode(&bytes).expect("decode should succeed");
        assert_eq!(s, decoded);

        // Integer
        let n = 12345u64;
        let bytes = codec.encode(&n).expect("encode should succeed");
        let decoded: u64 = codec.decode(&bytes).expect("decode should succeed");
        assert_eq!(n, decoded);

        // Vec
        let v = vec![1, 2, 3, 4, 5];
        let bytes = codec.encode(&v).expect("encode should succeed");
        let decoded: Vec<i32> = codec.decode(&bytes).expect("decode should succeed");
        assert_eq!(v, decoded);
    }

    #[test]
    fn test_json_codec_decode_error() {
        let codec = JsonCodec;
        let invalid_json = b"not valid json {";

        let result: Result<TestMessage, CodecError> = codec.decode(invalid_json);
        assert!(result.is_err());

        let err = result.unwrap_err();
        assert!(matches!(err, CodecError::Decode(_)));
        assert!(err.to_string().contains("decode error"));
    }

    #[test]
    fn test_json_codec_type_mismatch() {
        let codec = JsonCodec;
        let msg = TestMessage {
            id: 42,
            content: "hello".to_string(),
        };

        let bytes = codec.encode(&msg).expect("encode should succeed");

        // Try to decode as wrong type
        let result: Result<String, CodecError> = codec.decode(&bytes);
        assert!(result.is_err());
    }

    #[test]
    fn test_codec_error_display() {
        let encode_err = CodecError::Encode(Box::new(std::io::Error::other("test encode error")));
        assert!(encode_err.to_string().contains("encode error"));

        let decode_err = CodecError::Decode(Box::new(std::io::Error::other("test decode error")));
        assert!(decode_err.to_string().contains("decode error"));
    }

    #[test]
    fn test_json_codec_is_clone() {
        let codec1 = JsonCodec;
        let codec2 = codec1.clone();

        let msg = TestMessage {
            id: 1,
            content: "test".to_string(),
        };

        // Both codecs should work identically
        let bytes1 = codec1.encode(&msg).expect("encode should succeed");
        let bytes2 = codec2.encode(&msg).expect("encode should succeed");
        assert_eq!(bytes1, bytes2);
    }

    #[test]
    fn test_json_codec_default() {
        let codec = JsonCodec::default();
        let msg = TestMessage {
            id: 99,
            content: "default".to_string(),
        };

        let bytes = codec.encode(&msg).expect("encode should succeed");
        let decoded: TestMessage = codec.decode(&bytes).expect("decode should succeed");
        assert_eq!(msg, decoded);
    }

    #[test]
    fn test_json_codec_empty_struct() {
        #[derive(Serialize, Deserialize, Debug, PartialEq)]
        struct Empty {}

        let codec = JsonCodec;
        let msg = Empty {};

        let bytes = codec.encode(&msg).expect("encode should succeed");
        assert_eq!(&bytes, b"{}");

        let decoded: Empty = codec.decode(&bytes).expect("decode should succeed");
        assert_eq!(msg, decoded);
    }

    #[test]
    fn test_json_codec_option() {
        let codec = JsonCodec;

        let some_val: Option<i32> = Some(42);
        let bytes = codec.encode(&some_val).expect("encode should succeed");
        let decoded: Option<i32> = codec.decode(&bytes).expect("decode should succeed");
        assert_eq!(some_val, decoded);

        let none_val: Option<i32> = None;
        let bytes = codec.encode(&none_val).expect("encode should succeed");
        let decoded: Option<i32> = codec.decode(&bytes).expect("decode should succeed");
        assert_eq!(none_val, decoded);
    }
}
