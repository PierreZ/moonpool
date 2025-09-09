use std::fmt::Debug;

/// Trait for swappable envelope serialization strategies.
///
/// Users can implement this trait to provide custom envelope formats.
/// This allows for future enhancements like Orleans-style rich metadata
/// or custom user implementations without modifying the core protocol logic.
pub trait EnvelopeSerializer: Clone {
    /// The envelope type that this serializer handles
    type Envelope: Debug + Clone;

    /// Serialize envelope to bytes for network transmission
    fn serialize(&self, envelope: &Self::Envelope) -> Vec<u8>;

    /// Deserialize bytes back to envelope
    fn deserialize(&self, data: &[u8]) -> Result<Self::Envelope, SerializationError>;
}

/// Errors that can occur during envelope serialization/deserialization
#[derive(Debug, Clone, PartialEq)]
pub enum SerializationError {
    /// Data is too short to contain a valid envelope
    TooShort,
    /// Data format is invalid or corrupted
    InvalidFormat,
    /// UTF-8 conversion failed
    InvalidUtf8,
}
