//! A timeline: a root seed, and the chaos steps where an exploring walk
//! drew new randomness.

use std::fmt;
use std::str::FromStr;

/// One branch point: after chaos step `step`, the run draws from `seed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Branch {
    /// The chaos step, from zero, after which the generator is replaced.
    pub step: u64,
    /// The seed of the new generator.
    pub seed: u64,
}

/// A run of a walk: its root seed, and the branch points an exploring
/// campaign added ([`Walk::explore`](crate::Walk::explore)).
///
/// A timeline with no branch is a plain seed. A child of a timeline runs
/// the same steps as its parent up to the branch step, so it starts from
/// the state where its parent found something new; then it draws a new
/// schedule. A timeline replays exactly, like a seed.
///
/// Its text form is the root seed, then `@step:seed` for each branch, in
/// hexadecimal for the branch seeds: `17@120:9e3779b97f4a7c15`. That text
/// is the value of `MOONPOOL_WALK_TIMELINE`.
///
/// ```rust
/// use moonpool_walk::Timeline;
///
/// let timeline: Timeline = "17@120:ff@300:1".parse().expect("a timeline");
/// assert_eq!(timeline.seed, 17);
/// assert_eq!(timeline.branches.len(), 2);
/// assert_eq!(timeline.to_string(), "17@120:ff@300:1");
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct Timeline {
    /// The root seed: it draws the model, the swarm and the first steps.
    pub seed: u64,
    /// The branch points, in increasing step order.
    pub branches: Vec<Branch>,
}

impl Timeline {
    /// The timeline of a plain seed.
    #[must_use]
    pub fn root(seed: u64) -> Self {
        Self {
            seed,
            branches: Vec::new(),
        }
    }

    /// The step of the last branch point, or `None` for a plain seed. A
    /// discovery at or before it was made by an ancestor.
    #[must_use]
    pub fn last_step(&self) -> Option<u64> {
        self.branches.last().map(|branch| branch.step)
    }

    /// The child that branches after chaos step `step`, the `nth` one at
    /// that step. Its seed mixes the parent, the step and `nth`, so two
    /// children never share a schedule.
    ///
    /// # Panics
    ///
    /// When `step` is not after the last branch point: a child only goes
    /// forward.
    #[must_use]
    pub fn child(&self, step: u64, nth: u64) -> Self {
        assert!(
            self.last_step().is_none_or(|last| step > last),
            "a child branches after its parent's last branch point"
        );
        let mut mix = self.seed;
        for branch in &self.branches {
            mix = splitmix(mix ^ branch.seed);
        }
        let seed = splitmix(splitmix(mix ^ step) ^ nth);
        let mut branches = self.branches.clone();
        branches.push(Branch { step, seed });
        Self {
            seed: self.seed,
            branches,
        }
    }
}

/// One `splitmix64` output for `state`.
fn splitmix(state: u64) -> u64 {
    let mut z = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

impl fmt::Display for Timeline {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.seed)?;
        for branch in &self.branches {
            write!(f, "@{}:{:x}", branch.step, branch.seed)?;
        }
        Ok(())
    }
}

impl FromStr for Timeline {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, String> {
        let bad = || format!("{text} is not a timeline (seed, then @step:hexseed for each branch)");
        let mut parts = text.trim().split('@');
        let seed = parts
            .next()
            .and_then(|seed| seed.parse().ok())
            .ok_or_else(bad)?;
        let mut branches: Vec<Branch> = Vec::new();
        for part in parts {
            let (step, branch_seed) = part.split_once(':').ok_or_else(bad)?;
            let step: u64 = step.parse().map_err(|_| bad())?;
            let branch_seed = u64::from_str_radix(branch_seed, 16).map_err(|_| bad())?;
            if branches.last().is_some_and(|last| step <= last.step) {
                return Err(format!("{text}: the branch steps must increase"));
            }
            branches.push(Branch {
                step,
                seed: branch_seed,
            });
        }
        Ok(Self { seed, branches })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_timeline_reads_what_it_writes() {
        let root = Timeline::root(42);
        assert_eq!(root.to_string(), "42");
        let child = root.child(10, 0).child(30, 1);
        let text = child.to_string();
        assert_eq!(text.parse::<Timeline>(), Ok(child));
    }

    #[test]
    fn children_draw_different_seeds() {
        let root = Timeline::root(7);
        let a = root.child(5, 0);
        let b = root.child(5, 1);
        let c = root.child(6, 0);
        assert_ne!(a.branches[0].seed, b.branches[0].seed);
        assert_ne!(a.branches[0].seed, c.branches[0].seed);
        assert_eq!(a, root.child(5, 0), "a child is deterministic");
    }

    #[test]
    fn a_bad_text_is_refused() {
        for text in ["", "x", "1@", "1@2", "1@2:zz", "1@5:1@5:2", "1@5:1@4:2"] {
            assert!(text.parse::<Timeline>().is_err(), "{text}");
        }
    }

    #[test]
    #[should_panic(expected = "after its parent's last branch point")]
    fn a_child_never_goes_back() {
        let _ = Timeline::root(1).child(9, 0).child(9, 0);
    }
}
