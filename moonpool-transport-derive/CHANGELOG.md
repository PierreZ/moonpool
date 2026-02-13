# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.4.0] - 2026-02-13

### 🚀 Features

- **moonpool-transport-derive**: Add #[virtual_actor] macro: generates ActorRef, dispatcher, and method constants
- **transport-derive**: Reserve token index 0 for virtual actor dispatch


## [0.3.0] - 2026-01-25

### 🚀 Features

- **transport**: Add Orleans-style ergonomic bound client pattern
- **transport**: Add #[derive(Interface)] macro for FDB-style interfaces

### 🚜 Refactor

- **transport**: Consolidate provider type params into Providers bundle
- **transport**: Improve code quality with 3 fixes
- **transport**: Change #[derive(Interface)] to #[interface] attribute macro

