//! Reference model and invariant checking for virtual actor simulation testing.
//!
//! Tracks expected balances and operation outcomes for validating actor
//! system correctness using BTreeMap-based deterministic state tracking.

use std::collections::BTreeMap;

/// Record of a failed operation.
#[derive(Debug, Clone)]
pub struct FailedOp {
    /// Name of the operation that failed.
    pub op_name: String,
    /// Reason for failure.
    pub reason: String,
}

/// Reference model for the banking actor system.
///
/// Tracks all operations and expected state to validate actor system
/// invariants (conservation law, balance consistency, etc.).
#[derive(Debug, Default, Clone)]
pub struct ActorRefModel {
    /// Expected balance per actor (ground truth).
    pub balances: BTreeMap<String, i64>,
    /// Total deposited across all actors.
    pub total_deposited: u64,
    /// Total withdrawn across all actors.
    pub total_withdrawn: u64,
    /// Successful operations per actor.
    pub ops_per_actor: BTreeMap<String, u64>,
    /// Failed operations per actor.
    pub failed_ops: BTreeMap<String, Vec<FailedOp>>,
    /// Total operations attempted.
    pub total_ops: u64,
    /// Total successful operations.
    pub successful_ops: u64,
}

impl ActorRefModel {
    /// Create a new empty reference model.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a successful deposit.
    pub fn record_deposit(&mut self, actor_id: &str, amount: u64) {
        *self.balances.entry(actor_id.to_string()).or_insert(0) += amount as i64;
        self.total_deposited += amount;
        *self.ops_per_actor.entry(actor_id.to_string()).or_insert(0) += 1;
        self.total_ops += 1;
        self.successful_ops += 1;
    }

    /// Record a successful withdrawal.
    pub fn record_withdrawal(&mut self, actor_id: &str, amount: u64) {
        *self.balances.entry(actor_id.to_string()).or_insert(0) -= amount as i64;
        self.total_withdrawn += amount;
        *self.ops_per_actor.entry(actor_id.to_string()).or_insert(0) += 1;
        self.total_ops += 1;
        self.successful_ops += 1;
    }

    /// Record a failed operation.
    pub fn record_failure(&mut self, actor_id: &str, op_name: &str, reason: &str) {
        self.failed_ops
            .entry(actor_id.to_string())
            .or_default()
            .push(FailedOp {
                op_name: op_name.to_string(),
                reason: reason.to_string(),
            });
        self.total_ops += 1;
    }

    /// Record a successful balance query (no state change).
    pub fn record_get_balance(&mut self, actor_id: &str) {
        *self.ops_per_actor.entry(actor_id.to_string()).or_insert(0) += 1;
        self.total_ops += 1;
        self.successful_ops += 1;
    }
}

/// Check conservation law: sum of all balances == total_deposited - total_withdrawn.
pub fn check_conservation_law(model: &ActorRefModel) {
    let sum_balances: i64 = model.balances.values().sum();
    let expected = model.total_deposited as i64 - model.total_withdrawn as i64;

    moonpool_sim::assert_always!(
        sum_balances == expected,
        &format!(
            "Conservation law violated: sum(balances)={} != deposited({}) - withdrawn({})",
            sum_balances, model.total_deposited, model.total_withdrawn
        )
    );
}

/// Check that no actor has a negative balance (given our withdraw guards).
pub fn check_balances_non_negative(model: &ActorRefModel) {
    for (actor_id, balance) in &model.balances {
        moonpool_sim::assert_always!(
            *balance >= 0,
            &format!(
                "Balance never negative violated: actor {} has balance {}",
                actor_id, balance
            )
        );
    }
}

/// Check all always-true invariants on the reference model.
pub fn check_always_invariants(model: &ActorRefModel) {
    check_conservation_law(model);
    check_balances_non_negative(model);
}
