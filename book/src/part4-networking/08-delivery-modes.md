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

All four modes are available as methods on the `RemoteMethod` field generated for each method on the client struct emitted by `#[service]`. For `#[service] trait Calculator { ... }`, the `Calculator` client struct carries one `RemoteMethod<Req, Resp>` per method, and the four delivery functions sit directly on it. The difference between modes is in what guarantees they provide and how they handle failures.

## send: Fire-and-Forget

The simplest mode. Send the request unreliably with no reply registered:

```rust
// `heartbeat` is `iface.heartbeat`, a RemoteMethod<HeartbeatRequest, ()>
iface.heartbeat.send(HeartbeatRequest { node_id })?;
```

No `ReplyFuture` is created. No endpoint is registered for a response. If the connection is down, the message is silently dropped. If the server responds, the response is discarded.

Use this for heartbeats, notifications, and any message where losing one is harmless because the next one compensates. The `well_known_endpoint` example demonstrates this with three heartbeats following a burst of replies.

## try_get_reply: At-Most-Once

Send unreliably, then race the reply against a disconnect signal from the `FailureMonitor`:

```rust
let response = iface.balance
    .try_get_reply(GetBalanceRequest { account_id })
    .await?;
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
let response = iface.join.get_reply(JoinRequest { node_id }).await?;
```

The request sits in the peer's reliable queue. If the TCP connection drops, the peer reconnects (with backoff), and the queued request is resent. The server **may receive the same request multiple times**. Your server must be prepared for duplicates.

This mode never gives up on its own. If the remote process dies permanently, the `ReplyFuture` hangs until the 30-second RPC timeout fires, returning `ReplyError::Timeout`. When you need to bound waiting on the application side, do **not** wrap `get_reply` with `time.timeout`. Use `get_reply_unless_failed_for` instead. The `FailureMonitor` already tracks liveness for the entire transport, and bypassing it with a wall-clock timeout makes two independent components race over the same decision. The next section is the right tool.

## get_reply_unless_failed_for: At-Least-Once with Timeout

Like `get_reply`, but gives up if the endpoint has been continuously failed for a specified duration:

```rust
let response = iface.register
    .get_reply_unless_failed_for(
        RegisterRequest { /* ... */ },
        Duration::from_secs(10),
    )
    .await?;
```

This combines reliable delivery with a failure timeout. First it waits for a disconnect signal from the `FailureMonitor`, then sleeps for the sustained failure duration. If the connection recovers during that sleep, the reliable retransmit resolves the reply future first and the timeout is cancelled.

Use this for singleton RPCs (registration, recruitment) where you want reliable delivery but cannot wait forever if the destination is permanently gone. Because the timeout consults the `FailureMonitor`, the same liveness signal informs every RPC and every retry decision in the process. One source of truth, exactly as FDB does it.

## MaybeDelivered: Explicit Ambiguity

The `ReplyError::MaybeDelivered` variant is the most important design decision in the transport layer. Most RPC frameworks hide delivery ambiguity behind generic timeouts. Moonpool, following FDB, makes it **explicit**.

When you receive `MaybeDelivered`, you know exactly one thing: the connection failed while the request was in flight. The request may have been fully processed, partially processed, or never received. The framework refuses to guess.

```rust
match iface.balance.try_get_reply(req).await {
    Ok(response) => handle_success(response),
    Err(e) if e.is_maybe_delivered() => {
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

The generated client struct exposes each method as a `RemoteMethod` field with all four delivery modes available. You choose the mode at the call site, so the same client can use `get_reply` for critical writes and `try_get_reply` for best-effort reads. For raw `ReplyFuture` control (useful in `select!` blocks), use `send_request`.

For strategies on handling `MaybeDelivered` correctly, including idempotent design, generation numbers, and read-before-retry, see [Designing Simulation-Friendly RPC](./10-designing-rpc.md).
