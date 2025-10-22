// BankAccountActor - Example actor for testing location-transparent messaging
//
// This is a stateless version for Phase 3 (User Story 1) testing.
// State persistence will be added in Phase 7 (User Story 5).

use moonpool::error::MessageError;
use moonpool::prelude::*;
use serde::{Deserialize, Serialize};

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
    pub fn deposit(&mut self, amount: u64) -> std::result::Result<u64, ActorError> {
        self.balance += amount;
        self.transactions.push(("deposit".to_string(), amount));
        Ok(self.balance)
    }

    /// Withdraw funds from account
    pub fn withdraw(&mut self, amount: u64) -> std::result::Result<u64, ActorError> {
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
    type State = (); // Stateless for Phase 3

    fn actor_id(&self) -> &ActorId {
        &self.actor_id
    }

    async fn on_activate(&mut self, _state: Option<()>) -> std::result::Result<(), ActorError> {
        tracing::info!("BankAccount activated: {}", self.actor_id);
        Ok(())
    }

    async fn on_deactivate(
        &mut self,
        reason: DeactivationReason,
    ) -> std::result::Result<(), ActorError> {
        tracing::info!(
            "BankAccount deactivating: {} (reason: {:?})",
            self.actor_id,
            reason
        );
        Ok(())
    }
}

// MessageHandler implementations for each request type
#[async_trait(?Send)]
impl MessageHandler<DepositRequest, u64> for BankAccountActor {
    async fn handle(
        &mut self,
        req: DepositRequest,
        _ctx: &ActorContext<Self>,
    ) -> std::result::Result<u64, ActorError> {
        self.deposit(req.amount)
    }
}

#[async_trait(?Send)]
impl MessageHandler<WithdrawRequest, u64> for BankAccountActor {
    async fn handle(
        &mut self,
        req: WithdrawRequest,
        _ctx: &ActorContext<Self>,
    ) -> std::result::Result<u64, ActorError> {
        self.withdraw(req.amount)
    }
}

#[async_trait(?Send)]
impl MessageHandler<GetBalanceRequest, u64> for BankAccountActor {
    async fn handle(
        &mut self,
        _req: GetBalanceRequest,
        _ctx: &ActorContext<Self>,
    ) -> std::result::Result<u64, ActorError> {
        Ok(self.get_balance())
    }
}

// Manual message dispatch for BankAccountActor (Phase 3 approach)
//
// This extension trait provides method dispatch for BankAccountActor.
// In Phase 4, this will be replaced by automatic dispatch via MessageHandler trait.
use moonpool::messaging::{Message, MessageBus};

pub trait BankAccountActorDispatch {
    async fn dispatch_message_impl(
        &self,
        message: Message,
        message_bus: &MessageBus,
    ) -> std::result::Result<(), ActorError>;
}

#[async_trait(?Send)]
impl BankAccountActorDispatch for ActorContext<BankAccountActor> {
    async fn dispatch_message_impl(
        &self,
        message: Message,
        message_bus: &MessageBus,
    ) -> std::result::Result<(), ActorError> {
        // Extract method name from message
        let method_name = &message.method_name;

        // Check actor state
        if self.get_state() != ActivationState::Valid {
            return Err(ActorError::ProcessingFailed(format!(
                "Actor not in Valid state: {:?}",
                self.get_state()
            )));
        }

        // Match on method name and dispatch to appropriate handler
        let response_payload = match method_name.as_str() {
            "DepositRequest" => {
                // Deserialize request
                let request: DepositRequest = serde_json::from_slice(&message.payload)
                    .map_err(|e| ActorError::Message(MessageError::Serialization(e)))?;

                // Call handler
                let response = {
                    let mut actor = self.actor_instance.borrow_mut();
                    MessageHandler::<DepositRequest, u64>::handle(&mut *actor, request, self)
                        .await?
                };

                // Serialize response
                serde_json::to_vec(&response)
                    .map_err(|e| ActorError::Message(MessageError::Serialization(e)))?
            }
            "WithdrawRequest" => {
                // Deserialize request
                let request: WithdrawRequest = serde_json::from_slice(&message.payload)
                    .map_err(|e| ActorError::Message(MessageError::Serialization(e)))?;

                // Call handler
                let response = {
                    let mut actor = self.actor_instance.borrow_mut();
                    MessageHandler::<WithdrawRequest, u64>::handle(&mut *actor, request, self)
                        .await?
                };

                // Serialize response
                serde_json::to_vec(&response)
                    .map_err(|e| ActorError::Message(MessageError::Serialization(e)))?
            }
            "GetBalanceRequest" => {
                // Deserialize request
                let request: GetBalanceRequest = serde_json::from_slice(&message.payload)
                    .map_err(|e| ActorError::Message(MessageError::Serialization(e)))?;

                // Call handler
                let response = {
                    let mut actor = self.actor_instance.borrow_mut();
                    MessageHandler::<GetBalanceRequest, u64>::handle(&mut *actor, request, self)
                        .await?
                };

                // Serialize response
                serde_json::to_vec(&response)
                    .map_err(|e| ActorError::Message(MessageError::Serialization(e)))?
            }
            _ => {
                return Err(ActorError::UnknownMethod(method_name.clone()));
            }
        };

        // Send response if this was a Request (not OneWay)
        if message.direction == crate::messaging::Direction::Request {
            let response_msg = Message::response(&message, response_payload);
            message_bus.route_message(response_msg).await?;
        }

        // Update last message time
        self.update_last_message_time();

        Ok(())
    }
}
