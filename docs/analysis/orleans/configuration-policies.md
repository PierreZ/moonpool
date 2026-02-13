# Configuration and Lifecycle Policies

## Reference Files
- `GrainCollectionOptions.cs:1-78` - Collection, timeout, and memory pressure configuration
- `DeactivationReason.cs:1-71` - Deactivation reason struct and descriptions
- `IGrainBase.cs:242-311` - DeactivationReasonCode enum definition
- `ActivationData.cs` - Idle detection and collection implementation (referenced throughout)

## Overview

Orleans provides **configurable lifecycle policies** for managing grain activations:
- **Collection timing**: How often to check for idle grains
- **Collection age**: How long idle before eligible for collection
- **Activation/deactivation timeouts**: Time limits for lifecycle operations
- **Per-grain-type configuration**: Custom policies for specific grain types
- **Deactivation reasons**: Structured codes for why grains deactivate
- **Memory pressure shedding**: Automatic activation shedding under high memory usage

The design provides:
- ✅ **Configurable aging**: Different idle timeouts per grain type
- ✅ **Timeout protection**: Prevent runaway activation/deactivation
- ✅ **Diagnostic reasons**: Structured deactivation tracking
- ✅ **Graceful degradation**: Timeouts don't crash, just deactivate
- ✅ **Memory pressure relief**: Shed activations when memory is constrained

---

## GrainCollectionOptions: Configuration

### Purpose
Central configuration for grain lifecycle timing, policies, and memory pressure management.

**Code**: `GrainCollectionOptions.cs:9-77`

```csharp
public class GrainCollectionOptions
{
    // Regulates the periodic collection of inactive grains
    public TimeSpan CollectionQuantum { get; set; } = DEFAULT_COLLECTION_QUANTUM;
    public static readonly TimeSpan DEFAULT_COLLECTION_QUANTUM = TimeSpan.FromMinutes(1);

    // Default period of inactivity necessary for collection
    public TimeSpan CollectionAge { get; set; } = TimeSpan.FromMinutes(15);

    // Per-grain-type collection age overrides
    public Dictionary<string, TimeSpan> ClassSpecificCollectionAge { get; set; } = new Dictionary<string, TimeSpan>();

    // Timeout before giving up when trying to activate a grain
    public TimeSpan ActivationTimeout { get; set; } = DEFAULT_ACTIVATION_TIMEOUT;
    public static readonly TimeSpan DEFAULT_ACTIVATION_TIMEOUT = TimeSpan.FromSeconds(30);

    // Timeout before giving up when trying to deactivate a grain
    public TimeSpan DeactivationTimeout { get; set; } = DEFAULT_DEACTIVATION_TIMEOUT;
    public static readonly TimeSpan DEFAULT_DEACTIVATION_TIMEOUT = TimeSpan.FromSeconds(30);

    // Memory pressure activation shedding (v10 addition)
    public bool EnableActivationSheddingOnMemoryPressure { get; set; }
    public TimeSpan MemoryUsagePollingPeriod { get; set; } = TimeSpan.FromSeconds(5);
    public double MemoryUsageLimitPercentage { get; set; } = 80;
    public double MemoryUsageTargetPercentage { get; set; } = 75;
}
```

### Collection Quantum

**Purpose**: How frequently the collection process runs

**Default**: 1 minute (line 14, constant on line 19)

**What it controls**:
- Background task wakes up every `CollectionQuantum`
- Scans all activations for idle ones
- Initiates deactivation for eligible grains

**Tuning considerations**:
- **Shorter quantum**: More responsive cleanup, higher CPU overhead
- **Longer quantum**: Less overhead, slower memory reclamation
- **Typical values**: 30 seconds to 5 minutes

**Implementation**: Likely a timer or periodic task in activation collector

### Collection Age

**Purpose**: Minimum idle time before grain is eligible for collection

**Default**: 15 minutes (line 24)

**What it means**:
- Grain must have no activity for at least `CollectionAge`
- After this time, grain becomes eligible for collection
- Next collection quantum cycle will deactivate it

**Activity definition**: (from ActivationData.cs)
- Processing a message
- Timer firing
- Any operation that calls `ResetKeepAliveRequest()`

**Tuning considerations**:
- **Shorter age**: Lower memory usage, more activation churn
- **Longer age**: Higher memory usage, better cache hit rate
- **Typical values**: 5 minutes to 2 hours

**Special value**: `Timeout.InfiniteTimeSpan` disables collection (line 128 in ActivationData.cs)

### Class-Specific Collection Age

**Purpose**: Override collection age for specific grain types

**Example configuration**:
```csharp
options.ClassSpecificCollectionAge["MyNamespace.LongLivedGrain"] = TimeSpan.FromHours(2);
options.ClassSpecificCollectionAge["MyNamespace.ShortLivedGrain"] = TimeSpan.FromMinutes(1);
```

**Why per-type?**
- Different grains have different access patterns
- Hot grains: Short timeout (always active, reclaim quickly if not)
- Cache grains: Long timeout (expensive to reload)
- Session grains: Match session timeout

**Lookup**: By grain type name (fully qualified)

### Activation Timeout

**Purpose**: Maximum time allowed for activation to complete

**Default**: 30 seconds (line 34, constant on line 39)

**What it limits**:
- Grain directory registration
- Lifecycle `OnStart()` callback
- Grain `OnActivateAsync()` callback
- Total time from creation to `Valid` state

**Triggered when exceeded**: (ActivationData.cs:1459, applied via `CancelAfter`)
- Cancellation token canceled
- Activation fails
- Transitions to `Deactivating`
- Messages rejected or forwarded

**Why timeout?**
- Prevent hung activations blocking resources
- Detect deadlocks or infinite loops in user code
- Ensure system remains responsive

**Tuning considerations**:
- **Shorter timeout**: Faster failure detection, risk false positives
- **Longer timeout**: Accommodate slow operations (external DB loads)
- **Typical values**: 10-60 seconds

### Deactivation Timeout

**Purpose**: Maximum time allowed for deactivation to complete

**Default**: 30 seconds (line 45, constant on line 50)

**What it limits**:
- Lifecycle `OnStop()` callback
- Grain `OnDeactivateAsync()` callback
- Timer disposal
- Migration (if applicable)
- Directory unregistration
- Total time from `Deactivating` to `Invalid` state

**Triggered when exceeded**: (ActivationData.cs:574, applied via `CancelAfter`)
- Cancellation token canceled
- Deactivation forced
- Activation may be marked "stuck" (lines 1045-1053)

**Why timeout?**
- Prevent hung deactivations blocking shutdown
- Detect deadlocks in cleanup code
- Ensure system can terminate gracefully

**Tuning considerations**:
- **Shorter timeout**: Faster shutdown, risk incomplete cleanup
- **Longer timeout**: Allow complex cleanup (flush caches, close connections)
- **Typical values**: 10-120 seconds

### Memory Pressure Shedding (v10 Addition)

**Purpose**: Automatically deactivate grains when memory usage is too high

**Code**: `GrainCollectionOptions.cs:52-77`

**Configuration**:
- `EnableActivationSheddingOnMemoryPressure`: Opt-in flag (default: `false`)
- `MemoryUsagePollingPeriod`: How often to check memory usage (default: 5 seconds)
- `MemoryUsageLimitPercentage`: Trigger threshold (default: 80%)
- `MemoryUsageTargetPercentage`: Target after shedding (default: 75%)

**Behavior**:
- When memory usage exceeds `MemoryUsageLimitPercentage`, the collector begins shedding activations
- Shedding continues until memory usage drops below `MemoryUsageTargetPercentage`
- Deactivated with `DeactivationReasonCode.HighMemoryPressure`
- Implementation in `ActivationCollector.cs`

---

## DeactivationReason: Structured Codes

### Purpose
Provide structured reason for why a grain is deactivating, used for:
- Logging and diagnostics
- Metrics and monitoring
- Conditional behavior (e.g., retry on transient failure)

**Code**: `DeactivationReason.cs:8-70`

```csharp
public readonly struct DeactivationReason
{
    public DeactivationReason(DeactivationReasonCode code, string text)
    {
        ReasonCode = code;
        Description = text;
        Exception = null;
    }

    public DeactivationReason(DeactivationReasonCode code, Exception? exception, string text)
    {
        ReasonCode = code;
        Description = text;
        Exception = exception;
    }

    public string Description { get; }
    public DeactivationReasonCode ReasonCode { get; }
    public Exception? Exception { get; }

    public override string ToString()
    {
        if (Exception is not null)
            return $"{ReasonCode}: {Description}. Exception: {Exception}";
        return $"{ReasonCode}: {Description}";
    }
}
```

**Three-part structure**:
1. **ReasonCode**: Enum identifying category
2. **Description**: Human-readable text
3. **Exception**: Optional (nullable) exception that caused deactivation

### Deactivation Reason Codes

**Code**: `IGrainBase.cs:242-311` (enum definition)

The full enum (`DeactivationReasonCode : byte`):

```csharp
public enum DeactivationReasonCode : byte
{
    None,                  // No reason provided
    ShuttingDown,          // Process is terminating
    ActivationFailed,      // Activation of the grain failed
    DirectoryFailure,      // Internal failure in grain directory
    ActivationIdle,        // Activation is idle (collection)
    ActivationUnresponsive,// Unresponsive to commands or requests
    DuplicateActivation,   // Another instance activated
    IncompatibleRequest,   // Request cannot be handled locally
    ApplicationError,      // An application error occurred
    ApplicationRequested,  // Application requested deactivation
    Migrating,             // Migrating to a new location
    RuntimeRequested,      // Runtime requested deactivation
    HighMemoryPressure,    // App running on low memory, forced deactivation
}
```

Based on usage in ActivationData.cs, the codes are used as follows:

**Application-Initiated** (ActivationData.cs:1812):
```csharp
DeactivationReasonCode.ApplicationRequested
// Description: "DeactivateOnIdle was called."
```
- User code called `DeactivateOnIdle()`
- Normal, expected deactivation

**Activation Lifecycle**:

1. **ActivationFailed** (ActivationData.cs:99, 1620)
   ```csharp
   // Description: "Error constructing grain instance." (line 99)
   // Description: "Failed to activate grain." (line 1620)
   ```
   - Grain constructor threw exception
   - `OnActivateAsync()` threw exception
   - Lifecycle `OnStart()` failed

2. **DirectoryFailure** (ActivationData.cs:1546)
   ```csharp
   // Description: "Failed to register activation in grain directory."
   ```
   - Grain directory registration failed
   - Network issue communicating with directory
   - Directory service unavailable

3. **DuplicateActivation** (ActivationData.cs:1512)
   ```csharp
   // Description: "This grain is active on another host ({address})."
   ```
   - Another activation already registered
   - Race during creation
   - Messages forwarded to existing activation

4. **ApplicationError** (ActivationData.cs:1629)
   ```csharp
   // Description: "Failed to activate grain."
   // Exception: The exception that occurred
   ```
   - Generic application error
   - Catch-all for unexpected exceptions during activation

**Migration** (ActivationData.cs:542, 988):
```csharp
DeactivationReasonCode.Migrating
// Description: "Migrating to a new location."

DeactivationReasonCode.IncompatibleRequest
// Description: "Received incompatible request for interface {type} version {version}.
//               This activation supports interface version {currentVersion}."
```
- Grain migrating to different silo
- Interface version mismatch detected

**System Issues**:

1. **ShuttingDown** (Catalog.cs:270)
   ```csharp
   // Description: "This process is terminating."
   ```
   - Silo shutting down
   - Graceful or forced termination
   - Note: Set in Catalog.cs, not ActivationData.cs; referenced at ActivationData.cs:1704

2. **ActivationUnresponsive** (ActivationData.cs:584, 1050-1051)
   ```csharp
   // Description: "Activation {this} has been processing request {request} since {duration} and is likely stuck."
   // Or: "{previous reason}. Activation {this} has been deactivating since {time} and is likely stuck"
   ```
   - Activation stuck processing message
   - Activation stuck deactivating
   - Exceeded `MaxRequestProcessingTime`

3. **ActivationIdle** (ActivationCollector.cs:407)
   ```csharp
   // Description: set dynamically by the activation collector
   ```
   - Grain collected due to idle timeout
   - Used by the ActivationCollector when collecting idle grains

4. **RuntimeRequested** (StatelessWorkerGrainContext.cs:243, 373)
   ```csharp
   // Description: "Worker deactivated due to inactivity."
   // Or: "Stateless worker grain context is being disposed."
   ```
   - Runtime-initiated deactivation
   - Used for stateless worker management

5. **HighMemoryPressure** (ActivationCollector.cs:393)
   ```csharp
   // Deactivated to relieve memory pressure
   ```
   - Memory usage exceeds configured limit
   - Forced deactivation to reduce memory footprint

### Usage Pattern in ActivationData

**Code**: Throughout ActivationData.cs

```csharp
// Initiate deactivation with reason (using condensed constructor syntax)
Deactivate(new(DeactivationReasonCode.ActivationFailed, sourceException, "Failed to activate grain."));

// Store reason for later inspection
DeactivationReason = reason;

// Use reason in logging
LogErrorActivatingGrain(_shared.Logger, DeactivationException, this);

// Check reason for conditional behavior
if (DeactivationReason.ReasonCode.IsTransientError())
{
    // Allow message forwarding with retry
}
```

**Transient vs Permanent** (defined in `IGrainBase.cs:313-319`):
- **Transient**: `DirectoryFailure` only
  - Messages may be retried
- **Permanent**: `DuplicateActivation`, `IncompatibleRequest`, `ActivationUnresponsive`, etc.
  - Messages rejected or forwarded without retry

---

## Idle Detection and Collection

### Idle Tracking

**Code**: ActivationData.cs:55, 440-445, 1302-1318

```csharp
private CoarseStopwatch _idleDuration;

public TimeSpan GetIdleness() => _idleDuration.Elapsed;

public bool IsStale() => GetIdleness() >= _shared.CollectionAgeLimit;

// Reset idle timer when message completes
private void OnCompletedRequest(Message message)
{
    lock (this)
    {
        _runningRequests.Remove(message);

        if (message.IsKeepAlive)
        {
            _idleDuration = CoarseStopwatch.StartNew();

            if (!_isInWorkingSet)
            {
                _isInWorkingSet = true;
                _shared.InternalRuntime.ActivationWorkingSet.OnActive(this);
            }
        }

        // The below logic only works for non-reentrant activations
        if (_blockingRequest is null || message.Equals(_blockingRequest))
        {
            _blockingRequest = null;
            _busyDuration = default;
        }
    }

    // Signal the message pump to see if there is another request which can be processed
    _workSignal.Signal();
}
```

**Idle tracking approach**:
1. Start idle timer when activation becomes inactive
2. Reset timer on each message completion (if `IsKeepAlive`)
3. Check `GetIdleness()` during collection scan
4. Deactivate if `IsStale()` returns true

**Keep-alive mechanism**:
- Not all messages reset idle timer
- `IsKeepAlive` defaults to `true`; suppressed via `MessageFlags.SuppressKeepAlive` flag (Message.cs:140-143)
- Prevents non-application messages from keeping grain alive

### Working Set Management

**Code**: ActivationData.cs:53, 873-883, 1314-1317

```csharp
private bool _isInWorkingSet = true;

bool IActivationWorkingSetMember.IsCandidateForRemoval(bool wouldRemove)
{
    const int IdlenessLowerBound = 10_000;  // 10 seconds
    lock (this)
    {
        var inactive = IsInactive && _idleDuration.ElapsedMilliseconds > IdlenessLowerBound;

        // This instance will remain in the working set if not pending removal or if currently active
        _isInWorkingSet = !wouldRemove || !inactive;
        return inactive;
    }
}
```

**Working set pattern**:
- **Working set**: Recently active grains kept in fast-access collection
- **Eviction**: Grains idle > 10 seconds removed from working set
- **Collection**: Working set scan is faster than full catalog scan

**Two-tier collection**:
1. **Fast path**: Scan working set for recently idle grains (10s+)
2. **Slow path**: Scan full catalog for long-idle grains (15min+)

**Performance**: Most grains are either very active or very idle, working set optimizes the common case

### KeepAliveUntil Extension

**Code**: ActivationData.cs:129, 447-465

```csharp
public DateTime KeepAliveUntil { get; set; } = DateTime.MinValue;

public void DelayDeactivation(TimeSpan timespan)
{
    if (timespan == TimeSpan.MaxValue || timespan == Timeout.InfiniteTimeSpan)
    {
        KeepAliveUntil = DateTime.MaxValue;
    }
    else if (timespan <= TimeSpan.Zero)
    {
        ResetKeepAliveRequest();  // Cancel extension
    }
    else
    {
        KeepAliveUntil = GrainRuntime.TimeProvider.GetUtcNow().UtcDateTime + timespan;
    }
}

public void ResetKeepAliveRequest() => KeepAliveUntil = DateTime.MinValue;
```

**Usage pattern**:
```csharp
// In grain code
DelayDeactivation(TimeSpan.FromMinutes(30));  // Keep alive for 30 more minutes
```

**Use cases**:
- Grain doing background work
- Holding external resource (connection, file)
- Caching expensive-to-compute data
- User session management

**Collection check**: Collector skips grain if `KeepAliveUntil > now`

**Reset**: Call with zero/negative timespan or `ResetKeepAliveRequest()`

### Exempt from Collection

**Code**: ActivationData.cs:128, 257

```csharp
public bool IsExemptFromCollection => _shared.CollectionAgeLimit == Timeout.InfiniteTimeSpan;

public TimeSpan CollectionAgeLimit => _shared.CollectionAgeLimit;
```

**When exempt**:
- Collection age set to infinite
- Configured via `GrainCollectionOptions.CollectionAge` per grain type
- Grain will never be collected due to idle timeout

**Still can deactivate**:
- Explicit `DeactivateOnIdle()` call
- Silo shutdown
- Error during message processing
- Manual deactivation
- High memory pressure (if enabled)

**Use cases**:
- Singleton grains
- System-level actors
- Coordinator grains

---

## Stuck Activation Detection

Orleans detects two types of "stuck" situations:

### 1. Stuck Processing Message

**Code**: ActivationData.cs:580-594, 955-960

```csharp
private void DeactivateStuckActivation()
{
    IsStuckProcessingMessage = true;
    var msg = $"Activation {this} has been processing request {_blockingRequest} since {_busyDuration} and is likely stuck.";
    var reason = new DeactivationReason(DeactivationReasonCode.ActivationUnresponsive, msg);

    // Mark as deactivating so messages are forwarded
    Deactivate(reason, cancellationToken: default);

    // Remove from catalog and directory
    UnregisterMessageTarget();
    _shared.InternalRuntime.GrainLocator.Unregister(Address, UnregistrationCause.Force).Ignore();
}

// Detection logic in RunMessageLoop (within MayInvokeRequest check)
if (_blockingRequest != null)
{
    var currentRequestActiveTime = _busyDuration.Elapsed;
    if (currentRequestActiveTime > _shared.MaxRequestProcessingTime && !IsStuckProcessingMessage)
    {
        DeactivateStuckActivation();
    }
}
```

**Detection criteria**:
- Activation has a blocking request
- Request has been running > `MaxRequestProcessingTime`
- Not already marked as stuck

**Response**:
1. Mark activation as stuck
2. Initiate deactivation
3. Unregister from catalog (new messages route to fresh activation)
4. Unregister from directory
5. Leave stuck activation dangling (will eventually GC)

**Why not kill?**
- Can't safely kill thread in managed runtime
- Request might be doing I/O, can't interrupt
- Safest: Create new activation, abandon old one

### 2. Stuck Deactivating

**Code**: ActivationData.cs:218-229 (property), 1041-1061 (detection in ProcessRequestsToInvalidActivation)

```csharp
private bool IsStuckDeactivating
{
    get => _extras?.IsStuckDeactivating ?? false;
    set { lock (this) { _extras ??= new(); _extras.IsStuckDeactivating = value; } }
}

// Detection in ProcessRequestsToInvalidActivation
if (State is ActivationState.Deactivating)
{
    var deactivatingTime = GrainRuntime.TimeProvider.GetUtcNow().UtcDateTime - DeactivationStartTime!.Value;
    if (deactivatingTime > _shared.MaxRequestProcessingTime && !IsStuckDeactivating)
    {
        IsStuckDeactivating = true;
        if (DeactivationReason.Description is { Length: > 0 } && DeactivationReason.ReasonCode != DeactivationReasonCode.ActivationUnresponsive)
        {
            DeactivationReason = new(DeactivationReasonCode.ActivationUnresponsive,
                $"{DeactivationReason.Description}. Activation {this} has been deactivating since {DeactivationStartTime.Value} and is likely stuck");
        }
    }

    if (!IsStuckDeactivating && !IsStuckProcessingMessage)
    {
        // Don't forward messages yet, still deactivating normally
        return;
    }
}
```

**Detection criteria**:
- Activation in `Deactivating` state
- Time since `DeactivationStartTime` > `MaxRequestProcessingTime`
- Not already marked as stuck

**Response**:
1. Mark as stuck deactivating
2. Update deactivation reason
3. Start forwarding queued messages (lines 1063-1073)
4. Allow new activation to be created

**Why forward messages?**
- Deactivation may never complete
- Queued messages would be lost
- Forward to new activation

**What about the stuck activation?**
- Continues trying to deactivate
- Will eventually complete or be GC'd
- Not in catalog, so not receiving new messages

### Configuration

**Code**: Defined in `GrainTypeSharedContext.cs:54-55`, sourced from `SiloMessagingOptions.cs:107-112`

**Actual defaults**:
- `MaxRequestProcessingTime`: **2 hours** (hard limit, triggers stuck detection)
  - From `SiloMessagingOptions.DEFAULT_MAX_REQUEST_PROCESSING_TIME = TimeSpan.FromHours(2)`
- `MaxWarningRequestProcessingTime`: **ResponseTimeout * 5** = **150 seconds** (soft limit, logs warning)
  - Computed as `messagingOptions.Value.ResponseTimeout.Multiply(5)` (ResponseTimeout defaults to 30s)

**Tuning considerations**:
- **Shorter timeout**: Faster stuck detection, risk false positives
- **Longer timeout**: Accommodate long operations, slower recovery
- **Note**: The 2-hour default is very permissive; most deployments should tune this down

---

## Patterns for Moonpool

### ✅ Adopt These Patterns

1. **Configurable Collection Policies**: Per-actor-type idle timeouts
   ```rust
   pub struct CollectionOptions {
       pub collection_quantum: Duration,
       pub default_collection_age: Duration,
       pub per_type_collection_age: HashMap<String, Duration>,
   }
   ```

2. **Structured Deactivation Reasons**: Enum + description + optional error
   ```rust
   pub struct DeactivationReason {
       pub code: DeactivationReasonCode,
       pub description: String,
       pub error: Option<Box<dyn Error>>,
   }

   pub enum DeactivationReasonCode {
       ApplicationRequested,
       ActivationFailed,
       ActivationIdle,
       Migrating,
       ActivationUnresponsive,
       ShuttingDown,
       HighMemoryPressure,
   }
   ```

3. **Idle Tracking with Reset**: Track idleness, reset on activity
   ```rust
   pub struct Activation {
       idle_since: Option<Instant>,
   }

   impl Activation {
       fn is_idle(&self, age_limit: Duration) -> bool {
           self.idle_since.map_or(false, |since| since.elapsed() >= age_limit)
       }

       fn on_message_completed(&mut self) {
           self.idle_since = Some(Instant::now());
       }
   }
   ```

4. **Keep-Alive Extension**: Explicit lifetime extension
   ```rust
   pub struct Activation {
       keep_alive_until: Option<Instant>,
   }

   impl Activation {
       pub fn delay_deactivation(&mut self, duration: Duration, time: &dyn TimeProvider) {
           self.keep_alive_until = Some(time.now() + duration);
       }
   }
   ```

5. **Stuck Detection**: Track long-running operations
   ```rust
   pub struct Activation {
       current_message_started: Option<Instant>,
       stuck_threshold: Duration,
   }

   impl Activation {
       fn check_if_stuck(&self, time: &dyn TimeProvider) -> bool {
           self.current_message_started
               .map_or(false, |started| (time.now() - started) > self.stuck_threshold)
       }
   }
   ```

6. **Timeout-Based Lifecycle**: Enforce timeouts on activation/deactivation
   ```rust
   async fn activate_with_timeout(
       &mut self,
       timeout: Duration,
       time: &dyn TimeProvider
   ) -> Result<()> {
       time.timeout(timeout, async {
           self.on_activate().await
       }).await
   }
   ```

### ⚠️ Adapt These Patterns

1. **Two-Tier Collection**: Working set + full catalog
   - Moonpool: Simpler initially, optimize later if needed
   - Single-threaded simulation may not need working set optimization

2. **Per-Type Configuration**: String-based lookup
   - Moonpool: Use type IDs or trait-based configuration
   - More type-safe than string matching

3. **Stuck Activation Abandonment**: Leave dangling, create new
   - Moonpool: Can be more explicit (task cancellation)
   - Single-threaded makes cancellation simpler

4. **Memory Pressure Shedding**: Orleans polls memory usage and sheds activations
   - Moonpool: Simulation environment has controlled resources; consider simulating memory pressure as a chaos event rather than polling real memory

### ❌ Avoid These Patterns

1. **DateTime for Timeouts**: Orleans uses `DateTime.UtcNow`
   - Moonpool: Use `Instant` from `TimeProvider` (monotonic, no clock adjustments)

2. **Background Collector Thread**: Orleans has periodic background task
   - Moonpool: Event-driven or explicit collection steps in simulation

3. **Global Singletons**: Orleans uses shared configuration
   - Moonpool: Pass via Provider traits for testability

---

## Testing Strategy for Phase 12

### Collection Testing

**Invariants**:
- Idle time = now - last_activity (always)
- Active count + idle count + deactivating count = total count
- Actors with keep_alive_until > now never collected

**Sometimes assertions**:
- Idle timeout triggers collection
- Keep-alive extension prevents collection
- Exempt actors never collected (unless explicit deactivation)

**Test scenarios**:
1. **Idle timeout**: No activity for > collection age → deactivation
2. **Keep-alive**: Extension prevents collection
3. **Message resets idle**: Processing message resets timer
4. **Per-type configuration**: Different timeouts per actor type

### Stuck Detection Testing

**Invariants**:
- Stuck actors unregistered from catalog
- New messages route to new activation
- Stuck count ≤ total created

**Sometimes assertions**:
- Long-running message triggers stuck detection
- Stuck deactivation detected after timeout
- Warnings logged before stuck declared

**Buggify injection**:
- Inject 5-10s delay in message processing → should trigger stuck
- Inject 5-10s delay in deactivation → should trigger stuck
- Verify new activation created and receives messages

### Timeout Testing

**Invariants**:
- Activating time ≤ activation_timeout (or failed)
- Deactivating time ≤ deactivation_timeout (or stuck)

**Sometimes assertions**:
- Activation timeout triggers
- Deactivation timeout triggers
- Timeouts result in proper cleanup

**Test scenarios**:
1. **Normal case**: Fast activation/deactivation (< timeout)
2. **Slow case**: Buggify inject delay (> timeout)
3. **Recovery**: New activation succeeds after timeout

---

## Summary

Orleans configuration and policies provide:
- **GrainCollectionOptions**: Centralized timing and memory pressure configuration
- **DeactivationReason**: Structured reason tracking
- **Idle detection**: Automatic lifecycle management
- **Stuck detection**: Fault tolerance for hung activations
- **Per-type policies**: Flexible configuration per grain type
- **Memory pressure shedding**: Automatic activation shedding under high memory (v10)

Key architectural principles:
- Configurable timeouts at multiple levels
- Structured reason codes for diagnostics
- Automatic collection with manual override
- Fault tolerance for stuck activations
- Explicit lifetime extension mechanism
- Memory-aware activation management

For Moonpool Phase 12: Implement configurable collection policies, structured deactivation reasons, idle tracking with keep-alive support, and stuck detection for fault tolerance. Use Provider traits for time abstraction and deterministic testing.
