//! The exploration tree of the website's Multiverse page (#418 (technique
//! pages, part 2)).
//!
//! `moonpool-explorer` forks and maps shared memory, so it does not run in a
//! browser. This test runs it natively on the site's register with the
//! planted bug, keeps the tree of timelines, and writes it to
//! `website/site/src/runs/exploration.json` with the run of the timeline that
//! found the bug. Like `site_runs.rs`, it fails when the file was stale.

use std::fs;
use std::path::PathBuf;

use moonpool_sim::{ExplorationConfig, ExplorationReport, SimulationReport, format_timeline};
use moonpool_wasm_demo::engine::{RunConfig, run, simulation};
use serde_json::{Value, json};

/// The root seed of the page: its root timeline passes, and exploration
/// finds the planted bug in a forked timeline.
const SEED: u64 = 7;

fn config() -> RunConfig {
    RunConfig {
        seed: SEED,
        bug: true,
        ..RunConfig::default()
    }
}

/// In-process (no fork), so the tree is the same on each run.
fn exploration() -> ExplorationConfig {
    ExplorationConfig {
        workers: 0,
        max_runs_per_seed: 48,
        branching_factor: 3,
        max_frontier: 64,
        max_recipe_len: 6,
    }
}

fn explore(seed: u64) -> SimulationReport {
    simulation(&RunConfig { seed, ..config() })
        .set_iterations(1)
        .set_debug_seeds(vec![seed])
        .enable_exploration(exploration())
        .record_exploration_tree()
        .run()
        .expect("the site's config is valid")
}

fn tree_json(exp: &ExplorationReport) -> Value {
    let nodes: Vec<Value> = exp
        .timelines
        .iter()
        .map(|t| {
            let parent = exp.timelines.iter().position(|p| {
                t.recipe.len() == p.recipe.len() + 1 && t.recipe.starts_with(&p.recipe)
            });
            json!({
                "recipe": format_timeline(&t.recipe),
                "parent": parent,
                "fork_at": t.recipe.last().map(|(count, _)| count),
                "failed": t.failed,
                "discoveries": t.discoveries.iter().map(|d| json!({
                    "call_count": d.call_count,
                    "kind": d.kind,
                    "assertion": d.assertion,
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    json!({ "seed": SEED, "nodes": nodes })
}

#[test]
#[ignore = "a search for the page's seed: cargo test --test exploration_tree -- --ignored"]
fn find_a_seed() {
    for seed in 0..40 {
        let root = run(&RunConfig { seed, ..config() }).expect("valid");
        if !root.report.passed {
            continue;
        }
        let report = explore(seed);
        let exp = report.exploration.expect("exploration ran");
        println!(
            "seed {seed}: {} timelines, {} bugs",
            exp.total_timelines, exp.bugs_found
        );
    }
}

fn page_json() -> String {
    let report = explore(SEED);
    let exp = report.exploration.expect("exploration ran");
    let root = run(&config()).expect("valid config");
    assert!(root.report.passed, "the root timeline of the page passes");
    let bug = exp
        .timelines
        .iter()
        .find(|t| t.failed)
        .expect("exploration finds the planted bug");
    let found = run(&RunConfig {
        recipe: Some(format_timeline(&bug.recipe)),
        ..config()
    })
    .expect("valid config");
    assert!(
        !found.report.passed,
        "the bug's recipe replays to the failure"
    );
    let mut page = tree_json(&exp);
    page["config"] = serde_json::to_value(config()).expect("a config serializes");
    page["exploration"] = json!(exploration_fields());
    page["root"] = serde_json::to_value(&root).expect("a run serializes");
    page["bug"] = serde_json::to_value(&found).expect("a run serializes");
    // One line per part keeps a diff readable.
    let parts: Vec<String> = ["seed", "config", "exploration", "nodes", "root", "bug"]
        .iter()
        .map(|k| format!("{:?}: {}", k, page[*k]))
        .collect();
    format!("{{\n{}\n}}\n", parts.join(",\n"))
}

/// The exploration settings, for the page to name them.
fn exploration_fields() -> Value {
    let e = exploration();
    json!({
        "workers": e.workers,
        "max_runs_per_seed": e.max_runs_per_seed,
        "branching_factor": e.branching_factor,
        "max_recipe_len": e.max_recipe_len,
    })
}

#[test]
fn the_exploration_tree_is_recorded() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../website/site/src/runs/exploration.json");
    let fresh = page_json();
    assert_eq!(fresh, page_json(), "the in-process tree is deterministic");
    let stale = fs::read_to_string(&path).unwrap_or_default() != fresh;
    if stale {
        fs::write(&path, &fresh).expect("write the exploration tree of the site");
    }
    assert!(
        !stale,
        "stale exploration tree, now written again (commit it): {}",
        path.display()
    );
}
