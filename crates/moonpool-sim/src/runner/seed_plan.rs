//! A per-seed plan the simulation's own code draws: a group's process count,
//! its attrition budget, whether an outage strikes, and values the run reads.
//!
//! A `.processes()` count is a range, and an attrition's `max_dead` is one
//! number for every seed. A campaign that draws a deployment shape needs the
//! two to follow one draw: a larger cell on a share of the seeds, with a
//! dead budget equal to the number of machines that cell promises to lose.
//! [`SimulationBuilder::seed_plan`](super::SimulationBuilder::seed_plan)
//! registers a hook that draws that shape once per seed, from the simulation
//! stream, before the groups resolve.

use std::collections::{BTreeMap, BTreeSet};

/// What a [`SimulationBuilder::seed_plan`](super::SimulationBuilder::seed_plan)
/// hook decides for one seed. Every field starts empty: a group the plan does
/// not name keeps its own count draw, its regimes' `max_dead` and its
/// outages.
#[derive(Debug, Default, Clone)]
pub struct SeedPlan {
    counts: BTreeMap<String, usize>,
    max_dead: BTreeMap<String, usize>,
    calm: BTreeSet<String>,
    values: BTreeMap<String, u64>,
}

impl SeedPlan {
    /// Run `group` with exactly `count` processes on this seed, in place of
    /// the count its `.processes()` registration draws. The group must be a
    /// `.processes()` group: a `.cluster()` group's topology owns its count.
    pub fn set_count(&mut self, group: impl Into<String>, count: usize) {
        self.counts.insert(group.into(), count);
    }

    /// Give every attrition regime whose victims are the group `group`
    /// ([`AttritionVictims::group`](super::AttritionVictims::group)) a
    /// `max_dead` of `max_dead` on this seed. The value replaces the
    /// configured base before `ChaosMode::Swarm` draws its regime, so a swarm
    /// seed may still draw the never-reboot regime (`max_dead = 0`).
    pub fn set_max_dead(&mut self, group: impl Into<String>, max_dead: usize) {
        self.max_dead.insert(group.into(), max_dead);
    }

    /// Strike no outage that names `group` on this seed
    /// ([`Outage::groups`](super::Outage::groups)).
    pub fn calm(&mut self, group: impl Into<String>) {
        self.calm.insert(group.into());
    }

    /// Publish `value` under `key` in the run's
    /// [`StateHandle`](crate::StateHandle), before any process or workload
    /// starts, so the run reads what the plan drew (`state.get::<u64>(key)`).
    pub fn publish(&mut self, key: impl Into<String>, value: u64) {
        self.values.insert(key.into(), value);
    }

    /// The count this plan sets for `group`, if any.
    pub(crate) fn count(&self, group: &str) -> Option<usize> {
        self.counts.get(group).copied()
    }

    /// The `max_dead` this plan sets for the victims of an attrition regime,
    /// if they are a group the plan names.
    pub(crate) fn max_dead(&self, victims: &super::AttritionVictims) -> Option<usize> {
        match victims {
            super::AttritionVictims::Group(group) => self.max_dead.get(group).copied(),
            super::AttritionVictims::Any | super::AttritionVictims::Tagged { .. } => None,
        }
    }

    /// Whether this plan keeps an outage over `groups`.
    pub(crate) fn strikes(&self, groups: &[String]) -> bool {
        !groups.iter().any(|group| self.calm.contains(group))
    }

    /// The values to publish, in key order.
    pub(crate) fn values(&self) -> impl Iterator<Item = (&str, u64)> {
        self.values
            .iter()
            .map(|(key, value)| (key.as_str(), *value))
    }

    /// The groups whose count this plan sets.
    pub(crate) fn counted_groups(&self) -> impl Iterator<Item = &str> {
        self.counts.keys().map(String::as_str)
    }
}

/// A hook that draws one seed's [`SeedPlan`].
pub(crate) type SeedPlanHook = Box<dyn Fn(&mut SeedPlan) + 'static>;

#[cfg(test)]
mod tests {
    use super::SeedPlan;
    use crate::runner::AttritionVictims;

    #[test]
    fn an_empty_plan_changes_nothing() {
        let plan = SeedPlan::default();
        assert_eq!(plan.count("machines"), None);
        assert_eq!(plan.max_dead(&AttritionVictims::group("machines")), None);
        assert!(plan.strikes(&["machines".to_string()]));
        assert_eq!(plan.values().count(), 0);
    }

    #[test]
    fn a_plan_names_its_groups_only() {
        let mut plan = SeedPlan::default();
        plan.set_count("machines", 7);
        plan.set_max_dead("machines", 2);
        plan.calm("acceptors");
        plan.publish("survive", 2);
        assert_eq!(plan.count("machines"), Some(7));
        assert_eq!(plan.count("acceptors"), None);
        assert_eq!(plan.max_dead(&AttritionVictims::group("machines")), Some(2));
        assert_eq!(plan.max_dead(&AttritionVictims::Any), None);
        assert_eq!(plan.max_dead(&AttritionVictims::group("acceptors")), None);
        assert!(!plan.strikes(&["acceptors".to_string(), "proxies".to_string()]));
        assert!(plan.strikes(&["machines".to_string()]));
        assert_eq!(plan.values().collect::<Vec<_>>(), vec![("survive", 2)]);
    }
}
