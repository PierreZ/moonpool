# Server, Client, and Endpoints

<!-- toc -->

- Server: `CalculatorServer::new(endpoint)` with `init()` and `serve()`
- Client: `CalculatorClient::bind(&providers, &endpoints)` returns `BoundCalculatorClient`
- EndpointMap: token → receiver routing, O(1) well-known lookup
- Well-known endpoints: deterministic addressing via `WellKnownToken`
- RequestStream: async request channel for incoming messages
- ReplyPromise / ReplyFuture: request-response futures (non-Send, contains `Rc<RefCell>`)
- NetNotifiedQueue: type-safe message queue with waker-based notification
