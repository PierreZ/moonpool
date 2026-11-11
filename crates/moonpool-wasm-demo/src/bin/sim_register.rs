//! The small system of the website as an example simulation:
//! `cargo xtask sim run register`.
//!
//! The replicated register (three servers, one client) with the site's
//! default chaos, the planted bug off, until the coverage is stable, and each
//! seed run twice to prove that it replays (`check_determinism`).

use moonpool_wasm_demo::engine::{RunConfig, simulation};

fn main() {
    moonpool_sim::init_sim_tracing(tracing::Level::WARN);
    let report = simulation(&RunConfig::default())
        .check_determinism()
        .run()
        .expect("simulation configuration is valid");
    report.eprint();
    std::process::exit(report.exit_code());
}
