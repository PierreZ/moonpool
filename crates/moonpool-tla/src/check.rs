//! Run TLC on a recorded trace and read its verdict.

use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::trace::Trace;
use crate::value::ValueError;

/// The environment variable that names the TLC launcher. When it is not set,
/// [`TraceCheck`] runs `tlc` from the `PATH` (the `tlaplus` package of
/// nixpkgs installs it, and moonpool's dev shell has it).
pub const TLC_ENV: &str = "MOONPOOL_TLC";

/// The invariant a trace spec defines to mark the end of the trace, by
/// default. TLC reports it violated when a behaviour of the spec matched
/// every event, and that is how an accepted trace shows.
pub const DEFAULT_END_INVARIANT: &str = "TraceUnfinished";

/// The variable of the trace spec that counts the matched events, by
/// default: `l` is the index of the next event to match, from 1.
pub const DEFAULT_CURSOR: &str = "l";

/// The module name the trace is written under, by default. The trace spec
/// says `EXTENDS ..., Trace`.
pub const DEFAULT_TRACE_MODULE: &str = "Trace";

/// What TLC says about one trace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// A behaviour of the spec matched every event of the trace, and every
    /// invariant of the configuration held on the way.
    Accepted,
    /// No behaviour of the spec matches the whole trace: the code took a
    /// step the spec forbids, or the spec lacks a step the code is right to
    /// take.
    Rejected(Rejection),
    /// A behaviour of the spec that matched a prefix of the trace broke an
    /// invariant of the configuration.
    ///
    /// Every step of a trace spec is a step of the model, so this verdict is
    /// about the spec, not the code: the model itself breaks the invariant,
    /// or the trace spec lets a hidden step leave the model's next-state
    /// relation. A bug in the code shows as [`Verdict::Rejected`].
    InvariantViolated {
        /// The invariant that TLC reports.
        invariant: String,
        /// How many events were matched when it broke.
        matched: usize,
        /// The model state that breaks it, as TLC prints it.
        state: String,
    },
}

impl Verdict {
    /// Whether the trace is accepted.
    #[must_use]
    pub fn is_accepted(&self) -> bool {
        matches!(self, Self::Accepted)
    }
}

/// The first event of a rejected trace that no behaviour of the spec matches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejection {
    /// The index of the event, from 0, in [`Trace::events`].
    pub index: usize,
    /// The model state after the last matched event, as TLC prints it:
    /// one `/\ var = value` line per variable. Empty when even the first
    /// event does not match.
    pub state: String,
}

impl fmt::Display for Rejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "event {} does not match the spec", self.index)?;
        if !self.state.is_empty() {
            write!(f, "; the model state before it:\n{}", self.state)?;
        }
        Ok(())
    }
}

/// Why a trace could not be checked: no verdict exists.
#[derive(Debug)]
pub enum CheckError {
    /// The trace holds a value TLC cannot read.
    Value(ValueError),
    /// The spec folder or the temporary folder could not be read or written.
    Io(std::io::Error),
    /// TLC could not be started: it is not installed, or [`TLC_ENV`] names
    /// a wrong path.
    TlcNotFound {
        /// The launcher that was tried.
        program: PathBuf,
        /// The error of the start.
        source: std::io::Error,
    },
    /// The spec folder or its configuration breaks the contract of
    /// [`TraceCheck`]: the end invariant is missing or not the last
    /// invariant, the configuration has a comment or a string that is not
    /// closed, a file uses a name of the generated wrapper, or TLC matched
    /// every event without reporting the end invariant.
    Contract(String),
    /// TLC ran but gave no verdict: a parse error in the spec, a wrong
    /// configuration, or an event the trace spec cannot read (a missing
    /// field, a value of the wrong type, a node outside the constant set).
    /// Guard the trace spec (`e.rm \in RM`) to turn a malformed event into
    /// a rejection.
    Tlc {
        /// TLC's exit code, if it exited.
        code: Option<i32>,
        /// TLC's whole output.
        output: String,
    },
}

impl fmt::Display for CheckError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Value(e) => write!(f, "the trace cannot be written: {e}"),
            Self::Io(e) => write!(f, "I/O error while preparing TLC: {e}"),
            Self::Contract(why) => write!(f, "the trace spec breaks the check's contract: {why}"),
            Self::TlcNotFound { program, source } => write!(
                f,
                "cannot start TLC (`{}`): {source}; install `tlaplus` or set {TLC_ENV}",
                program.display()
            ),
            Self::Tlc { code, output } => {
                write!(f, "TLC gave no verdict (exit code {code:?}):\n{output}")
            }
        }
    }
}

impl std::error::Error for CheckError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Value(e) => Some(e),
            Self::Io(e) => Some(e),
            Self::TlcNotFound { source, .. } => Some(source),
            Self::Contract(_) | Self::Tlc { .. } => None,
        }
    }
}

impl From<ValueError> for CheckError {
    fn from(e: ValueError) -> Self {
        Self::Value(e)
    }
}

impl From<std::io::Error> for CheckError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

/// The module that [`TraceCheck`] writes around the trace spec to learn how
/// many events TLC matched.
pub(crate) const WRAPPER: &str = "MoonpoolTraceCheck";

/// The prefix of the line the wrapper prints each time TLC matches more
/// events than before.
pub(crate) const MATCHED_MARK: &str = "<<\"moonpool-tla matched\", ";

/// Checks traces against one trace spec with TLC.
///
/// The spec folder holds the model (`TwoPhase.tla`), the trace spec that
/// extends it and the trace module (`TwoPhaseTrace.tla`), and the trace
/// spec's TLC configuration (`TwoPhaseTrace.cfg`). For each check, the
/// `.tla` and `.cfg` files of the folder are copied to a new temporary
/// folder, the trace is written beside them as `Trace.tla`, and TLC runs
/// there with `-deadlock -workers 1`.
///
/// The trace spec follows one contract:
///
/// - a variable, `l` by default, holds the index of the next event to
///   match, from 1;
/// - the configuration checks the invariant `TraceUnfinished ==
///   l <= Len(Trace)` (the name can change), as its **last** `INVARIANT`.
///   TLC reports it violated when a behaviour matched every event, and
///   that is how an accepted trace shows. TLC reports the first violated
///   invariant in configuration order, so an invariant listed after it
///   would never be seen; the check refuses such a configuration. When TLC
///   ends with no error, no behaviour reached the end of the trace: it is
///   rejected.
/// - the folder has no file named like the generated wrapper
///   (`MoonpoolTraceCheck.tla`, `.cfg`). A file named like the trace module
///   (`Trace.tla`) is not copied: the check writes the trace in its place,
///   so a sample trace can stay beside the trace spec for an editor.
///
/// ```tla
/// TraceNext == \/ l <= Len(Trace) /\ Step   /\ l' = l + 1   \* a logged step
///              \/ l <= Len(Trace) /\ Hidden /\ UNCHANGED l  \* a step the code does not log
/// ```
///
/// To name the first event that does not match, the check runs a small
/// generated module that extends the trace spec and adds a TLC state
/// constraint: it records the highest `l` that any behaviour reached. So a
/// rejection costs one TLC run, plus one more on the matched prefix to
/// print the model state before the event.
#[derive(Debug, Clone)]
pub struct TraceCheck {
    spec_dir: PathBuf,
    spec: String,
    trace_module: String,
    end_invariant: String,
    cursor: String,
    tlc: PathBuf,
    temp_root: Option<PathBuf>,
}

impl TraceCheck {
    /// A check against the trace spec `spec` (a module of `spec_dir`,
    /// `TwoPhaseTrace` say, with its configuration in `TwoPhaseTrace.cfg`).
    /// TLC is the launcher that [`TLC_ENV`] names, or `tlc`.
    pub fn new(spec_dir: impl Into<PathBuf>, spec: &str) -> Self {
        let tlc = std::env::var_os(TLC_ENV).map_or_else(|| PathBuf::from("tlc"), PathBuf::from);
        Self {
            spec_dir: spec_dir.into(),
            spec: spec.to_owned(),
            trace_module: DEFAULT_TRACE_MODULE.to_owned(),
            end_invariant: DEFAULT_END_INVARIANT.to_owned(),
            cursor: DEFAULT_CURSOR.to_owned(),
            tlc,
            temp_root: None,
        }
    }

    /// Write the trace under this module name (default `Trace`).
    #[must_use]
    pub fn trace_module(mut self, name: &str) -> Self {
        name.clone_into(&mut self.trace_module);
        self
    }

    /// The invariant that marks the end of the trace (default
    /// `TraceUnfinished`).
    #[must_use]
    pub fn end_invariant(mut self, name: &str) -> Self {
        name.clone_into(&mut self.end_invariant);
        self
    }

    /// The variable that holds the index of the next event to match
    /// (default `l`).
    #[must_use]
    pub fn cursor(mut self, name: &str) -> Self {
        name.clone_into(&mut self.cursor);
        self
    }

    pub(crate) fn spec_name(&self) -> &str {
        &self.spec
    }

    pub(crate) fn cursor_name(&self) -> &str {
        &self.cursor
    }

    pub(crate) fn end_invariant_name(&self) -> &str {
        &self.end_invariant
    }

    /// Run this TLC launcher instead of the default.
    #[must_use]
    pub fn tlc(mut self, program: impl Into<PathBuf>) -> Self {
        self.tlc = program.into();
        self
    }

    /// Create the folder of each check under `dir` instead of the system's
    /// temporary folder ([`std::env::temp_dir`]).
    #[must_use]
    pub fn temp_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.temp_root = Some(dir.into());
        self
    }

    /// Whether the TLC launcher can be started. A test that needs TLC uses
    /// it to skip on a machine without TLC.
    #[must_use]
    pub fn tlc_available(&self) -> bool {
        Command::new(&self.tlc)
            .arg("-help")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok()
    }

    /// Check the trace: one TLC run, and one more on a rejection to print
    /// the model state before the first event that does not match.
    ///
    /// # Errors
    ///
    /// Returns a [`CheckError`] when TLC cannot run or gives no verdict.
    pub fn check(&self, trace: &Trace) -> Result<Verdict, CheckError> {
        crate::value::check_identifier(&self.spec)?;
        crate::value::check_identifier(&self.cursor)?;
        let config = self.config()?;
        let run = self.run_tlc(trace, &config)?;
        match parse_output(&run, &self.end_invariant, &self.cursor)? {
            Verdict::Rejected(Rejection { index, .. }) if index >= trace.len() => {
                Err(CheckError::Contract(format!(
                    "TLC matched all {} events but did not report `{}` violated",
                    trace.len(),
                    self.end_invariant
                )))
            }
            Verdict::Rejected(Rejection { index, .. }) => {
                // TLC prints no behaviour when it ends with no error. The
                // matched prefix is accepted, and its run prints the
                // behaviour that matched it.
                let state = if index == 0 {
                    String::new()
                } else {
                    let prefix = self.run_tlc(&trace.prefix(index), &config)?;
                    match parse_output(&prefix, &self.end_invariant, &self.cursor)? {
                        Verdict::Accepted => last_state(&prefix.output),
                        other => {
                            return Err(CheckError::Contract(format!(
                                "TLC matched {index} events, but the prefix of {index} events \
                                 is not accepted: {other:?}"
                            )));
                        }
                    }
                };
                Ok(Verdict::Rejected(Rejection { index, state }))
            }
            verdict => Ok(verdict),
        }
    }

    /// Read the configuration and check the contract of the spec folder.
    pub(crate) fn config(&self) -> Result<String, CheckError> {
        // A `Trace.tla` in the folder is allowed (a sample trace helps an
        // editor parse the trace spec): the check does not copy it and
        // writes the real trace in its place.
        for reserved in [format!("{WRAPPER}.tla"), format!("{WRAPPER}.cfg")] {
            if self.spec_dir.join(&reserved).exists() {
                return Err(CheckError::Contract(format!(
                    "the spec folder has a file `{reserved}`, which the check writes"
                )));
            }
        }
        let config = std::fs::read_to_string(self.spec_dir.join(format!("{}.cfg", self.spec)))?;
        let invariants = config_invariants(&config)?;
        if invariants.last() != Some(&self.end_invariant.as_str()) {
            return Err(CheckError::Contract(format!(
                "`{}` must be the last INVARIANT of {}.cfg (the invariants are {invariants:?})",
                self.end_invariant, self.spec
            )));
        }
        Ok(config)
    }

    fn run_tlc(&self, trace: &Trace, config: &str) -> Result<TlcRun, CheckError> {
        // The wrapper's constraint comes first. TLC stops at the first
        // constraint that is false, so after a user constraint the wrapper
        // would miss the states outside it, and name an earlier event than
        // the first one that does not match.
        self.run_wrapped(
            trace,
            &wrapper_module(&self.spec, &self.cursor),
            &format!("CONSTRAINT MoonpoolMatched\n{config}\n"),
        )
    }

    /// Run TLC on `trace` with `wrapper` as the module around the trace
    /// spec and `config` as its whole configuration.
    pub(crate) fn run_wrapped(
        &self,
        trace: &Trace,
        wrapper: &str,
        config: &str,
    ) -> Result<TlcRun, CheckError> {
        let module = trace.to_module(&self.trace_module)?;
        let work = match &self.temp_root {
            Some(root) => TempDir::new(root)?,
            None => TempDir::new(&std::env::temp_dir())?,
        };
        let trace_file = format!("{}.tla", self.trace_module);
        for entry in std::fs::read_dir(&self.spec_dir)? {
            let path = entry?.path();
            let spec_file = path
                .extension()
                .is_some_and(|ext| ext == "tla" || ext == "cfg");
            if spec_file
                && let Some(name) = path.file_name()
                && name != trace_file.as_str()
            {
                std::fs::copy(&path, work.path().join(name))?;
            }
        }
        std::fs::write(work.path().join(&trace_file), module)?;
        std::fs::write(work.path().join(format!("{WRAPPER}.tla")), wrapper)?;
        std::fs::write(work.path().join(format!("{WRAPPER}.cfg")), config)?;
        // TLC unpacks its standard modules (`TLC.tla`, `Naturals.tla`, ...)
        // to fixed names in Java's temporary folder, so two runs that share
        // it race. Give each run its own.
        let java_tmp = work.path().join("java-tmp");
        std::fs::create_dir(&java_tmp)?;
        let mut java_options = std::env::var("JAVA_TOOL_OPTIONS").unwrap_or_default();
        java_options.push(' ');
        java_options.push_str(&java_option("java.io.tmpdir", &java_tmp)?);
        let output = Command::new(&self.tlc)
            .current_dir(work.path())
            .env("JAVA_TOOL_OPTIONS", java_options.trim_start())
            .args(["-deadlock", "-workers", "1", "-cleanup", "-config"])
            .arg(format!("{WRAPPER}.cfg"))
            .arg("-metadir")
            .arg(work.path().join("states"))
            .arg(format!("{WRAPPER}.tla"))
            .output()
            .map_err(|source| CheckError::TlcNotFound {
                program: self.tlc.clone(),
                source,
            })?;
        let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
        text.push_str(&String::from_utf8_lossy(&output.stderr));
        Ok(TlcRun {
            code: output.status.code(),
            output: text,
        })
    }
}

/// `-Dkey=value` for `JAVA_TOOL_OPTIONS`. Java splits that variable on
/// white space, so a value with a space goes in double quotes. Java has no
/// escape inside the quotes: a path with a double quote is refused.
fn java_option(key: &str, value: &Path) -> Result<String, CheckError> {
    let value = value.to_string_lossy();
    if value.contains('"') {
        return Err(CheckError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("Java cannot read a temporary folder with a double quote: {value}"),
        )));
    }
    if value.chars().any(char::is_whitespace) {
        Ok(format!("-D{key}=\"{value}\""))
    } else {
        Ok(format!("-D{key}={value}"))
    }
}

/// The module around the trace spec: a state constraint that is always
/// true and prints the cursor each time a behaviour matches more events
/// than any before. TLC register 1 holds the best so far (one worker).
fn wrapper_module(spec: &str, cursor: &str) -> String {
    format!(
        "---- MODULE {WRAPPER} ----\n\
         EXTENDS {spec}, TLC\n\
         ASSUME TLCSet(1, 0)\n\
         MoonpoolMatched ==\n  \
         IF {cursor} - 1 > TLCGet(1)\n  \
         THEN PrintT(<<\"moonpool-tla matched\", {cursor} - 1>>) /\\ TLCSet(1, {cursor} - 1)\n  \
         ELSE TRUE\n\
         ====\n"
    )
}

/// The exit code and the output of one TLC run.
pub(crate) struct TlcRun {
    pub(crate) code: Option<i32>,
    pub(crate) output: String,
}

/// Read the verdict out of TLC's output.
pub(crate) fn parse_output(
    run: &TlcRun,
    end_invariant: &str,
    cursor: &str,
) -> Result<Verdict, CheckError> {
    if let Some(invariant) = violated_invariant(&run.output) {
        if invariant == end_invariant {
            return Ok(Verdict::Accepted);
        }
        let state = last_state(&run.output);
        let matched = cursor_value(&state, cursor).map_or(0, |next| next.saturating_sub(1));
        return Ok(Verdict::InvariantViolated {
            invariant: invariant.to_owned(),
            matched,
            state,
        });
    }
    if run.code == Some(0) && run.output.contains("No error has been found") {
        return Ok(Verdict::Rejected(Rejection {
            index: matched_events(&run.output),
            state: String::new(),
        }));
    }
    Err(CheckError::Tlc {
        code: run.code,
        output: run.output.clone(),
    })
}

/// The invariants of a TLC configuration, in order: the names after each
/// `INVARIANT` or `INVARIANTS` keyword, up to the next keyword. Comments
/// (`\*` to the end of the line, and `(* ... *)`, which nest) and strings
/// are skipped.
fn config_invariants(config: &str) -> Result<Vec<&str>, CheckError> {
    let mut invariants = Vec::new();
    let mut in_invariants = false;
    for (_, token) in config_tokens(config)? {
        if KEYWORDS.contains(&token) {
            in_invariants = token.starts_with("INVARIANT");
        } else if in_invariants {
            invariants.push(token);
        }
    }
    Ok(invariants)
}

/// The keywords of a TLC configuration.
pub(crate) const KEYWORDS: [&str; 21] = [
    "SPECIFICATION",
    "INIT",
    "NEXT",
    "CONSTANT",
    "CONSTANTS",
    "INVARIANT",
    "INVARIANTS",
    "PROPERTY",
    "PROPERTIES",
    "CONSTRAINT",
    "CONSTRAINTS",
    "ACTION_CONSTRAINT",
    "ACTION_CONSTRAINTS",
    "SYMMETRY",
    "VIEW",
    "CHECK_DEADLOCK",
    "POSTCONDITION",
    "POSTCONDITIONS",
    "ALIAS",
    "TYPE",
    "TYPE_CONSTRAINT",
];

/// The words of a TLC configuration, outside comments and strings, each
/// with its byte offset. A word is a run of characters that are not white
/// space; a comment or a string ends a word. An unclosed comment or string
/// is refused.
pub(crate) fn config_tokens(config: &str) -> Result<Vec<(usize, &str)>, CheckError> {
    let bytes = config.as_bytes();
    let mut tokens = Vec::new();
    let mut word: Option<usize> = None;
    let mut i = 0;
    while i < bytes.len() {
        let rest = &bytes[i..];
        let skip_to = if rest.starts_with(b"\\*") {
            Some(
                rest.iter()
                    .position(|&b| b == b'\n')
                    .map_or(bytes.len(), |n| i + n),
            )
        } else if rest.starts_with(b"(*") {
            Some(block_comment_end(bytes, i)?)
        } else if rest[0] == b'"' {
            Some(string_end(bytes, i)?)
        } else {
            None
        };
        if let Some(next) = skip_to {
            if let Some(start) = word.take() {
                tokens.push((start, &config[start..i]));
            }
            i = next;
        } else if rest[0].is_ascii_whitespace() {
            if let Some(start) = word.take() {
                tokens.push((start, &config[start..i]));
            }
            i += 1;
        } else {
            word.get_or_insert(i);
            i += 1;
        }
    }
    if let Some(start) = word {
        tokens.push((start, &config[start..]));
    }
    Ok(tokens)
}

/// The index just after the `*)` that closes the block comment opened at
/// `start`. Block comments nest.
fn block_comment_end(bytes: &[u8], start: usize) -> Result<usize, CheckError> {
    let mut depth = 0usize;
    let mut i = start;
    while i + 1 < bytes.len() {
        match &bytes[i..i + 2] {
            b"(*" => {
                depth += 1;
                i += 2;
            }
            b"*)" => {
                depth -= 1;
                i += 2;
                if depth == 0 {
                    return Ok(i);
                }
            }
            _ => i += 1,
        }
    }
    Err(CheckError::Contract(
        "the configuration has a `(*` comment that is not closed".to_owned(),
    ))
}

/// The index just after the `"` that closes the string opened at `start`.
/// A backslash escapes the next character.
fn string_end(bytes: &[u8], start: usize) -> Result<usize, CheckError> {
    let mut i = start + 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'"' => return Ok(i + 1),
            _ => i += 1,
        }
    }
    Err(CheckError::Contract(
        "the configuration has a string that is not closed".to_owned(),
    ))
}

/// The highest event count that the wrapper printed; 0 when it printed none.
pub(crate) fn matched_events(output: &str) -> usize {
    output
        .lines()
        .filter_map(|line| line.trim().strip_prefix(MATCHED_MARK))
        .filter_map(|rest| rest.strip_suffix(">>"))
        .filter_map(|n| n.trim().parse().ok())
        .max()
        .unwrap_or(0)
}

/// The name in TLC's `Error: Invariant X is violated.` line (or `... is
/// violated by the initial state:`).
pub(crate) fn violated_invariant(output: &str) -> Option<&str> {
    output.lines().find_map(|line| {
        let rest = line.trim().strip_prefix("Error: Invariant ")?;
        rest.find(" is violated").map(|end| &rest[..end])
    })
}

/// The last state of TLC's error trace: the `/\ var = value` lines after the
/// last `State N:` header, or after `... violated by the initial state:`.
/// Empty when the output has no state.
pub(crate) fn last_state(output: &str) -> String {
    let mut last: Vec<&str> = Vec::new();
    let mut open = false;
    for line in output.lines() {
        if is_state_header(line) {
            last.clear();
            open = true;
        } else if open {
            if line.trim().is_empty() {
                open = false;
            } else {
                last.push(line);
            }
        }
    }
    last.join("\n")
}

/// A `State 3: <TraceNext ...>` header of TLC's error trace, or the line
/// that introduces an initial state breaking an invariant.
fn is_state_header(line: &str) -> bool {
    if line.starts_with("Error: Invariant ") && line.ends_with("by the initial state:") {
        return true;
    }
    line.strip_prefix("State ").is_some_and(|rest| {
        let digits = rest.chars().take_while(char::is_ascii_digit).count();
        digits > 0 && rest[digits..].starts_with(':')
    })
}

/// The value of `/\ cursor = N` in a state.
pub(crate) fn cursor_value(state: &str, cursor: &str) -> Option<usize> {
    let prefix = format!("/\\ {cursor} = ");
    state
        .lines()
        .find_map(|line| line.trim().strip_prefix(prefix.as_str()))
        .and_then(|value| value.trim().parse().ok())
}

/// A folder under the system's temporary folder, removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    /// A new, empty folder under `root`. `create_dir` fails on a name that exists, so
    /// the check never writes into a folder or a link it did not create.
    fn new(root: &Path) -> std::io::Result<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        loop {
            let n = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = root.join(format!(
                "moonpool-tla-{}-{n}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_nanos())
            ));
            match std::fs::create_dir(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e),
            }
        }
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        // Best effort: a folder left behind in the temporary folder is harmless.
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(code: i32, output: &str) -> TlcRun {
        TlcRun {
            code: Some(code),
            output: output.to_owned(),
        }
    }

    const BEHAVIOUR: &str = "Error: Invariant BelowFour is violated.\n\
        Error: The behavior up to this point is:\n\
        State 1: <Initial predicate>\n\
        /\\ l = 1\n\
        /\\ x = 0\n\
        \n\
        State 2: <TraceNext line 12, col 1 to line 12, col 40 of module CounterTrace>\n\
        /\\ l = 2\n\
        /\\ x = 1\n\
        \n\
        Finished in 00s\n";

    #[test]
    fn the_end_invariant_means_accepted() {
        let out = BEHAVIOUR.replace("BelowFour", "TraceUnfinished");
        assert_eq!(
            parse_output(&run(12, &out), "TraceUnfinished", "l").expect("verdict"),
            Verdict::Accepted
        );
    }

    #[test]
    fn another_invariant_is_reported_with_the_last_state() {
        let verdict = parse_output(&run(12, BEHAVIOUR), "TraceUnfinished", "l").expect("verdict");
        assert_eq!(
            verdict,
            Verdict::InvariantViolated {
                invariant: "BelowFour".to_owned(),
                matched: 1,
                state: "/\\ l = 2\n/\\ x = 1".to_owned(),
            }
        );
    }

    #[test]
    fn no_error_is_a_rejection_after_the_best_match() {
        let out = "<<\"moonpool-tla matched\", 1>>\n\
                   <<\"moonpool-tla matched\", 3>>\n\
                   Model checking completed. No error has been found.\n";
        assert_eq!(
            parse_output(&run(0, out), "TraceUnfinished", "l").expect("verdict"),
            Verdict::Rejected(Rejection {
                index: 3,
                state: String::new()
            })
        );
    }

    #[test]
    fn an_output_with_no_verdict_is_an_error() {
        let error = parse_output(&run(150, "Error: parse error"), "TraceUnfinished", "l");
        assert!(matches!(
            error,
            Err(CheckError::Tlc {
                code: Some(150),
                ..
            })
        ));
    }

    #[test]
    fn an_initial_state_is_read_too() {
        let out = "Error: Invariant BelowFour is violated by the initial state:\n\
                   /\\ l = 1\n\
                   /\\ x = 9\n\
                   \n";
        assert_eq!(last_state(out), "/\\ l = 1\n/\\ x = 9");
        assert_eq!(cursor_value(&last_state(out), "l"), Some(1));
        assert_eq!(violated_invariant(out), Some("BelowFour"));
    }

    #[test]
    fn the_invariants_of_a_configuration_are_read_in_order() {
        let config = "SPECIFICATION TraceSpec\n\
                      CONSTANT RM <- TraceRM\n\
                      INVARIANT TypeOK \\* the types\n\
                      INVARIANTS Consistent\n  TraceUnfinished\n\
                      CONSTRAINT Bound\n";
        assert_eq!(
            config_invariants(config).expect("readable"),
            ["TypeOK", "Consistent", "TraceUnfinished"]
        );
        assert!(
            config_invariants("SPECIFICATION S\n")
                .expect("readable")
                .is_empty(),
            "a config with no INVARIANT line names no invariant"
        );
    }

    #[test]
    fn block_comments_strings_and_type_constraints_are_read() {
        let config = "(* the trace (* nested *) check\n\
                      INVARIANT Hidden *)\n\
                      SPECIFICATION TraceSpec\n\
                      CONSTANT Name = \"INVARIANT (* \\\" Fake\"\n\
                      INVARIANT TypeOK(* inline *)Consistent\n\
                      TYPE_CONSTRAINT Bound\n\
                      INVARIANT TraceUnfinished \\* (* not a block\n";
        assert_eq!(
            config_invariants(config).expect("readable"),
            ["TypeOK", "Consistent", "TraceUnfinished"]
        );
    }

    #[test]
    fn an_unclosed_comment_or_string_is_refused() {
        for config in [
            "INVARIANT A (* open",
            "INVARIANT A (* (* *)",
            "CONSTANT N = \"open\nINVARIANT A",
        ] {
            assert!(
                matches!(config_invariants(config), Err(CheckError::Contract(_))),
                "{config}"
            );
        }
    }

    #[test]
    fn a_java_option_with_a_space_is_quoted() {
        assert_eq!(
            java_option("java.io.tmpdir", Path::new("/tmp/a b")).expect("quotable"),
            "-Djava.io.tmpdir=\"/tmp/a b\""
        );
        assert_eq!(
            java_option("java.io.tmpdir", Path::new("/tmp/ab")).expect("plain"),
            "-Djava.io.tmpdir=/tmp/ab"
        );
        assert!(java_option("java.io.tmpdir", Path::new("/tmp/a\"b")).is_err());
    }

    #[test]
    fn a_rejection_prints_its_state() {
        let rejection = Rejection {
            index: 2,
            state: "/\\ x = 2".to_owned(),
        };
        assert_eq!(
            rejection.to_string(),
            "event 2 does not match the spec; the model state before it:\n/\\ x = 2"
        );
        let first = Rejection {
            index: 0,
            state: String::new(),
        };
        assert_eq!(first.to_string(), "event 0 does not match the spec");
    }
}
