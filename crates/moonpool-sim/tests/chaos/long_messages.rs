//! Two assertion sites whose messages share their first 63 bytes stay two
//! sites in the report and in the coverage rule (#464).
//!
//! A slot keeps only the first 63 bytes of its message, but it is identified
//! by the hash of the full message. The report must not merge the two slots.

use async_trait::async_trait;
use moonpool_sim::{SimContext, SimulationBuilder, SimulationResult, Workload};

/// 76 bytes each; the two messages are different only from byte 70.
const PASSES: &str = "the coordinator must not accept a commit when the log is behind: case passes";
const NEVER: &str = "the coordinator must not accept a commit when the log is behind: case never ";

/// One site that passes and one site that never passes.
struct SharedPrefix;

#[async_trait]
impl Workload for SharedPrefix {
    fn name(&self) -> &'static str {
        "client"
    }

    async fn run(&mut self, _ctx: &SimContext) -> SimulationResult<()> {
        moonpool_sim::assert_sometimes!(
            true,
            "the coordinator must not accept a commit when the log is behind: case passes"
        );
        moonpool_sim::assert_sometimes!(
            false,
            "the coordinator must not accept a commit when the log is behind: case never "
        );
        Ok(())
    }
}

#[test]
fn messages_that_share_63_bytes_stay_two_sites() {
    assert!(PASSES.len() >= 64 && NEVER.len() >= 64);
    assert_eq!(PASSES.as_bytes()[..63], NEVER.as_bytes()[..63]);

    let report = SimulationBuilder::new()
        .until_coverage_stable(2, 6)
        .workload(SharedPrefix)
        .run()
        .expect("the run completes");

    let names: Vec<&String> = report
        .assertion_results
        .keys()
        .filter(|name| name.starts_with("the coordinator"))
        .collect();
    assert_eq!(names.len(), 2, "two sites, two results: {names:?}");
    assert!(
        names.iter().all(|name| name.contains("… #")),
        "a cut message ends with … and a hash suffix: {names:?}"
    );
    let never = names
        .iter()
        .find(|name| report.assertion_results[name.as_str()].successes == 0)
        .expect("the site that never passes keeps its own result");
    assert!(
        report
            .coverage_violations
            .iter()
            .any(|violation| violation.contains(never.as_str())),
        "the never-passing site is a coverage violation: {:?}",
        report.coverage_violations
    );

    // The coverage rule does not count the second site as reached.
    let saturation = report.saturation.expect("saturation is reported");
    assert_eq!(saturation.sometimes_total, 2);
    assert_eq!(saturation.sometimes_hit, 1);
    assert!(report.convergence_timeout, "the run cannot cover the site");
}
