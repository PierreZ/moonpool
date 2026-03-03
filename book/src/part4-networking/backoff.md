# Backoff and Reconnection

<!-- toc -->

When a connection fails, the worst thing a peer can do is immediately retry. If ten peers all lose their connections at the same moment (say, after a network partition heals), and all of them retry instantly, they will overwhelm the destination with simultaneous connection attempts. This creates a **reconnection storm** that can be worse than the original failure.

## Exponential Backoff

Moonpool peers use exponential backoff on reconnection, following FoundationDB's pattern (`FlowTransport.actor.cpp:892-897`). The `ReconnectState` tracks the current delay and doubles it after each failure, up to a configured maximum:

```rust
let next_delay = std::cmp::min(
    state.reconnect_state.current_delay * 2,
    config.max_reconnect_delay,
);
state.reconnect_state.current_delay = next_delay;
```

The default `PeerConfig` starts with a 100ms initial delay and caps at 30 seconds:

```rust
PeerConfig {
    initial_reconnect_delay: Duration::from_millis(100),
    max_reconnect_delay: Duration::from_secs(30),
    max_queue_size: 1000,
    connection_timeout: Duration::from_secs(5),
    max_connection_failures: None, // Unlimited retries
    monitor: Some(MonitorConfig::default()),
}
```

On a successful connection, the backoff resets to the initial delay. The failure counter resets to zero. The peer is ready for the next disruption with a clean slate.

## Why It Matters in Simulation

Without backoff, simulation tests that inject network failures produce degenerate behavior. The event queue fills with connection attempts that all fail, each failure spawns another immediate retry, and the simulation spends all its time processing reconnection events instead of making progress on actual workload logic.

With backoff, the chaos engine can sever connections freely. Peers back off, the event queue stays manageable, and when connections restore, peers reconnect in a staggered pattern that avoids thundering herd effects.

The `assert_sometimes_each!` macro tracks backoff depth across simulation runs, ensuring we exercise multiple levels of the exponential curve:

```rust
assert_sometimes_each!(
    "backoff_depth",
    [("attempt", state.reconnect_state.failure_count)]
);
```

## Profile Presets

Different network environments need different backoff tuning. `PeerConfig` provides presets:

| Profile | Initial Delay | Max Delay | Timeout | Max Failures |
|---------|--------------|-----------|---------|--------------|
| Default | 100ms | 30s | 5s | Unlimited |
| Local | 10ms | 1s | 500ms | 10 |
| WAN | 500ms | 60s | 30s | Unlimited |

For simulation tests, the default profile works well. The chaos engine can buggify the actual delays through the `TimeProvider`, stretching or shortening them to explore timing-sensitive code paths.
