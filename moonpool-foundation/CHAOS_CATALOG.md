# Chaos Injection Catalog

A comprehensive catalog of all fault injection and assertion coverage in moonpool-foundation.

## Overview

This document lists every point where we deliberately inject chaos and every behavior we validate. The goal is to find bugs before production by simulating hostile infrastructure conditions.

**Statistics**: 8 chaos injection points, 80+ assertions, 3 invariants, 100% coverage

**Philosophy**: Deterministic chaos testing - same seed produces identical behavior, making bugs reproducible and fixable

## Buggify Chaos Injection

These are deliberate faults injected into the system to test error handling and recovery.

| Component | What We Break | Why We Break It | Probability |
|-----------|---------------|-----------------|-------------|
| Protocol Parser | Corrupt the receive buffer by overwriting data | Ensures protocol can recover from data corruption without crashing or hanging | 3% |
| Protocol Parser | Force early exit to fragment packets artificially | Tests that protocol correctly buffers incomplete messages across multiple receives | 5% |
| Transport Server | Force parse failure when processing incoming data | Validates server gracefully handles parse errors and closes connections cleanly | 2% |
| Transport Server | Force write failure when sending responses | Ensures server properly cleans up connection state when writes fail | 2% |
| Transport Driver | Force send operation to fail | Tests reconnection backoff logic kicks in properly to avoid overwhelming failed peers | 5% |
| Transport Driver | Force peer creation to fail | Validates exponential backoff prevents connection spam when peer is unavailable | 6% |
| Peer Connection | Force write failure and requeue message | Ensures messages are preserved and retried rather than lost on transient failures | 2% |
| Test Workload | Generate burst of many messages at once | Creates queue pressure to trigger queue capacity checks and flow control logic | 5% |

## Sometimes Assert Coverage

These assertions verify that specific code paths are executed during testing. Each assertion must trigger at least once across all test runs to ensure comprehensive coverage.

### Protocol Parser (10 assertions)

| What We Check | Why It Matters |
|---------------|----------------|
| Protocol recovers from corrupted data | Corruption should trigger error path, not crash |
| Protocol handles extreme fragmentation | Messages split across many tiny reads must work |
| Protocol parses complete messages | Happy path must work reliably |
| Protocol handles concatenated messages | Multiple messages in one TCP read is common |
| Protocol fully consumes buffer | No data left behind after successful parse |
| Protocol handles partial messages | Incomplete messages must buffer for next read |
| Protocol encounters parse errors | Error paths must be exercised |
| Protocol clears buffer on error | Clean state after error enables recovery |
| Protocol processes messages | Forward progress is happening |
| Protocol buffers partial data | Incomplete messages stored correctly |

### Transport Server (11 assertions)

| What We Check | Why It Matters |
|---------------|----------------|
| Server handles peer closing connection | Graceful shutdown from client side |
| Server receives data successfully | Connection actually works |
| Server encounters forced parse errors | Chaos injection is triggering |
| Server parses envelopes correctly | Message deserialization works |
| Server handles partial messages | Buffering incomplete data works |
| Server encounters parsing errors | Error paths are tested |
| Server handles read errors gracefully | Connection failures don't crash server |
| Server encounters forced write failures | Chaos injection is triggering |
| Server handles write errors gracefully | Send failures don't crash server |
| Server sends data successfully | Response path works |
| Server cleans up connection state | No resource leaks on connection close |

### Transport Driver (8 assertions)

| What We Check | Why It Matters |
|---------------|----------------|
| Driver encounters forced send failures | Chaos injection is triggering |
| Driver encounters forced peer creation failures | Chaos injection is triggering |
| Driver handles peer failures | Failure detection works |
| Driver increases backoff delay | Exponential backoff is implemented |
| Driver reaches maximum backoff ceiling | Backoff doesn't grow unbounded |
| Driver prevents reconnect during backoff | Backoff actually delays reconnection attempts |
| Driver allows reconnect after backoff expires | Eventually reconnection is permitted |
| Driver recovers after failures | System self-heals |

### Peer Connection (4 assertions)

| What We Check | Why It Matters |
|---------------|----------------|
| Peer queue approaches capacity | Queue fills under load |
| Peer queue grows beyond single message | Multiple messages queued is normal |
| Peer requeues on failure | Messages preserved on send failure |
| Peer recovers after failures | Connection re-establishment works |

### Test Workload (9 assertions)

| What We Check | Why It Matters |
|---------------|----------------|
| Client sends pings | Request path works |
| Client receives pongs | Response path works |
| Short timeout succeeds | Fast operations complete |
| Client experiences timeout | Timeout path is tested |
| Client waits between operations | Pacing logic works |
| Client switches servers | Multi-server scenarios tested |
| Client sends bursts | Queue pressure scenario tested |
| Server receives pings | Server ingress works |
| Server sends pongs | Server egress works |

### Test Infrastructure (34+ assertions)

The assertion framework itself is validated to ensure isolation between tests, accurate coverage tracking, and proper statistical sampling.

## Invariant Checks

These are global properties checked after every simulation event. Unlike assertions that check local behavior, invariants validate cross-component consistency.

| Invariant | What It Detects | Why It Matters |
|-----------|-----------------|----------------|
| Message conservation (client to server) | Servers receiving more pings than clients sent | Would indicate message duplication in transport layer |
| Message conservation (server processing) | Servers sending more pongs than pings received | Would indicate server duplicating responses |
| Message conservation (server to client) | Clients receiving more pongs than servers sent | Would indicate message duplication in response path |

These three invariants together form a complete audit trail: messages can be lost (tested separately) but never duplicated or created from nothing.

## Chaos Strategy

### Probability Levels

| Probability | Where We Use It | Reasoning |
|-------------|-----------------|-----------|
| 2% (low) | Inner loops, frequently called paths | Happens often naturally, don't need high probability |
| 3% (low-medium) | Data corruption scenarios | Rare but need to test occasionally |
| 5% (medium) | Component boundaries, send operations | Common failure points worth regular testing |
| 6% (high) | Connection establishment | Catches problems early in lifecycle |

### Coverage Strategy

We achieve 100% coverage by testing three complete lifecycles:

1. **Connection Lifecycle**: Establishment → Operation → Failure → Recovery
2. **Message Lifecycle**: Send → Transit → Receive → Timeout
3. **Resource Lifecycle**: Queue growth → Near capacity → Backoff → Drain

Every error path has both a buggify injection to trigger it AND a sometimes_assert to verify it was tested.

## Summary

This chaos testing approach finds bugs through deterministic simulation:

- **8 buggify injections** break the system in controlled ways
- **80+ assertions** verify all code paths execute
- **3 invariants** catch bugs that individual assertions miss
- **100% coverage** means every assertion triggers at least once

The result: bugs found in testing rather than production, with reproducible failures via seed numbers.
