//! Fan-Out All Example — TLog-style commit **replication**.
//!
//! Demonstrates [`fan_out_all`] against three TLog replicas. The commit is
//! durable only after **every** replica has acknowledged it. Replies come
//! back in **input order**, so `acks[i]` corresponds to `endpoints[i]`.
//!
//! # Replication, not voting
//!
//! This is deliberately framed as replication, not quorum. FDB's TLog
//! commit path with the default `antiQuorum = 0` is exactly
//! "all-must-succeed": losing any replica loses durability. The
//! [`fan_out_all`] primitive expresses that shape directly. The separate
//! [`fan_out_quorum`] helper is for K-of-N **voting** patterns like Paxos,
//! not for replication.
//!
//! [`fan_out_quorum`]: moonpool_transport::fan_out_quorum
//!
//! # Run
//!
//! ```bash
//! cargo run --example fan_out_all_demo
//! ```

use std::rc::Rc;

use moonpool_transport::{
    JsonCodec, NetTransportBuilder, NetworkAddress, Providers, RpcError, ServiceEndpoint,
    TaskProvider, TokioProviders, fan_out_all, service,
};
use serde::{Deserialize, Serialize};

// ============================================================================
// Configuration
// ============================================================================

const REPLICA_ADDRS: [&str; 3] = ["127.0.0.1:4710", "127.0.0.1:4711", "127.0.0.1:4712"];
const CLIENT_ADDR: &str = "127.0.0.1:4713";

// ============================================================================
// Message Types
// ============================================================================

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct CommitRequest {
    version: u64,
    payload: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct CommitAck {
    tlog_id: u32,
    persisted_version: u64,
}

// ============================================================================
// Interface Definition
// ============================================================================

#[service(id = 0x544C_4F47)] // "TLOG"
trait TLog {
    async fn commit(&self, req: CommitRequest) -> Result<CommitAck, RpcError>;
}

// ============================================================================
// Replica
// ============================================================================

async fn start_replica(
    providers: &TokioProviders,
    tlog_id: u32,
    addr: NetworkAddress,
) -> Result<(), Box<dyn std::error::Error>> {
    let transport = Rc::new(
        NetTransportBuilder::new(providers.clone())
            .local_address(addr.clone())
            .build_listening()
            .await?,
    );
    let server = TLogServer::init(&transport, JsonCodec);
    let transport_for_task = Rc::clone(&transport);

    providers
        .task()
        .spawn_task(&format!("tlog_{}", tlog_id), async move {
            while let Some((req, reply)) = server
                .commit
                .recv_with_transport::<_, CommitAck>(&transport_for_task)
                .await
            {
                println!(
                    "  tlog {} persisting version {} ({} bytes)",
                    tlog_id,
                    req.version,
                    req.payload.len(),
                );
                reply.send(CommitAck {
                    tlog_id,
                    persisted_version: req.version,
                });
            }
        });

    println!("tlog {} listening on {}", tlog_id, addr);
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

    let replicas: Vec<TLogClient<JsonCodec>> = REPLICA_ADDRS
        .iter()
        .map(|addr_str| {
            let addr = NetworkAddress::parse(addr_str).expect("parse replica addr");
            TLogClient::new(addr, JsonCodec)
        })
        .collect();

    // fan_out_all takes a plain slice of endpoints — no Alternatives / no
    // QueueModel. One clone per replica.
    let endpoints: Vec<ServiceEndpoint<CommitRequest, CommitAck, JsonCodec>> =
        replicas.iter().map(|c| c.commit.clone()).collect();

    println!("\n=== Replicating commit to {} tlogs ===", endpoints.len());
    let acks = fan_out_all(
        &transport,
        &endpoints,
        CommitRequest {
            version: 1000,
            payload: "hello-world".into(),
        },
    )
    .await?;

    println!("\ncommit v1000 durable across all {} replicas:", acks.len());
    // Input-order guarantee: acks[i] is the reply from endpoints[i].
    for (i, ack) in acks.iter().enumerate() {
        println!(
            "  replica {} (tlog_id={}) persisted version {}",
            i, ack.tlog_id, ack.persisted_version,
        );
    }

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
        println!("=== fan_out_all Example (TLog replication) ===\n");

        let providers = TokioProviders::new();

        for (id, addr_str) in REPLICA_ADDRS.iter().enumerate() {
            let addr = NetworkAddress::parse(addr_str).expect("parse addr");
            if let Err(e) = start_replica(&providers, id as u32, addr).await {
                eprintln!("tlog {} failed to start: {}", id, e);
                std::process::exit(1);
            }
        }

        if let Err(e) = run_client(&providers).await {
            eprintln!("client error: {}", e);
            std::process::exit(1);
        }
    });
}
