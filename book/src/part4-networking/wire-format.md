# Wire Format

<!-- toc -->

- Packet layout: `[length:4][checksum:4][token:16][payload:N]`
- CRC32C checksums for integrity
- UID-based token routing: each endpoint has a unique 128-bit token
- Header: 24 bytes, max payload: 1MB
- `WireError` enum: InsufficientData, ChecksumMismatch, PacketTooLarge, InvalidLength
