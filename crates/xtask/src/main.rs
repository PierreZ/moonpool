use std::path::Path;
use std::process::{Command, ExitCode};
use std::time::Instant;

/// A simulation binary with its name and the crates to instrument with sancov.
#[derive(Clone, Copy)]
struct SimBinary {
    name: &'static str,
    package: &'static str,
    sancov_crates: &'static str,
}

impl SimBinary {
    const fn new(name: &'static str, package: &'static str, sancov_crates: &'static str) -> Self {
        Self {
            name,
            package,
            sancov_crates,
        }
    }

    /// Display name without the `sim-` prefix.
    fn display_name(&self) -> &str {
        self.name.strip_prefix("sim-").unwrap_or(self.name)
    }
}

const SIM_EXAMPLES_CRATE: &str = "moonpool_sim_examples";
const SIM_EXAMPLES_PACKAGE: &str = "moonpool-sim-examples";
const WORKSPACE_ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");

const SIM_BINARIES: &[SimBinary] = &[
    SimBinary::new("sim-maze-explore", SIM_EXAMPLES_PACKAGE, SIM_EXAMPLES_CRATE),
    SimBinary::new(
        "sim-dungeon-explore",
        SIM_EXAMPLES_PACKAGE,
        SIM_EXAMPLES_CRATE,
    ),
    SimBinary::new(
        "sim-frontier-explore",
        "moonpool-explorer",
        "moonpool_explorer",
    ),
    SimBinary::new("sim-axum-web", SIM_EXAMPLES_PACKAGE, SIM_EXAMPLES_CRATE),
    SimBinary::new(
        "sim-metrics-service",
        SIM_EXAMPLES_PACKAGE,
        SIM_EXAMPLES_CRATE,
    ),
    SimBinary::new("sim-topology", SIM_EXAMPLES_PACKAGE, SIM_EXAMPLES_CRATE),
    SimBinary::new("sim-tonic-grpc", SIM_EXAMPLES_PACKAGE, SIM_EXAMPLES_CRATE),
];

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if dispatch(&args) {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn dispatch(args: &[String]) -> bool {
    match args.first().map(String::as_str) {
        Some("sim") => sim_dispatch(&args[1..]),
        Some("help" | "--help" | "-h") | None => {
            print_usage();
            true
        }
        Some(cmd) => {
            eprintln!("unknown command: {cmd}");
            print_usage();
            false
        }
    }
}

fn print_usage() {
    eprintln!("Usage: cargo xtask <command>");
    eprintln!();
    eprintln!("Commands:");
    eprintln!("  sim   Simulation binary management");
    eprintln!();
    eprintln!("Run 'cargo xtask sim --help' for simulation subcommands.");
}

fn sim_dispatch(args: &[String]) -> bool {
    match args.first().map(String::as_str) {
        Some("list") => sim_list(&args[1..]),
        Some("run") => sim_run(&args[1..]),
        Some("run-all") => sim_run_all(),
        Some("help" | "--help" | "-h") | None => {
            sim_help();
            true
        }
        Some(cmd) => {
            eprintln!("unknown sim subcommand: {cmd}");
            sim_help();
            false
        }
    }
}

fn sim_help() {
    eprintln!("Usage: cargo xtask sim <subcommand>");
    eprintln!();
    eprintln!("Subcommands:");
    eprintln!("  list [filter...]     List simulation binaries");
    eprintln!("  run <filter...>      Run binaries matching filter(s)");
    eprintln!("  run-all              Run all simulation binaries");
    eprintln!();
    eprintln!("Examples:");
    eprintln!("  cargo xtask sim list");
    eprintln!("  cargo xtask sim list maze");
    eprintln!("  cargo xtask sim run maze");
    eprintln!("  cargo xtask sim run-all");
}

/// Format a duration as a human-readable string.
fn fmt_duration(d: std::time::Duration) -> String {
    let total_ms = d.as_millis();
    if total_ms < 1000 {
        format!("{total_ms}ms")
    } else if total_ms < 60_000 {
        format!("{:.1}s", d.as_secs_f64())
    } else {
        let mins = d.as_secs() / 60;
        let secs = d.as_secs() % 60;
        format!("{mins}m {secs:02}s")
    }
}

fn filter_binaries(filters: &[String]) -> Vec<SimBinary> {
    if filters.is_empty() {
        SIM_BINARIES.to_vec()
    } else {
        SIM_BINARIES
            .iter()
            .filter(|b| filters.iter().any(|f| b.name.contains(f.as_str())))
            .copied()
            .collect()
    }
}

fn sim_list(args: &[String]) -> bool {
    let binaries = filter_binaries(args);

    if binaries.is_empty() {
        eprintln!("No binaries match filters: {args:?}");
        return false;
    }

    for bin in &binaries {
        println!("{}", bin.display_name());
    }
    true
}

fn split_run_args(args: &[String]) -> (&[String], &[String]) {
    args.iter()
        .position(|argument| argument == "--")
        .map_or((args, &[]), |separator| {
            (&args[..separator], &args[separator + 1..])
        })
}

fn sim_run(args: &[String]) -> bool {
    let (filter_args, binary_args) = split_run_args(args);

    if filter_args.is_empty() {
        eprintln!("error: 'run' requires at least one filter argument");
        eprintln!();
        eprintln!("Usage: cargo xtask sim run <filter...> [-- <binary-args...>]");
        eprintln!("       cargo xtask sim run-all    (to run all binaries)");
        return false;
    }

    let binaries = filter_binaries(filter_args);

    if binaries.is_empty() {
        eprintln!("No binaries match filters: {filter_args:?}");
        return false;
    }

    run_binaries(&binaries, binary_args)
}

fn sim_run_all() -> bool {
    run_binaries(SIM_BINARIES, &[])
}

fn simulation_command(binary: &SimBinary, extra_args: &[String]) -> Command {
    let mut command = Command::new("cargo");
    command
        .current_dir(Path::new(WORKSPACE_ROOT))
        .args([
            "run",
            "--package",
            binary.package,
            "--bin",
            binary.name,
            "--target-dir",
            "target/sancov",
        ])
        .env("SANCOV_CRATES", binary.sancov_crates);
    if !extra_args.is_empty() {
        command.arg("--").args(extra_args);
    }
    command
}

fn run_binaries(binaries: &[SimBinary], extra_args: &[String]) -> bool {
    eprintln!(
        "Running {} simulation binaries (sancov enabled)",
        binaries.len()
    );
    eprintln!();

    let total_start = Instant::now();
    let mut passed = 0;
    let mut failed = Vec::new();

    for bin in binaries {
        let name = bin.display_name();
        eprintln!("--- {name} ---");
        let bin_start = Instant::now();

        // Use a separate target dir so cargo does not serve a cached
        // non-instrumented build (`SANCOV_CRATES` is not in its fingerprint).
        let mut cmd = simulation_command(bin, extra_args);

        match cmd.status() {
            Ok(status) if status.success() => {
                eprintln!("--- {name} --- ({})\n", fmt_duration(bin_start.elapsed()));
                passed += 1;
            }
            Ok(status) => {
                let code = status.code().unwrap_or(-1);
                eprintln!(
                    "{name}: exited with code {code} ({})\n",
                    fmt_duration(bin_start.elapsed())
                );
                failed.push(name);
            }
            Err(e) => {
                eprintln!("{name}: failed to launch: {e}\n");
                failed.push(name);
            }
        }
    }

    // Summary
    let total_elapsed = total_start.elapsed();
    eprintln!("=== Summary ===");
    eprintln!(
        "{} passed, {} failed, {} total ({})",
        passed,
        failed.len(),
        binaries.len(),
        fmt_duration(total_elapsed),
    );
    if !failed.is_empty() {
        eprintln!("Failed:");
        for name in &failed {
            eprintln!("  {name}");
        }
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{
        SIM_BINARIES, WORKSPACE_ROOT, filter_binaries, fmt_duration, simulation_command,
        split_run_args,
    };

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn filters_match_names_and_accept_multiple_terms() {
        let matches = filter_binaries(&strings(&["maze", "tonic"]));
        let names: Vec<_> = matches.iter().map(super::SimBinary::display_name).collect();
        assert_eq!(names, ["maze-explore", "tonic-grpc"]);
        assert!(filter_binaries(&strings(&["missing"])).is_empty());
    }

    #[test]
    fn run_separator_keeps_binary_arguments_out_of_filters() {
        let args = strings(&["maze", "--", "--seed", "42"]);
        let (filters, binary_args) = split_run_args(&args);
        assert_eq!(filters, strings(&["maze"]));
        assert_eq!(binary_args, strings(&["--seed", "42"]));
    }

    #[test]
    fn durations_use_compact_units() {
        assert_eq!(fmt_duration(Duration::from_millis(999)), "999ms");
        assert_eq!(fmt_duration(Duration::from_millis(1_250)), "1.2s");
        assert_eq!(fmt_duration(Duration::from_secs(125)), "2m 05s");
    }

    #[test]
    fn child_cargo_is_anchored_to_the_workspace_and_package() {
        let command = simulation_command(&SIM_BINARIES[0], &strings(&["--seed", "42"]));
        assert_eq!(
            command.get_current_dir(),
            Some(std::path::Path::new(WORKSPACE_ROOT))
        );
        let args: Vec<_> = command
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            strings(&[
                "run",
                "--package",
                "moonpool-sim-examples",
                "--bin",
                "sim-maze-explore",
                "--target-dir",
                "target/sancov",
                "--",
                "--seed",
                "42",
            ])
        );
    }
}
