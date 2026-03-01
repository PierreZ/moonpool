//! Banking Example: Virtual actors with state persistence.
//!
//! This example demonstrates the virtual actor networking layer with
//! `PersistentState<T>` for durable actor state.
//!
//! # What It Shows
//!
//! - A virtual `BankAccount` actor defined with `#[service]` macro
//! - **State persistence**: `PersistentState<BankAccountData>` + `InMemoryStateStore`
//! - **Lifecycle**: `on_activate` loads state, `DeactivateAfterIdle` removes actor after idle timeout
//! - Typed `BankAccountRef` via `node.actor_ref("alice")`
//! - Single process, local delivery, single transport
//!
//! # Architecture
//!
//! ```text
//! EndpointMap:
//!   UID(0xBA4E_4B00, 0) → BankAccount handler             (virtual, all methods)
//! ```

use std::rc::Rc;
use std::time::Duration;

use moonpool::actors::{
    ActorContext, ActorError, ActorHandler, ActorStateStore, ClusterConfig, DeactivationHint,
    InMemoryStateStore, MoonpoolNode, NodeConfig, PersistentState,
};
use moonpool::{MessageCodec, NetworkAddress, Providers, RpcError, actor_impl, service};
use serde::{Deserialize, Serialize};

// ============================================================================
// Virtual Actor: BankAccount (using #[service] macro)
// ============================================================================

/// Deposit request.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct DepositRequest {
    amount: i64,
}

/// Withdraw request.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct WithdrawRequest {
    amount: i64,
}

/// Get balance request.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct GetBalanceRequest {}

/// Balance response (shared by all methods).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct BalanceResponse {
    balance: i64,
}

/// Define the virtual actor interface — generates:
/// - `BankAccountRef<P>` for typed actor calls
/// - `bank_account_methods` module with DEPOSIT, WITHDRAW, GET_BALANCE constants
/// - `dispatch_bank_account()` function for routing method calls
#[service(id = 0xBA4E_4B00)]
trait BankAccount {
    async fn deposit(&mut self, req: DepositRequest) -> Result<BalanceResponse, RpcError>;
    async fn withdraw(&mut self, req: WithdrawRequest) -> Result<BalanceResponse, RpcError>;
    async fn get_balance(&mut self, req: GetBalanceRequest) -> Result<BalanceResponse, RpcError>;
}

/// Persistent state for the BankAccount — survives deactivation.
#[derive(Default, Debug, Clone, Serialize, Deserialize)]
struct BankAccountData {
    balance: i64,
}

/// BankAccount actor implementation with persistent state.
///
/// Uses `DeactivateAfterIdle(30s)` — the actor stays in memory for 30 seconds
/// after the last message, then is deactivated. State is persisted to the store
/// and reloaded on reactivation.
#[derive(Default)]
struct BankAccountImpl {
    state: Option<PersistentState<BankAccountData>>,
}

impl BankAccountImpl {
    fn balance(&self) -> i64 {
        self.state.as_ref().map(|s| s.state().balance).unwrap_or(0)
    }
}

#[async_trait::async_trait(?Send)]
impl BankAccount for BankAccountImpl {
    async fn deposit<P: Providers, C: MessageCodec>(
        &mut self,
        _ctx: &ActorContext<P, C>,
        req: DepositRequest,
    ) -> Result<BalanceResponse, RpcError> {
        let new_balance = self.balance() + req.amount;
        if let Some(s) = &mut self.state {
            s.state_mut().balance = new_balance;
            let _ = s.write_state().await;
        }
        println!(
            "  [server] DEPOSIT +{} → balance={}",
            req.amount, new_balance
        );
        Ok(BalanceResponse {
            balance: new_balance,
        })
    }

    async fn withdraw<P: Providers, C: MessageCodec>(
        &mut self,
        _ctx: &ActorContext<P, C>,
        req: WithdrawRequest,
    ) -> Result<BalanceResponse, RpcError> {
        let new_balance = self.balance() - req.amount;
        if let Some(s) = &mut self.state {
            s.state_mut().balance = new_balance;
            let _ = s.write_state().await;
        }
        println!(
            "  [server] WITHDRAW -{} → balance={}",
            req.amount, new_balance
        );
        Ok(BalanceResponse {
            balance: new_balance,
        })
    }

    async fn get_balance<P: Providers, C: MessageCodec>(
        &mut self,
        _ctx: &ActorContext<P, C>,
        req: GetBalanceRequest,
    ) -> Result<BalanceResponse, RpcError> {
        let _ = req;
        let balance = self.balance();
        println!("  [server] GET_BALANCE → balance={}", balance);
        Ok(BalanceResponse { balance })
    }
}

/// Actor handler bridge — `actor_type()` and `dispatch()` are auto-generated
/// by `#[actor_impl]`. Only lifecycle overrides are hand-written.
#[actor_impl(BankAccount)]
impl ActorHandler for BankAccountImpl {
    fn deactivation_hint(&self) -> DeactivationHint {
        DeactivationHint::DeactivateAfterIdle(Duration::from_secs(30))
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
            println!(
                "  [server] ACTIVATE {} (balance={})",
                ctx.id.identity,
                ps.state().balance
            );
            self.state = Some(ps);
        }
        Ok(())
    }
}

// ============================================================================
// Main
// ============================================================================

fn main() {
    println!("=== Banking Example: Virtual Actors ===\n");

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build_local(Default::default())
        .expect("Failed to create Tokio LocalRuntime");

    runtime.block_on(async {
        if let Err(e) = run_example().await {
            eprintln!("Example error: {}", e);
            std::process::exit(1);
        }
    });
}

async fn run_example() -> Result<(), Box<dyn std::error::Error>> {
    let local_addr = NetworkAddress::parse("127.0.0.1:4700")?;

    let state_store: Rc<dyn ActorStateStore> = Rc::new(InMemoryStateStore::new());

    let cluster = ClusterConfig::builder()
        .name("banking")
        .topology(vec![local_addr.clone()])
        .build()?;

    let config = NodeConfig::builder().state_store(state_store).build();

    let mut node = MoonpoolNode::new(cluster, config)
        .with_providers(moonpool::TokioProviders::new())
        .register::<BankAccountImpl>()
        .start()
        .await?;

    // Create typed actor references via node.actor_ref()
    let alice: BankAccountRef<_> = node.actor_ref("alice");
    let bob: BankAccountRef<_> = node.actor_ref("bob");

    println!("\n--- Virtual Actor: BankAccount ---\n");

    // Deposit to alice
    let resp = alice.deposit(DepositRequest { amount: 100 }).await?;
    println!("  [client] Alice balance after deposit: {}", resp.balance);
    assert_eq!(resp.balance, 100);

    // Deposit to bob
    let resp = bob.deposit(DepositRequest { amount: 50 }).await?;
    println!("  [client] Bob balance after deposit: {}", resp.balance);
    assert_eq!(resp.balance, 50);

    // Transfer: withdraw from alice, deposit to bob
    let resp = alice.withdraw(WithdrawRequest { amount: 30 }).await?;
    println!("  [client] Alice balance after withdraw: {}", resp.balance);
    assert_eq!(resp.balance, 70);

    let resp = bob.deposit(DepositRequest { amount: 30 }).await?;
    println!(
        "  [client] Bob balance after transfer deposit: {}",
        resp.balance
    );
    assert_eq!(resp.balance, 80);

    // Check final balances
    let alice_balance = alice.get_balance(GetBalanceRequest {}).await?;
    assert_eq!(alice_balance.balance, 70);
    println!("  [client] Alice final balance: {}", alice_balance.balance);

    let bob_balance = bob.get_balance(GetBalanceRequest {}).await?;
    assert_eq!(bob_balance.balance, 80);
    println!("  [client] Bob final balance: {}", bob_balance.balance);

    println!("\n--- Results ---\n");
    println!("  Virtual actors (BankAccount): alice=70, bob=80");

    node.shutdown().await?;

    println!("\nExample completed successfully!");
    Ok(())
}
