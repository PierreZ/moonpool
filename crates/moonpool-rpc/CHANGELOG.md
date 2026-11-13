# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.9.0] - 2026-11-13

### 🚀 Features

- **site**: Technique pages, playground, extras and the finished site ([#425](https://github.com/PierreZ/moonpool/pull/425))
- **website**: The new Astro site in terminal green, with the book in it ([#422](https://github.com/PierreZ/moonpool/pull/422))
- **moonpool-rpc**: Run the RPC runtime on compio (local provider traits) ([#354](https://github.com/PierreZ/moonpool/pull/354))
- **moonpool-rpc**: Mutual TLS, a certificate authority, and TLS under simulation
- **moonpool-core**: SystemProvider for CPU and disk counters
- **moonpool-rpc**: Per-endpoint admission queues
- **moonpool-rpc**: Buggify points inside the runtime
- **moonpool-rpc**: Streams, balancing, security and qualification (P4–P7) ([#276](https://github.com/PierreZ/moonpool/pull/276))
- **moonpool-rpc**: Typed dynamic-endpoint RPC, delivery contracts and interfaces across restarts (P1–P3) ([#275](https://github.com/PierreZ/moonpool/pull/275))

### 🐛 Bug Fixes

- **moonpool-rpc**: Failure runs, watcher probes, orderly close, receive buffer bounds, TLS docs ([#528](https://github.com/PierreZ/moonpool/pull/528))
- **moonpool-rpc**: Keep replies of a replaced connection, and stop false damage verdicts in the journal ([#400](https://github.com/PierreZ/moonpool/pull/400))
- **moonpool-rpc**: TLS randomness from the simulation, so a bit flip replays ([#397](https://github.com/PierreZ/moonpool/pull/397))
- **moonpool-rpc**: Send our Hello before the reader can end the session

### 📚 Documentation

- Rewrite the README, the book and the crate docs for every feature

### 🧪 Testing

- **moonpool-rpc-sim**: Observe every rpc buggify site, flag repeated stream runs, fix doc drift ([#524](https://github.com/PierreZ/moonpool/pull/524))
- **moonpool-rpc**: Wait for the server to count a version rejection
- **moonpool-rpc**: Wait for the late loser before the anonymous call
- **moonpool-rpc**: Wait for the dead session before the stale restart call

### ⚙️ Miscellaneous Tasks

- Bump Rust to 1.99 and update the Nix flake

### 📦 Other

- **moonpool-rpc,moonpool-rpc-sim**: Drop duplicate tracing dev-dependencies

