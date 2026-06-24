# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.8.0] - 2026-06-24

### 🚜 Refactor

- **transport-derive**: Simplify to_snake_case


## [0.7.0] - 2026-06-05

### 🚀 Features

- **core**: Make moonpool-core wasm-buildable and abstract TaskProvider JoinHandle
- Serializable interfaces with endpoint adjustment

### 🚜 Refactor

- **core,sim,transport,derive**: Post-migration cleanup of Send-bounded traits
- **core,transport,derive**: Migrate transport to Send-bounded traits
- **transport-derive**: Collapse extract_method_types match
- **transport-derive**: Drop dead has_ref tracking in service_impl
- **transport**: Apply rust-api-guidelines review
- Unify Server and Client into single interface type
- Move codec to transport, erase from user-facing API
- Erase codec and provider generics from user-facing types
- Bind transport at interface construction
- Dynamic endpoint tokens with well-known opt-in

### ⚙️ Miscellaneous Tasks

- **workspace**: Enable clippy pedantic and fix all warnings
- **transport-derive**: Strip obvious "what" comments


## [0.6.0] - 2026-03-28

### 🚀 Features

- **moonpool-transport**: Replace BoundClient with ServiceEndpoint

### 🐛 Bug Fixes

- **moonpool-transport-derive**: Use generic codec from context instead of hardcoded JsonCodec ([#62](https://github.com/PierreZ/moonpool/pull/62))

### 🚜 Refactor

- **moonpool-transport**: Embed codec in ServiceEndpoint
- **moonpool**: Remove virtual actor system entirely

### ⚙️ Miscellaneous Tasks

- Prepare release metadata and align book with codebase


## [0.5.0] - 2026-03-09

### 🚀 Features

- **sim**: Make spacesim RPC fault-aware with try_get_reply
- **moonpool**: Pass ActorContext to virtual actor trait methods, add NodeConfig and ClusterConfig builder

### 🚜 Refactor

- **moonpool**: Separate PlacementStrategy enum from PlacementDirector trait


## [0.4.0] - 2026-02-19

### 🚀 Features

- **moonpool-transport-derive**: Unified #[service] macro, #[actor_impl], and serve()
- **moonpool-transport-derive**: Add #[virtual_actor] macro: generates ActorRef, dispatcher, and method constants
- **transport-derive**: Reserve token index 0 for virtual actor dispatch

### 📚 Documentation

- Update all READMEs, CLAUDE.md, and rustdoc for 6-crate architecture


## [0.3.0] - 2026-01-25

### 🚀 Features

- **transport**: Add Orleans-style ergonomic bound client pattern
- **transport**: Add #[derive(Interface)] macro for FDB-style interfaces

### 🚜 Refactor

- **transport**: Consolidate provider type params into Providers bundle
- **transport**: Improve code quality with 3 fixes
- **transport**: Change #[derive(Interface)] to #[interface] attribute macro

