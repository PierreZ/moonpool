//! Hello Actor Example - Virtual actor demonstration.
//!
//! This example demonstrates Orleans-inspired virtual actors with auto-activation.
//! Can run in standalone mode (single node) or multi-node mode (multiple nodes in same process).
//!
//! # Usage
//!
//! Standalone (single node, default settings):
//! ```bash
//! cargo run --example hello_actor
//! ```
//!
//! Multi-node (3 nodes starting at port 5000):
//! ```bash
//! cargo run --example hello_actor -- --nodes 3
//! ```
//!
//! Custom seed and count:
//! ```bash
//! cargo run --example hello_actor -- --seed 123 --count 5
//! ```
//!
//! Full example with all options:
//! ```bash
//! cargo run --example hello_actor -- --nodes 3 --start-port 5010 --seed 42 --count 15
//! ```
//!
//! # What You'll See
//!
//! - Auto-activation of virtual actors on first message
//! - Directory-based placement decisions (shared across all nodes)
//! - Cross-node actor calls via network transport
//! - Rich tracing showing actor instantiation
//! - Clean extension trait API
//! - Random name generation using deterministic seed

mod hello_actor;

use clap::Parser;
use fake::Fake;
use fake::faker::name::en::FirstName;
use hello_actor::{HelloActor, HelloActorFactory, HelloActorRef};
use moonpool::directory::{Directory, SimpleDirectory};
use moonpool::prelude::*;
use moonpool::storage::{InMemoryStorage, StorageProvider};
use rand::SeedableRng;
use rand::rngs::StdRng;
use std::rc::Rc;
use std::time::Duration;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

/// CLI arguments
#[derive(Parser, Debug)]
#[command(name = "hello_actor")]
#[command(about = "Virtual actor example with auto-activation", long_about = None)]
struct Args {
    /// Number of nodes in the cluster (defaults to 2)
    #[arg(long, default_value = "2")]
    nodes: usize,

    /// Starting port for the cluster (defaults to 5000)
    /// Nodes will use sequential ports starting from this value
    #[arg(long, default_value = "5000")]
    start_port: u16,

    /// Random seed for name generation (optional, uses random seed if not provided)
    #[arg(long)]
    seed: Option<u64>,

    /// Number of random names to generate and greet (defaults to 10)
    #[arg(long, default_value = "10")]
    count: usize,
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
        count = args.count,
        "🚀 Starting virtual actor hello example"
    );

    // Create local task set for !Send actors
    let local = tokio::task::LocalSet::new();

    local
        .run_until(async move {
            if let Err(e) = run_cluster(ports, args.seed, args.count).await {
                tracing::error!(error = ?e, "Cluster failed");
            }
        })
        .await;

    Ok(())
}

async fn run_cluster(
    ports: Vec<u16>,
    seed: Option<u64>,
    count: usize,
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

    // Create shared storage (even though HelloActor has no state, storage is required)
    let shared_storage: Rc<dyn StorageProvider> = Rc::new(InMemoryStorage::new());

    // Create a runtime for each port
    let mut runtimes = Vec::new();
    for port in &ports {
        let runtime = ActorRuntime::<
            moonpool_foundation::TokioTaskProvider,
            moonpool::serialization::JsonSerializer,
        >::builder()
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
        .with_serializer(moonpool::serialization::JsonSerializer)
        .build()
        .await?;

        // Instrument all subsequent logs with node_id (one-liner!)
        let _span = tracing::info_span!("node", node_id = %runtime.node_id()).entered();

        tracing::info!(
            namespace = runtime.namespace(),
            port = port,
            "✅ ActorRuntime started"
        );

        // Register HelloActor with factory on each runtime
        runtime.register_actor::<HelloActor, _>(HelloActorFactory)?;
        tracing::info!("✅ HelloActor registered");

        runtimes.push(runtime);
    }

    // Small delay for startup
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Demonstrate virtual actor calls from first node
    let runtime = &runtimes[0];
    tracing::info!(
        "🎭 Demonstrating virtual actor calls from node {}...",
        runtime.node_id()
    );
    println!();

    // Generate random first names using the provided seed (or random if not provided)
    let actual_seed = seed.unwrap_or_else(rand::random);
    let mut rng = StdRng::seed_from_u64(actual_seed);
    let actor_keys: Vec<String> = (0..count)
        .map(|_| {
            let name: String = FirstName().fake_with_rng(&mut rng);
            name.to_lowercase()
        })
        .collect();

    tracing::info!(
        seed = actual_seed,
        count = count,
        names = ?actor_keys,
        "Generated random actor names"
    );

    for key in &actor_keys {
        let actor = runtime.get_actor::<HelloActor>("HelloActor", key)?;

        match actor.say_hello().await {
            Ok(greeting) => {
                println!("  📬 {}: {}", key.to_uppercase(), greeting);
            }
            Err(e) => {
                tracing::error!(key = key, error = ?e, "Failed to call actor");
            }
        }

        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    println!();
    tracing::info!("🎉 Example complete!");

    // Print actor distribution across nodes
    println!("\n📊 Actor Distribution:");
    for node in &cluster_nodes {
        let actors = shared_directory.get_actors_on_node(node).await;
        println!("  Node {}: {} actors", node, actors.len());
        for actor_id in actors {
            println!("    - {}", actor_id);
        }
    }

    // Give tasks time to clean up
    tokio::time::sleep(Duration::from_millis(100)).await;

    Ok(())
}
