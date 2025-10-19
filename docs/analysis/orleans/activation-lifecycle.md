# Activation Lifecycle Management

## Reference Files
- `Catalog.cs` - Main activation catalog and lifecycle orchestration
- `ActivationData.cs:1-2410` - Per-activation state and lifecycle management
- `ActivationDirectory.cs:1-96` - Thread-safe activation registry
- `Grain.cs:1-291` - Base grain class and API surface
- `IGrain.cs:1-49` - Grain marker interfaces
- `DeactivationReason.cs:1-72` - Deactivation reason codes and descriptions

## Overview

Orleans uses a **catalog-based activation model** where grains (actors) are created on-demand and managed through their lifecycle. The system ensures:
- **Single activation per grain** (except StatelessWorker)
- **Deterministic state transitions** through explicit state machines
- **Thread-safe concurrent access** via fine-grained locking
- **Graceful lifecycle management** with timeout enforcement

The architecture separates concerns:
- **Catalog** - Orchestrates activation creation/destruction across the silo
- **ActivationData** - Manages per-activation state, messages, and lifecycle
- **ActivationDirectory** - Thread-safe registry mapping GrainId → IGrainContext
- **Grain** - Base class providing user-facing API

---

## Catalog: Activation Orchestration

### Purpose
The Catalog is a SystemTarget (special grain) responsible for:
- Creating new activations
- Managing activation lifecycle across the silo
- Coordinating with grain directory for distributed placement
- Handling silo failure scenarios

### Key Implementation: GetOrCreateActivation

**Code**: `Catalog.cs:106-195`

**Pattern**: Double-check locking with race handling

```csharp
public IGrainContext GetOrCreateActivation(
    in GrainId grainId,
    Dictionary<string, object> requestContextData,
    MigrationContext rehydrationContext)
{
    // First check without lock (fast path)
    if (TryGetGrainContext(grainId, out var result))
    {
        rehydrationContext?.Dispose();
        return result;
    }

    // Lock over all activations to prevent multiple instances
    lock (activations)
    {
        // Second check with lock (race protection)
        if (TryGetGrainContext(grainId, out result))
        {
            rehydrationContext?.Dispose();
            return result;
        }

        if (!_siloStatusOracle.CurrentStatus.IsTerminating())
        {
            // Create new activation
            var address = new GrainAddress { ... };
            result = this.grainActivator.CreateInstance(address);
            activations.RecordNewTarget(result);
        }
    } // End lock

    if (result is null)
    {
        return UnableToCreateActivation(...);
    }

    // Async initialization outside of lock
    result.Activate(requestContextData);
    return result;
}
```

**Critical Insights**:
1. **Double-check pattern**: Check → Lock → Check again → Create
2. **Coarse lock**: `lock(activations)` prevents all concurrent activations - simple but safe
3. **Activation outside lock**: `result.Activate()` called after releasing lock
4. **Silo termination check**: Prevents activation during shutdown (line 131)
5. **Instrumentation**: `CatalogInstruments.ActivationsCreated` (line 152)

### Catalog Lifecycle Integration

**Code**: `Catalog.cs:355-359`

The Catalog participates in the Silo lifecycle to receive the status oracle. This enables checking if the silo is terminating before creating activations.

### Deactivation Orchestration

**Code**: `Catalog.cs:224-267`

```csharp
internal async Task DeactivateActivations(
    DeactivationReason reason,
    List<IGrainContext> list,
    CancellationToken cancellationToken)
{
    var options = new ParallelOptions
    {
        CancellationToken = CancellationToken.None,  // Don't cancel cleanup
        MaxDegreeOfParallelism = Environment.ProcessorCount * 512
    };

    await Parallel.ForEachAsync(list, options, (activation, _) =>
    {
        activation.Deactivate(reason, cancellationToken);
        return new (activation.Deactivated);  // Wait for completion
    }).WaitAsync(cancellationToken);
}
```

**Pattern**: Parallel deactivation with completion tracking
- High parallelism (512x processor count) for I/O-bound cleanup
- Waits for each activation's `Deactivated` task
- Used during shutdown and directory failure scenarios

---

## ActivationData: Per-Activation State Management

### Purpose
ActivationData is the heavyweight implementation of `IGrainContext`. Each activation has:
- Message queues (waiting and running requests)
- Lifecycle state machine
- Timers and components
- Service scope and dependency injection
- Message processing loop

**Code**: `ActivationData.cs:29-84` - Construction and core fields

### Activation State Machine

**Code**: `ActivationData.cs:90` - `ActivationState` enum

```
[Creating] → [Activating] → [Valid] → [Deactivating] → [Invalid]
```

**State Transitions**:
- **Creating**: Initial state, grain instance created (line 322)
- **Activating**: Lifecycle starting, `OnActivateAsync()` called (line 1579)
- **Valid**: Ready to process messages (line 1621)
- **Deactivating**: Shutting down gracefully (line 604)
- **Invalid**: Disposed, no longer usable (line 813)

**State Guards**: All state changes happen under `lock(this)` (lines 339, 577, 1617)

### Message Queues

**Code**: `ActivationData.cs:45-46, 920-1212`

```csharp
private readonly List<(Message Message, CoarseStopwatch QueuedTime)> _waitingRequests = new();
private readonly Dictionary<Message, CoarseStopwatch> _runningRequests = new();
private Message? _blockingRequest;
```

**Two-queue system**:
1. **Waiting queue**: Messages not yet being processed
2. **Running requests**: Messages currently executing (for reentrancy support)
3. **Blocking request**: The non-reentrant message currently blocking the activation

**Message Loop**: `RunMessageLoop()` (lines 920-1212)
- Runs forever in background task (line 930: `while (true)`)
- Processes operations (activation, deactivation) when no messages running
- Processes messages from waiting queue when state is Valid
- Never terminates - gets GC'd when activation is deactivated and dereferenced

### Lifecycle Commands

**Code**: `ActivationData.cs:2070-2128`

**Pattern**: Command objects for lifecycle operations

```csharp
private abstract class Command(CancellationTokenSource cts)
{
    public CancellationToken CancellationToken => _cts.Token;

    public sealed class Activate(...) : Command
    public sealed class Deactivate(...) : Command
    public sealed class Rehydrate(...) : Command
    public sealed class Delay(TimeSpan duration) : Command
}
```

**Usage**: Commands are enqueued via `ScheduleOperation()` (line 439) and processed by `ProcessOperationsAsync()` (lines 1157-1211)

**Why commands?**
- Serializes lifecycle operations with message processing
- Enables cancellation of pending operations
- Separates scheduling from execution

### Activation Process

**Code**: `ActivationData.cs:1483-1665`

**Key Steps**:

1. **Directory Registration** (lines 1499-1575)
   - Register with grain directory (if not StatelessWorker)
   - Handle race: if another activation already registered, forward to it
   - Retry with previous registration if stale entry exists
   - Set `ForwardingAddress` if duplicate detected

2. **Set State to Activating** (line 1579)

3. **Run Lifecycle** (lines 1588-1615)
   - Import request context
   - Call `lifecycle.OnStart()`
   - Call `grain.OnActivateAsync()`
   - Both wrapped in try-catch with detailed logging

4. **Set State to Valid** (line 1621)

5. **Error Handling** (lines 1631-1660)
   - Failures increment `CatalogInstruments.ActivationFailedToActivate`
   - Delay 5 seconds before deactivation (line 1646)
   - Prevents activation failure storms

**Concurrency**: Registration happens under timeout (line 1510), but lifecycle callbacks are not interruptible

### Deactivation Process

**Code**: `ActivationData.cs:1668-1826`

**Key Steps**:

1. **Initiation** (lines 577-612)
   - Can be called from any state
   - Sets `DeactivationReason` and `DeactivationStartTime`
   - Cancels pending operations
   - Transitions to Deactivating state
   - Schedules `Deactivate` command

2. **Finish Deactivating** (lines 1673-1826)
   - Stop all timers (line 1685)
   - Call `OnDeactivateAsync()` if grain was Valid (lines 1688-1709)
   - Run lifecycle stop (lines 1712-1727)
   - **Migration handling** (lines 1729-1762):
     - If `DehydrationContext` exists, perform migration
     - Place grain on new silo
     - Send dehydration context via `MigrationManager`
     - Update grain locator cache
   - Unregister from directory (lines 1768-1783)
   - Unregister from catalog (line 1812)
   - Dispose activation (line 1816)
   - Signal completion via `TaskCompletionSource` (line 1824)

**Error Tolerance**: All steps wrapped in try-catch, continue on errors (lines 1703, 1721, 1754, 1777)

### Message Processing

**Code**: `ActivationData.cs:958-1155`

**Processing Loop**: `ProcessPendingRequests()` (lines 958-1053)

```csharp
void ProcessPendingRequests()
{
    var i = 0;
    do {
        lock (this)
        {
            if (_waitingRequests.Count <= i) break;

            message = _waitingRequests[i].Message;

            // Reject if invalid state
            if (State != ActivationState.Valid && !message.IsLocalOnly) {
                ProcessRequestsToInvalidActivation();
                break;
            }

            // Check if can invoke (reentrancy)
            if (!MayInvokeRequest(message)) {
                ++i;  // Try next message

                // Detect stuck activation
                if (currentRequestActiveTime > MaxRequestProcessingTime) {
                    DeactivateStuckActivation();
                }
                continue;
            }

            // Check version compatibility
            if (message.InterfaceVersion > 0) {
                var compatible = compatibilityDirector.IsCompatible(...);
                if (!compatible) {
                    Deactivate(new DeactivationReason(...));
                    return;
                }
            }

            _waitingRequests.RemoveAt(i);
            RecordRunning(message, message.IsAlwaysInterleave);
        }

        // Invoke outside lock
        InvokeIncomingRequest(message);
    } while (true);
}
```

**Critical Patterns**:
1. **Lock-step iteration**: Hold lock, check message, release lock, process
2. **Skip non-invokable**: Increment `i` to try next message (reentrancy support)
3. **Stuck detection**: Track `_blockingRequest` duration (lines 989-1006)
4. **Version compatibility**: Deactivate if incompatible (lines 1012-1029)
5. **Invoke outside lock**: `InvokeIncomingRequest()` called after releasing lock

### Reentrancy Control

**Code**: `ActivationData.cs:1111-1155`

```csharp
bool MayInvokeRequest(Message incoming)
{
    if (!IsCurrentlyExecuting) return true;

    // Always-interleave messages (e.g., timers)
    if (incoming.IsAlwaysInterleave) return true;

    // No blocking request = reentrant grain
    if (_blockingRequest is null) return true;

    // Read-only messages can interleave with other read-only
    if (_blockingRequest.IsReadOnly && incoming.IsReadOnly) return true;

    // Call-chain reentrancy
    if (incoming.GetReentrancyId() is Guid id && IsReentrantSection(id)) {
        return true;
    }

    // Custom interleave predicate
    if (GetComponent<GrainCanInterleave>() is { } canInterleave) {
        return canInterleave.MayInterleave(GrainInstance, incoming);
    }

    return false;
}
```

**Reentrancy Types**:
1. **Always-interleave**: System messages, timers
2. **Reentrant grains**: `_blockingRequest == null`
3. **Read-only**: Concurrent read operations
4. **Call-chain reentrancy**: Tracked via `ReentrantRequestTracker` (lines 2130-2159)
5. **Custom predicate**: Application-defined interleaving

### Stuck Activation Detection

**Code**: `ActivationData.cs:614-629, 992-1006, 1078-1088`

**Two types of stuck detection**:

1. **Stuck processing message** (lines 992-1006):
   - Track duration since `_blockingRequest` started
   - If exceeds `MaxRequestProcessingTime`, call `DeactivateStuckActivation()`
   - Mark as stuck, keep dangling, unregister from catalog
   - New messages will route to fresh activation

2. **Stuck deactivating** (lines 1078-1088):
   - Track duration since `DeactivationStartTime`
   - If exceeds `MaxRequestProcessingTime` and still deactivating, mark stuck
   - Forward messages to new activation

**Recovery**: System "gives up" on stuck activation and creates new one, old one remains in memory

### Overload Protection

**Code**: `ActivationData.cs:349-378`

```csharp
public LimitExceededException? CheckOverloaded()
{
    int maxRequestsHardLimit = _shared.MessagingOptions.MaxEnqueuedRequestsHardLimit;
    int maxRequestsSoftLimit = _shared.MessagingOptions.MaxEnqueuedRequestsSoftLimit;

    if (IsStatelessWorker) {
        // Different limits for StatelessWorker
        maxRequestsHardLimit = _shared.MessagingOptions.MaxEnqueuedRequestsHardLimit_StatelessWorker;
        maxRequestsSoftLimit = _shared.MessagingOptions.MaxEnqueuedRequestsSoftLimit_StatelessWorker;
    }

    int count = GetRequestCount();

    if (maxRequestsHardLimit > 0 && count > maxRequestsHardLimit) {
        return new LimitExceededException(...);  // Reject message
    }

    if (maxRequestsSoftLimit > 0 && count > maxRequestsSoftLimit) {
        LogWarnActivationTooManyRequests(...);  // Just warn
        return null;
    }

    return null;
}
```

**Checked on message receive**: `ReceiveRequest()` (lines 1401-1417)

---

## ActivationDirectory: Thread-Safe Registry

### Purpose
Simple thread-safe mapping from `GrainId` to `IGrainContext`.

**Code**: `ActivationDirectory.cs:11-48`

```csharp
internal sealed class ActivationDirectory
{
    private int _activationsCount;
    private readonly ConcurrentDictionary<GrainId, IGrainContext> _activations = new();

    public IGrainContext? FindTarget(GrainId key)
    {
        _activations.TryGetValue(key, out var result);
        return result;
    }

    public void RecordNewTarget(IGrainContext target)
    {
        if (_activations.TryAdd(target.GrainId, target))
        {
            Interlocked.Increment(ref _activationsCount);
        }
    }

    public bool RemoveTarget(IGrainContext target)
    {
        if (_activations.TryRemove(KeyValuePair.Create(target.GrainId, target)))
        {
            Interlocked.Decrement(ref _activationsCount);
            return true;
        }
        return false;
    }
}
```

**Pattern**: ConcurrentDictionary + Interlocked counter
- Separate counter for fast `Count` access without dictionary enumeration
- `TryRemove(KeyValuePair)` ensures we remove exact instance (not a race with recreate)

---

## Grain Base Classes

### Grain Base Class

**Code**: `Grain.cs:15-161`

**Purpose**: Base class providing user-facing API for all grains

**Key APIs**:
- `OnActivateAsync(CancellationToken)` - hook called after activation (line 145)
- `OnDeactivateAsync(DeactivationReason, CancellationToken)` - hook called before deactivation (line 152)
- `DeactivateOnIdle()` - request deactivation (line 109)
- `MigrateOnIdle()` - request migration (line 120)
- `DelayDeactivation(TimeSpan)` - extend lifetime (line 133)
- `RegisterTimer(...)` - register grain timer (line 96, obsolete)

**Context Access**:
- `GrainContext` - access to activation context (line 21)
- `GrainFactory` - create grain references (line 30)
- `ServiceProvider` - DI container (line 35)

### IGrain Marker Interfaces

**Code**: `IGrain.cs:10-48`

Simple marker interfaces for different key types:
- `IGrain` - base marker
- `IGrainWithGuidKey` - Guid-keyed grains
- `IGrainWithIntegerKey` - long-keyed grains
- `IGrainWithStringKey` - string-keyed grains
- Compound key variants

**Purpose**: Type safety and grain reference creation

---

## Critical Concurrency Patterns

### 1. Double-Check Locking (Catalog)

**Pattern**: Check without lock → lock → check again → mutate

**Why**: Avoid lock contention on fast path (activation already exists)

**Correctness**: Second check under lock prevents TOCTOU races

### 2. Lock Granularity (ActivationData)

**Pattern**: `lock(this)` for all state mutations, but release before expensive operations

**Example**:
```csharp
lock (this) {
    message = _waitingRequests[i].Message;
    _waitingRequests.RemoveAt(i);
    RecordRunning(message);
}
// Release lock before invoking
InvokeIncomingRequest(message);
```

**Why**: Prevents deadlocks and reduces lock hold time

### 3. Interlocked Completion Flag (ActivationData)

**Pattern**: `Interlocked.CompareExchange(ref completed, 1, 0)` for one-time operations

**Used in**: Message completion, timeout handling, cancellation (multiple callers, exactly-once semantics)

### 4. Command Pattern (ActivationData)

**Pattern**: Enqueue lifecycle commands, process serially with message loop

**Why**: Serializes lifecycle operations with message processing without complex coordination

---

## Patterns for Moonpool

### ✅ Adopt These Patterns

1. **Explicit State Machines**: Use enums for lifecycle states, enforce transitions under lock
   - Rust: `enum ActivationState { Creating, Activating, Valid, Deactivating, Invalid }`

2. **Double-Check Locking**: Fast path check → lock → check again → mutate
   - Rust: `if let Some(activation) = catalog.get(&id)` → `lock()` → check again

3. **Message Queue Separation**: Separate waiting vs running queues for reentrancy
   - Rust: `VecDeque<Message>` for waiting, `HashMap<CorrelationId, Instant>` for running

4. **Command Pattern for Lifecycle**: Enqueue operations, process serially
   - Rust: `enum LifecycleCommand { Activate, Deactivate, Rehydrate }`

5. **Stuck Detection**: Track long-running operations, forcibly terminate
   - Rust: Use buggify to inject delays, verify detection triggers

6. **Lock Before Expensive Ops**: Hold locks only for mutations, release before I/O
   - Rust: Scope locks carefully, avoid holding `MutexGuard` across `.await`

### ⚠️ Adapt These Patterns

1. **Coarse Locking (Catalog)**: Orleans uses single lock for all activations
   - Moonpool: Measure first, consider sharded locks if needed (DashMap)

2. **Parallel Deactivation**: Orleans uses `Parallel.ForEachAsync` with high parallelism
   - Moonpool: Not needed - single-threaded simulation runtime

3. **ConcurrentDictionary**: Orleans relies on .NET concurrent collections
   - Moonpool: Use `HashMap` with lock or `DashMap`, profile first

4. **TaskScheduler Integration**: Orleans uses custom `TaskScheduler`
   - Moonpool: Use `Provider` traits, not Rust's `TaskScheduler`

### ❌ Avoid These Patterns

1. **Thread Pool Integration**: Orleans uses `IThreadPoolWorkItem`
   - Moonpool: Single-threaded, use `spawn_local` via `TaskProvider`

2. **Request Context Ambient State**: Orleans uses `RequestContext.CallContextData`
   - Moonpool: Pass context explicitly or use task-local storage

3. **Synchronous Locks in Async**: Orleans uses `lock(this)` in async methods
   - Moonpool: Use `tokio::sync::Mutex` for `.await` points, `std::sync::Mutex` for fast critical sections

---

## Open Questions for Moonpool

1. **Lock granularity**: Start with coarse locks (`Mutex<HashMap>`) or fine-grained (`DashMap`)?
   - Recommendation: Start coarse, measure, optimize if needed

2. **Activation addressing**: Use `Arc<ActivationData>` or handle-based system?
   - Recommendation: `Arc` for simplicity, matches Orleans' reference-based model

3. **Message loop**: Separate task per activation or poll-based processing?
   - Recommendation: Task per activation (matches Orleans), simpler lifecycle

4. **Stuck detection**: Timer-based polling or event-driven?
   - Recommendation: Timer-based (matches Orleans), deterministic in simulation

5. **Overload protection**: Per-activation limits or global limits?
   - Recommendation: Both - per-activation for safety, global for backpressure

---

## Summary

Orleans' activation lifecycle is built on:
- **Catalog** - Coarse-grained locking, simple race handling
- **ActivationData** - Complex state machine, message queues, lifecycle commands
- **ActivationDirectory** - Thread-safe registry with separate counter
- **Grain base classes** - User-facing API surface

Key architectural principles:
- Explicit state machines for determinism
- Lock before expensive operations, release quickly
- Command pattern for lifecycle serialization
- Stuck detection for fault tolerance
- Overload protection at message receive

For Moonpool Phase 12: Start with simplified version of these patterns, use Provider traits for testability, measure before optimizing concurrency.
