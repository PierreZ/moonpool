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

## Current State

### What We've Implemented
- ✅ Unified `Envelope` trait in foundation (combines 4 traits into 1)
- ✅ Message implements Envelope trait directly
- ✅ MessageSerializer that uses Message as envelope type
- ✅ Updated ClientTransport to use unified trait bounds
- ✅ Added prelude module and convenience type aliases
- ✅ Examples working for single-node, but still have issues in multi-node

### What's Still Broken
- ❌ Double serialization still happening (Message gets wrapped in another Message)
- ❌ Multi-node examples have correlation ID errors and buffer issues
- ❌ Using `request()` API which forces payload model on envelope-based messages

## The Solution: Envelope-Only Transport API

### Design Decision
After analysis, we determined foundation should be **envelope-only**, not payload-centric, because:

1. **FoundationDB is message-centric**: Sends arbitrary `ISerializeSource` objects
2. **Request-response requires correlation**: So correlation should be explicit via Envelope trait
3. **Simpler is better**: One API instead of dual API reduces complexity
4. **No hidden wrapping**: What you send is what goes on the wire

### Target Architecture

```rust
// Simple envelope trait - just what transport needs
trait Envelope {
    fn correlation_id(&self) -> u64;
}

// Clean transport API - one method, no factories
impl Transport {
    async fn send<E: Envelope + Serialize + DeserializeOwned>(
        &self,
        destination: &str, 
        envelope: E
    ) -> Result<E>
}

// Moonpool usage - zero overhead
impl Envelope for Message {
    fn correlation_id(&self) -> u64 { self.correlation_id.as_u64() }
}

// Direct usage - no double serialization!
let response = transport.send(destination, message).await?;
```

## Implementation Tasks

### Phase 1: Clean Up Foundation Transport API

1. **Remove legacy envelope system** 
   - Delete EnvelopeFactory, EnvelopeReplyDetection, EnvelopeSerializer traits
   - Delete RequestResponseEnvelopeFactory
   - Keep only the unified Envelope trait

2. **Replace ClientTransport API**
   - Remove `request<Factory>()` method
   - Add `send<E: Envelope>()` method
   - Update correlation handling to work with Envelope trait

3. **Replace ServerTransport API**
   - Remove factory-based reply methods
   - Add envelope-based reply methods
   - Ensure consistent with ClientTransport

4. **Provide SimpleEnvelope helper**
   ```rust
   struct SimpleEnvelope<T> {
       correlation_id: u64,
       payload: T,
   }
   ```
   For users who just want basic request-response without custom envelopes

### Phase 2: Update Moonpool

1. **Simplify network layer**
   - Remove MessageSerializer (no longer needed)
   - Remove MessageEnvelopeFactory 
   - Remove legacy trait implementations for Message
   - Use transport.send(message) directly

2. **Update ActorRuntime**
   - Server message processing: envelope IS the Message
   - Remove double deserialization
   - Use envelope-based reply methods

3. **Test multi-node scenarios**
   - Ensure correlation IDs work correctly
   - Verify no buffer/serialization errors
   - Confirm load balancing across nodes

### Phase 3: Update Foundation Tests

1. **Replace factory-based tests**
   - Convert `request<Factory>()` calls to `send()` calls
   - Use SimpleEnvelope where needed
   - Ensure all 173+ tests still pass

2. **Add envelope API tests**
   - Test custom envelope types
   - Test correlation handling
   - Test error scenarios

### Phase 4: Documentation and Cleanup

1. **Update foundation docs**
   - Document envelope-only API
   - Provide migration guide from old API
   - Update examples

2. **Update moonpool docs**
   - Document zero-copy Message sending
   - Update network architecture docs

3. **Remove unused imports and code**
   - Clean up legacy trait imports
   - Remove unused MessageSerializer
   - Fix clippy warnings

## Expected Benefits

### For Foundation
- **Simpler API**: One method instead of factory-based complexity
- **Better FoundationDB alignment**: Message-centric like the original
- **More flexible**: Supports any envelope type, not just payload wrappers
- **Cleaner codebase**: Fewer traits and concepts

### For Moonpool  
- **No double serialization**: Message sent directly to wire
- **Direct control**: Message structure preserved end-to-end
- **Better performance**: One serialization instead of two
- **Simpler code**: No more 110-line transport wrapper needed

### For Both
- **Architectural alignment**: Both systems work with same envelope concept
- **Type safety**: Envelope trait ensures correlation is handled
- **Deterministic testing**: Foundation's simulation benefits still available

## Validation

### Before/After Message Flow

**Before (current broken state)**:
```
Message → to_bytes() → Vec<u8> → 
  request() → MessageEnvelopeFactory::create_request() → 
  Message{payload: Vec<u8>} → MessageSerializer::serialize() → 
  ActorEnvelope::serialize() → wire
```
Result: Double serialization, correlation issues

**After (target state)**:
```
Message → transport.send() → 
  Message implements Envelope → 
  ActorEnvelope::serialize() → wire
```
Result: Single serialization, direct correlation

This validates the approach will solve the core problems while making both codebases simpler and more aligned.

## Current Branch State

Working on branch: `001-location-transparent-actors`
- Has partial implementation of unified Envelope trait
- Examples work for single-node, issues with multi-node
- Ready for Phase 1 implementation to complete the fix