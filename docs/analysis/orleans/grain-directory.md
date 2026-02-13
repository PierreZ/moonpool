# Orleans Grain Directory Analysis

**Purpose**: Distributed location service for virtual actors using consistent hash ring with fault tolerance

**Key insight**: The grain directory is a partitioned distributed hash table (DHT) that maps GrainId → GrainAddress, enabling location transparency. Orleans uses a consistent ring with 30 ranges per silo, view-based reconfigurations following Virtual Synchrony methodology, and automatic recovery from failures.

---

## Overview

The grain directory solves the **location transparency** problem: How do you route messages to virtual actors when you don't know which silo hosts them?

### Key Responsibilities

1. **Registration** - Record where grain activations are located
2. **Lookup** - Find the silo hosting a specific grain
3. **Unregistration** - Remove stale location entries
4. **Caching** - Optimize repeated lookups with local caches
5. **Partitioning** - Distribute directory data across silos using consistent hashing
6. **Rebalancing** - Transfer directory partitions during membership changes
7. **Recovery** - Reconstruct directory after silo failures

### Architecture Layers

```
┌──────────────────────────────────────────────────────────┐
│ GrainLocator (Facade)                                    │  ← Simple API for callers
├──────────────────────────────────────────────────────────┤
│ GrainLocatorResolver                                     │  ← Maps grain type → IGrainLocator
│   ├─ DhtGrainLocator (default grains)                    │
│   ├─ CachedGrainLocator (custom directory grains)        │
│   └─ ClientGrainLocator (client grains)                  │
├──────────────────────────────────────────────────────────┤
│ GrainDirectoryResolver                                   │  ← Maps grain type → IGrainDirectory
├──────────────────────────────────────────────────────────┤
│ LocalGrainDirectory → DistributedGrainDirectory (SysTarget) │  ← Consistent ring coordination
├──────────────────────────────────────────────────────────┤
│ GrainDirectoryPartition (30 per silo, each a SysTarget)  │  ← Range ownership with view changes
├──────────────────────────────────────────────────────────┤
│ Dictionary<GrainId, GrainAddress> + LRU Cache            │  ← In-memory hash table + cache
└──────────────────────────────────────────────────────────┘
```

---

## Core Components

### 1. GrainLocator (Facade)

**File**: `GrainLocator.cs:1-51`

Simple facade that delegates to the appropriate `IGrainLocator` implementation based on grain type. The locator resolver (`GrainLocatorResolver`) selects between `DhtGrainLocator` (for grains using the default distributed directory), `CachedGrainLocator` (for grains with custom directory), and `ClientGrainLocator` (for client grains).

```csharp
internal class GrainLocator
{
    private readonly GrainLocatorResolver _grainLocatorResolver;

    // Lookup grain location (may query remote directory partition)
    public ValueTask<GrainAddress?> Lookup(GrainId grainId)
        => GetGrainLocator(grainId.Type).Lookup(grainId);

    // Register grain activation location
    public Task<GrainAddress?> Register(GrainAddress address, GrainAddress? previousRegistration)
        => GetGrainLocator(address.GrainId.Type).Register(address, previousRegistration);

    // Unregister grain activation
    public Task Unregister(GrainAddress address, UnregistrationCause cause)
        => GetGrainLocator(address.GrainId.Type).Unregister(address, cause);

    // Check local cache without remote call
    public bool TryLookupInCache(GrainId grainId, [NotNullWhen(true)] out GrainAddress? address)
        => GetGrainLocator(grainId.Type).TryLookupInCache(grainId, out address);

    // Invalidate cached entry (by grain ID or by address)
    public void InvalidateCache(GrainId grainId)
        => GetGrainLocator(grainId.Type).InvalidateCache(grainId);
    public void InvalidateCache(GrainAddress address)
        => GetGrainLocator(address.GrainId.Type).InvalidateCache(address);

    // Update cache with valid address (from message header)
    public void UpdateCache(GrainId grainId, SiloAddress siloAddress)
        => GetGrainLocator(grainId.Type).UpdateCache(grainId, siloAddress);

    // Update or invalidate cache from a GrainAddressCacheUpdate (piggybacked on messages)
    public void UpdateCache(GrainAddressCacheUpdate update)
    {
        if (update.ValidGrainAddress is { } validAddress)
            UpdateCache(validAddress.GrainId, validAddress.SiloAddress);
        else
            InvalidateCache(update.InvalidGrainAddress);
    }

    private IGrainLocator GetGrainLocator(GrainType grainType)
        => _grainLocatorResolver.GetGrainLocator(grainType);
}
```

**Pattern**: **Facade** - Simplifies directory access by hiding resolver complexity

---

### 2. GrainDirectoryResolver (Strategy Selection)

**File**: `GrainDirectoryResolver.cs:12-88`

Selects which `IGrainDirectory` implementation to use for each grain type based on grain properties. Note: this resolves the *directory* (storage backend), not the *locator* (lookup strategy). The `GrainLocatorResolver` separately maps grain types to `IGrainLocator` implementations: `DhtGrainLocator` for grains using the default directory (null from resolver), `CachedGrainLocator` for custom directories, and `ClientGrainLocator` for client grain types.

```csharp
internal class GrainDirectoryResolver
{
    private readonly Dictionary<string, IGrainDirectory> directoryPerName;
    private readonly ConcurrentDictionary<GrainType, IGrainDirectory> directoryPerType;
    private readonly GrainPropertiesResolver grainPropertiesResolver;
    private readonly IGrainDirectoryResolver[] resolvers;

    public IGrainDirectory DefaultGrainDirectory { get; }

    // Resolve directory for grain type (cached)
    public IGrainDirectory Resolve(GrainType grainType)
        => directoryPerType.GetOrAdd(grainType, getGrainDirectoryInternal);

    // Returns true if grain type uses the default (built-in DHT) directory
    public bool IsUsingDefaultDirectory(GrainType grainType) => Resolve(grainType) == null;

    private IGrainDirectory GetGrainDirectoryPerType(GrainType grainType)
    {
        if (TryGetNonDefaultGrainDirectory(grainType, out var result))
            return result;

        return DefaultGrainDirectory;  // null when no named directory is registered
    }

    internal bool TryGetNonDefaultGrainDirectory(GrainType grainType, out IGrainDirectory directory)
    {
        grainPropertiesResolver.TryGetGrainProperties(grainType, out var properties);

        // Try custom resolvers first (e.g., for stateless grains, grain services)
        foreach (var resolver in resolvers)
        {
            if (resolver.TryResolveGrainDirectory(grainType, properties, out directory))
                return true;
        }

        // Check for [GrainDirectory("name")] attribute
        if (properties is not null
            && properties.Properties.TryGetValue(WellKnownGrainTypeProperties.GrainDirectory, out var directoryName)
            && !string.IsNullOrWhiteSpace(directoryName))
        {
            if (directoryPerName.TryGetValue(directoryName, out directory))
                return true;
            else
                throw new KeyNotFoundException($"Could not resolve grain directory {directoryName} for grain type {grainType}");
        }

        directory = null;
        return false;
    }
}
```

**Key Decisions**:
- Client grains: `ClientGrainLocator` (resolved in `GrainLocatorResolver`)
- Default directory grains: `DhtGrainLocator` wrapping `LocalGrainDirectory` -> `DistributedGrainDirectory`
- Custom directory grains: `CachedGrainLocator` wrapping user-provided `IGrainDirectory`
- Custom attribute: `[GrainDirectory("CustomName")]`

---

### 3. DistributedGrainDirectory (Consistent Ring DHT)

**File**: `DistributedGrainDirectory.cs:60-469`

SystemTarget that implements a distributed hash table using a consistent ring with 30 partitions per silo. Also implements `IGrainDirectoryClient` (for recovery RPCs from other silos) and `ILifecycleParticipant<ISiloLifecycle>` (for startup/shutdown lifecycle).

#### Architecture Summary (from file header comments, lines 19-58):

```
The grain directory is a key-value store where:
  Key = GrainId
  Value = GrainAddress (silo location + activation ID)

Partitioning Strategy:
  - Consistent hash ring with ranges assigned to active silos
  - 30 ranges per silo (virtual nodes for better load distribution)
  - Hash(GrainId) → Range → Owner Silo

View Changes (Virtual Synchrony):
  - Normal operation: Process requests locally without coordination
  - View change: Coordinate with other silos to transfer ranges

  When partition GROWS (new silo joined, or crashed silo's ranges acquired):
    - Previous owner seals lost range and creates snapshot of entries
    - New owner pulls snapshot via GetSnapshotAsync(version, rangeVersion, range)
    - New owner applies snapshot, then acknowledges via AcknowledgeSnapshotTransferAsync
    - If snapshot transfer fails (non-contiguous view, unavailable silo), recovery is performed

  When partition SHRINKS (new silo joined, taking some of our range):
    - Seal the lost range (create range lock)
    - Snapshot entries in that range, remove them from local directory
    - Hold snapshot until new owner pulls it or transfer is abandoned

Concurrency Control:
  - Versioned range locks (wedges) prevent access during view changes
  - All requests include caller's view number
  - All responses include partition's view number
  - View number mismatches trigger refresh and retry
  - Non-contiguous view changes (skipped versions) trigger recovery instead of transfer
```

#### Core Implementation

```csharp
// DistributedGrainDirectory.cs:60-61
internal sealed partial class DistributedGrainDirectory
    : SystemTarget, IGrainDirectory, IGrainDirectoryClient, ILifecycleParticipant<ISiloLifecycle>
{
    private readonly DirectoryMembershipService _membershipService;
    private readonly ImmutableArray<GrainDirectoryPartition> _partitions;  // 30 partitions
    private long _recoveryMembershipVersion;
    private ActivationDirectory _localActivations;
    private GrainDirectoryResolver? _grainDirectoryResolver;

    // Constructor creates exactly DirectoryMembershipSnapshot.PartitionsPerSilo (30) partitions
    // DistributedGrainDirectory.cs:94-100
    var partitions = ImmutableArray.CreateBuilder<GrainDirectoryPartition>(DirectoryMembershipSnapshot.PartitionsPerSilo);
    for (var i = 0; i < DirectoryMembershipSnapshot.PartitionsPerSilo; i++)
    {
        partitions.Add(new GrainDirectoryPartition(i, this, grainFactory, shared));
    }

    // IGrainDirectory implementation - route to appropriate partition
    // DistributedGrainDirectory.cs:104-120
    public async Task<GrainAddress?> Lookup(GrainId grainId)
        => await InvokeAsync(
            grainId,
            static (partition, version, grainId, cancellationToken) => partition.LookupAsync(version, grainId),
            grainId,
            CancellationToken.None);

    public async Task<GrainAddress?> Register(GrainAddress address)
        => await InvokeAsync(
            address.GrainId,
            static (partition, version, address, cancellationToken) => partition.RegisterAsync(version, address, null),
            address,
            CancellationToken.None);

    public async Task Unregister(GrainAddress address)
        => await InvokeAsync(
            address.GrainId,
            static (partition, version, address, cancellationToken) => partition.DeregisterAsync(version, address),
            address,
            CancellationToken.None);

    // Also has a Register overload with previousAddress (line 122-126)
    public async Task<GrainAddress?> Register(GrainAddress address, GrainAddress? previousAddress)
        => await InvokeAsync(...);

    // Core routing with view-aware retry logic
    // DistributedGrainDirectory.cs:130-199
    private async Task<TResult> InvokeAsync<TState, TResult>(
        GrainId grainId,
        Func<IGrainDirectoryPartition, MembershipVersion, TState, CancellationToken, ValueTask<DirectoryResult<TResult>>> func,
        TState state,
        CancellationToken cancellationToken,
        [CallerMemberName] string operation = "")
    {
        var view = _membershipService.CurrentView;
        var attempts = 0;
        const int MaxAttempts = 10;
        var delay = TimeSpan.FromMilliseconds(10);
        while (true)
        {
            cancellationToken.ThrowIfCancellationRequested();
            var initialRecoveryMembershipVersion = _recoveryMembershipVersion;

            // Find owner for this grain's hash (also checks recovery version)
            if (view.Version.Value < initialRecoveryMembershipVersion
                || !view.TryGetOwner(grainId, out var owner, out var partitionReference))
            {
                // No members in cluster or version too old - refresh view and retry
                if (view.Members.Length == 0 && view.Version.Value > 0)
                    return default!;

                var targetVersion = Math.Max(view.Version.Value + 1, initialRecoveryMembershipVersion);
                view = await _membershipService.RefreshViewAsync(new(targetVersion), cancellationToken);
                continue;
            }

            try
            {
                // Invoke on partition (local or remote SystemTarget call)
                var invokeResult = await func(partitionReference, view.Version, state, cancellationToken);
            }
            catch (OrleansMessageRejectionException) when (attempts < MaxAttempts && !cancellationToken.IsCancellationRequested)
            {
                // Target silo likely declared dead - back off and retry
                ++attempts;
                await Task.Delay(delay, cancellationToken);
                delay *= 1.5;
                continue;
            }

            // Check if recovery version changed (see comment on _recoveryMembershipVersion)
            if (initialRecoveryMembershipVersion != _recoveryMembershipVersion)
                continue;  // Retry with fresh view

            // Check if result is valid for current view
            if (!invokeResult.TryGetResult(view.Version, out var result))
            {
                // Remote has newer view - refresh and retry
                view = await _membershipService.RefreshViewAsync(invokeResult.Version, cancellationToken);
                continue;
            }

            return result;
        }
    }
}
```

**Key Insight**: The `_recoveryMembershipVersion` prevents race between registration and recovery. If a grain activates and registers while recovery is collecting activations, the registration must use a version >= recovery version. Otherwise, the registration might be lost.

**Pattern**: **Virtual Synchrony** - Two-phase operation (normal + view change) with wedges (range locks)

---

### 4. GrainDirectoryPartition (Range Ownership)

**Files**: `GrainDirectoryPartition.cs` + `GrainDirectoryPartition.Interface.cs` (partial class, split across two files)

Each silo has 30 partitions that manage directory ranges. `GrainDirectoryPartition` is itself a `SystemTarget` (can receive remote RPC calls). Partitions use versioned range locks during view changes. The main file handles view changes, snapshot transfers, and recovery. The Interface file implements the `IGrainDirectoryPartition` interface methods (Register, Lookup, Deregister).

#### View Change Processing (GrainDirectoryPartition.cs:302-344)

```csharp
internal sealed partial class GrainDirectoryPartition : SystemTarget, IGrainDirectoryPartition
{
    private readonly Dictionary<GrainId, GrainAddress> _directory = [];
    private readonly int _partitionIndex;
    private readonly DistributedGrainDirectory _owner;

    // Range locks: (range, version, completion) tuples prevent access during transfers
    private readonly List<(RingRange Range, MembershipVersion Version, TaskCompletionSource Completion)> _rangeLocks = [];

    // Snapshots held for transfer to new owners
    private readonly List<PartitionSnapshotState> _partitionSnapshots = [];

    private RingRange _currentRange;

    // Process view change (grow, shrink, or stay same)
    private void ProcessMembershipUpdate(DirectoryMembershipSnapshot current)
    {
        var previous = CurrentView;
        CurrentView = current;

        var previousRange = previous.GetRange(_id, _partitionIndex);
        _currentRange = current.GetRange(_id, _partitionIndex);

        // Compute range differences (using RingRange.Difference, not Subtract)
        var removedRange = previousRange.Difference(_currentRange).SingleOrDefault();
        var addedRange = _currentRange.Difference(previousRange).SingleOrDefault();

        if (!removedRange.IsEmpty)
        {
            // Partition shrunk - release range via ReleaseRangeAsync (snapshot + lock)
            _viewChangeTasks.Add(ReleaseRangeAsync(previous, current, removedRange));
        }

        if (!addedRange.IsEmpty)
        {
            // Partition grew - acquire range via AcquireRangeAsync (transfer or recover)
            _viewChangeTasks.Add(AcquireRangeAsync(previous, current, addedRange));
        }

        _viewUpdates.Publish(current);
    }
}
```

#### Interface Implementation (GrainDirectoryPartition.Interface.cs:13-56)

```csharp
// Lookup: waits for range locks, then checks ownership before serving
async ValueTask<DirectoryResult<GrainAddress?>> IGrainDirectoryPartition.LookupAsync(
    MembershipVersion version, GrainId grainId)
{
    // Wait for any range locks (wedges) to be released
    await WaitForRange(grainId, version);

    // Check if we still own this grain after waiting
    if (!IsOwner(CurrentView, grainId))
        return DirectoryResult.RefreshRequired<GrainAddress?>(CurrentView.Version);

    return DirectoryResult.FromResult(LookupCore(grainId), version);
}

// Register: waits for range, checks ownership, uses RegisterCore with CollectionsMarshal
async ValueTask<DirectoryResult<GrainAddress>> IGrainDirectoryPartition.RegisterAsync(
    MembershipVersion version, GrainAddress address, GrainAddress? currentRegistration)
{
    await WaitForRange(address.GrainId, version);
    if (!IsOwner(CurrentView, address.GrainId))
        return DirectoryResult.RefreshRequired<GrainAddress>(CurrentView.Version);

    return DirectoryResult.FromResult(RegisterCore(address, currentRegistration), version);
}

// RegisterCore stamps the address with the current view's MembershipVersion
private GrainAddress RegisterCore(GrainAddress newAddress, GrainAddress? existingAddress)
{
    ref var existing = ref CollectionsMarshal.GetValueRefOrAddDefault(_directory, newAddress.GrainId, out _);
    if (existing is null || existing.Matches(existingAddress) || IsSiloDead(existing))
    {
        // Set MembershipVersion to the view number in which it was registered
        if (newAddress.MembershipVersion != CurrentView.Version)
            newAddress = new() { GrainId = ..., SiloAddress = ..., ActivationId = ...,
                                 MembershipVersion = CurrentView.Version };
        existing = newAddress;
    }
    return existing;
}
```

**Key differences from previous analysis**:
- `DirectoryResult.WithVersionOnly<T>()` is now `DirectoryResult.RefreshRequired<T>()`
- `DirectoryResult.WithResult()` is now `DirectoryResult.FromResult()`
- Range locks are `List<(RingRange, MembershipVersion, TaskCompletionSource)>` tuples, not a dictionary
- `WaitForRange()` is an async method that blocks until range locks are released
- `IsOwner()` replaces direct range-contains checks (delegates to `CurrentView.TryGetOwner`)
- `RegisterCore` uses `CollectionsMarshal.GetValueRefOrAddDefault` for efficient upsert
- Registration stamps `MembershipVersion = CurrentView.Version` on the address

**Concurrency Pattern**: Version-based optimistic concurrency with async range lock waiting

---

## Cache Invalidation Protocol

### Cache Update Mechanism

Callers cache directory lookups locally. When a message is sent to a stale address (activation moved/died), the response includes a cache invalidation header.

**InsideRuntimeClient.cs:210-252** - Sniff incoming messages for cache updates:

```csharp
public void SniffIncomingMessage(Message message)
{
    try
    {
        if (message.CacheInvalidationHeader is { } cacheUpdates)
        {
            lock (cacheUpdates)
            {
                foreach (var update in cacheUpdates)
                {
                    GrainLocator.UpdateCache(update);  // Update or invalidate
                }
            }
        }
    }
    catch (Exception exc)
    {
        LogWarningSniffIncomingMessage(this.logger, exc);
    }
}
```

**MessageCenter.cs:284-287** - Add invalidation header when rejecting to stale address:

```csharp
if (oldAddress != null)
{
    message.AddToCacheInvalidationHeader(oldAddress, validAddress: validAddress);
}
```

Also at **MessageCenter.cs:358-361** when forwarding messages:

```csharp
if (oldAddress != null)
{
    message.AddToCacheInvalidationHeader(oldAddress, validAddress: destination);
}
```

**GrainAddressCacheUpdate** (`GrainAddressCacheUpdate.cs`) contains:
- `GrainId` - Identifier of the grain
- `InvalidGrainAddress` - Address to remove from cache (reconstructed from stored fields)
- `ValidGrainAddress` - New valid address (optional, null if only invalidating)
- `InvalidActivationId`, `InvalidSiloAddress` - Components of the invalid address

---

## View Change Protocol (Virtual Synchrony)

### Normal Operation

```
Client                          Partition Owner
  |                                   |
  |--- Lookup(GrainId, version) ---->|
  |                                   | (version matches)
  |                                   | Check local storage
  |<--- Result + version ------------|
  |                                   |
```

### View Change Detected

```
Client                    Partition A (old owner)     Partition B (new owner)
  |                              |                           |
  |-- Lookup(GrainId, v1) ----->|                           |
  |                              | (has v2, no longer owns)  |
  |<-- VersionMismatch(v2) ------|                           |
  |                              |                           |
  | (refresh to v2)              |                           |
  |                              |                           |
  |-- Lookup(GrainId, v2) ------------------------------>   |
  |                                                          | (has v2, owns range)
  |<----------------------- Result + v2 --------------------|
```

### Range Transfer (Partition Grows - Contiguous View Change)

When the view change is **contiguous** (version increments by exactly 1), snapshot transfer is attempted:

```
Previous Owner (ReleaseRangeAsync)           New Owner (AcquireRangeAsync)
     |                                              |
     | Lock removed range                           | Lock added range
     | Wait for any prior locks on range            |
     | Collect entries in removed range             |
     | Remove entries from local _directory         |
     | Store PartitionSnapshotState                 |
     | Unlock range                                 |
     |                                              |
     |<--- GetSnapshotAsync(version, rangeVer) -----|
     |                                              |
     |--- GrainDirectoryPartitionSnapshot -------->|
     |                                              | Wait for prior locks on range
     |                                              | Apply entries to _directory
     |                                              |
     |<--- AcknowledgeSnapshotTransferAsync --------|
     |                                              |
     | Remove PartitionSnapshotState                | Unlock range
     |                                              | Start serving requests
```

### Recovery (Non-Contiguous View or Failed Transfer)

When the view change is **non-contiguous** (skipped versions) or the snapshot transfer fails (silo unavailable), recovery is performed via `RecoverPartitionRange`:

```
New Owner (AcquireRangeAsync)             All Active Silos
    |                                            |
    | Snapshot transfer failed or non-contiguous |
    | Wait for prior range locks                 |
    |                                            |
    |--- RecoverRegisteredActivations ---------->| (to each active silo)
    |         (range R, version v3)              |
    |                                            | Iterate ActivationDirectory
    |                                            | Filter by range and directory type
    |                                            | Deactivate in-doubt activations
    |<---- List<GrainAddress> -------------------| (from each silo)
    |                                            |
    | Apply all entries to _directory            |
    | Unlock range                               |
    | Start serving requests                     |
```

**In-doubt activations**: Activations with `MembershipVersion == MinValue` (not yet fully registered) or those that are not currently valid are deactivated with a pre-canceled cancellation token to skip directory deregistration. The pre-canceled token is created via `cts.Cancel()` before iteration.

**DistributedGrainDirectory.cs:242-261** (inside `GetRegisteredActivations`):

```csharp
if (address.MembershipVersion == MembershipVersion.MinValue
    || activation is ActivationData activationData && !activationData.IsValid)
{
    try
    {
        // This activation has not completed registration or is not currently active.
        // Abort the activation with a pre-canceled cancellation token so that it skips directory deregistration.
        activation.Deactivate(
            new DeactivationReason(DeactivationReasonCode.DirectoryFailure,
                "This activation's directory partition was salvaged while registration status was in-doubt."),
            cts.Token);  // pre-canceled CancellationToken
        deactivationTasks.Add(activation.Deactivated);
    }
    catch (Exception exception)
    {
        LogWarningFailedToDeactivateActivation(_logger, exception, activation);
    }
}
```

---

## Consistent Ring Partitioning

### Range Assignment

Each silo gets **30 ranges** (virtual nodes) to improve load distribution. The constant is defined in `ConsistentRingOptions.DEFAULT_NUM_VIRTUAL_RING_BUCKETS = 30`.

**Calculation** (`DirectoryMembershipSnapshot.cs:19-127`):

```csharp
// PartitionsPerSilo is sourced from ConsistentRingOptions
internal const int PartitionsPerSilo = ConsistentRingOptions.DEFAULT_NUM_VIRTUAL_RING_BUCKETS;  // 30

// For each active silo, generate PartitionsPerSilo hash codes from SiloAddress
var hashCodes = getRingBoundaries(activeMember, PartitionsPerSilo);  // SiloAddress.GetUniformHashCodes(count)
hashCodes.Sort();

// Sort all (hash, memberIndex, partitionIndex) tuples to form the ring
hashIndexPairs.Sort(static (left, right) => left.Hash.CompareTo(right.Hash) ...);

// Each partition's range spans from its hash to the next hash in the sorted ring
var range = RingRange.Create(entryStart, nextStart);
// Special cases: single partition = RingRange.Full, hash collision = RingRange.Empty
```

Range sizes are **not uniform** -- they depend on the hash distribution of silo addresses. Each partition's range extends from its hash point to the next hash point on the ring.

### Finding Owner

**DirectoryMembershipSnapshot.cs:251-273**:

```csharp
public bool TryGetOwner(GrainId grainId, [NotNullWhen(true)] out SiloAddress? owner,
    [NotNullWhen(true)] out IGrainDirectoryPartition? partitionReference)
    => TryGetOwner(grainId.GetUniformHashCode(), out owner, out partitionReference);

public bool TryGetOwner(uint hashCode, [NotNullWhen(true)] out SiloAddress? owner,
    [NotNullWhen(true)] out IGrainDirectoryPartition? partitionReference)
{
    // Binary search on the ring boundaries
    var index = SearchAlgorithms.RingRangeBinarySearch(
        _ringBoundaries.Length,
        this,
        static (collection, index) => collection.GetRangeCore(index),
        hashCode);

    if (index >= 0)
    {
        var (_, memberIndex, partitionIndex) = _ringBoundaries[index];
        owner = Members[memberIndex];
        partitionReference = _partitionsByMember[memberIndex][partitionIndex];
        return true;
    }

    // No members in cluster
    owner = null;
    partitionReference = null;
    return false;
}
```

Additional range query methods on `DirectoryMembershipSnapshot`:
- `GetRange(SiloAddress, int partitionIndex)` -- returns `RingRange` for a specific partition
- `GetMemberRanges(SiloAddress)` -- returns `RingRangeCollection` (all ranges merged) for a silo
- `GetMemberRangesByPartition(SiloAddress)` -- returns `ImmutableArray<RingRange>` per partition index
- `RangeOwners` -- returns a `RangeCollection` enumerating `(RingRange, MemberIndex, PartitionIndex)` for all ring entries

---

## Integration with Activation Lifecycle

### Registration During Activation

**ActivationData.cs** (from activation-lifecycle.md):

```csharp
// 1. Activation created
activation = new ActivationData(...);
activation.Address.MembershipVersion = MembershipVersion.MinValue;  // Not yet registered

// 2. Start activation
await activation.OnActivateAsync();

// 3. Register in directory
var registeredAddress = await GrainLocator.Register(activation.Address, previousAddress: null);

// 4. Mark as registered
activation.Address.MembershipVersion = currentClusterVersion;
```

### Unregistration During Deactivation

```csharp
// 1. Begin deactivation
activation.SetState(ActivationState.Deactivating);

// 2. Run OnDeactivateAsync
await activation.OnDeactivateAsync(reason, cancellationToken);

// 3. Unregister from directory (unless cancellation token is pre-canceled)
if (!cancellationToken.IsCancellationRequested)
    await GrainLocator.Unregister(activation.Address, UnregistrationCause.Force);

// 4. Mark as invalid
activation.SetState(ActivationState.Invalid);
```

---

## Patterns for Moonpool

### Early Implementation: Static Directory

For Phase 12 early iterations, implement a simple **static directory** where all actors are assumed to be local:

```rust
/// Static directory - no distributed lookups, all actors are local
pub struct StaticDirectory {
    local_activations: HashMap<ActorId, ActorAddress>,
}

impl StaticDirectory {
    pub async fn lookup(&self, actor_id: ActorId) -> Option<ActorAddress> {
        self.local_activations.get(&actor_id).cloned()
    }

    pub async fn register(&mut self, address: ActorAddress) -> ActorAddress {
        self.local_activations.insert(address.actor_id, address.clone());
        address
    }

    pub async fn unregister(&mut self, address: ActorAddress) {
        self.local_activations.remove(&address.actor_id);
    }
}
```

**Why static first?**
- Simplifies Phase 12 initial implementation
- No network coordination needed
- Deterministic for simulation testing
- Focus on actor lifecycle, message routing, and request-response patterns

### Future: Distributed Directory

When ready to implement distributed directory:

1. **Consistent Ring Traits**:
```rust
pub trait ConsistentRing {
    fn get_owner(&self, actor_id: &ActorId) -> NodeId;
    fn get_ranges(&self, node_id: &NodeId) -> Vec<RingRange>;
}
```

2. **Directory Partition**:
```rust
pub struct DirectoryPartition {
    partition_index: u32,
    entries: HashMap<ActorId, ActorAddress>,
    current_view: MembershipVersion,
    current_ranges: RingRangeCollection,
}

impl DirectoryPartition {
    pub async fn lookup(&self, version: MembershipVersion, actor_id: ActorId)
        -> DirectoryResult<Option<ActorAddress>>
    {
        if version > self.current_view {
            return DirectoryResult::version_only(version);
        }

        if !self.current_ranges.contains(&actor_id) {
            return DirectoryResult::version_only(self.current_view);
        }

        let address = self.entries.get(&actor_id).cloned();
        DirectoryResult::with_result(address, self.current_view)
    }
}
```

3. **View Change Protocol** (similar to Virtual Synchrony):
   - Normal operation: process requests locally
   - View change: coordinate snapshot transfers with range locks
   - Recovery: collect registrations from all nodes

4. **Cache Invalidation**:
```rust
pub struct MessageHeader {
    pub cache_invalidation: Option<Vec<CacheUpdate>>,
}

pub struct CacheUpdate {
    pub invalid_address: ActorAddress,
    pub valid_address: Option<ActorAddress>,
}
```

---

## Key Takeaways for Moonpool

### 1. Location Transparency Architecture

```rust
// Facade pattern
pub struct ActorLocator {
    resolver: ActorDirectoryResolver,
}

// Per-actor-type strategy
pub struct ActorDirectoryResolver {
    directories: HashMap<String, Box<dyn ActorDirectory>>,
    default_directory: Box<dyn ActorDirectory>,
}

// Directory interface
#[async_trait(?Send)]
pub trait ActorDirectory {
    async fn register(&self, address: ActorAddress) -> ActorAddress;
    async fn lookup(&self, actor_id: ActorId) -> Option<ActorAddress>;
    async fn unregister(&self, address: ActorAddress);
}
```

### 2. Optimistic Caching

```rust
pub struct CachedDirectory {
    remote: Box<dyn ActorDirectory>,
    cache: LruCache<ActorId, ActorAddress>,
}

impl CachedDirectory {
    pub async fn lookup(&mut self, actor_id: ActorId) -> Option<ActorAddress> {
        // Try cache first
        if let Some(address) = self.cache.get(&actor_id) {
            return Some(address.clone());
        }

        // Query remote
        let address = self.remote.lookup(actor_id).await?;
        self.cache.put(actor_id, address.clone());
        Some(address)
    }

    pub fn invalidate(&mut self, actor_id: ActorId) {
        self.cache.pop(&actor_id);
    }
}
```

### 3. Version-Based Concurrency

```rust
pub struct VersionedResult<T> {
    pub value: T,
    pub version: MembershipVersion,
}

pub enum DirectoryResult<T> {
    Success { value: T, version: MembershipVersion },
    VersionMismatch { expected: MembershipVersion },
}

impl<T> DirectoryResult<T> {
    pub fn try_get_result(self, expected: MembershipVersion) -> Option<T> {
        match self {
            Self::Success { value, version } if version == expected => Some(value),
            _ => None,
        }
    }
}
```

---

## References

### Source Files

**Core Implementation**:
- `GrainLocator.cs:1-51` - Facade for directory operations
- `GrainLocatorResolver.cs:1-53` - Maps grain types to IGrainLocator (DhtGrainLocator, CachedGrainLocator, ClientGrainLocator)
- `GrainDirectoryResolver.cs:12-88` - Per-grain-type directory selection (IGrainDirectory resolution)
- `DhtGrainLocator.cs:1-153` - IGrainLocator for default directory, wraps LocalGrainDirectory, batched deregistration
- `DistributedGrainDirectory.cs:60-469` - Consistent ring DHT with view changes
- `GrainDirectoryPartition.cs:1-921` - Range ownership, view change protocol, snapshot transfers, recovery
- `GrainDirectoryPartition.Interface.cs:1-122` - Register/Lookup/Deregister with WaitForRange pattern
- `DirectoryMembershipSnapshot.cs:1-307` - Ring construction, range computation, TryGetOwner binary search
- `DirectoryResult.cs:1-33` - Result type with version checking (FromResult, RefreshRequired)
- `IGrainDirectoryPartition.cs:1-43` - Partition RPC interface (RegisterAsync, LookupAsync, DeregisterAsync, GetSnapshotAsync, AcknowledgeSnapshotTransferAsync)
- `IGrainDirectory.cs:1-73` - Directory interface contract (in Orleans.Core.Abstractions)

**Integration Points**:
- `InsideRuntimeClient.cs:210-252` - Cache invalidation via SniffIncomingMessage (with lock on cacheUpdates)
- `MessageCenter.cs:284-287` - Cache update headers in rejections
- `MessageCenter.cs:358-361` - Cache update headers when forwarding
- `GrainAddressCacheUpdate.cs:1-108` - Cache update directive (InvalidGrainAddress + optional ValidGrainAddress)
- `ActivationData.cs` (see activation-lifecycle.md) - Registration during activation
- `PlacementService.cs` - Creates activations, triggers directory registration

**Membership Integration**:
- `DirectoryMembershipService.cs` - Provides view updates for directory
- `ClusterMembershipSnapshot.cs` - Immutable cluster state (see membership-service.md)
- `ConsistentRingOptions.cs:18` - `DEFAULT_NUM_VIRTUAL_RING_BUCKETS = 30`

### Related Analyses

- **activation-lifecycle.md** - How activations register/unregister with directory
- **message-system.md** - How messages include cache invalidation headers
- **message-routing.md** - How InsideRuntimeClient uses directory for routing
- **membership-service.md** - Cluster membership drives directory view changes

### Algorithm References

- **Virtual Synchrony**: https://www.microsoft.com/en-us/research/publication/virtually-synchronous-methodology-for-dynamic-service-replication/
- **Vertical Paxos**: https://www.microsoft.com/en-us/research/publication/vertical-paxos-and-primary-backup-replication/
- **Amazon Dynamo**: https://www.allthingsdistributed.com/files/amazon-dynamo-sosp2007.pdf
- **Apache Cassandra virtual nodes**: https://docs.datastax.com/en/cassandra-oss/3.0/cassandra/architecture/archDataDistributeVnodesUsing.html

---

## Summary

Orleans' grain directory is a **partitioned distributed hash table** using:

1. **Consistent Ring** - 30 virtual nodes per silo for load distribution
2. **Virtual Synchrony** - Two-phase view changes with range locks (wedges)
3. **Optimistic Caching** - Local caches with proactive invalidation
4. **Version-Based Retry** - Automatic view refresh and request retry
5. **Snapshot Transfer** - Partition hand-off during membership changes
6. **Recovery Protocol** - Reconstruct directory from all active silos after failures

For Moonpool Phase 12, start with **StaticDirectory** (all actors local) to focus on actor lifecycle and message routing. Add distributed directory features incrementally as the system matures.
