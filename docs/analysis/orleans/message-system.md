# Message System and Request-Response Pattern

## Reference Files
- `Message.cs:1-446` - Message structure with packed headers
- `CallbackData.cs:1-228` - Response tracking and timeout management

## Overview

Orleans implements a **correlation-based request-response system** with:
- **Packed message headers**: Bit-field optimization for metadata
- **Correlation IDs**: Link requests to responses
- **CallbackData**: Track pending responses with timeout/cancellation
- **Message flags**: Control routing, reentrancy, keepalive behavior
- **Time-to-live tracking**: Efficient expiration without allocations

The design provides:
- ✅ **Type-safe addressing**: Strongly-typed grain IDs and silo addresses
- ✅ **Efficient headers**: 32-bit packed fields for flags
- ✅ **Timeout management**: Per-request timeouts with zero allocation
- ✅ **Cancellation support**: Propagate cancellation to remote calls
- ✅ **Correlation tracking**: Reliable request-response matching

---

## Message Structure

### Core Message Fields

**Code**: `Message.cs:10-37`

```csharp
internal sealed class Message : ISpanFormattable
{
    // Timing (non-serialized retry count)
    private short _retryCount;

    public CoarseStopwatch _timeToExpiry;

    // Core identification and body
    public object? BodyObject { get; set; }

    // Headers and identification
    public PackedHeaders _headers;
    public CorrelationId _id;

    // Request context
    public Dictionary<string, object>? _requestContextData;

    // Addressing
    public SiloAddress? _targetSilo;
    public GrainId _targetGrain;

    public SiloAddress? _sendingSilo;
    public GrainId _sendingGrain;

    // Interface metadata
    public ushort _interfaceVersion;
    public GrainInterfaceType _interfaceType;

    public List<GrainAddressCacheUpdate>? _cacheInvalidationHeader;
}
```

**Key Design Decisions**:

1. **Correlation ID** (line 24)
   - Unique identifier for request-response matching
   - Type: `CorrelationId` (likely a `Guid` or `long`)
   - Used by `CallbackData` to route responses

2. **Four-address routing** (lines 28-32)
   - `TargetSilo` + `TargetGrain`: Where message goes
   - `SendingSilo` + `SendingGrain`: Where response returns
   - Enables routing at both silo and grain level

3. **Interface versioning** (lines 34-35)
   - `_interfaceVersion`: Grain interface version
   - `_interfaceType`: Type identifier for the interface
   - Enables version compatibility checks

4. **Time-to-live** (line 19)
   - `CoarseStopwatch`: Low-resolution stopwatch (probably millisecond precision)
   - Tracks remaining time, not expiry timestamp
   - Efficient: No allocations, just arithmetic

### Packed Headers Optimization

**Code**: `Message.cs:399-443`

```csharp
internal struct PackedHeaders
{
    private const uint DirectionMask = 0x000F_0000;
    private const int DirectionShift = 16;
    private const uint ResponseTypeMask = 0x00F0_0000;
    private const int ResponseTypeShift = 20;
    private const uint ForwardCountMask = 0xFF00_0000;
    private const int ForwardCountShift = 24;

    public static implicit operator PackedHeaders(uint fields) => new() { _fields = fields };
    public static implicit operator uint(PackedHeaders value) => value._fields;

    // 32 bits: HHHH_HHHH RRRR_DDDD FFFF_FFFF FFFF_FFFF
    // F: 16 bits for MessageFlags
    // D: 4 bits for Direction
    // R: 4 bits for ResponseType
    // H: 8 bits for ForwardCount (hop count)
    private uint _fields;

    public int ForwardCount
    {
        readonly get => (int)(_fields >> ForwardCountShift);
        set => _fields = (_fields & ~ForwardCountMask) | (uint)value << ForwardCountShift;
    }

    public Directions Direction
    {
        readonly get => (Directions)((_fields & DirectionMask) >> DirectionShift);
        set => _fields = (_fields & ~DirectionMask) | (uint)value << DirectionShift;
    }

    public ResponseTypes ResponseType
    {
        readonly get => (ResponseTypes)((_fields & ResponseTypeMask) >> ResponseTypeShift);
        set => _fields = (_fields & ~ResponseTypeMask) | (uint)value << ResponseTypeShift;
    }

    public readonly bool HasFlag(MessageFlags flag) => (_fields & (uint)flag) != 0;

    public void SetFlag(MessageFlags flag, bool value) => _fields = value switch
    {
        true => _fields | (uint)flag,
        false => _fields & ~(uint)flag,
    };
}
```

**Bit Layout**:
```
32                   24                   16                   8                    0
|--------ForwardCount|---ResponseType----|----Direction----|--------MessageFlags--------|
    8 bits (0-255)      4 bits (0-15)       4 bits (0-15)         16 bits (flags)
```

**Why packed?**
- **Memory efficiency**: 32 bits vs 4+ separate fields (at least 16 bytes with padding)
- **Cache efficiency**: Single cache line for all headers
- **Serialization**: Can write as single `uint`; implicit operators enable `PackedHeaders <-> uint` conversion

**Access patterns**:
- **Read**: Mask + shift (`(_fields & mask) >> shift`)
- **Write**: Clear bits + set new value (`(_fields & ~mask) | (value << shift)`)
- **Flag check**: Bitwise AND (`(_fields & flag) != 0`)
- **Flag set**: Bitwise OR/AND NOT (`_fields | flag` or `_fields & ~flag`)

### Message Directions

**Code**: `Message.cs:42-48, 71-75`

```csharp
[GenerateSerializer]
public enum Directions : byte
{
    None,
    Request,
    Response,
    OneWay
}

public Directions Direction
{
    get => _headers.Direction;
    set => _headers.Direction = value;
}
```

**Semantic meaning**:
- **Request**: Expects a response, creates `CallbackData`
- **Response**: Reply to a previous request, matched by correlation ID
- **OneWay**: Fire-and-forget, no response expected
- **None**: Invalid/uninitialized

**Usage**: Controls message routing and callback registration

### Message Flags

**Code**: `Message.cs:375-397, 92-144`

```csharp
[Flags]
internal enum MessageFlags : ushort
{
    SystemMessage = 1 << 0,          // System-level message (e.g., silo-to-silo)
    ReadOnly = 1 << 1,               // Doesn't mutate state, can interleave
    AlwaysInterleave = 1 << 2,       // Can execute concurrently (e.g., timers)
    Unordered = 1 << 3,              // Order not guaranteed

    HasRequestContextData = 1 << 4,  // RequestContext attached
    HasInterfaceVersion = 1 << 5,    // Interface version field populated
    HasInterfaceType = 1 << 6,       // Interface type field populated
    HasCacheInvalidationHeader = 1 << 7, // Cache invalidation list attached
    HasTimeToLive = 1 << 8,          // Time-to-live tracking enabled

    IsLocalOnly = 1 << 9,            // Cannot be forwarded to another activation
    SuppressKeepAlive = 1 << 10,     // Don't extend activation lifetime

    Reserved = 1 << 15,              // Reserved for future use
}
```

**Key flags explained**:

1. **ReadOnly** (lines 98-108)
   - Indicates message doesn't mutate grain state
   - Enables read-only interleaving
   - Multiple read-only messages can execute concurrently

2. **AlwaysInterleave** (lines 110-114)
   - Message can always interleave with others
   - Used for system messages, timers, diagnostics
   - Bypasses reentrancy checks

3. **IsLocalOnly** (lines 122-132)
   - Message bound to specific activation instance
   - Cannot be forwarded during migration or deactivation
   - Used for internal grain operations

4. **SuppressKeepAlive** (lines 134-144, inverted as `IsKeepAlive`)
   - By default, messages extend activation lifetime (`IsKeepAlive` defaults to `true`)
   - Setting `SuppressKeepAlive` prevents lifetime extension
   - Used for probes, diagnostics

**Flag combinations**: Multiple flags can be set simultaneously

### Time-to-Live Tracking

**Code**: `Message.cs:19, 82, 210-238, 270-277`

```csharp
public CoarseStopwatch _timeToExpiry;

public bool IsExpired => _timeToExpiry is { IsDefault: false, ElapsedMilliseconds: > 0 };

public TimeSpan? TimeToLive
{
    get => _timeToExpiry.IsDefault ? null : -_timeToExpiry.Elapsed;
    set
    {
        if (value.HasValue)
        {
            SetTimeToLiveMilliseconds((long)value.Value.TotalMilliseconds);
        }
        else
        {
            SetInfiniteTimeToLive();
        }
    }
}

internal void SetTimeToLiveMilliseconds(long milliseconds)
{
    _headers.SetFlag(MessageFlags.HasTimeToLive, true);
    _timeToExpiry = CoarseStopwatch.StartNew(-milliseconds);  // Negative = countdown
}

internal void SetInfiniteTimeToLive()
{
    _headers.SetFlag(MessageFlags.HasTimeToLive, false);
    _timeToExpiry = default;
}

public bool IsExpirableMessage()
{
    GrainId id = TargetGrain;
    if (id.IsDefault) return false;

    // Don't set expiration for one way, system target and system grain messages
    return Direction != Directions.OneWay && !id.IsSystemTarget();
}
```

**Clever design: Negative countdown**
- Start with `-milliseconds` (e.g., `-5000` for 5 seconds)
- `CoarseStopwatch.Elapsed` increases over time
- When `Elapsed > 0`, message has expired
- No need to compare against current time, just check sign

**Why `CoarseStopwatch`?**
- Lower resolution than `Stopwatch` (probably 10-100ms granularity)
- Cheaper to read (fewer CPU cycles)
- Sufficient for message timeouts (don't need microsecond precision)

### Message Forwarding

**Code**: `Message.cs:152-156`

```csharp
public int ForwardCount
{
    get => _headers.ForwardCount;
    set => _headers.ForwardCount = value;
}
```

**Usage**: Track how many times message has been forwarded

**Why track?**
- Prevent infinite forwarding loops
- Metrics/diagnostics
- Reject messages forwarded too many times

**Where incremented**: In `MessageCenter.TryForwardMessage()` before resending (MessageCenter.cs:411)

### Cache Invalidation

**Code**: `Message.cs:240-305`

```csharp
public List<GrainAddressCacheUpdate>? CacheInvalidationHeader
{
    get => _cacheInvalidationHeader;
    set
    {
        _cacheInvalidationHeader = value;
        _headers.SetFlag(MessageFlags.HasCacheInvalidationHeader, value is not null);
    }
}

internal void AddToCacheInvalidationHeader(GrainAddress invalidAddress, GrainAddress? validAddress)
{
    var grainAddressCacheUpdate = new GrainAddressCacheUpdate(invalidAddress, validAddress);
    if (_cacheInvalidationHeader is null)
    {
        var newList = new List<GrainAddressCacheUpdate> { grainAddressCacheUpdate };
        if (Interlocked.CompareExchange(ref _cacheInvalidationHeader, newList, null) is not null)
        {
            // Another thread initialized it, add to the existing list
            lock (_cacheInvalidationHeader)
            {
                _cacheInvalidationHeader.Add(grainAddressCacheUpdate);
            }
        }
        else
        {
            _headers.SetFlag(MessageFlags.HasCacheInvalidationHeader, true);
        }
    }
    else
    {
        lock (_cacheInvalidationHeader)
        {
            _cacheInvalidationHeader.Add(grainAddressCacheUpdate);
        }
    }
}
```

**Purpose**: Piggyback cache invalidation on responses

**Thread Safety**: The `AddToCacheInvalidationHeader` method uses `Interlocked.CompareExchange` for the initial list creation and `lock` for subsequent additions, ensuring safe concurrent access from multiple threads. The `CacheInvalidationHeader` property setter also synchronizes the `HasCacheInvalidationHeader` flag with the field value.

**When added**:
- Activation deactivates while processing request
- Message forwarded to new activation
- Response includes invalidation so sender updates cache

**Pattern**: Accumulate multiple invalidations in single response

---

## CallbackData: Response Tracking

### Purpose
`CallbackData` tracks pending requests awaiting responses. Each request creates a `CallbackData` that:
- Waits for response from target silo
- Times out if response doesn't arrive
- Handles cancellation from caller
- Detects target silo failure

**Code**: `CallbackData.cs:10-32`

```csharp
internal sealed partial class CallbackData
{
    private readonly SharedCallbackData shared;
    private readonly IResponseCompletionSource context;
    private int completed;
    private StatusResponse? lastKnownStatus;
    private ValueStopwatch stopwatch;
    private CancellationTokenRegistration _cancellationTokenRegistration;

    public CallbackData(
        SharedCallbackData shared,
        IResponseCompletionSource ctx,
        Message msg)
    {
        this.shared = shared;
        this.context = ctx;
        this.Message = msg;
        this.stopwatch = ValueStopwatch.StartNew();
    }

    public Message Message { get; }
    public bool IsCompleted => this.completed == 1;
}
```

**Key fields**:
- `completed`: Interlocked flag for exactly-once completion
- `context`: Promise/future to complete when response arrives
- `stopwatch`: Track elapsed time for timeout
- `shared`: Shared configuration and unregister callback

### Completion Flag Pattern

**Code**: Used throughout CallbackData.cs (lines 96, 113, 143, 163)

```csharp
if (Interlocked.CompareExchange(ref completed, 1, 0) != 0)
{
    return;  // Already completed, do nothing
}

// ... complete the response ...
```

**Pattern**: Lock-free exactly-once semantics

**Why Interlocked?**
- Multiple paths can complete: response arrival, timeout, cancellation, silo failure
- Race condition: Response arrives same time as timeout
- Interlocked ensures exactly one path completes the callback

**Correct behavior**:
- First path: `CompareExchange` succeeds, completes callback
- Second path: `CompareExchange` fails, returns immediately
- No locks needed, no blocking

### Timeout Handling

**Code**: `CallbackData.cs:111-139`

```csharp
public void OnTimeout()
{
    if (Interlocked.CompareExchange(ref completed, 1, 0) != 0)
    {
        return;
    }

    this.stopwatch.Stop();
    if (shared.CancelRequestOnTimeout)
    {
        SignalCancellation();  // Notify remote silo to cancel
    }

    this.shared.Unregister(this.Message);
    _cancellationTokenRegistration.Dispose();
    ApplicationRequestInstruments.OnAppRequestsEnd((long)this.stopwatch.Elapsed.TotalMilliseconds);
    ApplicationRequestInstruments.OnAppRequestsTimedOut();

    OrleansCallBackDataEvent.Log.OnTimeout(this.Message);

    var msg = this.Message;
    var statusMessage = lastKnownStatus is StatusResponse status ? $"Last known status is {status}. " : string.Empty;
    var timeout = GetResponseTimeout();
    LogTimeout(this.shared.Logger, timeout, msg, statusMessage);

    var exception = new TimeoutException($"Response did not arrive on time in {timeout} for message: {msg}. {statusMessage}");
    context.Complete(Response.FromException(exception));
}
```

**Timeout flow**:
1. Check completion flag (exactly once)
2. Stop stopwatch (for metrics)
3. Optionally signal cancellation to remote silo
4. Unregister from tracking (prevent memory leak)
5. Dispose cancellation registration
6. Record metrics
7. Log timeout
8. Complete context with `TimeoutException`

**Configuration**: `GetResponseTimeout()` (line 82)
- Check if request has custom timeout (`IInvokable.GetDefaultResponseTimeout()`)
- Fall back to shared configuration timeout
- Per-request timeout flexibility

**Expiration check**: `IsExpired(long currentTimestamp)` (lines 65-69) uses raw stopwatch ticks for efficient comparison without `TimeSpan` allocations. `GetResponseTimeoutStopwatchTicks()` (lines 71-80) converts the timeout to native stopwatch ticks via `Stopwatch.Frequency`.

### Cancellation Handling

**Code**: `CallbackData.cs:34-109`

```csharp
public void SubscribeForCancellation(CancellationToken cancellationToken)
{
    if (!cancellationToken.CanBeCanceled) return;

    cancellationToken.ThrowIfCancellationRequested();
    _cancellationTokenRegistration = cancellationToken.UnsafeRegister(static arg =>
    {
        var callbackData = (CallbackData)arg!;
        callbackData.OnCancellation();
    }, this);
}

private void OnCancellation()
{
    // If waiting for acknowledgement, just signal and return
    if (shared.WaitForCancellationAcknowledgement)
    {
        SignalCancellation();
        return;
    }

    // Otherwise, cancel immediately without waiting
    if (Interlocked.CompareExchange(ref completed, 1, 0) != 0)
    {
        return;
    }

    stopwatch.Stop();
    SignalCancellation();  // Tell remote silo to cancel
    shared.Unregister(Message);
    ApplicationRequestInstruments.OnAppRequestsEnd((long)stopwatch.Elapsed.TotalMilliseconds);
    ApplicationRequestInstruments.OnAppRequestsTimedOut();
    OrleansCallBackDataEvent.Log.OnCanceled(Message);
    context.Complete(Response.FromException(new OperationCanceledException(_cancellationTokenRegistration.Token)));
    _cancellationTokenRegistration.Dispose();
}

private void SignalCancellation()
{
    // Only cancel requests which honor cancellation token.
    if (Message.BodyObject is IInvokable invokable && invokable.IsCancellable)
    {
        shared.CancellationManager?.SignalCancellation(Message.TargetSilo, Message.TargetGrain, Message.SendingGrain, Message.Id);
    }
}
```

**Two cancellation modes**:

1. **Wait for acknowledgement** (lines 88-92)
   - Send cancellation signal
   - Keep waiting for response or timeout
   - Remote grain decides whether to honor cancellation
   - Caller continues waiting

2. **Cancel immediately** (lines 96-109)
   - Send cancellation signal
   - Complete callback with `OperationCanceledException`
   - Caller's task completes immediately
   - Remote grain may still be processing

**Configuration**: `shared.WaitForCancellationAcknowledgement` controls mode

**Cancellation signal**: `SignalCancellation()` (lines 49-58)
- Guards with `IInvokable.IsCancellable` check before sending signal
- Uses null-conditional `shared.CancellationManager?.SignalCancellation(...)` since manager may not be set
- Sends message to target silo
- Target looks up activation and pending request

### Target Silo Failure

**Code**: `CallbackData.cs:141-159`

```csharp
public void OnTargetSiloFail()
{
    if (Interlocked.CompareExchange(ref this.completed, 1, 0) != 0)
    {
        return;
    }

    this.stopwatch.Stop();
    this.shared.Unregister(this.Message);
    _cancellationTokenRegistration.Dispose();
    ApplicationRequestInstruments.OnAppRequestsEnd((long)this.stopwatch.Elapsed.TotalMilliseconds);

    OrleansCallBackDataEvent.Log.OnTargetSiloFail(this.Message);
    var msg = this.Message;
    var statusMessage = lastKnownStatus is StatusResponse status ? $"Last known status is {status}. " : string.Empty;
    LogTargetSiloFail(this.shared.Logger, msg, statusMessage, Constants.TroubleshootingHelpLink);
    var exception = new SiloUnavailableException($"The target silo became unavailable for message: {msg}. {statusMessage}See {Constants.TroubleshootingHelpLink} for troubleshooting help.");
    this.context.Complete(Response.FromException(exception));
}
```

**Triggered by**: Silo status change to Dead/Terminated (likely from cluster membership)

**Flow**:
1. Check completion flag
2. Stop tracking
3. Record metrics
4. Complete with `SiloUnavailableException`

**Differs from timeout**: Explicitly knows target failed vs waiting too long

### Response Delivery

**Code**: `CallbackData.cs:161-212`

```csharp
public void DoCallback(Message response)
{
    if (Interlocked.CompareExchange(ref this.completed, 1, 0) != 0)
    {
        return;
    }

    OrleansCallBackDataEvent.Log.DoCallback(this.Message);

    this.stopwatch.Stop();
    _cancellationTokenRegistration.Dispose();
    ApplicationRequestInstruments.OnAppRequestsEnd((long)this.stopwatch.Elapsed.TotalMilliseconds);

    // Do callback outside the CallbackData lock
    ResponseCallback(response, this.context);
}

private static void ResponseCallback(Message message, IResponseCompletionSource context)
{
    try
    {
        var body = message.BodyObject;
        if (body is Response response)
        {
            context.Complete(response);
        }
        else
        {
            HandleRejectionResponse(context, body as RejectionResponse);
        }
    }
    catch (Exception exc)
    {
        // Catch the exception and break the promise with it
        context.Complete(Response.FromException(exc));
    }

    static void HandleRejectionResponse(IResponseCompletionSource context, RejectionResponse? rejection)
    {
        Exception exception;
        if (rejection?.RejectionType is Message.RejectionTypes.GatewayTooBusy)
        {
            exception = new GatewayTooBusyException();
        }
        else
        {
            exception = rejection?.Exception ?? new OrleansMessageRejectionException(rejection?.RejectionInfo ?? "Unable to send request - no rejection info available");
        }
        context.Complete(Response.FromException(exception));
    }
}
```

**Happy path**:
1. Check completion flag
2. Stop tracking
3. Record metrics
4. Extract response body
5. Complete context with success or failure

**Response types**:
- `Response` - Success, complete with result
- `RejectionResponse` - Target rejected (overloaded, not found, etc.)
- Exception - Something went wrong during delivery

**Error handling**: All paths wrapped in try-catch, complete with exception

### Status Updates

**Code**: `CallbackData.cs:60-63`

```csharp
public void OnStatusUpdate(StatusResponse status)
{
    this.lastKnownStatus = status;
}
```

**Purpose**: Track progress of long-running requests

**Used in**: Timeout and silo failure messages (lines 133, 155)
- "Last known status is XYZ" helps debugging
- Shows request made progress before failing

---

## Request-Response Pattern

### Full Request-Response Flow

**Sending a request**:

1. **Create message**
   - Set `Direction = Request`
   - Assign correlation ID
   - Set target and sender addresses

2. **Register callback**
   - Create `CallbackData`
   - Store in registry keyed by correlation ID
   - Subscribe to cancellation token
   - Start timeout timer

3. **Send message**
   - Serialize message
   - Send via transport layer
   - Message travels to target silo

**Receiving and processing**:

4. **Target receives request**
   - Deserialize message
   - Route to target activation
   - Enqueue in activation's inbox

5. **Process request**
   - Dequeue message
   - Invoke grain method
   - Generate response

6. **Send response**
   - Create response message
   - Set `Direction = Response`
   - Copy correlation ID from request
   - Swap target/sender addresses
   - Send via transport layer

**Completing the request**:

7. **Sender receives response**
   - Deserialize message
   - Look up `CallbackData` by correlation ID
   - Call `DoCallback(response)`
   - Complete future/promise
   - Caller's `await` completes

### Correlation ID Matching

**Key insight**: Correlation ID is the bridge

**Request**:
```
CorrelationId: abc-123
Direction: Request
TargetSilo: Silo2
TargetGrain: GrainX
SendingSilo: Silo1
SendingGrain: GrainY
```

**Response**:
```
CorrelationId: abc-123           // SAME ID
Direction: Response
TargetSilo: Silo1                // Swapped
TargetGrain: GrainY              // Swapped
SendingSilo: Silo2               // Swapped
SendingGrain: GrainX             // Swapped
```

**Lookup**: `callbacks[(TargetGrain, correlationId)]` -> `CallbackData` -> `IResponseCompletionSource` -> caller's Task

### Reliability Guarantees

Orleans message system provides:

**Guaranteed**:
- ✅ Exactly-once completion (via Interlocked)
- ✅ Correlation ID uniqueness (per sender)
- ✅ Timeout detection
- ✅ Cancellation propagation (best-effort)

**Not guaranteed**:
- ❌ Message delivery (can be lost)
- ❌ Response delivery (can be lost)
- ❌ Order preservation (unless configured)
- ❌ Cancellation honored by receiver

**Error handling**: All failures result in exception in caller

---

## Patterns for Moonpool

### ✅ Adopt These Patterns

1. **Packed Headers**: Bit-field optimization for metadata
   ```rust
   struct PackedHeaders {
       fields: u32,  // Direction (4 bits) | ResponseType (4 bits) | Flags (16 bits) | ForwardCount (8 bits)
   }

   impl PackedHeaders {
       fn direction(&self) -> Direction {
           Direction::from_u8((self.fields >> 16) & 0xF)
       }

       fn set_flag(&mut self, flag: MessageFlags) {
           self.fields |= flag as u32;
       }
   }
   ```

2. **Correlation-Based Request-Response**: Link requests to responses
   ```rust
   struct CallbackData {
       correlation_id: CorrelationId,
       completion: oneshot::Sender<Result<Response>>,
       started: Instant,
       timeout: Duration,
   }

   // Registry: HashMap<CorrelationId, CallbackData>
   ```

3. **Countdown Time-to-Live**: Negative duration, check sign for expiry
   ```rust
   struct Message {
       time_to_expiry: Option<Instant>,  // Expiry timestamp
   }

   impl Message {
       fn is_expired(&self) -> bool {
           self.time_to_expiry.map_or(false, |expiry| Instant::now() >= expiry)
       }
   }
   ```

4. **Interlocked Completion**: Exactly-once semantics
   ```rust
   use std::sync::atomic::{AtomicBool, Ordering};

   struct CallbackData {
       completed: AtomicBool,
   }

   impl CallbackData {
       fn complete(&self, result: Result<Response>) -> bool {
           if self.completed.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_ok() {
               // First to complete, do the work
               self.completion.send(result);
               true
           } else {
               // Already completed by another path
               false
           }
       }
   }
   ```

5. **Message Flags**: Separate concerns into bit flags
   ```rust
   bitflags! {
       struct MessageFlags: u16 {
           const READ_ONLY = 1 << 0;
           const ALWAYS_INTERLEAVE = 1 << 1;
           const IS_LOCAL_ONLY = 1 << 2;
           const SUPPRESS_KEEP_ALIVE = 1 << 3;
       }
   }
   ```

### ⚠️ Adapt These Patterns

1. **Four-address routing**: Orleans has sender/target grain and silo
   - Moonpool: Simplify to ActorId (no separate silo concept initially)
   - Can add later if needed for distributed placement

2. **Interface versioning**: Orleans checks compatibility
   - Moonpool: Not needed initially (single version)
   - Add when supporting upgrades

3. **Cache invalidation header**: Orleans piggybacks on responses
   - Moonpool: Not needed initially (no caching layer)
   - Add when implementing grain directory

4. **Request context**: Orleans uses ambient dictionary
   - Moonpool: Pass explicitly in message or use tokio task-local

### ❌ Avoid These Patterns

1. **CoarseStopwatch**: Orleans uses low-resolution timer
   - Moonpool: Use `Instant` from `TimeProvider`, already abstracted

2. **Dynamic message body**: Orleans uses `object BodyObject`
   - Moonpool: Use `enum Message { Request(...), Response(...), OneWay(...) }`
   - Type-safe, no runtime casting

3. **Mutable reference counters**: Orleans increments `ForwardCount`
   - Moonpool: Immutable messages (use copy-on-write if needed)

---

## Critical Insights for Phase 12

### Message Structure for Moonpool

Proposed message structure for Phase 12 Step 6:

```rust
pub struct Message {
    // Identification
    pub id: CorrelationId,
    pub direction: Direction,

    // Routing
    pub target: ActorId,
    pub sender: ActorId,

    // Payload
    pub payload: Vec<u8>,  // Serialized message body

    // Metadata
    pub flags: MessageFlags,
    pub time_to_expiry: Option<Instant>,
    pub forward_count: u8,
}

pub enum Direction {
    Request { reply_to: oneshot::Sender<Response> },
    Response,
    OneWay,
}
```

**Key decisions**:
- Embed `reply_to` channel in Request variant (type-safe)
- Use `ActorId` instead of separate silo/grain (simpler)
- `Vec<u8>` payload (serialization outside message struct)

### CallbackData Registry Pattern

For Phase 12 Step 7 (Request/Response):

```rust
pub struct MessageBus {
    callbacks: Mutex<HashMap<CorrelationId, CallbackData>>,
}

impl MessageBus {
    pub async fn request(&self, target: ActorId, payload: Vec<u8>) -> Result<Response> {
        let (tx, rx) = oneshot::channel();
        let correlation_id = CorrelationId::new();

        let callback = CallbackData::new(correlation_id, tx);
        self.callbacks.lock().unwrap().insert(correlation_id, callback);

        self.send(Message {
            id: correlation_id,
            direction: Direction::Request,
            target,
            payload,
            ...
        });

        // Wait with timeout
        tokio::time::timeout(Duration::from_secs(30), rx)
            .await?
            .map_err(|_| Error::Timeout)?
    }

    fn on_response(&self, msg: Message) {
        if let Some(callback) = self.callbacks.lock().unwrap().remove(&msg.id) {
            callback.complete(msg.payload);
        }
    }
}
```

**Testing strategy**:
- **Invariant**: Callbacks count = requests sent - responses received
- **Sometimes assert**: Timeouts occur
- **Sometimes assert**: Responses arrive before timeout
- **Buggify**: Inject delays before sending response

---

---

## Message Routing and Delivery

### InsideRuntimeClient (Request Lifecycle)

**File**: `InsideRuntimeClient.cs:27-651`

InsideRuntimeClient manages the full lifecycle of request-response messaging: creating requests, tracking callbacks, handling responses, and executing local invocations.

#### Sending Requests

**InsideRuntimeClient.cs:120-186**:

```csharp
public void SendRequest(
    GrainReference target,
    IInvokable request,
    IResponseCompletionSource context,
    InvokeMethodOptions options)
{
    var cancellationToken = request.GetCancellationToken();
    cancellationToken.ThrowIfCancellationRequested();

    var message = this.messageFactory.CreateMessage(request, options);
    message.InterfaceType = target.InterfaceType;
    message.InterfaceVersion = target.InterfaceVersion;

    // Fill in sender
    if (message.SendingSilo == null)
        message.SendingSilo = MySilo;

    IGrainContext sendingActivation = RuntimeContext.Current;

    if (sendingActivation == null)
    {
        var clientAddress = this.HostedClient.Address;
        message.SendingGrain = clientAddress.GrainId;
    }
    else
    {
        message.SendingGrain = sendingActivation.GrainId;
    }

    // Fill in destination
    var targetGrainId = target.GrainId;
    message.TargetGrain = targetGrainId;
    SharedCallbackData sharedData;
    if (SystemTargetGrainId.TryParse(targetGrainId, out var systemTargetGrainId))
    {
        message.TargetSilo = systemTargetGrainId.GetSiloAddress();
        message.IsSystemMessage = true;
        sharedData = this.systemSharedCallbackData;
    }
    else
    {
        sharedData = this.sharedCallbackData;
    }

    // Set TTL for expirable messages
    if (this.messagingOptions.DropExpiredMessages && message.IsExpirableMessage())
    {
        message.TimeToLive = request.GetDefaultResponseTimeout() ?? sharedData.ResponseTimeout;
    }

    var oneWay = (options & InvokeMethodOptions.OneWay) != 0;
    if (!oneWay)
    {
        // Register a callback for the response
        var callbackData = new CallbackData(sharedData, context, message);
        callbacks.TryAdd((message.SendingGrain, message.Id), callbackData);
        callbackData.SubscribeForCancellation(cancellationToken);
    }
    else
    {
        context?.Complete();  // OneWay completes immediately
    }

    this.messagingTrace.OnSendRequest(message);
    this.MessageCenter.AddressAndSendMessage(message);  // Route to MessageCenter
}
```

**Key Design Decisions**:
- **Early cancellation check**: `cancellationToken.ThrowIfCancellationRequested()` before creating message
- **Callback Registration Before Sending**: Ensures we're ready for response
- **OneWay Optimization**: No callback registration, immediate completion
- **SystemTarget Fast Path**: TargetSilo set directly (no placement/directory lookup); uses separate `systemSharedCallbackData` with `SystemResponseTimeout`
- **TTL Based on Response Timeout**: Enables expiration checking
- **Separate SharedCallbackData**: System targets use `systemSharedCallbackData` with different timeout and `cancelOnTimeout: false`

#### Handling Responses

**InsideRuntimeClient.cs:371-457**:

```csharp
public void ReceiveResponse(Message message)
{
    OrleansInsideRuntimeClientEvent.Log.ReceiveResponse(message);
    if (message.Result is Message.ResponseTypes.Rejection)
    {
        if (!message.TargetSilo.Matches(this.MySilo))
        {
            // gatewayed message - gateway back to sender
            this.MessageCenter.AddressAndSendMessage(message);
            return;
        }

        var rejection = (RejectionResponse)message.BodyObject;
        switch (rejection.RejectionType)
        {
            case Message.RejectionTypes.Overloaded:
                break;
            case Message.RejectionTypes.Unrecoverable:
            // Fall through & reroute
            case Message.RejectionTypes.Transient:
                if (message.CacheInvalidationHeader is null)
                {
                    // Remove from local directory cache
                    this.GrainLocator.InvalidateCache(message.SendingGrain);
                }
                break;
            case Message.RejectionTypes.CacheInvalidation when message.HasCacheInvalidationHeader:
                // The message targeted an invalid activation - no callback to complete
                return;
        }
    }
    else if (message.Result == Message.ResponseTypes.Status)
    {
        var status = (StatusResponse)message.BodyObject;
        callbacks.TryGetValue((message.TargetGrain, message.Id), out var callback);
        var request = callback?.Message;
        if (request is not null)
        {
            callback.OnStatusUpdate(status);
            // Log diagnostics if present
        }
        else
        {
            if (messagingOptions.CancelUnknownRequestOnStatusUpdate)
            {
                // Cancel the call since the caller has abandoned it
                _cancellationManager.SignalCancellation(
                    message.SendingSilo,
                    targetGrainId: message.SendingGrain,
                    sendingGrainId: message.TargetGrain,
                    messageId: message.Id);
            }
        }
        return;
    }

    // Normal response - lookup and complete callback
    bool found = callbacks.TryRemove((message.TargetGrain, message.Id), out var callbackData);
    if (found)
    {
        callbackData.DoCallback(message);
    }
}
```

**Response Flow**:
1. **Gatewayed rejection check**: If rejection targets a different silo, forward it back
2. Check if rejection -> handle cache invalidation (with `Overloaded` as no-op)
3. Check if status update -> update callback; if no callback found and `CancelUnknownRequestOnStatusUpdate` is enabled, signal cancellation to the remote grain
4. Normal response -> lookup callback by `(TargetGrain, MessageId)`, complete

#### Local Invocation

**InsideRuntimeClient.cs:254-336**:

```csharp
public async Task Invoke(IGrainContext target, Message message)
{
    try
    {
        // Don't process expired messages
        if (message.IsExpired)
        {
            this.messagingTrace.OnDropExpiredMessage(message, MessagingInstruments.Phase.Invoke);
            return;
        }

        // Import request context (ambient data)
        if (message.RequestContextData is { Count: > 0 })
        {
            RequestContextExtensions.Import(message.RequestContextData);
        }

        Response response;
        try
        {
            switch (message.BodyObject)
            {
                case IInvokable invokable:
                    {
                        invokable.SetTarget(target);
                        CancellationSourcesExtension.RegisterCancellationTokens(target, invokable);

                        // Apply filters if present (also checks grain instance)
                        if (GrainCallFilters is { Count: > 0 } || target.GrainInstance is IIncomingGrainCallFilter)
                        {
                            var invoker = new GrainMethodInvoker(message, target, invokable, GrainCallFilters, this.interfaceToImplementationMapping, this.responseCopier);
                            await invoker.Invoke();
                            response = invoker.Response;
                        }
                        else
                        {
                            // Direct invocation (no filters)
                            response = await invokable.Invoke();
                            response = this.responseCopier.Copy(response);
                        }

                        invokable.Dispose();
                        break;
                    }
                default:
                    throw new NotSupportedException($"Request {message.BodyObject} of type {message.BodyObject?.GetType()} is not supported");
            }
        }
        catch (Exception exc)
        {
            response = Response.FromException(exc);
        }

        if (response.Exception is { } invocationException)
        {
            // Log at Debug for request/response, Warning for OneWay
            LogGrainInvokeException(..., message.Direction != Message.Directions.OneWay ? LogLevel.Debug : LogLevel.Warning, ...);

            // Special handling for InconsistentStateException -> deactivate grain
            if (invocationException is InconsistentStateException ise && ise.IsSourceActivation)
            {
                ise.IsSourceActivation = false;
                target.Deactivate(new DeactivationReason(DeactivationReasonCode.ApplicationError, ...));
            }
        }

        if (message.Direction != Message.Directions.OneWay)
        {
            SafeSendResponse(message, response);
        }
    }
    catch (Exception exc)
    {
        if (message.Direction != Message.Directions.OneWay)
        {
            SafeSendExceptionResponse(message, exc);
        }
    }
}
```

**Invocation Flow**:
1. Check expiration -> drop if expired
2. Import RequestContext (ambient data propagation)
3. Set invokable target to activation
4. Apply filter pipeline (or direct invoke); also checks if grain itself implements `IIncomingGrainCallFilter`
5. Dispose invokable after execution
6. Handle exceptions -> create error response; log level varies by direction
7. Special: InconsistentStateException -> deactivate grain
8. Send response (unless OneWay)

#### Cache Snooping

**InsideRuntimeClient.cs:210-252**:

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
                    GrainLocator.UpdateCache(update);
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

**Called by MessageCenter for every incoming message**. Proactively updates the directory cache based on information piggybacked in message headers. The `lock (cacheUpdates)` synchronizes with `AddToCacheInvalidationHeader` which also locks the list during concurrent additions.

---

### MessageCenter (Routing and Delivery)

**File**: `MessageCenter.cs:16-689`

MessageCenter handles routing decisions, connection management, forwarding, and message delivery.

#### AddressAndSendMessage (Placement Integration)

**MessageCenter.cs:453-492**:

```csharp
internal Task AddressAndSendMessage(Message message)
{
    try
    {
        // Ask PlacementService to determine TargetSilo
        var messageAddressingTask = placementService.AddressMessage(message);
        if (messageAddressingTask.Status != TaskStatus.RanToCompletion)
        {
            return SendMessageAsync(messageAddressingTask, message);
        }

        SendMessage(message);
    }
    catch (Exception ex)
    {
        OnAddressingFailure(message, ex);
    }

    return Task.CompletedTask;

    async Task SendMessageAsync(Task addressMessageTask, Message m)
    {
        try
        {
            await addressMessageTask;
        }
        catch (Exception ex)
        {
            OnAddressingFailure(m, ex);
            return;
        }

        SendMessage(m);
    }

    void OnAddressingFailure(Message m, Exception ex)
    {
        this.messagingTrace.OnDispatcherSelectTargetFailed(m, ex);
        RejectMessage(m, Message.RejectionTypes.Unrecoverable, ex);
    }
}
```

**Placement Integration**: `PlacementService.AddressMessage()` determines `TargetSilo` by:
1. Check if SystemTarget -> use explicit address
2. Check directory cache -> use cached address
3. No cache hit -> query directory, potentially trigger placement

**Note**: `SendMessageAsync` and `OnAddressingFailure` are local functions defined within `AddressAndSendMessage`, keeping the async/error paths cleanly scoped.

#### SendMessage (Routing Decision)

**MessageCenter.cs:137-235**:

```csharp
public void SendMessage(Message msg)
{
    Debug.Assert(!msg.IsLocalOnly);

    // Block application messages during "fast stop"
    if (IsBlockingApplicationMessages && !msg.IsSystemMessage && msg.Result is not Message.ResponseTypes.Rejection && !Constants.SystemMembershipTableType.Equals(msg.TargetGrain))
    {
        this.messagingTrace.OnDropBlockedApplicationMessage(msg);
    }
    else
    {
        msg.SendingSilo ??= _siloAddress;

        if (stopped)
        {
            SendRejection(msg, Message.RejectionTypes.Unrecoverable, "Outbound queue stopped");
            return;
        }

        // Don't process expired messages
        if (msg.IsExpired)
        {
            this.messagingTrace.OnDropExpiredMessage(msg, MessagingInstruments.Phase.Send);
            return;
        }

        // Check if destined for proxy (gateway or hosted client)
        if (TryDeliverToProxy(msg))
        {
            return;
        }

        if (msg.TargetSilo is not { } targetSilo)
        {
            SendRejection(msg, Message.RejectionTypes.Unrecoverable, "No target silo.");
            return;
        }

        messagingTrace.OnSendMessage(msg);

        // LOCAL OPTIMIZATION: Same-silo delivery
        if (targetSilo.Matches(_siloAddress))
        {
            MessagingInstruments.LocalMessagesSentCounterAggregator.Add(1);
            this.ReceiveMessage(msg);  // Direct delivery, no network
        }
        else
        {
            // REMOTE DELIVERY: Get connection and send
            if (this.connectionManager.TryGetConnection(targetSilo, out var existingConnection))
            {
                existingConnection.Send(msg);
                return;
            }
            else if (this.siloStatusOracle.IsDeadSilo(targetSilo))
            {
                // Do not try to establish connection to dead silo
                // Only reject requests and one-way messages
                if (msg.Direction is Message.Directions.Request or Message.Directions.OneWay)
                {
                    this.SendRejection(msg, Message.RejectionTypes.Transient, "Target silo is known to be dead", new SiloUnavailableException());
                }
                return;
            }
            else
            {
                // Establish new connection
                var connectionTask = this.connectionManager.GetConnection(targetSilo);
                if (connectionTask.IsCompletedSuccessfully)
                {
                    connectionTask.Result.Send(msg);
                }
                else
                {
                    _ = SendAsync(this, connectionTask, msg);
                }
            }
        }
    }
}
```

**Routing Decision Tree**:
1. **IsLocalOnly assertion**: Debug assert that message is not local-only (local-only messages should not reach `SendMessage`)
2. **Application blocking**: During "fast stop", drop non-system, non-rejection application messages (except membership table grain)
3. **Expiration check**: Drop if TTL exceeded
4. **Proxy check**: Client messages go to gateway/hosted client
5. **Local optimization**: Same-silo -> call `ReceiveMessage()` directly (99% latency reduction)
6. **Remote delivery**: Get connection -> send over network
7. **Dead silo check**: Reject only Request/OneWay messages if target is known dead (responses to dead silos are silently dropped)
8. **Connection establishment**: Async connection creation if needed; failure sends Transient rejection

#### ReceiveMessage (Dispatch)

**MessageCenter.cs:508-551**:

```csharp
public void ReceiveMessage(Message msg)
{
    Debug.Assert(!msg.IsLocalOnly);
    try
    {
        this.messagingTrace.OnIncomingMessageAgentReceiveMessage(msg);

        // Try proxy delivery first (client message)
        if (TryDeliverToProxy(msg))
        {
            return;
        }
        else if (msg.Direction == Message.Directions.Response)
        {
            // Response message -> route to InsideRuntimeClient
            this.catalog.RuntimeClient.ReceiveResponse(msg);
        }
        else
        {
            // Request message -> get or create activation
            var targetActivation = catalog.GetOrCreateActivation(
                msg.TargetGrain,
                msg.RequestContextData,
                rehydrationContext: null);

            if (targetActivation is null)
            {
                ProcessMessageToNonExistentActivation(msg);
                return;
            }

            targetActivation.ReceiveMessage(msg);  // Enqueue to activation's WorkItemGroup
            _messageObserver?.Invoke(msg);
        }
    }
    catch (Exception ex)
    {
        MessagingProcessingInstruments.OnDispatcherMessageProcessedError(msg);
        this.RejectMessage(msg, Message.RejectionTypes.Transient, ex);
    }
}
```

**Dispatch Logic**:
1. Assert message is not local-only
2. Try proxy delivery -> gateway/hosted client
3. Response -> `InsideRuntimeClient.ReceiveResponse()`
4. Request -> `Catalog.GetOrCreateActivation()` -> `activation.ReceiveMessage()`; invoke `_messageObserver` for statistics
5. Catch errors -> record error metrics and reject with Transient

**Integration**: `activation.ReceiveMessage()` enqueues in the activation's WorkItemGroup (see task-scheduling.md).

#### Forwarding (Stale Address Handling)

**MessageCenter.cs:348-416**:

```csharp
private void TryForwardRequest(Message message, GrainAddress? oldAddress, GrainAddress? destination, string? failedOperation = null, Exception? exc = null)
{
    Debug.Assert(!message.IsLocalOnly);

    bool forwardingSucceeded = false;
    var forwardingAddress = destination?.SiloAddress;
    try
    {
        this.messagingTrace.OnDispatcherForwarding(message, oldAddress, forwardingAddress, failedOperation, exc);

        // Add cache invalidation header
        if (oldAddress != null)
        {
            message.AddToCacheInvalidationHeader(oldAddress, validAddress: destination);
        }

        forwardingSucceeded = this.TryForwardMessage(message, forwardingAddress);
    }
    catch (Exception exc2)
    {
        forwardingSucceeded = false;
        exc = exc2;
    }
    finally
    {
        var sentRejection = false;

        // OneWay: Send cache invalidation response even if forwarding succeeded
        if (message.Direction == Message.Directions.OneWay)
        {
            this.RejectMessage(message, Message.RejectionTypes.CacheInvalidation, exc, "OneWay message sent to invalid activation");
            sentRejection = true;
        }

        if (!forwardingSucceeded)
        {
            this.messagingTrace.OnDispatcherForwardingFailed(message, oldAddress, forwardingAddress, failedOperation, exc);
            if (!sentRejection)
            {
                RejectMessage(message, Message.RejectionTypes.Transient, exc, ...);
            }
        }
    }
}

private bool TryForwardMessage(Message message, SiloAddress? forwardingAddress)
{
    if (!MayForward(message, this.messagingOptions)) return false;

    message.ForwardCount = message.ForwardCount + 1;
    MessagingProcessingInstruments.OnDispatcherMessageForwared(message);

    ResendMessageImpl(message, forwardingAddress);
    return true;
}

private static bool MayForward(Message message, SiloMessagingOptions messagingOptions)
{
    return message.ForwardCount < messagingOptions.MaxForwardCount;
}
```

**Forwarding Protocol**:
1. Assert message is not local-only
2. Check `ForwardCount < MaxForwardCount` via `MayForward()` helper (default 2)
3. Increment `ForwardCount`
4. Record forwarding metrics via `MessagingProcessingInstruments.OnDispatcherMessageForwared()`
5. Add cache invalidation header with old and new addresses
6. Resend message to new address (or null to trigger placement)
7. OneWay messages: Send cache invalidation rejection even if forwarding succeeds
8. On forwarding failure: log failure trace and send Transient rejection

**Why forward?** When a message arrives for an activation that:
- No longer exists (deactivated)
- Moved to another silo (migration)
- Was never created (directory race)

---

### Cache Invalidation Protocol

#### Message Header Structure

Messages include `List<GrainAddressCacheUpdate>` for proactive cache updates:

```csharp
public struct GrainAddressCacheUpdate
{
    public GrainAddress InvalidGrainAddress { get; set; }
    public GrainAddress? ValidGrainAddress { get; set; }
}
```

#### Update Flow

```
Sender                    Wrong Silo                Correct Silo
  |                            |                          |
  |-- Request (GrainX) ------->| (X not here)             |
  |                            | Forward to Correct Silo  |
  |                            |--- Request + Header ---->|
  |                            |    InvalidAddr: WrongSilo|
  |                            |    ValidAddr: CorrectSilo|
  |                            |                          |
  |                            |<-------- Response -------|
  |<-- Response + Header ------|                          |
  |    (SniffIncomingMessage)  |                          |
  |    Update cache:           |                          |
  |      Invalidate WrongSilo  |                          |
  |      Cache CorrectSilo     |                          |
```

**Benefits**:
- **Proactive**: Sender learns about stale cache without explicit query
- **Piggyback**: No extra messages needed
- **Propagation**: Headers flow back through forwarding chain

---

### Rejection Types

**Message.RejectionTypes** (Message.cs:60-69):
- **Transient** - Temporary failure (activation moved, not found), retry with fresh address
- **Overloaded** - Silo overloaded, retry later
- **Unrecoverable** - Permanent failure (SystemTarget doesn't exist, silo stopped), don't retry
- **GatewayTooBusy** - Gateway overloaded, creates `GatewayTooBusyException` in `CallbackData.ResponseCallback`
- **CacheInvalidation** - OneWay to invalid activation, update cache but don't complete callback

---

### Local vs Remote Optimization

**Performance Characteristics**:

**Local Call** (same silo):
- Directly calls `ReceiveMessage()`
- No serialization, no network, no deserialization
- ~100ns overhead

**Remote Call** (different silo):
- Serializes message -> Network send/receive -> Deserializes
- ~1-10ms overhead (LAN)

**Optimization Impact**: **99% latency reduction** for same-silo calls

---

## Summary

Orleans message system is built on:
- **Packed headers**: 32-bit bit-fields for efficient metadata
- **Correlation IDs**: Reliable request-response matching
- **CallbackData**: Track pending responses with timeout/cancellation
- **Message flags**: Control routing, reentrancy, lifecycle
- **Time-to-live**: Efficient expiration checking
- **InsideRuntimeClient**: Request lifecycle, callback management, local invocation
- **MessageCenter**: Routing decisions, local optimization, forwarding
- **Cache invalidation**: Proactive updates via message headers
- **Forwarding**: Bounded retries with MaxForwardCount

Key architectural principles:
- Exactly-once completion via Interlocked
- Local optimization (same-silo fast path)
- Automatic forwarding with bounded retries
- Proactive cache invalidation propagation
- Separate timeout, cancellation, silo failure paths

For Moonpool Phase 12: Use correlation-based pattern, embed channels for type safety, implement local optimization, track all pending requests in registry.
