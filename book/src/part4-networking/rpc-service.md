# RPC with #[service]

<!-- toc -->

- The `#[service]` proc macro: define a trait, get server + client for free
- Two modes auto-detected from method receivers:
  - `&self` methods → RPC mode (stateless request/response)
  - `&mut self` methods → actor mode (stateful, routed by identity)
- The id attribute: `#[service(id = 0xCA1C_0000)]` — unique identifier for endpoint routing
