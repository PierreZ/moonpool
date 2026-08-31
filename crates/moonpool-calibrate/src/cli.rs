//! Argument parsing.
//!
//! Hand-rolled, matching the workspace's existing CLI convention (`xtask` parses
//! its own arguments and takes no dependency to do it).

use std::fmt;
use std::path::PathBuf;

use crate::network::DEFAULT_PORT;

/// Recorded samples per operation when `--samples` is not given.
pub const DEFAULT_SAMPLES: u64 = 1_000;

/// Unrecorded warmup iterations per operation when `--warmup` is not given.
pub const DEFAULT_WARMUP: u64 = 100;

/// What the binary was asked to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Print usage on stderr and exit successfully.
    Help,
    /// Measure storage latency and generate Rust on stdout.
    Storage {
        /// Scratch file to measure against.
        file: PathBuf,
        /// Recorded samples per operation.
        samples: u64,
        /// Unrecorded warmup iterations per operation.
        warmup: u64,
    },
    /// Run the ping/pong responder the measuring side connects to.
    NetworkListen {
        /// TCP port to bind on all interfaces.
        port: u16,
    },
    /// Measure round-trip time against a listener and generate Rust on stdout.
    NetworkMeasure {
        /// `host:port` of the listener.
        address: String,
        /// Recorded samples.
        samples: u64,
        /// Unrecorded warmup round trips.
        warmup: u64,
    },
}

/// Why a command line could not be understood.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliError {
    /// The first argument is not a known command.
    UnknownCommand(String),
    /// `network` was given without (or with an unknown) subcommand.
    UnknownNetworkSubcommand(String),
    /// A flag that this command does not accept.
    UnknownFlag(String),
    /// A flag that needs a value was the last argument.
    MissingValue(String),
    /// A flag's value could not be parsed.
    InvalidValue {
        /// The flag whose value was rejected.
        flag: String,
        /// The offending value.
        value: String,
    },
    /// `--samples` or `--warmup` was zero where at least one is required.
    ZeroSamples,
    /// `network measure` was given no `host:port`.
    MissingAddress,
    /// A positional argument appeared where none is accepted.
    UnexpectedArgument(String),
}

impl fmt::Display for CliError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownCommand(command) => write!(formatter, "unknown command: {command}"),
            Self::UnknownNetworkSubcommand(subcommand) => {
                write!(formatter, "unknown network subcommand: {subcommand}")
            }
            Self::UnknownFlag(flag) => write!(formatter, "unknown flag: {flag}"),
            Self::MissingValue(flag) => write!(formatter, "{flag} requires a value"),
            Self::InvalidValue { flag, value } => {
                write!(formatter, "invalid value for {flag}: {value}")
            }
            Self::ZeroSamples => write!(formatter, "--samples must be at least 1"),
            Self::MissingAddress => {
                write!(formatter, "network measure requires a <host:port> address")
            }
            Self::UnexpectedArgument(argument) => {
                write!(formatter, "unexpected argument: {argument}")
            }
        }
    }
}

impl std::error::Error for CliError {}

/// Parse the arguments following the binary name.
///
/// # Errors
///
/// Returns a [`CliError`] describing the first problem found.
pub fn parse(args: &[String]) -> Result<Command, CliError> {
    match args.first().map(String::as_str) {
        None | Some("help" | "--help" | "-h") => Ok(Command::Help),
        Some("storage") => parse_storage(&args[1..]),
        Some("network") => parse_network(&args[1..]),
        Some(other) => Err(CliError::UnknownCommand(other.to_owned())),
    }
}

fn parse_storage(args: &[String]) -> Result<Command, CliError> {
    let mut file = crate::storage::default_file();
    let mut samples = DEFAULT_SAMPLES;
    let mut warmup = DEFAULT_WARMUP;

    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--file" => file = PathBuf::from(value(args, &mut index)?),
            "--samples" => samples = number(args, &mut index)?,
            "--warmup" => warmup = number(args, &mut index)?,
            flag if flag.starts_with('-') => return Err(CliError::UnknownFlag(flag.to_owned())),
            other => return Err(CliError::UnexpectedArgument(other.to_owned())),
        }
        index += 1;
    }

    if samples == 0 {
        return Err(CliError::ZeroSamples);
    }
    Ok(Command::Storage {
        file,
        samples,
        warmup,
    })
}

fn parse_network(args: &[String]) -> Result<Command, CliError> {
    match args.first().map(String::as_str) {
        Some("listen") => parse_network_listen(&args[1..]),
        Some("measure") => parse_network_measure(&args[1..]),
        Some(other) => Err(CliError::UnknownNetworkSubcommand(other.to_owned())),
        None => Err(CliError::UnknownNetworkSubcommand(String::new())),
    }
}

fn parse_network_listen(args: &[String]) -> Result<Command, CliError> {
    let mut port = DEFAULT_PORT;

    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--port" => port = number(args, &mut index)?,
            flag if flag.starts_with('-') => return Err(CliError::UnknownFlag(flag.to_owned())),
            other => return Err(CliError::UnexpectedArgument(other.to_owned())),
        }
        index += 1;
    }

    Ok(Command::NetworkListen { port })
}

fn parse_network_measure(args: &[String]) -> Result<Command, CliError> {
    let mut address = None;
    let mut samples = DEFAULT_SAMPLES;
    let mut warmup = DEFAULT_WARMUP;

    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--samples" => samples = number(args, &mut index)?,
            "--warmup" => warmup = number(args, &mut index)?,
            flag if flag.starts_with('-') => return Err(CliError::UnknownFlag(flag.to_owned())),
            other if address.is_none() => address = Some(other.to_owned()),
            other => return Err(CliError::UnexpectedArgument(other.to_owned())),
        }
        index += 1;
    }

    if samples == 0 {
        return Err(CliError::ZeroSamples);
    }
    let address = address.ok_or(CliError::MissingAddress)?;
    Ok(Command::NetworkMeasure {
        address,
        samples,
        warmup,
    })
}

/// Consume the value that follows the flag at `index`, advancing past it.
fn value(args: &[String], index: &mut usize) -> Result<String, CliError> {
    let flag = args[*index].clone();
    *index += 1;
    args.get(*index)
        .cloned()
        .ok_or(CliError::MissingValue(flag))
}

/// Consume and parse a numeric flag value.
fn number<T: std::str::FromStr>(args: &[String], index: &mut usize) -> Result<T, CliError> {
    let flag = args[*index].clone();
    let raw = value(args, index)?;
    raw.parse().map_err(|_| CliError::InvalidValue {
        flag,
        value: raw.clone(),
    })
}

/// Usage text, written to stderr so stdout stays a clean Rust stream.
#[must_use]
pub fn usage() -> &'static str {
    "\
Usage: moonpool-calibrate <command>

Measure the real host with raw std I/O and emit moonpool LatencyDistribution
constants on stdout. Diagnostics go to stderr, so stdout can be redirected
straight into a source file.

Commands:
  storage [--file PATH] [--samples N] [--warmup N]
      Measure read, write and sync latency against a scratch file.

  network listen [--port P]
      Run the ping/pong responder the measuring side connects to.

  network measure <host:port> [--samples N] [--warmup N]
      Measure small-message TCP round-trip time against a listener.

Examples:
  moonpool-calibrate storage > measured_storage.rs
  moonpool-calibrate network listen
  moonpool-calibrate network measure host-b:7777 > measured_network.rs
"
}

#[cfg(test)]
mod tests {
    use super::{CliError, Command, DEFAULT_SAMPLES, DEFAULT_WARMUP, parse, usage};
    use std::path::PathBuf;

    fn args(raw: &[&str]) -> Vec<String> {
        raw.iter().map(|item| (*item).to_owned()).collect()
    }

    #[test]
    fn no_arguments_prints_help() {
        assert_eq!(parse(&[]).expect("help"), Command::Help);
        assert_eq!(parse(&args(&["--help"])).expect("help"), Command::Help);
        assert_eq!(parse(&args(&["-h"])).expect("help"), Command::Help);
        assert!(usage().contains("moonpool-calibrate storage"));
    }

    #[test]
    fn storage_defaults_are_applied() {
        let command = parse(&args(&["storage"])).expect("storage");
        assert_eq!(
            command,
            Command::Storage {
                file: crate::storage::default_file(),
                samples: DEFAULT_SAMPLES,
                warmup: DEFAULT_WARMUP,
            }
        );
    }

    #[test]
    fn storage_flags_are_honoured() {
        let command = parse(&args(&[
            "storage",
            "--samples",
            "42",
            "--warmup",
            "7",
            "--file",
            "/tmp/scratch",
        ]))
        .expect("storage");
        assert_eq!(
            command,
            Command::Storage {
                file: PathBuf::from("/tmp/scratch"),
                samples: 42,
                warmup: 7,
            }
        );
    }

    #[test]
    fn network_measure_takes_a_positional_address() {
        let command = parse(&args(&[
            "network",
            "measure",
            "host-b:7777",
            "--samples",
            "9",
        ]))
        .expect("network measure");
        assert_eq!(
            command,
            Command::NetworkMeasure {
                address: "host-b:7777".to_owned(),
                samples: 9,
                warmup: DEFAULT_WARMUP,
            }
        );
    }

    #[test]
    fn network_listen_defaults_to_the_documented_port() {
        assert_eq!(
            parse(&args(&["network", "listen"])).expect("listen"),
            Command::NetworkListen { port: 7777 }
        );
        assert_eq!(
            parse(&args(&["network", "listen", "--port", "9001"])).expect("listen"),
            Command::NetworkListen { port: 9001 }
        );
    }

    #[test]
    fn unknown_command_is_rejected() {
        assert_eq!(
            parse(&args(&["bandwidth"])).expect_err("unknown"),
            CliError::UnknownCommand("bandwidth".to_owned())
        );
    }

    #[test]
    fn unknown_network_subcommand_is_rejected() {
        assert_eq!(
            parse(&args(&["network"])).expect_err("missing subcommand"),
            CliError::UnknownNetworkSubcommand(String::new())
        );
        assert_eq!(
            parse(&args(&["network", "flood"])).expect_err("unknown subcommand"),
            CliError::UnknownNetworkSubcommand("flood".to_owned())
        );
    }

    #[test]
    fn unknown_flags_and_stray_positionals_are_rejected() {
        assert_eq!(
            parse(&args(&["storage", "--iops", "10"])).expect_err("unknown flag"),
            CliError::UnknownFlag("--iops".to_owned())
        );
        assert_eq!(
            parse(&args(&["storage", "extra"])).expect_err("stray positional"),
            CliError::UnexpectedArgument("extra".to_owned())
        );
        assert_eq!(
            parse(&args(&["network", "measure", "a:1", "b:2"])).expect_err("second address"),
            CliError::UnexpectedArgument("b:2".to_owned())
        );
    }

    #[test]
    fn flag_values_are_validated() {
        assert_eq!(
            parse(&args(&["storage", "--samples"])).expect_err("missing value"),
            CliError::MissingValue("--samples".to_owned())
        );
        assert_eq!(
            parse(&args(&["storage", "--samples", "many"])).expect_err("not a number"),
            CliError::InvalidValue {
                flag: "--samples".to_owned(),
                value: "many".to_owned(),
            }
        );
        assert_eq!(
            parse(&args(&["network", "listen", "--port", "70000"])).expect_err("port overflow"),
            CliError::InvalidValue {
                flag: "--port".to_owned(),
                value: "70000".to_owned(),
            }
        );
        assert_eq!(
            parse(&args(&["storage", "--samples", "0"])).expect_err("zero samples"),
            CliError::ZeroSamples
        );
    }

    #[test]
    fn network_measure_requires_an_address() {
        assert_eq!(
            parse(&args(&["network", "measure"])).expect_err("missing address"),
            CliError::MissingAddress
        );
    }

    #[test]
    fn errors_render_a_useful_message() {
        assert_eq!(
            CliError::UnknownFlag("--nope".to_owned()).to_string(),
            "unknown flag: --nope"
        );
        assert_eq!(
            CliError::ZeroSamples.to_string(),
            "--samples must be at least 1"
        );
    }
}
