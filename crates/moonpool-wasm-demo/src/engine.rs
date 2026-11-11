//! The run engine of the website (#416 (engine and player)): `run(config)`
//! runs one seed of a scenario and returns what the run recorded.
//!
//! The recorder is an ordinary [`Invariant`] that reads the trace events of
//! the run through [`TraceQuery`], as any oracle does. The scenario code has no
//! drawing code. The result ([`Run`]) is the stable model that the player of
//! the site draws; `model` is its version.

use std::cell::Cell;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use moonpool_sim::runner::builder::{ProcessCount, WorkloadCount};
use moonpool_sim::{
    Attrition, AttritionScope, AttritionVictims, Chaos, ChaosMode, FieldValue, Invariant,
    SIM_FAULT_EVENT_NAME, SimulationBuilder, TraceEvent, TraceQuery, rng_call_count,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::register::{
    self, CLIENT, Freshness, INVARIANT, RegisterClient, RegisterInvariant, RegisterServer, SERVER,
};

/// The version of the [`Run`] model. Bump it when a field changes meaning.
pub const MODEL: u32 = 1;

/// The chaos surfaces of a run. Each one that is on uses `ChaosMode::Random`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ChaosSet {
    /// Network faults: latency, clogs, partitions, closed connections.
    pub network: bool,
    /// Storage faults: errors, torn and lost writes, crashes.
    pub storage: bool,
    /// Reboots: a server crashes or stops and starts again.
    pub reboots: bool,
}

impl Default for ChaosSet {
    /// The default chaos of the site: network faults and reboots.
    fn default() -> Self {
        Self {
            network: true,
            storage: false,
            reboots: true,
        }
    }
}

/// What to run: a scenario, a seed, its chaos, and the planted bug.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RunConfig {
    /// The scenario. Today one: `"register"`, the replicated register.
    pub scenario: String,
    /// The seed of the run.
    pub seed: u64,
    /// The chaos surfaces that are on.
    pub chaos: ChaosSet,
    /// The number of server processes (1 to 5).
    pub servers: usize,
    /// The planted bug: the client reads from one server, not from a quorum.
    pub bug: bool,
}

impl Default for RunConfig {
    fn default() -> Self {
        Self {
            scenario: "register".into(),
            seed: 256,
            chaos: ChaosSet::default(),
            servers: 3,
            bug: false,
        }
    }
}

/// One process or workload of the run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProcessInfo {
    /// Its address, the `source` of its events.
    pub ip: String,
    /// Its group (`"register"`) or its workload name (`"client"`).
    pub name: String,
    /// `"process"` (the system under test) or `"workload"` (the test driver).
    pub role: &'static str,
}

/// One trace event of the run.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Event {
    /// Its order in the run, from 0.
    pub seq: u64,
    /// Simulated time, in milliseconds.
    pub time_ms: u64,
    /// The address of the process that emitted it, or `"sim"` for a fault.
    pub source: String,
    /// The event name (`"write_committed"`, `"sim_fault"` …).
    pub name: String,
    /// Its fields.
    pub fields: BTreeMap<String, Value>,
}

/// One fault that the simulator injected (a `SimFaultEvent`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Fault {
    /// The `seq` of its event.
    pub seq: u64,
    /// Simulated time, in milliseconds.
    pub time_ms: u64,
    /// `SimFaultEvent::kind`: `"partition_created"`, `"process_force_kill"` …
    pub kind: String,
    /// The process or link it hit, when it names one (`ip`, `from`, `to` …).
    pub fields: BTreeMap<String, Value>,
}

/// One message between two processes, from its `msg_send` and `msg_recv` events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Message {
    /// Its id: `2 * rid` for a request, `2 * rid + 1` for its answer.
    pub id: u64,
    /// `"write"`, `"read"`, `"write_ack"` or `"read_reply"`.
    pub kind: String,
    /// The sender's address.
    pub from: String,
    /// The receiver's address.
    pub to: String,
    /// When it left, in simulated milliseconds.
    pub sent_ms: u64,
    /// When it arrived; `None` when it never did.
    pub received_ms: Option<u64>,
}

/// The counts of one assertion in this run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Assertion {
    /// Its message.
    pub msg: String,
    /// `"always"`, `"sometimes"`, `"reachable"` …
    pub kind: String,
    /// How many times it held.
    pub passes: u64,
    /// How many times it did not.
    pub fails: u64,
}

/// The summary of the simulation report for this seed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Summary {
    /// The seed passed: no error, no always-assertion broken.
    pub passed: bool,
    /// The always-assertions that broke, as the report writes them.
    pub violations: Vec<String>,
    /// The simulated time of the last event, in milliseconds.
    pub sim_time_ms: u64,
    /// The first event that broke the scenario's invariant, if one did.
    pub first_violation: Option<Violation>,
}

/// The event where the scenario's invariant broke first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Violation {
    /// The `seq` of the event.
    pub seq: u64,
    /// Simulated time, in milliseconds.
    pub time_ms: u64,
    /// The message of the invariant.
    pub message: String,
}

/// A print of the run: a hash of its events and the count of random draws.
/// One seed always gives the same print.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Fingerprint {
    /// FNV-1a 64 of the events, as 16 hex digits.
    pub events: String,
    /// The draws on the simulation's random stream.
    pub draws: u64,
}

/// What one run recorded: the model of the site's player.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Run {
    /// The version of this model ([`MODEL`]).
    pub model: u32,
    /// The config of the run.
    pub config: RunConfig,
    /// The processes and the workload.
    pub processes: Vec<ProcessInfo>,
    /// Every recorded event, faults included, in order.
    pub events: Vec<Event>,
    /// The faults, in order.
    pub faults: Vec<Fault>,
    /// The messages, in the order they left.
    pub messages: Vec<Message>,
    /// The assertions of the scenario and of the simulator.
    pub assertions: Vec<Assertion>,
    /// The report.
    pub report: Summary,
    /// The print of the run.
    pub fingerprint: Fingerprint,
}

/// An error in a config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError(pub String);

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ConfigError {}

#[derive(Default)]
struct Captured {
    events: Vec<TraceEvent>,
    draws: u64,
}

/// Reads every event of the scenario and each fault, after each step.
struct Recorder {
    names: Vec<&'static str>,
    cursors: Vec<Cell<usize>>,
    out: Arc<Mutex<Captured>>,
}

impl Invariant for Recorder {
    fn name(&self) -> &'static str {
        "site_recorder"
    }

    fn observe(&self, query: &dyn TraceQuery, _sim_time_ms: u64) {
        let mut out = self
            .out
            .lock()
            .expect("Mutex poisoned: prior task panicked");
        for (name, cursor) in self.names.iter().zip(&self.cursors) {
            out.events.extend(query.since(name, cursor));
        }
        out.draws = rng_call_count();
    }

    fn reset(&mut self) {
        self.cursors.iter().for_each(|c| c.set(0));
        *self
            .out
            .lock()
            .expect("Mutex poisoned: prior task panicked") = Captured::default();
    }
}

fn field_json(value: &FieldValue) -> Value {
    match value {
        FieldValue::U64(v) => Value::from(*v),
        FieldValue::I64(v) => Value::from(*v),
        FieldValue::Bool(v) => Value::from(*v),
        FieldValue::F64(v) => serde_json::Number::from_f64(*v).map_or(Value::Null, Value::Number),
        FieldValue::Str(v) => Value::from(v.as_str()),
    }
}

/// `AlwaysOrUnreachable` -> `always_or_unreachable`.
pub(crate) fn snake_case(name: &str) -> String {
    let mut out = String::new();
    for (i, c) in name.chars().enumerate() {
        if c.is_ascii_uppercase() && i > 0 {
            out.push('_');
        }
        out.push(c.to_ascii_lowercase());
    }
    out
}

/// FNV-1a 64 over bytes.
pub(crate) fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3)
    })
}

fn chaos_surfaces(chaos: ChaosSet, servers: usize) -> Vec<Chaos> {
    let mut out = Vec::new();
    if chaos.network {
        out.push(Chaos::Network(ChaosMode::Random));
    }
    if chaos.storage {
        out.push(Chaos::Storage(ChaosMode::Random));
    }
    if chaos.reboots {
        out.push(Chaos::Attrition {
            config: Attrition {
                // A minority at a time, so a quorum can stay up.
                max_dead: (servers - 1) / 2,
                prob_graceful: 0.3,
                prob_crash: 0.7,
                prob_wipe: 0.0,
                recovery_delay_ms: None,
                grace_period_ms: None,
                scope: AttritionScope::PerProcess,
                victims: AttritionVictims::group(SERVER),
                wipe_budget: None,
            },
            mode: ChaosMode::Random,
        });
    }
    out
}

/// The simulation of a config, with no seed and no recorder: the small
/// system, its invariant, its chaos. `run` adds one seed and the recorder;
/// the example binary `sim-register` runs it over many seeds.
///
/// The config must be valid ([`check`]).
#[must_use]
pub fn simulation(config: &RunConfig) -> SimulationBuilder {
    let bug = config.bug;
    let builder = SimulationBuilder::new()
        .processes(ProcessCount::Fixed(config.servers), || {
            Box::new(RegisterServer)
        })
        .workloads(WorkloadCount::Fixed(1), move |_| {
            Box::new(RegisterClient::new(bug))
        })
        .invariant(RegisterInvariant::default())
        .chaos_duration(Duration::from_secs(4));
    let surfaces = chaos_surfaces(config.chaos, config.servers);
    if surfaces.is_empty() {
        builder
    } else {
        builder.enable_chaos(surfaces)
    }
}

/// Check a config: a known scenario and 1 to 5 servers.
///
/// # Errors
///
/// The reason the config is not valid.
pub fn check(config: &RunConfig) -> Result<(), ConfigError> {
    if config.scenario != "register" {
        return Err(ConfigError(format!(
            "unknown scenario {:?}: the one scenario is \"register\"",
            config.scenario
        )));
    }
    if !(1..=5).contains(&config.servers) {
        return Err(ConfigError(format!(
            "servers is {}: it must be 1 to 5",
            config.servers
        )));
    }
    Ok(())
}

/// Run one seed of a scenario and return what it recorded.
///
/// # Errors
///
/// A config with an unknown scenario or a server count out of 1 to 5.
///
/// # Panics
///
/// When the recorder's lock is poisoned, which means an invariant panicked.
pub fn run(config: &RunConfig) -> Result<Run, ConfigError> {
    check(config)?;
    let out = Arc::new(Mutex::new(Captured::default()));
    let mut names: Vec<&'static str> = register::EVENTS.to_vec();
    names.push(SIM_FAULT_EVENT_NAME);
    let recorder = Recorder {
        cursors: names.iter().map(|_| Cell::new(0)).collect(),
        names,
        out: out.clone(),
    };
    let builder = simulation(config)
        .invariant(recorder)
        .set_iterations(1)
        .set_debug_seeds(vec![config.seed]);
    let report = builder
        .run()
        .map_err(|e| ConfigError(format!("the simulation refused the config: {e}")))?;

    let captured = std::mem::take(&mut *out.lock().expect("Mutex poisoned: prior task panicked"));
    let mut raw = captured.events;
    raw.sort_by_key(|e| e.seq);
    let events: Vec<Event> = raw
        .iter()
        .map(|e| Event {
            seq: e.seq,
            time_ms: e.time_ms,
            source: e.source.clone(),
            name: e.name.clone(),
            fields: e
                .fields
                .iter()
                .map(|(k, v)| (k.clone(), field_json(v)))
                .collect(),
        })
        .collect();
    let faults = events
        .iter()
        .filter(|e| e.name == SIM_FAULT_EVENT_NAME)
        .map(|e| {
            let mut fields = e.fields.clone();
            let kind = fields
                .remove("kind")
                .and_then(|k| k.as_str().map(String::from))
                .unwrap_or_default();
            Fault {
                seq: e.seq,
                time_ms: e.time_ms,
                kind,
                fields,
            }
        })
        .collect();
    let messages = pair_messages(&events);
    let processes = (1..=config.servers)
        .map(|i| ProcessInfo {
            ip: format!("10.0.1.{i}"),
            name: SERVER.into(),
            role: "process",
        })
        .chain(std::iter::once(ProcessInfo {
            ip: "10.0.0.1".into(),
            name: CLIENT.into(),
            role: "workload",
        }))
        .collect();
    let assertions = report
        .assertion_details
        .iter()
        .map(|d| Assertion {
            msg: d.msg.clone(),
            kind: snake_case(&format!("{:?}", d.kind)),
            passes: d.pass_count,
            fails: d.fail_count,
        })
        .collect();
    let sim_time_ms = events.iter().map(|e| e.time_ms).max().unwrap_or(0);
    let first_violation = first_violation(&events);
    let print = serde_json::to_vec(&events).unwrap_or_default();
    Ok(Run {
        model: MODEL,
        config: config.clone(),
        processes,
        events,
        faults,
        messages,
        assertions,
        report: Summary {
            passed: report.failed_runs == 0 && report.assertion_violations.is_empty(),
            violations: report.assertion_violations.clone(),
            sim_time_ms,
            first_violation,
        },
        fingerprint: Fingerprint {
            events: format!("{:016x}", fnv1a(&print)),
            draws: captured.draws,
        },
    })
}

/// The first event that breaks the register's invariant, with the rule the
/// invariant uses.
pub(crate) fn first_violation(events: &[Event]) -> Option<Violation> {
    let mut rule = Freshness::default();
    events
        .iter()
        .find(|e| {
            let version = e.fields.get("version").and_then(Value::as_u64).unwrap_or(0);
            !rule.observe(&e.name, version)
        })
        .map(|e| Violation {
            seq: e.seq,
            time_ms: e.time_ms,
            message: INVARIANT.into(),
        })
}

/// Pair each `msg_send` with its `msg_recv` by message id.
pub(crate) fn pair_messages(events: &[Event]) -> Vec<Message> {
    let id = |e: &Event| e.fields.get("msg").and_then(Value::as_u64);
    let text = |e: &Event, k: &str| {
        e.fields
            .get(k)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    let mut received: BTreeMap<u64, u64> = BTreeMap::new();
    for e in events.iter().filter(|e| e.name == "msg_recv") {
        if let Some(m) = id(e) {
            received.entry(m).or_insert(e.time_ms);
        }
    }
    events
        .iter()
        .filter(|e| e.name == "msg_send")
        .filter_map(|e| {
            let m = id(e)?;
            Some(Message {
                id: m,
                kind: text(e, "kind"),
                from: e.source.clone(),
                to: text(e, "to"),
                sent_ms: e.time_ms,
                received_ms: received.get(&m).copied(),
            })
        })
        .collect()
}

/// Run a config given as JSON and return the run as JSON: the entry point of
/// the browser. An error comes back as `{"error": "…"}`.
#[must_use]
pub fn run_json(config: &str) -> String {
    let result = serde_json::from_str::<RunConfig>(config)
        .map_err(|e| ConfigError(format!("bad config: {e}")))
        .and_then(|c| run(&c));
    match result {
        Ok(run) => serde_json::to_string(&run).unwrap_or_else(|e| error_json(&e.to_string())),
        Err(e) => error_json(&e.0),
    }
}

fn error_json(message: &str) -> String {
    serde_json::json!({ "error": message }).to_string()
}

/// The message of the invariant that the planted bug breaks.
#[must_use]
pub fn invariant_message() -> &'static str {
    INVARIANT
}
