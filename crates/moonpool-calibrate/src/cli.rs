//! Argument parsing, built on `clap`'s derive API.
//!
//! One rule shapes the surface: **stdout belongs to generated Rust**. Help,
//! version, and parse errors are rendered by the caller onto stderr rather than
//! clap's default stdout, so `moonpool-calibrate storage > measured_storage.rs`
//! can never be corrupted by usage text.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

use crate::network::DEFAULT_PORT;

/// Recorded samples per operation when `--samples` is not given.
pub const DEFAULT_SAMPLES: u64 = 1_000;

/// Unrecorded warmup iterations per operation when `--warmup` is not given.
pub const DEFAULT_WARMUP: u64 = 100;

/// Measure the real host and emit moonpool `LatencyDistribution` constants.
#[derive(Debug, Parser)]
#[command(
    name = "moonpool-calibrate",
    version,
    about = "Measure the real host and emit moonpool LatencyDistribution constants",
    long_about = "Measure the real host with raw std I/O and emit moonpool LatencyDistribution \
constants on stdout.\n\n\
Diagnostics go to stderr, so stdout can be redirected straight into a source file:\n  \
moonpool-calibrate storage > measured_storage.rs",
    arg_required_else_help = true
)]
pub struct Cli {
    /// What to measure.
    #[command(subcommand)]
    pub command: Command,
}

/// What the binary was asked to do.
#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum Command {
    /// Measure read, write and sync latency against a scratch file.
    Storage {
        /// Scratch file to measure against.
        #[arg(long, value_name = "PATH", default_value_os_t = crate::storage::default_file())]
        file: PathBuf,

        /// Recorded samples per operation.
        #[arg(long, value_name = "N", default_value_t = DEFAULT_SAMPLES, value_parser = at_least_one())]
        samples: u64,

        /// Unrecorded warmup iterations per operation.
        #[arg(long, value_name = "N", default_value_t = DEFAULT_WARMUP)]
        warmup: u64,
    },

    /// Measure small-message TCP round-trip time.
    Network {
        /// Which side of the measurement to run.
        #[command(subcommand)]
        command: NetworkCommand,
    },
}

/// The two halves of a network calibration.
#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum NetworkCommand {
    /// Run the ping/pong responder the measuring side connects to.
    Listen {
        /// TCP port to bind on all interfaces.
        #[arg(long, value_name = "PORT", default_value_t = DEFAULT_PORT)]
        port: u16,
    },

    /// Measure round-trip time against a listener.
    Measure {
        /// `host:port` of the listener.
        #[arg(value_name = "HOST:PORT")]
        address: String,

        /// Recorded samples.
        #[arg(long, value_name = "N", default_value_t = DEFAULT_SAMPLES, value_parser = at_least_one())]
        samples: u64,

        /// Unrecorded warmup round trips.
        #[arg(long, value_name = "N", default_value_t = DEFAULT_WARMUP)]
        warmup: u64,
    },
}

/// Value parser rejecting a sample count of zero: there is nothing to take
/// percentiles of.
fn at_least_one() -> clap::builder::RangedU64ValueParser {
    clap::value_parser!(u64).range(1..)
}

#[cfg(test)]
mod tests {
    use super::{Cli, Command, DEFAULT_SAMPLES, DEFAULT_WARMUP, NetworkCommand};
    use clap::error::ErrorKind;
    use clap::{CommandFactory, Parser as _};
    use std::path::PathBuf;

    fn parse(tokens: &[&str]) -> Result<Command, clap::Error> {
        let mut command_line = vec!["moonpool-calibrate"];
        command_line.extend_from_slice(tokens);
        Cli::try_parse_from(command_line).map(|cli| cli.command)
    }

    #[test]
    fn the_command_definition_is_valid() {
        // clap's own consistency checks: duplicate flags, bad defaults, and
        // conflicting settings all surface here rather than at runtime.
        Cli::command().debug_assert();
    }

    #[test]
    fn no_arguments_shows_help_and_reports_a_usage_error() {
        let error = parse(&[]).expect_err("bare invocation");
        assert_eq!(
            error.kind(),
            ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
        );
        // `arg_required_else_help` shows the help text but still reports a
        // non-zero exit: naming no subcommand is a usage error, not a request.
        assert_ne!(error.exit_code(), 0);
        assert!(error.render().to_string().contains("storage"));
    }

    #[test]
    fn help_is_rendered_without_touching_stdout() {
        let error = parse(&["--help"]).expect_err("help is reported as an Error");
        assert_eq!(error.kind(), ErrorKind::DisplayHelp);
        assert_eq!(error.exit_code(), 0);

        let rendered = error.render().to_string();
        assert!(rendered.contains("moonpool-calibrate storage > measured_storage.rs"));
        assert!(rendered.contains("storage"));
        assert!(rendered.contains("network"));
    }

    #[test]
    fn storage_defaults_are_applied() {
        assert_eq!(
            parse(&["storage"]).expect("storage"),
            Command::Storage {
                file: crate::storage::default_file(),
                samples: DEFAULT_SAMPLES,
                warmup: DEFAULT_WARMUP,
            }
        );
    }

    #[test]
    fn storage_flags_are_honoured() {
        let command = parse(&[
            "storage",
            "--samples",
            "42",
            "--warmup",
            "7",
            "--file",
            "/tmp/scratch",
        ])
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
        assert_eq!(
            parse(&["network", "measure", "host-b:7777", "--samples", "9"])
                .expect("network measure"),
            Command::Network {
                command: NetworkCommand::Measure {
                    address: "host-b:7777".to_owned(),
                    samples: 9,
                    warmup: DEFAULT_WARMUP,
                },
            }
        );
    }

    #[test]
    fn network_listen_defaults_to_the_documented_port() {
        assert_eq!(
            parse(&["network", "listen"]).expect("listen"),
            Command::Network {
                command: NetworkCommand::Listen { port: 7777 },
            }
        );
        assert_eq!(
            parse(&["network", "listen", "--port", "9001"]).expect("listen"),
            Command::Network {
                command: NetworkCommand::Listen { port: 9001 },
            }
        );
    }

    #[test]
    fn unknown_commands_and_subcommands_are_rejected() {
        assert_eq!(
            parse(&["bandwidth"]).expect_err("unknown command").kind(),
            ErrorKind::InvalidSubcommand
        );
        assert_eq!(
            parse(&["network"]).expect_err("missing subcommand").kind(),
            ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
        );
        assert_eq!(
            parse(&["network", "flood"])
                .expect_err("unknown subcommand")
                .kind(),
            ErrorKind::InvalidSubcommand
        );
    }

    #[test]
    fn unknown_flags_and_stray_positionals_are_rejected() {
        assert_eq!(
            parse(&["storage", "--iops", "10"])
                .expect_err("unknown flag")
                .kind(),
            ErrorKind::UnknownArgument
        );
        assert_eq!(
            parse(&["storage", "extra"])
                .expect_err("stray positional")
                .kind(),
            ErrorKind::UnknownArgument
        );
        assert_eq!(
            parse(&["network", "measure", "a:1", "b:2"])
                .expect_err("second address")
                .kind(),
            ErrorKind::UnknownArgument
        );
    }

    #[test]
    fn flag_values_are_validated() {
        assert_eq!(
            parse(&["storage", "--samples"])
                .expect_err("missing value")
                .kind(),
            ErrorKind::InvalidValue
        );
        assert_eq!(
            parse(&["storage", "--samples", "many"])
                .expect_err("not a number")
                .kind(),
            ErrorKind::ValueValidation
        );
        assert_eq!(
            parse(&["network", "listen", "--port", "70000"])
                .expect_err("port overflow")
                .kind(),
            ErrorKind::ValueValidation
        );
    }

    #[test]
    fn a_zero_sample_count_is_rejected() {
        let error = parse(&["storage", "--samples", "0"]).expect_err("zero samples");
        assert_eq!(error.kind(), ErrorKind::ValueValidation);
        assert_ne!(error.exit_code(), 0);

        assert_eq!(
            parse(&["network", "measure", "a:1", "--samples", "0"])
                .expect_err("zero samples")
                .kind(),
            ErrorKind::ValueValidation
        );
    }

    #[test]
    fn network_measure_requires_an_address() {
        assert_eq!(
            parse(&["network", "measure"])
                .expect_err("missing address")
                .kind(),
            ErrorKind::MissingRequiredArgument
        );
    }
}
