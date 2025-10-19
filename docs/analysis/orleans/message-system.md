# Message System and Request-Response Pattern

## Reference Files
- `Message.cs:1-428` - Message structure with packed headers
- `CallbackData.cs:1-220` - Response tracking and timeout management

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

**Code**: `Message.cs:8-36`

```csharp
internal sealed class Message
{
    // Core identification
    public CorrelationId _id;
    public object BodyObject { get; set; }

    // Addressing
    public SiloAddress _targetSilo;
    public GrainId _targetGrain;
    public SiloAddress _sendingSilo;
    public GrainId _sendingGrain;

    // Interface metadata
    public ushort _interfaceVersion;
    public GrainInterfaceType _interfaceType;

    // Headers and metadata
    public PackedHeaders _headers;
    public Dictionary<string, object> _requestContextData;
    public List<GrainAddressCacheUpdate> _cacheInvalidationHeader;

    // Timing
    public CoarseStopwatch _timeToExpiry;
    private short _retryCount;
}
```

**Key Design Decisions**:

1. **Correlation ID** (line 22)
   - Unique identifier for request-response matching
   - Type: `CorrelationId` (likely a `Guid` or `long`)
   - Used by `CallbackData` to route responses

2. **Four-address routing** (lines 26-30)
   - `TargetSilo` + `TargetGrain`: Where message goes
   - `SendingSilo` + `SendingGrain`: Where response returns
   - Enables routing at both silo and grain level

3. **Interface versioning** (lines 32-33)
   - `_interfaceVersion`: Grain interface version
   - `_interfaceType`: Type identifier for the interface
   - Enables version compatibility checks (ActivationData.cs:1012-1029)

4. **Time-to-live** (line 17)
   - `CoarseStopwatch`: Low-resolution stopwatch (probably millisecond precision)
   - Tracks remaining time, not expiry timestamp
   - Efficient: No allocations, just arithmetic

### Packed Headers Optimization

**Code**: `Message.cs:381-425`

```csharp
internal struct PackedHeaders
{
    // 32 bits: HHHH_HHHH RRRR_DDDD FFFF_FFFF FFFF_FFFF
    // F: 16 bits for MessageFlags
    // D: 4 bits for Direction
    // R: 4 bits for ResponseType
    // H: 8 bits for ForwardCount (hop count)
    private uint _fields;

    public int ForwardCount
    {
        readonly get => (int)(_fields >> ForwardCountShift);  // Shift 24 bits
        set => _fields = (_fields & ~ForwardCountMask) | (uint)value << ForwardCountShift;
    }

    public Directions Direction
    {
        readonly get => (Directions)((_fields & DirectionMask) >> DirectionShift);  // Shift 16 bits
        set => _fields = (_fields & ~DirectionMask) | (uint)value << DirectionShift;
    }

    public ResponseTypes ResponseType
    {
        readonly get => (ResponseTypes)((_fields & ResponseTypeMask) >> ResponseTypeShift);  // Shift 20 bits
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
- **Serialization**: Can write as single `uint`

**Access patterns**:
- **Read**: Mask + shift (`(_fields & mask) >> shift`)
- **Write**: Clear bits + set new value (`(_fields & ~mask) | (value << shift)`)
- **Flag check**: Bitwise AND (`(_fields & flag) != 0`)
- **Flag set**: Bitwise OR/AND NOT (`_fields | flag` or `_fields & ~flag`)

### Message Directions

**Code**: `Message.cs:40-46, 69-73`

```csharp
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

**Code**: `Message.cs:357-379, 90-142`

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

1. **ReadOnly** (line 102-106)
   - Indicates message doesn't mutate grain state
   - Enables read-only interleaving (ActivationData.cs:1129-1131)
   - Multiple read-only messages can execute concurrently

2. **AlwaysInterleave** (line 108-112)
   - Message can always interleave with others
   - Used for system messages, timers, diagnostics
   - Bypasses reentrancy checks (ActivationData.cs:1119-1121)

3. **IsLocalOnly** (line 126-130)
   - Message bound to specific activation instance
   - Cannot be forwarded during migration or deactivation
   - Used for internal grain operations

4. **SuppressKeepAlive** (line 138-142, inverted)
   - By default, messages extend activation lifetime
   - Setting this prevents lifetime extension
   - Used for probes, diagnostics

**Flag combinations**: Multiple flags can be set simultaneously

### Time-to-Live Tracking

**Code**: `Message.cs:17, 80, 208-236, 268-275`

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

**Code**: `Message.cs:150-154`

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

**Where incremented**: When activation deactivates and forwards messages (ActivationData.cs:1452-1458)

### Cache Invalidation

**Code**: `Message.cs:238-287`

```csharp
public List<GrainAddressCacheUpdate> CacheInvalidationHeader { get; set; }

internal void AddToCacheInvalidationHeader(GrainAddress invalidAddress, GrainAddress validAddress)
{
    var list = new List<GrainAddressCacheUpdate>();
    if (CacheInvalidationHeader != null)
    {
        list.AddRange(CacheInvalidationHeader);
    }
    list.Add(new GrainAddressCacheUpdate(invalidAddress, validAddress));
    CacheInvalidationHeader = list;
}
```

**Purpose**: Piggyback cache invalidation on responses

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

**Code**: `CallbackData.cs:10-28`

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

**Code**: Used throughout CallbackData.cs (lines 87, 104, 134, 154)

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

**Code**: `CallbackData.cs:102-130`

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

**Configuration**: `GetResponseTimeout()` (line 73)
- Check if request has custom timeout (`IInvokable.GetDefaultResponseTimeout()`)
- Fall back to shared configuration timeout
- Per-request timeout flexibility

### Cancellation Handling

**Code**: `CallbackData.cs:34-100`

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

private void SignalCancellation() =>
    shared.CancellationManager.SignalCancellation(Message.TargetSilo, Message.TargetGrain, Message.SendingGrain, Message.Id);
```

**Two cancellation modes**:

1. **Wait for acknowledgement** (lines 78-82)
   - Send cancellation signal
   - Keep waiting for response or timeout
   - Remote grain decides whether to honor cancellation
   - Caller continues waiting

2. **Cancel immediately** (lines 87-99)
   - Send cancellation signal
   - Complete callback with `OperationCanceledException`
   - Caller's task completes immediately
   - Remote grain may still be processing

**Configuration**: `shared.WaitForCancellationAcknowledgement` controls mode

**Cancellation signal**: `CancellationManager.SignalCancellation()` (line 49)
- Sends message to target silo
- Target looks up activation and pending request
- Calls `IInvokable.TryCancel()` on request (ActivationData.cs:1969-1977)

### Target Silo Failure

**Code**: `CallbackData.cs:132-150`

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

**Code**: `CallbackData.cs:152-203`

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

**Code**: `CallbackData.cs:51-54`

```csharp
public void OnStatusUpdate(StatusResponse status)
{
    this.lastKnownStatus = status;
}
```

**Purpose**: Track progress of long-running requests

**Used in**: Timeout and silo failure messages (lines 124, 146)
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

**Lookup**: `callbacks[correlationId]` → `CallbackData` → `IResponseCompletionSource` → caller's Task

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

## Summary

Orleans message system is built on:
- **Packed headers**: 32-bit bit-fields for efficient metadata
- **Correlation IDs**: Reliable request-response matching
- **CallbackData**: Track pending responses with timeout/cancellation
- **Message flags**: Control routing, reentrancy, lifecycle
- **Time-to-live**: Efficient expiration checking

Key architectural principles:
- Exactly-once completion via Interlocked
- Separate timeout, cancellation, silo failure paths
- Flexible per-request configuration
- Comprehensive instrumentation

For Moonpool Phase 12: Use correlation-based pattern, embed channels for type safety, implement timeout via Provider traits, track all pending requests in registry.
