//! `SimulationBuilder::live`: a live run advanced in steps is the same run as
//! `SimulationBuilder::run`, and a workload can take commands its caller
//! pushes between two steps.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use moonpool_sim::{
    LiveStatus, Process, RandomProvider, SimContext, SimulationBuilder, SimulationResult,
    TimeProvider, Workload,
};

/// What the run saw, shared with the factories.
type Seen = Arc<Mutex<Vec<(u64, String)>>>;

fn push(seen: &Seen, at: Duration, what: String) {
    let ms = u64::try_from(at.as_millis()).unwrap_or(u64::MAX);
    seen.lock()
        .expect("Mutex poisoned: prior task panicked")
        .push((ms, what));
}

/// A process that beats at random intervals until it is shut down.
struct Beat(Seen);

#[async_trait]
impl Process for Beat {
    fn name(&self) -> &'static str {
        "beat"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        while !ctx.shutdown().is_cancelled() {
            let pause: u64 = ctx.random().random_range(10..90);
            push(&self.0, ctx.time().now(), format!("beat {pause}"));
            if ctx
                .time()
                .sleep(Duration::from_millis(pause))
                .await
                .is_err()
            {
                break;
            }
        }
        Ok(())
    }
}

/// A workload that sleeps random amounts, then takes the commands queued
/// for it, until it reads `stop`.
struct Operator {
    seen: Seen,
    commands: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl Workload for Operator {
    fn name(&self) -> &'static str {
        "operator"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        loop {
            let pause: u64 = ctx.random().random_range(5..50);
            if ctx
                .time()
                .sleep(Duration::from_millis(pause))
                .await
                .is_err()
            {
                return Ok(());
            }
            let taken: Vec<String> = std::mem::take(
                &mut *self
                    .commands
                    .lock()
                    .expect("Mutex poisoned: prior task panicked"),
            );
            for command in taken {
                push(&self.seen, ctx.time().now(), format!("command {command}"));
                if command == "stop" {
                    return Ok(());
                }
            }
        }
    }
}

fn builder(seen: &Seen, commands: &Arc<Mutex<Vec<String>>>) -> SimulationBuilder {
    let beats = seen.clone();
    SimulationBuilder::new()
        .processes(2, move || Box::new(Beat(beats.clone())))
        .workload(Operator {
            seen: seen.clone(),
            commands: commands.clone(),
        })
        .set_iterations(1)
        .set_debug_seeds(vec![42])
}

#[test]
fn a_live_run_is_the_same_run_as_a_whole_run() {
    // The whole run: the stop command is there from the start, and the
    // operator reads it after its first pause.
    let whole: Seen = Arc::default();
    let commands = Arc::new(Mutex::new(vec!["stop".to_string()]));
    builder(&whole, &commands).run().expect("the whole run");

    // The live run, advanced one simulated millisecond at a time.
    let live: Seen = Arc::default();
    let commands = Arc::new(Mutex::new(vec!["stop".to_string()]));
    let mut sim = builder(&live, &commands).live().expect("a live run");
    let clock = sim.clock();
    let mut target = Duration::ZERO;
    let mut steps = 0;
    loop {
        target += Duration::from_millis(1);
        steps += 1;
        match sim.advance(&mut || clock.now() >= target) {
            LiveStatus::Running => assert!(steps < 100_000, "the live run never ends"),
            LiveStatus::Finished(result) => {
                result.expect("the live run passes");
                break;
            }
        }
    }
    assert!(steps > 1, "the live run paused at least once");
    let whole = whole.lock().expect("whole").clone();
    let live = live.lock().expect("live").clone();
    assert!(!whole.is_empty());
    assert_eq!(live, whole, "pausing changes nothing in the run");
}

#[test]
fn a_command_pushed_between_steps_reaches_the_workload() {
    let seen: Seen = Arc::default();
    let commands = Arc::new(Mutex::new(Vec::new()));
    let mut sim = builder(&seen, &commands).live().expect("a live run");
    let clock = sim.clock();

    // Half a simulated second with no command: the run goes on.
    assert!(matches!(
        sim.advance(&mut || clock.now() >= Duration::from_millis(500)),
        LiveStatus::Running
    ));
    assert!(sim.now() >= Duration::from_millis(500));

    commands
        .lock()
        .expect("commands")
        .extend(["hello".to_string(), "stop".to_string()]);
    let status = sim.advance(&mut || false);
    assert!(matches!(status, LiveStatus::Finished(Ok(()))), "{status:?}");
    let seen = seen.lock().expect("seen").clone();
    let hello = seen
        .iter()
        .find(|(_, what)| what == "command hello")
        .expect("the workload read the command");
    assert!(
        hello.0 >= 500,
        "the command arrived after the pause: {hello:?}"
    );
    // Once finished, a live run stays finished.
    assert!(matches!(
        sim.advance(&mut || false),
        LiveStatus::Finished(Ok(()))
    ));
}
