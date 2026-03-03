# Backoff and Reconnection

<!-- toc -->

- Exponential backoff: `PeerConfig` with `initial_backoff` (100ms) and `max_backoff` (5s)
- FDB's backoff pattern: `FlowTransport.actor.cpp:892-897`
- Why backoff matters in simulation: without it, reconnection storms overwhelm the event queue
- Buggify interaction: buggified connections may use shorter/longer backoff
