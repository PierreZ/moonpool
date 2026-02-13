//! Simulation workloads for the virtual actor system.
//!
//! Implements BankingWorkload that exercises the actor system through
//! the operation alphabet, tracking results in a reference model.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use async_trait::async_trait;
use moonpool::actors::{
    ActorContext, ActorDirectory, ActorError, ActorHandler, ActorHost, ActorRouter,
    ActorStateStore, ActorType, DeactivationHint, InMemoryDirectory, InMemoryStateStore,
    LocalPlacement, PersistentState, PlacementStrategy,
};
use moonpool::{
    Endpoint, JsonCodec, MessageCodec, NetTransportBuilder, Providers, RpcError, TaskProvider,
    TimeProvider, UID, virtual_actor,
};
use moonpool_sim::runner::context::SimContext;
use moonpool_sim::runner::workload::Workload;
use moonpool_sim::{SimulationError, assert_always};
use serde::{Deserialize, Serialize};

use super::alphabet::{ActorOp, OpWeights, generate_operation};
use super::reference_model::{ActorRefModel, check_always_invariants};

// ============================================================================
// BankAccount virtual actor definition
// ============================================================================

/// Deposit request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DepositRequest {
    /// Amount to deposit.
    pub amount: i64,
}

/// Withdraw request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WithdrawRequest {
    /// Amount to withdraw.
    pub amount: i64,
}

/// Get balance request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetBalanceRequest {}

/// Balance response (shared by all methods).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BalanceResponse {
    /// Current balance after the operation.
    pub balance: i64,
}

/// Virtual actor interface for a bank account.
#[virtual_actor(id = 0xBA4E_4B00)]
trait BankAccount {
    /// Deposit funds.
    async fn deposit(&mut self, req: DepositRequest) -> Result<BalanceResponse, RpcError>;
    /// Withdraw funds. Returns error if insufficient balance.
    async fn withdraw(&mut self, req: WithdrawRequest) -> Result<BalanceResponse, RpcError>;
    /// Query current balance.
    async fn get_balance(&mut self, req: GetBalanceRequest) -> Result<BalanceResponse, RpcError>;
}

/// Persistent state for the bank account.
#[derive(Default, Debug, Clone, Serialize, Deserialize)]
struct BankAccountData {
    balance: i64,
}

/// Bank account actor implementation with persistent state.
#[derive(Default)]
struct BankAccountImpl {
    state: Option<PersistentState<BankAccountData>>,
}

impl BankAccountImpl {
    fn balance(&self) -> i64 {
        self.state.as_ref().map(|s| s.state().balance).unwrap_or(0)
    }
}

#[async_trait(?Send)]
impl BankAccount for BankAccountImpl {
    async fn deposit(&mut self, req: DepositRequest) -> Result<BalanceResponse, RpcError> {
        let new_balance = self.balance() + req.amount;
        if let Some(s) = &mut self.state {
            s.state_mut().balance = new_balance;
            let _ = s.write_state().await;
        }
        Ok(BalanceResponse {
            balance: new_balance,
        })
    }

    async fn withdraw(&mut self, req: WithdrawRequest) -> Result<BalanceResponse, RpcError> {
        let current = self.balance();
        if req.amount > current {
            return Err(RpcError::Reply(moonpool::ReplyError::Serialization {
                message: format!(
                    "insufficient funds: balance={}, requested={}",
                    current, req.amount
                ),
            }));
        }
        let new_balance = current - req.amount;
        if let Some(s) = &mut self.state {
            s.state_mut().balance = new_balance;
            let _ = s.write_state().await;
        }
        Ok(BalanceResponse {
            balance: new_balance,
        })
    }

    async fn get_balance(&mut self, _req: GetBalanceRequest) -> Result<BalanceResponse, RpcError> {
        Ok(BalanceResponse {
            balance: self.balance(),
        })
    }
}

#[async_trait(?Send)]
impl ActorHandler for BankAccountImpl {
    fn actor_type() -> ActorType {
        BankAccountRef::<moonpool::SimProviders>::ACTOR_TYPE
    }

    fn deactivation_hint(&self) -> DeactivationHint {
        DeactivationHint::DeactivateAfterIdle(Duration::from_millis(200))
    }

    async fn on_activate<P: Providers, C: MessageCodec>(
        &mut self,
        ctx: &ActorContext<P, C>,
    ) -> Result<(), ActorError> {
        if let Some(store) = ctx.state_store() {
            let ps = PersistentState::<BankAccountData>::load(
                store.clone(),
                "BankAccount",
                &ctx.id.identity,
            )
            .await
            .map_err(|e| ActorError::HandlerError(format!("state load: {e}")))?;
            self.state = Some(ps);
        }
        Ok(())
    }

    async fn dispatch<P: Providers, C: MessageCodec>(
        &mut self,
        ctx: &ActorContext<P, C>,
        method: u32,
        body: &[u8],
    ) -> Result<Vec<u8>, ActorError> {
        dispatch_bank_account(self, ctx, method, body).await
    }
}

// ============================================================================
// Operation execution
// ============================================================================

/// Result of executing an actor operation.
#[derive(Debug)]
#[allow(dead_code)]
enum OpResult {
    /// Deposit succeeded.
    DepositOk { balance: i64 },
    /// Withdrawal succeeded.
    WithdrawOk { balance: i64 },
    /// Balance query succeeded.
    BalanceOk { balance: i64 },
    /// Operation failed (expected for adversarial/nemesis ops).
    Failed { reason: String },
    /// No-op (e.g., deactivation sleep).
    Noop,
}

/// Execute a single actor operation.
async fn execute_op<P: Providers>(
    op: &ActorOp,
    router: &Rc<ActorRouter<P>>,
    time: &P::Time,
) -> OpResult {
    match op {
        ActorOp::Deposit { actor_id, amount } => {
            let actor_ref = BankAccountRef::new(actor_id.as_str(), router);
            match actor_ref
                .deposit(DepositRequest {
                    amount: *amount as i64,
                })
                .await
            {
                Ok(resp) => OpResult::DepositOk {
                    balance: resp.balance,
                },
                Err(e) => OpResult::Failed {
                    reason: e.to_string(),
                },
            }
        }

        ActorOp::Withdraw { actor_id, amount } => {
            let actor_ref = BankAccountRef::new(actor_id.as_str(), router);
            match actor_ref
                .withdraw(WithdrawRequest {
                    amount: *amount as i64,
                })
                .await
            {
                Ok(resp) => OpResult::WithdrawOk {
                    balance: resp.balance,
                },
                Err(e) => OpResult::Failed {
                    reason: e.to_string(),
                },
            }
        }

        ActorOp::GetBalance { actor_id } => {
            let actor_ref = BankAccountRef::new(actor_id.as_str(), router);
            match actor_ref.get_balance(GetBalanceRequest {}).await {
                Ok(resp) => OpResult::BalanceOk {
                    balance: resp.balance,
                },
                Err(e) => OpResult::Failed {
                    reason: e.to_string(),
                },
            }
        }

        ActorOp::Transfer { from, to, amount } => {
            let from_ref = BankAccountRef::new(from.as_str(), router);
            // First withdraw
            match from_ref
                .withdraw(WithdrawRequest {
                    amount: *amount as i64,
                })
                .await
            {
                Ok(_) => {
                    // Then deposit
                    let to_ref = BankAccountRef::new(to.as_str(), router);
                    match to_ref
                        .deposit(DepositRequest {
                            amount: *amount as i64,
                        })
                        .await
                    {
                        Ok(_) => {
                            moonpool_sim::assert_sometimes!(true, "transfer_completed");
                            OpResult::DepositOk { balance: 0 } // placeholder
                        }
                        Err(e) => {
                            // Deposit failed after withdrawal — compensate
                            let _ = from_ref
                                .deposit(DepositRequest {
                                    amount: *amount as i64,
                                })
                                .await;
                            OpResult::Failed {
                                reason: format!("transfer deposit failed: {e}"),
                            }
                        }
                    }
                }
                Err(e) => {
                    moonpool_sim::assert_sometimes!(true, "insufficient_funds_rejected");
                    OpResult::Failed {
                        reason: format!("transfer withdraw failed: {e}"),
                    }
                }
            }
        }

        ActorOp::SendToNonExistent { actor_id } => {
            let actor_ref = BankAccountRef::new(actor_id.as_str(), router);
            // Sending to a non-existent actor will activate it (virtual actor pattern)
            match actor_ref.get_balance(GetBalanceRequest {}).await {
                Ok(resp) => OpResult::BalanceOk {
                    balance: resp.balance,
                },
                Err(e) => OpResult::Failed {
                    reason: e.to_string(),
                },
            }
        }

        ActorOp::InvalidMethod { actor_id, method } => {
            // Send a raw request with an invalid method discriminant
            let actor_id_typed =
                moonpool::actors::ActorId::new(BankAccountRef::<P>::ACTOR_TYPE, actor_id.as_str());
            let result: Result<BalanceResponse, _> = router
                .send_actor_request(&actor_id_typed, *method, &GetBalanceRequest {})
                .await;
            match result {
                Ok(_) => OpResult::Failed {
                    reason: "expected error for invalid method".to_string(),
                },
                Err(_e) => OpResult::Noop,
            }
        }

        ActorOp::ConcurrentCallsSameActor { actor_id, count } => {
            // Send concurrent calls — they should all serialize (turn-based concurrency)
            let mut handles = Vec::new();
            for _ in 0..*count {
                let actor_ref = BankAccountRef::new(actor_id.as_str(), router);
                handles.push(tokio::task::spawn_local(async move {
                    actor_ref.get_balance(GetBalanceRequest {}).await
                }));
            }
            let mut all_ok = true;
            for handle in handles {
                if let Ok(Err(_)) = handle.await {
                    all_ok = false;
                }
            }
            if all_ok {
                moonpool_sim::assert_sometimes!(true, "concurrent_calls_serialized");
            }
            OpResult::Noop
        }

        ActorOp::ZeroAmountDeposit { actor_id } => {
            let actor_ref = BankAccountRef::new(actor_id.as_str(), router);
            match actor_ref.deposit(DepositRequest { amount: 0 }).await {
                Ok(resp) => OpResult::DepositOk {
                    balance: resp.balance,
                },
                Err(e) => OpResult::Failed {
                    reason: e.to_string(),
                },
            }
        }

        ActorOp::MaxAmountWithdraw { actor_id } => {
            let actor_ref = BankAccountRef::new(actor_id.as_str(), router);
            match actor_ref
                .withdraw(WithdrawRequest {
                    amount: i64::MAX / 2,
                })
                .await
            {
                Ok(resp) => OpResult::WithdrawOk {
                    balance: resp.balance,
                },
                Err(e) => {
                    moonpool_sim::assert_sometimes!(true, "insufficient_funds_rejected");
                    OpResult::Failed {
                        reason: e.to_string(),
                    }
                }
            }
        }

        ActorOp::DeactivateActor { .. } => {
            // Sleep to trigger the DeactivateAfterIdle timeout
            let _ = time.sleep(Duration::from_millis(250)).await;
            moonpool_sim::assert_sometimes!(true, "deactivate_on_idle_exercised");
            OpResult::Noop
        }

        ActorOp::FloodSingleActor { actor_id, count } => {
            // Rapid-fire multiple deposits
            let mut successes = 0u32;
            for _ in 0..*count {
                let actor_ref = BankAccountRef::new(actor_id.as_str(), router);
                if actor_ref
                    .deposit(DepositRequest { amount: 1 })
                    .await
                    .is_ok()
                {
                    successes += 1;
                }
            }
            OpResult::DepositOk {
                balance: successes as i64,
            }
        }
    }
}

// ============================================================================
// BankingWorkload
// ============================================================================

/// Single-node banking workload that exercises the full actor lifecycle.
pub struct BankingWorkload {
    /// Operation distribution weights.
    weights: OpWeights,
    /// Number of operations to execute.
    ops_count: usize,
    /// Number of actor identities in the pool.
    actor_count: usize,
}

impl BankingWorkload {
    /// Create a new banking workload with default settings.
    pub fn new() -> Self {
        Self {
            weights: OpWeights::default(),
            ops_count: 50,
            actor_count: 5,
        }
    }

    /// Create with specific configuration.
    pub fn with_config(weights: OpWeights, ops_count: usize, actor_count: usize) -> Self {
        Self {
            weights,
            ops_count,
            actor_count,
        }
    }
}

#[async_trait(?Send)]
impl Workload for BankingWorkload {
    fn name(&self) -> &str {
        "banking"
    }

    async fn run(&self, ctx: &SimContext) -> Result<(), SimulationError> {
        let addr = format!("{}:4700", ctx.my_ip());
        let providers = ctx.providers().clone();
        let local_addr = moonpool::NetworkAddress::parse(&addr)
            .map_err(|e| SimulationError::InvalidState(format!("parse addr: {e}")))?;

        // Build transport
        let transport = NetTransportBuilder::new(providers)
            .local_address(local_addr.clone())
            .build()
            .map_err(|e| SimulationError::InvalidState(format!("build transport: {e}")))?;

        // Actor infrastructure
        let directory: Rc<dyn ActorDirectory> = Rc::new(InMemoryDirectory::new());
        let state_store: Rc<dyn ActorStateStore> = Rc::new(InMemoryStateStore::new());

        let local_endpoint = Endpoint::new(
            local_addr,
            UID::new(BankAccountRef::<moonpool::SimProviders>::ACTOR_TYPE.0, 0),
        );
        let placement: Rc<dyn PlacementStrategy> = Rc::new(LocalPlacement::new(local_endpoint));
        let router = Rc::new(ActorRouter::new(
            transport.clone(),
            directory.clone(),
            placement,
            JsonCodec,
        ));
        let host =
            ActorHost::new(transport, router.clone(), directory).with_state_store(state_store);

        host.register::<BankAccountImpl>();

        // Yield to let the processing loop start
        ctx.providers().task().yield_now().await;

        // Run operations
        let model = RefCell::new(ActorRefModel::new());

        for i in 0..self.ops_count {
            if ctx.shutdown().is_cancelled() {
                break;
            }

            let op = generate_operation(ctx.random(), &self.weights, self.actor_count);
            let result = execute_op(&op, &router, ctx.time()).await;

            // Update reference model
            {
                let mut m = model.borrow_mut();
                match (&op, &result) {
                    (ActorOp::Deposit { actor_id, amount }, OpResult::DepositOk { .. }) => {
                        m.record_deposit(actor_id, *amount);
                    }
                    (ActorOp::Withdraw { actor_id, amount }, OpResult::WithdrawOk { .. }) => {
                        m.record_withdrawal(actor_id, *amount);
                    }
                    (ActorOp::GetBalance { actor_id }, OpResult::BalanceOk { .. }) => {
                        m.record_get_balance(actor_id);
                    }
                    (ActorOp::Transfer { from, to, amount }, OpResult::DepositOk { .. }) => {
                        // Transfer succeeded: record both sides
                        m.record_withdrawal(from, *amount);
                        m.record_deposit(to, *amount);
                    }
                    (ActorOp::ZeroAmountDeposit { actor_id }, OpResult::DepositOk { .. }) => {
                        m.record_deposit(actor_id, 0);
                    }
                    (
                        ActorOp::FloodSingleActor {
                            actor_id, count, ..
                        },
                        OpResult::DepositOk { balance },
                    ) => {
                        // Each successful deposit in the flood was +1
                        m.record_deposit(actor_id, *balance as u64);
                        // Record the remaining as failures
                        let failed_count = *count as i64 - *balance;
                        for _ in 0..failed_count {
                            m.record_failure(actor_id, "flood_deposit", "failed in flood");
                        }
                    }
                    (_, OpResult::Failed { reason }) => {
                        let actor_id = match &op {
                            ActorOp::Deposit { actor_id, .. }
                            | ActorOp::Withdraw { actor_id, .. }
                            | ActorOp::GetBalance { actor_id }
                            | ActorOp::SendToNonExistent { actor_id }
                            | ActorOp::InvalidMethod { actor_id, .. }
                            | ActorOp::ConcurrentCallsSameActor { actor_id, .. }
                            | ActorOp::ZeroAmountDeposit { actor_id }
                            | ActorOp::MaxAmountWithdraw { actor_id }
                            | ActorOp::DeactivateActor { actor_id }
                            | ActorOp::FloodSingleActor { actor_id, .. } => actor_id.as_str(),
                            ActorOp::Transfer { from, .. } => from.as_str(),
                        };
                        let op_name = match &op {
                            ActorOp::Deposit { .. } => "deposit",
                            ActorOp::Withdraw { .. } => "withdraw",
                            ActorOp::GetBalance { .. } => "get_balance",
                            ActorOp::Transfer { .. } => "transfer",
                            ActorOp::SendToNonExistent { .. } => "send_to_non_existent",
                            ActorOp::InvalidMethod { .. } => "invalid_method",
                            ActorOp::ConcurrentCallsSameActor { .. } => "concurrent_calls",
                            ActorOp::ZeroAmountDeposit { .. } => "zero_deposit",
                            ActorOp::MaxAmountWithdraw { .. } => "max_withdraw",
                            ActorOp::DeactivateActor { .. } => "deactivate",
                            ActorOp::FloodSingleActor { .. } => "flood",
                        };
                        m.record_failure(actor_id, op_name, reason);
                    }
                    _ => {} // Noop results
                }
            }

            // Periodic yield to let sim events process
            if i % 5 == 4 {
                let _ = ctx.time().sleep(Duration::from_millis(1)).await;
            }
        }

        // Verify final balances match reference model
        {
            let m = model.borrow();
            for (actor_id, expected_balance) in &m.balances {
                let actor_ref = BankAccountRef::new(actor_id.as_str(), &router);
                if let Ok(resp) = actor_ref.get_balance(GetBalanceRequest {}).await {
                    assert_always!(
                        resp.balance == *expected_balance,
                        &format!(
                            "Balance mismatch for {}: expected={}, actual={}",
                            actor_id, expected_balance, resp.balance
                        )
                    );
                }
            }
        }

        // Publish reference model for check phase
        ctx.state().publish("banking_model", model.into_inner());

        // Drop host to trigger deactivation
        drop(host);

        // Yield to let deactivation complete
        ctx.providers().task().yield_now().await;

        Ok(())
    }

    async fn check(&self, ctx: &SimContext) -> Result<(), SimulationError> {
        if let Some(model) = ctx.state().get::<ActorRefModel>("banking_model") {
            check_always_invariants(&model);

            // Verify we actually ran operations
            assert_always!(
                model.total_ops > 0,
                "Banking workload should have executed at least one operation"
            );

            // Verify sometimes assertions
            moonpool_sim::assert_sometimes!(model.successful_ops > 0, "some_ops_succeeded");
        }

        Ok(())
    }
}
