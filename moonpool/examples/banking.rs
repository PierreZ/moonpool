//! Banking Example: Virtual actors with state persistence.
//!
//! This example demonstrates the virtual actor networking layer running
//! alongside static RPC interfaces on the same transport, with
//! `PersistentState<T>` for durable actor state.
//!
//! # What It Shows
//!
//! - A static `PingPong` interface (existing pattern, proves coexistence)
//! - A virtual `BankAccount` actor defined with `#[service]` macro
//! - **State persistence**: `PersistentState<BankAccountData>` + `InMemoryStateStore`
//! - **Lifecycle**: `on_activate` loads state, `DeactivateAfterIdle` removes actor after idle timeout
//! - Typed `BankAccountRef` for ergonomic actor calls
//! - Single process, local delivery, single transport
//!
//! # Architecture
//!
//! ```text
//! EndpointMap:
//!   UID(0x5049_4E47, 1) → PingPong::ping RequestStream   (static, index 1)
//!   UID(0xBA4E_4B00, 0) → BankAccount handler             (virtual, all methods)
//! ```

use std::rc::Rc;
use std::time::Duration;

use moonpool::actors::{
    ActorContext, ActorError, ActorHandler, ActorRouter, ActorStateStore, ClusterConfig,
    DeactivationHint, InMemoryStateStore, MoonpoolNode, PersistentState,
};
use moonpool::{
    Endpoint, JsonCodec, MessageCodec, NetworkAddress, Providers, RpcError, UID, actor_impl,
    service,
};
use serde::{Deserialize, Serialize};

// ============================================================================
// Static Interface: PingPong (proves coexistence)
// ============================================================================

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct PingRequest {
    seq: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct PingResponse {
    seq: u32,
}

#[service(id = 0x5049_4E47)]
trait PingPong {
    async fn ping(&self, req: PingRequest) -> Result<PingResponse, RpcError>;
}

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
    async fn deposit(&mut self, req: DepositRequest) -> Result<BalanceResponse, RpcError> {
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

    async fn withdraw(&mut self, req: WithdrawRequest) -> Result<BalanceResponse, RpcError> {
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

    async fn get_balance(&mut self, req: GetBalanceRequest) -> Result<BalanceResponse, RpcError> {
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
// Static PingPong server loop (runs alongside ActorHost)
// ============================================================================

async fn run_ping_server<P: Providers>(
    transport: Rc<moonpool::NetTransport<P>>,
    ping_server: PingPongServer<JsonCodec>,
) {
    // Process 2 ping messages
    for _ in 0..2 {
        if let Some((req, reply)) = ping_server
            .ping
            .recv_with_transport::<_, PingResponse>(&transport)
            .await
        {
            println!("  [server] PING seq={}", req.seq);
            reply.send(PingResponse { seq: req.seq });
        }
    }
}

// ============================================================================
// Client: uses BankAccountRef for virtual actors, bound client for static
// ============================================================================

async fn run_client<P: Providers>(
    router: Rc<ActorRouter<P>>,
    transport: Rc<moonpool::NetTransport<P>>,
    local_addr: NetworkAddress,
) -> Result<(), Box<dyn std::error::Error>> {
    // Set up static PingPong client
    let ping_client = PingPongClient::new(local_addr).bind(transport, JsonCodec);

    println!("\n--- Virtual Actor: BankAccount ---\n");

    // Create typed actor references (no network call — just handles)
    let alice = BankAccountRef::new("alice", &router);
    let bob = BankAccountRef::new("bob", &router);

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

    println!("\n--- Static Interface: PingPong ---\n");

    // Also call static endpoint to prove coexistence
    let pong = ping_client.ping(PingRequest { seq: 42 }).await?;
    assert_eq!(pong.seq, 42);
    println!("  [client] Static ping seq=42 → pong seq={}", pong.seq);

    // One more ping
    let pong = ping_client.ping(PingRequest { seq: 99 }).await?;
    assert_eq!(pong.seq, 99);
    println!("  [client] Static ping seq=99 → pong seq={}", pong.seq);

    println!("\n--- Results ---\n");
    println!("  Virtual actors (BankAccount): alice=70, bob=80");
    println!("  Static endpoint (PingPong): 2 successful pings");
    println!("  Both coexist on the same transport!");

    Ok(())
}

// ============================================================================
// Main
// ============================================================================

fn main() {
    println!("=== Banking Example: Virtual Actors + Static Endpoints ===\n");

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
    // Shared local address for single-process local delivery
    let local_addr = NetworkAddress::parse("127.0.0.1:4700")?;

    // State store for persistent actor state
    let state_store: Rc<dyn ActorStateStore> = Rc::new(InMemoryStateStore::new());

    // Build and start a MoonpoolNode with banking actors
    let local_endpoint = Endpoint::new(
        local_addr.clone(),
        UID::new(BankAccountRef::<moonpool::TokioProviders>::ACTOR_TYPE.0, 0),
    );
    let placement = Rc::new(moonpool::actors::LocalPlacement::new(local_endpoint));

    let mut node = MoonpoolNode::join(ClusterConfig::single_node(local_addr.clone()))
        .with_providers(moonpool::TokioProviders::new())
        .with_address(local_addr.clone())
        .with_placement(placement)
        .with_state_store(state_store)
        .register::<BankAccountImpl>()
        .start()
        .await?;

    // Register static PingPong handler on the same transport
    let ping_server = PingPongServer::init(node.transport(), JsonCodec);

    // Spawn static PingPong server loop as a background task
    let server_transport = node.transport().clone();
    tokio::task::spawn_local(async move {
        run_ping_server(server_transport, ping_server).await;
    });

    // Run client (sends requests that are delivered locally)
    run_client(node.router().clone(), node.transport().clone(), local_addr).await?;

    node.shutdown().await?;

    println!("\nExample completed successfully!");
    Ok(())
}
