//! Fan-Out Quorum Example — Paxos-style accept phase (**voting**).
//!
//! Demonstrates [`fan_out_quorum`] against five Paxos acceptors. The
//! proposer issues one accept request and returns as soon as **three** of
//! the five acceptors have promised the ballot. The remaining two replies
//! are explicitly redundant — the proposer does not wait for them.
//!
//! # Voting, not replication
//!
//! `fan_out_quorum` is the K-of-N voting primitive. It is fundamentally
//! different from [`fan_out_all`], which is about replication ("every
//! replica must ack"). Here the remaining peers after the quorum is met are
//! redundant, not required.
//!
//! Replies come back in **completion order**, not input order — the caller
//! is counting votes, not correlating replies with senders by index.
//!
//! [`fan_out_all`]: moonpool_transport::fan_out_all
//!
//! # Run
//!
//! ```bash
//! cargo run --example fan_out_quorum_demo
//! ```

use std::rc::Rc;

use moonpool_transport::{
    JsonCodec, NetTransportBuilder, NetworkAddress, Providers, RpcError, ServiceEndpoint,
    TaskProvider, TokioProviders, fan_out_quorum, service,
};
use serde::{Deserialize, Serialize};

// ============================================================================
// Configuration
// ============================================================================

const ACCEPTOR_ADDRS: [&str; 5] = [
    "127.0.0.1:4720",
    "127.0.0.1:4721",
    "127.0.0.1:4722",
    "127.0.0.1:4723",
    "127.0.0.1:4724",
];
const PROPOSER_ADDR: [&str; 1] = ["127.0.0.1:4725"];
const MAJORITY: usize = 3;

// ============================================================================
// Message Types
// ============================================================================

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct AcceptRequest {
    ballot: u64,
    value: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct AcceptResponse {
    acceptor_id: u32,
    accepted: bool,
}

// ============================================================================
// Interface Definition
// ============================================================================

#[service(id = 0x5041_5853)] // "PAXS"
trait Acceptor {
    async fn accept(&self, req: AcceptRequest) -> Result<AcceptResponse, RpcError>;
}

// ============================================================================
// Acceptor
// ============================================================================

async fn start_acceptor(
    providers: &TokioProviders,
    acceptor_id: u32,
    addr: NetworkAddress,
) -> Result<(), Box<dyn std::error::Error>> {
    let transport = Rc::new(
        NetTransportBuilder::new(providers.clone())
            .local_address(addr.clone())
            .build_listening()
            .await?,
    );
    let server = AcceptorServer::init(&transport, JsonCodec);
    let transport_for_task = Rc::clone(&transport);

    providers
        .task()
        .spawn_task(&format!("acceptor_{}", acceptor_id), async move {
            while let Some((req, reply)) = server
                .accept
                .recv_with_transport::<_, AcceptResponse>(&transport_for_task)
                .await
            {
                println!(
                    "  acceptor {} received accept(ballot={}, value={:?})",
                    acceptor_id, req.ballot, req.value,
                );
                reply.send(AcceptResponse {
                    acceptor_id,
                    accepted: true,
                });
            }
        });

    println!("acceptor {} listening on {}", acceptor_id, addr);
    Ok(())
}

// ============================================================================
// Proposer
// ============================================================================

async fn run_proposer(providers: &TokioProviders) -> Result<(), Box<dyn std::error::Error>> {
    let proposer_addr = NetworkAddress::parse(PROPOSER_ADDR[0])?;
    let transport = NetTransportBuilder::new(providers.clone())
        .local_address(proposer_addr)
        .build_listening()
        .await?;

    let acceptors: Vec<AcceptorClient<JsonCodec>> = ACCEPTOR_ADDRS
        .iter()
        .map(|addr_str| {
            let addr = NetworkAddress::parse(addr_str).expect("parse acceptor addr");
            AcceptorClient::new(addr, JsonCodec)
        })
        .collect();

    let endpoints: Vec<ServiceEndpoint<AcceptRequest, AcceptResponse, JsonCodec>> =
        acceptors.iter().map(|c| c.accept.clone()).collect();

    println!(
        "\n=== Proposing ballot 42 to {} acceptors (need {} to proceed) ===",
        endpoints.len(),
        MAJORITY,
    );

    let promises = fan_out_quorum(
        &transport,
        &endpoints,
        AcceptRequest {
            ballot: 42,
            value: "proposal-v1".into(),
        },
        MAJORITY,
    )
    .await?;

    println!(
        "\nmajority reached: {} of {} acceptors promised ballot 42",
        promises.len(),
        endpoints.len(),
    );
    // Replies are in completion order, not input order — the proposer is
    // counting votes, not correlating with specific acceptors.
    for promise in &promises {
        println!(
            "  acceptor {} accepted={}",
            promise.acceptor_id, promise.accepted
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
        println!("=== fan_out_quorum Example (Paxos accept phase) ===\n");

        let providers = TokioProviders::new();

        for (id, addr_str) in ACCEPTOR_ADDRS.iter().enumerate() {
            let addr = NetworkAddress::parse(addr_str).expect("parse addr");
            if let Err(e) = start_acceptor(&providers, id as u32, addr).await {
                eprintln!("acceptor {} failed to start: {}", id, e);
                std::process::exit(1);
            }
        }

        if let Err(e) = run_proposer(&providers).await {
            eprintln!("proposer error: {}", e);
            std::process::exit(1);
        }
    });
}
