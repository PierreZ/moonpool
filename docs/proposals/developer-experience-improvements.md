# Moonpool Developer Experience Improvements

**Status**: Proposal
**Date**: 2026-01-10
**Author**: Analysis based on FDB actor model and current codebase

## Executive Summary

This document proposes improvements to Moonpool's developer experience based on analysis of:
1. FoundationDB's actor model (docs/analysis/fdb-actor.md)
2. Current calculator example (moonpool-transport/examples/calculator.rs)
3. Comprehensive codebase exploration

The Phase 12C APIs (`NetTransportBuilder`, `register_handler_at`, `recv_with_transport`) represent excellent progress. This proposal identifies remaining friction points and actionable improvements.

---

## Current State Assessment

### What's Working Well

| Component | DX Rating | Notes |
|-----------|-----------|-------|
| Provider traits | 9/10 | Clean abstraction, simulation-ready |
| NetTransportBuilder | 9/10 | Eliminates Rc/weak_self footgun |
| Codec system | 8/10 | Pluggable, debuggable with JSON |
| Buggify/assertion macros | 8/10 | Excellent chaos testing support |
| Documentation | 9/10 | FDB references enable understanding |

### Pain Points Identified

| Issue | Severity | Impact |
|-------|----------|--------|
| Type parameter explosion (`<N, T, TP>`) | High | Cascades through all user code |
| Reply token generation (non-deterministic) | High | Breaks simulation reproducibility |
| Message type boilerplate | Medium | Repetitive derive annotations |
| Turbofish in recv_with_transport | Medium | Complex type inference |
| Multi-method interface setup | Low | Verbose but functional |

---

## Tiered Improvement Proposals

### Tier 1: Quick Wins (Low Effort, High Impact)

#### 1.1 Fix Reply Token Generation [CRITICAL]

**Current code** (`moonpool-transport/src/rpc/request.rs:108-112`):
```rust
fn rand_reply_id() -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(0x1_0000_0000);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}
```

**Problem**: Uses global atomic counter instead of `RandomProvider`, breaking simulation determinism.

**Solution**: Accept `RandomProvider` and use it for token generation:

```rust
pub fn send_request<Req, Resp, N, T, TP, R, C>(
    transport: &NetTransport<N, T, TP>,
    random: &R,  // NEW: RandomProvider for deterministic tokens
    destination: &Endpoint,
    request: Req,
    codec: C,
) -> Result<ReplyFuture<Resp, C>, MessagingError>
where
    R: RandomProvider,
    // ... existing bounds
{
    let reply_token = UID::new(random.random(), random.random());
    // ...
}
```

**Alternative**: Add `RandomProvider` to `NetTransport` struct itself:
```rust
pub struct NetTransport<N, T, TP, R = ()>
where
    R: RandomProvider + Clone + 'static,
```

**Effort**: 1-2 hours
**Impact**: Enables deterministic simulation testing of RPC flows

---

#### 1.2 Type Aliases for Common Configurations

**Current pain**: Every function signature includes `<N, T, TP>`:
```rust
fn my_handler<N, T, TP>(transport: &Rc<NetTransport<N, T, TP>>)
where
    N: NetworkProvider + Clone + 'static,
    T: TimeProvider + Clone + 'static,
    TP: TaskProvider + Clone + 'static,
```

**Solution**: Add type aliases in `moonpool-transport/src/lib.rs`:

```rust
/// Type alias for tokio-based transport (production use)
pub type TokioTransport = NetTransport<TokioNetworkProvider, TokioTimeProvider, TokioTaskProvider>;

/// Type alias for simulation transport (testing use)
#[cfg(feature = "sim")]
pub type SimTransport = NetTransport<SimNetworkProvider, SimTimeProvider, SimTaskProvider>;
```

**Effort**: 30 minutes
**Impact**: Cleaner signatures in examples and user code

---

#### 1.3 Convenience Methods on NetTransport

**Current**:
```rust
let future: ReplyFuture<Resp, JsonCodec> = send_request(
    &transport,
    &endpoint,
    request,
    JsonCodec,
)?;
```

**Proposed**: Add method directly on transport:
```rust
impl<N, T, TP> NetTransport<N, T, TP> {
    /// Send a request and receive a typed response.
    pub fn call<Req, Resp, C>(
        &self,
        destination: &Endpoint,
        request: Req,
        codec: C,
    ) -> Result<ReplyFuture<Resp, C>, MessagingError>
    where
        Req: Serialize,
        Resp: DeserializeOwned + 'static,
        C: MessageCodec,
    {
        send_request(self, destination, request, codec)
    }
}
```

**Usage becomes**:
```rust
let future = transport.call(&endpoint, request, JsonCodec)?;
```

**Effort**: 30 minutes
**Impact**: More discoverable API, fewer imports

---

### Tier 2: Medium Effort Improvements

#### 2.1 Message Definition Macro

**Current boilerplate** (repeated for every RPC):
```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
struct AddRequest {
    a: i64,
    b: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AddResponse {
    result: i64,
}
```

**Proposed macro**:
```rust
/// Defines request and response types with required derives.
#[macro_export]
macro_rules! rpc_messages {
    (
        $(#[$req_meta:meta])*
        request $req:ident { $($req_field:ident: $req_ty:ty),* $(,)? }

        $(#[$resp_meta:meta])*
        response $resp:ident { $($resp_field:ident: $resp_ty:ty),* $(,)? }
    ) => {
        $(#[$req_meta])*
        #[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
        pub struct $req {
            $(pub $req_field: $req_ty,)*
        }

        $(#[$resp_meta])*
        #[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
        pub struct $resp {
            $(pub $resp_field: $resp_ty,)*
        }
    };
}
```

**Usage**:
```rust
rpc_messages! {
    request AddRequest { a: i64, b: i64 }
    response AddResponse { result: i64 }
}
```

**Effort**: 2-3 hours
**Impact**: Reduces boilerplate, enforces consistency

---

#### 2.2 Interface Definition Helper

**Current** (calculator.rs):
```rust
const CALC_INTERFACE: u64 = 0xCA1C_0000;
const METHOD_ADD: u64 = 0;
const METHOD_SUB: u64 = 1;
// ... manual registration for each method
```

**Proposed macro**:
```rust
/// Define an RPC interface with multiple methods.
#[macro_export]
macro_rules! rpc_interface {
    (
        $(#[$meta:meta])*
        $vis:vis interface $name:ident ($base_token:expr) {
            $(
                $method:ident($req:ty) -> $resp:ty [$index:expr]
            ),* $(,)?
        }
    ) => {
        $(#[$meta])*
        $vis struct $name;

        impl $name {
            /// Base token for this interface
            pub const BASE: u64 = $base_token;

            $(
                #[doc = concat!("Endpoint token for ", stringify!($method), " method")]
                pub const [<$method:upper>]: $crate::UID =
                    $crate::UID::new($base_token, $index);
            )*
        }

        impl $name {
            /// Create endpoints for all methods pointing to a server address.
            pub fn endpoints(server: &$crate::NetworkAddress) -> [<$name Endpoints>] {
                [<$name Endpoints>] {
                    $(
                        $method: $crate::Endpoint::new(server.clone(), Self::[<$method:upper>]),
                    )*
                }
            }
        }

        $vis struct [<$name Endpoints>] {
            $(pub $method: $crate::Endpoint,)*
        }
    };
}
```

**Usage**:
```rust
rpc_interface! {
    pub interface Calculator(0xCA1C_0000) {
        add(AddRequest) -> AddResponse [0],
        sub(SubRequest) -> SubResponse [1],
        mul(MulRequest) -> MulResponse [2],
        div(DivRequest) -> DivResponse [3],
    }
}

// Client usage
let endpoints = Calculator::endpoints(&server_addr);
let future = transport.call(&endpoints.add, req, JsonCodec)?;

// Server usage
let (add_stream, _) = transport.register_handler_at::<AddRequest, _>(
    Calculator::BASE, 0, JsonCodec);
```

**Effort**: 4-6 hours (requires paste crate for identifier concatenation)
**Impact**: Significant reduction in multi-method interface boilerplate

---

#### 2.3 Builder Pattern for Requests

**Current**:
```rust
let future: ReplyFuture<Resp, JsonCodec> = send_request(&transport, &endpoint, req, JsonCodec)?;
match time.timeout(Duration::from_secs(5), future).await {
    Ok(Ok(Ok(resp))) => { /* success */ }
    _ => { /* error */ }
}
```

**Proposed fluent API**:
```rust
impl<N, T, TP> NetTransport<N, T, TP> {
    pub fn request<Req>(&self, destination: &Endpoint, request: Req) -> RequestBuilder<Req, N, T, TP>
    where Req: Serialize {
        RequestBuilder::new(self, destination, request)
    }
}

pub struct RequestBuilder<'a, Req, N, T, TP> {
    transport: &'a NetTransport<N, T, TP>,
    destination: &'a Endpoint,
    request: Req,
    timeout: Option<Duration>,
    retries: u32,
}

impl<'a, Req, N, T, TP> RequestBuilder<'a, Req, N, T, TP> {
    pub fn with_timeout(mut self, duration: Duration) -> Self {
        self.timeout = Some(duration);
        self
    }

    pub fn with_retries(mut self, count: u32) -> Self {
        self.retries = count;
        self
    }

    pub async fn send<Resp, C>(self, codec: C) -> Result<Resp, RpcError>
    where
        Resp: DeserializeOwned + 'static,
        C: MessageCodec,
    {
        // Implementation with timeout and retry logic
    }
}
```

**Usage**:
```rust
let response: AddResponse = transport
    .request(&endpoint, AddRequest { a: 1, b: 2 })
    .with_timeout(Duration::from_secs(5))
    .send(JsonCodec)
    .await?;
```

**Effort**: 4-6 hours
**Impact**: Cleaner async code, built-in timeout/retry

---

### Tier 3: Architectural Improvements (Higher Effort)

#### 3.1 Provider Bundle Type

**Problem**: `<N, T, TP>` appears in nearly every signature.

**Solution**: Bundle providers into a single type:

```rust
/// Bundle of all providers needed for transport operation.
pub trait ProviderBundle: Clone + 'static {
    type Network: NetworkProvider + Clone + 'static;
    type Time: TimeProvider + Clone + 'static;
    type Task: TaskProvider + Clone + 'static;
    type Random: RandomProvider + Clone + 'static;

    fn network(&self) -> &Self::Network;
    fn time(&self) -> &Self::Time;
    fn task(&self) -> &Self::Task;
    fn random(&self) -> &Self::Random;
}

/// Tokio provider bundle for production use.
#[derive(Clone)]
pub struct TokioProviders {
    network: TokioNetworkProvider,
    time: TokioTimeProvider,
    task: TokioTaskProvider,
    random: TokioRandomProvider,
}

impl ProviderBundle for TokioProviders { /* ... */ }

/// Simplified transport type.
pub struct Transport<P: ProviderBundle> {
    providers: P,
    // ... other fields
}
```

**Benefits**:
- Single type parameter instead of three
- Easier to pass around
- Natural place for `RandomProvider`

**Effort**: 1-2 days (significant refactor)
**Impact**: Major simplification of all signatures

---

#### 3.2 Derive Macro for Services (Future)

Ultimate goal inspired by FDB's interfaces:

```rust
#[derive(RpcService)]
pub trait CalculatorService {
    #[rpc(method = 0)]
    async fn add(&self, req: AddRequest) -> AddResponse;

    #[rpc(method = 1)]
    async fn sub(&self, req: SubRequest) -> SubResponse;
}

// Generates:
// - CalculatorClient with methods that send RPCs
// - CalculatorServer trait to implement
// - CalculatorEndpoints struct
// - Token constants
```

**Effort**: 2-4 days (proc macro)
**Impact**: Ultimate DX - matches gRPC/tonic ergonomics

---

## Comparison: Before and After

### Calculator Client (Current)

```rust
let add_endpoint = Endpoint::new(server_addr.clone(), UID::new(CALC_INTERFACE, METHOD_ADD));

let future: ReplyFuture<AddResponse, JsonCodec> = send_request(
    &transport,
    &add_endpoint,
    AddRequest { a: 10, b: 5 },
    JsonCodec,
)?;

match time.timeout(Duration::from_secs(5), future).await {
    Ok(Ok(Ok(resp))) => println!("Result: {}", resp.result),
    _ => println!("Error"),
}
```

### Calculator Client (After Tier 1+2)

```rust
let endpoints = Calculator::endpoints(&server_addr);

let resp: AddResponse = transport
    .request(&endpoints.add, AddRequest { a: 10, b: 5 })
    .with_timeout(Duration::from_secs(5))
    .send(JsonCodec)
    .await?;

println!("Result: {}", resp.result);
```

---

## Implementation Priority

| Priority | Item | Effort | Impact | Dependencies |
|----------|------|--------|--------|--------------|
| P0 | 1.1 Fix reply token generation | 2h | Critical | None |
| P1 | 1.2 Type aliases | 30m | High | None |
| P1 | 1.3 Convenience methods | 30m | High | None |
| P2 | 2.1 Message macro | 3h | Medium | None |
| P2 | 2.3 Request builder | 5h | High | 1.1, 1.3 |
| P3 | 2.2 Interface macro | 6h | Medium | 2.1 |
| P4 | 3.1 Provider bundle | 2d | High | None (but breaking) |

---

## Appendix: FDB Patterns Not Yet Implemented

From `docs/analysis/fdb-actor.md`, these patterns remain opportunities:

1. **Interface serialization optimization**: FDB only serializes one endpoint token, deriving others via offsets. Moonpool could adopt this for bandwidth efficiency.

2. **Well-known endpoint bootstrap**: FDB uses compile-time constants for system services. Moonpool has `WellKnownToken` but could expand usage.

3. **ServerDBInfo broadcast**: FDB broadcasts interface information cluster-wide. Consider for service discovery.

4. **FailureMonitor integration**: FDB tracks endpoint health cluster-wide. Could enhance Peer with failure detection callbacks.

---

## Conclusion

Moonpool has solid architectural foundations. The proposed improvements focus on developer ergonomics without architectural changes (Tiers 1-2) while laying groundwork for future simplification (Tier 3).

**Recommended next steps**:
1. Implement P0 (reply token fix) immediately - it's a correctness issue
2. Implement P1 items (type aliases, convenience methods) - quick wins
3. Consider P2 items based on user feedback
4. Evaluate P4 for next major version
