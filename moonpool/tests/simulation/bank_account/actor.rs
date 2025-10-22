// BankAccountActor - Example actor for testing location-transparent messaging
//
// This is a stateless version for Phase 3 (User Story 1) testing.
// State persistence will be added in Phase 7 (User Story 5).

use moonpool::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Request to deposit funds
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DepositRequest {
    pub amount: u64,
}

/// Request to withdraw funds
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WithdrawRequest {
    pub amount: u64,
}

/// Request to get current balance
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetBalanceRequest;

/// BankAccountActor - Simple actor maintaining a balance
///
/// **Phase 3**: Stateless version, balance held in memory only
/// **Phase 7**: Will add ActorState<BankAccountState> for persistence
pub struct BankAccountActor {
    actor_id: ActorId,
    /// In-memory balance (ephemeral, not persisted in Phase 3)
    balance: u64,
    /// Transaction log for testing invariants
    transactions: Vec<(String, u64)>,
}

impl BankAccountActor {
    pub fn new(actor_id: ActorId) -> Self {
        Self {
            actor_id,
            balance: 0,
            transactions: Vec::new(),
        }
    }

    /// Deposit funds into account
    pub fn deposit(&mut self, amount: u64) -> Result<u64, ActorError> {
        self.balance += amount;
        self.transactions.push(("deposit".to_string(), amount));
        Ok(self.balance)
    }

    /// Withdraw funds from account
    pub fn withdraw(&mut self, amount: u64) -> Result<u64, ActorError> {
        if amount > self.balance {
            return Err(ActorError::InsufficientFunds {
                requested: amount,
                available: self.balance,
            });
        }
        self.balance -= amount;
        self.transactions.push(("withdraw".to_string(), amount));
        Ok(self.balance)
    }

    /// Get current balance
    pub fn get_balance(&self) -> u64 {
        self.balance
    }

    /// Get transaction count (for testing)
    pub fn transaction_count(&self) -> usize {
        self.transactions.len()
    }
}

// Actor trait implementation
#[async_trait(?Send)]
impl Actor for BankAccountActor {
    type State = ();  // Stateless for Phase 3

    fn actor_id(&self) -> &ActorId {
        &self.actor_id
    }

    async fn on_activate(&mut self, _state: Option<()>) -> Result<(), ActorError> {
        tracing::info!("BankAccount activated: {}", self.actor_id);
        Ok(())
    }

    async fn on_deactivate(&mut self, reason: DeactivationReason) -> Result<(), ActorError> {
        tracing::info!("BankAccount deactivating: {} (reason: {:?})", self.actor_id, reason);
        Ok(())
    }
}

// MessageHandler implementations for each request type
#[async_trait(?Send)]
impl MessageHandler<DepositRequest, u64> for BankAccountActor {
    async fn handle(&mut self, req: DepositRequest, _ctx: &ActorContext<Self>) -> Result<u64, ActorError> {
        self.deposit(req.amount)
    }
}

#[async_trait(?Send)]
impl MessageHandler<WithdrawRequest, u64> for BankAccountActor {
    async fn handle(&mut self, req: WithdrawRequest, _ctx: &ActorContext<Self>) -> Result<u64, ActorError> {
        self.withdraw(req.amount)
    }
}

#[async_trait(?Send)]
impl MessageHandler<GetBalanceRequest, u64> for BankAccountActor {
    async fn handle(&mut self, _req: GetBalanceRequest, _ctx: &ActorContext<Self>) -> Result<u64, ActorError> {
        Ok(self.get_balance())
    }
}
