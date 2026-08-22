use std::time::Duration;

use async_trait::async_trait;
use futures::io::{AsyncReadExt, AsyncWriteExt};
use moonpool_sim::{
    NetworkProvider, Process, SimContext, SimulationError, SimulationResult, TcpListenerTrait,
    TimeProvider, Workload, assert_always, assert_reachable, assert_sometimes,
};

use crate::protocol::{Frame, decode_frame, encode_frame};

pub(crate) const REQUESTS: u32 = 12;
pub(crate) const CHAOS_DURATION: Duration = Duration::from_secs(10);

const TIMEOUT: Duration = Duration::from_millis(700);
const SLOW_RTT_MS: u64 = 100;

fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// Node B: accepts raw TCP connections and echoes valid ping frames.
pub(crate) struct PongServer;

#[async_trait]
impl Process for PongServer {
    fn name(&self) -> &'static str {
        "pong-server"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let listener = ctx.network().bind(ctx.my_ip()).await?;
        let shutdown = ctx.shutdown().clone();

        loop {
            let accepted = moonpool_sim::select! {
                biased;
                result = listener.accept() => result,
                () = shutdown.cancelled() => return Ok(()),
            };
            let (mut stream, peer) = match accepted {
                Ok(connection) => connection,
                Err(error) => {
                    tracing::warn!(%error, "accept failed under network chaos");
                    continue;
                }
            };

            let exchange = async {
                let mut frame = Frame::default();
                stream.read_exact(&mut frame).await?;
                let Some(sequence) = decode_frame(&frame) else {
                    tracing::warn!(%peer, "discarding corrupt ping frame");
                    return Ok::<(), std::io::Error>(());
                };
                stream.write_all(&encode_frame(sequence)).await
            };

            moonpool_sim::select! {
                biased;
                result = exchange => {
                    if let Err(error) = result {
                        tracing::warn!(%peer, %error, "ping connection failed under network chaos");
                    }
                }
                () = shutdown.cancelled() => return Ok(()),
            }
        }
    }
}

async fn exchange_ping(ctx: &SimContext, server_ip: &str, sequence: u64) -> SimulationResult<u64> {
    let mut stream = ctx.network().connect(server_ip).await?;
    stream
        .write_all(&encode_frame(sequence))
        .await
        .map_err(|error| SimulationError::InvalidState(format!("write ping: {error}")))?;

    let mut response = Frame::default();
    stream
        .read_exact(&mut response)
        .await
        .map_err(|error| SimulationError::InvalidState(format!("read pong: {error}")))?;
    decode_frame(&response)
        .ok_or_else(|| SimulationError::InvalidState("corrupt pong frame".into()))
}

/// Node A: sends raw pings and emits the recorder's client event contract.
pub(crate) struct PingClient;

#[async_trait]
impl Workload for PingClient {
    fn name(&self) -> &'static str {
        "ping-client"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let Some(server_ip) = ctx.topology().all_process_ips().first().cloned() else {
            return Ok(());
        };
        let time = ctx.time().clone();
        let shutdown = ctx.shutdown().clone();

        for sequence in 0..u64::from(REQUESTS) {
            if shutdown.is_cancelled() {
                break;
            }

            let sent_at = time.now();
            tracing::info!(seq_id = sequence, "client_issued");
            let result = moonpool_sim::select! {
                biased;
                result = exchange_ping(ctx, &server_ip, sequence) => Some(result),
                () = shutdown.cancelled() => None,
                _ = time.sleep(TIMEOUT) => None,
            };

            if let Some(Ok(pong_sequence)) = result {
                assert_always!(
                    pong_sequence == sequence,
                    "pong echoes the ping it answered"
                );
                let rtt_ms = duration_ms(time.now().saturating_sub(sent_at));
                assert_sometimes!(rtt_ms >= SLOW_RTT_MS, "a round trip is slowed by chaos");
                tracing::info!(seq_id = sequence, "client_acknowledged");
            } else {
                assert_reachable!("chaos drops a request before any pong returns");
                tracing::info!(seq_id = sequence, "client_failed");
            }
        }
        Ok(())
    }
}
