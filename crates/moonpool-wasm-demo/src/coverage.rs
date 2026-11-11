//! Coverage over seeds (#418 (technique pages, part 2)): run the seeds of a
//! config one after the other until each check of the register
//! ([`CHECKS`](crate::register::CHECKS)) has fired and the coverage is
//! stable, as `UntilCoverageStable` does, or until a cap. The site's
//! Assertions page plays the result seed by seed.

use serde::{Deserialize, Serialize};

use crate::engine::{ConfigError, RunConfig, run};
use crate::register::CHECKS;

/// What to run: the config of the first seed, the plateau, the cap.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CoverageConfig {
    /// The config of each seed; the seeds are `run.seed`, `run.seed + 1`, …
    pub run: RunConfig,
    /// Seeds in a row with no new check that make the coverage stable,
    /// once each check has fired (`plateau_seeds` of `UntilCoverageStable`).
    pub plateau: usize,
    /// The most seeds to run (`max_iterations`).
    pub max_seeds: usize,
}

impl Default for CoverageConfig {
    fn default() -> Self {
        Self {
            run: RunConfig::default(),
            plateau: 10,
            max_seeds: 100,
        }
    }
}

/// One seed of a coverage run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CoverageSeed {
    /// The seed.
    pub seed: u64,
    /// The seed passed.
    pub passed: bool,
    /// The checks this seed hit, in [`CHECKS`] order.
    pub hit: Vec<String>,
    /// The checks that no earlier seed hit.
    pub new: Vec<String>,
}

/// The result of a coverage run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Coverage {
    /// The config.
    pub config: CoverageConfig,
    /// The checks of the register, in the order of the code.
    pub checks: Vec<String>,
    /// Each seed that ran, in order.
    pub seeds: Vec<CoverageSeed>,
    /// Each check fired and the last `plateau` seeds added nothing.
    pub stable: bool,
    /// The checks that no seed hit: each one is a failure of the campaign.
    pub missing: Vec<String>,
}

/// Run seeds until each check has fired and the coverage is stable, or
/// until `max_seeds`.
///
/// # Errors
///
/// A config that [`run`] refuses.
pub fn coverage(config: &CoverageConfig) -> Result<Coverage, ConfigError> {
    let mut fired: Vec<&str> = Vec::new();
    let mut seeds = Vec::new();
    let mut quiet = 0;
    let mut stable = false;
    for k in 0..config.max_seeds {
        let seed = config.run.seed + k as u64;
        let one = run(&RunConfig {
            seed,
            ..config.run.clone()
        })?;
        let hit: Vec<&str> = CHECKS
            .iter()
            .copied()
            .filter(|c| one.assertions.iter().any(|a| a.msg == *c && a.passes > 0))
            .collect();
        let new: Vec<&str> = hit.iter().copied().filter(|c| !fired.contains(c)).collect();
        fired.extend(&new);
        quiet = if new.is_empty() { quiet + 1 } else { 0 };
        seeds.push(CoverageSeed {
            seed,
            passed: one.report.passed,
            hit: hit.iter().map(|c| (*c).to_string()).collect(),
            new: new.iter().map(|c| (*c).to_string()).collect(),
        });
        if fired.len() == CHECKS.len() && quiet >= config.plateau {
            stable = true;
            break;
        }
    }
    Ok(Coverage {
        config: config.clone(),
        checks: CHECKS.iter().map(|c| (*c).to_string()).collect(),
        seeds,
        stable,
        missing: CHECKS
            .iter()
            .filter(|c| !fired.contains(c))
            .map(|c| (*c).to_string())
            .collect(),
    })
}

/// [`coverage`] with JSON in and JSON out, for the browser. An error comes
/// back as `{"error": "…"}`.
#[must_use]
pub fn coverage_json(config: &str) -> String {
    let result = serde_json::from_str::<CoverageConfig>(config)
        .map_err(|e| ConfigError(format!("bad config: {e}")))
        .and_then(|c| coverage(&c));
    match result {
        Ok(c) => serde_json::to_string(&c)
            .unwrap_or_else(|e| serde_json::json!({ "error": e.to_string() }).to_string()),
        Err(e) => serde_json::json!({ "error": e.0 }).to_string(),
    }
}
