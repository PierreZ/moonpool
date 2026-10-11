# Admission, Security and Derive

<!-- toc -->

[Typed RPC with moonpool-rpc](./02-rpc.md) covered what a call means. This
chapter covers what happens to a request **before** a handler sees it: the
admission checks, the budgets that push back, the buggify points the runtime
carries inside those paths, who may call what, protocol versions, and the
`derive` macro that generates an interface from a trait.

## What Admission Checks, and When

Before one byte of the body reaches the codec, the server checks, in order:
the incarnation, the token (slot and generation), the interface, the method,
the schema version, the codec, the streaming flag, and then **who is
calling**. A restarted process at the same `ip:port` refuses its
predecessor's references with `StaleIncarnation`. A reused slot never answers
an old token. A caller compiled against schema 2 gets `SchemaMismatch`
instead of garbage. Local calls take the same path, with the same bytes,
limits and security check, so a caller cannot tell a local endpoint from a
remote one except by latency.

## Budgets That Push Back

Every queue the runtime owns has a budget: per endpoint, per connection and
per runtime (`ResourceLimits` and `StreamPolicy` inside `RpcConfig`). A
budget is enforced where work would **enter**, and a refusal there is
`Overloaded` with `NotAdmitted`: nothing was queued, sent or handed to a
handler. Nothing already admitted is ever dropped to make room.

| Budget | Scope | Default |
|---|---|---|
| `RpcConfig::endpoint_queue_capacity`, `ResourceLimits::endpoint_queue_bytes` | endpoint | 256 requests, 16 MiB |
| `ResourceLimits::max_inflight_per_connection`, `max_inflight_requests` | connection, runtime | 1024, 16384 |
| `RpcConfig::max_queued_requests`, `ResourceLimits::max_queued_bytes_per_connection` | connection | 1024, 16 MiB |
| `ResourceLimits::max_retained_bytes` | runtime | 64 MiB of reliable request bodies |
| `StreamPolicy::window_bytes`, `max_buffered_bytes` | stream, consuming runtime | 1 MiB, 256 MiB |
| `StreamPolicy::max_producer_bytes_per_connection`, `max_producer_bytes` | connection, producing runtime | 64 MiB, 1 GiB |
| `RpcConfig::max_frame_bytes` | frame | 1 MiB |

Replies are the interesting one. In the first package replies shared the
control queue, so a caller that pipelined requests to a server whose writer
was stuck could fill that queue and get the whole session closed. Now each
admitted request reserves the room of its reply, and beyond the in-flight
budget new requests are refused before admission. A slow writer pushes back
instead of disconnecting.

Progress under saturation comes from ordering, not from a scheduler. The
writer sends control frames first (handshake, pings, rejections), then the
coalesced stream acknowledgements, then one data frame per source in turn.
The socket reader never waits on an application queue, and reader and
writer both yield after `max_frames_per_batch` frames (64 by default,
measured with the `stream_saturation` example). A burst of 4096 slow calls
against the defaults admits 1024 and refuses the other 3072 as `Overloaded`,
with the session still open.

### Per-Endpoint Admission Queues

Every endpoint takes the runtime's queue (`RpcConfig::endpoint_queue()`)
unless it brings its own. `register_with`, `register_well_known_with` and
`ServiceGroup::serve_with` take an `EndpointQueue`, so one runtime can serve
a latency-sensitive endpoint with a short queue next to a bulk endpoint with
a deep one:

```rust,ignore
use std::num::{NonZeroU64, NonZeroUsize};
use moonpool_rpc::EndpointQueue;

// A short queue: a heartbeat that waits is worse than one refused.
let short = EndpointQueue::new(NonZeroUsize::new(8)?, NonZeroU64::new(64 << 10)?);
let (beat_ref, mut beats) = rpc.register_with::<Heartbeat>(AccessClass::Public, short)?;
```

Both bounds are non-zero by construction: an endpoint that can queue nothing
could never admit a request.

## Buggify Points Inside the Runtime

The campaigns' faults come from outside the runtime: network chaos and
scripted crashes. Those rarely land on the exact instant a contract is
decided. So `moonpool-rpc` carries its own BUGGIFY points through the
zero-dependency `moonpool-buggify` crate, inert outside a simulation. Each
one forces an outcome the contract already allows, never one it rules out:

| Point | What it forces | The contract it exercises |
|---|---|---|
| Cut behind an at-most-once request (`buggify_fault_with_prob!(0.01)`) | The session is flushed and closed right after a two-way request's frame left. | A single attempt fails `MaybeExecuted`, never `NotAdmitted`, and never runs twice. |
| Cut behind a reliable request (`buggify_fault_with_prob!(0.1)`) | The same, after a reliable request. | The retained copy is sent again and may run more than once. |
| Spurious overload at admission (`buggify_fault_with_prob!(0.02)`) | A request is refused `Overloaded` after its security check, before any queue. | Declined without effect, and a balancer fails over. |
| Deferred stream acknowledgement (`buggify_with_prob!(0.5)`) | A taken item's acknowledgement waits for the next item's cumulative one. | Credit is delayed, never lost. |
| Spurious failure-monitor wakeup (`buggify_with_prob!(0.1)`) | The change latch is bumped with nothing changed. | Every wait re-checks its condition. |

The first three use `buggify_fault_with_prob!`, so they stop when the chaos
window closes and a campaign's recovery checks see a quiet runtime. Each one
is counted in `RpcStats` (`injected_request_cuts`,
`injected_reliable_request_cuts`, `injected_overloads`,
`injected_ack_deferrals`, `injected_spurious_wakeups`), and the campaigns
turn every non-zero counter into an `assert_reachable!`, so a sweep shows
that each point fired. See [Buggify](../part3-building/08-buggify.md) for
how activation and swarm masks reach these sites.

## Who May Call What

An address is not an identity, and neither is an endpoint token. Both are
routing data anyone can copy. `moonpool-rpc` separates three questions.
**Who is the server?** Server-authenticated TLS answers it. **Who is the
client?** A credential attached to each request answers it, verified by the
server. **May this client call this endpoint?** An `AccessPolicy` answers
it, per request, in admission, before the body is decoded.

Every endpoint is registered `Public` or `Private`. **Private endpoints fail
closed.** The default `SecurityConfig` verifies nothing, so public endpoints
answer anyone and private ones refuse everyone:

```rust,ignore
use moonpool_rpc::security::jwt::{JwksKeys, JwtConfig, JwtVerifier};  // feature `jwt`
use moonpool_rpc::security::{Credential, SecurityConfig, SystemUtc};

let keys = JwksKeys::from_json(&jwks_document)?;
let verifier = JwtVerifier::new(keys.clone(), JwtConfig::new("issuer", "rpc"))?;
let config = RpcConfig {
    security: SecurityConfig::enforced(verifier).with_clock(SystemUtc),
    ..RpcConfig::default()
};
// Callers attach a credential per runtime (`SecurityConfig::with_credentials`)
// or per client.
let client = service_ref.bind(&rpc).with_credentials(Credential::bearer(token));
```

Three starting points exist. `SecurityConfig::default()` verifies nothing.
`SecurityConfig::enforced(verifier)` verifies credentials and authorizes
with `DefaultPolicy`, or your own `with_policy`. `SecurityConfig::trusted_network()`
admits everything and claims nothing, FoundationDB's plaintext default, here
a choice and never an accident. An `IpAllowList` (`with_allow_list`) refuses
connections before a byte is read, but it restricts reachability only: an
allowed address still needs a credential for a private endpoint.

A refusal is `Unauthenticated(CredentialError)` or `PermissionDenied`. The
`CredentialError` names what is wrong: `Missing`, `Malformed`, `TooLarge`,
`UnsupportedAlgorithm`, `UnknownKey`, `BadSignature`, `WrongIssuer`,
`WrongAudience`, `Expired`, `NotYetValid`, `ClockUnavailable`,
`InsecureTransport` or `Rejected`. Both refusals are `NotAdmitted` and
neither is terminal for the reference, because keys, tokens and policies
change. The handler sees the verified caller through
`ReplyHandle::principal()`. Denials are audit events (target
`moonpool_rpc::audit`) that name the endpoint, the peer and the reason, never
the credential, and the `moonpool_rpc_requests_denied_total{reason}` counter
aggregates them.

**JWT and JWKS** (feature `jwt`). `JwtVerifier` checks, in order: size,
header and algorithm allow list (public-key algorithms only, so the classic
HS256 confusion is refused), the key id in the **current** key set, the
signature, issuer and audience, then `exp` and `nbf`. `JwksKeys::replace_json`
swaps the whole set, as FoundationDB's `applyPublicKeySet` does: a key left
out is revoked at once.

**Credentials never leave on a session that cannot protect them.** A request
carrying a credential goes out only on an encrypted session, or when the
client opted in with `send_credentials_over_plaintext()`, and never on a
protocol version 1 session. A refused send fails as `NotAdmitted` with
`CredentialWithheld`. A session this runtime **accepted** never carries one
either, because nothing authenticated its dialer.

**TLS** (feature `tls`). `security::tls::Tls` is a session upgrade over the
provider's own streams (rustls through `futures-rustls`, TLS 1.3, the `ring`
provider), passed to `RpcDriver::listen_with` or `client_only_with`. The
client verifies the server's chain and the name it expects (`ServerNames`,
by default the IP it dialed). `RotatingCertificate::replace` and
`TlsClient::replace_roots` rotate for new sessions. **Mutual TLS is not
provided**: clients are authenticated by their request credentials.

**Two clocks.** `TimeProvider::now` is scheduling time, never Unix time, so
credential and certificate validity read a separate `UtcClock`: `SystemUtc`
in production, a scripted `FixedUtc` in tests and simulation. The default
`NoUtc` knows no time, so an expiring credential fails closed until a clock
is chosen. The TLS and JWT adapters are native-only features. The policy
itself (`SecurityConfig`, `AccessPolicy`, `RequestVerifier`, allow lists, key
rotation, the clocks) is always compiled, so simulations and wasm builds
exercise the same trust decisions.

## Protocol Versions and Rolling Upgrades

Each runtime announces the versions it speaks (`RpcConfig::protocol_versions`,
`1..=2` by default). A session runs at the highest version both sides
speak, and a peer with none in common is refused at the handshake
(`version_rejections` counts them). Version 2 changes no frame layout: it
gives the request's metadata section its credential meaning and adds three
reply statuses. **Nothing newer than the negotiated version is ever sent or
decoded.** A refusal a version 1 peer cannot decode reaches it as the
version 1 status that proves the same thing: endpoint not found. A server
that verifies credentials speaks version 2 only, so an old client is refused
at the handshake instead of being let in anonymously. Golden hex fixtures
(`tests/fixtures/wire-v1.txt`, `wire-v2.txt`) pin every envelope of both
versions. To pin a build during an upgrade, narrow the range to `1..=1`.

## Generated Interfaces

With the `derive` feature, one trait generates the interface written by hand
in the previous chapter:

```rust,ignore
#[moonpool_rpc::service(id = 0x6b76_0001, version = 1)]
pub trait Kv {
    #[method(id = 1, schema = 1)]
    async fn get(&self, key: Key) -> Value;
    #[method(id = 2, schema = 1)]
    async fn put(&self, entry: Entry);
}
```

It generates `KvInterface` (the `RpcInterface` marker), one method marker per
method (`KvGet`, `KvPut`), `KvRef` (an `InterfaceRef<KvInterface>`),
`KvClient<P>`, `KvRequest` (one variant per method, holding the
`IncomingRequest` and its `ReplyHandle`) and `KvServer`. Serving a handler
that implements the trait is three calls:

```rust,ignore
let kv = KvServer::register(&rpc, AccessClass::Public)?;
directory.publish(kv.interface_ref().to_bytes());   // the application's job
kv.serve(&store).await;   // pulls the next request of any method and replies

// A caller: every delivery mode, per method.
let kv = KvClient::bind(&KvRef::from_bytes(&published)?, &rpc)?;
let value = kv.get().try_get_reply(&key).await?;
```

The macro owns no protocol state. A server that wants to reply later, never
reply or look at the caller calls `kv.next().await` and matches on
`KvRequest::Get(IncomingRequest { request, reply })`. Ids stay explicit, and
a duplicate method id is a compile error, also when it hides behind a
constant. A hand-written interface with the same ids is byte-for-byte
interchangeable with the generated one: `tests/derive.rs` serves a derived
server to a manual client and a manual server to a derived client. The
`derive` feature is off by default, so a production tree stays free of
proc-macro dependencies unless asked for. Through the facade crate, the
features are `moonpool`'s `rpc`, `rpc-derive`, `rpc-tls` and `rpc-jwt`.
