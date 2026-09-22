//! The relay process: a second process group that calls the server itself,
//! so process-to-process calls ride the same runtime code as the workload's.

use futures::StreamExt;
use futures::stream::FuturesUnordered;

use async_trait::async_trait;
use moonpool_rpc::{AccessClass, Execution, IncomingRequest, RpcHandle, ServiceRef};
use moonpool_sim::{
    Process, SimContext, SimProviders, SimulationError, SimulationResult, StateHandle,
    assert_always,
};

use super::messages::{Echo, Probe, Relay, RelayOutcome, Relayed};
use super::state::{Board, RELAY_REF_KEY, SERVER_REFS_KEY, ServerRefs, bump};
use super::{CALL_TIMEOUT, listen, report_stats};

/// The relay role.
pub struct RelayProcess {
    /// Run sessions over the corrupting wire.
    pub corrupt_wire: bool,
}

#[async_trait]
impl Process for RelayProcess {
    fn name(&self) -> &'static str {
        "relay"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let state = ctx.state().clone();
        let boot = bump(&state, "rpc.relay.boots");
        let board = Board::of(&state);
        let label = format!("relay#{boot}");
        let (driver, rpc) = listen(ctx, self.corrupt_wire).await?;
        if let Some(probe) = rpc.probe() {
            board.register_probe(&label, probe);
        }
        let (relay_ref, mut stream) = rpc
            .register::<Relay>(AccessClass::Public)
            .map_err(|error| SimulationError::InvalidState(format!("register: {error}")))?;
        state.publish(RELAY_REF_KEY, relay_ref.to_bytes());

        let serve = async {
            let mut inflight = FuturesUnordered::new();
            loop {
                moonpool_sim::select! {
                    incoming = stream.next() => match incoming {
                        Some(incoming) => inflight.push(forward(&rpc, &state, incoming)),
                        None => return,
                    },
                    Some(()) = inflight.next(), if !inflight.is_empty() => {}
                }
            }
        };
        let report = report_stats(&rpc, &board, &label, ctx);
        moonpool_sim::select! {
            error = driver.run() => Err(SimulationError::IoError(format!("rpc driver: {error}"))),
            () = serve => Ok(()),
            () = report => Ok(()),
            () = ctx.shutdown().cancelled() => Ok(()),
        }
    }
}

/// One forwarded attempt: the relay's own single call to the server's echo,
/// whose outcome it reports back truthfully.
async fn forward(
    rpc: &RpcHandle<SimProviders>,
    state: &StateHandle,
    IncomingRequest { request, reply }: IncomingRequest<Relay>,
) {
    let id = request.id;
    // The relay learns the server's current reference explicitly each time.
    let echo = state
        .get::<ServerRefs>(SERVER_REFS_KEY)
        .and_then(|refs| ServiceRef::<Echo>::from_bytes(&refs.echo).ok());
    let (outcome, text) = match echo {
        None => (RelayOutcome::NotAdmitted, String::new()),
        Some(echo) => match echo
            .bind(rpc)
            .try_get_reply_within(&Probe::new(id), CALL_TIMEOUT)
            .await
        {
            Ok(echoed) => {
                assert_always!(
                    echoed.id == id && request.is_intact() && echoed.text == request.text,
                    "a relayed reply matches its request"
                );
                (RelayOutcome::Replied, echoed.text)
            }
            Err(error) => (
                match error.execution() {
                    Execution::NotAdmitted => RelayOutcome::NotAdmitted,
                    Execution::Executed => RelayOutcome::Executed,
                    _ => RelayOutcome::MaybeExecuted,
                },
                String::new(),
            ),
        },
    };
    let _ = reply.send(&Relayed {
        id,
        outcome: outcome as u32,
        text,
    });
}
