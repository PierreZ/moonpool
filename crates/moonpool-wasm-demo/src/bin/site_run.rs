//! Run configs of the site's engine natively and print each run as JSON, one
//! line per config: `cargo run -p moonpool-wasm-demo --bin site-run -- '{"seed":7}'`.
//!
//! `scripts/check-site-wasm.sh` compares these lines with the same configs
//! run on wasm32 under node.

fn main() {
    for config in std::env::args().skip(1) {
        println!("{}", moonpool_wasm_demo::run_json(&config));
    }
}
