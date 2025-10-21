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
┌────────────────────────────────────────────┐
│ GrainLocator (Facade)                      │  ← Simple API for callers
├────────────────────────────────────────────┤
│ GrainDirectoryResolver                     │  ← Per-grain-type directory selection
├────────────────────────────────────────────┤
│ DistributedGrainDirectory (SystemTarget)   │  ← Consistent ring coordination
├────────────────────────────────────────────┤
│ GrainDirectoryPartition (30 per silo)      │  ← Range ownership with view changes
├────────────────────────────────────────────┤
│ Local Storage + Cache                      │  ← In-memory hash table + LRU cache
└────────────────────────────────────────────┘
```

---

## Core Components

### 1. GrainLocator (Facade)

**File**: `GrainLocator.cs:1-50`

Simple facade that delegates to the appropriate `IGrainLocator` implementation based on grain type.

```csharp
internal class GrainLocator
{
    private readonly GrainLocatorResolver _grainLocatorResolver;

    // Lookup grain location (may query remote directory partition)
    public ValueTask<GrainAddress?> Lookup(GrainId grainId)
        => GetGrainLocator(grainId.Type).Lookup(grainId);

    // Register grain activation location
    public Task<GrainAddress> Register(GrainAddress address, GrainAddress? previousRegistration)
        => GetGrainLocator(address.GrainId.Type).Register(address, previousRegistration);

    // Unregister grain activation
    public Task Unregister(GrainAddress address, UnregistrationCause cause)
        => GetGrainLocator(address.GrainId.Type).Unregister(address, cause);

    // Check local cache without remote call
    public bool TryLookupInCache(GrainId grainId, [NotNullWhen(true)] out GrainAddress? address)
        => GetGrainLocator(grainId.Type).TryLookupInCache(grainId, out address);

    // Invalidate cached entry
    public void InvalidateCache(GrainId grainId)
        => GetGrainLocator(grainId.Type).InvalidateCache(grainId);

    // Update cache with valid address (from message header)
    public void UpdateCache(GrainId grainId, SiloAddress siloAddress)
        => GetGrainLocator(grainId.Type).UpdateCache(grainId, siloAddress);

    private IGrainLocator GetGrainLocator(GrainType grainType)
        => _grainLocatorResolver.GetGrainLocator(grainType);
}
```

**Pattern**: **Facade** - Simplifies directory access by hiding resolver complexity

---

### 2. GrainDirectoryResolver (Strategy Selection)

**File**: `GrainDirectoryResolver.cs:12-88`

Selects which directory implementation to use for each grain type based on grain properties.

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
        => directoryPerType.GetOrAdd(grainType, GetGrainDirectoryPerType);

    private IGrainDirectory GetGrainDirectoryPerType(GrainType grainType)
    {
        if (TryGetNonDefaultGrainDirectory(grainType, out var result))
            return result;

        return DefaultGrainDirectory;  // DistributedGrainDirectory
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
        if (properties?.Properties.TryGetValue(WellKnownGrainTypeProperties.GrainDirectory, out var directoryName)
            && !string.IsNullOrWhiteSpace(directoryName))
        {
            if (directoryPerName.TryGetValue(directoryName, out directory))
                return true;
            else
                throw new KeyNotFoundException($"Could not resolve grain directory {directoryName}");
        }

        directory = null;
        return false;
    }
}
```

**Key Decisions**:
- Stateless grains: No directory (activations are ephemeral)
- GrainServices: Consistent ring-based directory
- Most grains: `DistributedGrainDirectory` (default)
- Custom attribute: `[GrainDirectory("CustomName")]`

---

### 3. DistributedGrainDirectory (Consistent Ring DHT)

**File**: `DistributedGrainDirectory.cs:60-468`

SystemTarget that implements a distributed hash table using a consistent ring with 30 partitions per silo.

#### Architecture Summary (from file header comments):

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

  When partition GROWS (silo joined):
    - Previous owner seals lost range
    - Creates snapshot of directory entries in that range
    - New owner requests snapshot and applies it
    - New owner acknowledges transfer

  When partition SHRINKS (silo left):
    - Perform recovery: ask all silos for their registrations in the range
    - Deactivate in-doubt activations (not fully registered)

Concurrency Control:
  - Versioned range locks (wedges) prevent access during view changes
  - All requests include caller's view number
  - All responses include partition's view number
  - View number mismatches trigger refresh and retry
```

#### Core Implementation

```csharp
internal sealed partial class DistributedGrainDirectory
    : SystemTarget, IGrainDirectory, IGrainDirectoryClient
{
    private readonly DirectoryMembershipService _membershipService;
    private readonly ImmutableArray<GrainDirectoryPartition> _partitions;  // 30 partitions
    private long _recoveryMembershipVersion;
    private ActivationDirectory _localActivations;

    // IGrainDirectory implementation - route to appropriate partition
    public async Task<GrainAddress?> Lookup(GrainId grainId)
        => await InvokeAsync(
            grainId,
            static (partition, version, grainId, ct) => partition.LookupAsync(version, grainId),
            grainId,
            CancellationToken.None);

    public async Task<GrainAddress?> Register(GrainAddress address)
        => await InvokeAsync(
            address.GrainId,
            static (partition, version, address, ct) => partition.RegisterAsync(version, address, null),
            address,
            CancellationToken.None);

    public async Task Unregister(GrainAddress address)
        => await InvokeAsync(
            address.GrainId,
            static (partition, version, address, ct) => partition.DeregisterAsync(version, address),
            address,
            CancellationToken.None);

    // Core routing with view-aware retry logic
    private async Task<TResult> InvokeAsync<TState, TResult>(
        GrainId grainId,
        Func<IGrainDirectoryPartition, MembershipVersion, TState, CancellationToken, ValueTask<DirectoryResult<TResult>>> func,
        TState state,
        CancellationToken cancellationToken)
    {
        var view = _membershipService.CurrentView;
        while (true)
        {
            var initialRecoveryVersion = _recoveryMembershipVersion;

            // Find owner for this grain's hash
            if (!view.TryGetOwner(grainId, out var owner, out var partitionReference))
            {
                // No members in cluster - refresh view and retry
                view = await _membershipService.RefreshViewAsync(new(view.Version.Value + 1), cancellationToken);
                continue;
            }

            // Invoke on partition (local or remote SystemTarget call)
            var invokeResult = await func(partitionReference, view.Version, state, cancellationToken);

            // Check if recovery version changed (see comment on _recoveryMembershipVersion)
            if (initialRecoveryVersion != _recoveryMembershipVersion)
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

**File**: `GrainDirectoryPartition.cs` (referenced from DistributedGrainDirectory.cs:94-100)

Each silo has 30 partitions that manage directory ranges. Partitions use versioned range locks during view changes.

```csharp
internal sealed class GrainDirectoryPartition : IGrainDirectoryPartition
{
    private readonly int _partitionIndex;
    private readonly DistributedGrainDirectory _directory;

    // In-memory storage for this partition's ranges
    private readonly Dictionary<GrainId, GrainAddress> _entries;

    // Range locks for view change coordination
    private readonly Dictionary<MembershipVersion, RangeLock> _rangeLocks;

    // Process view change (grow, shrink, or stay same)
    public async Task ProcessMembershipUpdateAsync(DirectoryMembershipSnapshot view)
    {
        var myRanges = view.GetMemberRanges(Silo, _partitionIndex);
        var previousView = _currentView;

        if (previousView is null)
        {
            // First view - no transfer needed
            _currentView = view;
            _currentRanges = myRanges;
            return;
        }

        var previousRanges = _currentRanges;
        var gained = myRanges.Subtract(previousRanges);  // Ranges I now own
        var lost = previousRanges.Subtract(myRanges);    // Ranges I no longer own

        if (!gained.IsEmpty)
        {
            // Partition grew - receive snapshots from previous owners
            await ReceiveRangeSnapshots(gained, view);
        }

        if (!lost.IsEmpty)
        {
            // Partition shrunk - send snapshots to new owners
            await SendRangeSnapshots(lost, view);
        }

        _currentView = view;
        _currentRanges = myRanges;
        ReleaseLocks(previousView.Version);
    }

    // Lookup with version checking
    public ValueTask<DirectoryResult<GrainAddress?>> LookupAsync(MembershipVersion version, GrainId grainId)
    {
        // If caller has newer view, tell them to refresh
        if (version > _currentView.Version)
            return new(DirectoryResult.WithVersionOnly<GrainAddress?>(version));

        // Check if grain is in our current ranges
        if (!_currentRanges.Contains(grainId))
            return new(DirectoryResult.WithVersionOnly<GrainAddress?>(_currentView.Version));

        // Lookup in local storage
        _entries.TryGetValue(grainId, out var address);
        return new(DirectoryResult.WithResult(address, _currentView.Version));
    }

    // Register with version checking and previous address handling
    public ValueTask<DirectoryResult<GrainAddress?>> RegisterAsync(
        MembershipVersion version,
        GrainAddress address,
        GrainAddress? previousAddress)
    {
        if (version > _currentView.Version)
            return new(DirectoryResult.WithVersionOnly<GrainAddress?>(version));

        if (!_currentRanges.Contains(address.GrainId))
            return new(DirectoryResult.WithVersionOnly<GrainAddress?>(_currentView.Version));

        // Remove previous address if provided
        if (previousAddress is not null)
            _entries.TryRemove(previousAddress.GrainId, out _);

        // Try to register (only if not already registered)
        if (_entries.TryAdd(address.GrainId, address))
            return new(DirectoryResult.WithResult(address, _currentView.Version));
        else
            // Already registered - return existing
            return new(DirectoryResult.WithResult(_entries[address.GrainId], _currentView.Version));
    }
}
```

**Concurrency Pattern**: Version-based optimistic concurrency control

---

## Cache Invalidation Protocol

### Cache Update Mechanism

Callers cache directory lookups locally. When a message is sent to a stale address (activation moved/died), the response includes a cache invalidation header.

**InsideRuntimeClient.cs:209-220** - Sniff incoming messages for cache updates:

```csharp
public void SniffIncomingMessage(Message message)
{
    try
    {
        if (message.CacheInvalidationHeader != null)
        {
            foreach (var update in message.CacheInvalidationHeader)
            {
                GrainLocator.UpdateCache(update);  // Update or invalidate
            }
        }
    }
    catch (Exception exc)
    {
        logger.LogWarning(exc, "SniffIncomingMessage has thrown exception. Ignoring.");
    }
}
```

**MessageCenter.cs:294-297** - Add invalidation header when rejecting to stale address:

```csharp
if (oldAddress != null)
{
    message.AddToCacheInvalidationHeader(oldAddress, validAddress: validAddress);
}
```

**GrainAddressCacheUpdate** contains:
- `InvalidGrainAddress` - Address to remove from cache
- `ValidGrainAddress` - New valid address (optional)

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

### Range Transfer (Partition Grows)

```
Previous Owner                                   New Owner
     |                                              |
     | Seal lost range (create RangeLock)          |
     | Create snapshot of entries in range         |
     |                                              |
     |<-------- RequestSnapshot(range, v2) --------|
     |                                              |
     |---------- Snapshot(entries) --------------->|
     |                                              | Apply entries locally
     |                                              | Start serving requests
     |                                              |
     |<-------- AcknowledgeTransfer(range) --------|
     |                                              |
     | Delete snapshot                              |
     | Release RangeLock                            |
```

### Recovery (Silo Crashed)

```
New Owner                         All Active Silos
    |                                    |
    | Silo X crashed, I now own range R |
    |                                    |
    |--- RecoverRegisteredActivations -->| (broadcast)
    |         (range R, version v3)      |
    |                                    | Collect activations in range R
    |                                    | Deactivate in-doubt activations
    |<---- List<GrainAddress> -----------| (from each silo)
    |                                    |
    | Merge all responses                |
    | Remove duplicates                  |
    | Populate local storage             |
    | Start serving requests             |
```

**In-doubt activations**: Activations with `MembershipVersion == MinValue` (not yet fully registered) are deactivated with a pre-canceled cancellation token to skip directory deregistration.

**DistributedGrainDirectory.cs:242-254**:

```csharp
if (address.MembershipVersion == MembershipVersion.MinValue
    || activation is ActivationData activationData && !activationData.IsValid)
{
    // This activation has not completed registration or is not currently active.
    // Abort the activation with a pre-canceled cancellation token so that it skips directory deregistration.
    activation.Deactivate(
        new DeactivationReason(DeactivationReasonCode.DirectoryFailure,
            "This activation's directory partition was salvaged while registration status was in-doubt."),
        preCanceledToken);
    deactivationTasks.Add(activation.Deactivated);
}
```

---

## Consistent Ring Partitioning

### Range Assignment

Each silo gets **30 ranges** (virtual nodes) to improve load distribution.

**Calculation** (DirectoryMembershipSnapshot):
```csharp
const int PartitionsPerSilo = 30;
var totalPartitions = activeMembers.Length * PartitionsPerSilo;
var rangeSize = uint.MaxValue / totalPartitions;

// For silo i, partition j:
var hash = Hash(siloAddress, partitionIndex);
var rangeStart = hash;
var rangeEnd = rangeStart + rangeSize;
```

### Finding Owner

```csharp
public bool TryGetOwner(GrainId grainId, out SiloAddress owner, out IGrainDirectoryPartition partition)
{
    var hash = grainId.GetUniformHashCode();

    // Binary search in sorted ring
    var index = FindRangeIndex(hash);
    var range = _sortedRanges[index];

    owner = range.Owner;
    partition = GetPartitionReference(owner, range.PartitionIndex);
    return true;
}
```

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
- `GrainLocator.cs:1-50` - Facade for directory operations
- `GrainDirectoryResolver.cs:12-88` - Per-grain-type directory selection
- `DistributedGrainDirectory.cs:60-468` - Consistent ring DHT with view changes
- `GrainDirectoryPartition.cs` - Range ownership and view change protocol
- `IGrainDirectory.cs:1-75` - Directory interface contract

**Integration Points**:
- `InsideRuntimeClient.cs:209-220` - Cache invalidation via SniffIncomingMessage
- `MessageCenter.cs:294-297` - Cache update headers in rejections
- `ActivationData.cs` (see activation-lifecycle.md) - Registration during activation
- `PlacementService.cs` - Creates activations, triggers directory registration

**Membership Integration**:
- `DirectoryMembershipService.cs` - Provides view updates for directory
- `ClusterMembershipSnapshot.cs` - Immutable cluster state (see membership-service.md)

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
