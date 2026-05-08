# Drop Semantics and the WaitFailure Pattern

<!-- toc -->

The previous chapter showed an interface flowing as bytes between processes. Once those bytes land and the client starts calling methods, the next question is what happens when the **server** stops answering. Not when the network drops, that path has its own apparatus through the failure monitor, but when the server-side actor that owns a `ReplyPromise` simply goes away.

The answer turns on one tiny contract on `ReplyPromise`. Drop a promise without fulfilling it and the client gets `BrokenPromise`. That single rule is the foundation of FoundationDB's **WaitFailure** pattern, and it is why the moonpool transport surfaces process death without any heartbeat machinery at all.

`BrokenPromise` is reserved for that liveness signal. When a handler returns an `Err` while the server is still alive and well, the wire carries `ReplyError::Application { message }` instead. The two errors travel through the same packet shape, but only `BrokenPromise` triggers the failure monitor's permanent-failure path on receipt. We rely on this distinction below.

## The Drop Contract

`ReplyPromise<T>` is the server-side handle handed out alongside each request. The client created a `ReplyFuture` when it called `get_reply`, and that future is registered in the transport's endpoint map waiting for a reply packet. The promise carries the address it must reply to and a serialized encoder for `Result<T, ReplyError>`. The normal happy path is `reply.send(value)`. But what if nothing calls `send`?

The `Drop` impl in `moonpool-transport/src/rpc/reply_promise.rs:166-183` covers exactly this case:

```rust
impl<T: Serialize> Drop for ReplyPromise<T> {
    fn drop(&mut self) {
        let mut inner = self.inner.borrow_mut();
        if !inner.fulfilled {
            let result: Result<T, ReplyError> = Err(ReplyError::BrokenPromise);
            if let Ok(payload) = (inner.encode_fn)(&result)
                && let Some(sender) = inner.sender.take()
            {
                sender(&inner.reply_endpoint, &payload);
            }
            inner.fulfilled = true;
        }
    }
}
```

The promise serializes `Err(BrokenPromise)` and ships it to the client's reply endpoint as if the server had explicitly called `send_error`. The client's `ReplyFuture` resolves with `Err(ReplyError::BrokenPromise)` exactly the same way it would resolve any other server-emitted error. There is no special path on the client. There is no second channel. The error rides the same packet shape as a normal reply.

This means a server can panic, a future can be cancelled, an `Rc` can drop to zero, or a `match` arm can simply forget to call `send`, and the client learns within one network round-trip. No timeout has to fire. The failure monitor does not have to declare the address dead. The client just gets a typed error.

## Liveness Without Heartbeats

FoundationDB extends the contract into a coordination primitive. The reference is `docs/analysis/foundationdb/layer-3-fdbrpc.md:357-371`, the **WaitFailure** server:

```cpp
ACTOR Future<Void> waitFailureServer(FutureStream<ReplyPromise<Void>> requests) {
    state std::vector<ReplyPromise<Void>> outstanding;
    loop {
        ReplyPromise<Void> req = waitNext(requests);
        outstanding.push_back(req);
    }
}
```

The server receives ping-style requests and **never** answers any of them. Each promise sits in a `Vec` for as long as the actor lives. The actor's job is to be alive, and the proof of life is that it keeps owning the promises. When the actor terminates, the `Vec` goes out of scope, every promise it held drops at once, and every client that was waiting on those replies receives `BrokenPromise` in the same instant.

Compare this to a periodic heartbeat. A heartbeat charges the system every interval whether anything has changed or not. If you set the heartbeat to one second, every monitored process pays one timer tick and one network round-trip per second forever. Most of those ticks deliver no information at all because the process did not in fact die during that interval.

The WaitFailure pattern inverts the cost. The price of "I am alive" is one `ReplyPromise` held in memory. The price of "I just died" is one packet per waiting client, sent automatically by the runtime as the promise drops. Until the death event happens, the system is silent. The information is delivered exactly when there is information to deliver, and the latency is bounded by the time it takes the operating system to release the actor's memory.

## A Minimal Example

Here is the same pattern in moonpool's API, readable as a continuation of the calculator setup from earlier chapters:

```rust
#[service]
trait Liveness {
    async fn watch(&self, req: WatchRequest) -> Result<(), RpcError>;
}

// Server side: hold every reply promise, never answer.
async fn run_liveness_server(transport: Rc<NetTransport>) {
    let server = Liveness::well_known(&transport, WLTOKEN_LIVENESS);
    let mut held: Vec<ReplyPromise<()>> = Vec::new();
    loop {
        if let Some((_req, reply)) = server.watch.recv().await {
            held.push(reply);
        } else {
            break;
        }
    }
    // Implicit: when this function returns or panics, `held` drops,
    // every ReplyPromise drops, every waiting client gets BrokenPromise.
}

// Client side: any awaiter of `watch` learns about server death.
async fn watch_server(client: Liveness) {
    match client.watch.get_reply(WatchRequest {}).await {
        Ok(()) => unreachable!("server is supposed to hold forever"),
        Err(RpcError::Reply(ReplyError::BrokenPromise)) => {
            tracing::warn!("liveness server died, taking action");
        }
        Err(e) => tracing::error!(?e, "unexpected error"),
    }
}
```

The client does not poll. The client awaits a single `get_reply` call. The contract is "you stay blocked until I die". When the server panics or the runtime tears down the actor, the held promises drop in their `Vec`, the runtime ships `Err(BrokenPromise)` to every blocked client, and each client's await returns with the typed error.

## What This Buys Us

The drop contract removes an entire category of liveness bookkeeping. There is no heartbeat task, no skew clock, no missed-N-pings counter, no flap-detector. There is one `ReplyPromise` per watcher, sitting in memory, doing nothing until something interesting happens. The runtime guarantees the notification on death, and the chaos engine in simulation exercises both the success and failure paths through the same machinery that handles normal RPC.

The pattern composes with the failure monitor. A server that gets killed by chaos drops its promises during teardown, and the client learns through `BrokenPromise` before the failure monitor even gets around to marking the address `Failed`. The two signals overlap in the simulation event stream, and the test driver can correlate them to validate that the application reacted to either one.

Drop semantics are not a special feature here. They are the absence of a feature. The transport guarantees a well-defined error on a destructor, and that minimal guarantee is enough to build coordination on top of.
