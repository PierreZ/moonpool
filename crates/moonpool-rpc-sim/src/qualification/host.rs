//! Host-speed variation: a harness wrapper that makes every poll of a
//! process or workload task cost host time, drawn from nothing the
//! simulation sees (a local poll counter, never the simulation's RNG or
//! clock). A seed must replay the same semantic record whatever the host
//! speed: any decision taken from wall-clock time (a hidden `Instant::now`,
//! an OS timer, a thread race) would show up as a divergence.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use async_trait::async_trait;
use moonpool_sim::{Process, SimContext, SimulationResult, Workload};

/// How much host time each poll costs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum HostJitter {
    /// No added host time.
    #[default]
    Off,
    /// Busy-spin this many iterations on every poll (a slower CPU).
    Spin(u32),
    /// Sleep the OS thread this long on every `every`-th poll (a
    /// descheduled or overloaded host).
    Stall {
        /// Every how many polls.
        every: u64,
        /// For how long.
        sleep: Duration,
    },
}

/// A future whose polls cost host time.
struct Jitter<F> {
    inner: Pin<Box<F>>,
    mode: HostJitter,
    polls: u64,
}

impl<F: Future> Future for Jitter<F> {
    type Output = F::Output;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<F::Output> {
        self.polls += 1;
        match self.mode {
            HostJitter::Off => {}
            HostJitter::Spin(iterations) => {
                for _ in 0..iterations {
                    std::hint::spin_loop();
                }
            }
            HostJitter::Stall { every, sleep } => {
                if every > 0 && self.polls.is_multiple_of(every) {
                    std::thread::sleep(sleep);
                }
            }
        }
        self.inner.as_mut().poll(cx)
    }
}

fn jitter<F: Future>(inner: F, jitter: HostJitter) -> Jitter<F> {
    Jitter {
        inner: Box::pin(inner),
        mode: jitter,
        polls: 0,
    }
}

struct JitteredProcess {
    inner: Box<dyn Process>,
    jitter: HostJitter,
}

#[async_trait]
impl Process for JitteredProcess {
    fn name(&self) -> &str {
        self.inner.name()
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let jitter_mode = self.jitter;
        jitter(self.inner.run(ctx), jitter_mode).await
    }
}

struct JitteredWorkload {
    inner: Box<dyn Workload>,
    jitter: HostJitter,
}

#[async_trait]
impl Workload for JitteredWorkload {
    fn name(&self) -> &str {
        self.inner.name()
    }

    async fn setup(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        self.inner.setup(ctx).await
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let jitter_mode = self.jitter;
        jitter(self.inner.run(ctx), jitter_mode).await
    }

    async fn check(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        self.inner.check(ctx).await
    }
}

/// `inner`, with every poll of its task costing host time.
pub(crate) fn jittered(inner: Box<dyn Process>, jitter: HostJitter) -> Box<dyn Process> {
    if jitter == HostJitter::Off {
        return inner;
    }
    Box::new(JitteredProcess { inner, jitter })
}

/// `inner`, with every poll of its run phase costing host time.
pub(crate) fn jittered_workload(inner: Box<dyn Workload>, jitter: HostJitter) -> Box<dyn Workload> {
    if jitter == HostJitter::Off {
        return inner;
    }
    Box::new(JitteredWorkload { inner, jitter })
}
