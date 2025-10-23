//! Hello Actor Example - Virtual actor demonstration.
//!
//! This example demonstrates Orleans-inspired virtual actors with auto-activation.
//! Can run in standalone mode (single node) or distributed mode (multiple nodes).
//!
//! # Usage
//!
//! Standalone (single node):
//! ```bash
//! cargo run --example hello_actor -- --port 5000
//! ```
//!
//! Distributed (two nodes, terminal 1):
//! ```bash
//! cargo run --example hello_actor -- --port 5000 --ports 5000,5001
//! ```
//!
//! Distributed (two nodes, terminal 2):
//! ```bash
//! cargo run --example hello_actor -- --port 5001 --ports 5000,5001
//! ```
//!
//! # What You'll See
//!
//! - Auto-activation of virtual actors on first message
//! - Directory-based placement decisions
//! - Rich tracing showing actor instantiation
//! - Clean extension trait API
//!
//! # Note
//!
//! Distributed mode requires network transport (not yet wired up).
//! Use standalone mode to see virtual actors working locally.

mod hello_actor;

use clap::Parser;
use hello_actor::{HelloActor, HelloActorFactory, HelloActorRef};
use moonpool::directory::SimpleDirectory;
use moonpool::prelude::*;
use std::time::Duration;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

/// CLI arguments
#[derive(Parser, Debug)]
#[command(name = "hello_actor")]
#[command(about = "Virtual actor example with auto-activation", long_about = None)]
struct Args {
    /// Port to listen on for this node
    #[arg(short, long, default_value = "5000")]
    port: u16,

    /// Comma-separated list of all ports in the cluster (optional, defaults to single-node)
    /// Example: --ports 5000,5001,5002
    #[arg(long)]
    ports: Option<String>,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    // Parse CLI arguments
    let args = Args::parse();

    // Initialize tracing with nice formatting
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,moonpool=debug".into()),
        )
        .with(tracing_subscriber::fmt::layer().with_target(false))
        .init();

    let is_distributed = args.ports.is_some();
    let mode = if is_distributed {
        "distributed"
    } else {
        "standalone"
    };

    tracing::info!(
        port = args.port,
        mode = mode,
        "🚀 Starting virtual actor hello example"
    );

    // Create local task set for !Send actors
    let local = tokio::task::LocalSet::new();

    local
        .run_until(async move {
            if let Err(e) = run_node(args).await {
                tracing::error!(error = ?e, "Node failed");
            }
        })
        .await;

    Ok(())
}

async fn run_node(args: Args) -> std::result::Result<(), ActorError> {
    let port = args.port;

    // Parse cluster nodes from --ports argument
    let cluster_nodes = if let Some(ports_str) = args.ports {
        // Distributed mode: parse comma-separated port list
        let ports: std::result::Result<Vec<u16>, _> =
            ports_str.split(',').map(|s| s.trim().parse()).collect();

        let ports = ports
            .map_err(|e| ActorError::InvalidConfiguration(format!("Invalid ports list: {}", e)))?;

        ports
            .into_iter()
            .map(|p| NodeId::from(format!("127.0.0.1:{}", p)))
            .collect::<std::result::Result<Vec<_>, _>>()?
    } else {
        // Standalone mode: single node
        vec![NodeId::from(format!("127.0.0.1:{}", port))?]
    };

    let mode = if cluster_nodes.len() > 1 {
        "distributed"
    } else {
        "standalone"
    };

    tracing::info!(
        port = port,
        cluster_size = cluster_nodes.len(),
        mode = mode,
        "Initializing ActorRuntime with SimpleDirectory"
    );

    // Create shared directory
    let directory = SimpleDirectory::new(cluster_nodes);

    let runtime = ActorRuntime::<moonpool_foundation::TokioTaskProvider>::builder()
        .namespace("example")
        .listen_addr(format!("127.0.0.1:{}", port))?
        .directory(directory)
        .build()
        .await?;

    tracing::info!(
        node_id = %runtime.node_id(),
        namespace = runtime.namespace(),
        "✅ ActorRuntime started"
    );

    // Register HelloActor with factory
    tracing::info!("Registering HelloActor type with factory");
    runtime.register_actor::<HelloActor, _>(HelloActorFactory)?;
    tracing::info!("✅ HelloActor registered (auto-activation enabled)");

    // Small delay for startup
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Demonstrate calling multiple virtual actors
    // Each actor will auto-activate on first message
    tracing::info!("🎭 Demonstrating virtual actor calls...");
    println!();

    // Define actor keys - these will be placed by the directory
    let actor_keys = ["alice", "bob", "charlie", "david", "eve"];

    // Call each actor using the extension trait
    for key in &actor_keys {
        tracing::info!(key = key, "Getting ActorRef (no activation yet)");

        // Get actor reference - this is lightweight, no network call
        let actor = runtime.get_actor::<HelloActor>("HelloActor", *key)?;

        tracing::info!(
            key = key,
            actor_id = %actor.actor_id(),
            "Got ActorRef, sending first message (will trigger auto-activation)"
        );

        // First message triggers auto-activation!
        // The extension trait makes this look like a regular async method call
        match actor.say_hello().await {
            Ok(greeting) => {
                println!("  📬 {}: {}", key.to_uppercase(), greeting);
            }
            Err(e) => {
                tracing::error!(key = key, error = ?e, "Failed to call actor");
            }
        }

        // Small delay for readability
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    println!();
    tracing::info!("✅ All actor calls completed");
    tracing::info!("💡 Virtual actors are now activated and ready for more messages");

    // Call alice again to show it's still activated
    println!();
    tracing::info!("Calling alice again (already activated, no new instance)");
    let alice = runtime.get_actor::<HelloActor>("HelloActor", "alice")?;
    let greeting = alice.say_hello().await?;
    println!("  📬 ALICE (2nd call): {}", greeting);

    println!();
    tracing::info!("🎉 Example complete! Press Ctrl+C to exit");

    // Keep runtime alive
    loop {
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
}
