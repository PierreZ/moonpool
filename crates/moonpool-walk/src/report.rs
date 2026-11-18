//! What a campaign reports: its counts, its coverage, and how it failed.

use std::fmt;

use crate::Stats;

/// A `sometimes!` or `reachable!` site and how many times it passed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Site {
    /// The message of the site, cut to its first 63 bytes.
    pub message: String,
    /// How many times its condition was true, over the whole campaign.
    pub hits: u64,
}

/// The result of a campaign in which every seed passed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    /// How many seeds ran.
    pub seeds: u64,
    /// The counts over every seed.
    pub stats: Stats,
    /// The name of each action of the model, in the order of
    /// [`Stats::actions`].
    pub actions: Vec<String>,
    /// Every `sometimes!` and `reachable!` site the campaign knows, in the
    /// order the campaign met them.
    pub sites: Vec<Site>,
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let stats = &self.stats;
        writeln!(
            f,
            "{} seeds, {} steps: {} delivered, {} dropped, {} duplicated, {} evicted, {} crashes, {} restarts, {} events",
            self.seeds,
            stats.steps,
            stats.delivered,
            stats.dropped,
            stats.duplicated,
            stats.evicted,
            stats.crashes,
            stats.restarts,
            stats.events,
        )?;
        for (name, count) in self.actions.iter().zip(&stats.actions) {
            writeln!(f, "  action {name}: {count}")?;
        }
        for site in &self.sites {
            writeln!(f, "  reached {} times: {}", site.hits, site.message)?;
        }
        Ok(())
    }
}

/// A seed that failed: where, why, and how to run it again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeedFailure {
    /// The seed.
    pub seed: u64,
    /// The step that failed, from zero.
    pub step: u64,
    /// What failed: the message of the panic, the assertion that recorded a
    /// failure, or the liveness claim that did not hold.
    pub message: String,
    /// The last entries of the log, oldest first.
    pub tail: Vec<String>,
    /// The state of the model ([`Model::describe`](crate::Model::describe)).
    pub state: String,
    /// The parts of the walk that this seed's swarm switched off.
    pub swarm: String,
    /// The environment variable that pins this seed, when the walk reads
    /// one.
    pub replay: Option<String>,
}

impl fmt::Display for SeedFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "seed {} failed at step {}: {}",
            self.seed, self.step, self.message
        )?;
        if let Some(replay) = &self.replay {
            writeln!(f, "run it again alone with {replay}")?;
        }
        if !self.swarm.is_empty() {
            writeln!(f, "swarm: {}", self.swarm)?;
        }
        if !self.tail.is_empty() {
            writeln!(f, "the last {} entries of the log:", self.tail.len())?;
            for line in &self.tail {
                writeln!(f, "  {line}")?;
            }
        }
        if !self.state.is_empty() {
            writeln!(f, "the model:\n{}", self.state)?;
        }
        Ok(())
    }
}

/// Why a campaign failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WalkError {
    /// One seed failed. The campaign stops at the first one.
    Seed(Box<SeedFailure>),
    /// Every seed passed, but the campaign did not reach what it must
    /// reach: a `sometimes!` or `reachable!` site that never passed, or an
    /// assertion the table could not count. One line for each.
    Coverage(Vec<String>),
}

impl fmt::Display for WalkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Seed(failure) => failure.fmt(f),
            Self::Coverage(gaps) => {
                writeln!(f, "every seed passed, but the campaign has coverage gaps:")?;
                for gap in gaps {
                    writeln!(f, "  {gap}")?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for WalkError {}
