//! Wire format for packet serialization.
//!
//! Packet format: `[length:4][checksum:4][token:16][payload:N]`
//!
//! - **length**: Total packet size including header (little-endian u32)
//! - **checksum**: CRC32C of (token + payload) for integrity verification
//! - **token**: Destination endpoint UID (two little-endian u64)
//! - **payload**: Application data (custom serialization)

use crate::UID;

/// Header size: 4 (length) + 4 (checksum) + 16 (token) = 24 bytes.
pub const HEADER_SIZE: usize = 24;

/// Maximum payload size (1MB).
///
/// Packets larger than this are rejected to prevent memory exhaustion attacks.
pub const MAX_PAYLOAD_SIZE: usize = 1024 * 1024;

/// Wire format error types.
#[derive(Debug, Clone, thiserror::Error)]
pub enum WireError {
    /// Not enough data to parse the packet.
    #[error("insufficient data: need {needed} bytes, have {have}")]
    InsufficientData {
        /// Minimum bytes required to parse.
        needed: usize,
        /// Actual bytes available.
        have: usize,
    },

    /// Checksum verification failed - data was corrupted.
    #[error("checksum mismatch: expected {expected:#010x}, got {actual:#010x}")]
    ChecksumMismatch {
        /// Expected checksum from header.
        expected: u32,
        /// Computed checksum from data.
        actual: u32,
    },

    /// Payload exceeds maximum allowed size.
    #[error("packet too large: {size} bytes (max {MAX_PAYLOAD_SIZE})")]
    PacketTooLarge {
        /// Actual payload size in bytes.
        size: usize,
    },

    /// Length field has an invalid value.
    #[error("invalid packet length: {length}")]
    InvalidLength {
        /// The invalid length value from the header.
        length: u32,
    },
}

/// Packet header for wire format.
///
/// Contains the fixed-size header fields that precede the payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PacketHeader {
    /// Total packet size including header.
    pub length: u32,
    /// CRC32C checksum of (token + payload).
    pub checksum: u32,
    /// Destination endpoint token.
    pub token: UID,
}

impl PacketHeader {
    /// Serialize header into buffer (must be at least HEADER_SIZE bytes).
    ///
    /// # Panics
    ///
    /// Panics if buffer is smaller than HEADER_SIZE.
    pub fn serialize_into(&self, buf: &mut [u8]) {
        debug_assert!(buf.len() >= HEADER_SIZE);
        buf[0..4].copy_from_slice(&self.length.to_le_bytes());
        buf[4..8].copy_from_slice(&self.checksum.to_le_bytes());
        buf[8..16].copy_from_slice(&self.token.first.to_le_bytes());
        buf[16..24].copy_from_slice(&self.token.second.to_le_bytes());
    }

    /// Deserialize header from buffer.
    ///
    /// # Errors
    ///
    /// Returns `InsufficientData` if buffer is smaller than HEADER_SIZE.
    pub fn deserialize(buf: &[u8]) -> Result<Self, WireError> {
        if buf.len() < HEADER_SIZE {
            return Err(WireError::InsufficientData {
                needed: HEADER_SIZE,
                have: buf.len(),
            });
        }

        let length = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
        let checksum = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
        let first = u64::from_le_bytes([
            buf[8], buf[9], buf[10], buf[11], buf[12], buf[13], buf[14], buf[15],
        ]);
        let second = u64::from_le_bytes([
            buf[16], buf[17], buf[18], buf[19], buf[20], buf[21], buf[22], buf[23],
        ]);

        Ok(Self {
            length,
            checksum,
            token: UID::new(first, second),
        })
    }
}

/// Compute CRC32C checksum over token + payload.
fn compute_checksum(token: UID, payload: &[u8]) -> u32 {
    let mut data = Vec::with_capacity(16 + payload.len());
    data.extend_from_slice(&token.first.to_le_bytes());
    data.extend_from_slice(&token.second.to_le_bytes());
    data.extend_from_slice(payload);
    crc32c::crc32c(&data)
}

/// Serialize a packet with token and payload.
///
/// Returns: `[length:4][checksum:4][token:16][payload:N]`
///
/// # Errors
///
/// Returns `PacketTooLarge` if payload exceeds MAX_PAYLOAD_SIZE.
///
/// # Examples
///
/// ```
/// use moonpool_transport::{UID, serialize_packet, deserialize_packet};
///
/// let token = UID::new(1, 2);
/// let payload = b"hello";
///
/// let packet = serialize_packet(token, payload).expect("serialize");
/// let (recv_token, recv_payload) = deserialize_packet(&packet).expect("deserialize");
///
/// assert_eq!(token, recv_token);
/// assert_eq!(payload.as_slice(), recv_payload.as_slice());
/// ```
pub fn serialize_packet(token: UID, payload: &[u8]) -> Result<Vec<u8>, WireError> {
    if payload.len() > MAX_PAYLOAD_SIZE {
        return Err(WireError::PacketTooLarge {
            size: payload.len(),
        });
    }

    let total_length = HEADER_SIZE + payload.len();
    let mut data = vec![0u8; total_length];

    let checksum = compute_checksum(token, payload);

    let header = PacketHeader {
        length: total_length as u32,
        checksum,
        token,
    };

    header.serialize_into(&mut data[..HEADER_SIZE]);
    data[HEADER_SIZE..].copy_from_slice(payload);

    Ok(data)
}

/// Deserialize a packet, validating checksum.
///
/// # Errors
///
/// - `InsufficientData`: Not enough bytes to parse header or full packet
/// - `ChecksumMismatch`: Data was corrupted
/// - `InvalidLength`: Length field is malformed
///
/// # Examples
///
/// ```
/// use moonpool_transport::{UID, serialize_packet, deserialize_packet};
///
/// let token = UID::new(1, 2);
/// let packet = serialize_packet(token, b"test").expect("serialize");
///
/// let (recv_token, payload) = deserialize_packet(&packet).expect("deserialize");
/// assert_eq!(token, recv_token);
/// ```
pub fn deserialize_packet(data: &[u8]) -> Result<(UID, Vec<u8>), WireError> {
    let header = PacketHeader::deserialize(data)?;

    // Validate length field
    if header.length < HEADER_SIZE as u32 {
        return Err(WireError::InvalidLength {
            length: header.length,
        });
    }

    let expected_len = header.length as usize;
    if data.len() < expected_len {
        return Err(WireError::InsufficientData {
            needed: expected_len,
            have: data.len(),
        });
    }

    let payload = &data[HEADER_SIZE..expected_len];

    // Validate checksum
    let computed = compute_checksum(header.token, payload);
    if computed != header.checksum {
        return Err(WireError::ChecksumMismatch {
            expected: header.checksum,
            actual: computed,
        });
    }

    Ok((header.token, payload.to_vec()))
}

/// Try to deserialize from a buffer that may contain partial data.
///
/// This is useful for streaming scenarios where packets arrive incrementally.
///
/// # Returns
///
/// - `Ok(Some((token, payload, consumed)))` if a complete packet was parsed
/// - `Ok(None)` if more data is needed (not an error condition)
/// - `Err` if data is malformed
///
/// # Examples
///
/// ```
/// use moonpool_transport::{UID, serialize_packet, try_deserialize_packet};
///
/// let token = UID::new(1, 2);
/// let packet = serialize_packet(token, b"test").expect("serialize");
///
/// // Partial data returns None
/// assert!(try_deserialize_packet(&packet[..10]).expect("partial").is_none());
///
/// // Complete packet returns Some
/// let result = try_deserialize_packet(&packet).expect("complete");
/// assert!(result.is_some());
/// ```
pub fn try_deserialize_packet(data: &[u8]) -> Result<Option<(UID, Vec<u8>, usize)>, WireError> {
    if data.len() < HEADER_SIZE {
        return Ok(None); // Need more data for header
    }

    let header = PacketHeader::deserialize(data)?;

    if header.length < HEADER_SIZE as u32 {
        return Err(WireError::InvalidLength {
            length: header.length,
        });
    }

    let expected_len = header.length as usize;
    if data.len() < expected_len {
        return Ok(None); // Need more data for payload
    }

    let payload = &data[HEADER_SIZE..expected_len];

    // Validate checksum
    let computed = compute_checksum(header.token, payload);
    if computed != header.checksum {
        return Err(WireError::ChecksumMismatch {
            expected: header.checksum,
            actual: computed,
        });
    }

    Ok(Some((header.token, payload.to_vec(), expected_len)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::WellKnownToken;

    #[test]
    fn test_serialize_deserialize_roundtrip() {
        let token = UID::new(0x123456789ABCDEF0, 0xFEDCBA9876543210);
        let payload = b"hello world";

        let packet = serialize_packet(token, payload).expect("serialize");
        let (recv_token, recv_payload) = deserialize_packet(&packet).expect("deserialize");

        assert_eq!(token, recv_token);
        assert_eq!(payload.as_slice(), recv_payload.as_slice());
    }

    #[test]
    fn test_checksum_validation() {
        let token = UID::new(1, 2);
        let packet = serialize_packet(token, b"test").expect("serialize");

        // Corrupt the payload
        let mut corrupted = packet.clone();
        corrupted[HEADER_SIZE] ^= 0xFF;

        let result = deserialize_packet(&corrupted);
        assert!(matches!(result, Err(WireError::ChecksumMismatch { .. })));
    }

    #[test]
    fn test_checksum_header_corruption() {
        let token = UID::new(1, 2);
        let packet = serialize_packet(token, b"test").expect("serialize");

        // Corrupt the token in header (this will cause checksum mismatch)
        let mut corrupted = packet.clone();
        corrupted[10] ^= 0xFF;

        let result = deserialize_packet(&corrupted);
        assert!(matches!(result, Err(WireError::ChecksumMismatch { .. })));
    }

    #[test]
    fn test_well_known_token() {
        let token = UID::well_known(WellKnownToken::Ping as u32);
        assert!(token.is_well_known());
        assert_eq!(token.first, u64::MAX);
        assert_eq!(token.second, 1);

        // Verify serialization works with well-known tokens
        let packet = serialize_packet(token, b"ping").expect("serialize");
        let (recv_token, _) = deserialize_packet(&packet).expect("deserialize");
        assert_eq!(token, recv_token);
    }

    #[test]
    fn test_insufficient_data_header() {
        let result = deserialize_packet(&[0u8; 10]);
        assert!(matches!(
            result,
            Err(WireError::InsufficientData {
                needed: HEADER_SIZE,
                have: 10
            })
        ));
    }

    #[test]
    fn test_insufficient_data_payload() {
        let token = UID::new(1, 2);
        let packet = serialize_packet(token, b"test data that is longer").expect("serialize");

        // Take only header + partial payload
        let partial = &packet[..HEADER_SIZE + 5];
        let result = deserialize_packet(partial);
        assert!(matches!(result, Err(WireError::InsufficientData { .. })));
    }

    #[test]
    fn test_try_deserialize_partial_header() {
        let token = UID::new(1, 2);
        let packet = serialize_packet(token, b"test data").expect("serialize");

        // Partial header
        let result = try_deserialize_packet(&packet[..10]);
        assert!(matches!(result, Ok(None)));
    }

    #[test]
    fn test_try_deserialize_partial_payload() {
        let token = UID::new(1, 2);
        let packet = serialize_packet(token, b"test data").expect("serialize");

        // Header complete but payload partial
        let result = try_deserialize_packet(&packet[..HEADER_SIZE + 2]);
        assert!(matches!(result, Ok(None)));
    }

    #[test]
    fn test_try_deserialize_complete() {
        let token = UID::new(1, 2);
        let packet = serialize_packet(token, b"test data").expect("serialize");

        // Complete packet
        let result = try_deserialize_packet(&packet).expect("deserialize");
        assert!(result.is_some());

        let (recv_token, recv_payload, consumed) = result.expect("has data");
        assert_eq!(token, recv_token);
        assert_eq!(b"test data".as_slice(), recv_payload.as_slice());
        assert_eq!(consumed, packet.len());
    }

    #[test]
    fn test_try_deserialize_with_extra_data() {
        let token = UID::new(1, 2);
        let packet = serialize_packet(token, b"test").expect("serialize");

        // Add extra data after packet
        let mut extended = packet.clone();
        extended.extend_from_slice(b"extra garbage");

        let result = try_deserialize_packet(&extended).expect("deserialize");
        let (recv_token, recv_payload, consumed) = result.expect("has data");

        assert_eq!(token, recv_token);
        assert_eq!(b"test".as_slice(), recv_payload.as_slice());
        assert_eq!(consumed, packet.len()); // Only original packet consumed
    }

    #[test]
    fn test_adjusted_uid() {
        let base = UID::new(0x1000, 0x2000);
        let adj1 = base.adjusted(1);
        let adj2 = base.adjusted(2);

        // Each adjusted UID should be unique
        assert_ne!(base, adj1);
        assert_ne!(adj1, adj2);
        assert_ne!(base, adj2);

        // Verify serialization works with adjusted UIDs
        for (i, uid) in [base, adj1, adj2].iter().enumerate() {
            let packet = serialize_packet(*uid, format!("msg{}", i).as_bytes()).expect("serialize");
            let (recv_token, _) = deserialize_packet(&packet).expect("deserialize");
            assert_eq!(*uid, recv_token);
        }
    }

    #[test]
    fn test_empty_payload() {
        let token = UID::new(42, 43);
        let packet = serialize_packet(token, &[]).expect("serialize");

        assert_eq!(packet.len(), HEADER_SIZE);

        let (recv_token, recv_payload) = deserialize_packet(&packet).expect("deserialize");
        assert_eq!(token, recv_token);
        assert!(recv_payload.is_empty());
    }

    #[test]
    fn test_packet_too_large() {
        let token = UID::new(1, 1);
        let large_payload = vec![0u8; MAX_PAYLOAD_SIZE + 1];

        let result = serialize_packet(token, &large_payload);
        assert!(matches!(result, Err(WireError::PacketTooLarge { .. })));
    }

    #[test]
    fn test_max_size_payload() {
        let token = UID::new(1, 1);
        let max_payload = vec![0xAB; MAX_PAYLOAD_SIZE];

        let packet = serialize_packet(token, &max_payload).expect("serialize");
        let (recv_token, recv_payload) = deserialize_packet(&packet).expect("deserialize");

        assert_eq!(token, recv_token);
        assert_eq!(max_payload, recv_payload);
    }

    #[test]
    fn test_invalid_length_too_small() {
        // Create packet with length field smaller than header size
        let mut bad_packet = vec![0u8; HEADER_SIZE];
        bad_packet[0..4].copy_from_slice(&10u32.to_le_bytes()); // length = 10 < 24

        let result = deserialize_packet(&bad_packet);
        assert!(matches!(
            result,
            Err(WireError::InvalidLength { length: 10 })
        ));
    }

    #[test]
    fn test_header_serialization() {
        let header = PacketHeader {
            length: 100,
            checksum: 0xDEADBEEF,
            token: UID::new(0x1234567890ABCDEF, 0xFEDCBA0987654321),
        };

        let mut buf = [0u8; HEADER_SIZE];
        header.serialize_into(&mut buf);

        let deserialized = PacketHeader::deserialize(&buf).expect("deserialize");
        assert_eq!(header, deserialized);
    }

    #[test]
    fn test_packet_structure() {
        let token = UID::new(0x1111111111111111, 0x2222222222222222);
        let payload = b"test";

        let packet = serialize_packet(token, payload).expect("serialize");

        // Verify structure
        assert_eq!(packet.len(), HEADER_SIZE + payload.len());

        // Check length field
        let length = u32::from_le_bytes([packet[0], packet[1], packet[2], packet[3]]);
        assert_eq!(length as usize, packet.len());

        // Check token
        let first = u64::from_le_bytes(packet[8..16].try_into().expect("slice"));
        let second = u64::from_le_bytes(packet[16..24].try_into().expect("slice"));
        assert_eq!(first, token.first);
        assert_eq!(second, token.second);

        // Check payload
        assert_eq!(&packet[HEADER_SIZE..], payload.as_slice());
    }
}
