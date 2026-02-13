//! Banking Example: Virtual actors + static endpoints coexisting.
//!
//! This example demonstrates the virtual actor networking layer running
//! alongside static RPC interfaces on the same transport.
//!
//! # What It Shows
//!
//! - A static `PingPong` interface (existing pattern, proves coexistence)
//! - A virtual `BankAccount` actor defined with `#[virtual_actor]` macro
//! - `ActorHost` for automatic activation and dispatch (no manual select! loop)
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

use moonpool::actors::{
    ActorContext, ActorDirectory, ActorError, ActorHandler, ActorHost, ActorRouter, ActorType,
    InMemoryDirectory, LocalPlacement, PlacementStrategy,
};
use moonpool::{
    Endpoint, JsonCodec, MessageCodec, NetTransportBuilder, NetworkAddress, Providers, RpcError,
    UID, interface, virtual_actor,
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

#[interface(id = 0x5049_4E47)]
trait PingPong {
    async fn ping(&self, req: PingRequest) -> Result<PingResponse, RpcError>;
}

// ============================================================================
// Virtual Actor: BankAccount (using #[virtual_actor] macro)
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
#[virtual_actor(id = 0xBA4E_4B00)]
trait BankAccount {
    async fn deposit(&mut self, req: DepositRequest) -> Result<BalanceResponse, RpcError>;
    async fn withdraw(&mut self, req: WithdrawRequest) -> Result<BalanceResponse, RpcError>;
    async fn get_balance(&mut self, req: GetBalanceRequest) -> Result<BalanceResponse, RpcError>;
}

/// BankAccount actor implementation.
#[derive(Default)]
struct BankAccountImpl {
    balance: i64,
}

#[async_trait::async_trait(?Send)]
impl BankAccount for BankAccountImpl {
    async fn deposit(&mut self, req: DepositRequest) -> Result<BalanceResponse, RpcError> {
        self.balance += req.amount;
        println!(
            "  [server] DEPOSIT +{} → balance={}",
            req.amount, self.balance
        );
        Ok(BalanceResponse {
            balance: self.balance,
        })
    }

    async fn withdraw(&mut self, req: WithdrawRequest) -> Result<BalanceResponse, RpcError> {
        self.balance -= req.amount;
        println!(
            "  [server] WITHDRAW -{} → balance={}",
            req.amount, self.balance
        );
        Ok(BalanceResponse {
            balance: self.balance,
        })
    }

    async fn get_balance(&mut self, req: GetBalanceRequest) -> Result<BalanceResponse, RpcError> {
        let _ = req;
        println!("  [server] GET_BALANCE → balance={}", self.balance);
        Ok(BalanceResponse {
            balance: self.balance,
        })
    }
}

/// Hand-written bridge: delegates to the generated dispatch function.
/// (Could be macro-generated in a future iteration.)
#[async_trait::async_trait(?Send)]
impl ActorHandler for BankAccountImpl {
    fn actor_type() -> ActorType {
        BankAccountRef::<moonpool::TokioProviders>::ACTOR_TYPE
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

    // Build transport (no listening needed — all delivery is local)
    let providers = moonpool::TokioProviders::new();
    let transport = NetTransportBuilder::new(providers)
        .local_address(local_addr.clone())
        .build()?;

    // Register static PingPong handler
    let ping_server = PingPongServer::init(&transport, JsonCodec);

    // Shared directory for actor location tracking
    let directory: Rc<dyn ActorDirectory> = Rc::new(InMemoryDirectory::new());

    // Build router + host
    let local_endpoint = Endpoint::new(
        local_addr.clone(),
        UID::new(BankAccountRef::<moonpool::TokioProviders>::ACTOR_TYPE.0, 0),
    );
    let placement: Rc<dyn PlacementStrategy> = Rc::new(LocalPlacement::new(local_endpoint));
    let router = Rc::new(ActorRouter::new(
        transport.clone(),
        directory.clone(),
        placement,
        JsonCodec,
    ));
    let host = ActorHost::new(transport.clone(), router.clone(), directory);

    // Register actor type — host spawns processing loop internally
    host.register::<BankAccountImpl>();

    // Spawn static PingPong server loop as a background task
    let server_transport = transport.clone();
    tokio::task::spawn_local(async move {
        run_ping_server(server_transport, ping_server).await;
    });

    // Run client (sends requests that are delivered locally)
    run_client(router, transport, local_addr).await?;

    println!("\nExample completed successfully!");
    Ok(())
}
