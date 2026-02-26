//! Reference model and invariants for banking actor simulation.
//!
//! The `BankingModel` tracks expected balances and aggregate totals.
//! Invariants validate conservation laws and balance constraints.

use std::collections::BTreeMap;

use moonpool_sim::{Invariant, StateHandle, assert_always};

/// Key used to publish the reference model into `StateHandle`.
pub const BANKING_MODEL_KEY: &str = "banking_model";

/// Reference model for banking operations.
///
/// Tracks expected balances alongside total deposits and withdrawals
/// for conservation law verification.
#[derive(Debug, Clone, Default)]
pub struct BankingModel {
    /// Current expected balance for each account.
    pub balances: BTreeMap<String, i64>,
    /// Total amount successfully deposited across all accounts.
    pub total_deposited: i64,
    /// Total amount successfully withdrawn across all accounts.
    pub total_withdrawn: i64,
}

impl BankingModel {
    /// Apply a deposit to the model.
    pub fn deposit(&mut self, account: &str, amount: i64) {
        *self.balances.entry(account.to_string()).or_insert(0) += amount;
        self.total_deposited += amount;
    }

    /// Apply a withdrawal to the model. Returns true if successful.
    pub fn withdraw(&mut self, account: &str, amount: i64) -> bool {
        let balance = self.balances.entry(account.to_string()).or_insert(0);
        if *balance >= amount {
            *balance -= amount;
            self.total_withdrawn += amount;
            true
        } else {
            false
        }
    }

    /// Get the balance of an account.
    pub fn balance(&self, account: &str) -> i64 {
        self.balances.get(account).copied().unwrap_or(0)
    }

    /// Sum of all account balances.
    pub fn total_balance(&self) -> i64 {
        self.balances.values().sum()
    }
}

/// Conservation law invariant: sum(balances) == total_deposited - total_withdrawn.
///
/// This is the core correctness property of the banking system.
pub struct ConservationLaw;

impl Invariant for ConservationLaw {
    fn name(&self) -> &str {
        "conservation_law"
    }

    fn check(&self, state: &StateHandle, _sim_time_ms: u64) {
        if let Some(model) = state.get::<BankingModel>(BANKING_MODEL_KEY) {
            let total = model.total_balance();
            let expected = model.total_deposited - model.total_withdrawn;
            assert_always!(
                total == expected,
                format!(
                    "conservation law violated: sum(balances)={} != deposited({}) - withdrawn({})",
                    total, model.total_deposited, model.total_withdrawn
                )
            );
        }
    }
}

/// Balance non-negative invariant: all balances >= 0.
pub struct NonNegativeBalances;

impl Invariant for NonNegativeBalances {
    fn name(&self) -> &str {
        "non_negative_balances"
    }

    fn check(&self, state: &StateHandle, _sim_time_ms: u64) {
        if let Some(model) = state.get::<BankingModel>(BANKING_MODEL_KEY) {
            for (account, balance) in &model.balances {
                assert_always!(
                    *balance >= 0,
                    format!("account '{}' has negative balance: {}", account, balance)
                );
            }
        }
    }
}
