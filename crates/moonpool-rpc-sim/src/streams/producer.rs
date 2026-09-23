//! The producer: a streaming `ScanItems` endpoint whose handlers write the
//! producer ledger as they go, a unary `Ping`, and a well-known directory
//! that tells callers the current boot's references. On a graceful
//! shutdown it fails its open streams with [`SHUTDOWN_CODE`] and keeps its
//! runtime running for a short grace so the ends reach their consumers.

use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::FuturesUnordered;
use moonpool_rpc::protocol::stream_item_frame_len;
use moonpool_rpc::{AccessClass, IncomingRequest, RequestStream, RpcDriver, SendError};
use moonpool_sim::{
    Process, SimContext, SimulationError, SimulationResult, TimeProvider, assert_always,
    assert_sometimes,
};

use super::messages::{
    Chunk, DIRECTORY_ID, Directory, End, Listing, Ping, SHUTDOWN_CODE, ScanItems,
};
use super::policy::streams_config;
use super::state::{PRODUCER_BOOTS_KEY, PRODUCER_LABEL, ProducerEnd, StreamLedger};
use super::{RPC_PORT, report_stats};
use crate::foundations::state::{Board, bump};

/// The producer role: a fresh incarnation per boot, at the same address.
pub struct StreamsProducer;

/// How long a gracefully shutting-down producer keeps its runtime running
/// so the ends of its streams are written.
const SHUTDOWN_GRACE: Duration = Duration::from_millis(40);

#[async_trait]
impl Process for StreamsProducer {
    fn name(&self) -> &'static str {
        "producer"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let state = ctx.state().clone();
        let boot = bump(&state, PRODUCER_BOOTS_KEY);
        let board = Board::of(&state);
        let label = format!("{PRODUCER_LABEL}{boot}");
        for (_, probe) in board.probes(PRODUCER_LABEL, &label) {
            assert_always!(
                probe.is_released(),
                "a stopped producer released every stream, connection and task"
            );
        }
        let address = format!("{}:{RPC_PORT}", ctx.my_ip());
        let (driver, rpc) = RpcDriver::listen(ctx.providers().clone(), &address, streams_config())
            .await
            .map_err(|error| SimulationError::IoError(format!("rpc listen: {error}")))?;
        if let Some(probe) = rpc.probe() {
            board.register_probe(&label, probe);
        }
        let register_error = |error| SimulationError::InvalidState(format!("register: {error}"));
        let (scan_ref, scans) = rpc
            .register::<ScanItems>(AccessClass::Public)
            .map_err(register_error)?;
        let (ping_ref, pings) = rpc
            .register::<Ping>(AccessClass::Public)
            .map_err(register_error)?;
        let (_, directory) = rpc
            .register_well_known::<Directory>(DIRECTORY_ID, AccessClass::Public)
            .map_err(register_error)?;
        let listing = Listing {
            boot,
            scan: scan_ref.to_bytes(),
            probe: ping_ref.to_bytes(),
        };
        tracing::info!(boot, "rpc_streams_producer_ready");
        let ledger = StreamLedger::of(&state);
        let serve = async {
            futures::join!(
                serve_scans(scans, &ledger, boot, ctx),
                serve_pings(pings, &ledger, ctx),
                serve_directory(directory, &listing, ctx),
                report_stats(&rpc, &board, &label, ctx),
            );
        };
        let graceful = async {
            ctx.shutdown().cancelled().await;
            // The handlers fail their streams on the same signal; keep the
            // runtime running a moment so those ends are written.
            let _ = ctx.time().sleep(SHUTDOWN_GRACE).await;
        };
        moonpool_sim::select! {
            error = driver.run() => Err(SimulationError::IoError(format!("rpc driver: {error}"))),
            () = serve => Ok(()),
            () = graceful => Ok(()),
        }
    }
}

async fn serve_directory(
    mut stream: RequestStream<Directory>,
    listing: &Listing,
    ctx: &SimContext,
) {
    loop {
        moonpool_sim::select! {
            incoming = stream.recv() => match incoming {
                Some(IncomingRequest { reply, .. }) => {
                    let _ = reply.send(listing);
                }
                None => return,
            },
            () = ctx.shutdown().cancelled() => return,
        }
    }
}

async fn serve_pings(mut stream: RequestStream<Ping>, ledger: &StreamLedger, ctx: &SimContext) {
    let mut replies = FuturesUnordered::new();
    loop {
        moonpool_sim::select! {
            incoming = stream.recv() => match incoming {
                Some(IncomingRequest { request, reply }) => {
                    // Recorded first: the ledger judges what a refusal proved.
                    ledger.probe(request.id);
                    replies.push(async move {
                        let hold = Duration::from_millis(u64::from(request.hold_ms));
                        if !hold.is_zero() {
                            let _ = ctx.time().sleep(hold).await;
                        }
                        let _ = reply.send(&request);
                    });
                }
                None => return,
            },
            Some(()) = replies.next(), if !replies.is_empty() => {}
            () = ctx.shutdown().cancelled() => return,
        }
    }
}

async fn serve_scans(
    mut stream: RequestStream<ScanItems>,
    ledger: &StreamLedger,
    boot: u64,
    ctx: &SimContext,
) {
    let mut producers = FuturesUnordered::new();
    loop {
        moonpool_sim::select! {
            incoming = stream.next() => match incoming {
                Some(incoming) => producers.push(produce(incoming, ledger, boot, ctx)),
                None => break,
            },
            Some(()) = producers.next(), if !producers.is_empty() => {}
            () = ctx.shutdown().cancelled() => break,
        }
    }
    // Refuse new streams, and let every producer end its own (on shutdown
    // they fail with SHUTDOWN_CODE at once).
    drop(stream);
    while producers.next().await.is_some() {}
}

fn send_end(error: &SendError) -> ProducerEnd {
    match error {
        SendError::Cancelled => ProducerEnd::Cancelled,
        SendError::Disconnected => ProducerEnd::Disconnected,
        _ => ProducerEnd::Other,
    }
}

/// Serve one stream as its request asks, writing the producer ledger.
async fn produce(
    IncomingRequest { request, reply }: IncomingRequest<ScanItems>,
    ledger: &StreamLedger,
    boot: u64,
    ctx: &SimContext,
) {
    let Ok(producer) = reply.into_stream() else {
        assert_always!(false, "a streaming method's request opens a stream");
        return;
    };
    let id = request.id;
    let window = producer.window();
    ledger.open(id, boot, window);
    let end = End::from_u32(request.end);
    let data = vec![0x5a; usize::try_from(request.item_bytes).unwrap_or(0)];
    let mut seq = 0u64;
    loop {
        if end != End::Endless && seq >= request.count {
            break;
        }
        let delay = if seq == 0 {
            request.first_delay_ms
        } else {
            request.delay_ms
        };
        let pause = async {
            if delay > 0 {
                let _ = ctx
                    .time()
                    .sleep(Duration::from_millis(u64::from(delay)))
                    .await;
            }
        };
        let shutdown = moonpool_sim::select! {
            () = pause => false,
            () = ctx.shutdown().cancelled() => true,
        };
        if shutdown {
            ledger.ended(id, ProducerEnd::Failed(SHUTDOWN_CODE), producer.in_flight());
            let _ = producer.fail(SHUTDOWN_CODE);
            return;
        }
        let chunk = Chunk {
            id,
            seq,
            boot,
            data: data.clone(),
        };
        let size = stream_item_frame_len(prost::Message::encoded_len(&chunk));
        let started = ctx.time().now();
        let sent = moonpool_sim::select! {
            sent = producer.send(&chunk) => Some(sent),
            () = ctx.shutdown().cancelled() => None,
        };
        match sent {
            None => {
                ledger.ended(id, ProducerEnd::Failed(SHUTDOWN_CODE), producer.in_flight());
                let _ = producer.fail(SHUTDOWN_CODE);
                return;
            }
            Some(Ok(())) => {
                let waited = ctx.time().now() > started;
                let (sent_bytes, consumed) = ledger.sent(id, size, waited);
                // The external bound on buffering: the producer never runs
                // further ahead of what the consumer's application took than
                // the window, give or take the one item a waiting consumer
                // was handed (acknowledged on arrival, recorded when its
                // task runs).
                assert_always!(
                    sent_bytes.saturating_sub(consumed) <= window + size,
                    "a producer never runs ahead of consumption beyond its window"
                );
            }
            Some(Err(SendError::TooLarge {
                size: refused,
                limit,
            })) => {
                assert_always!(
                    refused > limit && limit <= window,
                    "an oversized item is refused against the window"
                );
                assert_sometimes!(true, "rpc stream oversized item refused without waiting");
                ledger.ended(id, ProducerEnd::TooLarge, producer.in_flight());
                drop(producer);
                return;
            }
            Some(Err(error)) => {
                if error == SendError::Cancelled {
                    assert_sometimes!(true, "rpc stream producer observed cancellation");
                }
                ledger.ended(id, send_end(&error), producer.in_flight());
                return;
            }
        }
        seq += 1;
    }
    close(producer, end, request.code, id, ledger);
}

/// End a stream the way its request asked, recording it first.
fn close(
    producer: moonpool_rpc::StreamProducer<ScanItems>,
    end: End,
    code: u64,
    id: u64,
    ledger: &StreamLedger,
) {
    let in_flight = producer.in_flight();
    match end {
        End::Fail => {
            ledger.ended(id, ProducerEnd::Failed(code), in_flight);
            let _ = producer.fail(code);
        }
        End::Drop => {
            ledger.ended(id, ProducerEnd::Dropped, in_flight);
            drop(producer);
        }
        End::Finish | End::Endless => {
            ledger.ended(id, ProducerEnd::Finished, in_flight);
            let _ = producer.finish();
        }
    }
}
