//! Banking Example: Virtual actors with state persistence across 3 nodes.
//!
//! This example demonstrates the virtual actor networking layer with
//! `PersistentState<T>` for durable actor state, distributed across
//! 3 nodes using round-robin placement.
//!
//! # What It Shows
//!
//! - A virtual `BankAccount` actor defined with `#[service]` macro
//! - **State persistence**: `PersistentState<BankAccountData>` + `InMemoryStateStore`
//! - **Lifecycle**: `on_activate` loads state, `DeactivateAfterIdle` removes actor after idle timeout
//! - **Multi-node**: 3 nodes on same process with `SharedMembership` and `RoundRobinPlacement`
//! - **Directory listing**: Print where each actor was placed
//! - Typed `BankAccountRef` via `node.actor_ref("alice")`
//!
//! # Architecture
//!
//! ```text
//! Node1 (127.0.0.1:4700) ─┐
//! Node2 (127.0.0.1:4701) ─┤── SharedMembership + InMemoryDirectory
//! Node3 (127.0.0.1:4702) ─┘   RoundRobinPlacement distributes actors
//!
//! EndpointMap (per node):
//!   UID(0xBA4E_4B00, 0) → BankAccount handler
//! ```

use std::rc::Rc;
use std::time::Duration;

use moonpool::actors::{
    ActorContext, ActorDirectory, ActorError, ActorHandler, ActorStateStore, ClusterConfig,
    DeactivationHint, InMemoryDirectory, InMemoryStateStore, MoonpoolNode, NodeConfig,
    PersistentState, PlacementStrategy, RoundRobinPlacement, SharedMembership,
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
    println!("=== Banking Example: 3-Node Virtual Actors ===\n");

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
    // --- 3 node addresses ---
    let addr1 = NetworkAddress::parse("127.0.0.1:4700")?;
    let addr2 = NetworkAddress::parse("127.0.0.1:4701")?;
    let addr3 = NetworkAddress::parse("127.0.0.1:4702")?;

    // --- Shared cluster resources ---
    // SharedMembership starts empty; each node self-registers during start().
    let membership = Rc::new(SharedMembership::new());
    let directory: Rc<dyn ActorDirectory> = Rc::new(InMemoryDirectory::new());
    let placement: Rc<dyn PlacementStrategy> = Rc::new(RoundRobinPlacement::new());
    let state_store: Rc<dyn ActorStateStore> = Rc::new(InMemoryStateStore::new());

    let cluster = ClusterConfig::builder()
        .name("banking")
        .membership(membership.clone())
        .directory(directory.clone())
        .build()?;

    // Per-node config: own address, shared placement and state store.
    let make_config = |addr: NetworkAddress| -> NodeConfig {
        NodeConfig::builder()
            .address(addr)
            .placement(placement.clone())
            .state_store(state_store.clone())
            .build()
    };

    // --- Start all 3 nodes before making actor calls ---
    // This ensures all 3 are Active in membership so round-robin sees them.
    let mut node1 = MoonpoolNode::new(cluster.clone(), make_config(addr1.clone()))
        .with_providers(moonpool::TokioProviders::new())
        .register::<BankAccountImpl>()
        .start()
        .await?;

    let mut node2 = MoonpoolNode::new(cluster.clone(), make_config(addr2.clone()))
        .with_providers(moonpool::TokioProviders::new())
        .register::<BankAccountImpl>()
        .start()
        .await?;

    let mut node3 = MoonpoolNode::new(cluster.clone(), make_config(addr3.clone()))
        .with_providers(moonpool::TokioProviders::new())
        .register::<BankAccountImpl>()
        .start()
        .await?;

    println!(
        "Started 3 nodes: {}, {}, {}",
        node1.address(),
        node2.address(),
        node3.address()
    );

    // --- Create typed actor references from node1 ---
    // Any node could be used; round-robin placement determines where actors land.
    let alice: BankAccountRef<_> = node1.actor_ref("alice");
    let bob: BankAccountRef<_> = node1.actor_ref("bob");
    let charlie: BankAccountRef<_> = node1.actor_ref("charlie");

    println!("\n--- Virtual Actor: BankAccount (3 nodes, round-robin) ---\n");

    // Deposit to alice
    let resp = alice.deposit(DepositRequest { amount: 100 }).await?;
    println!("  [client] Alice balance after deposit: {}", resp.balance);
    assert_eq!(resp.balance, 100);

    // Deposit to bob
    let resp = bob.deposit(DepositRequest { amount: 50 }).await?;
    println!("  [client] Bob balance after deposit: {}", resp.balance);
    assert_eq!(resp.balance, 50);

    // Deposit to charlie
    let resp = charlie.deposit(DepositRequest { amount: 75 }).await?;
    println!("  [client] Charlie balance after deposit: {}", resp.balance);
    assert_eq!(resp.balance, 75);

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

    let charlie_balance = charlie.get_balance(GetBalanceRequest {}).await?;
    assert_eq!(charlie_balance.balance, 75);
    println!(
        "  [client] Charlie final balance: {}",
        charlie_balance.balance
    );

    // --- Directory listing: show where each actor was placed ---
    println!("\n--- Actor Directory ---\n");
    let mut entries = directory.list_all().await?;
    entries.sort_by(|a, b| a.actor_id.identity.cmp(&b.actor_id.identity));
    for entry in &entries {
        println!(
            "  BankAccount/{} → {}",
            entry.actor_id.identity, entry.endpoint.address
        );
    }

    println!("\n--- Results ---\n");
    println!("  Virtual actors: alice=70, bob=80, charlie=75");
    println!("  Actors distributed across 3 nodes via round-robin");

    // --- Shutdown all nodes ---
    node1.shutdown().await?;
    node2.shutdown().await?;
    node3.shutdown().await?;

    println!("\nExample completed successfully!");
    Ok(())
}
