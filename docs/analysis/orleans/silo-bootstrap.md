# Silo Bootstrap and SystemTargets

## Reference Files
- `Silo.cs:1-681` - Main silo class, bootstrap orchestration, lifecycle participation
- `SystemTarget.cs:1-397` - Base class for infrastructure actors
- `SystemTargetShared.cs:1-42` - Shared dependencies for SystemTargets
- `GrainService.cs:1-182` - Specialized SystemTarget with ring participation
- `ISiloLifecycle.cs:1-25` - Lifecycle interface
- `ServiceLifecycleStage.cs:1-63` - Lifecycle stage constants
- `LifecycleSchedulingSystemTarget.cs:683-691` - SystemTarget for lifecycle execution

## Overview

Orleans implements a **staged lifecycle bootstrap** where the actor runtime (Silo) initializes through well-defined lifecycle stages. The system uses:
- **Lifecycle Stages**: Ordered phases (RuntimeInitialize → RuntimeServices → RuntimeGrainServices → BecomeActive → Active)
- **Participant Pattern**: Components register callbacks for specific stages
- **SystemTargets**: Static infrastructure actors that provide runtime services
- **Ordered Execution**: All lifecycle operations run through a dedicated SystemTarget's WorkItemGroup

The architecture separates concerns:
- **Silo** - Top-level orchestrator, coordinates lifecycle
- **ISiloLifecycle** - Observable lifecycle with ordered stages
- **ILifecycleParticipant** - Components that hook into lifecycle stages
- **SystemTarget** - Base class for infrastructure actors (never deactivated)
- **GrainService** - Specialized SystemTarget with consistent ring participation

---

## Part 1: Silo Bootstrap (Actor Runtime Initialization)

### Silo Construction

**Code**: `Silo.cs:66-136`

The Silo constructor performs dependency injection and initial setup but **does not start anything**. It's a pure initialization phase.

```csharp
public Silo(ILocalSiloDetails siloDetails, IServiceProvider services)
{
    SystemStatus = SystemStatus.Creating;
    Services = services;

    // Core runtime components
    RingProvider = services.GetRequiredService<IConsistentRingProvider>();
    platformWatchdog = services.GetRequiredService<Watchdog>();
    this.siloDetails = siloDetails;

    // Message infrastructure
    messageCenter = Services.GetRequiredService<MessageCenter>();
    messageCenter.SniffIncomingMessage = runtimeClient.SniffIncomingMessage;

    runtimeClient = Services.GetRequiredService<InsideRuntimeClient>();
    grainFactory = Services.GetRequiredService<GrainFactory>();

    // Lifecycle setup
    this.siloLifecycle = this.Services.GetRequiredService<ISiloLifecycleSubject>();

    // Register all lifecycle participants from DI container
    IEnumerable<ILifecycleParticipant<ISiloLifecycle>> lifecycleParticipants =
        this.Services.GetServices<ILifecycleParticipant<ISiloLifecycle>>();
    foreach (ILifecycleParticipant<ISiloLifecycle> participant in lifecycleParticipants)
    {
        participant?.Participate(this.siloLifecycle);
    }

    // Silo itself participates in lifecycle
    this.Participate(this.siloLifecycle);

    this.SystemStatus = SystemStatus.Created;
}
```

**Key Insights**:

1. **Two-phase initialization**: Constructor sets up, `StartAsync()` triggers execution
2. **Dependency injection**: All major components resolved from DI container
3. **Lifecycle participant discovery**: DI container provides all `ILifecycleParticipant<ISiloLifecycle>` implementations
4. **Self-participation**: Silo registers itself as a lifecycle participant (line 133)
5. **Status tracking**: `SystemStatus` tracks silo state machine (Creating → Created → Starting → Running → Stopping → Terminated)

**Components initialized**:
- `MessageCenter` - Message routing and delivery
- `InsideRuntimeClient` - Internal client for grain calls
- `GrainFactory` - Grain reference creation
- `Watchdog` - Execution stall detection
- `IConsistentRingProvider` - Consistent hashing ring

### Lifecycle Participant Pattern

**Code**: `Silo.cs:126-133, 467-474`

Components register for lifecycle stages using the participant pattern:

```csharp
// During construction, discover all participants
IEnumerable<ILifecycleParticipant<ISiloLifecycle>> lifecycleParticipants =
    this.Services.GetServices<ILifecycleParticipant<ISiloLifecycle>>();

foreach (ILifecycleParticipant<ISiloLifecycle> participant in lifecycleParticipants)
{
    participant?.Participate(this.siloLifecycle);
}

// Silo's own participation
private void Participate(ISiloLifecycle lifecycle)
{
    lifecycle.Subscribe<Silo>(
        ServiceLifecycleStage.RuntimeInitialize,
        OnRuntimeInitializeStart,
        OnRuntimeInitializeStop);

    lifecycle.Subscribe<Silo>(
        ServiceLifecycleStage.RuntimeServices,
        OnRuntimeServicesStart,
        OnRuntimeServicesStop);

    lifecycle.Subscribe<Silo>(
        ServiceLifecycleStage.RuntimeGrainServices,
        OnRuntimeGrainServicesStart);

    lifecycle.Subscribe<Silo>(
        ServiceLifecycleStage.BecomeActive,
        OnBecomeActiveStart,
        OnBecomeActiveStop);

    lifecycle.Subscribe<Silo>(
        ServiceLifecycleStage.Active,
        OnActiveStart,
        OnActiveStop);
}
```

**Pattern**: `lifecycle.Subscribe<TObserver>(stage, onStart, onStop)`

**Lifecycle Direction**:
- **Startup**: Stages execute in ascending order (RuntimeInitialize → Active)
- **Shutdown**: Stages execute in descending order (Active → RuntimeInitialize)
- **Symmetry**: Each stage's `onStop` reverses its `onStart` actions

**Stage Constants** (`ServiceLifecycleStage.cs:7-61`):
```csharp
public static class ServiceLifecycleStage
{
    public const int First = int.MinValue;
    public const int RuntimeInitialize = 2000;
    public const int RuntimeServices = 4000;
    public const int RuntimeStorageServices = 6000;
    public const int RuntimeGrainServices = 8000;
    public const int AfterRuntimeGrainServices = 8100;
    public const int ApplicationServices = 10000;
    public const int BecomeActive = Active - 1;  // 19999
    public const int Active = 20000;
    public const int Last = int.MaxValue;
}
```

**Why integer stages?** Allows fine-grained ordering. Components can insert between stages (e.g., `8100` goes after `8000`).

### StartAsync: Triggering the Lifecycle

**Code**: `Silo.cs:143-157`

```csharp
public async Task StartAsync(CancellationToken cancellationToken)
{
    // SystemTarget for provider init calls
    this.lifecycleSchedulingSystemTarget = Services.GetRequiredService<LifecycleSchedulingSystemTarget>();

    try
    {
        // All lifecycle operations run through this SystemTarget's WorkItemGroup
        await this.lifecycleSchedulingSystemTarget.WorkItemGroup.QueueTask(
            () => this.siloLifecycle.OnStart(cancellationToken),
            lifecycleSchedulingSystemTarget);
    }
    catch (Exception exc)
    {
        LogErrorSiloStart(logger, exc);
        throw;
    }
}
```

**Critical Insight**: All lifecycle `onStart` callbacks execute **serially** through `LifecycleSchedulingSystemTarget.WorkItemGroup`.

**Why use a SystemTarget's WorkItemGroup?**
1. **Ordered execution**: WorkItemGroup ensures sequential processing
2. **Consistent context**: All lifecycle operations share the same RuntimeContext
3. **Deterministic**: No race conditions between lifecycle stages
4. **Error isolation**: Exceptions propagate cleanly

**LifecycleSchedulingSystemTarget** (`Silo.cs:683-691`):
```csharp
internal sealed class LifecycleSchedulingSystemTarget : SystemTarget
{
    public LifecycleSchedulingSystemTarget(SystemTargetShared shared)
        : base(Constants.LifecycleSchedulingSystemTargetType, shared)
    {
        shared.ActivationDirectory.RecordNewTarget(this);
    }
}
```

**Purpose**: Dummy SystemTarget that exists only to provide a WorkItemGroup for lifecycle execution.

### Lifecycle Stage 1: RuntimeInitialize

**Code**: `Silo.cs:159-171, 374-386`

```csharp
// Start
private Task OnRuntimeInitializeStart(CancellationToken ct)
{
    lock (lockable)
    {
        if (!this.SystemStatus.Equals(SystemStatus.Created))
            throw new InvalidOperationException($"Silo in wrong state: {this.SystemStatus}");

        this.SystemStatus = SystemStatus.Starting;
    }

    LogInfoSiloStarting(logger);
    return Task.CompletedTask;
}

// Stop (shutdown)
private async Task OnRuntimeInitializeStop(CancellationToken ct)
{
    try
    {
        await messageCenter.StopAsync();
    }
    catch (Exception exception)
    {
        LogErrorStoppingMessageCenter(logger, exception);
    }

    SystemStatus = SystemStatus.Terminated;
}
```

**Start Phase**:
- Validate state transition (Created → Starting)
- Update SystemStatus
- Purely a status change, no heavy work

**Stop Phase** (shutdown):
- Stop MessageCenter (stop accepting new connections)
- Set status to Terminated
- Final cleanup

### Lifecycle Stage 2: RuntimeServices

**Code**: `Silo.cs:191-194, 366-372`

```csharp
// Start
private Task OnRuntimeServicesStart(CancellationToken ct)
{
    return Task.CompletedTask;  // Currently a no-op
}

// Stop
private Task OnRuntimeServicesStop(CancellationToken ct)
{
    // Start rejecting all silo-to-silo application messages
    messageCenter.BlockApplicationMessages();

    return Task.CompletedTask;
}
```

**Start Phase**: No-op (services initialized via DI already)

**Stop Phase**:
- Block application messages
- Allow system messages (for coordination during shutdown)

**Purpose**: Placeholder stage for future service initialization

### Lifecycle Stage 3: RuntimeGrainServices

**Code**: `Silo.cs:196-216`

This is where the heavy lifting happens: GrainService creation and watchdog startup.

```csharp
private async Task OnRuntimeGrainServicesStart(CancellationToken ct)
{
    var stopWatch = Stopwatch.StartNew();

    // Load and init grain services before silo becomes active
    await StartAsyncTaskWithPerfAnalysis("Init grain services",
        () => CreateGrainServices(), stopWatch);

    try
    {
        // Start background timer tick to watch for platform execution stalls
        this.platformWatchdog.Start();
    }
    catch (Exception exc)
    {
        LogErrorStartingSiloGoingToFastKill(logger, exc, SiloAddress);
        throw;
    }

    LogDebugSiloStartComplete(logger, this.SystemStatus);
}
```

**CreateGrainServices** (`Silo.cs:232-239`):
```csharp
private async Task CreateGrainServices()
{
    var grainServices = this.Services.GetServices<IGrainService>();
    foreach (var grainService in grainServices)
    {
        await RegisterGrainService(grainService);
    }
}
```

**RegisterGrainService** (`Silo.cs:241-259`):
```csharp
private async Task RegisterGrainService(IGrainService service)
{
    var grainService = (GrainService)service;
    var activationDirectory = this.Services.GetRequiredService<ActivationDirectory>();

    // Register in activation directory (so it can receive messages)
    activationDirectory.RecordNewTarget(grainService);
    grainServices.Add(grainService);

    try
    {
        // Call Init() with timeout
        await grainService.QueueTask(() => grainService.Init(Services))
            .WaitAsync(this.initTimeout);
    }
    catch (TimeoutException exception)
    {
        LogErrorGrainInitializationTimeout(logger, exception, initTimeout);
        throw;
    }

    LogInfoGrainServiceRegistered(logger, service.GetType().FullName);
}
```

**Key Actions**:
1. **Discover GrainServices**: Get all `IGrainService` from DI container
2. **Register each service**: Add to ActivationDirectory (makes it addressable)
3. **Initialize services**: Call `Init()` via the service's WorkItemGroup
4. **Timeout enforcement**: Init must complete within `initTimeout` (default from config, extended if debugger attached)
5. **Start watchdog**: Platform execution stall detection

**Why register before initializing?**
- Services need to be addressable to receive messages during init
- Other components can reference them immediately

### Lifecycle Stage 4: BecomeActive

**Code**: `Silo.cs:218-222, 388-419`

```csharp
// Start
private Task OnBecomeActiveStart(CancellationToken ct)
{
    this.SystemStatus = SystemStatus.Running;
    return Task.CompletedTask;
}

// Stop
private async Task OnBecomeActiveStop(CancellationToken ct)
{
    try
    {
        try
        {
            var catalog = this.Services.GetRequiredService<Catalog>();
            await catalog.DeactivateAllActivations(ct);
        }
        catch (Exception exception)
        {
            if (!ct.IsCancellationRequested)
            {
                LogErrorDeactivatingActivations(logger, exception);
            }
            else
            {
                LogWarningSomeGrainsFailedToDeactivate(logger);
            }
        }

        // Wait for all queued messages to be sent before stopping outbound queue
        await Task.Delay(waitForMessageToBeQueuedForOutbound, ct).SuppressThrowing();
    }
    catch (Exception exc)
    {
        LogErrorSiloFailedToStopMembership(logger, exc);
    }

    // Stop accepting client connections
    await messageCenter.StopAcceptingClientMessages();
}
```

**Start Phase**:
- Set `SystemStatus = Running`
- Silo now accepts application traffic

**Stop Phase** (shutdown):
1. **Deactivate all activations**: Gracefully shut down all active grains
2. **Wait for outbound messages**: Give messages time to be queued
3. **Stop gateway**: Stop accepting client connections

**Why "BecomeActive" vs "Active"?**
- BecomeActive: Transition phase, minimal work
- Active: Fully operational, start application services

### Lifecycle Stage 5: Active

**Code**: `Silo.cs:224-230, 421-462`

```csharp
// Start
private async Task OnActiveStart(CancellationToken ct)
{
    foreach (var grainService in grainServices)
    {
        await StartGrainService(grainService);
    }
}

// Stop
private async Task OnActiveStop(CancellationToken ct)
{
    if (ct.IsCancellationRequested)
        return;

    // Send disconnect messages to clients
    if (this.messageCenter.Gateway != null)
    {
        try
        {
            await lifecycleSchedulingSystemTarget
                .QueueTask(() => this.messageCenter.Gateway.SendStopSendMessages(this.grainFactory))
                .WaitAsync(ct);
        }
        catch (Exception exception)
        {
            LogErrorSendingDisconnectRequests(logger, exception);
            if (!ct.IsCancellationRequested)
            {
                throw;
            }
        }
    }

    // Stop all grain services
    foreach (var grainService in grainServices)
    {
        try
        {
            await grainService
                .QueueTask(grainService.Stop)
                .WaitAsync(ct);
        }
        catch (Exception exception)
        {
            LogErrorStoppingGrainService(logger, grainService, exception);
            if (!ct.IsCancellationRequested)
            {
                throw;
            }
        }

        LogDebugGrainServiceStopped(logger, grainService.GetType().FullName, grainService.GetGrainId());
    }
}
```

**StartGrainService** (`Silo.cs:261-276`):
```csharp
private async Task StartGrainService(IGrainService service)
{
    var grainService = (GrainService)service;

    try
    {
        await grainService.QueueTask(grainService.Start).WaitAsync(this.initTimeout);
    }
    catch (TimeoutException exception)
    {
        LogErrorGrainStartupTimeout(logger, exception, initTimeout);
        throw;
    }

    LogInfoGrainServiceStarted(logger, service.GetType().FullName);
}
```

**Start Phase**:
- Call `Start()` on each GrainService
- GrainServices can now accept messages and perform work
- **Note**: Init'd in RuntimeGrainServices, Started in Active

**Stop Phase** (shutdown):
1. **Send disconnect requests**: Notify clients silo is shutting down
2. **Stop all GrainServices**: Call `Stop()` on each service
3. **Timeout enforcement**: Each stop must complete within cancellation token

**Two-phase GrainService startup** (Init vs Start):
- **Init** (RuntimeGrainServices): Register, setup, prepare
- **Start** (Active): Begin processing, subscribe to events
- **Why separate?** Faster startup - init can be slow, but we want to mark silo as Active ASAP

### Shutdown Flow (Reverse Order)

**Code**: `Silo.cs:295-364`

```csharp
public async Task StopAsync(CancellationToken cancellationToken)
{
    bool gracefully = !cancellationToken.IsCancellationRequested;
    bool stopAlreadyInProgress = false;

    lock (lockable)
    {
        if (this.SystemStatus.Equals(SystemStatus.Stopping) ||
            this.SystemStatus.Equals(SystemStatus.ShuttingDown) ||
            this.SystemStatus.Equals(SystemStatus.Terminated))
        {
            stopAlreadyInProgress = true;
        }
        else if (!this.SystemStatus.Equals(SystemStatus.Running))
        {
            throw new InvalidOperationException($"Can't shutdown from state {this.SystemStatus}");
        }
        else
        {
            if (gracefully)
                this.SystemStatus = SystemStatus.ShuttingDown;
            else
                this.SystemStatus = SystemStatus.Stopping;
        }
    }

    if (stopAlreadyInProgress)
    {
        // Wait for other shutdown to complete
        while (!this.SystemStatus.Equals(SystemStatus.Terminated))
        {
            await Task.Delay(TimeSpan.FromSeconds(1));
        }
        await this.SiloTerminated;
        return;
    }

    try
    {
        // Lifecycle stages stop in reverse order
        await this.lifecycleSchedulingSystemTarget.QueueTask(
            () => this.siloLifecycle.OnStop(cancellationToken));
    }
    finally
    {
        // Signal silo terminated
        await Task.Run(() => this.siloTerminatedTask.TrySetResult(0));
    }
}
```

**Shutdown Flow**:
1. **Check if already stopping**: Prevent multiple concurrent shutdowns
2. **Set shutdown status**: `ShuttingDown` (graceful) or `Stopping` (fast)
3. **Execute lifecycle.OnStop()**: Stages execute in **reverse order**
   - Active → BecomeActive → RuntimeGrainServices → RuntimeServices → RuntimeInitialize
4. **Signal completion**: `siloTerminatedTask` completes

**Graceful vs Non-graceful**:
- **Graceful** (`!cancellationToken.IsCancellationRequested`):
  - Set status to `ShuttingDown`
  - Give stages time to cleanup
  - Wait for operations to complete
- **Non-graceful** (`cancellationToken.IsCancellationRequested`):
  - Set status to `Stopping`
  - Aggressive cancellation
  - May lose in-flight work

### Critical Bootstrap Patterns

#### 1. Two-Phase Initialization

**Pattern**: Constructor sets up, StartAsync triggers execution

**Why?**
- Constructor must be fast and synchronous
- Async initialization happens in StartAsync
- Allows dependency injection to complete before async work

#### 2. Lifecycle Participant Discovery

**Pattern**: DI container provides all `ILifecycleParticipant<ISiloLifecycle>`

**Code**:
```csharp
IEnumerable<ILifecycleParticipant<ISiloLifecycle>> lifecycleParticipants =
    this.Services.GetServices<ILifecycleParticipant<ISiloLifecycle>>();
foreach (var participant in lifecycleParticipants)
{
    participant?.Participate(this.siloLifecycle);
}
```

**Why?**
- Decoupled: Components register themselves without Silo knowing about them
- Extensible: New components just implement `ILifecycleParticipant`
- Testable: Mock participants for testing

#### 3. Ordered Execution via SystemTarget WorkItemGroup

**Pattern**: All lifecycle operations queue through `LifecycleSchedulingSystemTarget.WorkItemGroup`

**Why?**
- **Sequential**: No concurrent lifecycle operations
- **Deterministic**: Same order every time
- **RuntimeContext**: All operations share the same context
- **Error propagation**: Exceptions bubble up cleanly

#### 4. Symmetric Lifecycle (Start/Stop)

**Pattern**: Each stage subscribes with both `onStart` and `onStop`

**Why?**
- **Resource cleanup**: What you start, you stop
- **Reverse order**: Stop undoes Start
- **Error tolerance**: Stop continues even if errors occur

#### 5. Timeout Enforcement

**Pattern**: Critical operations wrapped with `WaitAsync(timeout)`

**Code**:
```csharp
await grainService.QueueTask(() => grainService.Init(Services))
    .WaitAsync(this.initTimeout);
```

**Why?**
- **Prevent hangs**: Lifecycle must make progress
- **Debugger-friendly**: Timeout extended if debugger attached
- **Fast failure**: Better to fail fast than hang indefinitely

---

## Part 2: SystemTargets (Static Infrastructure Actors)

### SystemTarget Overview

**Code**: `SystemTarget.cs:18-75`

SystemTargets are **static infrastructure actors** that provide runtime services. They differ fundamentally from regular grains (activations).

```csharp
public abstract partial class SystemTarget : ISystemTarget, ISystemTargetBase, IGrainContext
{
    private readonly SystemTargetGrainId _id;
    private readonly SystemTargetShared _shared;
    private readonly HashSet<IGrainTimer> _timers = [];
    private GrainReference _selfReference;
    private Message _running;
    private Dictionary<Type, object> _components = new Dictionary<Type, object>();

    public SiloAddress Silo => _shared.SiloAddress;
    internal GrainAddress ActivationAddress { get; }
    internal ActivationId ActivationId { get; set; }
    internal WorkItemGroup WorkItemGroup { get; }

    internal SystemTarget(GrainType grainType, SystemTargetShared shared)
        : this(SystemTargetGrainId.Create(grainType, shared.SiloAddress), shared)
    {
    }

    internal SystemTarget(SystemTargetGrainId grainId, SystemTargetShared shared)
    {
        _id = grainId;
        _shared = shared;

        // Deterministic activation ID from grain ID
        ActivationId = ActivationId.GetDeterministic(grainId.GrainId);
        ActivationAddress = GrainAddress.GetAddress(Silo, _id.GrainId, ActivationId);

        _logger = shared.LoggerFactory.CreateLogger(GetType());

        // Each SystemTarget has its own WorkItemGroup
        WorkItemGroup = _shared.CreateWorkItemGroup(this);

        // Update metrics (unless singleton system target)
        if (!Constants.IsSingletonSystemTarget(GrainId.Type))
        {
            GrainInstruments.IncrementSystemTargetCounts(Constants.SystemTargetName(GrainId.Type));
        }
    }
}
```

**Key Characteristics**:

1. **Static Lifetime**
   - Created during silo startup
   - Never deactivated (live for silo's lifetime)
   - No idle timeout, no LRU eviction

2. **Deterministic Addressing**
   - `ActivationId = ActivationId.GetDeterministic(grainId)`
   - Same GrainId always maps to same ActivationId on a silo
   - No activation races, no "GetOrCreate" logic

3. **Single-Threaded Execution**
   - Each SystemTarget has its own `WorkItemGroup`
   - WorkItemGroup provides single-threaded message processing
   - Similar to actor mailbox pattern

4. **No Activation Lifecycle**
   - `Activate()` is a no-op (line 327)
   - `Deactivate()` is a no-op (line 330)
   - `Deactivated` returns `Task.CompletedTask` (line 333)

5. **No Migration**
   - `Rehydrate()` disposes context (line 345-349)
   - `Migrate()` is a no-op (line 351-354)
   - SystemTargets are tied to their silo

6. **Component-Based**
   - `_components` dictionary for extensions
   - `GetComponent<T>()` / `SetComponent<T>()` pattern
   - Similar to ActivationData's component model

### SystemTarget vs Regular Grain

| Aspect | SystemTarget | Regular Grain (Activation) |
|--------|--------------|----------------------------|
| **Lifetime** | Static (silo lifetime) | Dynamic (created on-demand) |
| **Deactivation** | Never | Idle timeout, LRU, explicit |
| **Addressing** | Deterministic ActivationId | Random ActivationId |
| **Activation** | Constructor only | OnActivateAsync lifecycle |
| **Migration** | Not supported | Supported |
| **Use Case** | Infrastructure services | Application logic |
| **Examples** | GrainDirectory, Membership | User-defined grains |

### Location Transparency: The Critical Difference

**Regular grains** enjoy full location transparency, while **SystemTargets** are explicitly silo-bound. This is the most important architectural distinction between the two.

#### Regular Grains: Full Location Transparency

**From application developer perspective**:

```csharp
// Get grain reference - NO silo address specified
var account = grainFactory.GetGrain<IAccountGrain>("alice");

// Call the grain - developer doesn't know or care where it runs
var balance = await account.GetBalance();
```

**What Orleans does automatically**:
1. **Grain directory lookup**: Which silo (if any) has this grain activated?
2. **Placement decision** (if not activated): Pick best silo based on placement strategy
3. **Activation** (if needed): Create grain instance, run `OnActivateAsync()`
4. **Message routing**: Send call to correct silo (may be local or remote)
5. **Migration** (if needed): Move grain to different silo, update directory

**Developer sees**: Just a grain ID (`"alice"`), no silo address
**Orleans provides**: Automatic location discovery and routing

#### SystemTargets: Silo-Specific Addressing

**From code perspective**:

```csharp
// SystemTarget constructor - GrainId INCLUDES silo address
internal SystemTarget(GrainType grainType, SystemTargetShared shared)
    : this(SystemTargetGrainId.Create(grainType, shared.SiloAddress), shared)
{
    // ActivationId is deterministic: same type + same silo = same ActivationId
    ActivationId = ActivationId.GetDeterministic(grainId.GrainId);
    ActivationAddress = GrainAddress.GetAddress(Silo, _id.GrainId, ActivationId);
}
```

**Key insight**: `SystemTargetGrainId.Create(grainType, shared.SiloAddress)`

The SystemTarget's grain ID **includes the silo address**. This means:

**To call a SystemTarget, you must specify**:
1. The SystemTarget type
2. **Which silo it's on**

**Example** (conceptual - this is internal Orleans code):
```csharp
// Regular grain - no silo specified
var grain = grainFactory.GetGrain<IAccountGrain>("alice");

// SystemTarget - MUST specify target silo
var catalog = GetSystemTarget<ICatalog>(targetSiloAddress);
```

**Why no location transparency?**
1. **No grain directory**: SystemTargets aren't in the grain directory
2. **No placement**: No decision about where to create them - they're created at silo startup
3. **No migration**: They can't move (`Migrate()` is a no-op)
4. **Silo-local state**: They manage silo-specific resources

#### Practical Implications

**Regular Grain Call Flow**:
```
Developer: grainFactory.GetGrain<IAccount>("alice").GetBalance()
    ↓
1. Grain directory: Lookup "alice" → find silo address (or null)
2. If not activated: Pick silo, activate grain
3. Route message to target silo
4. Execute on grain's WorkItemGroup
5. Return result
```

**SystemTarget Call Flow**:
```
Orleans internal code: GetSystemTarget<Catalog>(siloX).GetOrCreateActivation(...)
    ↓
1. NO directory lookup - caller already knows silo
2. Directly route to siloX
3. Execute on SystemTarget's WorkItemGroup
4. Return result
```

#### Why This Design?

SystemTargets are **infrastructure services that manage silo-local resources**:

**Examples**:
- **Catalog** - Manages activations **on this silo**
- **ActivationDirectory** - Registry of activations **on this silo**
- **MessageCenter** - Routes messages **for this silo**
- **GrainDirectory** (partition) - Manages directory entries **owned by this silo**

These services are **inherently silo-specific**. Making them location-transparent would be:
- **Unnecessary**: They always run locally on their silo
- **Incorrect**: You need to call the Catalog **on the specific silo** you care about
- **Inefficient**: Would require directory lookups for infrastructure calls

**When you need to call another silo's Catalog**: You explicitly address it by silo address, because you're asking "What activations does **silo X** have?"

**When you call an account grain**: You don't care which silo it's on, you just want "Alice's account" - Orleans figures out the location.

### Location Transparency Summary

| Aspect | Regular Grains | SystemTargets |
|--------|---------------|---------------|
| **Developer specifies** | Grain ID only | Grain type + Silo address |
| **Grain directory** | Yes - dynamic lookup | No - deterministic addressing |
| **Placement** | Dynamic (placement strategy) | Static (created at silo startup) |
| **Migration** | Supported | Not supported |
| **Call routing** | Orleans finds location | Caller specifies location |
| **Typical callers** | Applications, other grains | Orleans internal infrastructure |
| **Example** | `GetGrain<IAccount>("alice")` | `GetSystemTarget<Catalog>(siloAddress)` |

**For Moonpool**: This distinction is critical. System actors (like Catalog) should require node-address specification, while regular actors should be fully location-transparent. This matches Orleans' design and prevents confusion about what's happening under the hood.

### SystemTargetShared: Shared Dependencies

**Code**: `SystemTargetShared.cs:12-41`

SystemTargets share common dependencies via `SystemTargetShared`:

```csharp
internal sealed class SystemTargetShared(
    InsideRuntimeClient runtimeClient,
    ILocalSiloDetails localSiloDetails,
    ILoggerFactory loggerFactory,
    IOptions<SchedulingOptions> schedulingOptions,
    GrainReferenceActivator grainReferenceActivator,
    ITimerRegistry timerRegistry,
    ActivationDirectory activations)
{
    private readonly ILogger<WorkItemGroup> _workItemGroupLogger =
        loggerFactory.CreateLogger<WorkItemGroup>();
    private readonly ILogger<ActivationTaskScheduler> _activationTaskSchedulerLogger =
        loggerFactory.CreateLogger<ActivationTaskScheduler>();

    public SiloAddress SiloAddress => localSiloDetails.SiloAddress;
    public ILoggerFactory LoggerFactory => loggerFactory;
    public GrainReferenceActivator GrainReferenceActivator => grainReferenceActivator;
    public ITimerRegistry TimerRegistry => timerRegistry;
    public RuntimeMessagingTrace MessagingTrace => new(loggerFactory);
    public InsideRuntimeClient RuntimeClient => runtimeClient;
    public ActivationDirectory ActivationDirectory => activations;

    public WorkItemGroup CreateWorkItemGroup(SystemTarget systemTarget)
    {
        ArgumentNullException.ThrowIfNull(systemTarget);
        return new WorkItemGroup(
            systemTarget,
            _workItemGroupLogger,
            _activationTaskSchedulerLogger,
            schedulingOptions);
    }
}
```

**Pattern**: Shared dependencies pattern

**Why?**
- **Reduce allocations**: Share common dependencies across all SystemTargets
- **Dependency injection**: Resolved once from DI container
- **WorkItemGroup factory**: Create WorkItemGroups with consistent configuration

**Shared Dependencies**:
- `InsideRuntimeClient` - Make grain calls from SystemTarget
- `SiloAddress` - This silo's address
- `LoggerFactory` - Create loggers
- `GrainReferenceActivator` - Create grain references
- `ITimerRegistry` - Register timers
- `ActivationDirectory` - Register/lookup activations

### SystemTarget Message Processing

**Code**: `SystemTarget.cs:134-144, 303-321`

```csharp
internal void HandleNewRequest(Message request)
{
    _running = request;
    RuntimeClient.Invoke(this, request).Ignore();
}

internal void HandleResponse(Message response)
{
    _running = response;
    RuntimeClient.ReceiveResponse(response);
}

// IGrainContext.ReceiveMessage implementation
public void ReceiveMessage(object message)
{
    var msg = (Message)message;
    switch (msg.Direction)
    {
        case Message.Directions.Request:
        case Message.Directions.OneWay:
            {
                MessagingTrace.OnEnqueueMessageOnActivation(msg, this);
                var workItem = new RequestWorkItem(this, msg);
                WorkItemGroup.QueueWorkItem(workItem);
                break;
            }

        default:
            LogInvalidMessage(_logger, msg);
            break;
    }
}
```

**Message Flow**:

1. **Message arrives**: Router calls `ReceiveMessage(message)`
2. **Enqueue work item**: Create `RequestWorkItem`, queue to `WorkItemGroup`
3. **WorkItemGroup processes**: Single-threaded execution
4. **HandleNewRequest**: Track `_running` message, invoke via `RuntimeClient`
5. **Response handling**: `HandleResponse` routes responses to callback registry

**Key Insight**: SystemTargets use the **same message processing infrastructure** as regular grains:
- WorkItemGroup for single-threaded execution
- RequestWorkItem for message wrapping
- RuntimeClient for invocation

**Difference**: No reentrancy logic, no activation lifecycle, no deactivation

### SystemTarget Timers

**Code**: `SystemTarget.cs:163-221, 356-371`

SystemTargets can register timers just like grains:

```csharp
public IGrainTimer RegisterTimer(Func<object, Task> callback, object state,
    TimeSpan dueTime, TimeSpan period)
{
    ArgumentNullException.ThrowIfNull(callback);
    var timer = _shared.TimerRegistry.RegisterGrainTimer(
        this,
        static (state, _) => state.Callback(state.State),
        (Callback: callback, State: state),
        new() { DueTime = dueTime, Period = period, Interleave = true });
    return timer;
}

public IGrainTimer RegisterGrainTimer<TState>(
    Func<TState, CancellationToken, Task> callback, TState state,
    TimeSpan dueTime, TimeSpan period)
{
    CheckRuntimeContext();
    ArgumentNullException.ThrowIfNull(callback);
    var timer = _shared.TimerRegistry.RegisterGrainTimer(
        this, callback, state,
        new() { DueTime = dueTime, Period = period, Interleave = true });
    return timer;
}

void IGrainTimerRegistry.OnTimerCreated(IGrainTimer timer)
{
    lock (_timers) { _timers.Add(timer); }
}

void IGrainTimerRegistry.OnTimerDisposed(IGrainTimer timer)
{
    lock (_timers) { _timers.Remove(timer); }
}

private void StopAllTimers()
{
    List<IGrainTimer> timers;
    lock (_timers)
    {
        timers = _timers.ToList();
        _timers.Clear();
    }

    foreach (var timer in timers)
    {
        timer.Dispose();
    }
}
```

**Pattern**: Timer registry with cleanup

**Key Features**:
- **Interleave = true**: Timer callbacks always interleave
- **Tracked**: All timers tracked in `_timers` HashSet
- **Cleanup**: `Dispose()` stops all timers
- **Thread-safe**: Lock around `_timers` access

**Use Cases**:
- Periodic health checks
- Background cleanup
- Watchdog timers
- Metrics collection

### SystemTarget Disposal

**Code**: `SystemTarget.cs:335-343`

```csharp
public void Dispose()
{
    if (!Constants.IsSingletonSystemTarget(GrainId.Type))
    {
        GrainInstruments.DecrementSystemTargetCounts(Constants.SystemTargetName(GrainId.Type));
    }

    StopAllTimers();
}
```

**Disposal**:
1. Update metrics (decrement count)
2. Stop all timers
3. **Note**: WorkItemGroup not explicitly disposed (managed by scheduler)

**When called?** Typically only during silo shutdown when SystemTarget is garbage collected.

---

## GrainServices: Specialized SystemTargets

### GrainService Overview

**Code**: `GrainService.cs:13-58`

GrainServices extend SystemTargets with **consistent ring participation**:

```csharp
public abstract partial class GrainService : SystemTarget, IRingRangeListener, IGrainService
{
    private readonly IConsistentRingProvider ring;
    private readonly string typeName;
    private GrainServiceStatus status;

    protected CancellationTokenSource StoppedCancellationTokenSource { get; }
    protected int RangeSerialNumber { get; private set; }
    protected IRingRange RingRange { get; private set; }
    protected GrainServiceStatus Status { get; set; }

    internal GrainService(GrainId grainId, IConsistentRingProvider ringProvider,
        SystemTargetShared shared)
        : base(SystemTargetGrainId.Create(grainId.Type, shared.SiloAddress), shared)
    {
        typeName = this.GetType().FullName;
        Logger = shared.LoggerFactory.CreateLogger(typeName);

        ring = ringProvider;
        StoppedCancellationTokenSource = new CancellationTokenSource();
    }

    public virtual Task Init(IServiceProvider serviceProvider)
    {
        return Task.CompletedTask;
    }
}
```

**Additional Features Beyond SystemTarget**:

1. **Consistent Ring Integration**
   - `IRingRangeListener` interface
   - Notified when ring range changes
   - Used for partitioning work across silos

2. **Three-Phase Lifecycle**
   - `Init()` - Initialization (called in RuntimeGrainServices stage)
   - `Start()` - Begin operation (called in Active stage)
   - `Stop()` - Shutdown (called during silo stop)

3. **Ring Range Tracking**
   - `RingRange` - Current range owned
   - `RangeSerialNumber` - Monotonic version number
   - Updated on cluster topology changes

4. **Status State Machine**
   - `Booting` → `Started` → `Stopped`
   - Lifecycle hooks called on transitions

### GrainService Lifecycle

**Code**: `GrainService.cs:71-142`

```csharp
public virtual Task Init(IServiceProvider serviceProvider)
{
    return Task.CompletedTask;
}

public virtual Task Start()
{
    RingRange = ring.GetMyRange();
    LogInformationServiceStarting(Logger, this.typeName, Silo, new(Silo), RingRange);

    // Background initialization
    StartInBackground().Ignore();

    return Task.CompletedTask;
}

protected virtual Task StartInBackground()
{
    Status = GrainServiceStatus.Started;
    return Task.CompletedTask;
}

private void OnStatusChange(GrainServiceStatus oldStatus, GrainServiceStatus newStatus)
{
    if (oldStatus != GrainServiceStatus.Started && newStatus == GrainServiceStatus.Started)
    {
        // Subscribe to ring changes when started
        ring.SubscribeToRangeChangeEvents(this);
    }
    if (oldStatus != GrainServiceStatus.Stopped && newStatus == GrainServiceStatus.Stopped)
    {
        // Unsubscribe when stopped
        ring.UnSubscribeFromRangeChangeEvents(this);
    }
}

public virtual Task Stop()
{
    StoppedCancellationTokenSource.Cancel();

    LogInformationServiceStopping(Logger, typeName);
    Status = GrainServiceStatus.Stopped;

    return Task.CompletedTask;
}

// IRingRangeListener implementation
void IRingRangeListener.RangeChangeNotification(IRingRange oldRange, IRingRange newRange, bool increased)
{
    this.WorkItemGroup.QueueTask(() => OnRangeChange(oldRange, newRange, increased), this).Ignore();
}

public virtual Task OnRangeChange(IRingRange oldRange, IRingRange newRange, bool increased)
{
    LogInformationRangeChanged(Logger, oldRange, newRange, increased);
    RingRange = newRange;
    RangeSerialNumber++;

    return Task.CompletedTask;
}
```

**Lifecycle Flow**:

1. **Construction**: GrainService created (inherits SystemTarget construction)
2. **Registration**: Added to ActivationDirectory (by Silo.RegisterGrainService)
3. **Init**: Called in RuntimeGrainServices stage
4. **Start**: Called in Active stage
   - Get initial ring range
   - Subscribe to ring changes
   - Background initialization
5. **Runtime**: Process messages, react to ring changes
6. **Stop**: Called during silo shutdown
   - Cancel ongoing work
   - Unsubscribe from ring changes

**Why three phases?**
- **Init**: Register, setup dependencies
- **Start**: Begin work (but don't block silo startup)
- **StartInBackground**: Heavy initialization after silo is Active

### Ring Range Participation

**Pattern**: Partitioned services using consistent hashing

**Use Case**: Distribute work across silos based on hash ring

**Example**: Reminder service partitions reminders by grain ID hash
- Each silo owns a range of the hash ring
- When topology changes, ranges rebalance
- `OnRangeChange` notified to adjust ownership

**Code Flow**:
1. Cluster topology changes (silo joins/leaves)
2. Consistent ring recalculates ranges
3. Ring provider calls `RangeChangeNotification` on all listeners
4. GrainService queues `OnRangeChange` to WorkItemGroup
5. Service adjusts its work (e.g., drop reminders no longer owned, acquire new ones)

**Why queue through WorkItemGroup?**
- Single-threaded: No concurrent range updates
- Ordered: Range changes process in order
- Isolated: Doesn't block ring provider

### GrainService Examples in Orleans

1. **Reminder Service** (`ReminderServiceBase`)
   - Partitions reminders across silos
   - Reacts to ring changes to hand off/acquire reminders

2. **Grain Directory** (`GrainDirectoryPartition`)
   - Partitions directory entries by grain ID hash
   - Hands off entries when ring changes

3. **Type Management** (`ClusterManifestSystemTarget`)
   - Manages grain type metadata
   - Not partitioned (singleton-style)

4. **Placement** (`DeploymentLoadPublisher`)
   - Publishes silo load metrics
   - Each silo publishes its own load

---

## Patterns for Moonpool

### ✅ Adopt These Patterns

1. **Staged Lifecycle Bootstrap**
   ```rust
   pub enum LifecycleStage {
       RuntimeInitialize,
       RuntimeServices,
       BecomeActive,
       Active,
   }

   pub trait LifecycleParticipant {
       fn on_start(&mut self, stage: LifecycleStage) -> Result<()>;
       fn on_stop(&mut self, stage: LifecycleStage) -> Result<()>;
   }

   // Lifecycle manager executes stages in order
   impl Lifecycle {
       pub async fn start(&mut self) -> Result<()> {
           for stage in [RuntimeInitialize, RuntimeServices, BecomeActive, Active] {
               for participant in &mut self.participants {
                   participant.on_start(stage).await?;
               }
           }
           Ok(())
       }
   }
   ```

2. **SystemTarget Pattern (Static Actors)**
   ```rust
   pub struct SystemTarget {
       id: ActorId,
       mailbox: mpsc::UnboundedReceiver<Message>,
       state: Box<dyn Any>,  // Service-specific state
   }

   impl SystemTarget {
       // Deterministic ID from type
       pub fn new(type_id: SystemTargetType, silo_address: Address) -> Self {
           let id = ActorId::system_target(type_id, silo_address);
           // ...
       }

       // Never deactivated - lives forever
       pub async fn run(mut self) {
           while let Some(msg) = self.mailbox.recv().await {
               self.handle_message(msg).await;
           }
       }
   }
   ```

3. **WorkItemGroup Pattern (Single-Threaded Execution)**
   ```rust
   pub struct WorkItemGroup {
       mailbox: mpsc::UnboundedReceiver<WorkItem>,
   }

   impl WorkItemGroup {
       pub async fn run(mut self) {
           // Single-threaded message loop
           while let Some(item) = self.mailbox.recv().await {
               item.execute().await;
           }
       }
   }

   // All lifecycle operations queue through lifecycle SystemTarget
   pub struct LifecycleCoordinator {
       work_item_group: WorkItemGroup,
   }
   ```

4. **Lifecycle Participant Discovery**
   ```rust
   // Components register themselves for lifecycle stages
   pub struct ActorSystem {
       participants: Vec<Box<dyn LifecycleParticipant>>,
   }

   impl ActorSystem {
       pub fn add_participant(&mut self, participant: impl LifecycleParticipant + 'static) {
           self.participants.push(Box::new(participant));
       }

       pub async fn start(&mut self) -> Result<()> {
           for stage in LifecycleStage::all() {
               for participant in &mut self.participants {
                   participant.on_start(stage).await?;
               }
           }
           Ok(())
       }
   }
   ```

5. **Symmetric Lifecycle (Start/Stop)**
   ```rust
   impl LifecycleParticipant for MessageBus {
       async fn on_start(&mut self, stage: LifecycleStage) -> Result<()> {
           match stage {
               LifecycleStage::RuntimeInitialize => self.initialize(),
               LifecycleStage::Active => self.start_accepting_messages(),
               _ => Ok(()),
           }
       }

       async fn on_stop(&mut self, stage: LifecycleStage) -> Result<()> {
           // Reverse order: Active stops first, RuntimeInitialize stops last
           match stage {
               LifecycleStage::Active => self.stop_accepting_messages(),
               LifecycleStage::RuntimeInitialize => self.shutdown(),
               _ => Ok(()),
           }
       }
   }
   ```

6. **Timeout Enforcement**
   ```rust
   pub async fn init_grain_service(&self, service: &mut GrainService) -> Result<()> {
       tokio::time::timeout(
           self.init_timeout,
           service.init()
       ).await
           .map_err(|_| Error::InitializationTimeout)?
   }
   ```

### ⚠️ Adapt These Patterns

1. **Dependency Injection via DI Container**
   - Orleans: `IServiceProvider.GetRequiredService<T>()`
   - Moonpool: Use Provider traits, manual construction, or simplified DI
   ```rust
   // Instead of DI container, use Provider traits
   pub struct SystemTargetShared {
       time_provider: Arc<dyn TimeProvider>,
       network_provider: Arc<dyn NetworkProvider>,
       task_provider: Arc<dyn TaskProvider>,
   }
   ```

2. **GrainServices with Ring Participation**
   - Orleans: Complex consistent hashing, range notifications
   - Moonpool: Start simple (single-node), add partitioning later if needed
   ```rust
   // Phase 12: No partitioning yet
   pub struct GrainService {
       // Just a SystemTarget initially
   }

   // Future: Add partitioning
   pub trait RingRangeListener {
       fn on_range_change(&mut self, old: Range, new: Range);
   }
   ```

3. **SystemTargetShared Pattern**
   - Orleans: Single shared instance for all SystemTargets
   - Moonpool: Use Arc for shared dependencies, simpler than Orleans
   ```rust
   #[derive(Clone)]
   pub struct SystemTargetShared {
       silo_address: Address,
       activation_directory: Arc<ActivationDirectory>,
       message_bus: Arc<MessageBus>,
   }
   ```

4. **LifecycleSchedulingSystemTarget**
   - Orleans: Dummy SystemTarget just for WorkItemGroup
   - Moonpool: Could use a dedicated task instead
   ```rust
   // Option 1: Dedicated task (simpler)
   pub struct LifecycleCoordinator {
       task: JoinHandle<()>,
   }

   // Option 2: SystemTarget (matches Orleans, more consistent)
   pub struct LifecycleSchedulingSystemTarget {
       mailbox: mpsc::UnboundedReceiver<LifecycleCommand>,
   }
   ```

### ❌ Avoid These Patterns

1. **Complex DI Container Integration**
   - Orleans: Heavy use of `IServiceProvider`, reflection, service discovery
   - Moonpool: Keep dependencies explicit, use Provider traits
   ```rust
   // ❌ Avoid
   trait ServiceProvider {
       fn get_service<T>(&self) -> Option<Arc<T>>;
   }

   // ✅ Use
   struct Dependencies {
       time: Arc<dyn TimeProvider>,
       network: Arc<dyn NetworkProvider>,
   }
   ```

2. **Debugger-Specific Timeout Extensions**
   - Orleans: Extends timeouts if debugger attached
   - Moonpool: Not needed in simulation environment
   ```rust
   // ❌ Avoid
   let timeout = if is_debugger_attached() {
       Duration::from_minutes(10)
   } else {
       config.timeout
   };

   // ✅ Use
   let timeout = config.timeout;  // Deterministic always
   ```

3. **Multiple SystemStatus Enums**
   - Orleans: `SystemStatus` with many states (Creating, Created, Starting, Running, ShuttingDown, Stopping, Terminated)
   - Moonpool: Simplify to essential states
   ```rust
   // ✅ Simpler state machine
   pub enum ActorSystemStatus {
       Initializing,
       Running,
       Stopping,
       Stopped,
   }
   ```

4. **GC and Platform-Specific Checks**
   - Orleans: Checks for Server GC, logs warnings
   - Moonpool: Not applicable to Rust
   ```rust
   // ❌ No equivalent needed in Rust
   ```

5. **Lock-Based Status Transitions**
   - Orleans: `lock (lockable) { SystemStatus = ... }`
   - Moonpool: Use message passing or atomic state
   ```rust
   // ✅ Message-based state machine
   pub enum LifecycleCommand {
       Start,
       Stop,
   }

   // State machine runs in single task, no lock needed
   ```

---

## Critical Insights for Moonpool Phase 12

### 1. Lifecycle is the Foundation

The lifecycle system is how Orleans **coordinates initialization** across dozens of components:
- Catalog (activation management)
- MessageCenter (networking)
- GrainDirectory (location service)
- Membership (cluster management)
- Reminder Service (timers)
- And many more...

**For Moonpool**: Implement lifecycle **first** (Phase 12 Step 1), then build other components as lifecycle participants.

### 2. SystemTargets Enable Infrastructure Services

SystemTargets solve a bootstrapping problem: How do you implement actor infrastructure (directory, messaging, catalog) **using actors themselves**?

**Answer**: SystemTargets are actors that:
- Never deactivate (no lifecycle)
- Have deterministic IDs (no placement)
- Are created at silo startup (no on-demand activation)

**For Moonpool**: ActorCatalog, MessageBus, and other infrastructure should be SystemTargets.

### 3. WorkItemGroup Provides Deterministic Execution

Orleans uses WorkItemGroup for:
- Single-threaded message processing (no races within an actor)
- Ordered execution (FIFO message delivery)
- RuntimeContext isolation (each actor has its own context)

**For Moonpool**: WorkItemGroup maps to `spawn_local` task with message queue:
```rust
pub async fn run_actor(mut mailbox: mpsc::UnboundedReceiver<Message>) {
    while let Some(msg) = mailbox.recv().await {
        handle_message(msg).await;
    }
}
```

### 4. Two-Phase GrainService Startup (Init vs Start)

Orleans separates:
- **Init**: Heavy setup, registration
- **Start**: Begin work

**Why?** Silo startup is slow if you do everything in one phase. Init can run while silo is still starting, Start happens once silo is ready.

**For Moonpool**: Not critical initially (single-node), but consider for distributed setup.

### 5. Symmetric Lifecycle is Critical

Every `onStart` must have a corresponding `onStop`:
- Allocate → Free
- Register → Unregister
- Subscribe → Unsubscribe
- Start → Stop

**For Moonpool**: Test lifecycle with `start() -> stop() -> start()` repeatedly. Ensure no leaks.

### 6. Lifecycle Operations Must Be Ordered

Orleans queues all lifecycle operations through `LifecycleSchedulingSystemTarget.WorkItemGroup` to ensure:
- No concurrent lifecycle operations
- Deterministic order
- Clean error propagation

**For Moonpool**: Critical for simulation determinism. Use Provider traits to ensure lifecycle runs through simulation scheduler.

---

## Summary

Orleans silo bootstrap is built on:
- **Staged lifecycle**: Ordered phases (RuntimeInitialize → Active)
- **Participant pattern**: Components register for lifecycle stages
- **SystemTargets**: Static infrastructure actors
- **WorkItemGroup**: Single-threaded execution contexts
- **Symmetric shutdown**: Stop reverses Start

SystemTargets differ from regular grains:
- Static lifetime vs dynamic activation
- Deterministic addressing vs random ActivationId
- No lifecycle vs OnActivateAsync/OnDeactivateAsync
- Infrastructure vs application logic

GrainServices extend SystemTargets with:
- Consistent ring participation
- Range change notifications
- Partitioned work distribution

Key architectural principles:
- Two-phase initialization (construction vs startup)
- Lifecycle discovery via DI container
- Ordered execution via WorkItemGroup
- Timeout enforcement on critical operations
- Symmetric lifecycle (start/stop)

For Moonpool Phase 12: Implement lifecycle system first, create SystemTargets for infrastructure (Catalog, MessageBus), use Provider traits for testability, keep initial version simple (no partitioning, no complex DI).
