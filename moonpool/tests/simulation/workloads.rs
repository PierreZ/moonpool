//! Banking workload for actor simulation testing.
//!
//! Implements the `Workload` trait with a banking actor system:
//! deposit, withdraw, get_balance, and transfer operations executed
//! against a reference model for conservation law verification.

use std::rc::Rc;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use moonpool::actors::{
    ActorContext, ActorDirectory, ActorError, ActorHandler, ActorHost, ActorRouter,
    ActorStateStore, DeactivationHint, InMemoryDirectory, InMemoryStateStore, LocalPlacement,
    PersistentState, PlacementStrategy,
};
use moonpool::{
    Endpoint, JsonCodec, MessageCodec, NetTransportBuilder, NetworkAddress, Providers,
    RandomProvider, RpcError, TimeProvider, UID, actor_impl, service,
};
use moonpool_sim::providers::SimProviders;
use moonpool_sim::{SimContext, SimulationResult, Workload, assert_sometimes};

use super::invariants::{BANKING_MODEL_KEY, BankingModel};
use super::operations::{ActorOp, random_op};

// ============================================================================
// Virtual Actor: BankAccount
// ============================================================================

/// Deposit request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DepositRequest {
    pub amount: i64,
}

/// Withdraw request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WithdrawRequest {
    pub amount: i64,
}

/// Get balance request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetBalanceRequest {}

/// Balance response (shared by all methods).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BalanceResponse {
    pub balance: i64,
}

/// Define the virtual actor interface.
#[service(id = 0xBA4E_4B00)]
trait BankAccount {
    async fn deposit(&mut self, req: DepositRequest) -> Result<BalanceResponse, RpcError>;
    async fn withdraw(&mut self, req: WithdrawRequest) -> Result<BalanceResponse, RpcError>;
    async fn get_balance(&mut self, req: GetBalanceRequest) -> Result<BalanceResponse, RpcError>;
}

/// Persistent state for the BankAccount.
#[derive(Default, Debug, Clone, Serialize, Deserialize)]
struct BankAccountData {
    balance: i64,
}

/// BankAccount actor implementation with persistent state.
///
/// Uses `DeactivateOnIdle` for maximum lifecycle exercise during simulation.
/// Withdraw always succeeds (balance checking is done at the workload level).
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
        let new_balance = self.balance() - req.amount;
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

#[actor_impl(BankAccount)]
impl ActorHandler for BankAccountImpl {
    fn deactivation_hint(&self) -> DeactivationHint {
        DeactivationHint::DeactivateOnIdle
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
}

// ============================================================================
// BankingWorkload
// ============================================================================

/// Banking workload for actor simulation testing.
///
/// Executes random banking operations against a set of accounts,
/// tracking a reference model for invariant checking.
/// Balance checking for withdrawals is done at the workload level:
/// withdrawals are only issued when the model shows sufficient funds.
pub struct BankingWorkload {
    /// Number of operations to execute per run.
    num_ops: usize,
    /// Account names to use.
    accounts: Vec<String>,
    /// Actor infrastructure (populated in setup).
    host: Option<ActorHost<SimProviders>>,
    router: Option<Rc<ActorRouter<SimProviders>>>,
    /// Reference model for tracking expected state.
    model: BankingModel,
}

impl BankingWorkload {
    /// Create a new banking workload.
    pub fn new(num_ops: usize, accounts: Vec<String>) -> Self {
        Self {
            num_ops,
            accounts,
            host: None,
            router: None,
            model: BankingModel::default(),
        }
    }
}

#[async_trait(?Send)]
impl Workload for BankingWorkload {
    fn name(&self) -> &str {
        "banking"
    }

    async fn setup(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let addr_str = format!("{}:4700", ctx.my_ip());
        let local_addr = NetworkAddress::parse(&addr_str).map_err(|e| {
            moonpool_sim::SimulationError::InvalidState(format!("invalid address: {e}"))
        })?;

        let transport = NetTransportBuilder::new(ctx.providers().clone())
            .local_address(local_addr.clone())
            .build()
            .map_err(|e| {
                moonpool_sim::SimulationError::InvalidState(format!("transport build: {e}"))
            })?;

        let directory: Rc<dyn ActorDirectory> = Rc::new(InMemoryDirectory::new());
        let state_store: Rc<dyn ActorStateStore> = Rc::new(InMemoryStateStore::new());
        let local_endpoint = Endpoint::new(
            local_addr,
            UID::new(BankAccountRef::<SimProviders>::ACTOR_TYPE.0, 0),
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

        self.host = Some(host);
        self.router = Some(router);
        self.model = BankingModel::default();

        Ok(())
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let router = self
            .router
            .as_ref()
            .ok_or_else(|| {
                moonpool_sim::SimulationError::InvalidState("router not initialized".to_string())
            })?
            .clone();

        // Seed initial deposits so accounts have funds
        for account in &self.accounts {
            let actor_ref = BankAccountRef::new(account.clone(), &router);
            let initial = ctx.random().random_range(100..500);
            match actor_ref.deposit(DepositRequest { amount: initial }).await {
                Ok(_) => self.model.deposit(account, initial),
                Err(e) => {
                    return Err(moonpool_sim::SimulationError::InvalidState(format!(
                        "initial deposit failed: {e}"
                    )));
                }
            }
        }

        // Publish initial model
        ctx.state().publish(BANKING_MODEL_KEY, self.model.clone());

        // Main operation loop
        for i in 0..self.num_ops {
            if ctx.shutdown().is_cancelled() {
                break;
            }

            // Periodic sleep to generate simulation events and prevent false
            // deadlock detection. Pure local-delivery actor calls produce no
            // simulation events, so the orchestrator's deadlock detector can
            // trigger if several consecutive non-SmallDelay operations run.
            if i % 10 == 0 {
                let _ = ctx.time().sleep(Duration::from_nanos(1)).await;
            }

            let op = random_op(ctx.random(), &self.accounts);

            match op {
                ActorOp::Deposit { account, amount } => {
                    let actor_ref = BankAccountRef::new(account.clone(), &router);
                    match actor_ref.deposit(DepositRequest { amount }).await {
                        Ok(resp) => {
                            self.model.deposit(&account, amount);
                            let expected = self.model.balance(&account);
                            moonpool_sim::assert_always!(
                                resp.balance == expected,
                                format!(
                                    "deposit balance mismatch: got {}, expected {}",
                                    resp.balance, expected
                                )
                            );
                        }
                        Err(e) => {
                            tracing::warn!("deposit failed: {}", e);
                        }
                    }
                }
                ActorOp::Withdraw { account, amount } => {
                    let can_withdraw = self.model.balance(&account) >= amount;
                    if !can_withdraw {
                        assert_sometimes!(true, "insufficient_funds_rejected");
                        continue;
                    }
                    let actor_ref = BankAccountRef::new(account.clone(), &router);
                    match actor_ref.withdraw(WithdrawRequest { amount }).await {
                        Ok(resp) => {
                            let withdrew = self.model.withdraw(&account, amount);
                            moonpool_sim::assert_always!(
                                withdrew,
                                "model withdraw should succeed when actor succeeded"
                            );
                            let expected = self.model.balance(&account);
                            moonpool_sim::assert_always!(
                                resp.balance == expected,
                                format!(
                                    "withdraw balance mismatch: got {}, expected {}",
                                    resp.balance, expected
                                )
                            );
                            assert_sometimes!(true, "withdraw_succeeded");
                        }
                        Err(e) => {
                            tracing::warn!("withdraw failed: {}", e);
                        }
                    }
                }
                ActorOp::GetBalance { account } => {
                    let actor_ref = BankAccountRef::new(account.clone(), &router);
                    match actor_ref.get_balance(GetBalanceRequest {}).await {
                        Ok(resp) => {
                            let expected = self.model.balance(&account);
                            moonpool_sim::assert_always!(
                                resp.balance == expected,
                                format!(
                                    "get_balance mismatch for '{}': got {}, expected {}",
                                    account, resp.balance, expected
                                )
                            );
                        }
                        Err(e) => {
                            tracing::warn!("get_balance failed: {}", e);
                        }
                    }
                }
                ActorOp::Transfer { from, to, amount } => {
                    let can_transfer = self.model.balance(&from) >= amount;
                    if !can_transfer {
                        assert_sometimes!(true, "transfer_insufficient_funds");
                        continue;
                    }
                    let from_ref = BankAccountRef::new(from.clone(), &router);
                    match from_ref.withdraw(WithdrawRequest { amount }).await {
                        Ok(_) => {
                            let withdrew = self.model.withdraw(&from, amount);
                            moonpool_sim::assert_always!(
                                withdrew,
                                "model withdraw should succeed for transfer"
                            );

                            let to_ref = BankAccountRef::new(to.clone(), &router);
                            match to_ref.deposit(DepositRequest { amount }).await {
                                Ok(_) => {
                                    self.model.deposit(&to, amount);
                                    assert_sometimes!(true, "transfer_completed");
                                }
                                Err(e) => {
                                    // Deposit to 'to' failed, but withdraw from 'from' already
                                    // committed at the actor level. Don't revert the model --
                                    // the money left 'from' even though it didn't arrive at 'to'.
                                    // Conservation law still holds: sum(balances) decreased by
                                    // amount, total_withdrawn increased by amount.
                                    tracing::warn!("transfer deposit failed: {}", e);
                                }
                            }
                        }
                        Err(e) => {
                            tracing::warn!("transfer withdraw failed: {}", e);
                        }
                    }
                }
                ActorOp::SmallDelay => {
                    let _ = ctx.time().sleep(Duration::from_millis(10)).await;
                }
            }

            // Publish updated model for invariant checking
            ctx.state().publish(BANKING_MODEL_KEY, self.model.clone());
        }

        // Shutdown: drop host to trigger deactivation
        drop(self.host.take());

        Ok(())
    }

    async fn check(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        // Final conservation law check
        let total = self.model.total_balance();
        let expected = self.model.total_deposited - self.model.total_withdrawn;
        moonpool_sim::assert_always!(
            total == expected,
            format!(
                "final conservation law: sum(balances)={} != deposited({}) - withdrawn({})",
                total, self.model.total_deposited, self.model.total_withdrawn
            )
        );

        // Check all balances are non-negative
        for (account, balance) in &self.model.balances {
            moonpool_sim::assert_always!(
                *balance >= 0,
                format!(
                    "final check: account '{}' has negative balance: {}",
                    account, balance
                )
            );
        }

        // Publish final state
        ctx.state().publish(BANKING_MODEL_KEY, self.model.clone());

        Ok(())
    }
}
