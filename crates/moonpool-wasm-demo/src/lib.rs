//! The run engine of the moonpool website: one moonpool simulation that runs
//! natively and as wasm in a browser tab, and returns what the run recorded.
//!
//! The scenario is a small replicated register ([`register`]): three server
//! `Process`es that keep one value on disk, and one client `Workload` that
//! writes to a quorum and reads it back. It is ordinary moonpool code against
//! the providers, with `tracing` events, an `Invariant`, `assert_sometimes!`,
//! `hint!` and `buggify!`. A planted bug, off by default, reads from one
//! server in place of a quorum.
//!
//! [`run`] runs one seed of a [`RunConfig`] and returns a [`Run`](engine::Run),
//! the stable model that the site's player draws. [`run_json`] is the same
//! call, JSON in and JSON out, and it is the whole browser surface. The same
//! config gives the same bytes natively and in the browser. The crate is not
//! published.
//!
//! # Quick example
//!
//! ```rust
//! use moonpool_wasm_demo::{RunConfig, run, run_json};
//!
//! let run = run(&RunConfig { seed: 42, ..RunConfig::default() }).expect("valid config");
//! assert!(run.report.passed);
//! assert!(run.messages.iter().all(|m| m.received_ms.is_none_or(|r| r >= m.sent_ms)));
//!
//! // The browser receives this JSON; the same config gives the same bytes.
//! assert_eq!(run_json(r#"{"seed": 42}"#), run_json(r#"{"seed": 42}"#));
//! ```
//!
//! # Features
//!
//! - **Observation on top, not inside.** The scenario emits ordinary
//!   `tracing` events. The recorder of [`engine`] is an `Invariant` that reads
//!   them through `TraceQuery`, with the injected faults under
//!   `SIM_FAULT_EVENT_NAME`. The scenario holds no drawing code.
//! - **wasm entry point.** On `wasm32`, `run(config)` is exported through
//!   `wasm-bindgen` and installs `console_error_panic_hook`.
//!
//! # Feature flags
//!
//! None. The crate depends on `moonpool-sim` with `default-features =
//! false`: no tokio providers and no fork-based exploration, which is the
//! configuration that builds for `wasm32-unknown-unknown`.

pub mod engine;
pub mod register;

pub use engine::{ChaosSet, RunConfig, run, run_json};

#[cfg(target_arch = "wasm32")]
use wasm_bindgen::prelude::wasm_bindgen;

/// JavaScript entry point exported as `run(config)` on wasm: a [`RunConfig`]
/// as JSON in, the [`engine::Run`] as JSON out.
#[cfg(target_arch = "wasm32")]
#[wasm_bindgen(js_name = run)]
#[must_use]
pub fn run_wasm(config: &str) -> String {
    console_error_panic_hook::set_once();
    run_json(config)
}
