//! Check a trace while the run that writes it goes on.
//!
//! A long run writes more events than one TLC run can follow: TLC keeps one
//! behaviour of at most 65,535 states, and a post-run check sees a
//! divergence only when the run ends. [`Live`] takes the events as the run
//! writes them and checks them in chunks on a thread of its own, while the
//! run goes on. Each chunk starts from every model state at which the
//! chunk before it ended, so the chunks together check what one TLC run
//! over the whole trace would check, with no limit on the length. The
//! first chunk that no behaviour matches stops the check, and the run can
//! see it ([`Live::is_stopped`]) and stop too.
//!
//! The hand-off: the wrapper module prints each state at the end of a
//! chunk (the cursor one past the chunk's last event), and the next chunk
//! starts from the set of those states, with the cursor back at 1. Each
//! state and its two markers are one `PrintT`, so a line that TLC prints
//! from another thread (its progress line) never falls inside a state. A
//! printed state is a TLA+ value, so the next chunk reads it back: every
//! model value in a state must be a name the spec declares (a constant
//! that the configuration binds to the model value of the same name, as
//! `None = None`).

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::thread::JoinHandle;

use crate::Value;
use crate::check::{
    CheckError, KEYWORDS, MATCHED_MARK, Rejection, TlcRun, TraceCheck, Verdict, WRAPPER,
    config_tokens, cursor_value, last_state, matched_events, parse_output, violated_invariant,
};
use crate::trace::Trace;

/// The events of one chunk, by default.
pub const DEFAULT_CHUNK: usize = 500;

/// The chunks that may wait for TLC before [`Live::event`] blocks, by
/// default.
pub const DEFAULT_BACKLOG: usize = 4;

/// The most events of one chunk: TLC keeps one behaviour of at most 65,535
/// states, and a chunk of `n` events is a behaviour of `n + 1` states.
pub const MAX_CHUNK: usize = 65_534;

/// The string that opens a state printed at the end of a chunk.
const END_OPEN: &str = "\"moonpool-tla end {\"";
/// The string that closes it.
const END_CLOSE: &str = "\"moonpool-tla end }\"";

/// How to check one stream of events live: built by [`TraceCheck::live`].
#[derive(Debug, Clone)]
pub struct LiveCheck {
    check: TraceCheck,
    next: String,
    chunk: usize,
    backlog: usize,
}

impl TraceCheck {
    /// A live check against this trace spec. `next` is the next-state
    /// action of the trace spec (`TraceNext`): a chunk after the first
    /// starts from the states the chunk before it ended in, so it runs as
    /// `INIT` those states and `NEXT next` in place of the configuration's
    /// `SPECIFICATION`. A configuration that names `INIT` and `NEXT`
    /// itself keeps its `NEXT`.
    #[must_use]
    pub fn live(&self, next: &str) -> LiveCheck {
        LiveCheck {
            check: self.clone(),
            next: next.to_owned(),
            chunk: DEFAULT_CHUNK,
            backlog: DEFAULT_BACKLOG,
        }
    }
}

impl LiveCheck {
    /// Check `events` events at a time (default [`DEFAULT_CHUNK`]).
    ///
    /// # Panics
    ///
    /// When `events` is 0 or above [`MAX_CHUNK`].
    #[must_use]
    pub fn chunk(mut self, events: usize) -> Self {
        assert!(
            (1..=MAX_CHUNK).contains(&events),
            "a chunk holds 1 to {MAX_CHUNK} events, not {events}"
        );
        self.chunk = events;
        self
    }

    /// Let at most `chunks` full chunks wait for TLC (default
    /// [`DEFAULT_BACKLOG`]); a run that writes faster then waits in
    /// [`Live::event`].
    ///
    /// # Panics
    ///
    /// When `chunks` is 0.
    #[must_use]
    pub fn backlog(mut self, chunks: usize) -> Self {
        assert!(chunks > 0, "the backlog holds at least one chunk");
        self.backlog = chunks;
        self
    }

    /// Start the check on its own thread. The definitions of `start` hold
    /// for the whole stream; its events are the first events.
    ///
    /// # Errors
    ///
    /// Returns a [`CheckError`] when an identifier of the check is not a
    /// TLA+ identifier, or the configuration breaks the contract of
    /// [`TraceCheck`].
    pub fn start(self, start: &Trace) -> Result<Live, CheckError> {
        crate::value::check_identifier(&self.next)?;
        let config = self.check.config()?;
        let (sender, receiver) = sync_channel(self.backlog);
        let stopped = Arc::new(AtomicBool::new(false));
        let mut definitions = start.prefix(0);
        let worker = {
            let stopped = Arc::clone(&stopped);
            let definitions = definitions.clone();
            std::thread::spawn(move || {
                let result = Worker::new(self.check, self.next, config, definitions).run(&receiver);
                stopped.store(true, Ordering::SeqCst);
                result
            })
        };
        let mut live = Live {
            pending: Trace::new(),
            template: std::mem::take(&mut definitions),
            chunk: self.chunk,
            sender: Some(sender),
            worker: Some(worker),
            stopped,
            events: 0,
        };
        live.pending = live.template.clone();
        live.extend(start);
        Ok(live)
    }
}

/// A live check that runs: give it the events of the run in order, then
/// [`finish`](Self::finish) it for the verdict.
///
/// A [`Live`] dropped before [`finish`](Self::finish) leaves its thread to
/// check what it already sent, and nobody reads the verdict.
#[derive(Debug)]
pub struct Live {
    pending: Trace,
    template: Trace,
    chunk: usize,
    sender: Option<SyncSender<Trace>>,
    worker: Option<JoinHandle<Result<Verdict, CheckError>>>,
    stopped: Arc<AtomicBool>,
    events: usize,
}

impl Live {
    /// Append one event, as [`Trace::event`]. Returns `false` once the
    /// check has stopped: a chunk was refused or broke an invariant, or TLC
    /// gave no verdict. The run can stop then; later events are dropped.
    pub fn event<K: Into<String>>(
        &mut self,
        name: &str,
        fields: impl IntoIterator<Item = (K, Value)>,
    ) -> bool {
        if self.is_stopped() {
            return false;
        }
        self.pending.event(name, fields);
        self.after_push()
    }

    /// Append every event of `trace`, in order; its definitions are not
    /// read. Returns `false` once the check has stopped, as
    /// [`event`](Self::event).
    pub fn extend(&mut self, trace: &Trace) -> bool {
        for event in trace.events() {
            if self.is_stopped() {
                return false;
            }
            self.pending.push_value(event.clone());
            self.after_push();
        }
        !self.is_stopped()
    }

    /// Whether the check has stopped before the end of the stream: a chunk
    /// was refused or broke an invariant, or TLC gave no verdict.
    #[must_use]
    pub fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::SeqCst)
    }

    /// How many events were given so far.
    #[must_use]
    pub fn events(&self) -> usize {
        self.events
    }

    /// Check the last chunk and wait for the verdict of the whole stream.
    /// A rejection and a broken invariant count their events from the
    /// first event of the stream.
    ///
    /// # Errors
    ///
    /// Returns a [`CheckError`] when TLC cannot run or gives no verdict.
    ///
    /// # Panics
    ///
    /// When the thread of the check panicked.
    pub fn finish(mut self) -> Result<Verdict, CheckError> {
        if !self.pending.is_empty() {
            self.send();
        }
        drop(self.sender.take());
        let worker = self.worker.take().expect("a live check finishes once");
        match worker.join() {
            Ok(result) => result,
            Err(panic) => std::panic::resume_unwind(panic),
        }
    }

    fn after_push(&mut self) -> bool {
        self.events += 1;
        if self.pending.len() >= self.chunk {
            self.send();
        }
        !self.is_stopped()
    }

    fn send(&mut self) {
        let chunk = std::mem::replace(&mut self.pending, self.template.clone());
        if let Some(sender) = &self.sender {
            // The worker drops its receiver when it stops: the chunk is then
            // not needed.
            let _ = sender.send(chunk);
        }
    }
}

/// The thread of a live check.
struct Worker {
    check: TraceCheck,
    next: String,
    config: String,
    definitions: Trace,
    /// The variables of the trace spec, from its initial state.
    names: Vec<String>,
    /// The states at which the last chunk ended; `None` before the first.
    starts: Option<Vec<String>>,
    /// The events checked so far.
    offset: usize,
}

impl Worker {
    fn new(check: TraceCheck, next: String, config: String, definitions: Trace) -> Self {
        Self {
            check,
            next,
            config,
            definitions,
            names: Vec::new(),
            starts: None,
            offset: 0,
        }
    }

    fn run(mut self, chunks: &Receiver<Trace>) -> Result<Verdict, CheckError> {
        if let Some(verdict) = self.learn_names()? {
            return Ok(verdict);
        }
        for chunk in chunks {
            if let Some(verdict) = self.judge(&chunk)? {
                return Ok(verdict);
            }
        }
        Ok(Verdict::Accepted)
    }

    /// Read the variables of the trace spec from an initial state: TLC
    /// prints it when the empty trace ends at once. `Some` when the initial
    /// state already breaks an invariant.
    fn learn_names(&mut self) -> Result<Option<Verdict>, CheckError> {
        let empty = self.definitions.prefix(0);
        let run = self.check.run_wrapped(
            &empty,
            &self.wrapper(&[], None),
            &format!("CONSTRAINT MoonpoolMatched\n{}\n", self.config),
        )?;
        match parse_output(&run, self.check.end_invariant_name(), self.cursor())? {
            Verdict::Accepted => {}
            verdict @ Verdict::InvariantViolated { .. } => return Ok(Some(verdict)),
            Verdict::Rejected(_) => {
                return Err(CheckError::Contract(format!(
                    "the empty trace does not end at once: `{}` must be violated by the \
                     initial state",
                    self.check.end_invariant_name()
                )));
            }
        }
        self.names = state_names(&last_state(&run.output));
        if !self.names.iter().any(|name| name == self.cursor()) {
            return Err(CheckError::Contract(format!(
                "the trace spec has no variable `{}`",
                self.cursor()
            )));
        }
        Ok(None)
    }

    /// Check one chunk. `Some` when the stream stops here.
    fn judge(&mut self, chunk: &Trace) -> Result<Option<Verdict>, CheckError> {
        let run = self.run_chunk(chunk)?;
        if let Some(invariant) = violated_invariant(&run.output) {
            let state = last_state(&run.output);
            let matched = cursor_value(&state, self.cursor()).map_or(0, |n| n.saturating_sub(1));
            return Ok(Some(Verdict::InvariantViolated {
                invariant: invariant.to_owned(),
                matched: self.offset + matched,
                state,
            }));
        }
        let ends = no_error(&run)?;
        if ends.is_empty() {
            let index = matched_events(&run.output);
            if index >= chunk.len() {
                return Err(CheckError::Contract(format!(
                    "TLC matched all {} events of a chunk but printed no state at its end",
                    chunk.len()
                )));
            }
            let state = if index > 0 {
                no_error(&self.run_chunk(&chunk.prefix(index))?)?.join("\n")
            } else {
                self.starts.as_ref().map_or_else(String::new, |s| {
                    s.join(
                        "
",
                    )
                })
            };
            return Ok(Some(Verdict::Rejected(Rejection {
                index: self.offset + index,
                state,
            })));
        }
        self.offset += chunk.len();
        self.starts = Some(ends);
        Ok(None)
    }

    fn run_chunk(&self, chunk: &Trace) -> Result<TlcRun, CheckError> {
        let starts = self.starts.as_deref();
        let config = live_config(
            &self.config,
            self.check.end_invariant_name(),
            starts.map(|_| self.next.as_str()),
        )?;
        let names: Vec<&str> = self.names.iter().map(String::as_str).collect();
        self.check.run_wrapped(
            chunk,
            &self.wrapper(&names, starts),
            &format!("CONSTRAINT MoonpoolMatched\n{config}\n"),
        )
    }

    fn cursor(&self) -> &str {
        self.check.cursor_name()
    }

    /// The module around the trace spec for one chunk: it records the best
    /// match as [`TraceCheck`]'s wrapper does, prints each state at the end
    /// of the chunk (when it knows the variables, `names`), and defines
    /// `MoonpoolStart`, the states the chunk starts from.
    fn wrapper(&self, names: &[&str], starts: Option<&[String]>) -> String {
        let cursor = self.cursor();
        let spec = self.check.spec_name();
        let mut out = format!(
            "---- MODULE {WRAPPER} ----\n\
             EXTENDS {spec}, TLC\n\
             LOCAL INSTANCE Sequences\n\
             ASSUME TLCSet(1, 0)\n\
             MoonpoolTrue == TRUE\n"
        );
        if let Some(starts) = starts {
            out.push_str("MoonpoolStates == {\n  ");
            out.push_str(&starts.join(",\n  "));
            out.push_str("\n}\n");
            out.push_str("MoonpoolStart == \\E MoonpoolS \\in MoonpoolStates :\n");
            for name in names {
                if *name == cursor {
                    let _ = writeln!(out, "  /\\ {name} = 1");
                } else {
                    let _ = writeln!(out, "  /\\ {name} = MoonpoolS.{name}");
                }
            }
        }
        let _ = write!(
            out,
            "MoonpoolMatched ==\n  \
             /\\ IF {cursor} - 1 > TLCGet(1)\n     \
             THEN PrintT({MATCHED_MARK}{cursor} - 1>>) /\\ TLCSet(1, {cursor} - 1)\n     \
             ELSE TRUE\n"
        );
        if !names.is_empty() {
            let fields: Vec<String> = names.iter().map(|n| format!("{n} |-> {n}")).collect();
            let _ = write!(
                out,
                "  /\\ IF {cursor} = Len(Trace) + 1\n     \
                 THEN PrintT(<<{END_OPEN}, [{}], {END_CLOSE}>>)\n     \
                 ELSE TRUE\n",
                fields.join(", ")
            );
        }
        out.push_str("====\n");
        out
    }
}

/// The states that a run which ended with no error printed at the end of
/// its chunk, each once, in the order printed.
fn no_error(run: &TlcRun) -> Result<Vec<String>, CheckError> {
    if run.code == Some(0) && run.output.contains("No error has been found") {
        Ok(end_states(&run.output))
    } else {
        Err(CheckError::Tlc {
            code: run.code,
            output: run.output.clone(),
        })
    }
}

/// The values printed between the end markers, each once.
///
/// The wrapper prints a state and its two markers in one `PrintT`, so the
/// three are one print: TLC prints its own messages (the progress line it
/// prints each minute) from another thread, and such a message falls
/// before or after a print, never inside it. A state is the text between
/// the two markers, without the commas of the tuple.
fn end_states(output: &str) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut states = Vec::new();
    let mut rest = output;
    while let Some(open) = rest.find(END_OPEN) {
        let after = &rest[open + END_OPEN.len()..];
        let Some(close) = after.find(END_CLOSE) else {
            break;
        };
        let state = after[..close]
            .trim()
            .trim_start_matches(',')
            .trim_end_matches(',')
            .trim()
            .to_owned();
        if seen.insert(state.clone()) {
            states.push(state);
        }
        rest = &after[close + END_CLOSE.len()..];
    }
    states
}

/// The variables of a state as TLC prints it: one `/\ name = value` line
/// for each, a long value going on over the next lines.
fn state_names(state: &str) -> Vec<String> {
    state
        .lines()
        .filter_map(|line| line.strip_prefix("/\\ "))
        .filter_map(|rest| rest.split_once(" = "))
        .map(|(name, _)| name.trim().to_owned())
        .collect()
}

/// The configuration of a chunk: the end invariant is replaced by
/// `MoonpoolTrue` (the wrapper prints the end states, and TLC must go on
/// after the first), and with `next`, the chunk starts from
/// `MoonpoolStart`: `SPECIFICATION s` becomes `INIT MoonpoolStart NEXT
/// next`, and `INIT i` becomes `INIT MoonpoolStart`.
fn live_config(
    config: &str,
    end_invariant: &str,
    next: Option<&str>,
) -> Result<String, CheckError> {
    let tokens = config_tokens(config)?;
    let mut edits: Vec<(usize, usize, String)> = Vec::new();
    let mut keyword = "";
    for &(at, token) in &tokens {
        if KEYWORDS.contains(&token) {
            keyword = token;
            if token == "SPECIFICATION" && next.is_some() {
                edits.push((at, at + token.len(), "INIT".to_owned()));
                keyword = "SPECIFICATION-NEXT";
            }
            continue;
        }
        match (keyword, next) {
            ("INVARIANT" | "INVARIANTS", _) if token == end_invariant => {
                edits.push((at, at + token.len(), "MoonpoolTrue".to_owned()));
            }
            ("SPECIFICATION-NEXT", Some(next)) => {
                edits.push((at, at + token.len(), format!("MoonpoolStart\nNEXT {next}")));
            }
            ("INIT", Some(_)) => {
                edits.push((at, at + token.len(), "MoonpoolStart".to_owned()));
            }
            _ => {}
        }
    }
    let mut out = config.to_owned();
    for (start, end, text) in edits.into_iter().rev() {
        out.replace_range(start..end, &text);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_later_chunk_starts_from_the_hand_off() {
        let config =
            "SPECIFICATION TraceSpec\nCONSTANT N = 3\nINVARIANT Ok\nINVARIANT TraceUnfinished\n";
        assert_eq!(
            live_config(config, "TraceUnfinished", Some("TraceNext")).expect("readable"),
            "INIT MoonpoolStart\nNEXT TraceNext\nCONSTANT N = 3\nINVARIANT Ok\nINVARIANT MoonpoolTrue\n"
        );
        assert_eq!(
            live_config(config, "TraceUnfinished", None).expect("readable"),
            "SPECIFICATION TraceSpec\nCONSTANT N = 3\nINVARIANT Ok\nINVARIANT MoonpoolTrue\n"
        );
        let split = "INIT TraceInit\nNEXT TraceNext\nINVARIANTS Ok TraceUnfinished\n";
        assert_eq!(
            live_config(split, "TraceUnfinished", Some("Ignored")).expect("readable"),
            "INIT MoonpoolStart\nNEXT TraceNext\nINVARIANTS Ok MoonpoolTrue\n"
        );
    }

    #[test]
    fn the_end_states_are_read_once_each() {
        let out = "<<\"moonpool-tla end {\", [l |-> 3, x |-> 2], \"moonpool-tla end }\">>\n\
                   noise\n\
                   << \"moonpool-tla end {\",\n  [ l |-> 3,\n    x |-> 4 ],\n  \"moonpool-tla end }\" >>\n\
                   <<\"moonpool-tla end {\", [l |-> 3, x |-> 2], \"moonpool-tla end }\">>\n";
        assert_eq!(
            end_states(out),
            ["[l |-> 3, x |-> 2]", "[ l |-> 3,\n    x |-> 4 ]"]
        );
    }

    /// TLC prints its progress line each minute from another thread. A
    /// chunk that runs longer than a minute must not read that line as a
    /// part of a state: the next chunk would not parse.
    #[test]
    fn a_tlc_message_is_never_read_as_a_state() {
        let out = "Progress(123) at 2026-11-23 10:04:37: 9 states generated\n\
                   <<\"moonpool-tla end {\", [l |-> 3, x |-> 2], \"moonpool-tla end }\">>\n\
                   Progress(124) at 2026-11-23 10:05:37: 12 states generated\n\
                   << \"moonpool-tla end {\",\n  [l |-> 3, x |-> 4],\n  \"moonpool-tla end }\" >>\n";
        assert_eq!(
            end_states(out),
            ["[l |-> 3, x |-> 2]", "[l |-> 3, x |-> 4]"]
        );
    }

    #[test]
    fn the_names_of_a_state_are_read() {
        let state = "/\\ f = << 1,\n  2 >>\n/\\ l = 1\n/\\ x = 0";
        assert_eq!(state_names(state), ["f", "l", "x"]);
    }
}
