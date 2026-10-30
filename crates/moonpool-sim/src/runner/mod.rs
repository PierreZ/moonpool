//! The runner: configure a campaign of seeds, run it, report it.
//!
//! [`SimulationBuilder`] is the entry point. For each seed the runner builds
//! a fresh [`SimWorld`](crate::SimWorld) and executor, boots the
//! [`Process`] groups, runs the [`Workload`]s, runs the fault injectors
//! during the chaos window, and checks the result. The campaign returns a
//! [`SimulationReport`].
//!
//! | Module | Contents |
//! |--------|----------|
//! | [`builder`] | [`SimulationBuilder`], [`Chaos`], [`ChaosMode`], [`IterationControl`], [`WorkloadCount`] |
//! | [`process`] | [`Process`] (the system under test), [`RebootKind`], [`Attrition`] and its victim filters |
//! | [`workload`] | [`Workload`] (the test driver) |
//! | [`context`] | [`SimContext`], [`SelfCrash`], [`ProviderEnv`] |
//! | [`deployment`] | [`Deployment`]: a workload spawns and retires processes mid-run |
//! | [`membership`] | [`Membership`], [`DomainSpec`], [`DeploymentChanges`]: the live process set |
//! | [`fault_injector`] | [`FaultInjector`], [`FaultContext`], the built-in attrition injector |
//! | [`outage`] | [`Outage`], [`OutageVeto`], [`OutageLanded`] |
//! | [`gray_failure`] | [`GrayFailure`], [`GrayKinds`] |
//! | [`hint`] | The hint sink and [`HintVeto`] |
//! | [`wipe`] | [`WipeBudget`], [`WipeHeal`], [`WipeLedger`]: rolling wipes |
//! | [`groups`] | [`GroupRegistry`]: process groups and their IP ranges |
//! | [`tags`] | [`ProcessTags`], [`TagRegistry`]: round-robin tags |
//! | [`locality`] | [`LocalityConfig`], [`MachineRegistry`]: failure-domain topology |
//! | [`topology`] | [`WorkloadTopology`]: what one process or workload sees of the cluster |
//! | [`app_metrics`] | [`MetricsHandle`]: per-node application metrics |
//! | [`report`] | [`SimulationReport`], [`SimulationMetrics`], [`BugRecipe`](report::BugRecipe) |
//! | [`display`] | The report renderer |

pub mod app_metrics;
pub mod builder;
mod config;
pub mod context;
pub mod deployment;
pub mod display;
pub mod fault_injector;
pub mod gray_failure;
pub mod groups;
pub mod hint;
pub(crate) mod iteration;
pub mod locality;
pub mod membership;
pub(crate) mod metrics;
pub(crate) mod orchestrator;
pub mod outage;
pub mod process;
pub(crate) mod process_manager;
pub mod report;
mod run_loop;
pub mod shard;
pub(crate) mod stall;
pub mod tags;
pub mod topology;
pub(crate) mod wall_clock;
pub mod wipe;
pub mod workload;

// Re-export main types at module level
pub use app_metrics::{INSTANCE_LABEL, MetricsHandle};
pub use builder::{Chaos, ChaosMode, WorkloadCount};
pub use builder::{DEFAULT_MAX_SPAWNS, IterationControl, SimulationBuilder};
pub use context::{ProviderBundle, ProviderEnv, ProviderFactory, SelfCrash, SimContext};
pub use deployment::Deployment;
pub use fault_injector::{FaultContext, FaultInjector};
pub use gray_failure::{GrayFailure, GrayKinds};
pub use groups::GroupRegistry;
pub use hint::{HINT_VETO_KEY, HintVeto, TIMED_REBOOT_LABEL};
pub use locality::{LocalityConfig, MachineRegistry};
pub use membership::{DeploymentChanges, DomainSpec, Membership};
pub use outage::{OUTAGE_STATE_KEY, OUTAGE_VETO_KEY, Outage, OutageLanded, OutageVeto, VETO_RETRY};
pub use process::{Attrition, AttritionScope, AttritionVictims, Process, RebootKind};
pub use report::{SimulationMetrics, SimulationReport};
pub use shard::{
    Aim, Core, Downgrade, SHARD_BOOK_KEY, ShardBook, ShardCounts, ShardEvent, ShardFaults,
    ShardState, ShardView, Shards, replica_ip,
};
pub use tags::{ProcessTags, TagRegistry};
pub use topology::WorkloadTopology;
pub use wipe::{WIPE_LEDGER_KEY, WipeBudget, WipeHeal, WipeLedger};
pub use workload::Workload;
