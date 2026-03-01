use std::process::{self, Command};
use std::time::Instant;

/// A simulation binary with its name and the crates to instrument with sancov.
struct SimBinary {
    name: &'static str,
    sancov_crates: &'static str,
}

const SIM_BINARIES: &[SimBinary] = &[
    SimBinary {
        name: "sim-maze-explore",
        sancov_crates: "moonpool_sim_examples",
    },
    SimBinary {
        name: "sim-dungeon-explore",
        sancov_crates: "moonpool_sim_examples",
    },
    SimBinary {
        name: "sim-adaptive-explore",
        sancov_crates: "moonpool_explorer",
    },
    SimBinary {
        name: "sim-metastable-explore",
        sancov_crates: "moonpool,moonpool_transport",
    },
    SimBinary {
        name: "sim-banking-chaos",
        sancov_crates: "moonpool,moonpool_transport",
    },
    SimBinary {
        name: "sim-transport-e2e",
        sancov_crates: "moonpool_transport",
    },
    SimBinary {
        name: "sim-transport-messaging",
        sancov_crates: "moonpool_transport",
    },
];

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    match args.first().map(|s| s.as_str()) {
        Some("sim") => run_sim(&args[1..]),
        Some("help") | Some("--help") | Some("-h") | None => print_usage(),
        Some(cmd) => {
            eprintln!("unknown command: {cmd}");
            print_usage();
            process::exit(1);
        }
    }
}

fn print_usage() {
    eprintln!("Usage: cargo xtask <command>");
    eprintln!();
    eprintln!("Commands:");
    eprintln!("  sim [filter...]   Run simulation binaries (sancov always enabled)");
    eprintln!();
    eprintln!("Examples:");
    eprintln!("  cargo xtask sim              Run all simulation binaries");
    eprintln!("  cargo xtask sim maze         Run binaries matching 'maze'");
}

/// Format a duration as a human-readable string.
fn fmt_duration(d: std::time::Duration) -> String {
    let total_ms = d.as_millis();
    if total_ms < 1000 {
        format!("{}ms", total_ms)
    } else if total_ms < 60_000 {
        format!("{:.1}s", d.as_secs_f64())
    } else {
        let mins = d.as_secs() / 60;
        let secs = d.as_secs() % 60;
        format!("{}m {:02}s", mins, secs)
    }
}

fn run_sim(args: &[String]) {
    let filters: Vec<&str> = args.iter().map(|s| s.as_str()).collect();

    let binaries: Vec<&SimBinary> = if filters.is_empty() {
        SIM_BINARIES.iter().collect()
    } else {
        SIM_BINARIES
            .iter()
            .filter(|b| filters.iter().any(|f| b.name.contains(f)))
            .collect()
    };

    if binaries.is_empty() {
        eprintln!("No binaries match filters: {:?}", filters);
        process::exit(1);
    }

    eprintln!(
        "Running {} simulation binaries (sancov enabled)",
        binaries.len()
    );
    eprintln!();

    let total_start = Instant::now();
    let mut passed = Vec::new();
    let mut failed = Vec::new();

    for bin in &binaries {
        eprintln!("--- {} ---", bin.name);
        let bin_start = Instant::now();

        let mut cmd = Command::new("cargo");
        cmd.args(["run", "--bin", bin.name]);

        cmd.env("SANCOV_CRATES", bin.sancov_crates);
        // Use a separate target dir so cargo doesn't serve a cached
        // non-instrumented build (SANCOV_CRATES isn't in cargo's fingerprint).
        cmd.args(["--target-dir", "target/sancov"]);

        match cmd.status() {
            Ok(status) if status.success() => {
                eprintln!(
                    "--- {} --- ({})\n",
                    bin.name,
                    fmt_duration(bin_start.elapsed())
                );
                passed.push(bin.name);
            }
            Ok(status) => {
                let code = status.code().unwrap_or(-1);
                eprintln!(
                    "{}: exited with code {code} ({})\n",
                    bin.name,
                    fmt_duration(bin_start.elapsed())
                );
                failed.push(bin.name);
            }
            Err(e) => {
                eprintln!("{}: failed to launch: {e}\n", bin.name);
                failed.push(bin.name);
            }
        }
    }

    // Summary
    let total_elapsed = total_start.elapsed();
    eprintln!("=== Summary ===");
    eprintln!(
        "{} passed, {} failed, {} total ({})",
        passed.len(),
        failed.len(),
        binaries.len(),
        fmt_duration(total_elapsed),
    );
    if !failed.is_empty() {
        eprintln!("Failed:");
        for name in &failed {
            eprintln!("  {name}");
        }
        process::exit(1);
    }
}
