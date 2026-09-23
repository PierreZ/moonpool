//! The third participant: it never recruits anything itself. It adopts the
//! members the workload recruited — their interfaces arrive inside a
//! request — and calls each of them through its own runtime.

use std::time::Duration;

use async_trait::async_trait;
use moonpool_rpc::{AccessClass, IncomingRequest, RequestStream, RpcDriver, RpcHandle};
use moonpool_sim::{
    Process, SimContext, SimProviders, SimulationError, SimulationResult, assert_always,
};

use super::messages::{ADOPTER_ID, Adopt, Adopted, Adopter, Probe, RoleClient};
use crate::foundations::state::Board;
use crate::foundations::{RPC_PORT, report_stats, rpc_config};

/// Deadline of each call the third participant makes.
const MEMBER_TIMEOUT: Duration = Duration::from_secs(3);

/// The third participant role.
pub struct Third;

#[async_trait]
impl Process for Third {
    fn name(&self) -> &'static str {
        "third"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let board = Board::of(ctx.state());
        let label = format!("third@{}", ctx.my_ip());
        let address = format!("{}:{RPC_PORT}", ctx.my_ip());
        let (driver, rpc) = RpcDriver::listen(ctx.providers().clone(), &address, rpc_config())
            .await
            .map_err(|error| SimulationError::IoError(format!("rpc listen: {error}")))?;
        let (_, adopter) = rpc
            .register_well_known::<Adopter>(ADOPTER_ID, AccessClass::Public)
            .map_err(|error| SimulationError::InvalidState(format!("register: {error}")))?;
        moonpool_sim::select! {
            error = driver.run() => Err(SimulationError::IoError(format!("rpc driver: {error}"))),
            () = adopt(&rpc, adopter) => Ok(()),
            () = report_stats(&rpc, &board, &label, ctx) => Ok(()),
            () = ctx.shutdown().cancelled() => Ok(()),
        }
    }
}

async fn adopt(rpc: &RpcHandle<SimProviders>, mut adopter: RequestStream<Adopter>) {
    while let Some(IncomingRequest { request, reply }) = adopter.recv().await {
        let Adopt {
            configuration,
            members,
            ids,
        } = request;
        let mut report = Adopted::default();
        for (member, id) in members.iter().zip(ids) {
            assert_always!(
                member.configuration == configuration,
                "a forwarded member names its recruited configuration"
            );
            let Some(role) = &member.role else {
                report.failures.push(format!("{id}:missing"));
                continue;
            };
            let probe = Probe {
                id,
                participant: member.participant.clone(),
                boot: member.boot,
                configuration: member.configuration,
            };
            // The interface arrived as data; binding it here is the third
            // participant's own, explicit choice.
            let outcome = match RoleClient::bind(role, rpc) {
                Ok(client) => {
                    client
                        .status()
                        .try_get_reply_within(&probe, MEMBER_TIMEOUT)
                        .await
                }
                Err(error) => Err(error),
            };
            match outcome {
                Ok(answer) => report.answers.push(answer),
                Err(error) => report.failures.push(format!(
                    "{id}:{:?}/{:?}",
                    error.reason(),
                    error.execution()
                )),
            }
        }
        let _ = reply.send(&report);
    }
}
