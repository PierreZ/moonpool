# The Sim Book

<div style="text-align: center; margin: 2em 0;">
  <img src="images/logo.png" alt="Moonpool logo" style="max-width: 300px;" />
</div>

A guide to building and testing distributed systems with **moonpool**, a deterministic simulation framework for Rust.

Moonpool brings together ideas from FoundationDB, TigerBeetle, and Antithesis into a single library-level framework. Write your system once with provider traits, then run it against a simulated world that is deliberately worse than production. Same code, different wiring. No `#[cfg(test)]`. No mocks.

This book covers the philosophy, the architecture, and the practical details of building simulation-tested distributed systems.
