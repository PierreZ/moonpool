//! A minimal, dependency-free protobuf reader and writer for routing data.
//!
//! References have to encode and decode without a runtime, without the
//! `prost` feature (the wasm and lean builds) and inside applications'
//! prost messages. This module is the one implementation of their bytes;
//! with the `prost` feature the `prost::Message` impls call into it, so both
//! paths produce the same encoding. It follows prost and proto3: scalar
//! fields equal to their default are not written, unknown fields (groups
//! included) are skipped, keys above `u32::MAX` and tag zero are refused
//! (cross-checked against prost in the tests).

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

/// Where encoded bytes go: a buffer, or a counter that only measures.
pub(crate) trait Sink {
    fn put(&mut self, bytes: &[u8]);
}

impl Sink for Vec<u8> {
    fn put(&mut self, bytes: &[u8]) {
        self.extend_from_slice(bytes);
    }
}

/// Measures an encoding without allocating.
#[derive(Debug, Default)]
pub(crate) struct Counter(pub(crate) usize);

impl Sink for Counter {
    fn put(&mut self, bytes: &[u8]) {
        self.0 += bytes.len();
    }
}

/// Appends protobuf fields in the order the caller writes them, skipping
/// fields equal to their default (proto3).
pub(crate) struct ProtoWriter<'a, S: Sink>(pub(crate) &'a mut S);

impl<S: Sink> ProtoWriter<'_, S> {
    fn raw_varint(&mut self, mut value: u64) {
        let mut buf = [0u8; 10];
        let mut len = 0;
        while value >= 0x80 {
            buf[len] = (value.to_le_bytes()[0] & 0x7f) | 0x80;
            value >>= 7;
            len += 1;
        }
        buf[len] = value.to_le_bytes()[0];
        self.0.put(&buf[..=len]);
    }

    fn key(&mut self, tag: u32, wire_type: WireType) {
        self.raw_varint((u64::from(tag) << 3) | wire_type.code());
    }

    /// A `uint32`/`uint64`/enum field; nothing for zero.
    pub(crate) fn varint(&mut self, tag: u32, value: u64) -> &mut Self {
        if value != 0 {
            self.key(tag, WireType::Varint);
            self.raw_varint(value);
        }
        self
    }

    /// A `fixed64` field; nothing for zero.
    pub(crate) fn fixed64(&mut self, tag: u32, value: u64) -> &mut Self {
        if value != 0 {
            self.key(tag, WireType::Fixed64);
            self.0.put(&value.to_le_bytes());
        }
        self
    }

    /// A `bytes` field; nothing when empty.
    pub(crate) fn bytes(&mut self, tag: u32, value: &[u8]) -> &mut Self {
        if !value.is_empty() {
            self.key(tag, WireType::Bytes);
            self.raw_varint(value.len() as u64);
            self.0.put(value);
        }
        self
    }
}

/// One decoded field value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Value<'a> {
    Varint(u64),
    Fixed64(u64),
    Bytes(&'a [u8]),
    /// A field of another wire type (fixed32, group), skipped.
    Other,
}

/// Why protobuf bytes could not be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProtoError {
    Truncated,
    InvalidVarint,
    InvalidKey,
    InvalidWireType,
    UnexpectedEndGroup,
    RecursionLimit,
}

impl std::fmt::Display for ProtoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Truncated => "truncated protobuf field",
            Self::InvalidVarint => "invalid protobuf varint",
            Self::InvalidKey => "invalid protobuf field key",
            Self::InvalidWireType => "invalid protobuf wire type",
            Self::UnexpectedEndGroup => "unexpected protobuf end-group",
            Self::RecursionLimit => "protobuf groups nested too deeply",
        })
    }
}

/// prost's default recursion limit, applied to nested unknown groups.
const RECURSION_LIMIT: u32 = 100;

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

    /// A field key: the tag (`1..2^29`) and the raw wire type.
    fn key(&mut self) -> Result<(u32, u64), ProtoError> {
        let key = self.raw_varint()?;
        let key = u32::try_from(key).map_err(|_| ProtoError::InvalidKey)?;
        let tag = key >> 3;
        if tag == 0 {
            return Err(ProtoError::InvalidKey);
        }
        Ok((tag, u64::from(key & 0x7)))
    }

    fn value(&mut self, tag: u32, wire_type: u64, depth: u32) -> Result<Value<'a>, ProtoError> {
        Ok(match wire_type {
            0 => Value::Varint(self.raw_varint()?),
            1 => {
                let mut raw = [0u8; 8];
                raw.copy_from_slice(self.take(8)?);
                Value::Fixed64(u64::from_le_bytes(raw))
            }
            2 => {
                let len = self.raw_varint()?;
                Value::Bytes(self.take(len)?)
            }
            3 => {
                self.skip_group(tag, depth)?;
                Value::Other
            }
            4 => return Err(ProtoError::UnexpectedEndGroup),
            5 => {
                self.take(4)?;
                Value::Other
            }
            _ => return Err(ProtoError::InvalidWireType),
        })
    }

    /// Skip a group's fields up to its matching end-group key.
    fn skip_group(&mut self, tag: u32, depth: u32) -> Result<(), ProtoError> {
        if depth >= RECURSION_LIMIT {
            return Err(ProtoError::RecursionLimit);
        }
        loop {
            if self.0.is_empty() {
                return Err(ProtoError::Truncated);
            }
            let (inner, wire_type) = self.key()?;
            if wire_type == 4 {
                return if inner == tag {
                    Ok(())
                } else {
                    Err(ProtoError::UnexpectedEndGroup)
                };
            }
            self.value(inner, wire_type, depth + 1)?;
        }
    }

    /// The next field, or `None` at the end of the input.
    pub(crate) fn field(&mut self) -> Result<Option<(u32, Value<'a>)>, ProtoError> {
        if self.0.is_empty() {
            return Ok(None);
        }
        let (tag, wire_type) = self.key()?;
        let value = self.value(tag, wire_type, 0)?;
        Ok(Some((tag, value)))
    }
}

#[cfg(test)]
mod tests {
    use super::{Counter, ProtoError, ProtoReader, ProtoWriter, Value};

    #[test]
    fn varints_round_trip_at_every_width() {
        for value in [1, 127, 128, 300, u64::from(u32::MAX), u64::MAX] {
            let mut out = Vec::new();
            ProtoWriter(&mut out).varint(1, value);
            let mut counter = Counter::default();
            ProtoWriter(&mut counter).varint(1, value);
            assert_eq!(counter.0, out.len());
            let mut reader = ProtoReader::new(&out);
            assert_eq!(reader.field(), Ok(Some((1, Value::Varint(value)))));
            assert_eq!(reader.field(), Ok(None));
        }
    }

    #[test]
    fn defaults_are_not_written() {
        let mut out = Vec::new();
        ProtoWriter(&mut out)
            .varint(1, 0)
            .fixed64(2, 0)
            .bytes(3, &[]);
        assert!(out.is_empty());
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
        // An unknown group is skipped; an unmatched end-group is refused.
        assert_eq!(
            ProtoReader::new(&[0x0b, 0x10, 0x01, 0x0c]).field(),
            Ok(Some((1, Value::Other)))
        );
        assert_eq!(
            ProtoReader::new(&[0x0c]).field(),
            Err(ProtoError::UnexpectedEndGroup)
        );
        assert_eq!(
            ProtoReader::new(&[0x0e]).field(),
            Err(ProtoError::InvalidWireType)
        );
        assert_eq!(
            ProtoReader::new(&[0x0d, 1, 2, 3, 4]).field(),
            Ok(Some((1, Value::Other)))
        );
        // A key above u32::MAX (a tag beyond 2^29 - 1) is refused.
        assert_eq!(
            ProtoReader::new(&[0x80, 0x80, 0x80, 0x80, 0x10, 0x00]).field(),
            Err(ProtoError::InvalidKey)
        );
    }
}
