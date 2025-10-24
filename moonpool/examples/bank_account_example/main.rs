//! Bank Account Example - State persistence with DeactivateOnIdle across multiple nodes.
//!
//! This example demonstrates Orleans-inspired state management with:
//! - Automatic state loading from storage on activation
//! - State persistence after each operation
//! - DeactivateOnIdle policy (actors deactivate when queue is empty)
//! - State reloading on reactivation (possibly on different node!)
//! - Multi-node deployment with shared storage
//!
//! # Usage
//!
//! Standalone (single node, default settings):
//! ```bash
//! cargo run --example bank_account
//! ```
//!
//! Multi-node (3 nodes starting at port 5000):
//! ```bash
//! cargo run --example bank_account -- --nodes 3
//! ```
//!
//! Custom seed and operations:
//! ```bash
//! cargo run --example bank_account -- --seed 123 --operations 10
//! ```
//!
//! Full example with all options:
//! ```bash
//! cargo run --example bank_account -- --nodes 3 --start-port 5010 --seed 42 --operations 15
//! ```
//!
//! # What You'll See
//!
//! - Actors activate on first deposit/withdraw
//! - State persisted after each operation
//! - Actors deactivate when idle (queue empty)
//! - State RELOADED when actors reactivate (possibly on different node!)
//! - Final balance matches sum of all operations
//! - Actor distribution across nodes

mod bank_account;

use bank_account::{BankAccountActor, BankAccountActorFactory, BankAccountActorRef};
use clap::Parser;
use moonpool::directory::{Directory, SimpleDirectory};
use moonpool::prelude::*;
use moonpool::storage::{InMemoryStorage, StorageProvider};
use rand::Rng;
use rand::SeedableRng;
use rand::rngs::StdRng;
use std::rc::Rc;
use std::time::Duration;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

/// CLI arguments
#[derive(Parser, Debug)]
#[command(name = "bank_account")]
#[command(about = "Bank account example with state persistence across multiple nodes", long_about = None)]
struct Args {
    /// Number of nodes in the cluster (defaults to 2)
    #[arg(long, default_value = "2")]
    nodes: usize,

    /// Starting port for the cluster (defaults to 5000)
    /// Nodes will use sequential ports starting from this value
    #[arg(long, default_value = "5000")]
    start_port: u16,

    /// Random seed for operation generation (optional, uses random seed if not provided)
    #[arg(long)]
    seed: Option<u64>,

    /// Number of random operations to perform (defaults to 8)
    #[arg(long, default_value = "8")]
    operations: usize,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    // Parse CLI arguments
    let args = Args::parse();

    // Initialize tracing with nice formatting
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with(tracing_subscriber::fmt::layer().with_target(true))
        .init();

    // Validate nodes count
    if args.nodes == 0 {
        return Err("Number of nodes must be at least 1".into());
    }

    // Generate sequential ports starting from start_port
    let ports: Vec<u16> = (args.start_port..args.start_port + args.nodes as u16).collect();

    let mode = if ports.len() > 1 {
        "multi-node"
    } else {
        "standalone"
    };

    tracing::info!(
        ports = ?ports,
        mode = mode,
        seed = ?args.seed,
        operations = args.operations,
        "🚀 Starting bank account state persistence example"
    );

    // Create local task set for !Send actors
    let local = tokio::task::LocalSet::new();

    local
        .run_until(async move {
            if let Err(e) = run_cluster(ports, args.seed, args.operations).await {
                tracing::error!(error = ?e, "Cluster failed");
            }
        })
        .await;

    Ok(())
}

async fn run_cluster(
    ports: Vec<u16>,
    seed: Option<u64>,
    operation_count: usize,
) -> std::result::Result<(), ActorError> {
    // Create cluster node IDs from ports
    let cluster_nodes: Vec<NodeId> = ports
        .iter()
        .map(|p| NodeId::from(format!("127.0.0.1:{}", p)))
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|e| ActorError::InvalidConfiguration(format!("Invalid node ID: {}", e)))?;

    tracing::info!(
        cluster_size = cluster_nodes.len(),
        "Creating shared directory and storage for all nodes"
    );

    // Create shared directory for multi-node scenario
    // All runtimes will share the same directory so they can see each other's actors
    let shared_directory: Rc<dyn Directory> = Rc::new(SimpleDirectory::new());

    // Create shared storage for state persistence (CRITICAL: shared across all nodes!)
    // This allows actors to reactivate on different nodes with the same state
    let shared_storage: Rc<dyn StorageProvider> = Rc::new(InMemoryStorage::new());

    // Create a runtime for each port
    let mut runtimes = Vec::new();
    for port in &ports {
        let runtime = ActorRuntime::<moonpool_foundation::TokioTaskProvider>::builder()
            .namespace("example")
            .listen_addr(format!("127.0.0.1:{}", port))?
            .cluster_nodes(cluster_nodes.clone())
            .shared_directory(shared_directory.clone())
            .with_storage(shared_storage.clone())
            .with_providers(
                moonpool_foundation::TokioNetworkProvider,
                moonpool_foundation::TokioTimeProvider,
                moonpool_foundation::TokioTaskProvider,
            )
            .build()
            .await?;

        // Instrument all subsequent logs with node_id (one-liner!)
        let _span = tracing::info_span!("node", node_id = %runtime.node_id()).entered();

        tracing::info!(
            namespace = runtime.namespace(),
            port = port,
            "✅ ActorRuntime started with shared storage"
        );

        // Register BankAccountActor with factory on each runtime
        runtime.register_actor::<BankAccountActor, _>(BankAccountActorFactory)?;
        tracing::info!("✅ BankAccountActor registered (DeactivateOnIdle policy)");

        runtimes.push(runtime);
    }

    // Small delay for startup
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Demonstrate bank account operations from first node
    let runtime = &runtimes[0];
    tracing::info!(
        "💰 Demonstrating bank account operations from node {}...",
        runtime.node_id()
    );

    println!("\n📊 Bank Account State Persistence Demo (Multi-Node)");
    println!("==========================================\n");

    // Get three bank accounts
    let alice = runtime.get_actor::<BankAccountActor>("BankAccount", "alice")?;
    let bob = runtime.get_actor::<BankAccountActor>("BankAccount", "bob")?;
    let charlie = runtime.get_actor::<BankAccountActor>("BankAccount", "charlie")?;

    let accounts = vec![("Alice", alice), ("Bob", bob), ("Charlie", charlie)];

    // Generate random operations using seed
    let actual_seed = seed.unwrap_or_else(|| rand::random());
    let mut rng = StdRng::seed_from_u64(actual_seed);

    tracing::info!(seed = actual_seed, "Generating random operations");

    for op_num in 0..operation_count {
        // Pick random account
        let account_idx = rng.random_range(0..accounts.len());
        let (name, account_ref) = &accounts[account_idx];

        // Random operation: 70% deposit, 30% withdraw
        let is_deposit = rng.random_bool(0.7);
        let amount = rng.random_range(50..500);

        println!("🔸 Operation {}: {}", op_num + 1, name);

        if is_deposit {
            match account_ref.deposit(amount).await {
                Ok(new_balance) => {
                    println!("  ✅ Deposited ${} → New balance: ${}", amount, new_balance);
                }
                Err(e) => {
                    println!("  ❌ Deposit failed: {}", e);
                }
            }
        } else {
            match account_ref.withdraw(amount).await {
                Ok(new_balance) => {
                    println!("  ✅ Withdrew ${} → New balance: ${}", amount, new_balance);
                }
                Err(e) => {
                    println!("  💸 Withdraw failed: {} (insufficient funds)", e);
                }
            }
        }

        // Small delay between operations to allow for deactivation
        // DeactivateOnIdle will trigger when queue is empty
        if op_num < operation_count - 1 {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    println!("\n📈 Final Balances");
    println!("==========================================\n");

    for (name, account_ref) in &accounts {
        match account_ref.get_balance().await {
            Ok(balance) => {
                println!("  💰 {}: ${}", name, balance);
            }
            Err(e) => {
                println!("  ❌ {}: Failed to get balance: {}", name, e);
            }
        }
    }

    println!("\n🎉 Example complete!");
    println!("\n💡 Key observations:");
    println!("  - Actors activate on first message");
    println!("  - State persisted after each deposit/withdraw");
    println!("  - Actors deactivate when idle (DeactivateOnIdle)");
    println!("  - State RELOADED on reactivation (possibly on different node!)");
    println!("  - Final balances reflect all operations");
    println!("  - Shared storage allows state to persist across nodes");
    println!("  - All actors have deactivated after final operations\n");

    // Give tasks time to clean up
    tokio::time::sleep(Duration::from_millis(100)).await;

    Ok(())
}
