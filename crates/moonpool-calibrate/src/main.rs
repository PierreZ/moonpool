//! `moonpool-calibrate` — measure the real host, print moonpool configuration.
//!
//! stdout carries generated Rust and nothing else; every diagnostic goes to
//! stderr, so `moonpool-calibrate storage > measured_storage.rs` produces a
//! compilable file.

use std::net::TcpListener;
use std::path::Path;
use std::process::ExitCode;

use clap::Parser;
use moonpool_calibrate::cli::{Cli, Command, NetworkCommand};
use moonpool_calibrate::codegen::{Constant, GeneratedFile};
use moonpool_calibrate::network;
use moonpool_calibrate::stats::Summary;
use moonpool_calibrate::storage::{self, BLOCK_SIZE};

fn main() -> ExitCode {
    // `try_parse` rather than `parse`: clap writes help and version to *stdout*,
    // which this binary reserves for generated Rust. Rendering it here keeps
    // `moonpool-calibrate storage > measured_storage.rs` uncorruptible.
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => {
            eprint!("{}", error.render());
            // Help and version report success; a genuine parse error does not.
            return if error.exit_code() == 0 {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            };
        }
    };

    match run(cli.command) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("moonpool-calibrate: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(command: Command) -> std::io::Result<()> {
    match command {
        Command::Storage {
            file,
            samples,
            warmup,
        } => run_storage(&file, samples, warmup),
        Command::Network {
            command: NetworkCommand::Listen { port },
        } => run_listen(port),
        Command::Network {
            command:
                NetworkCommand::Measure {
                    address,
                    samples,
                    warmup,
                },
        } => run_network_measure(&address, samples, warmup),
    }
}

fn run_storage(file: &Path, samples: u64, warmup: u64) -> std::io::Result<()> {
    eprintln!("moonpool-calibrate: storage");
    eprintln!("  scratch file : {}", file.display());
    eprintln!("  block size   : {BLOCK_SIZE} bytes");
    eprintln!("  warmup       : {warmup} per operation");
    eprintln!("  samples      : {samples} per operation");
    eprintln!("  measuring through raw std::fs (moonpool is bypassed)...");

    let measured = storage::measure(file, samples, warmup)?;
    let read = measured.read.summary();
    let write = measured.write.summary();
    let sync = measured.sync.summary();

    report(&[read, write, sync]);

    let generated = GeneratedFile::new("moonpool-calibrate storage")
        .note(format!(
            "Samples: {samples} per operation, after {warmup} warmup iterations."
        ))
        .note(format!("Block size: {BLOCK_SIZE} bytes."))
        .note("Path: std::fs::File read / write / sync_all, timed with Instant.".to_owned())
        .constant(Constant::from_summary(
            "STORAGE_READ_LATENCY",
            &format!("Measured latency of a {BLOCK_SIZE}-byte read."),
            read,
        ))
        .constant(Constant::from_summary(
            "STORAGE_WRITE_LATENCY",
            &format!("Measured latency of a {BLOCK_SIZE}-byte write."),
            write,
        ))
        .constant(Constant::from_summary(
            "STORAGE_SYNC_LATENCY",
            "Measured latency of `sync_all` with one dirty block outstanding.",
            sync,
        ))
        .render();

    print!("{generated}");
    Ok(())
}

fn run_listen(port: u16) -> std::io::Result<()> {
    let listener = TcpListener::bind(("0.0.0.0", port))?;
    eprintln!("moonpool-calibrate: network listen");
    eprintln!("  listening on {}", listener.local_addr()?);
    eprintln!(
        "  echoing {}-byte messages; ctrl-c to stop",
        network::MESSAGE_LEN
    );
    network::serve(&listener)
}

fn run_network_measure(address: &str, samples: u64, warmup: u64) -> std::io::Result<()> {
    eprintln!("moonpool-calibrate: network measure");
    eprintln!("  peer         : {address}");
    eprintln!("  message      : {} bytes", network::MESSAGE_LEN);
    eprintln!("  warmup       : {warmup}");
    eprintln!("  samples      : {samples}");
    eprintln!("  measuring through raw std::net (moonpool is bypassed)...");

    let latencies = network::measure(address, samples, warmup)?;
    let rtt = latencies.summary();
    report(&[rtt]);

    let one_way = rtt.bounds().divided_by(2);
    eprintln!(
        "  one-way (rtt / 2): {:?} .. {:?}",
        one_way.start, one_way.end
    );

    let generated = GeneratedFile::new(format!("moonpool-calibrate network measure {address}"))
        .note(format!(
            "Samples: {samples}, after {warmup} warmup round trips."
        ))
        .note(format!(
            "Path: one std::net::TcpStream, {}-byte ping/pong, TCP_NODELAY on, timed with Instant.",
            network::MESSAGE_LEN
        ))
        .constant(Constant::from_summary(
            "NETWORK_RTT_LATENCY",
            "Measured small-message TCP round-trip time.",
            rtt,
        ))
        .constant(Constant::from_bounds(
            "NETWORK_LATENCY",
            "One-way delay (round trip halved), for moonpool's one-way link knobs.",
            one_way,
        ))
        .render();

    print!("{generated}");
    Ok(())
}

/// Print the percentile table on stderr.
fn report(summaries: &[Summary]) {
    eprintln!();
    eprintln!(
        "  {:<9}{:>12}{:>12}{:>12}{:>12}{:>12}{:>9}",
        "operation", "p01", "p50", "p95", "p99", "max", "samples"
    );
    for summary in summaries {
        eprintln!(
            "  {:<9}{:>12}{:>12}{:>12}{:>12}{:>12}{:>9}",
            summary.name,
            format!("{:?}", summary.p01),
            format!("{:?}", summary.p50),
            format!("{:?}", summary.p95),
            format!("{:?}", summary.p99),
            format!("{:?}", summary.max),
            summary.count,
        );
    }
    eprintln!();
    eprintln!("  generated bounds use p01 .. p99; writing Rust to stdout.");
}
