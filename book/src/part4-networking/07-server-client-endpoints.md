# Server, Client, and Endpoints

<!-- toc -->

The `#[service]` macro generates the types. Now we need to understand how to **wire them up**: how the server registers with the transport, how the client connects, and how the endpoint routing system delivers messages to the right place.

## Starting a Server

The generated `Server` type provides two patterns. The simple path uses `serve()`, which spawns background tasks and returns a handle:

```rust
let server = CalculatorServer::init(&transport, JsonCodec);
let handle = server.serve(transport.clone(), Rc::new(CalculatorImpl), &providers);
// Tasks run until handle is dropped or stop() is called
```

`init()` registers all method endpoints with the transport's `EndpointMap`. Each method gets its own `RequestStream`, backed by a `NetNotifiedQueue` that receives incoming request envelopes.

`serve()` consumes the server and spawns one task per method. Each task loops on `recv_with_transport`, dispatches to the handler, and sends the response back through the `ReplyPromise`. The returned `ServerHandle` holds close functions for each stream. Dropping it or calling `stop()` closes the streams, which causes the tasks to exit cleanly.

For more control, you can skip `serve()` and process each `RequestStream` manually:

```rust
let server = CalculatorServer::init(&transport, JsonCodec);
// Handle the `add` stream yourself
while let Some((req, reply)) = server.add.recv_with_transport(&transport).await {
    reply.send(AddResponse { result: req.a + req.b });
}
```

## Connecting a Client

The client side is even simpler. Create a `Client` with the server's address, then call methods on its `ServiceEndpoint` fields:

```rust
let calc = CalculatorClient::new(server_address);

// Each field is a ServiceEndpoint — you choose the delivery mode at the call site
let resp = calc.add.get_reply(&transport, AddRequest { a: 1, b: 2 }, JsonCodec).await?;
assert_eq!(resp.result, 3);
```

There is no `bind()` step and no bound client type. The `ServiceEndpoint` carries the destination address and method UID, and you pass the transport and codec at each call. This makes the delivery mode explicit: `get_reply` for at-least-once, `try_get_reply` for at-most-once, `send` for fire-and-forget. See [Delivery Modes](./08-delivery-modes.md) for the full set.

Under the hood, `get_reply` creates a temporary `ReplyFuture` registered at a unique endpoint, then the response arrives as a packet routed to that endpoint.

## EndpointMap: Token Routing

The `EndpointMap` is the routing table at the heart of `NetTransport`. When a packet arrives with a token, the transport looks it up here to find the receiver.

It uses a **hybrid lookup** strategy:

- **Well-known endpoints** use O(1) array access. The first 64 token indices are reserved for system endpoints. `WellKnownToken::Ping` (index 1) is used for health monitoring, `WellKnownToken::EndpointNotFound` (index 0) handles unroutable messages.
- **Dynamic endpoints** use a `BTreeMap<UID, Rc<dyn MessageReceiver>>`. These are allocated at runtime for service methods and request-response correlation.

```rust
// Well-known: O(1) array lookup
map.insert_well_known(WellKnownToken::Ping, receiver)?;

// Dynamic: BTreeMap lookup
map.insert(UID::new(0xCA1C_0000, 1), receiver);
```

Well-known endpoints cannot be removed. Dynamic endpoints can be registered and deregistered as services come and go.

## WellKnownToken

The `WellKnownToken` enum defines system-level endpoints:

| Token | Index | Purpose |
|-------|-------|---------|
| `EndpointNotFound` | 0 | Handles messages to unknown endpoints |
| `Ping` | 1 | Connection health monitoring |
| `UnauthorizedEndpoint` | 2 | Authentication failures |
| `FirstAvailable` | 3 | First index available for user services |

A well-known UID has `first == u64::MAX` and `second` equal to the token index. The `is_well_known()` method checks this, letting the endpoint map take the fast array path.

## RequestStream

`RequestStream<Req, C>` is the server-side abstraction for receiving typed requests. Each stream wraps a `NetNotifiedQueue` that the transport pushes incoming packets into. When you call `recv()` or `recv_with_transport()`, it awaits the next `RequestEnvelope<Req>` from the queue and returns the deserialized request paired with a `ReplyPromise`.

The `RequestEnvelope` bundles the request payload with a `reply_to` endpoint, the address where the client is listening for the response:

```rust
struct RequestEnvelope<T> {
    request: T,
    reply_to: Endpoint,
}
```

## ReplyPromise and ReplyFuture

These two types form the request-response correlation mechanism.

**`ReplyPromise<T, C>`** lives on the server side. When the server finishes processing a request, it calls `reply.send(response)` to serialize and deliver the response to the client's `reply_to` endpoint. If the promise is dropped without being fulfilled, it automatically sends a `ReplyError::BrokenPromise` to the client so the client does not hang forever.

```rust
// Server side
let (req, reply) = stream.recv_with_transport(&transport).await?;
reply.send(AddResponse { result: req.a + req.b });
```

**`ReplyFuture<T, C>`** lives on the client side. It implements `Future` and resolves when the server's response arrives at the temporary endpoint that `send_request` registered. The future polls a `NetNotifiedQueue` for the response. If the queue is closed (connection failure), it resolves with the appropriate `ReplyError`.

`ReplyFuture` implements `Drop` to close its queue when the future is cancelled or goes out of scope. This prevents leaked wakers and ensures the temporary endpoint is cleaned up even if the caller abandons the request. Without this, a killed process would leave orphaned reply queues that hang forever.

Both types are `!Send` because they contain `Rc<RefCell<...>>` internally. This is deliberate. Our entire execution model is single-threaded, and these types are designed to be efficient within that constraint rather than paying the cost of `Arc<Mutex<...>>` for thread safety we will never use.

## ReplyError

The `ReplyError` enum covers every failure mode in the request-response lifecycle:

| Variant | Meaning |
|---------|---------|
| `BrokenPromise` | Server dropped the promise without responding |
| `ConnectionFailed` | Network connection failed during the request |
| `Timeout` | RPC timed out (default: 30 seconds) |
| `Serialization` | Encoding or decoding failed |
| `EndpointNotFound` | Destination endpoint is not registered |
| `MaybeDelivered` | Peer disconnected, delivery is uncertain |

`MaybeDelivered` is the most important variant. It maps directly to FoundationDB's `request_maybe_delivered` (error 1030). Instead of hiding delivery ambiguity behind a generic timeout, it tells you explicitly: the connection failed and we do not know whether the server processed your request. See [Delivery Modes](./08-delivery-modes.md) for how each delivery function produces this error and [Designing Simulation-Friendly RPC](./10-designing-rpc.md) for strategies to handle it.

## Putting It Together

Here is the complete flow for a single RPC call:

1. **Client** calls `calc.add.get_reply(&transport, req, codec)`, which calls `send_request`
2. `send_request` creates a `ReplyFuture` at a unique temporary endpoint and registers it in the `EndpointMap`
3. The request is serialized as a `RequestEnvelope` with the temporary endpoint as `reply_to`, then sent to the server's method endpoint (`UID(0xCA1C_0000, 1)`)
4. **Transport** routes the packet to the server's `RequestStream` via the `EndpointMap`
5. **Server** receives `(AddRequest, ReplyPromise)` from the stream
6. Server calls `reply.send(AddResponse { ... })`, which serializes and sends to the `reply_to` endpoint
7. **Transport** routes the response packet to the client's temporary endpoint
8. **ReplyFuture** resolves with the deserialized `AddResponse`
9. The temporary endpoint is deregistered from the `EndpointMap`

All of this happens over the same `Peer` connections and wire format we covered in previous chapters. In simulation, every step goes through the `SimWorld` event queue, making the entire RPC flow deterministic and subject to chaos injection.
