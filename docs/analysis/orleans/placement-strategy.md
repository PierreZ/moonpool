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
+------------------------------------------------------------+
| PlacementService (Central Orchestrator)                    |  <- 16-worker pool, message addressing
+------------------------------------------------------------+
| PlacementStrategyResolver / PlacementDirectorResolver      |  <- DI-based strategy-to-director mapping
+------------------------------------------------------------+
| PlacementFilterStrategyResolver / FilterDirectorResolver   |  <- Ordered filter chain
+------------------------------------------------------------+
| IPlacementDirector (8 built-in implementations)            |  <- Random, PreferLocal, Hash, ActivationCount, ResourceOptimized, etc.
+------------------------------------------------------------+
| IPlacementContext                                          |  <- Compatible silos, local silo info, version management
+------------------------------------------------------------+
| GrainLocator -> LocalGrainDirectory / DhtGrainLocator      |  <- Directory service integration
+------------------------------------------------------------+
```

---

## Core Components

### 1. PlacementService (Central Orchestrator)

**File**: `src/Orleans.Runtime/Placement/PlacementService.cs:1-443`

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
    if (_grainLocator.TryLookupInCache(grainId, out var result) && CachedAddressIsValid(message, result))
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

#### Cache Invalidation (lines 175-214)

**Code**: `CachedAddressIsValid()` method

```csharp
private bool CachedAddressIsValid(Message message, GrainAddress cachedAddress)
{
    // Verify that the result from the cache has not been invalidated
    if (message.CacheInvalidationHeader is { } cacheUpdates)
    {
        lock (cacheUpdates)
        {
            return CachedAddressIsValidCore(message, cachedAddress, cacheUpdates);
        }
    }

    return true;

    // Process invalidation updates
    bool CachedAddressIsValidCore(Message message, GrainAddress cachedAddress, List<GrainAddressCacheUpdate> cacheUpdates)
    {
        var resultIsValid = true;
        foreach (var update in cacheUpdates)
        {
            var invalidAddress = update.InvalidGrainAddress;
            var validAddress = update.ValidGrainAddress;
            _grainLocator.UpdateCache(update);  // Apply update immediately

            if (cachedAddress.Matches(validAddress))
                resultIsValid = true;
            else if (cachedAddress.Matches(invalidAddress))
                resultIsValid = false;
        }
        return resultIsValid;
    }
}
```

**Pattern**: **Proactive Invalidation** - Messages carry cache updates to avoid stale lookups

Note: The `CacheInvalidationHeader` is a `List<GrainAddressCacheUpdate>` protected by a `lock`, not a pattern-matched `{ Count: > 0 }` collection as in earlier versions.

#### Compatible Silo Resolution (lines 104-146)

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
        foreach (var placementFilter in filters)
        {
            var director = _placementFilterDirectoryResolver.GetFilterDirector(placementFilter);
            filteredSilos = director.Filter(placementFilter, target, filteredSilos);
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
3. **Ordered filter chain** - Apply all placement filters sequentially (filters receive `PlacementTarget` directly, not a separate `PlacementFilterContext`)
4. **Detailed error messages** - Report both grain type and interface version mismatches

#### Placement Worker (lines 230-404)

**16 concurrent workers for parallel placement**:

```csharp
private class PlacementWorker
{
    private readonly Dictionary<GrainId, GrainPlacementWorkItem> _inProgress = new();
    private readonly SingleWaiterAutoResetEvent _workSignal = new() { RunContinuationsAsynchronously = true };
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
        Action signalWaiter = _workSignal.Signal;
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
                        workItem.Result.GetAwaiter().UnsafeOnCompleted(signalWaiter);
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

        GrainPlacementWorkItem GetOrAddWorkItem(GrainId target)
        {
            ref var workItem = ref CollectionsMarshal.GetValueRefOrAddDefault(_inProgress, target, out _);
            workItem ??= new();
            return workItem;
        }
    }

    // Placement logic: lookup -> place -> cache
    private async Task<SiloAddress> GetOrPlaceActivationAsync(Message firstMessage)
    {
        await Task.Yield();
        var target = new PlacementTarget(
            firstMessage.TargetGrain,
            firstMessage.RequestContextData,
            firstMessage.InterfaceType,
            firstMessage.InterfaceVersion);

        // Try directory lookup first
        var targetGrain = target.GrainIdentity;
        var result = await _placementService._grainLocator.Lookup(targetGrain);
        if (result is not null)
            return result.SiloAddress;

        // No activation exists - run placement
        var strategy = _placementService._strategyResolver.GetPlacementStrategy(target.GrainIdentity.Type);
        var director = _placementService._directorResolver.GetPlacementDirector(strategy);
        var siloAddress = await director.OnAddActivation(strategy, target, _placementService);

        // Double-check cache (race with another worker)
        if (_placementService._grainLocator.TryLookupInCache(targetGrain, out result)
            && _placementService.CachedAddressIsValid(firstMessage, result))
        {
            return result.SiloAddress;
        }

        // Update cache and return
        _placementService._grainLocator.InvalidateCache(targetGrain);
        _placementService._grainLocator.UpdateCache(targetGrain, siloAddress);
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
5. **Efficient dictionary access** - Uses `CollectionsMarshal.GetValueRefOrAddDefault` to avoid double lookups

---

### 2. Placement Strategy Interfaces

#### IPlacementDirector (Core Interface)

**File**: `src/Orleans.Core/Placement/IPlacementDirector.cs:1-47`

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

**File**: `src/Orleans.Core/Placement/IPlacementContext.cs:1-38`

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

**File**: `src/Orleans.Core/Placement/PlacementTarget.cs:1-46`

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

**File**: `src/Orleans.Runtime/Placement/RandomPlacementDirector.cs:6-22`

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
        return Task.FromResult(compatibleSilos[Random.Shared.Next(compatibleSilos.Length)]);
    }
}
```

**Use Case**: Default strategy when no special requirements, good load distribution

---

#### 3.2 PreferLocalPlacementDirector (Locality Optimization)

**File**: `src/Orleans.Runtime/Placement/PreferLocalPlacementDirector.cs:14-29`

```csharp
internal class PreferLocalPlacementDirector : RandomPlacementDirector, IPlacementDirector
{
    private Task<SiloAddress> _cachedLocalSilo;

    public override Task<SiloAddress>
        OnAddActivation(PlacementStrategy strategy, PlacementTarget target, IPlacementContext context)
    {
        // if local silo is not active or does not support this type of grain, revert to random placement
        if (context.LocalSiloStatus != SiloStatus.Active || !context.GetCompatibleSilos(target).Contains(context.LocalSilo))
        {
            return base.OnAddActivation(strategy, target, context);
        }

        return _cachedLocalSilo ??= Task.FromResult(context.LocalSilo);
    }
}
```

**Key Design Notes**:
- Does NOT check placement hint - delegates to `base.OnAddActivation()` (RandomPlacementDirector) which checks it on the fallback path
- Uses `_cachedLocalSilo` field to cache the `Task<SiloAddress>` for the local silo, avoiding repeated `Task.FromResult` allocations
- No `ILocalSiloDetails` dependency - uses `context.LocalSilo` directly
- Checks both `LocalSiloStatus == Active` AND local silo in compatible set before preferring local

**Use Case**: Minimize network hops, co-locate related grains, reduce latency

---

#### 3.3 HashBasedPlacementDirector (Deterministic Placement)

**File**: `src/Orleans.Runtime/Placement/HashBasedPlacementDirector.cs:6-24`

```csharp
internal class HashBasedPlacementDirector : IPlacementDirector
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

        // Sort silos for deterministic ordering, then hash-based selection
        var sortedSilos = compatibleSilos.OrderBy(s => s).ToArray();
        int hash = (int)(target.GrainIdentity.GetUniformHashCode() & 0x7fffffff);
        return Task.FromResult(sortedSilos[hash % sortedSilos.Length]);
    }
}
```

**Key Design Note**: The compatible silos array is **sorted first** (`OrderBy(s => s)`) to ensure deterministic placement regardless of the order silos appear in the compatible set. Without this sorting, the same grain could be placed on different silos depending on the order of the `GetCompatibleSilos` result, defeating the purpose of hash-based placement.

**Use Case**: Predictable placement, grain affinity, co-location of related grains (same hash prefix)

---

#### 3.4 ActivationCountPlacementDirector (Power of Two Choices)

**File**: `src/Orleans.Runtime/Placement/ActivationCountPlacementDirector.cs:13-127`

**Most sophisticated load-balancing algorithm**:

```csharp
internal class ActivationCountPlacementDirector : RandomPlacementDirector,
    ISiloStatisticsChangeListener, IPlacementDirector
{
    private class CachedLocalStat
    {
        private int _activationCount;

        internal CachedLocalStat(SiloRuntimeStatistics siloStats) => SiloStats = siloStats;

        public SiloRuntimeStatistics SiloStats { get; }
        public int ActivationCount => _activationCount;
        public void IncrementActivationCount(int delta) => Interlocked.Add(ref _activationCount, delta);
    }

    private readonly ConcurrentDictionary<SiloAddress, CachedLocalStat> _localCache = new();
    private readonly SiloAddress _localAddress;
    private readonly int _chooseHowMany;  // Default: 2 (power of two choices)

    private SiloAddress SelectSiloPowerOfK(SiloAddress[] silos)
    {
        var compatibleSilos = silos.ToSet();

        // Filter to relevant silos (active, compatible, not overloaded)
        var relevantSilos = new List<KeyValuePair<SiloAddress, CachedLocalStat>>();
        var totalSilos = 0;
        foreach (var kv in _localCache)
        {
            totalSilos++;
            if (kv.Value.SiloStats.IsOverloaded) continue;
            if (!compatibleSilos.Contains(kv.Key)) continue;
            relevantSilos.Add(kv);
        }

        if (relevantSilos.Count > 0)
        {
            // Select K random candidates
            int chooseFrom = Math.Min(relevantSilos.Count, _chooseHowMany);
            var chooseFromThoseSilos = new List<KeyValuePair<SiloAddress, CachedLocalStat>>(chooseFrom);
            while (chooseFromThoseSilos.Count < chooseFrom)
            {
                int index = Random.Shared.Next(relevantSilos.Count);
                var pickedSilo = relevantSilos[index];
                relevantSilos.RemoveAt(index);
                chooseFromThoseSilos.Add(pickedSilo);
            }

            // Pick candidate with minimum load (explicit loop, not LINQ MinBy)
            KeyValuePair<SiloAddress, CachedLocalStat> minLoadedSilo = default;
            var minLoad = int.MaxValue;
            foreach (var s in chooseFromThoseSilos)
            {
                var load = s.Value.ActivationCount + s.Value.SiloStats.RecentlyUsedActivationCount;
                if (load < minLoad)
                {
                    minLoadedSilo = s;
                    minLoad = load;
                }
            }

            // Increment by total silos (heuristic for concurrent placements)
            minLoadedSilo.Value.IncrementActivationCount(totalSilos);

            return minLoadedSilo.Key;
        }

        throw new SiloUnavailableException("Unable to select a candidate...");
    }

    public override Task<SiloAddress> OnAddActivation(PlacementStrategy strategy, PlacementTarget target, IPlacementContext context)
        => Task.FromResult(OnAddActivationInternal(target, context));

    private SiloAddress OnAddActivationInternal(PlacementTarget target, IPlacementContext context)
    {
        var compatibleSilos = context.GetCompatibleSilos(target);

        // Check for placement hint first
        if (IPlacementDirector.GetPlacementHint(target.RequestContextData, compatibleSilos) is { } placementHint)
            return placementHint;

        // If the cache was not populated, just place locally
        if (_localCache.IsEmpty)
            return _localAddress;

        return SelectSiloPowerOfK(compatibleSilos);
    }

    // Subscribe to silo statistics updates
    public void SiloStatisticsChangeNotification(SiloAddress updatedSilo, SiloRuntimeStatistics newSiloStats)
    {
        _localCache[updatedSilo] = new(newSiloStats);
    }

    public void RemoveSilo(SiloAddress removedSilo)
    {
        _localCache.TryRemove(removedSilo, out _);
    }
}
```

**Algorithm**: **Power of Two Choices**
1. Randomly select K candidates (default K=2)
2. Pick candidate with minimum activation count (using explicit loop, not LINQ)
3. Increment by total silo count (account for concurrent placements)

**Key Features**:
- Provably better than pure random (exponential improvement with K=2)
- Low overhead (no global coordination)
- Adaptive to load changes via silo statistics
- Excludes overloaded silos
- Falls back to local silo when cache is empty (no statistics yet)

**Use Case**: General-purpose load balancing, better than random with minimal overhead

---

#### 3.5 ResourceOptimizedPlacementDirector (Advanced Scoring)

**File**: `src/Orleans.Runtime/Placement/ResourceOptimizedPlacementDirector.cs:14-239`

**Most sophisticated placement algorithm in Orleans**:

```csharp
internal sealed class ResourceOptimizedPlacementDirector : IPlacementDirector, ISiloStatisticsChangeListener
{
    private const int FourKiloByte = 4096;

    private readonly SiloAddress _localSilo;
    private readonly NormalizedWeights _weights;
    private readonly float _localSiloPreferenceMargin;
    private readonly ConcurrentDictionary<SiloAddress, ResourceStatistics> _siloStatistics = [];
    private readonly Task<SiloAddress> _cachedLocalSilo;

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

        return _cachedLocalSilo;

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

                    if (stats.MaxAvailableMemory > maxMaxAvailableMemory)
                        maxMaxAvailableMemory = stats.MaxAvailableMemory;

                    if (stats.ActivationCount > maxActivationCount)
                        maxActivationCount = stats.ActivationCount;

                    if (silo.Equals(_localSilo))
                        localSiloStatistics = stats;
                }
            }

            relevantSilos = relevantSilos[0..relevantSilosCount];

            // Select sqrt(N) candidates (e.g., 4 from 16 silos)
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
                var localStats = localSiloStatistics.Value;
                localSiloScore = CalculateScore(in localStats, maxMaxAvailableMemory, maxActivationCount);
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
            float normalizedMemoryUsage = stats.NormalizedMemoryUsage;
            float normalizedAvailableMemory = stats.NormalizedAvailableMemory;
            float normalizedMaxAvailableMemory = stats.MaxAvailableMemory / maxMaxAvailableMemory;

            score += _weights.MemoryUsageWeight * normalizedMemoryUsage +
                     _weights.AvailableMemoryWeight * (1 - normalizedAvailableMemory) +
                     _weights.MaxAvailableMemoryWeight * (1 - normalizedMaxAvailableMemory);
        }

        score += _weights.ActivationCountWeight * stats.ActivationCount / (float)maxActivationCount;

        return score;  // Range: [0, 1]
    }

    // Fisher-Yates partial shuffle
    static void ShufflePrefix(Span<(int SiloIndex, ResourceStatistics SiloStatistics)> values, int prefixLength)
    {
        var max = values.Length;
        for (var i = 0; i < prefixLength; i++)
        {
            var chosen = Random.Shared.Next(i, max);
            if (chosen != i)
                (values[chosen], values[i]) = (values[i], values[chosen]);
        }
    }

    // Subscribe to silo statistics
    public void SiloStatisticsChangeNotification(SiloAddress address, SiloRuntimeStatistics statistics)
        => _siloStatistics.AddOrUpdate(
            key: address,
            factoryArgument: statistics,
            addValueFactory: static (_, statistics) => ResourceStatistics.FromRuntime(statistics),
            updateValueFactory: static (_, _, statistics) => ResourceStatistics.FromRuntime(statistics));

    private record NormalizedWeights(float CpuUsageWeight, float MemoryUsageWeight, float AvailableMemoryWeight, float MaxAvailableMemoryWeight, float ActivationCountWeight);
    private readonly record struct ResourceStatistics(bool IsOverloaded, float CpuUsage, float NormalizedMemoryUsage, float NormalizedAvailableMemory, float MaxAvailableMemory, int ActivationCount)
    {
        public static ResourceStatistics FromRuntime(SiloRuntimeStatistics statistics)
            => new(
                IsOverloaded: statistics.IsOverloaded,
                CpuUsage: statistics.EnvironmentStatistics.FilteredCpuUsagePercentage,
                NormalizedMemoryUsage: statistics.EnvironmentStatistics.NormalizedFilteredMemoryUsage,
                NormalizedAvailableMemory: statistics.EnvironmentStatistics.NormalizedFilteredAvailableMemory,
                MaxAvailableMemory: statistics.EnvironmentStatistics.MaximumAvailableMemoryBytes,
                ActivationCount: statistics.ActivationCount);
    }
}
```

**Algorithm**: **Weighted Multi-Dimensional Scoring + sqrt(N) Selection**
1. Collect non-overloaded compatible silos
2. Randomly select sqrt(N) candidates (e.g., 4 from 16)
3. Score each candidate using weighted formula:
   - CPU usage (normalized to [0,1] from percentage)
   - Memory usage (pre-normalized via `NormalizedFilteredMemoryUsage`)
   - Available memory (pre-normalized via `NormalizedFilteredAvailableMemory`, inverted)
   - Max available memory (normalized across cluster, inverted)
   - Activation count (normalized by max)
4. Pick candidate with minimum score
5. Prefer local silo if score difference < margin

**Key Features**:
- **Configurable weights** - Tune for CPU vs memory vs activation count (normalized so weights sum to 1)
- **Stack allocation** - Avoid heap allocation for clusters up to ~170 silos (4KB / 24 bytes per entry)
- **sqrt(N) candidate selection** - Balance between random and exhaustive search
- **Local silo preference** - Reduce network hops if local score is close (configurable margin)
- **Tie-breaking jitter** - Small random perturbation to avoid first-in-list bias
- **Pre-normalized statistics** - `ResourceStatistics` stores pre-normalized memory values from `EnvironmentStatistics`
- **Cached local silo task** - `_cachedLocalSilo` field avoids repeated `Task.FromResult` allocation

**Performance Characteristics**:
- Time: O(N + sqrt(N)) where N = compatible silos
- Space: O(N) on stack for N <= 170, otherwise O(N) from ArrayPool

**Use Case**: Production workloads with heterogeneous hardware, resource-constrained clusters

---

#### 3.6 SiloRoleBasedPlacementDirector (Role-Based Filtering)

**File**: `src/Orleans.Runtime/Placement/SiloRoleBasedPlacementDirector.cs:8-42`

```csharp
internal class SiloRoleBasedPlacementDirector : IPlacementDirector
{
    private readonly MembershipTableManager membershipTableManager;

    public virtual Task<SiloAddress> OnAddActivation(
        PlacementStrategy strategy,
        PlacementTarget target,
        IPlacementContext context)
    {
        // Extract role name from grain identity key
        var siloRole = target.GrainIdentity.Key.ToString();

        // Filter to silos matching required role from membership table
        var compatibleSilos = membershipTableManager.MembershipTableSnapshot.Entries
            .Where(s => s.Value.Status == SiloStatus.Active && s.Value.RoleName == siloRole)
            .Select(s => s.Key)
            .Intersect(context.GetCompatibleSilos(target))
            .ToArray();

        if (compatibleSilos == null || compatibleSilos.Length == 0)
            throw new OrleansException($"Cannot place grain with RoleName {siloRole}...");

        // If a valid placement hint was specified, use it.
        if (IPlacementDirector.GetPlacementHint(target.RequestContextData, compatibleSilos) is { } placementHint)
            return Task.FromResult(placementHint);

        // Random selection from matching silos
        return Task.FromResult(compatibleSilos[Random.Shared.Next(compatibleSilos.Length)]);
    }
}
```

**Key Design Notes**:
- Uses `MembershipTableManager` to query membership entries, NOT `ISiloStatusOracle` metadata
- Extracts role from `target.GrainIdentity.Key.ToString()`, not from the placement strategy object
- Filters by `RoleName` property from membership table entries
- Intersects role-matching silos with compatible silos for version safety
- Checks placement hint AFTER role filtering (not before)

**Use Case**: Dedicated hardware pools (GPU silos, high-memory silos, etc.), regulatory compliance

---

#### 3.7 StatelessWorkerDirector (High-Throughput)

**File**: `src/Orleans.Runtime/Placement/StatelessWorkerDirector.cs:6-28`

```csharp
internal class StatelessWorkerDirector : IPlacementDirector
{
    public Task<SiloAddress> OnAddActivation(PlacementStrategy strategy, PlacementTarget target, IPlacementContext context)
    {
        var compatibleSilos = context.GetCompatibleSilos(target);

        // If the current silo is not shutting down, place locally if compatible
        if (!context.LocalSiloStatus.IsTerminating())
        {
            foreach (var silo in compatibleSilos)
            {
                if (silo.Equals(context.LocalSilo))
                {
                    return Task.FromResult(context.LocalSilo);
                }
            }
        }

        // otherwise, place somewhere else
        return Task.FromResult(compatibleSilos[Random.Shared.Next(compatibleSilos.Length)]);
    }
}
```

**Key Design Notes**:
- Does NOT inherit from `RandomPlacementDirector` (implements `IPlacementDirector` directly)
- Uses `context.LocalSiloStatus.IsTerminating()` instead of checking `== SiloStatus.Active`
- Uses explicit `foreach` loop with `silo.Equals(context.LocalSilo)` instead of `Contains()`
- No `ILocalSiloDetails` dependency - uses `context.LocalSilo` directly
- Does NOT check placement hint - always prefers local silo for stateless workers

**Use Case**: Stateless request processing, parallel fan-out, high-throughput workloads

**Special Property**: Multiple activations per grain ID allowed (controlled by `MaxLocalWorkers`)

---

#### 3.8 ClientObserversPlacementDirector (Client Observer Protection)

**File**: `src/Orleans.Runtime/Placement/ClientObserversPlacementDirector.cs:8-12`

```csharp
internal class ClientObserversPlacementDirector : IPlacementDirector
{
    public Task<SiloAddress> OnAddActivation(PlacementStrategy strategy, PlacementTarget target, IPlacementContext context)
        => throw new ClientNotAvailableException(target.GrainIdentity);
}
```

**Purpose**: Prevents placement of client observer activations on silos - always throws. Client observers must be activated on the client, not on a silo.

---

### 4. Placement Filtering System

#### IPlacementFilterDirector (Filter Interface)

**File**: `src/Orleans.Core/Placement/IPlacementFilterDirector.cs:1-12`

```csharp
public interface IPlacementFilterDirector
{
    IEnumerable<SiloAddress> Filter(PlacementFilterStrategy filterStrategy, PlacementTarget target, IEnumerable<SiloAddress> silos);
}
```

Note: The filter interface receives the `PlacementTarget` directly (not a separate `PlacementFilterContext` type). This gives filters access to the grain identity, interface type, interface version, and request context data.

#### PlacementFilterStrategyResolver

**File**: `src/Orleans.Runtime/Placement/Filtering/PlacementFilterStrategyResolver.cs:1-73`

Resolves which filters apply to each grain type:

```csharp
public sealed class PlacementFilterStrategyResolver
{
    private readonly ConcurrentDictionary<GrainType, PlacementFilterStrategy[]> _resolvedFilters = new();
    private readonly GrainPropertiesResolver _grainPropertiesResolver;
    private readonly IServiceProvider _services;

    public PlacementFilterStrategy[] GetPlacementFilterStrategies(GrainType grainType)
        => _resolvedFilters.GetOrAdd(grainType, _getFiltersInternal);

    private PlacementFilterStrategy[] GetPlacementFilterStrategyInternal(GrainType grainType)
    {
        _grainPropertiesResolver.TryGetGrainProperties(grainType, out var properties);

        if (properties is not null
            && properties.Properties.TryGetValue(WellKnownGrainTypeProperties.PlacementFilter, out var placementFilterIds)
            && !string.IsNullOrWhiteSpace(placementFilterIds))
        {
            var filterList = new List<PlacementFilterStrategy>();
            foreach (var filterId in placementFilterIds.Split(","))
            {
                // Resolve filter from DI container using keyed service
                var filter = _services.GetKeyedService<PlacementFilterStrategy>(filterId);
                if (filter is not null)
                {
                    filter.Initialize(properties);
                    filterList.Add(filter);
                }
                else
                {
                    throw new KeyNotFoundException($"Could not resolve placement filter strategy {filterId}...");
                }
            }

            // Sort by order property and validate uniqueness
            var orderedFilters = filterList.OrderBy(f => f.Order).ToArray();
            if (orderedFilters.Select(f => f.Order).Distinct().Count() != orderedFilters.Length)
            {
                throw new InvalidOperationException($"Placement filters for grain type {grainType} have duplicate order values...");
            }
            return orderedFilters;
        }

        return [];
    }
}
```

**Key Design Notes**:
- Resolves filters from **comma-separated filter IDs** stored in `WellKnownGrainTypeProperties.PlacementFilter`, not via prefix scanning of properties
- Uses **keyed DI services** (`GetKeyedService<PlacementFilterStrategy>(filterId)`) instead of `Type.GetType`/`Activator.CreateInstance`
- Validates that filter **order values are unique** (throws if duplicates)
- Returns empty array `[]` instead of `Array.Empty<PlacementFilterStrategy>()` (C# 12 collection expression)

**Pattern**: **Chain of Responsibility** - Ordered filter chain

#### RequiredMatchSiloMetadataPlacementFilterDirector

**File**: `src/Orleans.Runtime/Placement/Filtering/RequiredMatchSiloMetadataPlacementFilterDirector.cs:9-54`

```csharp
internal class RequiredMatchSiloMetadataPlacementFilterDirector(
    ILocalSiloDetails localSiloDetails,
    ISiloMetadataCache siloMetadataCache) : IPlacementFilterDirector
{
    public IEnumerable<SiloAddress> Filter(
        PlacementFilterStrategy filterStrategy,
        PlacementTarget target,
        IEnumerable<SiloAddress> silos)
    {
        var metadataKeys = (filterStrategy as RequiredMatchSiloMetadataPlacementFilterStrategy)?.MetadataKeys ?? [];

        if (metadataKeys.Length == 0)
            return silos;

        // Get LOCAL silo's metadata values for the required keys
        var localMetadata = siloMetadataCache.GetSiloMetadata(localSiloDetails.SiloAddress);
        var localRequiredMetadata = GetMetadata(localMetadata, metadataKeys);

        // Only include silos whose metadata matches the local silo's metadata for all required keys
        return silos.Where(silo =>
        {
            var remoteMetadata = siloMetadataCache.GetSiloMetadata(silo);
            return DoesMetadataMatch(localRequiredMetadata, remoteMetadata, metadataKeys);
        });
    }
}
```

**Key Design Notes**:
- Uses `ISiloMetadataCache` (not `ISiloStatusOracle.GetSiloMetadata()`)
- Matches remote silos' metadata against the **local silo's metadata** for specified keys - this means "require silos that have the SAME metadata values as me for these keys"
- Uses primary constructor syntax (C# 12)
- The filter strategy provides `MetadataKeys` (string array), not single key/value pairs

**Use Case**: Enforce placement constraints - e.g., only place on silos in the same region/zone as the requesting silo

#### PreferredMatchSiloMetadataPlacementFilterDirector

**File**: `src/Orleans.Runtime/Placement/Filtering/PreferredMatchSiloMetadataPlacementFilterDirector.cs:10-80`

```csharp
internal class PreferredMatchSiloMetadataPlacementFilterDirector(
    ILocalSiloDetails localSiloDetails,
    ISiloMetadataCache siloMetadataCache) : IPlacementFilterDirector
{
    public IEnumerable<SiloAddress> Filter(
        PlacementFilterStrategy filterStrategy,
        PlacementTarget target,
        IEnumerable<SiloAddress> silos)
    {
        var preferredMatchStrategy = filterStrategy as PreferredMatchSiloMetadataPlacementFilterStrategy;
        var minCandidates = preferredMatchStrategy?.MinCandidates ?? 1;
        var orderedMetadataKeys = preferredMatchStrategy?.OrderedMetadataKeys ?? [];

        var localSiloMetadata = siloMetadataCache.GetSiloMetadata(localSiloDetails.SiloAddress).Metadata;
        if (localSiloMetadata.Count == 0)
            return silos;

        var siloList = silos.ToList();
        if (siloList.Count <= minCandidates)
            return siloList;

        // Score each silo based on how many metadata keys match (from most to least important)
        // Keys are ordered: first = least important, last = most important
        // Matching stops at the first non-matching key (consecutive matching from the end)
        var maxScore = 0;
        var siloScores = new int[siloList.Count];
        var scoreCounts = new int[orderedMetadataKeys.Length + 1];
        for (var i = 0; i < siloList.Count; i++)
        {
            var siloMetadata = siloMetadataCache.GetSiloMetadata(siloList[i]).Metadata;
            var siloScore = 0;
            for (var j = orderedMetadataKeys.Length - 1; j >= 0; --j)
            {
                if (siloMetadata.TryGetValue(orderedMetadataKeys[j], out var siloMetadataValue) &&
                    localSiloMetadata.TryGetValue(orderedMetadataKeys[j], out var localSiloMetadataValue) &&
                    siloMetadataValue == localSiloMetadataValue)
                {
                    siloScore = ++siloScores[i];
                    maxScore = Math.Max(maxScore, siloScore);
                }
                else
                {
                    break;  // Stop scoring on first non-match
                }
            }
            scoreCounts[siloScore]++;
        }

        if (maxScore == 0)
            return siloList;

        // Find the score cutoff that gives at least minCandidates
        var candidateCount = 0;
        var scoreCutOff = orderedMetadataKeys.Length;
        for (var i = scoreCounts.Length - 1; i >= 0; i--)
        {
            candidateCount += scoreCounts[i];
            if (candidateCount >= minCandidates)
            {
                scoreCutOff = i;
                break;
            }
        }

        return siloList.Where((_, i) => siloScores[i] >= scoreCutOff);
    }
}
```

**Key Design Notes**:
- Significantly more sophisticated than a simple key/value match
- **Ordered metadata keys** with hierarchical matching (last = most important, first = least)
- **Consecutive matching from the most important key** - scoring breaks on first non-match
- **MinCandidates guarantee** - adjusts score cutoff to ensure at least N candidates remain
- Falls back to full silo list if no silos match any metadata keys or if already at/below minCandidates

**Use Case**: Hierarchical locality preferences (e.g., prefer same-rack > same-zone > same-region, with minimum candidate pool size)

---

### 5. Directory Service Integration

#### GrainLocator (Facade)

**File**: `src/Orleans.Runtime/GrainDirectory/GrainLocator.cs:1-50`

```csharp
internal class GrainLocator
{
    private readonly GrainLocatorResolver _grainLocatorResolver;

    public ValueTask<GrainAddress?> Lookup(GrainId grainId)
        => GetGrainLocator(grainId.Type).Lookup(grainId);

    public Task<GrainAddress?> Register(GrainAddress address, GrainAddress? previousRegistration)
        => GetGrainLocator(address.GrainId.Type).Register(address, previousRegistration);

    public Task Unregister(GrainAddress address, UnregistrationCause cause)
        => GetGrainLocator(address.GrainId.Type).Unregister(address, cause);

    public bool TryLookupInCache(GrainId grainId, out GrainAddress? address)
        => GetGrainLocator(grainId.Type).TryLookupInCache(grainId, out address);

    public void InvalidateCache(GrainId grainId)
        => GetGrainLocator(grainId.Type).InvalidateCache(grainId);

    public void InvalidateCache(GrainAddress address)
        => GetGrainLocator(address.GrainId.Type).InvalidateCache(address);

    public void UpdateCache(GrainId grainId, SiloAddress siloAddress)
        => GetGrainLocator(grainId.Type).UpdateCache(grainId, siloAddress);

    public void UpdateCache(GrainAddressCacheUpdate update)
    {
        if (update.ValidGrainAddress is { } validAddress)
        {
            UpdateCache(validAddress.GrainId, validAddress.SiloAddress);
        }
        else
        {
            InvalidateCache(update.InvalidGrainAddress);
        }
    }

    private IGrainLocator GetGrainLocator(GrainType grainType)
        => _grainLocatorResolver.GetGrainLocator(grainType);
}
```

**Pattern**: **Facade** - Simple interface hiding per-grain-type locator resolution

**Key Design Notes**:
- Has two `InvalidateCache` overloads: one by `GrainId`, one by `GrainAddress`
- `UpdateCache(GrainAddressCacheUpdate)` handles both cache updates and invalidations based on whether `ValidGrainAddress` is present

#### DhtGrainLocator (DHT-Based Directory)

**File**: `src/Orleans.Runtime/GrainDirectory/DhtGrainLocator.cs:1-153`

```csharp
internal class DhtGrainLocator : IGrainLocator
{
    private readonly ILocalGrainDirectory _localGrainDirectory;
    private readonly IGrainContext _grainContext;
    private readonly object _initLock = new();
    private BatchedDeregistrationWorker _forceWorker;
    private BatchedDeregistrationWorker _neaWorker;

    public async ValueTask<GrainAddress> Lookup(GrainId grainId)
        => (await _localGrainDirectory.LookupAsync(grainId)).Address;

    public async Task<GrainAddress> Register(GrainAddress address, GrainAddress previousAddress)
        => (await _localGrainDirectory.RegisterAsync(address, currentRegistration: previousAddress)).Address;

    public Task Unregister(GrainAddress address, UnregistrationCause cause)
    {
        EnsureInitialized();

        var worker = cause switch
        {
            UnregistrationCause.Force => _forceWorker,
            UnregistrationCause.NonexistentActivation => _neaWorker,
            _ => throw new ArgumentOutOfRangeException(...)
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
            var tcs = new TaskCompletionSource<bool>(TaskCreationOptions.RunContinuationsAsynchronously);
            _queue.Writer.TryWrite((tcs, address));
            return tcs.Task;
        }

        private async Task ProcessDeregistrationQueue()
        {
            var operations = new List<TaskCompletionSource<bool>>();
            var addresses = new List<GrainAddress>();

            while (await _queue.Reader.WaitToReadAsync())
            {
                try
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
                catch (Exception ex)
                {
                    foreach (var op in operations)
                        op.TrySetException(ex);
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
4. **Lazy initialization** - Workers created lazily via `EnsureInitialized()` with double-check locking (required because SystemTargets are not valid until registered with Catalog)
5. **Error propagation** - Batch failures propagate exceptions to all waiting callers

#### LocalGrainDirectory (Consistent Hash Ring)

**File**: `src/Orleans.Runtime/GrainDirectory/LocalGrainDirectory.cs` (relevant excerpts)

**Partition calculation** (lines 295-365):

```csharp
internal sealed partial class LocalGrainDirectory : ILocalGrainDirectory
{
    internal IGrainDirectoryCache DirectoryCache { get; }
    internal LocalGrainDirectoryPartition DirectoryPartition { get; }

    // Calculate which silo owns directory partition for this grain
    public SiloAddress? CalculateGrainDirectoryPartition(GrainId grainId)
    {
        // System targets are always owned by the local silo
        if (grainId.IsSystemTarget())
            return MyAddress;

        int hash = unchecked((int)grainId.GetUniformHashCode());
        bool excludeMySelf = !Running;

        var existing = this.directoryMembership;
        if (existing.MembershipRingList.Count == 0)
            return !Running ? null : MyAddress;

        // Linear search in ring (traverses from end)
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
}
```

**Lookup with caching** (lines 645-713):

```csharp
    public async Task<AddressAndTag> LookupAsync(GrainId grainId, int hopCount = 0)
    {
        var forwardAddress = CheckIfShouldForward(grainId, hopCount, "LookUpAsync");

        // On non-first hops, retry with delay to let membership settle
        if (hopCount > 0 && forwardAddress != null)
        {
            await Task.Delay(RETRY_DELAY);
            forwardAddress = CheckIfShouldForward(grainId, hopCount, "LookUpAsync");
        }

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
```

**Pattern**: **Consistent Hashing** with **Forwarding** and **Caching**

**Key Design Notes**:
- Uses linear traversal of the ring (comment mentions binary search as future optimization)
- `HOP_LIMIT = 6` prevents infinite forwarding loops
- `RETRY_DELAY = 200ms` on non-first hops allows membership to settle
- System targets (system grains) bypass the ring and are always local

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
- Lock contention on message enqueue (mitigated by per-worker distribution)

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
    void SiloStatisticsChangeNotification(SiloAddress updatedSilo, SiloRuntimeStatistics newStats);
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

### Adopt These Patterns

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

3. **Multi-Level Caching**: Message cache -> actor locator cache -> directory cache
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

### Adapt These Patterns

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

### Avoid These Patterns

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
    | (choose target silo)
Directory: "Where is this existing activation?"
    | (register/lookup/cache)
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
3. **8 Built-in Directors** - Random, PreferLocal, Hash, ActivationCount (Power of Two), ResourceOptimized (weighted scoring), SiloRole, StatelessWorker, ClientObservers
4. **Placement Filtering** - Ordered chain of metadata-based filters (Required/Preferred) with hierarchical matching
5. **Directory Integration** - Tight coupling with grain directory for lookup/registration
6. **Load Awareness** - Directors subscribe to silo statistics for intelligent placement
7. **Multi-Level Caching** - Message cache -> locator cache -> directory cache

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
- `src/Orleans.Runtime/Placement/PlacementService.cs:1-443` - Central orchestrator with 16-worker pool
- `src/Orleans.Runtime/Placement/PlacementStrategyResolver.cs:1-88` - Strategy resolution by grain type
- `src/Orleans.Runtime/Placement/PlacementDirectorResolver.cs` - Director resolution via DI

**Interfaces**:
- `src/Orleans.Core/Placement/IPlacementDirector.cs:1-47` - Core placement director interface
- `src/Orleans.Core/Placement/IPlacementContext.cs:1-38` - Context provider interface
- `src/Orleans.Core/Placement/PlacementTarget.cs:1-46` - Placement input structure
- `src/Orleans.Core/Placement/IPlacementFilterDirector.cs:1-12` - Filter director interface

**Directors**:
- `src/Orleans.Runtime/Placement/RandomPlacementDirector.cs:6-22` - Simple random selection
- `src/Orleans.Runtime/Placement/PreferLocalPlacementDirector.cs:14-29` - Locality optimization
- `src/Orleans.Runtime/Placement/HashBasedPlacementDirector.cs:6-24` - Deterministic placement (with sorting)
- `src/Orleans.Runtime/Placement/ActivationCountPlacementDirector.cs:13-127` - Power of Two Choices
- `src/Orleans.Runtime/Placement/ResourceOptimizedPlacementDirector.cs:14-239` - Weighted multi-dimensional scoring
- `src/Orleans.Runtime/Placement/SiloRoleBasedPlacementDirector.cs:8-42` - Role-based filtering via membership table
- `src/Orleans.Runtime/Placement/StatelessWorkerDirector.cs:6-28` - High-throughput local placement
- `src/Orleans.Runtime/Placement/ClientObserversPlacementDirector.cs:8-12` - Client observer protection

**Filtering**:
- `src/Orleans.Runtime/Placement/Filtering/PlacementFilterStrategyResolver.cs:1-73` - Filter resolution via keyed DI services
- `src/Orleans.Runtime/Placement/Filtering/RequiredMatchSiloMetadataPlacementFilterDirector.cs:9-54` - Hard constraints (match local silo metadata)
- `src/Orleans.Runtime/Placement/Filtering/PreferredMatchSiloMetadataPlacementFilterDirector.cs:10-80` - Soft preferences (hierarchical scoring with MinCandidates)

**Statistics**:
- `src/Orleans.Runtime/Placement/ISiloStatisticsChangeListener.cs:1-14` - Statistics change listener interface
- `src/Orleans.Runtime/Placement/DeploymentLoadPublisher.cs` - Statistics aggregation and broadcasting

**Directory Integration**:
- `src/Orleans.Runtime/GrainDirectory/GrainLocator.cs:1-50` - Locator facade
- `src/Orleans.Runtime/GrainDirectory/DhtGrainLocator.cs:1-153` - DHT-based directory with batching
- `src/Orleans.Runtime/GrainDirectory/LocalGrainDirectory.cs:295-365` - Consistent hash partition calculation
- `src/Orleans.Runtime/GrainDirectory/LocalGrainDirectory.cs:645-713` - Lookup with forwarding and caching

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
