//! Load-Balanced Reads Example.
//!
//! Demonstrates [`load_balance`] over a group of equivalent KV read replicas.
//!
//! The client builds an [`Alternatives`] list from three replica endpoints,
//! creates a fresh [`QueueModel`], and issues one read. `load_balance` picks
//! the best alternative (lowest smoothed outstanding count), sends the
//! request via `get_reply`, and returns the response.
//!
//! # Run
//!
//! ```bash
//! cargo run --example load_balanced_reads
//! ```
//!
//! Three server tasks and the client all run inside the same
//! `current_thread` tokio runtime. Server tasks are spawned via
//! `TaskProvider::spawn_task` — **never** `tokio::spawn` directly — so the
//! example honours the project-wide provider rule in `CLAUDE.md`.

use std::rc::Rc;

use moonpool_transport::{
    Alternatives, AtMostOnce, Distance, FailureStatus, JsonCodec, NetTransportBuilder,
    NetworkAddress, Providers, QueueModel, RpcError, TaskProvider, TokioProviders, load_balance,
    service,
};
use serde::{Deserialize, Serialize};

// ============================================================================
// Configuration
// ============================================================================

const REPLICA_ADDRS: [&str; 3] = ["127.0.0.1:4700", "127.0.0.1:4701", "127.0.0.1:4702"];
const CLIENT_ADDR: &str = "127.0.0.1:4703";

// ============================================================================
// Message Types
// ============================================================================

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct GetRequest {
    key: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct GetResponse {
    value: Option<String>,
    replica_id: u32,
}

// ============================================================================
// Interface Definition
// ============================================================================

#[service(id = 0x4C42_5244)] // "LBRD"
trait KvRead {
    async fn get(&self, req: GetRequest) -> Result<GetResponse, RpcError>;
}

// ============================================================================
// Server Replica
// ============================================================================

/// Build a replica transport, register the `KvRead` service on it, and spawn
/// a background task that echoes every `get` request back with the replica's
/// own id baked into the response.
async fn start_replica(
    providers: &TokioProviders,
    replica_id: u32,
    addr: NetworkAddress,
) -> Result<(), Box<dyn std::error::Error>> {
    let transport = Rc::new(
        NetTransportBuilder::new(providers.clone())
            .local_address(addr.clone())
            .build_listening()
            .await?,
    );
    let server = KvReadServer::init(&transport, JsonCodec);
    let transport_for_task = Rc::clone(&transport);

    providers
        .task()
        .spawn_task(&format!("kv_replica_{}", replica_id), async move {
            while let Some((req, reply)) = server
                .get
                .recv_with_transport::<_, GetResponse>(&transport_for_task)
                .await
            {
                let response = GetResponse {
                    value: Some(format!("replica-{}:{}", replica_id, req.key)),
                    replica_id,
                };
                println!(
                    "  replica {} received get(key={:?}) -> {:?}",
                    replica_id, req.key, response.value,
                );
                reply.send(response);
            }
        });

    // Keep `transport` alive by holding its Rc — dropping it here would
    // decrement the strong count from 2 back to 1 (the clone inside the
    // task), which is fine. The outer Rc goes out of scope at return.
    println!("replica {} listening on {}", replica_id, addr);
    Ok(())
}

// ============================================================================
// Client
// ============================================================================

async fn run_client(providers: &TokioProviders) -> Result<(), Box<dyn std::error::Error>> {
    let client_addr = NetworkAddress::parse(CLIENT_ADDR)?;
    let transport = NetTransportBuilder::new(providers.clone())
        .local_address(client_addr)
        .build_listening()
        .await?;

    // Register each replica address with the failure monitor as Available.
    // load_balance() skips alternatives whose FailureMonitor::state is
    // Failed, and the default for unknown addresses is Failed. In a live
    // deployment the peer connection layer would flip these to Available
    // after a successful connect — here we mark them manually so the very
    // first load_balance call can find them.
    for addr_str in &REPLICA_ADDRS {
        transport
            .failure_monitor()
            .set_status(addr_str, FailureStatus::Available);
    }

    // Build one KvReadClient per replica address. Each client's `get` field
    // is a typed ServiceEndpoint pointing at that replica.
    let replicas: Vec<KvReadClient<JsonCodec>> = REPLICA_ADDRS
        .iter()
        .map(|addr_str| {
            let addr = NetworkAddress::parse(addr_str).expect("parse replica addr");
            KvReadClient::new(addr, JsonCodec)
        })
        .collect();

    // `Alternatives` holds cloned ServiceEndpoints tagged with locality.
    // Here we treat all three replicas as SameDc — a real deployment would
    // tag local-DC replicas SameDc and cross-DC replicas Remote.
    let alts = Alternatives::new(replicas.iter().map(|c| (c.get.clone(), Distance::SameDc)));

    // One QueueModel is shared across every load_balance call — wrap it in
    // an Rc if multiple concurrent callers need it.
    let model = QueueModel::new();

    println!("\n=== Load-balanced read ===");
    let resp = load_balance(
        &transport,
        &alts,
        GetRequest {
            key: "hello".into(),
        },
        AtMostOnce::False, // reads are idempotent
        &model,
    )
    .await?;

    println!(
        "client received: replica={} value={:?}\n",
        resp.replica_id, resp.value,
    );

    Ok(())
}

// ============================================================================
// Main Entry Point
// ============================================================================

fn main() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build_local(Default::default())
        .expect("build tokio runtime");

    runtime.block_on(async {
        println!("=== Load-Balanced Reads Example ===\n");

        let providers = TokioProviders::new();

        // Start three replicas. Each call spawns a background server task
        // via TaskProvider::spawn_task.
        for (id, addr_str) in REPLICA_ADDRS.iter().enumerate() {
            let addr = NetworkAddress::parse(addr_str).expect("parse addr");
            if let Err(e) = start_replica(&providers, id as u32, addr).await {
                eprintln!("replica {} failed to start: {}", id, e);
                std::process::exit(1);
            }
        }

        // Drive the client on the main task. When this returns, `runtime` is
        // dropped and the spawned replica tasks are aborted cleanly.
        if let Err(e) = run_client(&providers).await {
            eprintln!("client error: {}", e);
            std::process::exit(1);
        }
    });
}
