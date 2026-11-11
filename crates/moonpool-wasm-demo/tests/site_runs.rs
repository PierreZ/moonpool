//! The recorded runs of the website's pages (#416 (engine and player)).
//!
//! Each live page shows its recorded runs first, so the page is complete
//! before the wasm engine loads and when it does not load. Then the browser
//! runs the same configs and gets the same runs. These files are written by
//! this test, never by hand: the test writes a file and fails when what it
//! wrote differs from what was there, so CI fails on a stale file. Run the
//! test again locally to accept the new runs, and commit them.

use std::fs;
use std::path::PathBuf;

use moonpool_wasm_demo::{RunConfig, run};

/// The runs of each page, by file name under `website/site/src/runs/`.
fn pages() -> Vec<(&'static str, Vec<RunConfig>)> {
    let seed = |seed| RunConfig {
        seed,
        ..RunConfig::default()
    };
    vec![
        // The hero of the home page and the demo of the chapter
        // "Simulation in the Browser".
        ("home", vec![seed(256)]),
        // Determinism: the seed of the page, and the next seed to compare.
        ("determinism", vec![seed(256), seed(257)]),
    ]
}

fn page_json(configs: &[RunConfig]) -> String {
    let runs: Vec<String> = configs
        .iter()
        .map(|c| {
            let run = run(c).expect("the configs of the site are valid");
            serde_json::to_string(&run).expect("a run serializes to JSON")
        })
        .collect();
    // One run per line keeps a diff readable.
    format!("[\n{}\n]\n", runs.join(",\n"))
}

#[test]
fn page_runs_are_recorded() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../website/site/src/runs");
    let mut stale = Vec::new();
    for (name, configs) in pages() {
        let path = dir.join(format!("{name}.json"));
        let fresh = page_json(&configs);
        if fs::read_to_string(&path).unwrap_or_default() != fresh {
            fs::write(&path, &fresh).expect("write a recorded run of the site");
            stale.push(path.display().to_string());
        }
    }
    assert!(
        stale.is_empty(),
        "stale recorded runs, now written again (commit them): {stale:?}"
    );
}

#[test]
fn one_config_records_the_same_bytes() {
    // The page compares a live run with the recorded one by its fingerprint:
    // that only holds if one config always gives the same bytes.
    for (_, configs) in pages() {
        assert_eq!(page_json(&configs), page_json(&configs));
    }
}
