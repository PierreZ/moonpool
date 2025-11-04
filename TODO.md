# TODO: Fix Double Serialization and Transport API

## Problem Analysis

### Original Issue
User asked to "improve DX in foundation based on how moonpool is using foundation". The main problem was that moonpool had to create a 110-line `FoundationTransport` wrapper due to foundation's complex envelope system with 4 separate traits.

### Root Cause Discovered
Through implementation we discovered a **fundamental architectural mismatch**:

1. **Foundation's model**: Payload-centric with transport-owned envelopes
   - `request<Factory>(destination, payload: Vec<u8>) -> Vec<u8>`
   - Transport wraps payloads in its own envelope format
   - Uses 4 traits: EnvelopeSerializer, EnvelopeFactory, EnvelopeReplyDetection, LegacyEnvelope

2. **Moonpool's needs**: Message-centric with app-owned envelopes
   - `Message` already contains all routing/correlation metadata
   - Needs to send complete actor messages with identity preserved

3. **Result**: Double serialization and API impedance mismatch
   - Message → bytes → RequestResponseEnvelope → bytes (double serialization)
   - Network errors: "failed to fill whole buffer", correlation ID mismatches
   - Examples hanging in multi-node scenarios

## Current State (As of Phase 1 Completion)

### ✅ Phase 1: Foundation API Cleanup - COMPLETED

**What Was Done:**
1. ✅ Removed 4 legacy traits (EnvelopeFactory, EnvelopeReplyDetection, EnvelopeSerializer, LegacyEnvelope)
2. ✅ Updated ClientTransport API:
   - Removed `request<Factory>()` method
   - Renamed `request_envelope()` to `send()`
   - Uses direct `E: Envelope` type parameter
3. ✅ Updated ServerTransport API:
   - Replaced `send_reply<Factory>()` with `reply_with_payload()`
   - Type parameter changed to `E: Envelope`
4. ✅ Updated TransportDriver/TransportProtocol:
   - Removed EnvelopeSerializer trait usage
   - Changed to direct `E: Envelope` type parameter
   - Calls `envelope.to_bytes()` and `E::from_bytes()` directly
5. ✅ Fixed all 172 foundation tests (100% passing)
6. ✅ Updated module exports (mod.rs, prelude.rs, network/mod.rs)
7. ✅ Cleaned up all compiler warnings

**Foundation Status:**
- ✅ All 172 tests passing
- ✅ Simulation tests working (10k iterations, 100% success rate)
- ✅ No compiler warnings
- ✅ Ready to commit

### ❌ Phase 2: Moonpool Updates - NOT STARTED

**What's Broken:**
- ❌ Moonpool crate doesn't compile (still uses old traits)
- ❌ MessageSerializer needs to be removed
- ❌ Message needs to use new Envelope trait directly
- ❌ ActorRuntime needs to use new transport API

**Files That Need Updating:**
- `moonpool/src/messaging/network.rs` - Remove MessageSerializer, update Message
- `moonpool/src/runtime/actor_runtime.rs` - Use new transport API

## The Solution: Envelope-Only Transport API

### Design Decision
After analysis, we determined foundation should be **envelope-only**, not payload-centric, because:

1. **FoundationDB is message-centric**: Sends arbitrary `ISerializeSource` objects
2. **Request-response requires correlation**: So correlation should be explicit via Envelope trait
3. **Simpler is better**: One API instead of dual API reduces complexity
4. **No hidden wrapping**: What you send is what goes on the wire

### Target Architecture (Now Implemented in Foundation)

```rust
// Simple envelope trait - just what transport needs
trait Envelope {
    fn to_bytes(&self) -> Vec<u8>;
    fn from_bytes(data: &[u8]) -> Result<Self, EnvelopeError>;
    fn try_from_buffer(buffer: &mut Vec<u8>) -> Result<Option<Self>, EnvelopeError>;
    fn correlation_id(&self) -> u64;
    fn is_response_to(&self, request_id: u64) -> bool;
    fn create_response(&self, payload: Vec<u8>) -> Self;
    fn payload(&self) -> &[u8];
}

// Clean transport API - one method, no factories
impl ClientTransport {
    async fn send<E: Envelope>(&self, destination: &str, envelope: E) -> Result<E>
}

// Moonpool usage - zero overhead (NEEDS IMPLEMENTATION)
impl Envelope for Message {
    fn to_bytes(&self) -> Vec<u8> { /* serde serialize */ }
    fn from_bytes(data: &[u8]) -> Result<Self> { /* serde deserialize */ }
    fn correlation_id(&self) -> u64 { self.correlation_id.as_u64() }
    fn create_response(&self, payload: Vec<u8>) -> Self { /* create reply */ }
    // ... etc
}

// Direct usage - no double serialization!
let response = transport.send(destination, message).await?;
```

## Remaining Implementation Tasks

### Phase 2: Update Moonpool (NEXT STEP)

1. **Update Message to implement new Envelope trait**
   - Remove old trait implementations (EnvelopeSerializer, etc.)
   - Implement new unified Envelope trait
   - Handle serialization via serde directly

2. **Remove MessageSerializer**
   - No longer needed with envelope-only API
   - Remove from messaging/network.rs

3. **Update ActorRuntime**
   - Use `transport.send(message)` instead of `request<Factory>()`
   - Use `reply_with_payload()` instead of `send_reply<Factory>()`
   - Remove double deserialization

4. **Test multi-node scenarios**
   - Ensure correlation IDs work correctly
   - Verify no buffer/serialization errors
   - Confirm load balancing across nodes

### Phase 3: Documentation and Cleanup

1. **Update foundation docs**
   - Document envelope-only API
   - Provide migration guide from old API
   - Update examples

2. **Update moonpool docs**
   - Document zero-copy Message sending
   - Update network architecture docs

3. **Remove unused imports and code**
   - Clean up legacy trait imports
   - Fix any remaining clippy warnings

## Expected Benefits

### For Foundation
- **Simpler API**: One method instead of factory-based complexity ✅
- **Better FoundationDB alignment**: Message-centric like the original ✅
- **More flexible**: Supports any envelope type, not just payload wrappers ✅
- **Cleaner codebase**: Fewer traits and concepts ✅

### For Moonpool
- **No double serialization**: Message sent directly to wire (PENDING)
- **Direct control**: Message structure preserved end-to-end (PENDING)
- **Better performance**: One serialization instead of two (PENDING)
- **Simpler code**: No more 110-line transport wrapper needed (PENDING)

### For Both
- **Architectural alignment**: Both systems work with same envelope concept (PARTIAL)
- **Type safety**: Envelope trait ensures correlation is handled ✅
- **Deterministic testing**: Foundation's simulation benefits still available ✅

## Validation

### Message Flow After Phase 1

**Current State (Foundation Only)**:
```
RequestResponseEnvelope → to_bytes() → wire
  (foundation internal testing works perfectly)
```

**Target State (Moonpool with Foundation)**:
```
Message → transport.send() →
  Message implements Envelope →
  to_bytes() → wire
  (no double serialization!)
```

## Current Branch State

Working on branch: `001-location-transparent-actors`
- ✅ Phase 1 complete: Foundation API cleanup done
- ✅ All 172 foundation tests passing (100% success rate)
- ❌ Moonpool crate broken (expected - needs Phase 2)
- 📝 Ready to commit Phase 1 and proceed with Phase 2
