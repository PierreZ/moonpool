# Network Faults

<!-- toc -->

- TCP-level simulation, not packet-level: connection faults matter more than individual packet faults
- Latency injection: configurable range per connection
- Packet loss and reordering
- Connection drops: random disconnection with configurable probability
- Half-open connections: one side thinks the connection is alive, the other doesn't
- Graceful close vs abort: FIN delivery semantics, send_closed vs recv_closed
- Config: `NetworkConfig` with knobs for all fault dimensions
