//! Hello Actor Example - Virtual actor demonstration.
//!
//! This example demonstrates Orleans-inspired virtual actors with auto-activation.
//! Can run in standalone mode (single node) or multi-node mode (multiple nodes in same process).
//!
//! # Usage
//!
//! Standalone (single node):
//! ```bash
//! cargo run --example hello_actor
//! ```
//!
//! Multi-node (2 nodes with shared directory):
//! ```bash
//! cargo run --example hello_actor -- --ports 5000,5001
//! ```
//!
//! # What You'll See
//!
//! - Auto-activation of virtual actors on first message
//! - Directory-based placement decisions (shared across all nodes)
//! - Cross-node actor calls via network transport
//! - Rich tracing showing actor instantiation
//! - Clean extension trait API

mod hello_actor;

use clap::Parser;
use hello_actor::{HelloActor, HelloActorFactory, HelloActorRef};
use moonpool::directory::SimpleDirectory;
use moonpool::prelude::*;
use std::rc::Rc;
use std::time::Duration;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

/// CLI arguments
#[derive(Parser, Debug)]
#[command(name = "hello_actor")]
#[command(about = "Virtual actor example with auto-activation", long_about = None)]
struct Args {
    /// Comma-separated list of all ports in the cluster (defaults to single node on 5000)
    /// Example: --ports 5000,5001,5002
    #[arg(long, default_value = "5000")]
    ports: String,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    // Parse CLI arguments
    let args = Args::parse();

    // Initialize tracing with nice formatting
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .with(tracing_subscriber::fmt::layer().with_target(false))
        .init();

    // Parse ports list
    let ports: Vec<u16> = args
        .ports
        .split(',')
        .map(|s| s.trim().parse())
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|e| format!("Invalid port number: {}", e))?;

    let mode = if ports.len() > 1 {
        "multi-node"
    } else {
        "standalone"
    };

    tracing::info!(
        ports = ?ports,
        mode = mode,
        "🚀 Starting virtual actor hello example"
    );

    // Create local task set for !Send actors
    let local = tokio::task::LocalSet::new();

    local
        .run_until(async move {
            if let Err(e) = run_cluster(ports).await {
                tracing::error!(error = ?e, "Cluster failed");
            }
        })
        .await;

    Ok(())
}

async fn run_cluster(ports: Vec<u16>) -> std::result::Result<(), ActorError> {
    // Create cluster node IDs from ports
    let cluster_nodes: Vec<NodeId> = ports
        .iter()
        .map(|p| NodeId::from(format!("127.0.0.1:{}", p)))
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|e| ActorError::InvalidConfiguration(format!("Invalid node ID: {}", e)))?;

    tracing::info!(
        cluster_size = cluster_nodes.len(),
        "Creating shared directory for all nodes"
    );

    // Create ONE shared directory instance that all runtimes will use
    let shared_directory = Rc::new(SimpleDirectory::new(cluster_nodes.clone()));

    // Create a runtime for each port, all sharing the same directory
    let mut runtimes = Vec::new();
    for port in &ports {
        tracing::info!(port = port, "Creating ActorRuntime");

        let runtime = ActorRuntime::<moonpool_foundation::TokioTaskProvider>::builder()
            .namespace("example")
            .listen_addr(format!("127.0.0.1:{}", port))?
            .directory((*shared_directory).clone()) // Clone the Rc, not the directory
            .with_providers(
                moonpool_foundation::TokioNetworkProvider,
                moonpool_foundation::TokioTimeProvider,
                moonpool_foundation::TokioTaskProvider,
            )
            .build()
            .await?;

        tracing::info!(
            node_id = %runtime.node_id(),
            namespace = runtime.namespace(),
            "✅ ActorRuntime started"
        );

        // Register HelloActor with factory on each runtime
        runtime.register_actor::<HelloActor, _>(HelloActorFactory)?;
        tracing::info!(node_id = %runtime.node_id(), "✅ HelloActor registered");

        runtimes.push(runtime);
    }

    // Small delay for startup
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Use the first runtime to make actor calls
    let runtime = &runtimes[0];

    // Demonstrate calling multiple virtual actors
    tracing::info!(
        "🎭 Demonstrating virtual actor calls from node {}...",
        runtime.node_id()
    );
    println!();

    // Define actor keys - these will be placed by the shared directory
    let actor_keys = ["alice", "bob", "charlie", "david", "eve"];

    // Call each actor using the extension trait
    for key in &actor_keys {
        let actor = runtime.get_actor::<HelloActor>("HelloActor", *key)?;

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
    tracing::info!("✅ All actor calls completed");

    // Call alice again to show it's still activated
    println!();
    tracing::info!("Calling alice again (already activated, no new instance)");
    let alice = runtime.get_actor::<HelloActor>("HelloActor", "alice")?;
    let greeting = alice.say_hello().await?;
    println!("  📬 ALICE (2nd call): {}", greeting);

    println!();
    tracing::info!("🎉 Example complete!");

    Ok(())
}
