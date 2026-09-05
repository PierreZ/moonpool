//! The simulation's one random stream.
//!
//! Every random decision a simulation makes — the fault a device injects, the
//! delay a message takes, the task the executor polls next, the branch a
//! `select!` tries first, the per-seed swarm subset, a buggify activation, and
//! every draw a process or workload makes through its `RandomProvider` — comes
//! from the single thread-local [`SIM_RNG`] below. There is no private stream
//! for the framework's own decisions: moonpool draws its randomness exactly the
//! way the code it simulates does, so the same seed replays the same run, bit
//! for bit, and adding or removing a draw anywhere shifts everything after it.
//!
//! The stream is **counted**: every draw increments [`rng_call_count`], and the
//! fork explorer records `count@seed` coordinates and replays them by reseeding
//! at a call count ([`set_rng_breakpoints`]). Because scheduling, swarm masks
//! and select offsets are draws on the same stream, a recipe replays them too.
//!
//! The stream also carries the **determinism canary**
//! ([`begin_determinism_record`] / [`begin_determinism_check`] /
//! [`finish_determinism_check`], `SimulationBuilder::check_determinism`), the
//! mechanism madsim's `Runtime::check_determinism` uses: a run is executed
//! twice with the same seed; the first run records a fingerprint after every
//! draw (a probe of the stream's state after the draw, taken on a *clone* so
//! the check consumes no randomness, mixed with the logical clock), the second
//! compares each fingerprint against the record, and at the end the whole
//! record must have been consumed. Any uncontrolled difference — task
//! scheduling, operation order, the number or order of draws, simulated
//! timing — moves the stream or the clock and trips it.
//!
//! The state is thread-local so parallel test threads never share a stream.

use rand::SeedableRng;
use rand::{
    RngExt,
    distr::{Distribution, StandardUniform, uniform::SampleUniform},
};
use rand_chacha::ChaCha8Rng;
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::fmt;
use std::time::Duration;

thread_local! {
    /// The simulation's random number generator.
    ///
    /// `ChaCha8Rng` for deterministic, reproducible randomness; reseeded by
    /// [`set_sim_seed`] once per simulation.
    static SIM_RNG: RefCell<ChaCha8Rng> = RefCell::new(ChaCha8Rng::seed_from_u64(0));

    /// The seed last installed by [`set_sim_seed`], for error reporting.
    static CURRENT_SEED: RefCell<u64> = const { RefCell::new(0) };

    /// Draws since the last reset, the fork explorer's replay coordinate.
    static RNG_CALL_COUNT: Cell<u64> = const { Cell::new(0) };

    /// Pending `(target_count, new_seed)` breakpoints, sorted by target. When
    /// the call count exceeds `target_count`, the stream reseeds with
    /// `new_seed` and the count restarts at 1.
    static RNG_BREAKPOINTS: RefCell<VecDeque<(u64, u64)>> = const { RefCell::new(VecDeque::new()) };

    /// The per-seed operation-alphabet swarm mask: one bit per `u8` operation
    /// id, drawn once per iteration by [`draw_swarm_op_mask`] from [`SIM_RNG`].
    /// `None` means every operation is enabled (no swarm).
    static SWARM_OP_MASK: Cell<Option<[u64; 4]>> = const { Cell::new(None) };

    /// The logical clock as of the scheduler's last pop, in nanoseconds
    /// (published by [`note_logical_time`]); the time half of a fingerprint.
    static SIM_CLOCK_NANOS: Cell<u64> = const { Cell::new(0) };

    /// The determinism canary (see the module docs).
    static CANARY: RefCell<Canary> = const { RefCell::new(Canary::Off) };
}

/// The determinism canary's state.
enum Canary {
    /// Not checking: draws take no fingerprint.
    Off,
    /// First run: every draw appends its fingerprint.
    Recording(Vec<u64>),
    /// Second run: every draw is compared against the record.
    Checking {
        expected: Vec<u64>,
        /// Fingerprints consumed so far.
        index: usize,
        /// The first mismatch; comparison stops once it is set.
        tripped: Option<DeterminismViolation>,
    },
}

/// Why a determinism check failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeterminismViolation {
    /// Draw number `index` (zero-based) fingerprinted differently from the
    /// recorded run: the stream or the logical clock had already diverged.
    Diverged {
        /// Zero-based index of the first mismatching draw.
        index: usize,
        /// The recorded fingerprint.
        expected: u64,
        /// The fingerprint the replay produced.
        actual: u64,
    },
    /// The replay drew more times than the recorded run.
    ExtraDraws {
        /// Number of draws the first run recorded.
        recorded: usize,
    },
    /// The replay ended early: it consumed a matching prefix of the record
    /// and then stopped drawing.
    Unconsumed {
        /// Number of draws the first run recorded.
        recorded: usize,
        /// Number of draws the replay made.
        consumed: usize,
    },
}

impl fmt::Display for DeterminismViolation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Diverged {
                index,
                expected,
                actual,
            } => write!(
                f,
                "draw {index} fingerprinted {actual:#018x}, the recorded run had {expected:#018x}"
            ),
            Self::ExtraDraws { recorded } => {
                write!(f, "the replay drew more than the {recorded} draws recorded")
            }
            Self::Unconsumed { recorded, consumed } => write!(
                f,
                "the replay stopped after {consumed} of the {recorded} draws recorded"
            ),
        }
    }
}

impl Canary {
    fn observe(&mut self, fingerprint: u64) {
        match self {
            Self::Off => {}
            Self::Recording(log) => log.push(fingerprint),
            Self::Checking {
                expected,
                index,
                tripped,
            } => {
                if tripped.is_some() {
                    return;
                }
                match expected.get(*index) {
                    None => {
                        *tripped = Some(DeterminismViolation::ExtraDraws {
                            recorded: expected.len(),
                        });
                    }
                    Some(&recorded) if recorded != fingerprint => {
                        *tripped = Some(DeterminismViolation::Diverged {
                            index: *index,
                            expected: recorded,
                            actual: fingerprint,
                        });
                    }
                    Some(_) => *index += 1,
                }
            }
        }
    }
}

/// 64-bit `SplitMix64` finalizer: spreads the logical clock over all 64 bits
/// before it is mixed into a fingerprint.
fn mix64(mut x: u64) -> u64 {
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

/// Perform one draw on the stream: count it, fire any breakpoint, run `op`,
/// and — while the canary is armed — fingerprint the state it left behind.
///
/// The fingerprint probes a *clone* of the generator, so arming the canary
/// never consumes simulation randomness: a checked run draws exactly what an
/// unchecked run draws.
fn draw<T>(op: impl FnOnce(&mut ChaCha8Rng) -> T) -> T {
    pre_sample();
    let armed = CANARY.with(|canary| !matches!(*canary.borrow(), Canary::Off));
    SIM_RNG.with(|rng| {
        let mut rng = rng.borrow_mut();
        let result = op(&mut rng);
        if armed {
            let probe: u64 = rng.clone().random();
            let fingerprint = probe ^ mix64(SIM_CLOCK_NANOS.with(Cell::get));
            CANARY.with(|canary| canary.borrow_mut().observe(fingerprint));
        }
        result
    })
}

/// Increment the call counter and fire any breakpoint it crossed.
///
/// Called before every draw.
fn pre_sample() {
    RNG_CALL_COUNT.with(|c| c.set(c.get() + 1));
    check_rng_breakpoint();
}

/// Pop every breakpoint whose target the count has exceeded (using `>`),
/// reseeding the stream for each. The count restarts at 1 because the current
/// draw is the first of the new segment.
fn check_rng_breakpoint() {
    RNG_BREAKPOINTS.with(|bp| {
        let mut breakpoints = bp.borrow_mut();
        while let Some(&(target_count, new_seed)) = breakpoints.front() {
            let count = RNG_CALL_COUNT.with(std::cell::Cell::get);
            if count > target_count {
                breakpoints.pop_front();
                SIM_RNG.with(|rng| {
                    *rng.borrow_mut() = ChaCha8Rng::seed_from_u64(new_seed);
                });
                CURRENT_SEED.with(|s| {
                    *s.borrow_mut() = new_seed;
                });
                RNG_CALL_COUNT.with(|c| c.set(1));
            } else {
                break;
            }
        }
    });
}

/// Draw a value from the simulation stream.
///
/// The same seed always yields the same sequence of draws on one thread.
#[must_use]
pub fn sim_random<T>() -> T
where
    StandardUniform: Distribution<T>,
{
    draw(|rng| rng.sample(StandardUniform))
}

/// Draw a value in `range` (exclusive upper bound) from the simulation stream.
pub fn sim_random_range<T>(range: std::ops::Range<T>) -> T
where
    T: SampleUniform + PartialOrd,
{
    draw(|rng| rng.random_range(range))
}

/// Draw a value in `range`, or return `range.start` without drawing when the
/// range is empty.
pub fn sim_random_range_or_default<T>(range: std::ops::Range<T>) -> T
where
    T: SampleUniform + PartialOrd + Clone,
{
    if range.start >= range.end {
        range.start
    } else {
        sim_random_range(range)
    }
}

/// Draw an `f64` in `[0.0, 1.0)` from the simulation stream.
///
/// Matches FDB's `deterministicRandom()->random01()`.
#[must_use]
pub fn sim_random_f64() -> f64 {
    draw(|rng| rng.sample(StandardUniform))
}

/// Return `true` with probability `p`, drawn from the simulation stream.
///
/// Always consumes exactly one draw regardless of the outcome, so a sequence
/// of coin flips has a fixed call-count footprint.
#[must_use]
pub fn sim_random_bool(p: f64) -> bool {
    sim_random_f64() < p
}

/// Seed the simulation stream.
///
/// The same seed always produces the same sequence of draws.
pub fn set_sim_seed(seed: u64) {
    SIM_RNG.with(|rng| {
        *rng.borrow_mut() = ChaCha8Rng::seed_from_u64(seed);
    });
    CURRENT_SEED.with(|current| {
        *current.borrow_mut() = seed;
    });
}

/// The seed last installed by [`set_sim_seed`] (0 if none), for error reports.
#[must_use]
pub fn current_sim_seed() -> u64 {
    CURRENT_SEED.with(|current| *current.borrow())
}

/// Reset the stream, the call count, the breakpoints, the swarm mask and the
/// logical clock.
///
/// Call before [`set_sim_seed`] so nothing carries over between consecutive
/// simulations on one thread. The determinism canary is deliberately left
/// alone: it spans the two runs of one seed, each of which begins with this
/// reset.
pub fn reset_sim_rng() {
    SIM_RNG.with(|rng| {
        *rng.borrow_mut() = ChaCha8Rng::seed_from_u64(0);
    });
    CURRENT_SEED.with(|current| {
        *current.borrow_mut() = 0;
    });
    RNG_CALL_COUNT.with(|c| c.set(0));
    RNG_BREAKPOINTS.with(|bp| bp.borrow_mut().clear());
    SWARM_OP_MASK.with(|mask| mask.set(None));
    SIM_CLOCK_NANOS.with(|clock| clock.set(0));
}

/// Draws since the last seed set or reset — the fork explorer's coordinate.
#[must_use]
pub fn rng_call_count() -> u64 {
    RNG_CALL_COUNT.with(std::cell::Cell::get)
}

/// Restart the call count at zero (a new counting segment after a reseed).
pub fn reset_rng_call_count() {
    RNG_CALL_COUNT.with(|c| c.set(0));
}

/// Install replay breakpoints: `(target_count, new_seed)` pairs sorted by
/// target. When the call count exceeds a target, the stream reseeds with the
/// paired seed and the count restarts at 1.
pub fn set_rng_breakpoints(breakpoints: Vec<(u64, u64)>) {
    RNG_BREAKPOINTS.with(|bp| {
        *bp.borrow_mut() = VecDeque::from(breakpoints);
    });
}

/// Clear all replay breakpoints.
pub fn clear_rng_breakpoints() {
    RNG_BREAKPOINTS.with(|bp| bp.borrow_mut().clear());
}

/// Route `moonpool_core::select!` branch offsets through the simulation
/// stream on this thread.
///
/// The runner installs this once per iteration; every `select!` execution is
/// then one counted draw, so branch polling order replays with the seed and
/// with an exploration recipe.
pub fn install_select_offset() {
    moonpool_core::select_support::set_select_offset_override(Some(select_offset_from_stream));
}

/// Uninstall the simulation stream as the `select!` offset source, restoring
/// moonpool-core's entropy fallback (production behavior).
pub fn uninstall_select_offset() {
    moonpool_core::select_support::set_select_offset_override(None);
}

/// The offset source registered with moonpool-core: one draw per `select!`.
fn select_offset_from_stream(branches: u32) -> u32 {
    sim_random_range(0..branches)
}

/// Publish the logical clock: the scheduler calls this whenever it advances
/// time, so a fingerprint can mix in *when* a draw happened.
pub(crate) fn note_logical_time(now: Duration) {
    let nanos = u64::try_from(now.as_nanos()).unwrap_or(u64::MAX);
    SIM_CLOCK_NANOS.with(|clock| clock.set(nanos));
}

/// Arm the determinism canary for the recording run: from now on every draw
/// appends its fingerprint to a fresh record (any earlier record is dropped).
pub fn begin_determinism_record() {
    CANARY.with(|canary| *canary.borrow_mut() = Canary::Recording(Vec::new()));
}

/// Switch the canary from recording to checking: the next draws are compared,
/// one by one, against the record the recording run built.
///
/// # Panics
///
/// Panics if no recording run preceded it — a programmer error in the runner,
/// never an operating condition.
pub fn begin_determinism_check() {
    CANARY.with(|canary| {
        let mut canary = canary.borrow_mut();
        let Canary::Recording(expected) = std::mem::replace(&mut *canary, Canary::Off) else {
            panic!("begin_determinism_check called without a recording run");
        };
        *canary = Canary::Checking {
            expected,
            index: 0,
            tripped: None,
        };
    });
}

/// Disarm the canary and report on the checking run.
///
/// `Ok(n)` means the replay produced exactly the `n` recorded fingerprints in
/// order — and *all* of them: a replay that matched a prefix and then stopped
/// drawing fails with [`DeterminismViolation::Unconsumed`], so an early exit
/// is caught as surely as a divergence.
///
/// # Errors
///
/// The first divergence, an extra draw, or an unconsumed tail.
///
/// # Panics
///
/// Panics if no checking run is in progress — a programmer error in the
/// runner, never an operating condition.
pub fn finish_determinism_check() -> Result<usize, DeterminismViolation> {
    CANARY.with(|canary| {
        let Canary::Checking {
            expected,
            index,
            tripped,
        } = canary.replace(Canary::Off)
        else {
            panic!("finish_determinism_check called without a checking run");
        };
        if let Some(violation) = tripped {
            return Err(violation);
        }
        if index < expected.len() {
            return Err(DeterminismViolation::Unconsumed {
                recorded: expected.len(),
                consumed: index,
            });
        }
        Ok(index)
    })
}

/// Disarm the canary without a verdict (an aborted run).
pub fn stop_determinism_canary() {
    CANARY.with(|canary| *canary.borrow_mut() = Canary::Off);
}

/// Draw this iteration's operation-alphabet swarm mask from the simulation
/// stream: 256 bits, one per `u8` operation id, each independently on with
/// probability 0.5.
///
/// The runner calls this once per iteration when `.swarm_operations()` is
/// enabled, right after seeding, so the mask sits at a fixed call-count
/// position. Consumes exactly four draws.
pub fn draw_swarm_op_mask() {
    let mask = [
        sim_random::<u64>(),
        sim_random::<u64>(),
        sim_random::<u64>(),
        sim_random::<u64>(),
    ];
    SWARM_OP_MASK.with(|cell| cell.set(Some(mask)));
}

/// Clear the swarm mask: [`swarm_op_enabled`] reports every operation enabled
/// (the full alphabet — zero behavior change).
pub fn clear_swarm_op_mask() {
    SWARM_OP_MASK.with(|cell| cell.set(None));
}

/// Report whether operation `op_id` is enabled by this iteration's
/// operation-alphabet swarm mask.
///
/// Without a mask ([`draw_swarm_op_mask`] not called since the last reset or
/// [`clear_swarm_op_mask`]), every operation is enabled. With one, the answer
/// is a bit of the mask drawn once at the start of the iteration: querying
/// consumes no randomness, so a workload may ask any number of times in any
/// order and always sees the same subset.
///
/// Callers own the empty-subset fallback: if a seed disables every operation in
/// the alphabet, the workload should fall back to the full alphabet so it always
/// has something to do.
#[must_use]
pub fn swarm_op_enabled(op_id: u8) -> bool {
    match SWARM_OP_MASK.with(Cell::get) {
        None => true,
        Some(mask) => {
            let word = mask[usize::from(op_id / 64)];
            word & (1 << (op_id % 64)) != 0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Assert two f64 values are bit-identical.
    fn assert_f64_eq(left: f64, right: f64) {
        assert_eq!(left.to_bits(), right.to_bits(), "{left} != {right}");
    }

    /// Assert two f64 values are bit-different.
    fn assert_f64_ne(left: f64, right: f64) {
        assert_ne!(left.to_bits(), right.to_bits(), "{left} == {right}");
    }

    #[test]
    fn test_deterministic_randomness() {
        set_sim_seed(42);
        let value1: f64 = sim_random();
        let value2: u32 = sim_random();
        let value3: bool = sim_random();

        set_sim_seed(42);
        assert_f64_eq(value1, sim_random::<f64>());
        assert_eq!(value2, sim_random::<u32>());
        assert_eq!(value3, sim_random::<bool>());
    }

    #[test]
    fn test_different_seeds_produce_different_values() {
        set_sim_seed(1);
        let value1_seed1: f64 = sim_random();
        let value2_seed1: f64 = sim_random();

        set_sim_seed(2);
        let value1_seed2: f64 = sim_random();
        let value2_seed2: f64 = sim_random();

        assert_f64_ne(value1_seed1, value1_seed2);
        assert_f64_ne(value2_seed1, value2_seed2);
    }

    #[test]
    fn test_sim_random_range() {
        set_sim_seed(42);

        for _ in 0..100 {
            let value = sim_random_range(10..20);
            assert!(value >= 10);
            assert!(value < 20);
        }

        for _ in 0..100 {
            let value = sim_random_range(0.0..1.0);
            assert!(value >= 0.0);
            assert!(value < 1.0);
        }
    }

    #[test]
    fn test_range_determinism() {
        set_sim_seed(123);
        let value1 = sim_random_range(100..1000);
        let value2 = sim_random_range(0.0..10.0);

        set_sim_seed(123);
        assert_eq!(value1, sim_random_range(100..1000));
        assert_f64_eq(value2, sim_random_range(0.0..10.0));
    }

    #[test]
    fn test_reset_clears_state() {
        set_sim_seed(42);
        let _advance1: f64 = sim_random();
        let _advance2: f64 = sim_random();
        let after_advance: f64 = sim_random();

        reset_sim_rng();
        set_sim_seed(42);
        let first_value: f64 = sim_random();

        assert_f64_ne(after_advance, first_value);
    }

    #[test]
    fn test_sequence_persistence_within_thread() {
        set_sim_seed(42);
        let value1: f64 = sim_random();
        let value2: f64 = sim_random();
        let value3: f64 = sim_random();

        set_sim_seed(42);
        assert_f64_eq(value1, sim_random::<f64>());
        assert_f64_eq(value2, sim_random::<f64>());
        assert_f64_eq(value3, sim_random::<f64>());
    }

    #[test]
    fn test_multiple_resets_and_seeds() {
        for seed in [1, 42, 12345] {
            reset_sim_rng();
            set_sim_seed(seed);
            let first: f64 = sim_random();

            reset_sim_rng();
            set_sim_seed(seed);
            assert_f64_eq(first, sim_random::<f64>());
        }
    }

    #[test]
    fn test_current_sim_seed() {
        set_sim_seed(12345);
        assert_eq!(current_sim_seed(), 12345);

        set_sim_seed(98765);
        assert_eq!(current_sim_seed(), 98765);

        reset_sim_rng();
        assert_eq!(current_sim_seed(), 0);
    }

    #[test]
    fn test_call_counting() {
        reset_sim_rng();
        set_sim_seed(42);
        assert_eq!(rng_call_count(), 0);

        let _: f64 = sim_random();
        assert_eq!(rng_call_count(), 1);

        let _: u32 = sim_random();
        assert_eq!(rng_call_count(), 2);

        let _ = sim_random_range(0..100);
        assert_eq!(rng_call_count(), 3);

        let _ = sim_random_f64();
        assert_eq!(rng_call_count(), 4);

        // Exactly one draw, whatever the outcome.
        let _ = sim_random_bool(0.5);
        assert_eq!(rng_call_count(), 5);

        // sim_random_range_or_default with a valid range delegates to sim_random_range
        let _ = sim_random_range_or_default(0..100);
        assert_eq!(rng_call_count(), 6);

        // sim_random_range_or_default with an empty range does NOT draw
        let _ = sim_random_range_or_default(100..100);
        assert_eq!(rng_call_count(), 6);
    }

    #[test]
    fn test_breakpoint_reseed() {
        reset_sim_rng();
        set_sim_seed(100);

        let mut old_values = Vec::new();
        for _ in 0..5 {
            old_values.push(sim_random::<f64>());
        }

        reset_sim_rng();
        set_sim_seed(200);
        let new_seed_first: f64 = sim_random();

        // Replay: seed 100, breakpoint at count=5 to reseed to 200
        reset_sim_rng();
        set_sim_seed(100);
        set_rng_breakpoints(vec![(5, 200)]);

        for (i, expected) in old_values.iter().enumerate() {
            let actual: f64 = sim_random();
            assert_eq!(
                expected.to_bits(),
                actual.to_bits(),
                "Mismatch at call {}",
                i + 1
            );
        }

        // Call 6 triggers breakpoint (count 6 > 5), reseeds to 200
        let after_breakpoint: f64 = sim_random();
        assert_f64_eq(after_breakpoint, new_seed_first);
        assert_eq!(rng_call_count(), 1);
        assert_eq!(current_sim_seed(), 200);
    }

    #[test]
    fn test_chained_breakpoints() {
        reset_sim_rng();
        set_sim_seed(10);
        set_rng_breakpoints(vec![(3, 20), (2, 30)]);

        let _: f64 = sim_random(); // count=1
        let _: f64 = sim_random(); // count=2
        let _: f64 = sim_random(); // count=3
        assert_eq!(current_sim_seed(), 10);

        // Call 4: count becomes 4 > 3, breakpoint fires: reseed to 20, count=1
        let _: f64 = sim_random();
        assert_eq!(current_sim_seed(), 20);
        assert_eq!(rng_call_count(), 1);

        let _: f64 = sim_random(); // count=2

        // Call 3 of seed 20: count becomes 3 > 2, breakpoint fires: reseed to 30, count=1
        let _: f64 = sim_random();
        assert_eq!(current_sim_seed(), 30);
        assert_eq!(rng_call_count(), 1);
    }

    #[test]
    fn test_replay_determinism() {
        // Run 1: record a "recipe" — seed 42, fork at call 3 to seed 99
        reset_sim_rng();
        set_sim_seed(42);
        let _: f64 = sim_random();
        let _: f64 = sim_random();
        let _: f64 = sim_random();
        let fork_count = rng_call_count();
        set_sim_seed(99);
        reset_rng_call_count();
        let post_fork_1: f64 = sim_random();
        let post_fork_2: f64 = sim_random();

        // Run 2: replay using breakpoints
        reset_sim_rng();
        set_sim_seed(42);
        set_rng_breakpoints(vec![(fork_count, 99)]);
        let _: f64 = sim_random();
        let _: f64 = sim_random();
        let _: f64 = sim_random();
        let replay_1: f64 = sim_random();
        let replay_2: f64 = sim_random();

        assert_f64_eq(post_fork_1, replay_1);
        assert_f64_eq(post_fork_2, replay_2);
    }

    #[test]
    fn test_reset_clears_everything_including_breakpoints() {
        set_sim_seed(42);
        let _: f64 = sim_random();
        let _: f64 = sim_random();
        set_rng_breakpoints(vec![(10, 99)]);
        draw_swarm_op_mask();

        assert_eq!(rng_call_count(), 6);

        reset_sim_rng();

        assert_eq!(rng_call_count(), 0);
        assert_eq!(current_sim_seed(), 0);
        assert!(
            (0..=u8::MAX).all(swarm_op_enabled),
            "reset must clear the swarm mask"
        );

        set_sim_seed(42);
        let _: f64 = sim_random();
        assert_eq!(rng_call_count(), 1);
        assert_eq!(current_sim_seed(), 42); // no breakpoint triggered
    }

    #[test]
    fn select_offsets_are_counted_draws_on_the_sim_stream() {
        reset_sim_rng();
        set_sim_seed(42);
        let offsets_a: Vec<u32> = (0..5).map(|_| select_offset_from_stream(8)).collect();
        assert_eq!(rng_call_count(), 5, "one counted draw per select!");
        assert!(offsets_a.iter().all(|&o| o < 8));

        reset_sim_rng();
        set_sim_seed(42);
        let offsets_b: Vec<u32> = (0..5).map(|_| select_offset_from_stream(8)).collect();
        assert_eq!(offsets_a, offsets_b, "same seed, same offset stream");
        assert!(
            offsets_a.iter().any(|&o| o != offsets_a[0]),
            "offset stream should vary"
        );
    }

    #[test]
    fn swarm_op_disabled_enables_full_alphabet() {
        clear_swarm_op_mask();
        for op in 0..32u8 {
            assert!(
                swarm_op_enabled(op),
                "op {op} must be enabled when swarm is off"
            );
        }
    }

    #[test]
    fn swarm_op_mask_is_drawn_once_and_idempotent() {
        const N: u8 = 16;
        reset_sim_rng();
        set_sim_seed(123);
        draw_swarm_op_mask();
        assert_eq!(rng_call_count(), 4, "the mask is exactly four draws");
        let forward: Vec<bool> = (0..N).map(swarm_op_enabled).collect();

        // Re-query in reverse, with repeats: querying draws nothing and
        // yields the identical mask regardless of call order or count.
        let mut reverse = vec![false; usize::from(N)];
        for op in (0..N).rev() {
            let _ = swarm_op_enabled(op);
            reverse[usize::from(op)] = swarm_op_enabled(op);
        }
        assert_eq!(
            forward, reverse,
            "mask must be idempotent and order-independent"
        );
        assert_eq!(rng_call_count(), 4, "queries must not draw");

        // Same seed, same mask.
        reset_sim_rng();
        set_sim_seed(123);
        draw_swarm_op_mask();
        let again: Vec<bool> = (0..N).map(swarm_op_enabled).collect();
        assert_eq!(forward, again);
        clear_swarm_op_mask();
    }

    #[test]
    fn swarm_op_varies_across_seeds_and_reaches_extremes() {
        const N: u8 = 10;
        let mut min_enabled = usize::MAX;
        let mut max_enabled = 0usize;
        for seed in 0..4000u64 {
            reset_sim_rng();
            set_sim_seed(seed);
            draw_swarm_op_mask();
            let count = (0..N).filter(|&op| swarm_op_enabled(op)).count();
            min_enabled = min_enabled.min(count);
            max_enabled = max_enabled.max(count);
        }
        // Spread across the alphabet: some seeds yield a near-empty subset,
        // others the full alphabet.
        assert!(
            min_enabled <= 1,
            "expected a near-empty subset; min was {min_enabled}"
        );
        assert_eq!(
            max_enabled,
            usize::from(N),
            "expected a full subset; max was {max_enabled}"
        );
        clear_swarm_op_mask();
    }

    #[test]
    fn swarm_op_mask_covers_the_whole_u8_alphabet() {
        reset_sim_rng();
        set_sim_seed(9);
        draw_swarm_op_mask();
        let enabled = (0..=u8::MAX).filter(|&op| swarm_op_enabled(op)).count();
        assert!(
            enabled > 64 && enabled < 192,
            "half-ish of 256, got {enabled}"
        );
        clear_swarm_op_mask();
    }

    /// Seed the stream and draw `n` times, advancing the clock every other draw.
    fn draw_sequence(seed: u64, n: usize) {
        reset_sim_rng();
        set_sim_seed(seed);
        for i in 0..u64::try_from(n).expect("small") {
            if i % 2 == 1 {
                note_logical_time(Duration::from_millis(i));
            }
            let _: u64 = sim_random();
        }
    }

    #[test]
    fn canary_accepts_an_identical_replay_and_consumes_nothing() {
        begin_determinism_record();
        draw_sequence(7, 20);
        let recorded_last: u64 = sim_random();
        begin_determinism_check();
        draw_sequence(7, 20);
        let replayed_last: u64 = sim_random();
        assert_eq!(finish_determinism_check(), Ok(21));
        // Fingerprinting probes a clone: the checked stream is the plain one.
        reset_sim_rng();
        set_sim_seed(7);
        for _ in 0..20 {
            let _: u64 = sim_random();
        }
        let plain_last: u64 = sim_random();
        assert_eq!(recorded_last, plain_last);
        assert_eq!(replayed_last, plain_last);
    }

    #[test]
    fn canary_trips_on_a_diverging_stream() {
        begin_determinism_record();
        draw_sequence(7, 10);
        begin_determinism_check();
        draw_sequence(8, 10);
        match finish_determinism_check() {
            Err(DeterminismViolation::Diverged { index: 0, .. }) => {}
            other => panic!("expected a divergence at draw 0, got {other:?}"),
        }
    }

    #[test]
    fn canary_trips_when_only_the_clock_differs() {
        begin_determinism_record();
        draw_sequence(7, 10);
        begin_determinism_check();
        reset_sim_rng();
        set_sim_seed(7);
        for i in 0..10u64 {
            // Same draws, one clock tick later than the recording run.
            note_logical_time(Duration::from_millis(i + 1));
            let _: u64 = sim_random();
        }
        match finish_determinism_check() {
            Err(DeterminismViolation::Diverged { .. }) => {}
            other => panic!("expected a divergence, got {other:?}"),
        }
    }

    #[test]
    fn canary_trips_on_extra_draws() {
        begin_determinism_record();
        draw_sequence(7, 10);
        begin_determinism_check();
        draw_sequence(7, 11);
        assert_eq!(
            finish_determinism_check(),
            Err(DeterminismViolation::ExtraDraws { recorded: 10 })
        );
    }

    #[test]
    fn canary_trips_on_an_early_exit_after_a_matching_prefix() {
        begin_determinism_record();
        draw_sequence(7, 10);
        begin_determinism_check();
        draw_sequence(7, 6);
        assert_eq!(
            finish_determinism_check(),
            Err(DeterminismViolation::Unconsumed {
                recorded: 10,
                consumed: 6
            })
        );
    }

    /// An extra draw is invisible to the fingerprint until the clock or the
    /// draw count exposes it: the stream's fifth state is the fifth state
    /// whichever call reached it. Here the extra draw at step 4 shares its
    /// clock with the recorded fifth draw, so the divergence is reported at
    /// the next clock tick (draw 5) — and the verdict is the *first* mismatch,
    /// not the last.
    #[test]
    fn canary_reports_the_first_mismatch_only() {
        begin_determinism_record();
        draw_sequence(7, 10);
        begin_determinism_check();
        reset_sim_rng();
        set_sim_seed(7);
        for i in 0..10u64 {
            if i % 2 == 1 {
                note_logical_time(Duration::from_millis(i));
            }
            if i == 4 {
                // One extra draw shifts everything after it.
                let _: u64 = sim_random();
            }
            let _: u64 = sim_random();
        }
        match finish_determinism_check() {
            Err(DeterminismViolation::Diverged { index: 5, .. }) => {}
            other => panic!("expected a divergence at draw 5, got {other:?}"),
        }
    }

    #[test]
    fn canary_off_by_default_and_after_finish() {
        stop_determinism_canary();
        draw_sequence(3, 5);
        assert!(CANARY.with(|c| matches!(*c.borrow(), Canary::Off)));
        begin_determinism_record();
        draw_sequence(3, 5);
        begin_determinism_check();
        draw_sequence(3, 5);
        assert_eq!(finish_determinism_check(), Ok(5));
        assert!(CANARY.with(|c| matches!(*c.borrow(), Canary::Off)));
    }
}
