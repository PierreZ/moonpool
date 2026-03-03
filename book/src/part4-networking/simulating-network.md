# Simulating the Network

<!-- toc -->

- Philosophy: TCP-level simulation, not packet-level — connection faults matter more
- What's simulated: connection establishment, latency, disconnection, half-open states, backoff
- What's NOT simulated: individual packet routing, MTU, congestion windows
- The same code runs over real TCP (production) or simulated connections (simulation)
- FDB's influence: the network simulation mirrors FlowTransport's connection model
