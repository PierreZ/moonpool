//! A minimal, dependency-free protobuf reader and writer for routing data.
//!
//! References have to encode and decode without a runtime, without the
//! `prost` feature (the wasm and lean builds) and inside applications'
//! prost messages. This module is the one implementation of their bytes;
//! with the `prost` feature the `prost::Message` impls call into it, so both
//! paths produce the same encoding (cross-checked in the tests against a
//! prost-derived mirror).

/// Protobuf wire types used by routing data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WireType {
    Varint,
    Fixed64,
    Bytes,
}

impl WireType {
    const fn code(self) -> u64 {
        match self {
            Self::Varint => 0,
            Self::Fixed64 => 1,
            Self::Bytes => 2,
        }
    }
}

/// Appends protobuf fields, always in the order the caller writes them.
pub(crate) struct ProtoWriter<'a>(pub(crate) &'a mut Vec<u8>);

impl ProtoWriter<'_> {
    fn raw_varint(&mut self, mut value: u64) {
        while value >= 0x80 {
            self.0.push((value.to_le_bytes()[0] & 0x7f) | 0x80);
            value >>= 7;
        }
        self.0.push(value.to_le_bytes()[0]);
    }

    fn key(&mut self, tag: u32, wire_type: WireType) {
        self.raw_varint((u64::from(tag) << 3) | wire_type.code());
    }

    /// A `uint32`/`uint64`/enum field, written even when zero so the
    /// encoding of a reference is fixed.
    pub(crate) fn varint(&mut self, tag: u32, value: u64) -> &mut Self {
        self.key(tag, WireType::Varint);
        self.raw_varint(value);
        self
    }

    /// A `fixed64` field.
    pub(crate) fn fixed64(&mut self, tag: u32, value: u64) -> &mut Self {
        self.key(tag, WireType::Fixed64);
        self.0.extend_from_slice(&value.to_le_bytes());
        self
    }

    /// A `bytes` field.
    pub(crate) fn bytes(&mut self, tag: u32, value: &[u8]) -> &mut Self {
        self.key(tag, WireType::Bytes);
        self.raw_varint(value.len() as u64);
        self.0.extend_from_slice(value);
        self
    }
}

/// One decoded field value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Value<'a> {
    Varint(u64),
    Fixed64(u64),
    Bytes(&'a [u8]),
    /// A field of another wire type, skipped.
    Other,
}

/// Why protobuf bytes could not be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProtoError {
    Truncated,
    InvalidVarint,
    InvalidKey,
    /// Groups are deprecated and never part of routing data.
    UnsupportedWireType,
}

impl std::fmt::Display for ProtoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Truncated => "truncated protobuf field",
            Self::InvalidVarint => "invalid protobuf varint",
            Self::InvalidKey => "invalid protobuf field key",
            Self::UnsupportedWireType => "unsupported protobuf wire type",
        })
    }
}

/// Reads protobuf fields from a bounded slice.
pub(crate) struct ProtoReader<'a>(&'a [u8]);

impl<'a> ProtoReader<'a> {
    pub(crate) fn new(bytes: &'a [u8]) -> Self {
        Self(bytes)
    }

    fn raw_varint(&mut self) -> Result<u64, ProtoError> {
        let mut value = 0u64;
        for (index, byte) in self.0.iter().enumerate().take(10) {
            let bits = u64::from(byte & 0x7f);
            if index == 9 && bits > 1 {
                return Err(ProtoError::InvalidVarint);
            }
            value |= bits << (7 * index);
            if byte & 0x80 == 0 {
                self.0 = &self.0[index + 1..];
                return Ok(value);
            }
        }
        Err(if self.0.len() < 10 {
            ProtoError::Truncated
        } else {
            ProtoError::InvalidVarint
        })
    }

    fn take(&mut self, len: u64) -> Result<&'a [u8], ProtoError> {
        let len = usize::try_from(len).map_err(|_| ProtoError::Truncated)?;
        if self.0.len() < len {
            return Err(ProtoError::Truncated);
        }
        let (head, rest) = self.0.split_at(len);
        self.0 = rest;
        Ok(head)
    }

    /// The next field, or `None` at the end of the input.
    pub(crate) fn field(&mut self) -> Result<Option<(u32, Value<'a>)>, ProtoError> {
        if self.0.is_empty() {
            return Ok(None);
        }
        let key = self.raw_varint()?;
        let tag = u32::try_from(key >> 3).map_err(|_| ProtoError::InvalidKey)?;
        if tag == 0 {
            return Err(ProtoError::InvalidKey);
        }
        let value = match key & 0x7 {
            0 => Value::Varint(self.raw_varint()?),
            1 => {
                let bytes = self.take(8)?;
                let mut raw = [0u8; 8];
                raw.copy_from_slice(bytes);
                Value::Fixed64(u64::from_le_bytes(raw))
            }
            2 => {
                let len = self.raw_varint()?;
                Value::Bytes(self.take(len)?)
            }
            5 => {
                self.take(4)?;
                Value::Other
            }
            _ => return Err(ProtoError::UnsupportedWireType),
        };
        Ok(Some((tag, value)))
    }
}

#[cfg(test)]
mod tests {
    use super::{ProtoError, ProtoReader, ProtoWriter, Value};

    #[test]
    fn varints_round_trip_at_every_width() {
        for value in [0, 1, 127, 128, 300, u64::from(u32::MAX), u64::MAX] {
            let mut out = Vec::new();
            ProtoWriter(&mut out).varint(1, value);
            let mut reader = ProtoReader::new(&out);
            assert_eq!(reader.field(), Ok(Some((1, Value::Varint(value)))));
            assert_eq!(reader.field(), Ok(None));
        }
    }

    #[test]
    fn malformed_input_is_an_error_not_a_panic() {
        assert_eq!(
            ProtoReader::new(&[0x08]).field(),
            Err(ProtoError::Truncated)
        );
        assert_eq!(
            ProtoReader::new(&[0x08, 0xff]).field(),
            Err(ProtoError::Truncated)
        );
        assert_eq!(
            ProtoReader::new(&[
                0x08, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x7f
            ])
            .field(),
            Err(ProtoError::InvalidVarint)
        );
        assert_eq!(
            ProtoReader::new(&[0x00]).field(),
            Err(ProtoError::InvalidKey)
        );
        assert_eq!(
            ProtoReader::new(&[0x12, 0x05, 0x01]).field(),
            Err(ProtoError::Truncated)
        );
        assert_eq!(
            ProtoReader::new(&[0x0b]).field(),
            Err(ProtoError::UnsupportedWireType)
        );
        assert_eq!(
            ProtoReader::new(&[0x0d, 1, 2, 3, 4]).field(),
            Ok(Some((1, Value::Other)))
        );
    }
}
