//! The run engine and the small system of the site (#416 (engine and player)).

use moonpool_wasm_demo::engine::{ChaosSet, RunConfig, parse_recipe, run, run_json};

fn config(seed: u64, bug: bool) -> RunConfig {
    RunConfig {
        seed,
        bug,
        ..RunConfig::default()
    }
}

#[test]
fn one_seed_gives_the_same_run() {
    let a = run_json(r#"{"seed": 42}"#);
    assert_eq!(a, run_json(r#"{"seed": 42}"#));
    assert!(!a.contains("\"error\""), "{a}");
}

#[test]
fn a_run_records_messages_events_and_a_fingerprint() {
    let run = run(&config(7, false)).expect("valid config");
    assert_eq!(run.processes.len(), 4);
    assert!(run.events.iter().any(|e| e.name == "write_committed"));
    assert!(
        run.messages
            .iter()
            .any(|m| m.kind == "write" && m.received_ms.is_some())
    );
    assert!(
        run.messages
            .iter()
            .all(|m| m.received_ms.is_none_or(|r| r >= m.sent_ms))
    );
    assert!(run.events.windows(2).all(|w| w[0].seq < w[1].seq));
    assert_eq!(run.fingerprint.events.len(), 16);
    assert!(run.fingerprint.draws > 0);
    assert!(run.report.passed, "{:?}", run.report.violations);
}

#[test]
fn a_bad_config_is_an_error() {
    assert!(run_json(r#"{"scenario": "nope"}"#).contains("unknown scenario"));
    assert!(run_json(r#"{"servers": 9}"#).contains("1 to 5"));
    assert!(run_json(r#"{"colour": 1}"#).contains("bad config"));
}

#[test]
fn without_chaos_no_fault_is_injected() {
    let calm = RunConfig {
        chaos: ChaosSet {
            network: false,
            storage: false,
            reboots: false,
        },
        ..config(3, false)
    };
    let run = run(&calm).expect("valid config");
    assert!(run.faults.is_empty());
    // Each request arrives: only `buggify!` (a lost answer) drops a message.
    assert!(
        run.messages
            .iter()
            .filter(|m| m.id % 2 == 0)
            .all(|m| m.received_ms.is_some())
    );
}

/// The rates of the pull request: `cargo test -p moonpool-wasm-demo --test
/// engine -- --ignored --nocapture rates`.
#[test]
#[ignore = "measures the failure rate over many seeds"]
fn rates() {
    let n: u64 = std::env::var("SEEDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(200);
    for bug in [true, false] {
        let failed = (1..=n)
            .filter(|&s| !run(&config(s, bug)).expect("valid").report.passed)
            .count();
        println!("bug={bug}: {failed} of {n} seeds fail");
    }
}

#[test]
fn the_fault_tables_name_every_family_of_the_simulator() {
    use moonpool_sim::{NetworkFault, StorageFault};
    use moonpool_wasm_demo::engine::{NETWORK_FAULTS, STORAGE_FAULTS};
    let net: Vec<&str> = NETWORK_FAULTS.iter().map(|(n, _)| *n).collect();
    let sim: Vec<&str> = NetworkFault::ALL.iter().map(|f| f.name()).collect();
    assert_eq!(net, sim);
    assert!(NETWORK_FAULTS.iter().all(|(n, f)| f.name() == *n));
    let sto: Vec<&str> = STORAGE_FAULTS.iter().map(|(n, _)| *n).collect();
    let sim: Vec<&str> = StorageFault::ALL.iter().map(|f| f.name()).collect();
    assert_eq!(sto, sim);
    assert!(STORAGE_FAULTS.iter().all(|(n, f)| f.name() == *n));
}

#[test]
fn a_swarm_seed_names_its_families_and_keeps_them_out_of_the_events() {
    let config = RunConfig {
        seed: 3,
        swarm: true,
        chaos: ChaosSet {
            network: true,
            storage: true,
            reboots: true,
        },
        ..RunConfig::default()
    };
    let swarm = run(&config).expect("valid config");
    let families = swarm.families.expect("a run with chaos names its families");
    assert!(families.network.len() < 9 || families.storage.len() < 12);
    assert!(
        swarm.faults.iter().all(|f| f.kind != "chaos_families"),
        "the families are not a fault of the run"
    );
    let off = run(&RunConfig {
        chaos: ChaosSet {
            network: false,
            storage: false,
            reboots: true,
        },
        ..RunConfig::default()
    })
    .expect("valid config");
    assert!(off.families.is_none());
}

#[test]
fn the_checks_list_every_coverage_assertion_of_the_register() {
    let code = include_str!("../src/register.rs");
    let mut found = Vec::new();
    for (i, _) in code
        .match_indices("assert_sometimes!(")
        .chain(code.match_indices("assert_reachable!("))
    {
        let rest = &code[i..];
        let end = rest.find(");").expect("a macro call ends");
        let call = &rest[..end];
        let msg = call.rsplit('"').nth(1).expect("a literal message");
        found.push((i, msg.to_string()));
    }
    found.sort();
    let found: Vec<String> = found.into_iter().map(|(_, m)| m).collect();
    assert_eq!(found, moonpool_wasm_demo::register::CHECKS);
}

#[test]
fn coverage_stops_when_stable_and_names_a_check_no_seed_reaches() {
    use moonpool_wasm_demo::coverage::{CoverageConfig, coverage};
    use moonpool_wasm_demo::engine::NETWORK_FAULTS;
    // Every network family but bit_flip: no frame is ever damaged.
    let no_flip: Vec<String> = NETWORK_FAULTS
        .iter()
        .map(|(n, _)| (*n).to_string())
        .filter(|n| n != "bit_flip")
        .collect();
    let off = coverage(&CoverageConfig {
        run: RunConfig {
            seed: 1,
            network_faults: Some(no_flip),
            ..RunConfig::default()
        },
        plateau: 10,
        max_seeds: 40,
    })
    .expect("valid config");
    assert!(!off.stable);
    assert_eq!(off.seeds.len(), 40, "it runs to the cap");
    assert_eq!(off.missing, ["a server drops a damaged frame"]);
}

#[test]
fn a_recipe_reads_as_moonpool_writes_it() {
    let recipe = vec![(10, 17_215_453_406_249_577_386), (3, 1)];
    let text = moonpool_sim::format_timeline(&recipe);
    assert_eq!(
        parse_recipe(&text).expect("a recipe moonpool wrote"),
        recipe
    );
    assert_eq!(parse_recipe("").expect("the root"), Vec::new());
    assert!(parse_recipe("10:42").is_err());
    assert!(parse_recipe("10@-1").is_err());
    let refused = run_json(r#"{"seed": 7, "recipe": "ten@1"}"#);
    assert!(refused.contains("\"error\""), "{refused}");
}
