//! BankAccountActor - Demonstrates state management with DeactivateOnIdle policy.
//!
//! This actor shows how to:
//! - Persist state using `ActorState<Balance>`
//! - Load state automatically on reactivation
//! - Use `DeactivateOnIdle` policy to trigger deactivation

use moonpool::actor::ActorState;
use moonpool::prelude::*;
use serde::{Deserialize, Serialize};

/// Bank account balance state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Balance {
    pub balance: u32,
}

impl Default for Balance {
    fn default() -> Self {
        Self { balance: 1000 } // Default starting balance
    }
}

/// Deposit request message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DepositRequest {
    pub amount: u32,
}

/// Withdraw request message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WithdrawRequest {
    pub amount: u32,
}

/// Get balance request message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetBalanceRequest;

/// Bank account actor with persistent state.
pub struct BankAccountActor {
    actor_id: ActorId,
    state: Option<ActorState<Balance>>,
}

impl BankAccountActor {
    pub fn new(actor_id: ActorId) -> Self {
        Self {
            actor_id,
            state: None, // Will be set during activation
        }
    }

    fn key(&self) -> &str {
        &self.actor_id.key
    }
}

// Implement Actor trait
#[async_trait(?Send)]
impl Actor for BankAccountActor {
    type State = Balance;
    const ACTOR_TYPE: &'static str = "BankAccount";

    fn actor_id(&self) -> &ActorId {
        &self.actor_id
    }

    // Use Local placement for better performance
    fn placement_hint() -> PlacementHint {
        PlacementHint::Random
    }

    // *** CRITICAL: DeactivateOnIdle policy ***
    // This actor will deactivate when its message queue is empty
    fn deactivation_policy() -> DeactivationPolicy {
        DeactivationPolicy::DeactivateOnIdle
    }

    async fn on_activate(&mut self, state: ActorState<Balance>) -> Result<()> {
        // ActorCatalog has already created the ActorState wrapper with loaded or default state
        let balance = state.get().balance;
        let is_reactivation = balance != Balance::default().balance;

        self.state = Some(state);

        if is_reactivation {
            tracing::info!(
                actor_id = %self.actor_id,
                key = self.key(),
                balance = balance,
                "BankAccountActor REACTIVATED with loaded state from storage"
            );
        } else {
            tracing::info!(
                actor_id = %self.actor_id,
                key = self.key(),
                balance = balance,
                "BankAccountActor activated with default balance"
            );
        }

        Ok(())
    }

    async fn on_deactivate(&mut self, reason: DeactivationReason) -> Result<()> {
        let final_balance = self.state.as_ref().map(|s| s.get().balance).unwrap_or(0);

        tracing::info!(
            actor_id = %self.actor_id,
            key = self.key(),
            reason = ?reason,
            final_balance = final_balance,
            "BankAccountActor DEACTIVATING"
        );

        // State changes are already persisted in handlers via state.persist().await
        // No need to persist again on deactivation
        Ok(())
    }

    // Register message handlers
    fn register_handlers<S: moonpool::serialization::Serializer + 'static>(
        registry: &mut HandlerRegistry<Self, S>,
    ) {
        registry.register::<DepositRequest, u32>();
        registry.register::<WithdrawRequest, u32>();
        registry.register::<GetBalanceRequest, u32>();
    }
}

// Implement MessageHandler for DepositRequest
#[async_trait(?Send)]
impl<S: moonpool::serialization::Serializer> MessageHandler<DepositRequest, u32, S>
    for BankAccountActor
{
    async fn handle(&mut self, req: DepositRequest, _ctx: &ActorContext<Self, S>) -> Result<u32> {
        let state = self.state.as_ref().ok_or_else(|| {
            ActorError::ProcessingFailed("Actor state not initialized".to_string())
        })?;

        // Update balance
        let mut current = state.get().clone();
        current.balance += req.amount;
        state.set(current.clone());

        // Persist to storage
        state.persist().await?;

        tracing::info!(
            actor_id = %self.actor_id,
            key = self.key(),
            amount = req.amount,
            new_balance = current.balance,
            "Deposit completed"
        );

        Ok(current.balance)
    }
}

// Implement MessageHandler for WithdrawRequest
#[async_trait(?Send)]
impl<S: moonpool::serialization::Serializer> MessageHandler<WithdrawRequest, u32, S>
    for BankAccountActor
{
    async fn handle(&mut self, req: WithdrawRequest, _ctx: &ActorContext<Self, S>) -> Result<u32> {
        let state = self.state.as_ref().ok_or_else(|| {
            ActorError::ProcessingFailed("Actor state not initialized".to_string())
        })?;

        // Check sufficient funds and prepare update
        let updated_balance = {
            let current = state.get();
            if current.balance < req.amount {
                return Err(ActorError::ProcessingFailed(format!(
                    "Insufficient funds: requested {}, available {}",
                    req.amount, current.balance
                )));
            }

            // Update balance
            let mut updated = current.clone();
            updated.balance -= req.amount;
            drop(current); // Explicitly drop before mutating state
            state.set(updated.clone());
            updated.balance
        };

        // Persist to storage
        state.persist().await?;

        tracing::info!(
            actor_id = %self.actor_id,
            key = self.key(),
            amount = req.amount,
            new_balance = updated_balance,
            "Withdrawal completed"
        );

        Ok(updated_balance)
    }
}

// Implement MessageHandler for GetBalanceRequest
#[async_trait(?Send)]
impl<S: moonpool::serialization::Serializer> MessageHandler<GetBalanceRequest, u32, S>
    for BankAccountActor
{
    async fn handle(
        &mut self,
        _req: GetBalanceRequest,
        _ctx: &ActorContext<Self, S>,
    ) -> Result<u32> {
        let state = self.state.as_ref().ok_or_else(|| {
            ActorError::ProcessingFailed("Actor state not initialized".to_string())
        })?;

        let balance = state.get().balance;

        tracing::debug!(
            actor_id = %self.actor_id,
            key = self.key(),
            balance = balance,
            "Balance queried"
        );

        Ok(balance)
    }
}

// Extension trait for typed API
#[async_trait(?Send)]
pub trait BankAccountActorRef {
    /// Deposit money into the account.
    async fn deposit(&self, amount: u32) -> Result<u32>;

    /// Withdraw money from the account.
    async fn withdraw(&self, amount: u32) -> Result<u32>;

    /// Get current balance.
    async fn get_balance(&self) -> Result<u32>;
}

// Implement extension trait for ActorRef<BankAccountActor>
#[async_trait(?Send)]
impl BankAccountActorRef for ActorRef<BankAccountActor> {
    async fn deposit(&self, amount: u32) -> Result<u32> {
        self.call(DepositRequest { amount }).await
    }

    async fn withdraw(&self, amount: u32) -> Result<u32> {
        self.call(WithdrawRequest { amount }).await
    }

    async fn get_balance(&self) -> Result<u32> {
        self.call(GetBalanceRequest).await
    }
}

// Factory for creating BankAccountActor instances
pub struct BankAccountActorFactory;

#[async_trait(?Send)]
impl ActorFactory for BankAccountActorFactory {
    type Actor = BankAccountActor;

    async fn create(&self, actor_id: ActorId) -> Result<Self::Actor> {
        tracing::info!(
            actor_id = %actor_id,
            key = &actor_id.key,
            "Creating BankAccountActor instance"
        );
        Ok(BankAccountActor::new(actor_id))
    }
}
