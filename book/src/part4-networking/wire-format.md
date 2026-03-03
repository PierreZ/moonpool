# Wire Format

<!-- toc -->

TCP gives us a reliable byte stream, but our application needs **framed messages** routed to specific endpoints. The wire format bridges this gap: it defines how we serialize a message with its destination, protect it with a checksum, and parse it back on the other side.

## Packet Layout

Every packet on the wire follows this structure:

```text
 0                   4                   8
 ├───────────────────┼───────────────────┤
 │   length (u32)    │  checksum (u32)   │
 ├───────────────────┴───────────────────┤
 │              token (UID)              │
 │           first: u64 (8 bytes)        │
 │          second: u64 (8 bytes)        │
 ├───────────────────────────────────────┤
 │            payload (N bytes)          │
 └───────────────────────────────────────┘

 Header: 24 bytes total
   - length:   4 bytes, little-endian u32 (total packet size including header)
   - checksum: 4 bytes, CRC32C of (token + payload)
   - token:   16 bytes, two little-endian u64 values
```

The header is exactly 24 bytes (`HEADER_SIZE`). The payload can be up to 1MB (`MAX_PAYLOAD_SIZE`). Anything larger is rejected to prevent memory exhaustion.

## CRC32C Checksums

Every packet carries a CRC32C checksum computed over the token and payload bytes:

```rust
fn compute_checksum(token: UID, payload: &[u8]) -> u32 {
    let mut data = Vec::with_capacity(16 + payload.len());
    data.extend_from_slice(&token.first.to_le_bytes());
    data.extend_from_slice(&token.second.to_le_bytes());
    data.extend_from_slice(payload);
    crc32c::crc32c(&data)
}
```

The checksum covers the token because corrupted routing is just as dangerous as corrupted data. If a bit flip changes the destination token, the message would be delivered to the wrong endpoint silently. Including the token in the checksum catches this.

On deserialization, the receiver recomputes the checksum and compares. Any mismatch produces a `WireError::ChecksumMismatch` with both the expected and actual values for debugging.

## UID-Based Token Routing

The 16-byte token field is a `UID` that identifies the destination endpoint. When a packet arrives, the transport layer looks up this token in the `EndpointMap` to find the right receiver.

UIDs come in two flavors:

- **Well-known tokens** have `first == u64::MAX`. The `second` field is an index into a fixed-size array for O(1) lookup. These are used for system endpoints like ping (`WellKnownToken::Ping`).
- **Dynamic tokens** use arbitrary values for both fields. These are looked up in a `BTreeMap` and are used for request-response correlation and service endpoints.

The `UID::adjusted()` method derives related tokens from a base UID, which is how service methods get their individual endpoint tokens from a single service ID.

## Streaming Deserialization

Real TCP streams deliver data in chunks that do not align with packet boundaries. The `try_deserialize_packet` function handles this gracefully:

```rust
pub fn try_deserialize_packet(data: &[u8])
    -> Result<Option<(UID, Vec<u8>, usize)>, WireError>
```

It returns:
- `Ok(Some((token, payload, consumed)))` when a complete packet is available
- `Ok(None)` when more data is needed (not enough bytes for the header or full packet)
- `Err(...)` when the data is malformed

The `consumed` count tells the caller how many bytes to advance in the read buffer, making it straightforward to process multiple packets from a single TCP read.

## Error Cases

The `WireError` enum covers four failure modes:

| Variant | Meaning |
|---------|---------|
| `InsufficientData` | Not enough bytes to parse header or full packet |
| `ChecksumMismatch` | Data was corrupted in transit |
| `PacketTooLarge` | Payload exceeds 1MB limit |
| `InvalidLength` | Length field is malformed (e.g., smaller than header size) |

In simulation, the chaos engine can inject data corruption that triggers `ChecksumMismatch`. This exercises the error handling paths in the connection task without needing to model individual bit errors on the wire.
