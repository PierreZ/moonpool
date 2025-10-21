# Membership Service and Failure Detection

## Reference Files
- `ClusterMembershipSnapshot.cs:1-132` - Immutable cluster membership snapshot
- `ClusterMember.cs:1-66` - Individual cluster member representation
- `ClusterMembershipService.cs:1-134` - Membership service facade
- `MembershipTableManager.cs:1-993` - Storage orchestration and lifecycle
- `MembershipAgent.cs:1-420` - Local silo status management
- `SiloHealthMonitor.cs:1-448` - Per-silo failure detection
- `ClusterHealthMonitor.cs:1-414` - Cluster-wide monitoring orchestration
- `SiloStatusOracle.cs:1-139` - Read-only membership view
- `MembershipGossiper.cs:1-33` - Gossip protocol implementation
- `ISiloStatusOracle.cs:1-83` - Status oracle interface

## Overview

Orleans implements a **distributed membership service** with sophisticated failure detection. The system ensures:
- **Eventually consistent cluster view** via shared storage + gossip
- **Vote-based failure detection** to prevent false positives
- **Multi-level probing** (direct + indirect) for partition tolerance
- **Optimistic concurrency** with ETags to handle write conflicts
- **Immutable snapshots** for consistent reads

The architecture separates concerns:
- **ClusterMembershipSnapshot** - Immutable view of cluster state at a version
- **MembershipTableManager** - Orchestrates storage reads/writes, owns lifecycle
- **MembershipAgent** - Manages local silo status transitions (Joining → Active → Dead)
- **SiloHealthMonitor** - Monitors one remote silo via periodic probes
- **ClusterHealthMonitor** - Decides which silos to monitor, manages SiloHealthMonitors
- **SiloStatusOracle** - Provides read-only view of cluster membership to other components
- **MembershipGossiper** - Propagates changes quickly via gossip (reduces latency)

---

## Part 1: Core Data Structures

### ClusterMembershipSnapshot: Immutable Cluster State

**Code**: `ClusterMembershipSnapshot.cs:10-38`

```csharp
[Serializable, GenerateSerializer, Immutable]
public sealed class ClusterMembershipSnapshot
{
    public ClusterMembershipSnapshot(
        ImmutableDictionary<SiloAddress, ClusterMember> members,
        MembershipVersion version)
    {
        this.Members = members;
        this.Version = version;
    }

    [Id(0)]
    public ImmutableDictionary<SiloAddress, ClusterMember> Members { get; }

    [Id(1)]
    public MembershipVersion Version { get; }
}
```

**Key Characteristics**:

1. **Immutable** (line 10)
   - Created once, never modified
   - Safe to share across threads
   - Enables structural sharing (cheap copies)

2. **Versioned** (line 37-38)
   - `MembershipVersion` - monotonically increasing
   - Used to order snapshots and detect updates
   - Prevents applying stale snapshots

3. **Dictionary-based** (line 31)
   - `SiloAddress → ClusterMember` mapping
   - Fast O(1) lookup by silo address
   - `ImmutableDictionary` for thread-safety

**GetSiloStatus** (`ClusterMembershipSnapshot.cs:45-61`):
```csharp
public SiloStatus GetSiloStatus(SiloAddress silo)
{
    var status = this.Members.TryGetValue(silo, out var entry) ? entry.Status : SiloStatus.None;

    // If not found, check if there's a newer instance of this logical silo
    if (status == SiloStatus.None)
    {
        foreach (var member in this.Members)
        {
            if (member.Key.IsSuccessorOf(silo))  // Same endpoint, newer generation
            {
                status = SiloStatus.Dead;
                break;
            }
        }
    }

    return status;
}
```

**Pattern**: If silo not in table, but a newer generation exists at same endpoint → consider old one Dead

**CreateUpdate** (`ClusterMembershipSnapshot.cs:73-106`):
```csharp
public ClusterMembershipUpdate CreateUpdate(ClusterMembershipSnapshot previous)
{
    var changes = ImmutableHashSet.CreateBuilder<ClusterMember>();

    foreach (var entry in this.Members)
    {
        // Include any entry which is new or has changed state
        if (!previous.Members.TryGetValue(entry.Key, out var previousEntry)
            || previousEntry.Status != entry.Value.Status)
        {
            changes.Add(entry.Value);
        }
    }

    // Handle entries which were removed entirely
    foreach (var entry in previous.Members)
    {
        if (!this.Members.TryGetValue(entry.Key, out _))
        {
            changes.Add(new ClusterMember(entry.Key, SiloStatus.Dead, entry.Value.Name));
        }
    }

    return new ClusterMembershipUpdate(this, changes.ToImmutableArray());
}
```

**Pattern**: Compute delta between snapshots (only changed/new/removed entries)

### ClusterMember: Individual Member Representation

**Code**: `ClusterMember.cs:8-28`

```csharp
[Serializable, GenerateSerializer, Immutable]
public sealed class ClusterMember : IEquatable<ClusterMember>
{
    public ClusterMember(SiloAddress siloAddress, SiloStatus status, string name)
    {
        this.SiloAddress = siloAddress ?? throw new ArgumentNullException(nameof(siloAddress));
        this.Status = status;
        this.Name = name;
    }

    [Id(0)]
    public SiloAddress SiloAddress { get; }

    [Id(1)]
    public SiloStatus Status { get; }

    [Id(2)]
    public string Name { get; }
}
```

**Simple value type**:
- **SiloAddress** - Unique identifier (endpoint + generation)
- **Status** - `Created | Joining | Active | ShuttingDown | Stopping | Dead`
- **Name** - Human-readable name (persists across restarts)

**Equality** (lines 55-58):
- Based on `SiloAddress`, `Status`, and `Name`
- Enables snapshot diffing
- HashCode based only on `SiloAddress` (line 61)

---

## Part 2: MembershipTableManager - Storage Orchestration

### Purpose

MembershipTableManager is the **authoritative source** for membership state. It:
- Reads/writes to shared storage (IMembershipTable abstraction)
- Publishes snapshots to subscribers via `AsyncEnumerable`
- Manages local silo status transitions
- Coordinates voting for failure detection
- Handles stale entry cleanup (older generations of same silo)

**Code**: `MembershipTableManager.cs:18-83`

### Snapshot Publishing Pattern

**Code**: `MembershipTableManager.cs:39, 73-76`

```csharp
private readonly AsyncEnumerable<MembershipTableSnapshot> updates;

this.updates = new AsyncEnumerable<MembershipTableSnapshot>(
    initialValue: this.snapshot,
    updateValidator: (previous, proposed) => proposed.IsSuccessorTo(previous),
    onPublished: update => Interlocked.Exchange(ref this.snapshot, update));
```

**Pattern**: Publish-subscribe with version monotonicity enforcement

- **updateValidator**: Ensures versions always increase (prevents stale updates)
- **onPublished**: Atomic snapshot replacement
- **AsyncEnumerable**: Subscribers receive stream of updates

**Usage** (`MembershipTableManager.cs:89`):
```csharp
public IAsyncEnumerable<MembershipTableSnapshot> MembershipTableUpdates => this.updates;
```

Subscribers use `await foreach` to process updates:
```csharp
await foreach (var snapshot in membershipTableManager.MembershipTableUpdates)
{
    // React to membership change
}
```

### Lifecycle Integration

**Code**: `MembershipTableManager.cs:958-984`

```csharp
void ILifecycleParticipant<ISiloLifecycle>.Participate(ISiloLifecycle lifecycle)
{
    lifecycle.Subscribe(
        nameof(MembershipTableManager),
        ServiceLifecycleStage.RuntimeGrainServices,  // Early stage
        OnRuntimeGrainServicesStart,
        OnRuntimeGrainServicesStop);

    async Task OnRuntimeGrainServicesStart(CancellationToken ct)
    {
        await Task.Run(() => this.Start());
        tasks.Add(Task.Run(() => this.PeriodicallyRefreshMembershipTable()));
    }
}
```

**Two background tasks**:
1. **Periodic refresh** (`MembershipTableManager.cs:250-293`):
   - Polls storage every `TableRefreshTimeout` (default 60s)
   - Detects changes made by other silos
   - Publishes new snapshots to subscribers
   - Exponential backoff on failure

2. **Suspect/kill processing** (`MembershipTableManager.cs:687-725`):
   - Processes failure detection votes from `ClusterHealthMonitor`
   - Bounded channel with DropOldest strategy
   - Retries with exponential backoff on write conflicts

### Optimistic Concurrency with ETags

**Code**: `MembershipTableManager.cs:407-456`

```csharp
private async Task<bool> TryUpdateMyStatusGlobalOnce(SiloStatus newStatus)
{
    var table = await membershipTableProvider.ReadAll();
    var (myEntry, myEtag) = this.GetOrCreateLocalSiloEntry(table, newStatus);

    // Check if I'm already marked dead
    if (myEntry.Status == SiloStatus.Dead && myEntry.Status != newStatus)
    {
        this.KillMyselfLocally("I should be Dead according to membership table");
        return true;
    }

    myEntry.Status = newStatus;
    myEntry.IAmAliveTime = GetDateTimeUtcNow();

    bool ok;
    TableVersion next = table.Version.Next();
    if (myEtag != null)
    {
        ok = await membershipTableProvider.UpdateRow(myEntry, myEtag, next);
    }
    else
    {
        ok = await membershipTableProvider.InsertRow(myEntry, next);
    }

    if (ok)
    {
        this.CurrentStatus = newStatus;
        // ... publish updated snapshot
    }

    return ok;  // false = write conflict, caller will retry
}
```

**Pattern**: Read → Modify → Write with ETag check

- **ETag** - Opaque version token per row (from storage)
- **UpdateRow fails** if ETag changed (someone else wrote)
- **Retry loop** (`MembershipTableManager.cs:349`) with exponential backoff
- **Unlimited retries** for contention (constant `NUM_CONDITIONAL_WRITE_CONTENTION_ATTEMPTS = -1`)

### Voting-Based Failure Detection

**Code**: `MembershipTableManager.cs:784-921`

```csharp
private async Task<bool> InnerTryToSuspectOrKill(
    SiloAddress silo,
    SiloAddress indirectProbingSilo,
    CancellationToken cancellationToken)
{
    var table = await membershipTableProvider.ReadAll().WaitAsync(cancellationToken);
    var now = GetDateTimeUtcNow();

    var (entry, eTag) = ...;

    // Check if already dead
    if (entry.Status == SiloStatus.Dead)
    {
        this.ProcessTableUpdate(table, "TrySuspectOrKill");
        return true;
    }

    // Get all valid (non-expired) votes
    var freshVotes = entry.GetFreshVotes(now, this.clusterMembershipOptions.DeathVoteExpirationTimeout);

    // Add my vote to the list
    entry.AddOrUpdateSuspector(myAddress, now, clusterMembershipOptions.NumVotesForDeathDeclaration);

    // Include the indirect probe silo's vote as well, if it exists
    if (indirectProbingSilo is not null)
    {
        entry.AddOrUpdateSuspector(indirectProbingSilo, now, clusterMembershipOptions.NumVotesForDeathDeclaration);
    }

    freshVotes = entry.GetFreshVotes(now, this.clusterMembershipOptions.DeathVoteExpirationTimeout);

    // Determine if there are enough votes to evict
    int activeNonStaleSilos = table.Members.Count(kv =>
        kv.Item1.Status == SiloStatus.Active &&
        !kv.Item1.HasMissedIAmAlives(clusterMembershipOptions, now));

    var numVotesRequiredToEvict = Math.Min(
        clusterMembershipOptions.NumVotesForDeathDeclaration,
        (activeNonStaleSilos + 1) / 2);  // Majority vote

    if (freshVotes.Count >= numVotesRequiredToEvict)
    {
        log.LogInformation("Evicting '{Silo}'. Fresh votes: {Count}, required: {Required}", ...);
        return await DeclareDead(entry, eTag, table.Version, now);
    }

    // Not enough votes yet, just add my vote
    log.LogInformation("Voting to evict '{Silo}'. Fresh votes: {Count}", ...);
    var ok = await membershipTableProvider.UpdateRow(entry, eTag, table.Version.Next());

    if (ok)
    {
        GossipToOthers(localSiloEntry.SiloAddress, localSiloEntry.Status).Ignore();
    }

    return ok;
}
```

**Voting Rules**:
1. **Vote expiration** (default 2 minutes): Old votes ignored
2. **Quorum**: `min(NumVotesForDeathDeclaration, (activeNonStaleSilos + 1) / 2)`
   - Default `NumVotesForDeathDeclaration = 2`
   - Small clusters: One vote enough (majority of 1 or 2)
   - Large clusters: Need configured minimum (default 2)
3. **Indirect probes contribute**: Intermediary's vote counts
4. **Write conflicts**: Retry, may find silo already declared dead

**DeclareDead** (`MembershipTableManager.cs:923-954`):
```csharp
private async Task<bool> DeclareDead(MembershipEntry entry, string etag, TableVersion tableVersion, DateTime time)
{
    entry = entry.Copy();
    entry.AddSuspector(myAddress, time);  // Add killer to suspect list for diagnostics
    entry.Status = SiloStatus.Dead;

    bool ok = await membershipTableProvider.UpdateRow(entry, etag, tableVersion.Next());

    if (ok)
    {
        var table = await membershipTableProvider.ReadAll();
        this.ProcessTableUpdate(table, "DeclareDead");
        GossipToOthers(entry.SiloAddress, entry.Status).Ignore();
        return true;
    }

    log.LogInformation("Failed to update {Silo} to Dead due to write conflicts. Will retry.");
    return false;
}
```

**Pattern**: Mark dead → Gossip immediately (don't wait for refresh)

### IAmAlive Heartbeats

**Code**: `MembershipTableManager.cs:199-208`

```csharp
public async Task UpdateIAmAlive()
{
    var entry = new MembershipEntry
    {
        SiloAddress = myAddress,
        IAmAliveTime = GetDateTimeUtcNow()
    };

    await this.membershipTableProvider.UpdateIAmAlive(entry);
}
```

**Called by**: `MembershipAgent.UpdateIAmAlive()` (periodic timer)

**Purpose**:
- Prove liveness to other silos
- Other silos check `HasMissedIAmAlives(options, now)` before declaring dead
- Cheap operation (single field update, may not require ETag check depending on storage)

### Gossip Protocol

**Code**: `MembershipTableManager.cs:616-645`

```csharp
private async Task GossipToOthers(SiloAddress updatedSilo, SiloStatus updatedStatus)
{
    if (!this.clusterMembershipOptions.UseLivenessGossip) return;

    var now = GetDateTimeUtcNow();
    var gossipPartners = new List<SiloAddress>();

    foreach (var item in this.MembershipTableSnapshot.Entries)
    {
        var entry = item.Value;
        if (entry.SiloAddress.IsSameLogicalSilo(this.myAddress)) continue;
        if (!IsFunctionalForMembership(entry.Status)) continue;
        if (entry.HasMissedIAmAlives(this.clusterMembershipOptions, now)) continue;

        gossipPartners.Add(entry.SiloAddress);

        bool IsFunctionalForMembership(SiloStatus status)
        {
            return status == SiloStatus.Active || status == SiloStatus.ShuttingDown || status == SiloStatus.Stopping;
        }
    }

    try
    {
        await this.gossiper.GossipToRemoteSilos(
            gossipPartners,
            MembershipTableSnapshot,
            updatedSilo,
            updatedStatus);
    }
    catch (Exception exception)
    {
        this.log.LogWarning(exception, "Error while gossiping status to other silos");
    }
}
```

**Gossip targets**:
- All `Active | ShuttingDown | Stopping` silos
- Exclude silos that haven't updated IAmAlive recently
- Send current full snapshot + which silo changed

**RefreshFromSnapshot** (`MembershipTableManager.cs:108-135`):
```csharp
public async Task RefreshFromSnapshot(MembershipTableSnapshot snapshot)
{
    if (snapshot.Version == MembershipVersion.MinValue)
        throw new ArgumentException("Cannot call RefreshFromSnapshot with Version == MembershipVersion.MinValue");

    this.log.LogInformation("Received cluster membership snapshot via gossip: {Snapshot}", snapshot);

    if (snapshot.Entries.TryGetValue(this.myAddress, out var localSiloEntry))
    {
        if (localSiloEntry.Status == SiloStatus.Dead && this.CurrentStatus != SiloStatus.Dead)
        {
            this.log.LogWarning("I should be Dead according to membership table (in RefreshFromSnapshot)");
            this.KillMyselfLocally("...");
        }
    }

    this.updates.TryPublish(MembershipTableSnapshot.Update, snapshot);
}
```

**Pattern**: Gossiped snapshot applied immediately (low latency), but still validate via storage later

---

## Part 3: MembershipAgent - Local Silo Lifecycle

### Purpose

MembershipAgent manages the **local silo's status** through lifecycle stages:
- **Joining** → **Active** → **ShuttingDown/Stopping** → **Dead**

**Code**: `MembershipAgent.cs:16-49`

### Lifecycle Stages and Status Transitions

**Code**: `MembershipAgent.cs:333-411`

```csharp
void ILifecycleParticipant<ISiloLifecycle>.Participate(ISiloLifecycle lifecycle)
{
    // Stage: RuntimeInitialize + 1 (before outbound queue closes)
    lifecycle.Subscribe(
        nameof(MembershipAgent),
        ServiceLifecycleStage.RuntimeInitialize + 1,
        OnRuntimeInitializeStart: _ => Task.CompletedTask,
        OnRuntimeInitializeStop: async ct => await BecomeDead());

    // Stage: AfterRuntimeGrainServices
    lifecycle.Subscribe(
        nameof(MembershipAgent),
        ServiceLifecycleStage.AfterRuntimeGrainServices,
        AfterRuntimeGrainServicesStart: async ct => await BecomeJoining(),
        AfterRuntimeGrainServicesStop: _ => Task.CompletedTask);

    // Stage: BecomeActive
    lifecycle.Subscribe(
        nameof(MembershipAgent),
        ServiceLifecycleStage.BecomeActive,
        OnBecomeActiveStart: async ct =>
        {
            await BecomeActive();
            tasks.Add(Task.Run(() => this.UpdateIAmAlive()));
        },
        OnBecomeActiveStop: async ct =>
        {
            if (ct.IsCancellationRequested)
            {
                await BecomeStopping();
            }
            else
            {
                // Graceful shutdown
                var gracePeriod = Task.WhenAll(
                    Task.Delay(ClusterMembershipOptions.ClusteringShutdownGracePeriod),
                    ct.WhenCancelled());
                var task = await Task.WhenAny(gracePeriod, this.BecomeShuttingDown());
                if (ReferenceEquals(task, gracePeriod))
                {
                    this.log.LogWarning("Graceful shutdown aborted: starting ungraceful shutdown");
                    await BecomeStopping();
                }
            }
        });
}
```

**Lifecycle Flow**:

1. **AfterRuntimeGrainServices → Joining**
   - Infrastructure ready, but not accepting traffic
   - Register in membership table
   - Validate connectivity to existing Active silos

2. **BecomeActive → Active**
   - Validate connectivity to all active silos (via probes)
   - Update status to Active
   - Start IAmAlive heartbeat timer

3. **OnActiveStop → ShuttingDown/Stopping**
   - **Graceful** (`!ct.IsCancellationRequested`): `ShuttingDown`
   - **Forced** (`ct.IsCancellationRequested`): `Stopping`
   - Grace period: `ClusteringShutdownGracePeriod` (default 10 seconds)

4. **OnRuntimeInitializeStop → Dead**
   - Final status update
   - Other silos will see this silo as dead

### ValidateInitialConnectivity

**Code**: `MembershipAgent.cs:129-250`

```csharp
private async Task ValidateInitialConnectivity()
{
    var maxAttemptTime = this.clusterMembershipOptions.MaxJoinAttemptTime;
    var attemptNumber = 1;
    var attemptUntil = this.getUtcDateTime() + maxAttemptTime;

    while (true)
    {
        var activeSilos = new List<SiloAddress>();
        foreach (var item in this.tableManager.MembershipTableSnapshot.Entries)
        {
            var entry = item.Value;
            if (entry.Status != SiloStatus.Active) continue;
            if (entry.SiloAddress.IsSameLogicalSilo(this.localSilo.SiloAddress)) continue;
            if (entry.HasMissedIAmAlives(this.clusterMembershipOptions, now) != default) continue;

            activeSilos.Add(entry.SiloAddress);
        }

        var failedSilos = await CheckClusterConnectivity(activeSilos.ToArray());

        // If there were no failures, terminate the loop
        if (failedSilos.Count == 0) break;

        this.log.LogError(
            "Failed to get ping responses from {FailedCount} of {ActiveCount} active silos. "
            + "Will continue attempting to validate connectivity until {Timeout}. Attempt #{Attempt}",
            failedSilos.Count, activeSilos.Count, attemptUntil, attemptNumber);

        if (now + TimeSpan.FromSeconds(5) > attemptUntil)
        {
            throw new OrleansClusterConnectivityCheckFailedException("Failed to validate connectivity");
        }

        await Task.Delay(TimeSpan.FromSeconds(5));
        await this.tableManager.Refresh();
        ++attemptNumber;
    }

    async Task<List<SiloAddress>> CheckClusterConnectivity(SiloAddress[] members)
    {
        var tasks = new List<Task<bool>>(members.Length);
        var timeout = this.clusterMembershipOptions.ProbeTimeout;

        foreach (var silo in members)
        {
            tasks.Add(ProbeSilo(this.siloProber, silo, timeout, this.log));
        }

        await Task.WhenAll(tasks);

        var failed = new List<SiloAddress>();
        for (var i = 0; i < tasks.Count; i++)
        {
            if (tasks[i].Status != TaskStatus.RanToCompletion || !tasks[i].GetAwaiter().GetResult())
            {
                failed.Add(members[i]);
            }
        }

        return failed;
    }
}
```

**Pattern**: Probe all active silos before transitioning to Active

**Why?** Prevents adding silos that can't communicate (network issues, firewall)

**Retry behavior**:
- Keep trying until `MaxJoinAttemptTime` (default 5 minutes)
- Refresh membership every 5 seconds (detect new active silos)
- Fail fast if timeout exceeded

### UpdateIAmAlive Timer

**Code**: `MembershipAgent.cs:60-102`

```csharp
private async Task UpdateIAmAlive()
{
    TimeSpan? overrideDelayPeriod = RandomTimeSpan.Next(this.clusterMembershipOptions.IAmAliveTablePublishTimeout);
    var exponentialBackoff = new ExponentialBackoff(EXP_BACKOFF_CONTENTION_MIN, EXP_BACKOFF_CONTENTION_MAX, EXP_BACKOFF_STEP);
    var runningFailures = 0;

    while (await this.iAmAliveTimer.NextTick(overrideDelayPeriod) && !this.tableManager.CurrentStatus.IsTerminating())
    {
        try
        {
            await this.tableManager.UpdateIAmAlive();
            overrideDelayPeriod = default;
            runningFailures = 0;
        }
        catch (Exception exception)
        {
            runningFailures += 1;
            this.log.LogWarning(exception, "Failed to update IAmAlive, will retry shortly");
            overrideDelayPeriod = exponentialBackoff.Next(runningFailures);
        }
    }
}
```

**Pattern**: Periodic timer with jitter and exponential backoff on failure

- **Initial jitter**: Random delay up to `IAmAliveTablePublishTimeout` (prevents thundering herd)
- **Period**: `IAmAliveTablePublishTimeout` (default 5 seconds)
- **Backoff on failure**: Exponential (min 200ms, max 2min)
- **Stop when**: Silo status becomes terminating

---

## Part 4: SiloHealthMonitor - Per-Silo Failure Detection

### Purpose

SiloHealthMonitor is responsible for **monitoring one specific remote silo** via periodic probing. It:
- Sends periodic pings (probes)
- Tracks consecutive failures
- Supports **direct** and **indirect** probing
- Reports probe results to `ClusterHealthMonitor`

**Code**: `SiloHealthMonitor.cs:20-78`

### Direct Probing

**Code**: `SiloHealthMonitor.cs:256-321`

```csharp
private async Task<ProbeResult> ProbeDirectly(CancellationToken cancellation)
{
    var id = ++_nextProbeId;

    var roundTripTimer = ValueStopwatch.StartNew();
    ProbeResult probeResult;
    Exception? failureException;

    try
    {
        await _prober.Probe(TargetSiloAddress, id, cancellation).WaitAsync(cancellation);
        failureException = null;
    }
    catch (OperationCanceledException exception)
    {
        failureException = new OperationCanceledException($"Cancelled after {roundTripTimer.Elapsed}");
    }
    catch (Exception exception)
    {
        failureException = exception;
    }
    finally
    {
        roundTripTimer.Stop();
    }

    if (failureException is null)
    {
        MessagingInstruments.OnPingReplyReceived(TargetSiloAddress);

        _failedProbes = 0;
        _elapsedSinceLastSuccessfulResponse.Restart();
        LastRoundTripTime = roundTripTimer.Elapsed;
        probeResult = ProbeResult.CreateDirect(0, ProbeResultStatus.Succeeded);
    }
    else
    {
        MessagingInstruments.OnPingReplyMissed(TargetSiloAddress);

        var failedProbes = ++_failedProbes;
        _log.LogWarning(
            failureException,
            "Did not get response for probe #{Id} to {Silo} after {Elapsed}. Consecutive failures: {Count}",
            id, TargetSiloAddress, roundTripTimer.Elapsed, failedProbes);

        probeResult = ProbeResult.CreateDirect(failedProbes, ProbeResultStatus.Failed);
    }

    return probeResult;
}
```

**Pattern**: Try → Catch → Reset or increment counter → Return result

**Key Metrics**:
- `_failedProbes`: Consecutive failures (reset on success)
- `_elapsedSinceLastSuccessfulResponse`: Time since last success
- `LastRoundTripTime`: RTT of last successful probe

### Indirect Probing

**Code**: `SiloHealthMonitor.cs:330-404`

```csharp
private async Task<ProbeResult> ProbeIndirectly(
    SiloAddress intermediary,
    TimeSpan directProbeTimeout,
    CancellationToken cancellation)
{
    var id = ++_nextProbeId;

    var roundTripTimer = ValueStopwatch.StartNew();
    ProbeResult probeResult;

    try
    {
        var indirectResult = await _prober.ProbeIndirectly(
            intermediary,
            TargetSiloAddress,
            directProbeTimeout,
            id,
            cancellation).WaitAsync(cancellation);

        roundTripTimer.Stop();
        var roundTripTime = roundTripTimer.Elapsed - indirectResult.ProbeResponseTime;

        // Record timing regardless of the result
        _elapsedSinceLastSuccessfulResponse.Restart();
        LastRoundTripTime = roundTripTime;

        if (indirectResult.Succeeded)
        {
            _log.LogInformation(
                "Indirect probe #{Id} to {Silo} via {Intermediary} succeeded after {RTT} (direct: {DirectRTT})",
                id, TargetSiloAddress, intermediary, roundTripTimer.Elapsed, indirectResult.ProbeResponseTime);

            MessagingInstruments.OnPingReplyReceived(TargetSiloAddress);
            _failedProbes = 0;
            probeResult = ProbeResult.CreateIndirect(0, ProbeResultStatus.Succeeded, indirectResult, intermediary);
        }
        else
        {
            MessagingInstruments.OnPingReplyMissed(TargetSiloAddress);

            // Check if intermediary is healthy
            if (indirectResult.IntermediaryHealthScore > 0)
            {
                _log.LogInformation(
                    "Ignoring failure for probe #{Id} since intermediary is not healthy. Score: {Score}",
                    id, indirectResult.IntermediaryHealthScore);
                probeResult = ProbeResult.CreateIndirect(_failedProbes, ProbeResultStatus.Unknown, indirectResult, intermediary);
            }
            else
            {
                _log.LogWarning(
                    "Indirect probe #{Id} to {Silo} via {Intermediary} failed. Message: {Message}",
                    id, TargetSiloAddress, intermediary, indirectResult.FailureMessage);

                var missed = ++_failedProbes;
                probeResult = ProbeResult.CreateIndirect(missed, ProbeResultStatus.Failed, indirectResult, intermediary);
            }
        }
    }
    catch (Exception exception)
    {
        MessagingInstruments.OnPingReplyMissed(TargetSiloAddress);
        _log.LogWarning(exception, "Indirect probe request failed.");
        probeResult = ProbeResult.CreateIndirect(_failedProbes, ProbeResultStatus.Unknown, default, intermediary);
    }

    return probeResult;
}
```

**Indirect Probe Flow**:
1. Ask `intermediary` to probe `TargetSiloAddress`
2. Intermediary probes target with `directProbeTimeout`
3. Intermediary responds with:
   - `Succeeded` - Target responded
   - `Failed` + `FailureMessage` - Target didn't respond
   - `IntermediaryHealthScore` - How degraded is intermediary?
4. If intermediary unhealthy, ignore result (status = Unknown)

**Why indirect probing?**
- Detects network partitions
- Distinguishes target failure from local network issues
- If indirect probe succeeds, local network to target is partitioned

### Probe Timing and Adaptive Timeout

**Code**: `SiloHealthMonitor.cs:155-249`

```csharp
private async Task Run()
{
    TimeSpan? overrideDelay = RandomTimeSpan.Next(options.ProbeTimeout);
    while (await _pingTimer.NextTick(overrideDelay))
    {
        overrideDelay = default;

        // Determine if we should use direct or indirect probing
        var isDirectProbe = !options.EnableIndirectProbes
            || _failedProbes < options.NumMissedProbesLimit - 1
            || otherNodes.Length == 0;

        var timeout = GetTimeout(isDirectProbe);

        if (isDirectProbe)
        {
            probeResult = await this.ProbeDirectly(cancellation.Token);
        }
        else
        {
            // Pick random intermediary
            var intermediary = otherNodes[Random.Shared.Next(otherNodes.Length)];
            probeResult = await this.ProbeIndirectly(intermediary, timeout, cancellation.Token);

            // If intermediary unhealthy, recuse it and retry
            if (probeResult.Status != ProbeResultStatus.Succeeded
                && probeResult.IntermediaryHealthDegradationScore > 0)
            {
                _log.LogInformation("Recusing unhealthy intermediary '{Intermediary}' and trying again", intermediary);
                otherNodes = otherNodes.Where(node => !node.Equals(intermediary)).ToArray();
                overrideDelay = TimeSpan.FromMilliseconds(250);
            }
        }

        await _onProbeResult(this, probeResult);
    }

    TimeSpan GetTimeout(bool isDirectProbe)
    {
        var additionalTimeout = 0;

        if (options.ExtendProbeTimeoutDuringDegradation)
        {
            var localDegradationScore = _localSiloHealthMonitor.GetLocalHealthDegradationScore(DateTime.UtcNow);
            additionalTimeout += localDegradationScore;
        }

        if (!isDirectProbe)
        {
            additionalTimeout += 1;  // Extra hop
        }

        if (Debugger.IsAttached)
        {
            additionalTimeout += 25;  // 25x timeout when debugging
        }

        return options.ProbeTimeout.Multiply(1 + additionalTimeout);
    }
}
```

**Probing Strategy**:
1. **First `NumMissedProbesLimit - 1` failures**: Direct probes only
2. **After that**: Switch to indirect probes (via random intermediary)
3. **If intermediary unhealthy**: Recuse it, try different intermediary

**Adaptive timeout**:
- Base: `ProbeTimeout` (default 10 seconds)
- **Local degradation**: +1x per degradation point
- **Indirect probe**: +1x (extra network hop)
- **Debugger attached**: +25x (avoid false failures during debugging)

---

## Part 5: ClusterHealthMonitor - Monitoring Orchestration

### Purpose

ClusterHealthMonitor decides **which silos to monitor** and manages the `SiloHealthMonitor` instances. It:
- Selects probe targets using **expander graph** topology
- Creates/stops `SiloHealthMonitor` instances as membership changes
- Reacts to probe failures by calling `TryToSuspectOrKill`
- Evicts silos stuck in Joining state

**Code**: `ClusterHealthMonitor.cs:21-63`

### Expander Graph Monitoring Topology

**Code**: `ClusterHealthMonitor.cs:158-284`

```csharp
private ImmutableDictionary<SiloAddress, SiloHealthMonitor> UpdateMonitoredSilos(
    MembershipTableSnapshot membership,
    ImmutableDictionary<SiloAddress, SiloHealthMonitor> monitoredSilos,
    DateTime now)
{
    // Don't monitor if local silo not yet Active
    if (!membership.Entries.TryGetValue(this.localSiloDetails.SiloAddress, out var self)
        || !IsFunctionalForMembership(self.Status))
    {
        return ImmutableDictionary<SiloAddress, SiloHealthMonitor>.Empty;
    }

    var options = clusterMembershipOptions.CurrentValue;
    var numProbedSilos = options.NumProbedSilos;

    var silosToWatch = new List<SiloAddress>();
    var additionalSilos = new List<SiloAddress>();

    var tmpList = new List<(SiloAddress SiloAddress, int HashCode)>();
    foreach (var (candidate, entry) in membership.Entries)
    {
        if (!IsFunctionalForMembership(entry.Status)) continue;

        tmpList.Add((candidate, 0));

        if (candidate.IsSameLogicalSilo(this.localSiloDetails.SiloAddress)) continue;

        // Monitor all suspected and stale silos
        if (entry.GetFreshVotes(now, options.DeathVoteExpirationTimeout).Count > 0
            || entry.HasMissedIAmAlives(options, now))
        {
            additionalSilos.Add(candidate);
        }
    }

    // Expander graph construction using multiple hash rings
    for (var ringNum = 0; ringNum < numProbedSilos; ++ringNum)
    {
        // Update hash values with the current ring number
        for (var i = 0; i < tmpList.Count; i++)
        {
            var siloAddress = tmpList[i].SiloAddress;
            tmpList[i] = (siloAddress, siloAddress.GetConsistentHashCode(ringNum));
        }

        // Sort by hash value
        tmpList.Sort((x, y) => x.HashCode.CompareTo(y.HashCode));

        var myIndex = tmpList.FindIndex(el => el.SiloAddress.Equals(self.SiloAddress));

        // Starting at my index, find the first non-monitored silo and add it
        for (var i = 0; i < tmpList.Count - 1; i++)
        {
            var candidate = tmpList[(myIndex + i + 1) % tmpList.Count].SiloAddress;
            if (!silosToWatch.Contains(candidate))
            {
                silosToWatch.Add(candidate);
                break;
            }
        }
    }

    // Create monitors for silos we should watch
    var newProbedSilos = ImmutableDictionary.CreateBuilder<SiloAddress, SiloHealthMonitor>();
    foreach (var silo in silosToWatch.Union(additionalSilos))
    {
        if (!monitoredSilos.TryGetValue(silo, out var monitor))
        {
            monitor = this.createMonitor(silo);
            monitor.Start();
        }

        newProbedSilos[silo] = monitor;
    }

    return newProbedSilos.ToImmutable();

    static bool IsFunctionalForMembership(SiloStatus status)
        => status is SiloStatus.Active or SiloStatus.ShuttingDown or SiloStatus.Stopping;
}
```

**Expander Graph Algorithm**:

1. For each of `NumProbedSilos` rings (default 3):
   - Hash all silos using ring-specific seed: `GetConsistentHashCode(ringNum)`
   - Sort silos by hash value
   - Find local silo's position
   - Select first successor not already monitored

2. **Always monitor**:
   - Silos with fresh votes (suspected by others)
   - Silos that haven't updated IAmAlive recently

**Why expander graph?**
- **Probabilistic construction**: Multiple hash rings with different seeds
- **Low overlap**: Each silo monitors different set from others
- **Short paths**: Every silo reachable from every other in ~2-3 hops
- **Fast detection**: Concurrent failures detected quickly (minimal dependency chains)

**Based on**: "Stable and Consistent Membership at Scale with Rapid" (USENIX ATC 2018)

**Example (3-silo cluster, NumProbedSilos=2)**:
```
Ring 0 (seed=0):  Silo A → B → C → A
Ring 1 (seed=1):  Silo A → C → B → A

Monitoring graph:
A monitors: B (ring 0), C (ring 1)
B monitors: C (ring 0), A (ring 1)
C monitors: A (ring 0), B (ring 1)

Result: Every silo monitored by exactly 2 others
```

### Probe Result Handling

**Code**: `ClusterHealthMonitor.cs:318-337`

```csharp
private async Task OnProbeResultInternal(SiloHealthMonitor monitor, ProbeResult probeResult)
{
    if (this.shutdownCancellation.IsCancellationRequested) return;

    if (probeResult.IsDirectProbe)
    {
        if (probeResult.Status == ProbeResultStatus.Failed
            && probeResult.FailedProbeCount >= this.clusterMembershipOptions.CurrentValue.NumMissedProbesLimit)
        {
            await this.membershipService.TryToSuspectOrKill(monitor.TargetSiloAddress);
        }
    }
    else if (probeResult.Status == ProbeResultStatus.Failed)
    {
        // For indirect probes, pass the intermediary as well (contributes vote)
        await this.membershipService.TryToSuspectOrKill(monitor.TargetSiloAddress, probeResult.Intermediary);
    }
}
```

**Pattern**: Direct probe failure → Vote to kill; Indirect probe failure → Vote from 2 silos (self + intermediary)

**NumMissedProbesLimit** (default 3):
- Wait for 3 consecutive failures before voting
- Prevents single packet loss from triggering eviction
- Balances false positive rate vs detection time

### Evicting Stale Joining Silos

**Code**: `ClusterHealthMonitor.cs:118-155`

```csharp
private async Task EvictStaleStateSilos(MembershipTableSnapshot membership, DateTime utcNow)
{
    foreach (var member in membership.Entries)
    {
        if (IsCreatedOrJoining(member.Value.Status)
            && HasExceededMaxJoinTime(
                startTime: member.Value.StartTime,
                now: utcNow,
                maxJoinTime: this.clusterMembershipOptions.CurrentValue.MaxJoinAttemptTime))
        {
            try
            {
                await this.membershipService.TryToSuspectOrKill(member.Key);
            }
            catch(Exception exception)
            {
                log.LogError(exception, "Failed to evict stale joining silo {Silo}", member.Value.SiloAddress);
            }
        }
    }

    static bool IsCreatedOrJoining(SiloStatus status)
        => status == SiloStatus.Created || status == SiloStatus.Joining;

    static bool HasExceededMaxJoinTime(DateTime startTime, DateTime now, TimeSpan maxJoinTime)
        => now > startTime.Add(maxJoinTime);
}
```

**Pattern**: Evict silos stuck in `Created` or `Joining` for longer than `MaxJoinAttemptTime` (default 5 minutes)

**Why?** Prevents zombie silos from blocking cluster (e.g., failed mid-startup)

**Config**: `EvictWhenMaxJoinAttemptTimeExceeded` (default false, opt-in)

---

## Part 6: SiloStatusOracle - Read-Only Membership View

### Purpose

SiloStatusOracle provides a **cached, read-only view** of cluster membership for other components. It:
- Wraps `MembershipTableManager.MembershipTableSnapshot`
- Caches derived views (active silos, status dictionaries)
- Provides subscription API for status change notifications

**Code**: `SiloStatusOracle.cs:8-30`

### Cached Derived Views

**Code**: `SiloStatusOracle.cs:54-102`

```csharp
private MembershipTableSnapshot cachedSnapshot;
private Dictionary<SiloAddress, SiloStatus> siloStatusCache = new();
private Dictionary<SiloAddress, SiloStatus> siloStatusCacheOnlyActive = new();
private ImmutableArray<SiloAddress> _activeSilos = [];

public ImmutableArray<SiloAddress> GetActiveSilos()
{
    EnsureFreshCache();
    return _activeSilos;
}

public Dictionary<SiloAddress, SiloStatus> GetApproximateSiloStatuses(bool onlyActive = false)
{
    EnsureFreshCache();
    return onlyActive ? this.siloStatusCacheOnlyActive : this.siloStatusCache;
}

private void EnsureFreshCache()
{
    var currentMembership = this.membershipTableManager.MembershipTableSnapshot;
    if (ReferenceEquals(this.cachedSnapshot, currentMembership))
    {
        return;  // Cache hit
    }

    lock (this.cacheUpdateLock)
    {
        currentMembership = this.membershipTableManager.MembershipTableSnapshot;
        if (ReferenceEquals(this.cachedSnapshot, currentMembership))
        {
            return;  // Double-check after lock
        }

        var newSiloStatusCache = new Dictionary<SiloAddress, SiloStatus>();
        var newSiloStatusCacheOnlyActive = new Dictionary<SiloAddress, SiloStatus>();
        var newActiveSilos = ImmutableArray.CreateBuilder<SiloAddress>();

        foreach (var entry in currentMembership.Entries)
        {
            var silo = entry.Key;
            var status = entry.Value.Status;
            newSiloStatusCache[silo] = status;
            if (status == SiloStatus.Active)
            {
                newSiloStatusCacheOnlyActive[silo] = status;
                newActiveSilos.Add(silo);
            }
        }

        Interlocked.Exchange(ref this.cachedSnapshot, currentMembership);
        this.siloStatusCache = newSiloStatusCache;
        this.siloStatusCacheOnlyActive = newSiloStatusCacheOnlyActive;
        _activeSilos = newActiveSilos.ToImmutable();
    }
}
```

**Pattern**: Reference equality check → Lock → Double-check → Rebuild caches

**Why cache?**
- `GetActiveSilos()` called frequently (grain placement, routing)
- Avoid allocating `ImmutableArray` on every call
- Trade memory for CPU (cache 3 derived views)

**Thread-safety**:
- Reference equality check (fast path, no lock)
- Lock for cache rebuild (slow path)
- `Interlocked.Exchange` to publish new snapshot atomically

### Query Methods

**Code**: `SiloStatusOracle.cs:36-52, 104-132`

```csharp
public SiloStatus GetApproximateSiloStatus(SiloAddress silo)
{
    var status = this.membershipTableManager.MembershipTableSnapshot.GetSiloStatus(silo);

    if (status == SiloStatus.None && this.CurrentStatus == SiloStatus.Active)
    {
        this.log.LogDebug("SiloAddress {Silo} not registered in MembershipOracle", silo);
    }

    return status;
}

public bool IsDeadSilo(SiloAddress silo)
{
    if (silo.Equals(this.SiloAddress)) return false;
    return this.GetApproximateSiloStatus(silo) == SiloStatus.Dead;
}

public bool IsFunctionalDirectory(SiloAddress silo)
{
    if (silo.Equals(this.SiloAddress)) return true;
    var status = this.GetApproximateSiloStatus(silo);
    return !status.IsTerminating();  // Active, ShuttingDown, Stopping, Joining
}

public bool TryGetSiloName(SiloAddress siloAddress, out string siloName)
{
    var snapshot = this.membershipTableManager.MembershipTableSnapshot.Entries;
    if (snapshot.TryGetValue(siloAddress, out var entry))
    {
        siloName = entry.SiloName;
        return true;
    }

    siloName = default;
    return false;
}
```

**Methods**:
- `GetApproximateSiloStatus` - Query status (may be stale)
- `IsDeadSilo` - Check if dead
- `IsFunctionalDirectory` - Check if can use for directory operations
- `TryGetSiloName` - Get silo's human-readable name

**Why "Approximate"?**
- Reads from local snapshot (may lag storage)
- Other silos may have newer information
- Gossip and periodic refresh eventually converge

---

## Critical Insights for Moonpool

### 1. Separation of Concerns is Key

Orleans separates membership into distinct responsibilities:
- **MembershipTableManager**: Storage orchestrator, owns lifecycle
- **MembershipAgent**: Local silo status transitions
- **SiloHealthMonitor**: Failure detection for one remote silo
- **ClusterHealthMonitor**: Orchestrates all monitors
- **SiloStatusOracle**: Read-only view for consumers

**For Moonpool**: Start with simplified versions, but maintain separation. Easier to test and evolve.

### 2. Voting Prevents False Positives

Single silo's opinion not enough to declare another dead:
- Default: 2 votes required (configurable)
- Small clusters: Majority vote (1 or 2)
- Votes expire after 2 minutes (prevents stale votes)

**For Moonpool**: Essential for deterministic simulation. Buggify should inject:
- Network delays (cause probe failures)
- Slow IAmAlive updates (trigger votes)
- Validate voting thresholds prevent false evictions

### 3. Indirect Probing Handles Partitions

If direct probe fails, ask another silo to probe on your behalf:
- Distinguishes target failure from local network issues
- Intermediary's vote counts toward quorum
- Unhealthy intermediaries recused (prevent cascading false positives)

**For Moonpool**: Critical for network partition testing. Scenarios:
- A can't reach B, but C can reach B → B alive, A-B partition
- Neither A nor C can reach B → B likely dead

### 4. Gossip Reduces Latency

Storage-based membership has inherent latency (polling interval):
- Default `TableRefreshTimeout` = 60 seconds
- Can't reduce too much (storage load)

Gossip solves this:
- Changes propagated immediately to all active silos
- Reduces detection latency from minutes to seconds
- Best-effort (storage is still authoritative)

**For Moonpool**: Implement gossip after basic storage works. Use buggify to:
- Drop gossip messages (ensure storage fallback works)
- Delay gossip (test latency tolerance)

### 5. Expander Graph Topology Matters

Random monitoring leads to long detection chains:
- Silo A monitors B, B monitors C, C monitors D
- If C fails, D won't be detected until C times out

Expander graph minimizes this:
- Each silo monitored by multiple others
- Low overlap between monitoring sets
- Average path length ~2-3 hops

**For Moonpool**: Initially use simple "monitor all" or "monitor K random" approach. Add expander graph only if:
- Cluster size > 10 nodes
- Concurrent failure detection time matters

### 6. Optimistic Concurrency with Retries

Multiple silos writing to membership table creates conflicts:
- ETags prevent lost updates
- Unlimited retries with exponential backoff
- Eventual success guaranteed (assuming storage available)

**For Moonpool**: Use buggify to inject write conflicts. Validate:
- Retries eventually succeed
- No lost votes
- No double-eviction

### 7. IAmAlive is Cheap but Critical

Heartbeat mechanism:
- Updates single timestamp field
- Cheap operation (many storage systems optimize)
- Period: 5 seconds (default)
- Missing threshold: 3x period = 15 seconds

**For Moonpool**: Use `TimeProvider` trait. Buggify should:
- Delay IAmAlive updates (trigger "stale" detection)
- Skip updates (test missed threshold)
- Validate silos not evicted if IAmAlive current

---

## Patterns for Moonpool

### ✅ Adopt These Patterns

1. **Immutable Snapshots with Versioning**
   ```rust
   #[derive(Clone)]
   pub struct ClusterMembershipSnapshot {
       members: Arc<HashMap<NodeAddress, ClusterMember>>,
       version: MembershipVersion,
   }

   impl ClusterMembershipSnapshot {
       pub fn is_successor_to(&self, other: &Self) -> bool {
           self.version > other.version
       }
   }
   ```

2. **Voting-Based Eviction**
   ```rust
   pub struct MembershipEntry {
       silo_address: NodeAddress,
       status: NodeStatus,
       suspect_times: Vec<(NodeAddress, DateTime)>,
   }

   impl MembershipEntry {
       pub fn get_fresh_votes(&self, now: DateTime, expiration: Duration) -> Vec<NodeAddress> {
           self.suspect_times
               .iter()
               .filter(|(_, time)| now - *time < expiration)
               .map(|(addr, _)| *addr)
               .collect()
       }
   }
   ```

3. **Direct + Indirect Probing**
   ```rust
   pub enum ProbeStrategy {
       Direct,
       Indirect { intermediary: NodeAddress },
   }

   pub async fn probe(&self, target: NodeAddress, strategy: ProbeStrategy) -> ProbeResult {
       match strategy {
           ProbeStrategy::Direct => self.probe_directly(target).await,
           ProbeStrategy::Indirect { intermediary } => {
               self.probe_via_intermediary(target, intermediary).await
           }
       }
   }
   ```

4. **Snapshot Publishing with Version Validation**
   ```rust
   pub struct MembershipPublisher {
       current: Arc<RwLock<ClusterMembershipSnapshot>>,
       subscribers: Arc<Mutex<Vec<mpsc::UnboundedSender<ClusterMembershipSnapshot>>>>,
   }

   impl MembershipPublisher {
       pub fn try_publish(&self, snapshot: ClusterMembershipSnapshot) -> bool {
           let mut current = self.current.write();
           if !snapshot.is_successor_to(&current) {
               return false;  // Reject stale snapshot
           }
           *current = snapshot.clone();
           for subscriber in self.subscribers.lock().iter() {
               let _ = subscriber.send(snapshot.clone());
           }
           true
       }
   }
   ```

5. **Optimistic Concurrency with Retries**
   ```rust
   pub async fn update_status(&self, status: NodeStatus) -> Result<()> {
       let mut attempts = 0;
       let mut backoff = ExponentialBackoff::new(Duration::from_millis(100), Duration::from_secs(60));

       loop {
           let table = self.storage.read_all().await?;
           let (entry, etag) = self.get_or_create_local_entry(table, status);

           entry.status = status;
           entry.i_am_alive_time = Instant::now();

           match self.storage.update_row(entry, etag, table.version.next()).await {
               Ok(true) => return Ok(()),
               Ok(false) => {
                   attempts += 1;
                   time.sleep(backoff.next(attempts)).await;
                   continue;
               }
               Err(e) => return Err(e),
           }
       }
   }
   ```

### ⚠️ Adapt These Patterns

1. **Expander Graph Monitoring**
   - Orleans: Complex multi-ring hash algorithm
   - Moonpool: Start simple (monitor all or random K), add expander graph only if needed
   ```rust
   // Initial: Simple random K selection
   pub fn select_monitoring_targets(&self, all_nodes: &[NodeAddress], k: usize) -> Vec<NodeAddress> {
       all_nodes.choose_multiple(&mut rand::thread_rng(), k).cloned().collect()
   }

   // Later: Expander graph (if needed)
   pub fn select_monitoring_targets_expander(&self, all_nodes: &[NodeAddress], num_rings: usize) -> Vec<NodeAddress> {
       // Multi-ring hash algorithm (Orleans implementation)
   }
   ```

2. **Gossip Protocol**
   - Orleans: SystemTarget-based RPC
   - Moonpool: Start without gossip (polling only), add gossip later for latency
   ```rust
   // Phase 1: Polling only
   pub async fn refresh_membership(&self) {
       let table = self.storage.read_all().await?;
       self.publish_snapshot(table.to_snapshot());
   }

   // Phase 2: Add gossip
   pub async fn gossip_to_others(&self, updated_node: NodeAddress, status: NodeStatus) {
       for peer in self.get_gossip_targets() {
           self.send_gossip(peer, self.current_snapshot()).await.ok();
       }
   }
   ```

3. **Storage Abstraction**
   - Orleans: `IMembershipTable` with various backends (Azure, SQL, etc.)
   - Moonpool: Start with in-memory (for simulation), add persistence later
   ```rust
   pub trait MembershipStorage: Send + Sync {
       async fn read_all(&self) -> Result<MembershipTableData>;
       async fn update_row(&self, entry: MembershipEntry, etag: String, version: TableVersion) -> Result<bool>;
       async fn insert_row(&self, entry: MembershipEntry, version: TableVersion) -> Result<bool>;
   }

   // For simulation: In-memory with deterministic behavior
   pub struct InMemoryMembershipStorage {
       table: Arc<Mutex<MembershipTableData>>,
       buggify: Arc<dyn Buggify>,
   }
   ```

### ❌ Avoid These Patterns

1. **Complex Debugger Detection**
   - Orleans: Extends timeouts 25x when debugger attached
   - Moonpool: Not applicable in simulation (deterministic time)
   ```rust
   // ❌ Avoid
   let timeout = if is_debugger_attached() {
       base_timeout * 25
   } else {
       base_timeout
   };

   // ✅ Use
   let timeout = config.probe_timeout;  // Deterministic always
   ```

2. **Physical Host Detection**
   - Orleans: Detects silo migration vs restart based on hostname
   - Moonpool: Not meaningful in simulation
   ```rust
   // ❌ Avoid checking hostname changes

   // ✅ Use generation-based detection only
   pub fn is_newer_generation(&self, other: &NodeAddress) -> bool {
       self.endpoint == other.endpoint && self.generation > other.generation
   }
   ```

3. **Metric Instrumentation**
   - Orleans: Extensive metrics (MessagingInstruments, etc.)
   - Moonpool: Focus on simulation assertions first, add metrics later if needed
   ```rust
   // ❌ Don't prioritize metrics initially
   // ✅ Use sometimes_assert! for validation
   sometimes_assert!(probe_result.succeeded, "Probe should eventually succeed");
   ```

---

## Moonpool Implementation Plan

### Early Iterations: Static Membership

For initial moonpool phases, implement **simple static membership traits**:

```rust
/// Static membership configuration - no failure detection
pub struct StaticMembershipProvider {
    members: Vec<NodeAddress>,
}

impl MembershipProvider for StaticMembershipProvider {
    fn get_members(&self) -> Vec<NodeAddress> {
        self.members.clone()
    }

    fn is_member(&self, addr: &NodeAddress) -> bool {
        self.members.contains(addr)
    }
}

// Configuration via builder
let cluster = ActorSystemBuilder::new()
    .with_static_membership(vec![
        NodeAddress::new("127.0.0.1:5001"),
        NodeAddress::new("127.0.0.1:5002"),
        NodeAddress::new("127.0.0.1:5003"),
    ])
    .build();
```

**Why static first?**
- Simplifies early actor system testing
- No failure detection complexity
- Deterministic cluster topology
- Focus on message routing, activation, etc.

### Path to Full Membership Features

As moonpool evolves, add membership features incrementally:

**Phase 1**: Static membership (manual configuration)
- No failure detection
- No dynamic join/leave
- Fixed cluster topology

**Phase 2**: Storage-based membership
- Shared storage abstraction (`InMemoryMembershipStorage` for simulation)
- Status transitions (Joining → Active → Dead)
- IAmAlive heartbeats
- Optimistic concurrency with ETags

**Phase 3**: Failure detection
- Direct probing
- Voting-based eviction
- Configurable thresholds (votes required, missed probes limit)

**Phase 4**: Advanced features
- Indirect probing (partition tolerance)
- Gossip protocol (reduced latency)
- Expander graph monitoring (large clusters)

**Testing at each phase**:
- Chaos tests with buggify
- Network delays/partitions
- Concurrent failures
- Write conflicts
- Validate invariants (no split brain, eventual consistency)

---

## Summary

Orleans membership service is built on:
- **Immutable snapshots** with version monotonicity
- **Voting-based eviction** to prevent false positives
- **Multi-level probing** (direct + indirect) for partition tolerance
- **Optimistic concurrency** with ETags and unlimited retries
- **Gossip protocol** for low-latency propagation
- **Expander graph** monitoring topology for concurrent failure detection
- **IAmAlive heartbeats** for liveness proof

Key architectural principles:
- Separation of concerns (Manager, Agent, Monitor, Oracle)
- Snapshot-based updates (immutable state)
- Storage as authoritative source (gossip is optimization)
- Adaptive timeouts (local degradation, indirect probes)
- Probabilistic algorithms (expander graph, random intermediaries)

For Moonpool: Start with static membership for early iterations. Implement full membership features incrementally as actor system matures. Use buggify extensively to validate voting, probing, and convergence under hostile conditions.

**Early iterations**: Static membership trait where cluster members declared statically in configuration. **Later**: Full dynamic membership with failure detection, voting, and gossip as described in this document.
