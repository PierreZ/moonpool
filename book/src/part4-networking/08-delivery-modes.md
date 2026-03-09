# Delivery Modes

<!-- toc -->

The `#[service]` macro gives you a clean RPC interface, but it hides an important question: **what happens when the connection drops mid-request?** The answer depends on which delivery mode you choose. Moonpool provides four, matching FoundationDB's fdbrpc layer (`fdbrpc.h:727-895`).

## The Four Modes

| Function | Guarantee | Transport | On Disconnect |
|----------|-----------|-----------|---------------|
| `send` | Fire-and-forget | Unreliable | Silently lost |
| `try_get_reply` | At-most-once | Unreliable | `MaybeDelivered` |
| `get_reply` | At-least-once | Reliable | Retransmits on reconnect |
| `get_reply_unless_failed_for` | At-least-once + timeout | Reliable | `MaybeDelivered` after duration |

All four functions live in `moonpool::delivery` and take the same core arguments: a transport, a destination endpoint, a request, and a codec. The difference is in what guarantees they provide and how they handle failures.

## send: Fire-and-Forget

The simplest mode. Send the request unreliably with no reply registered:

```rust
delivery::send(&transport, &endpoint, HeartbeatRequest { node_id }, JsonCodec)?;
```

No `ReplyFuture` is created. No endpoint is registered for a response. If the connection is down, the message is silently dropped. If the server responds, the response is discarded.

Use this for heartbeats, notifications, and any message where losing one is harmless because the next one compensates.

## try_get_reply: At-Most-Once

Send unreliably, then race the reply against a disconnect signal from the `FailureMonitor`:

```rust
let response = delivery::try_get_reply::<_, BalanceResponse, _, _>(
    &transport, &endpoint, GetBalanceRequest { account_id }, JsonCodec,
).await?;
```

Under the hood, this is a `tokio::select!`:

```rust
select! {
    result = reply_future => result,
    () = failure_monitor.on_disconnect_or_failure(&endpoint) => Err(MaybeDelivered),
}
```

There is a fast path: if the `FailureMonitor` already knows the endpoint is failed, it returns `MaybeDelivered` immediately without sending anything. This prevents wasting work on requests that cannot succeed.

The at-most-once guarantee means the server processes your request **zero or one times**. If you get `MaybeDelivered`, the request might have been processed, or it might not have. You must handle this ambiguity explicitly.

## get_reply: At-Least-Once

Send reliably. If the connection drops and reconnects, the transport retransmits the request automatically:

```rust
let reply_future = delivery::get_reply::<_, JoinResponse, _, _>(
    &transport, &endpoint, JoinRequest { node_id }, JsonCodec,
)?;
let response = reply_future.await?;
```

The request sits in the peer's reliable queue. If the TCP connection drops, the peer reconnects (with backoff), and the queued request is resent. The server **may receive the same request multiple times**. Your server must be prepared for duplicates.

This mode never gives up. If the remote process dies permanently, the `ReplyFuture` hangs until the 30-second RPC timeout fires, returning `ReplyError::Timeout`.

## get_reply_unless_failed_for: At-Least-Once with Timeout

Like `get_reply`, but gives up if the endpoint has been continuously failed for a specified duration:

```rust
let response = delivery::get_reply_unless_failed_for::<_, RegisterResponse, _, _>(
    &transport, &endpoint, RegisterRequest { /* ... */ }, JsonCodec,
    Duration::from_secs(10),
).await?;
```

This combines reliable delivery with a failure timeout. First it waits for a disconnect signal from the `FailureMonitor`, then sleeps for the sustained failure duration. If the connection recovers during that sleep, the reliable retransmit resolves the reply future first and the timeout is cancelled.

Use this for singleton RPCs (registration, recruitment) where you want reliable delivery but cannot wait forever if the destination is permanently gone.

## MaybeDelivered: Explicit Ambiguity

The `ReplyError::MaybeDelivered` variant is the most important design decision in the transport layer. Most RPC frameworks hide delivery ambiguity behind generic timeouts. Moonpool, following FDB, makes it **explicit**.

When you receive `MaybeDelivered`, you know exactly one thing: the connection failed while the request was in flight. The request may have been fully processed, partially processed, or never received. The framework refuses to guess.

```rust
match delivery::try_get_reply(&transport, &ep, req, JsonCodec).await {
    Ok(response) => handle_success(response),
    Err(ReplyError::MaybeDelivered) => {
        // Read state to determine if the request was processed
        // before deciding whether to retry
    }
    Err(e) => handle_error(e),
}
```

This forces correct error handling at the application level. Simulation testing with chaos injection will trigger `MaybeDelivered` frequently, revealing any code path that ignores delivery ambiguity.

## Choosing a Delivery Mode

| Use case | Mode | Example |
|----------|------|---------|
| Notification, no reply needed | `send` | Heartbeat, trigger |
| Single server, accept failure | `try_get_reply` | Probe, status check |
| Single server, must deliver | `get_reply` | TLog rejoin, state sync |
| Single server, failure timeout | `get_reply_unless_failed_for` | Registration, recruitment |

The bound client generated by `#[service]` uses `get_reply` by default. For finer control, use the delivery mode functions directly with the transport and endpoint.

For strategies on handling `MaybeDelivered` correctly, including idempotent design, generation numbers, and read-before-retry, see [Designing Simulation-Friendly RPC](./10-designing-rpc.md).
