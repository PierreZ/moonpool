# Activation Lifecycle Management

## Reference Files
- `Catalog.cs:1-435` - Main activation catalog and lifecycle orchestration
- `ActivationData.cs:1-2436` - Per-activation state and lifecycle management
- `ActivationState.cs:1-31` - Activation state enum (separate file)
- `ActivationDirectory.cs:1-95` - Thread-safe activation registry (implements IEnumerable, IAsyncDisposable, IDisposable)
- `Grain.cs:1-305` - Base grain class and API surface (includes `Grain<TGrainState>` generic variant)
- `IGrain.cs:1-48` - Grain marker interfaces
- `DeactivationReason.cs:1-71` - Deactivation reason codes and descriptions

## Overview

Orleans uses a **catalog-based activation model** where grains (actors) are created on-demand and managed through their lifecycle. The system ensures:
- **Single activation per grain** (except StatelessWorker)
- **Deterministic state transitions** through explicit state machines
- **Thread-safe concurrent access** via lock striping and fine-grained locking
- **Graceful lifecycle management** with timeout enforcement

The architecture separates concerns:
- **Catalog** - Orchestrates activation creation/destruction across the silo
- **ActivationData** - Manages per-activation state, messages, and lifecycle
- **ActivationDirectory** - Thread-safe registry mapping GrainId -> IGrainContext
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

**Code**: `Catalog.cs:126-215`

**Pattern**: Double-check locking with striped locks and race handling

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
    else if (grainId.IsSystemTarget())
    {
        rehydrationContext?.Dispose();
        return null;
    }

    // Lock using striped lock for this grain ID
    lock (GetStripedLock(grainId))
    {
        // Second check with lock (race protection)
        if (TryGetGrainContext(grainId, out result))
        {
            rehydrationContext?.Dispose();
            return result;
        }

        if (_siloStatusOracle.CurrentStatus == SiloStatus.Active)
        {
            // Create new activation
            var address = new GrainAddress { ... };
            result = this.grainActivator.CreateInstance(address);
            activations.RecordNewTarget(result);
        }
    } // End lock

    if (result is null)
    {
        rehydrationContext?.Dispose();
        return UnableToCreateActivation(this, grainId);
    }

    CatalogInstruments.ActivationsCreated.Add(1);

    // Rehydration occurs before activation
    if (rehydrationContext is not null)
    {
        result.Rehydrate(rehydrationContext);
    }

    // Async initialization outside of lock
    result.Activate(requestContextData);
    return result;
}
```

**Critical Insights**:
1. **Double-check pattern**: Check -> Lock -> Check again -> Create
2. **Striped locking**: Uses 32 striped locks (`lock(GetStripedLock(grainId))`) hashed by GrainId for reduced contention (lines 24-27, 78-83) -- this replaces the older coarse `lock(activations)` approach
3. **System target early return**: System targets bypass activation creation (lines 136-140)
4. **Activation outside lock**: `result.Activate()` called after releasing lock
5. **Silo status check**: Only creates when `SiloStatus.Active` (line 150), not just "not terminating"
6. **Rehydration before activation**: `result.Rehydrate()` called before `result.Activate()` (lines 174-177)
7. **Instrumentation**: `CatalogInstruments.ActivationsCreated` (line 171)

### Lock Striping Implementation

**Code**: `Catalog.cs:24-27, 78-83`

```csharp
private const int LockCount = 32; // Must be a power of 2
private const int LockMask = LockCount - 1;
private readonly object[] _locks = new object[LockCount];

[MethodImpl(MethodImplOptions.AggressiveInlining)]
private object GetStripedLock(in GrainId grainId)
{
    var hash = grainId.GetUniformHashCode();
    var lockIndex = (int)(hash & LockMask);
    return _locks[lockIndex];
}
```

### Catalog Lifecycle Integration

**Code**: `Catalog.cs:373-377`

The Catalog participates in the Silo lifecycle to receive the status oracle. This enables checking if the silo is active before creating activations.

### Deactivation Orchestration

**Code**: `Catalog.cs:244-264`

```csharp
internal async Task DeactivateActivations(
    DeactivationReason reason,
    List<IGrainContext> list,
    CancellationToken cancellationToken)
{
    if (list == null || list.Count == 0) return;

    var options = new ParallelOptions
    {
        CancellationToken = CancellationToken.None,  // Don't cancel cleanup
        MaxDegreeOfParallelism = Environment.ProcessorCount * 512
    };

    await Parallel.ForEachAsync(list, options, (activation, _) =>
    {
        if (activation.GrainId.Type.IsSystemTarget())
        {
            return ValueTask.CompletedTask;
        }

        activation.Deactivate(reason, cancellationToken);
        return new (activation.Deactivated);  // Wait for completion
    }).WaitAsync(cancellationToken);
}
```

**Pattern**: Parallel deactivation with completion tracking
- High parallelism (512x processor count) for I/O-bound cleanup
- Skips system targets (lines 256-259)
- Waits for each activation's `Deactivated` task
- Used during shutdown and directory failure scenarios

**Additional**: `DeactivateAllActivations` (lines 266-287) iterates over all activations for full silo shutdown.

---

## ActivationData: Per-Activation State Management

### Purpose
ActivationData is the heavyweight implementation of `IGrainContext`. Each activation has:
- Message queues (waiting and running requests)
- Lifecycle state machine
- Timers and components
- Service scope and dependency injection
- Message processing loop

**Code**: `ActivationData.cs:31-85` - Construction and core fields

### Activation State Machine

**Code**: `ActivationState.cs:3-29` - `ActivationState` enum (separate file)

```
[Creating] -> [Activating] -> [Valid] -> [Deactivating] -> [Invalid]
```

**State Transitions**:
- **Creating**: Initial state, grain instance created via `SetGrainInstance()` (line 339)
- **Activating**: Lifecycle starting, `OnActivateAsync()` called (line 1555)
- **Valid**: Ready to process messages (line 1594)
- **Deactivating**: Shutting down gracefully (line 572)
- **Invalid**: Disposed, no longer usable (line 779)

**State Guards**: All state changes happen under `lock(this)` (lines 343, 549, 1590)

### Message Queues

**Code**: `ActivationData.cs:47-48, 886-1178`

```csharp
private readonly List<(Message Message, CoarseStopwatch QueuedTime)> _waitingRequests = new();
private readonly Dictionary<Message, CoarseStopwatch> _runningRequests = new();
private Message? _blockingRequest;
```

**Two-queue system**:
1. **Waiting queue**: Messages not yet being processed
2. **Running requests**: Messages currently executing (for reentrancy support)
3. **Blocking request**: The non-reentrant message currently blocking the activation

**Message Loop**: `RunMessageLoop()` (lines 886-1178)
- Runs forever in background task (line 896: `while (true)`)
- Processes operations (activation, deactivation) when no messages running
- Processes messages from waiting queue when state is Valid
- Never terminates - gets GC'd when activation is deactivated and dereferenced

### Lifecycle Commands

**Code**: `ActivationData.cs:2087-2145`

**Pattern**: Command objects for lifecycle operations

```csharp
private abstract class Command(CancellationTokenSource cts) : IDisposable
{
    public CancellationToken CancellationToken => _cts.Token;

    public sealed class Activate(...) : Command
    public sealed class Deactivate(...) : Command
    public sealed class Rehydrate(...) : Command
    public sealed class Delay(TimeSpan duration) : Command
}
```

**Usage**: Commands are enqueued via `ScheduleOperation()` (line 467) and processed by `ProcessOperationsAsync()` (lines 1123-1177)

**Why commands?**
- Serializes lifecycle operations with message processing
- Enables cancellation of pending operations
- Separates scheduling from execution
- Commands implement `IDisposable` for proper CancellationTokenSource cleanup

### Activation Process

**Code**: `ActivationData.cs:1464-1635`

**Key Steps**:

1. **Directory Registration** (lines 1478-1551)
   - Register with grain directory (if not StatelessWorker)
   - Handle race: if another activation already registered, forward to it
   - Retry with previous registration if stale entry on same silo exists
   - Set `ForwardingAddress` if duplicate detected

2. **Set State to Activating** (line 1555)

3. **Run Lifecycle** (lines 1561-1598)
   - Import request context
   - Call `lifecycle.OnStart()`
   - Call `grain.OnActivateAsync()`
   - Both wrapped in try-catch with detailed logging

4. **Set State to Valid** (line 1594)

5. **Error Handling** (lines 1601-1623)
   - Failures increment `CatalogInstruments.ActivationFailedToActivate`
   - Delay 5 seconds before deactivation (line 1616)
   - Prevents activation failure storms

**Concurrency**: Registration happens under timeout via `WaitAsync(cancellationToken)` (line 1489), but lifecycle callbacks are not interruptible

### Deactivation Process

**Code**: `ActivationData.cs:1643-1798`

**Key Steps**:

1. **Initiation** (lines 547-578)
   - Can be called from any state except Invalid
   - Sets `DeactivationReason` and `DeactivationStartTime`
   - Cancels pending operations
   - Transitions to Deactivating state
   - Schedules `Deactivate` command with linked CancellationToken and `DeactivationTimeout`

2. **Finish Deactivating** (lines 1643-1798)
   - Stop all timers (line 1652)
   - Call `OnDeactivateAsync()` if grain was Valid (lines 1657-1674)
   - Run lifecycle stop (lines 1677-1692)
   - **Migration handling** (lines 1694-1700, with `StartMigrationAsync` at 1765-1797):
     - If `DehydrationContext` exists, perform migration
     - Place grain on new silo via `PlaceMigratingGrainAsync`
     - Call `OnDehydrate` to capture state
     - Send dehydration context via `MigrationManager`
     - Update grain locator cache
   - Unregister from directory (lines 1706-1721)
   - Record shutdown metrics (lines 1733-1748)
   - Unregister from catalog (line 1750)
   - Dispose activation (line 1754)
   - Signal completion via `TaskCompletionSource` (line 1762)

**Error Tolerance**: All steps wrapped in try-catch, continue on errors (lines 1671, 1690, 1716, 1729)

### Message Processing

**Code**: `ActivationData.cs:924-1018`

**Processing Loop**: `ProcessPendingRequests()` (lines 924-1018)

```csharp
void ProcessPendingRequests()
{
    var i = 0;
    do {
        lock (this)
        {
            if (_waitingRequests.Count <= i) break;

            message = _waitingRequests[i].Message;

            // Reject if invalid state (with exception for local-only messages during deactivation)
            if (State != ActivationState.Valid
                && !(message.IsLocalOnly && State is ActivationState.Deactivating))
            {
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
3. **Local-only exception**: Local-only messages (system operations) are still accepted during Deactivating state (line 942)
4. **Stuck detection**: Track `_blockingRequest` duration (lines 955-971)
5. **Version compatibility**: Deactivate if incompatible (lines 978-994)
6. **Invoke outside lock**: `InvokeIncomingRequest()` called after releasing lock

### Reentrancy Control

**Code**: `ActivationData.cs:1076-1121`

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

    // Custom interleave predicate (checks both incoming and blocking message)
    if (GetComponent<GrainCanInterleave>() is { } canInterleave) {
        return canInterleave.MayInterleave(GrainInstance, incoming)
            || canInterleave.MayInterleave(GrainInstance, _blockingRequest);
    }

    return false;
}
```

**Reentrancy Types**:
1. **Always-interleave**: System messages, timers
2. **Reentrant grains**: `_blockingRequest == null`
3. **Read-only**: Concurrent read operations
4. **Call-chain reentrancy**: Tracked via `ReentrantRequestTracker` (lines 2147-2176)
5. **Custom predicate**: Application-defined interleaving -- checks both the incoming message AND the blocking request (line 1111)

### Stuck Activation Detection

**Code**: `ActivationData.cs:580-595, 955-971, 1041-1061`

**Two types of stuck detection**:

1. **Stuck processing message** (lines 955-971):
   - Track duration since `_blockingRequest` started
   - If exceeds `MaxRequestProcessingTime`, call `DeactivateStuckActivation()`
   - Mark as stuck, keep dangling, unregister from catalog and directory
   - New messages will route to fresh activation

2. **Stuck deactivating** (lines 1041-1061):
   - Track duration since `DeactivationStartTime`
   - If exceeds `MaxRequestProcessingTime` and still deactivating, mark stuck
   - Forward messages to new activation

**Recovery**: System "gives up" on stuck activation and creates new one, old one remains in memory

### Overload Protection

**Code**: `ActivationData.cs:377-406`

```csharp
public LimitExceededException? CheckOverloaded()
{
    string limitName = nameof(SiloMessagingOptions.MaxEnqueuedRequestsHardLimit);
    int maxRequestsHardLimit = _shared.MessagingOptions.MaxEnqueuedRequestsHardLimit;
    int maxRequestsSoftLimit = _shared.MessagingOptions.MaxEnqueuedRequestsSoftLimit;

    if (IsStatelessWorker) {
        // Different limits for StatelessWorker
        limitName = nameof(SiloMessagingOptions.MaxEnqueuedRequestsHardLimit_StatelessWorker);
        maxRequestsHardLimit = _shared.MessagingOptions.MaxEnqueuedRequestsHardLimit_StatelessWorker;
        maxRequestsSoftLimit = _shared.MessagingOptions.MaxEnqueuedRequestsSoftLimit_StatelessWorker;
    }

    if (maxRequestsHardLimit <= 0 && maxRequestsSoftLimit <= 0) return null; // No limits are set

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

**Checked on message receive**: `ReceiveRequest()` (lines 1374-1390)

---

## ActivationDirectory: Thread-Safe Registry

### Purpose
Simple thread-safe mapping from `GrainId` to `IGrainContext`.

**Code**: `ActivationDirectory.cs:11-47`

```csharp
internal sealed class ActivationDirectory : IEnumerable<KeyValuePair<GrainId, IGrainContext>>,
    IAsyncDisposable, IDisposable
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
- Implements `IEnumerable` for catalog iteration during shutdown and silo failure handling
- Implements `IAsyncDisposable` / `IDisposable` for graceful cleanup of all activations (lines 53-94)

---

## Grain Base Classes

### Grain Base Class

**Code**: `Grain.cs:16-173`

**Purpose**: Base class providing user-facing API for all grains

**Key APIs**:
- `OnActivateAsync(CancellationToken)` - hook called after activation (line 157)
- `OnDeactivateAsync(DeactivationReason, CancellationToken)` - hook called before deactivation (line 164)
- `DeactivateOnIdle()` - request deactivation (line 117)
- `MigrateOnIdle()` - request migration (line 130)
- `DelayDeactivation(TimeSpan)` - extend lifetime (line 143)
- `RegisterTimer(...)` - register grain timer (line 102, obsolete -- use `RegisterGrainTimer` instead)

**Context Access**:
- `GrainContext` - access to activation context (line 23)
- `GrainFactory` - create grain references (line 32)
- `ServiceProvider` - DI container (line 38)

### Grain<TGrainState> Generic Variant

**Code**: `Grain.cs:179-305`

Base class for grains with declared persistent state. Includes:
- Lifecycle observer for automatic state loading during activation
- Migration support via `IGrainMigrationParticipant` (dehydrate/rehydrate state)
- Avoids re-reading state if already rehydrated from migration

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

### 1. Double-Check Locking with Striped Locks (Catalog)

**Pattern**: Check without lock -> striped lock by GrainId hash -> check again -> mutate

**Why**: Avoid lock contention on fast path (activation already exists). Striped locks allow concurrent activation of different grains while preventing duplicate activation of the same grain.

**Correctness**: Second check under lock prevents TOCTOU races. 32 lock stripes balance contention reduction vs memory overhead.

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

### Adopt These Patterns

1. **Explicit State Machines**: Use enums for lifecycle states, enforce transitions under lock
   - Rust: `enum ActivationState { Creating, Activating, Valid, Deactivating, Invalid }`

2. **Double-Check Locking**: Fast path check -> lock -> check again -> mutate
   - Rust: `if let Some(activation) = catalog.get(&id)` -> `lock()` -> check again

3. **Message Queue Separation**: Separate waiting vs running queues for reentrancy
   - Rust: `VecDeque<Message>` for waiting, `HashMap<CorrelationId, Instant>` for running

4. **Command Pattern for Lifecycle**: Enqueue operations, process serially
   - Rust: `enum LifecycleCommand { Activate, Deactivate, Rehydrate }`

5. **Stuck Detection**: Track long-running operations, forcibly terminate
   - Rust: Use buggify to inject delays, verify detection triggers

6. **Lock Before Expensive Ops**: Hold locks only for mutations, release before I/O
   - Rust: Scope locks carefully, avoid holding `MutexGuard` across `.await`

### Adapt These Patterns

1. **Striped Locking (Catalog)**: Orleans uses 32 striped locks hashed by GrainId
   - Moonpool: Not needed in single-threaded simulation; if multi-threaded, consider sharded locks (DashMap)

2. **Parallel Deactivation**: Orleans uses `Parallel.ForEachAsync` with high parallelism
   - Moonpool: Not needed - single-threaded simulation runtime

3. **ConcurrentDictionary**: Orleans relies on .NET concurrent collections
   - Moonpool: Use `HashMap` with lock or `DashMap`, profile first

4. **TaskScheduler Integration**: Orleans uses custom `TaskScheduler`
   - Moonpool: Use `Provider` traits, not Rust's `TaskScheduler`

### Avoid These Patterns

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
- **Catalog** - Striped locking (32 locks by GrainId hash), double-check pattern, race handling
- **ActivationData** - Complex state machine, message queues, lifecycle commands
- **ActivationDirectory** - Thread-safe registry with separate counter, enumerable for shutdown
- **Grain base classes** - User-facing API surface with migration support

Key architectural principles:
- Explicit state machines for determinism
- Lock before expensive operations, release quickly
- Command pattern for lifecycle serialization
- Stuck detection for fault tolerance
- Overload protection at message receive
- Lock striping for reduced contention at catalog level

For Moonpool Phase 12: Start with simplified version of these patterns, use Provider traits for testability, measure before optimizing concurrency.
