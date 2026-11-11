//! The recorded runs of the website's home page (#415 (new site with the
//! book)).
//!
//! The home page shows these runs first, so it is complete before the wasm
//! bundle loads and when it does not load. Then the browser runs the same
//! seeds with `runSeed` and gets the same bytes. The file is written by this
//! test, never by hand: the test writes it and fails when what it wrote
//! differs from what was there, so CI fails on a stale file. Run the test
//! again locally to accept the new runs.

use std::fs;
use std::path::PathBuf;

use moonpool_wasm_demo::run_seed;

/// The seeds of the home page, in the order of its buttons.
const HOME_SEEDS: [u64; 5] = [42, 7, 123, 256, 1000];

fn home_runs_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../website/site/src/runs/home.json")
}

fn home_runs_json() -> String {
    let runs: Vec<_> = HOME_SEEDS.iter().map(|&seed| run_seed(seed)).collect();
    let mut json = serde_json::to_string_pretty(&runs).expect("run results serialize to JSON");
    json.push('\n');
    json
}

#[test]
fn home_page_runs_are_recorded() {
    let path = home_runs_path();
    let fresh = home_runs_json();
    let recorded = fs::read_to_string(&path).unwrap_or_default();
    if recorded != fresh {
        fs::write(&path, &fresh).expect("write website/site/src/runs/home.json");
        panic!(
            "{} was stale and is now written again: commit it",
            path.display()
        );
    }
}

#[test]
fn home_page_runs_replay_the_same_bytes() {
    // The page compares a live run with the recorded one by a fingerprint of
    // its JSON: that only holds if one seed always gives the same bytes.
    assert_eq!(home_runs_json(), home_runs_json());
}
