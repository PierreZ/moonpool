# Orleans Placement Strategy Analysis

**Purpose**: Distributed grain placement system for intelligent actor distribution across cluster nodes using pluggable strategies

**Key insight**: Orleans uses a Strategy-Director pattern where placement strategies are resolved per grain type and executed by corresponding directors. The system integrates tightly with the grain directory through PlacementService, which orchestrates message addressing, compatible silo resolution, placement filter application, and multi-level caching with a 16-worker pool architecture.

---

## Overview

The placement strategy system solves the **grain distribution problem**: How do you intelligently place grain activations across cluster silos to optimize for load balancing, locality, resource utilization, or custom requirements?

### Key Responsibilities

1. **Strategy Resolution** - Determine which placement strategy applies to each grain type
2. **Silo Selection** - Choose optimal silo from compatible candidates
3. **Filtering** - Apply metadata-based silo filters (required/preferred)
4. **Load Awareness** - Track silo statistics for intelligent placement decisions
5. **Directory Integration** - Coordinate with grain directory for activation lookup and registration
6. **Caching** - Multi-level caching (message cache, grain locator cache) with invalidation
7. **Worker Pool** - 16 concurrent workers for parallel placement operations

### Architecture Layers

```
┌────────────────────────────────────────────────────────────┐
│ PlacementService (Central Orchestrator)                    │  ← 16-worker pool, message addressing
├────────────────────────────────────────────────────────────┤
│ PlacementStrategyResolver / PlacementDirectorResolver      │  ← DI-based strategy-to-director mapping
├────────────────────────────────────────────────────────────┤
│ PlacementFilterStrategyResolver / FilterDirectorResolver   │  ← Ordered filter chain
├────────────────────────────────────────────────────────────┤
│ IPlacementDirector (9 built-in implementations)            │  ← Random, PreferLocal, Hash, ActivationCount, ResourceOptimized, etc.
├────────────────────────────────────────────────────────────┤
│ IPlacementContext                                          │  ← Compatible silos, local silo info, version management
├────────────────────────────────────────────────────────────┤
│ GrainLocator → LocalGrainDirectory / DhtGrainLocator      │  ← Directory service integration
└────────────────────────────────────────────────────────────┘
```

---

## Core Components

### 1. PlacementService (Central Orchestrator)

**File**: `PlacementService.cs:1-430`

The PlacementService is the heart of the placement system, implementing `IPlacementContext` and orchestrating all placement decisions.

**Core Structure**:

```csharp
internal partial class PlacementService : IPlacementContext
{
    private const int PlacementWorkerCount = 16;

    private readonly PlacementStrategyResolver _strategyResolver;
    private readonly PlacementDirectorResolver _directorResolver;
    private readonly GrainLocator _grainLocator;
    private readonly GrainVersionManifest _grainInterfaceVersions;
    private readonly ISiloStatusOracle _siloStatusOracle;
    private readonly PlacementWorker[] _workers;
    private readonly PlacementFilterStrategyResolver _filterStrategyResolver;
    private readonly PlacementFilterDirectorResolver _placementFilterDirectoryResolver;

    public SiloAddress LocalSilo { get; }
    public SiloStatus LocalSiloStatus => _siloStatusOracle.CurrentStatus;
}
```

**Pattern**: **Facade** + **Worker Pool** - Simplifies placement access while distributing work across concurrent workers

#### Message Addressing (lines 78-96)

The primary entry point for message routing:

```csharp
public Task AddressMessage(Message message)
{
    if (message.IsTargetFullyAddressed) return Task.CompletedTask;
    if (message.TargetGrain.IsDefault) ThrowMissingAddress();

    var grainId = message.TargetGrain;

    // Fast path: Check cache first
    if (_grainLocator.TryLookupInCache(grainId, out var result)
        && CachedAddressIsValid(message, result))
    {
        SetMessageTargetPlacement(message, result.SiloAddress);
        return Task.CompletedTask;
    }

    // Slow path: Route to worker based on grain hash
    var worker = _workers[grainId.GetUniformHashCode() % PlacementWorkerCount];
    return worker.AddressMessage(message);
}
```

**Key Features**:
1. **Cache-first lookup** - Avoids directory calls for hot grains
2. **Cache validation** - Checks `CacheInvalidationHeader` for stale entries
3. **Worker distribution** - Consistent hashing distributes grains across 16 workers
4. **Fast path optimization** - Returns synchronously on cache hit

#### Cache Invalidation (lines 179-213)

**Code**: `CachedAddressIsValid()` method

```csharp
private bool CachedAddressIsValid(Message message, GrainAddress cachedAddress)
{
    // Verify that the result from the cache has not been invalidated
    return message.CacheInvalidationHeader switch
    {
        { Count: > 0 } cacheUpdates => CachedAddressIsValidCore(message, cachedAddress, cacheUpdates),
        _ => true
    };

    // Process invalidation updates
    bool CachedAddressIsValidCore(...)
    {
        var resultIsValid = true;
        foreach (var update in cacheUpdates)
        {
            _grainLocator.UpdateCache(update);  // Apply update immediately

            if (cachedAddress.Matches(update.ValidGrainAddress))
                resultIsValid = true;
            else if (cachedAddress.Matches(update.InvalidGrainAddress))
                resultIsValid = false;
        }
        return resultIsValid;
    }
}
```

**Pattern**: **Proactive Invalidation** - Messages carry cache updates to avoid stale lookups

#### Compatible Silo Resolution (lines 106-149)

**Critical for placement decisions**:

```csharp
public SiloAddress[] GetCompatibleSilos(PlacementTarget target)
{
    // Test-only: assume all silos compatible if flag set
    if (_assumeHomogeneousSilosForTesting)
        return AllActiveSilos;

    var grainType = target.GrainIdentity.Type;

    // Get silos that support this grain type and interface version
    var silos = target.InterfaceVersion > 0
        ? _versionSelectorManager.GetSuitableSilos(grainType, target.InterfaceType, target.InterfaceVersion).SuitableSilos
        : _grainInterfaceVersions.GetSupportedSilos(grainType).Result;

    var compatibleSilos = silos.Intersect(AllActiveSilos).ToArray();

    // Apply placement filters
    var filters = _filterStrategyResolver.GetPlacementFilterStrategies(grainType);
    if (filters.Length > 0)
    {
        IEnumerable<SiloAddress> filteredSilos = compatibleSilos;
        var context = new PlacementFilterContext(target.GrainIdentity.Type, target.InterfaceType, target.InterfaceVersion);

        foreach (var placementFilter in filters)
        {
            var director = _placementFilterDirectoryResolver.GetFilterDirector(placementFilter);
            filteredSilos = director.Filter(placementFilter, context, filteredSilos);
        }

        compatibleSilos = filteredSilos.ToArray();
    }

    // Throw if no compatible silos found
    if (compatibleSilos.Length == 0)
        throw new OrleansException($"No active nodes are compatible with grain {grainType}...");

    return compatibleSilos;
}
```

**Key Features**:
1. **Version awareness** - Only silos supporting the grain interface version
2. **Active silo filtering** - Exclude dead/terminating silos
3. **Ordered filter chain** - Apply all placement filters sequentially
4. **Detailed error messages** - Report both grain type and interface version mismatches

#### Placement Worker (lines 229-403)

**16 concurrent workers for parallel placement**:

```csharp
private class PlacementWorker
{
    private readonly Dictionary<GrainId, GrainPlacementWorkItem> _inProgress = new();
    private readonly SingleWaiterAutoResetEvent _workSignal = new();
    private readonly PlacementService _placementService;
    private List<(Message Message, TaskCompletionSource Completion)> _messages = new();

    // Entry point: enqueue message for placement
    public Task AddressMessage(Message message)
    {
        var completion = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);

        lock (_lockObj)
        {
            _messages ??= new();
            _messages.Add((message, completion));
        }

        _workSignal.Signal();
        return completion.Task;
    }

    // Background processing loop
    private async Task ProcessLoop()
    {
        while (true)
        {
            // Start new requests
            var messages = GetMessages();
            if (messages is not null)
            {
                foreach (var message in messages)
                {
                    var target = message.Message.TargetGrain;
                    var workItem = GetOrAddWorkItem(target);
                    workItem.Messages.Add(message);

                    if (workItem.Result is null)
                    {
                        // First message for this grain - start placement
                        workItem.Result = GetOrPlaceActivationAsync(message.Message);
                        workItem.Result.GetAwaiter().UnsafeOnCompleted(_workSignal.Signal);
                    }
                }
            }

            // Complete finished requests
            foreach (var pair in _inProgress)
            {
                if (pair.Value.Result.IsCompleted)
                {
                    AddressWaitingMessages(pair.Value);
                    _inProgress.Remove(pair.Key);
                }
            }

            await _workSignal.WaitAsync();
        }
    }

    // Placement logic: lookup → place → cache
    private async Task<SiloAddress> GetOrPlaceActivationAsync(Message firstMessage)
    {
        await Task.Yield();
        var target = new PlacementTarget(
            firstMessage.TargetGrain,
            firstMessage.RequestContextData,
            firstMessage.InterfaceType,
            firstMessage.InterfaceVersion);

        // Try directory lookup first
        var result = await _placementService._grainLocator.Lookup(target.GrainIdentity);
        if (result is not null)
            return result.SiloAddress;

        // No activation exists - run placement
        var strategy = _placementService._strategyResolver.GetPlacementStrategy(target.GrainIdentity.Type);
        var director = _placementService._directorResolver.GetPlacementDirector(strategy);
        var siloAddress = await director.OnAddActivation(strategy, target, _placementService);

        // Double-check cache (race with another worker)
        if (_placementService._grainLocator.TryLookupInCache(target.GrainIdentity, out result)
            && _placementService.CachedAddressIsValid(firstMessage, result))
        {
            return result.SiloAddress;
        }

        // Update cache and return
        _placementService._grainLocator.InvalidateCache(target.GrainIdentity);
        _placementService._grainLocator.UpdateCache(target.GrainIdentity, siloAddress);
        return siloAddress;
    }
}
```

**Pattern**: **Coalescing Worker Pool** - Multiple messages for same grain share single placement operation

**Key Features**:
1. **Grain-level coalescing** - Multiple concurrent messages for same grain wait on single placement
2. **Async processing** - Workers run independent async loops
3. **Race handling** - Double-check cache after placement to handle concurrent placements
4. **Completion tracking** - Use `UnsafeOnCompleted` for efficient wakeup

---

### 2. Placement Strategy Interfaces

#### IPlacementDirector (Core Interface)

**File**: `IPlacementDirector.cs:7-46`

```csharp
public interface IPlacementDirector
{
    public const string PlacementHintKey = nameof(PlacementHintKey);

    // Main placement decision method
    Task<SiloAddress> OnAddActivation(
        PlacementStrategy strategy,
        PlacementTarget target,
        IPlacementContext context);

    // Extract placement hint from request context
    public static SiloAddress GetPlacementHint(
        Dictionary<string, object> requestContextData,
        SiloAddress[] compatibleSilos)
    {
        if (requestContextData is { Count: > 0 } data
            && data.TryGetValue(PlacementHintKey, out var value)
            && value is SiloAddress placementHint
            && compatibleSilos.Contains(placementHint))
        {
            return placementHint;
        }
        return null;
    }
}
```

**Pattern**: **Strategy Pattern** - Interface for pluggable placement algorithms

**Placement Hint**: Optional mechanism for callers to suggest target silo (must be in compatible set)

#### IPlacementContext (Context Provider)

**File**: `IPlacementContext.cs:5-38`

```csharp
public interface IPlacementContext
{
    // Get compatible silos for this grain
    SiloAddress[] GetCompatibleSilos(PlacementTarget target);

    // Get compatible silos with version information
    IReadOnlyDictionary<ushort, SiloAddress[]> GetCompatibleSilosWithVersions(PlacementTarget target);

    // Local silo information
    SiloAddress LocalSilo { get; }
    SiloStatus LocalSiloStatus { get; }
}
```

**Purpose**: Provides environment information to placement directors

#### PlacementTarget (Placement Input)

**File**: `PlacementTarget.cs:5-45`

```csharp
public readonly struct PlacementTarget
{
    public GrainId GrainIdentity { get; }
    public GrainInterfaceType InterfaceType { get; }
    public ushort InterfaceVersion { get; }
    public Dictionary<string, object> RequestContextData { get; }
}
```

**Purpose**: Describes the grain being placed and request context

---

### 3. Placement Strategy Implementations

#### 3.1 RandomPlacementDirector (Simple Random)

**File**: `RandomPlacementDirector.cs:11-27`

```csharp
internal class RandomPlacementDirector : IPlacementDirector
{
    public virtual Task<SiloAddress> OnAddActivation(
        PlacementStrategy strategy,
        PlacementTarget target,
        IPlacementContext context)
    {
        var compatibleSilos = context.GetCompatibleSilos(target);

        // Check for placement hint first
        if (IPlacementDirector.GetPlacementHint(target.RequestContextData, compatibleSilos) is { } placementHint)
            return Task.FromResult(placementHint);

        // Random selection
        var silo = compatibleSilos[Random.Shared.Next(compatibleSilos.Length)];
        return Task.FromResult(silo);
    }
}
```

**Use Case**: Default strategy when no special requirements, good load distribution

---

#### 3.2 PreferLocalPlacementDirector (Locality Optimization)

**File**: `PreferLocalPlacementDirector.cs:10-35`

```csharp
internal class PreferLocalPlacementDirector : RandomPlacementDirector
{
    private readonly ILocalSiloDetails _localSiloDetails;

    public override Task<SiloAddress> OnAddActivation(
        PlacementStrategy strategy,
        PlacementTarget target,
        IPlacementContext context)
    {
        var compatibleSilos = context.GetCompatibleSilos(target);

        // Check for placement hint first
        if (IPlacementDirector.GetPlacementHint(target.RequestContextData, compatibleSilos) is { } placementHint)
            return Task.FromResult(placementHint);

        // Prefer local silo if compatible
        var localSilo = _localSiloDetails.SiloAddress;
        if (context.LocalSiloStatus == SiloStatus.Active && compatibleSilos.Contains(localSilo))
            return Task.FromResult(localSilo);

        // Fall back to random
        return base.OnAddActivation(strategy, target, context);
    }
}
```

**Use Case**: Minimize network hops, co-locate related grains, reduce latency

---

#### 3.3 HashBasedPlacementDirector (Deterministic Placement)

**File**: `HashBasedPlacementDirector.cs:10-29`

```csharp
internal class HashBasedPlacementDirector : IPlacementDirector
{
    public Task<SiloAddress> OnAddActivation(
        PlacementStrategy strategy,
        PlacementTarget target,
        IPlacementContext context)
    {
        var compatibleSilos = context.GetCompatibleSilos(target);

        // Check for placement hint first
        if (IPlacementDirector.GetPlacementHint(target.RequestContextData, compatibleSilos) is { } placementHint)
            return Task.FromResult(placementHint);

        // Consistent hash based on grain ID
        var hash = (int)(target.GrainIdentity.GetUniformHashCode() & 0x7FFFFFFF);
        var index = hash % compatibleSilos.Length;
        return Task.FromResult(compatibleSilos[index]);
    }
}
```

**Use Case**: Predictable placement, grain affinity, co-location of related grains (same hash prefix)

---

#### 3.4 ActivationCountPlacementDirector (Power of Two Choices)

**File**: `ActivationCountPlacementDirector.cs:13-127`

**Most sophisticated load-balancing algorithm**:

```csharp
internal class ActivationCountPlacementDirector : RandomPlacementDirector,
    ISiloStatisticsChangeListener, IPlacementDirector
{
    private class CachedLocalStat
    {
        internal SiloRuntimeStatistics SiloStats { get; }
        public int ActivationCount { get; private set; }
        public void IncrementActivationCount(int delta) => Interlocked.Add(ref _activationCount, delta);
    }

    private readonly ConcurrentDictionary<SiloAddress, CachedLocalStat> _localCache = new();
    private readonly int _chooseHowMany;  // Default: 2 (power of two choices)

    private SiloAddress SelectSiloPowerOfK(SiloAddress[] compatibleSilos)
    {
        var compatibleSet = compatibleSilos.ToSet();

        // Filter to relevant silos (active, compatible, not overloaded)
        var relevantSilos = new List<KeyValuePair<SiloAddress, CachedLocalStat>>();
        var totalSilos = 0;
        foreach (var kv in _localCache)
        {
            totalSilos++;
            if (kv.Value.SiloStats.IsOverloaded) continue;
            if (!compatibleSet.Contains(kv.Key)) continue;
            relevantSilos.Add(kv);
        }

        if (relevantSilos.Count > 0)
        {
            // Select K random candidates
            int chooseFrom = Math.Min(relevantSilos.Count, _chooseHowMany);
            var candidates = new List<KeyValuePair<SiloAddress, CachedLocalStat>>();
            while (candidates.Count < chooseFrom)
            {
                int index = Random.Shared.Next(relevantSilos.Count);
                candidates.Add(relevantSilos[index]);
                relevantSilos.RemoveAt(index);
            }

            // Pick candidate with minimum load
            var minLoadedSilo = candidates.MinBy(s =>
                s.Value.ActivationCount + s.Value.SiloStats.RecentlyUsedActivationCount);

            // Increment by total silos (heuristic for concurrent placements)
            minLoadedSilo.Value.IncrementActivationCount(totalSilos);

            return minLoadedSilo.Key;
        }

        throw new SiloUnavailableException("No compatible, non-overloaded silos available");
    }

    // Subscribe to silo statistics updates
    public void SiloStatisticsChangeNotification(SiloAddress updatedSilo, SiloRuntimeStatistics newSiloStats)
    {
        _localCache[updatedSilo] = new CachedLocalStat(newSiloStats);
    }

    public void RemoveSilo(SiloAddress removedSilo)
    {
        _localCache.TryRemove(removedSilo, out _);
    }
}
```

**Algorithm**: **Power of Two Choices**
1. Randomly select K candidates (default K=2)
2. Pick candidate with minimum activation count
3. Increment by total silo count (account for concurrent placements)

**Key Features**:
- Provably better than pure random (exponential improvement with K=2)
- Low overhead (no global coordination)
- Adaptive to load changes via silo statistics
- Excludes overloaded silos

**Use Case**: General-purpose load balancing, better than random with minimal overhead

---

#### 3.5 ResourceOptimizedPlacementDirector (Advanced Scoring)

**File**: `ResourceOptimizedPlacementDirector.cs:14-235`

**Most sophisticated placement algorithm in Orleans**:

```csharp
internal sealed class ResourceOptimizedPlacementDirector : IPlacementDirector, ISiloStatisticsChangeListener
{
    private const int FourKiloByte = 4096;

    private readonly SiloAddress _localSilo;
    private readonly NormalizedWeights _weights;
    private readonly float _localSiloPreferenceMargin;
    private readonly ConcurrentDictionary<SiloAddress, ResourceStatistics> _siloStatistics = [];

    public Task<SiloAddress> OnAddActivation(PlacementStrategy strategy, PlacementTarget target, IPlacementContext context)
    {
        var compatibleSilos = context.GetCompatibleSilos(target);

        // Check for placement hint first
        if (IPlacementDirector.GetPlacementHint(target.RequestContextData, compatibleSilos) is { } placementHint)
            return Task.FromResult(placementHint);

        // Handle edge cases
        if (compatibleSilos.Length == 0)
            throw new SiloUnavailableException($"Cannot place grain '{target.GrainIdentity}'...");

        if (compatibleSilos.Length == 1)
            return Task.FromResult(compatibleSilos[0]);

        if (_siloStatistics.IsEmpty)
            return Task.FromResult(compatibleSilos[Random.Shared.Next(compatibleSilos.Length)]);

        // Stack allocation optimization for large clusters (up to ~170 silos)
        (int Index, float Score, float? LocalSiloScore) pick;
        int compatibleSilosCount = compatibleSilos.Length;
        if (compatibleSilosCount * Unsafe.SizeOf<(int, ResourceStatistics)>() <= FourKiloByte)
        {
            pick = MakePick(stackalloc (int, ResourceStatistics)[compatibleSilosCount]);
        }
        else
        {
            var relevantSilos = ArrayPool<(int, ResourceStatistics)>.Shared.Rent(compatibleSilosCount);
            pick = MakePick(relevantSilos.AsSpan());
            ArrayPool<(int, ResourceStatistics)>.Shared.Return(relevantSilos);
        }

        // Prefer local silo if score is close enough
        var localSiloScore = pick.LocalSiloScore;
        if (!localSiloScore.HasValue
            || context.LocalSiloStatus != SiloStatus.Active
            || localSiloScore.Value - _localSiloPreferenceMargin > pick.Score)
        {
            return Task.FromResult(compatibleSilos[pick.Index]);
        }

        return Task.FromResult(_localSilo);

        // Core selection algorithm
        (int PickIndex, float PickScore, float? LocalSiloScore) MakePick(scoped Span<(int, ResourceStatistics)> relevantSilos)
        {
            // Build list of non-overloaded silos
            int relevantSilosCount = 0;
            float maxMaxAvailableMemory = 0;
            int maxActivationCount = 0;
            ResourceStatistics? localSiloStatistics = null;

            for (var i = 0; i < compatibleSilos.Length; ++i)
            {
                var silo = compatibleSilos[i];
                if (_siloStatistics.TryGetValue(silo, out var stats))
                {
                    if (!stats.IsOverloaded)
                        relevantSilos[relevantSilosCount++] = new(i, stats);

                    maxMaxAvailableMemory = Math.Max(maxMaxAvailableMemory, stats.MaxAvailableMemory);
                    maxActivationCount = Math.Max(maxActivationCount, stats.ActivationCount);

                    if (silo.Equals(_localSilo))
                        localSiloStatistics = stats;
                }
            }

            relevantSilos = relevantSilos[0..relevantSilosCount];

            // Select √N candidates (e.g., 4 from 16 silos)
            int candidateCount = (int)Math.Ceiling(Math.Sqrt(relevantSilosCount));
            ShufflePrefix(relevantSilos, candidateCount);
            var candidates = relevantSilos[0..candidateCount];

            // Find candidate with minimum score
            (int Index, float Score) pick = (0, 1f);
            foreach (var (index, statistics) in candidates)
            {
                float score = CalculateScore(in statistics, maxMaxAvailableMemory, maxActivationCount);
                float scoreJitter = Random.Shared.NextSingle() / 100_000f;  // Tie-breaking

                if (score + scoreJitter < pick.Score)
                    pick = (index, score);
            }

            // Calculate local silo score for preference check
            float? localSiloScore = null;
            if (localSiloStatistics.HasValue && !localSiloStatistics.Value.IsOverloaded)
            {
                localSiloScore = CalculateScore(in localSiloStatistics.Value, maxMaxAvailableMemory, maxActivationCount);
            }

            return (pick.Index, pick.Score, localSiloScore);
        }
    }

    // Multi-dimensional scoring function
    [MethodImpl(MethodImplOptions.AggressiveInlining)]
    private float CalculateScore(ref readonly ResourceStatistics stats, float maxMaxAvailableMemory, int maxActivationCount)
    {
        float normalizedCpuUsage = stats.CpuUsage / 100f;
        float score = _weights.CpuUsageWeight * normalizedCpuUsage;

        if (stats.MaxAvailableMemory > 0)
        {
            float maxAvailableMemory = stats.MaxAvailableMemory;
            float normalizedMemoryUsage = stats.MemoryUsage / maxAvailableMemory;
            float normalizedAvailableMemory = 1 - stats.AvailableMemory / maxAvailableMemory;
            float normalizedMaxAvailableMemory = maxAvailableMemory / maxMaxAvailableMemory;

            score += _weights.MemoryUsageWeight * normalizedMemoryUsage +
                     _weights.AvailableMemoryWeight * normalizedAvailableMemory +
                     _weights.MaxAvailableMemoryWeight * normalizedMaxAvailableMemory;
        }

        score += _weights.ActivationCountWeight * stats.ActivationCount / maxActivationCount;

        return score;  // Range: [0, 1]
    }

    // Fisher-Yates partial shuffle
    static void ShufflePrefix(Span<(int, ResourceStatistics)> values, int prefixLength)
    {
        for (var i = 0; i < prefixLength; i++)
        {
            var chosen = Random.Shared.Next(i, values.Length);
            if (chosen != i)
                (values[chosen], values[i]) = (values[i], values[chosen]);
        }
    }

    // Subscribe to silo statistics
    public void SiloStatisticsChangeNotification(SiloAddress address, SiloRuntimeStatistics statistics)
    {
        _siloStatistics.AddOrUpdate(
            key: address,
            addValueFactory: static (_, stats) => ResourceStatistics.FromRuntime(stats),
            updateValueFactory: static (_, _, stats) => ResourceStatistics.FromRuntime(stats),
            factoryArgument: statistics);
    }

    private record struct ResourceStatistics(
        bool IsOverloaded,
        float CpuUsage,
        float MemoryUsage,
        float AvailableMemory,
        float MaxAvailableMemory,
        int ActivationCount);
}
```

**Algorithm**: **Weighted Multi-Dimensional Scoring + √N Selection**
1. Collect non-overloaded compatible silos
2. Randomly select √N candidates (e.g., 4 from 16)
3. Score each candidate using weighted formula:
   - CPU usage (normalized)
   - Memory usage (normalized by max available)
   - Available memory (inverted, normalized)
   - Max available memory (normalized across cluster)
   - Activation count (normalized by max)
4. Pick candidate with minimum score
5. Prefer local silo if score difference < margin

**Key Features**:
- **Configurable weights** - Tune for CPU vs memory vs activation count
- **Stack allocation** - Avoid heap allocation for clusters up to ~170 silos
- **√N candidate selection** - Balance between random and exhaustive search
- **Local silo preference** - Reduce network hops if local score is close
- **Tie-breaking jitter** - Small random perturbation to avoid first-in-list bias
- **Normalization** - All metrics scaled to [0, 1] for fair comparison

**Performance Characteristics**:
- Time: O(N + √N) where N = compatible silos
- Space: O(N) on stack for N ≤ 170, otherwise O(N) from ArrayPool

**Use Case**: Production workloads with heterogeneous hardware, resource-constrained clusters

---

#### 3.6 SiloRoleBasedPlacementDirector (Metadata Filtering)

**File**: `SiloRoleBasedPlacementDirector.cs:13-62`

```csharp
internal class SiloRoleBasedPlacementDirector : IPlacementDirector
{
    public Task<SiloAddress> OnAddActivation(
        PlacementStrategy strategy,
        PlacementTarget target,
        IPlacementContext context)
    {
        var compatibleSilos = context.GetCompatibleSilos(target);

        // Check for placement hint first
        if (IPlacementDirector.GetPlacementHint(target.RequestContextData, compatibleSilos) is { } placementHint)
            return Task.FromResult(placementHint);

        // Filter to silos matching required role
        var roleBasedPlacement = (SiloRoleBasedPlacement)strategy;
        var silosInRole = compatibleSilos
            .Where(silo => _siloStatusOracle.GetSiloMetadata(silo)
                .TryGetValue(SiloRoleBasedPlacement.RoleMetadataKey, out var role)
                && role == roleBasedPlacement.Role)
            .ToArray();

        if (silosInRole.Length == 0)
            throw new SiloUnavailableException($"Cannot place grain '{target.GrainIdentity}' with role '{roleBasedPlacement.Role}'");

        // Random selection from matching silos
        return Task.FromResult(silosInRole[Random.Shared.Next(silosInRole.Length)]);
    }
}
```

**Use Case**: Dedicated hardware pools (GPU silos, high-memory silos, etc.), regulatory compliance

---

#### 3.7 StatelessWorkerDirector (High-Throughput)

**File**: `StatelessWorkerDirector.cs:16-47`

```csharp
internal class StatelessWorkerDirector : RandomPlacementDirector
{
    private readonly ILocalSiloDetails _localSiloDetails;

    public override Task<SiloAddress> OnAddActivation(
        PlacementStrategy strategy,
        PlacementTarget target,
        IPlacementContext context)
    {
        // Always prefer local silo for stateless workers
        var localSilo = _localSiloDetails.SiloAddress;

        if (context.LocalSiloStatus == SiloStatus.Active)
        {
            var compatibleSilos = context.GetCompatibleSilos(target);
            if (compatibleSilos.Contains(localSilo))
                return Task.FromResult(localSilo);
        }

        // Fall back to random if local not available
        return base.OnAddActivation(strategy, target, context);
    }
}
```

**Use Case**: Stateless request processing, parallel fan-out, high-throughput workloads

**Special Property**: Multiple activations per grain ID allowed (controlled by `MaxLocalWorkers`)

---

### 4. Placement Filtering System

#### PlacementFilterStrategyResolver

**File**: `PlacementFilterStrategyResolver.cs:12-64`

Resolves which filters apply to each grain type:

```csharp
internal class PlacementFilterStrategyResolver
{
    private readonly ConcurrentDictionary<GrainType, PlacementFilterStrategy[]> _resolvedPlacementFilterStrategies = new();
    private readonly GrainPropertiesResolver _grainPropertiesResolver;

    public PlacementFilterStrategy[] GetPlacementFilterStrategies(GrainType grainType)
    {
        return _resolvedPlacementFilterStrategies.GetOrAdd(grainType, ResolveStrategies);
    }

    private PlacementFilterStrategy[] ResolveStrategies(GrainType grainType)
    {
        if (!_grainPropertiesResolver.TryGetGrainProperties(grainType, out var properties))
            return Array.Empty<PlacementFilterStrategy>();

        // Extract filter strategy names from grain properties
        var filters = new List<PlacementFilterStrategy>();
        foreach (var property in properties.Properties)
        {
            if (property.Key.StartsWith(WellKnownGrainTypeProperties.PlacementFilterPrefix))
            {
                var filterTypeName = property.Value;
                var filterType = Type.GetType(filterTypeName);
                var filter = (PlacementFilterStrategy)Activator.CreateInstance(filterType);
                filter.Initialize(properties);
                filters.Add(filter);
            }
        }

        // Sort by order property
        filters.Sort((a, b) => a.Order.CompareTo(b.Order));
        return filters.ToArray();
    }
}
```

**Pattern**: **Chain of Responsibility** - Ordered filter chain

#### RequiredMatchSiloMetadataPlacementFilterDirector

**File**: `RequiredMatchSiloMetadataPlacementFilterDirector.cs:12-48`

```csharp
internal class RequiredMatchSiloMetadataPlacementFilterDirector : IPlacementFilterDirector
{
    private readonly ISiloStatusOracle _siloStatusOracle;

    public IEnumerable<SiloAddress> Filter(
        PlacementFilterStrategy strategy,
        PlacementFilterContext context,
        IEnumerable<SiloAddress> silos)
    {
        var filterStrategy = (RequiredMatchSiloMetadataPlacementFilterStrategy)strategy;
        var key = filterStrategy.Key;
        var value = filterStrategy.Value;

        return silos.Where(silo =>
        {
            var metadata = _siloStatusOracle.GetSiloMetadata(silo);
            return metadata.TryGetValue(key, out var siloValue) && siloValue == value;
        });
    }
}
```

**Use Case**: Enforce placement constraints (e.g., only "region=us-west" silos)

#### PreferredMatchSiloMetadataPlacementFilterDirector

**File**: `PreferredMatchSiloMetadataPlacementFilterDirector.cs:12-51`

```csharp
internal class PreferredMatchSiloMetadataPlacementFilterDirector : IPlacementFilterDirector
{
    public IEnumerable<SiloAddress> Filter(
        PlacementFilterStrategy strategy,
        PlacementFilterContext context,
        IEnumerable<SiloAddress> silos)
    {
        var filterStrategy = (PreferredMatchSiloMetadataPlacementFilterStrategy)strategy;

        var preferred = silos.Where(silo =>
        {
            var metadata = _siloStatusOracle.GetSiloMetadata(silo);
            return metadata.TryGetValue(filterStrategy.Key, out var value)
                && value == filterStrategy.Value;
        }).ToArray();

        // Return preferred silos if any, otherwise all silos
        return preferred.Length > 0 ? preferred : silos;
    }
}
```

**Use Case**: Soft preferences (e.g., prefer "hardware=gpu" but allow fallback)

---

### 5. Directory Service Integration

#### GrainLocator (Facade)

**File**: `GrainLocator.cs:8-49`

```csharp
internal class GrainLocator
{
    private readonly GrainLocatorResolver _grainLocatorResolver;

    public ValueTask<GrainAddress?> Lookup(GrainId grainId)
        => GetGrainLocator(grainId.Type).Lookup(grainId);

    public Task<GrainAddress> Register(GrainAddress address, GrainAddress? previousRegistration)
        => GetGrainLocator(address.GrainId.Type).Register(address, previousRegistration);

    public Task Unregister(GrainAddress address, UnregistrationCause cause)
        => GetGrainLocator(address.GrainId.Type).Unregister(address, cause);

    public bool TryLookupInCache(GrainId grainId, out GrainAddress? address)
        => GetGrainLocator(grainId.Type).TryLookupInCache(grainId, out address);

    public void InvalidateCache(GrainId grainId)
        => GetGrainLocator(grainId.Type).InvalidateCache(grainId);

    public void UpdateCache(GrainId grainId, SiloAddress siloAddress)
        => GetGrainLocator(grainId.Type).UpdateCache(grainId, siloAddress);

    private IGrainLocator GetGrainLocator(GrainType grainType)
        => _grainLocatorResolver.GetGrainLocator(grainType);
}
```

**Pattern**: **Facade** - Simple interface hiding per-grain-type locator resolution

#### DhtGrainLocator (DHT-Based Directory)

**File**: `DhtGrainLocator.cs:14-153`

```csharp
internal class DhtGrainLocator : IGrainLocator
{
    private readonly ILocalGrainDirectory _localGrainDirectory;
    private readonly IGrainContext _grainContext;
    private BatchedDeregistrationWorker _forceWorker;
    private BatchedDeregistrationWorker _neaWorker;

    public async ValueTask<GrainAddress> Lookup(GrainId grainId)
        => (await _localGrainDirectory.LookupAsync(grainId)).Address;

    public async Task<GrainAddress> Register(GrainAddress address, GrainAddress previousAddress)
        => (await _localGrainDirectory.RegisterAsync(address, currentRegistration: previousAddress)).Address;

    public Task Unregister(GrainAddress address, UnregistrationCause cause)
    {
        var worker = cause switch
        {
            UnregistrationCause.Force => _forceWorker,
            UnregistrationCause.NonexistentActivation => _neaWorker,
            _ => throw new ArgumentOutOfRangeException()
        };

        return worker.Unregister(address);
    }

    // Batched deregistration for efficiency
    private class BatchedDeregistrationWorker
    {
        private const int OperationBatchSizeLimit = 2_000;
        private readonly Channel<(TaskCompletionSource<bool> tcs, GrainAddress address)> _queue;

        public Task Unregister(GrainAddress address)
        {
            var tcs = new TaskCompletionSource<bool>();
            _queue.Writer.TryWrite((tcs, address));
            return tcs.Task;
        }

        private async Task ProcessDeregistrationQueue()
        {
            var operations = new List<TaskCompletionSource<bool>>();
            var addresses = new List<GrainAddress>();

            while (await _queue.Reader.WaitToReadAsync())
            {
                operations.Clear();
                addresses.Clear();

                // Batch up to 2000 operations
                while (operations.Count < OperationBatchSizeLimit && _queue.Reader.TryRead(out var op))
                {
                    operations.Add(op.tcs);
                    addresses.Add(op.address);
                }

                if (operations.Count > 0)
                {
                    await _localGrainDirectory.UnregisterManyAsync(addresses, _cause);
                    foreach (var op in operations)
                        op.TrySetResult(true);
                }
            }
        }
    }
}
```

**Pattern**: **Batching** - Coalesce up to 2000 deregistration operations for efficiency

**Key Features**:
1. **Separate workers** - Different batch workers for `Force` vs `NonexistentActivation` causes
2. **High throughput** - Batch size of 2000 reduces directory RPC overhead
3. **Async batching** - Uses `Channel<T>` for producer-consumer pattern

#### LocalGrainDirectory (Consistent Hash Ring)

**File**: `LocalGrainDirectory.cs:17-850` (excerpt relevant to placement)

```csharp
internal sealed class LocalGrainDirectory : ILocalGrainDirectory
{
    internal IGrainDirectoryCache DirectoryCache { get; }
    internal LocalGrainDirectoryPartition DirectoryPartition { get; }

    // Calculate which silo owns directory partition for this grain
    public SiloAddress? CalculateGrainDirectoryPartition(GrainId grainId)
    {
        var hash = unchecked((int)grainId.GetUniformHashCode());
        bool excludeMySelf = !Running;

        var existing = this.directoryMembership;
        if (existing.MembershipRingList.Count == 0)
            return !Running ? null : MyAddress;

        // Binary search in ring (simplified)
        for (var index = existing.MembershipRingList.Count - 1; index >= 0; --index)
        {
            var item = existing.MembershipRingList[index];
            if (IsSiloNextInTheRing(item, hash, excludeMySelf))
            {
                return item;
            }
        }

        return existing.MembershipRingList[existing.MembershipRingList.Count - 1];
    }

    // Lookup with caching
    public async Task<AddressAndTag> LookupAsync(GrainId grainId, int hopCount = 0)
    {
        var forwardAddress = CheckIfShouldForward(grainId, hopCount, "LookUpAsync");

        if (forwardAddress == null)
        {
            // We own this grain - lookup locally
            var localResult = DirectoryPartition.LookUpActivation(grainId);
            return localResult;
        }
        else
        {
            // Forward to owner silo
            var result = await GetDirectoryReference(forwardAddress).LookupAsync(grainId, hopCount + 1);

            // Cache the result
            if (result.Address is { } address && IsValidSilo(address.SiloAddress))
                DirectoryCache.AddOrUpdate(address, result.VersionTag);

            return result;
        }
    }
}
```

**Pattern**: **Consistent Hashing** with **Forwarding** and **Caching**

**Integration with Placement**:
1. Placement chooses target silo
2. Activation created on target silo
3. Directory registration routed to partition owner via consistent hash
4. Future lookups hit cache or forward to partition owner

---

## Key Algorithms and Patterns

### 1. Worker Pool Architecture

**Pattern**: **Fixed Worker Pool** with **Grain-Level Coalescing**

**Benefits**:
- Parallelism without unbounded concurrency
- Coalescing reduces duplicate placement work
- Consistent hashing distributes load evenly

**Trade-offs**:
- Fixed pool size (16 workers) - no auto-scaling
- Single-threaded processing per worker
- Lock contention on `_inProgress` dictionary

### 2. Multi-Level Caching

**Three cache levels**:

1. **Message-level cache** - PlacementService checks before worker dispatch
2. **GrainLocator cache** - Worker checks before placement decision
3. **Directory cache** - LocalGrainDirectory caches remote lookups

**Invalidation**:
- Proactive: Messages carry `CacheInvalidationHeader` from failed deliveries
- Reactive: Worker invalidates after placement, then updates with fresh address
- TTL: Directory cache may have configurable expiration (not shown in these files)

### 3. Strategy-Director Pattern

**Pattern**: **Strategy** (data) + **Director** (algorithm)

**Why separated?**:
- Strategies serializable (sent with grain properties)
- Directors stateful (hold silo statistics, caches)
- Strategies resolved per grain type, directors shared across grain types

**DI Integration**:
```csharp
services.AddKeyedSingleton<IPlacementDirector>(typeof(RandomPlacement), typeof(RandomPlacementDirector));
services.AddKeyedSingleton<IPlacementDirector>(typeof(ResourceOptimizedPlacement), typeof(ResourceOptimizedPlacementDirector));
```

### 4. Load-Aware Placement

**Pattern**: **Observer** - Directors subscribe to silo statistics

```csharp
internal interface ISiloStatisticsChangeListener
{
    void SiloStatisticsChangeNotification(SiloAddress address, SiloRuntimeStatistics statistics);
    void RemoveSilo(SiloAddress removedSilo);
}
```

**Publishers**:
- `DeploymentLoadPublisher` - Aggregates and broadcasts silo statistics
- Frequency: Typically every few seconds (configurable)

**Subscribers**:
- `ActivationCountPlacementDirector`
- `ResourceOptimizedPlacementDirector`

### 5. Placement Hint Protocol

**Mechanism**: Callers can suggest target silo via `RequestContext`

```csharp
RequestContext.Set(IPlacementDirector.PlacementHintKey, suggestedSilo);
```

**Validation**: Director checks hint is in compatible silo set

**Use Cases**:
- Grain migration (hint = target silo)
- Co-location (hint = silo hosting related grain)
- Affinity routing (hint = data partition location)

---

## Patterns for Moonpool

### ✅ Adopt These Patterns

1. **Strategy-Director Separation**: Data vs algorithm, enables serialization and state management
   ```rust
   pub trait PlacementStrategy: Debug + Send + Sync {}
   pub trait PlacementDirector: Send + Sync {
       async fn on_add_activation(&self, strategy: &dyn PlacementStrategy, target: PlacementTarget, context: &dyn PlacementContext) -> NodeId;
   }
   ```

2. **Worker Pool with Coalescing**: Fixed workers, grain-level deduplication
   ```rust
   struct PlacementWorker {
       in_progress: HashMap<ActorId, PlacementWorkItem>,
       messages: VecDeque<(Message, oneshot::Sender<NodeId>)>,
   }
   ```

3. **Multi-Level Caching**: Message cache → actor locator cache → directory cache
   ```rust
   // Fast path
   if let Some(address) = cache.get(actor_id) {
       if validate_cache_entry(message, address) {
           return address;
       }
   }
   ```

4. **Placement Hint Protocol**: Allow callers to suggest target
   ```rust
   pub struct RequestContext {
       pub placement_hint: Option<NodeId>,
   }
   ```

5. **Load-Aware Placement**: Subscribe to node statistics
   ```rust
   pub trait NodeStatisticsListener {
       fn on_statistics_update(&mut self, node: NodeId, stats: NodeStatistics);
       fn on_node_removed(&mut self, node: NodeId);
   }
   ```

6. **Ordered Filter Chain**: Apply filters sequentially
   ```rust
   let mut compatible_nodes = get_compatible_nodes(target);
   for filter in filters {
       compatible_nodes = filter.apply(compatible_nodes);
   }
   ```

### ⚠️ Adapt These Patterns

1. **16 Workers**: Orleans uses fixed pool size
   - Moonpool: Start with 1 (single-threaded), measure, scale if needed
   - Consider: Async tasks instead of thread pool

2. **ConcurrentDictionary**: Orleans relies on .NET concurrent collections
   - Moonpool: Use `tokio::sync::RwLock<HashMap>` or `DashMap`
   - Measure first: single-threaded simulation may not need concurrency

3. **Stack Allocation**: Orleans optimizes for large clusters (170 silos)
   - Moonpool: Heap allocation simpler, measure before optimizing
   - Consider: `SmallVec` for typical cluster sizes (< 10 nodes)

4. **ISiloStatisticsChangeListener**: Orleans uses interface-based pub/sub
   - Moonpool: Use `tokio::sync::watch` or message passing
   - More idiomatic Rust pattern

### ❌ Avoid These Patterns

1. **Synchronous Locks in Async**: Orleans uses `lock (...)` in async methods
   - Moonpool: Use `tokio::sync::Mutex` for async, `parking_lot::Mutex` for sync sections
   - Never hold locks across `.await`

2. **Interlocked Operations**: Orleans uses `Interlocked.Increment`
   - Moonpool: Use `AtomicUsize` with `Ordering::Relaxed` for counters
   - Or just `+=` in single-threaded sections

3. **TaskScheduler Integration**: Orleans custom TaskScheduler
   - Moonpool: Use `Provider` traits, not Rust's task system internals
   - Keep abstractions clean

---

## Critical Insights for Moonpool Phase 12

### Placement vs Directory Separation

**Orleans Architecture**:
```
Placement: "Where should this new activation go?"
    ↓ (choose target silo)
Directory: "Where is this existing activation?"
    ↓ (register/lookup/cache)
```

**Moonpool Phase 12**:
- Start with **static placement** (all actors local)
- Implement directory as simple `HashMap<ActorId, ActorAddress>`
- Add distributed placement in later phases

### Testing Strategy

**From Orleans patterns**:

1. **Strategy testing**: Unit test each director algorithm
   ```rust
   #[test]
   fn test_random_placement_distributes_evenly() {
       let director = RandomPlacementDirector::new();
       let placements = (0..1000).map(|_| director.on_add_activation(...)).collect();
       assert_distribution_within_tolerance(placements, nodes, 0.15);
   }
   ```

2. **Load testing**: Verify load-aware strategies respond to statistics
   ```rust
   #[test]
   fn test_activation_count_prefers_lighter_silos() {
       let mut director = ActivationCountPlacementDirector::new();
       director.on_statistics_update(node_a, stats_with_count(100));
       director.on_statistics_update(node_b, stats_with_count(10));

       let chosen = director.on_add_activation(...);
       assert_eq!(chosen, node_b);  // Should prefer lighter node
   }
   ```

3. **Cache invalidation**: Test proactive cache updates
   ```rust
   #[test]
   fn test_cache_invalidation_on_failed_delivery() {
       let cache = ActorCache::new();
       cache.insert(actor_id, old_address);

       // Simulate failed delivery with cache update
       let message = Message {
           cache_invalidation: Some(vec![CacheUpdate {
               invalid_address: old_address,
               valid_address: Some(new_address),
           }]),
           ..
       };

       cache.apply_invalidation(&message);
       assert_eq!(cache.get(actor_id), Some(new_address));
   }
   ```

4. **Filter chain**: Test ordered filter application
   ```rust
   #[test]
   fn test_required_filter_excludes_incompatible() {
       let nodes = vec![node_gpu, node_cpu];
       let filter = RequiredMatchFilter::new("hardware", "gpu");
       let filtered = filter.apply(nodes);
       assert_eq!(filtered, vec![node_gpu]);
   }
   ```

### Invariants to Check

**From Orleans architecture**:

1. **Single activation per grain** (except StatelessWorker): `catalog.get(grain_id).len() <= 1`
2. **Cache consistency**: Cached address must match directory (eventually)
3. **Compatible silo guarantee**: Chosen silo must support grain type
4. **Filter ordering**: Filters applied in order specified
5. **Load bounds**: Overloaded silos excluded from placement
6. **Coalescing**: Multiple concurrent requests for same grain share placement operation

---

## Summary

Orleans' placement strategy system is built on:

1. **PlacementService** - 16-worker pool with grain-level coalescing, multi-level caching
2. **Strategy-Director Pattern** - Pluggable algorithms with DI-based resolution
3. **9 Built-in Strategies** - Random, PreferLocal, Hash, ActivationCount (Power of Two), ResourceOptimized (weighted scoring), SiloRole, StatelessWorker, SystemTarget, ClientObservers
4. **Placement Filtering** - Ordered chain of metadata-based filters (Required/Preferred)
5. **Directory Integration** - Tight coupling with grain directory for lookup/registration
6. **Load Awareness** - Directors subscribe to silo statistics for intelligent placement
7. **Multi-Level Caching** - Message cache → locator cache → directory cache

Key architectural principles:
- Separation of strategy (data) and director (algorithm)
- Worker pool for parallelism without unbounded concurrency
- Multi-level caching with proactive invalidation
- Load-aware placement via observer pattern
- Flexible filtering for metadata-based constraints

For Moonpool Phase 12: Start with static placement (all local), implement directory as simple HashMap, add distributed placement incrementally with careful attention to caching and invalidation protocols.

---

## References

### Core Implementation Files

**Orchestration**:
- `PlacementService.cs:1-430` - Central orchestrator with 16-worker pool
- `PlacementStrategyResolver.cs` - Strategy resolution by grain type
- `PlacementDirectorResolver.cs` - Director resolution via DI

**Interfaces**:
- `IPlacementDirector.cs:7-46` - Core placement director interface
- `IPlacementContext.cs:5-38` - Context provider interface
- `PlacementTarget.cs:5-45` - Placement input structure
- `PlacementStrategy.cs:7-47` - Base strategy class

**Directors**:
- `RandomPlacementDirector.cs:11-27` - Simple random selection
- `PreferLocalPlacementDirector.cs:10-35` - Locality optimization
- `HashBasedPlacementDirector.cs:10-29` - Deterministic placement
- `ActivationCountPlacementDirector.cs:13-127` - Power of Two Choices
- `ResourceOptimizedPlacementDirector.cs:14-235` - Weighted multi-dimensional scoring
- `SiloRoleBasedPlacementDirector.cs:13-62` - Metadata-based filtering
- `StatelessWorkerDirector.cs:16-47` - High-throughput local placement

**Filtering**:
- `PlacementFilterStrategyResolver.cs:12-64` - Filter resolution
- `RequiredMatchSiloMetadataPlacementFilterDirector.cs:12-48` - Hard constraints
- `PreferredMatchSiloMetadataPlacementFilterDirector.cs:12-51` - Soft preferences

**Directory Integration**:
- `GrainLocator.cs:8-49` - Locator facade
- `DhtGrainLocator.cs:14-153` - DHT-based directory with batching
- `LocalGrainDirectory.cs:323-394` - Consistent hash partition calculation

### Algorithm References

- **Power of Two Choices**: https://www.eecs.harvard.edu/~michaelm/postscripts/handbook2001.pdf
- **Resource-Optimized Placement (Kalman filtering)**: https://www.ledjonbehluli.com/posts/orleans_resource_placement_kalman/
- **Consistent Hashing**: https://arxiv.org/abs/1406.2294
- **Fisher-Yates Shuffle**: https://en.wikipedia.org/wiki/Fisher%E2%80%93Yates_shuffle

### Related Analyses

- **grain-directory.md** - Directory service architecture and consistent ring
- **activation-lifecycle.md** - How activations are created and registered
- **message-system.md** - Cache invalidation protocol
- **membership-service.md** - Silo statistics and cluster membership
