//! Deterministic runner for the generic network provider contract.

use crate::contract_network::network_contract;
use crate::fixtures::SimFixtures;
use moonpool_sim::{SimProviders, SimWorld, executor::Executor};
use std::{future::Future, net::IpAddr, task::Poll};

const SEED: u64 = 20_260_916;

/// Drive a provider future and simulation events together without a Tokio runtime.
fn drive<F: Future>(sim: &mut SimWorld, future: F) -> F::Output {
    let mut executor = Executor::new(SEED);
    executor.block_on(async {
        futures::pin_mut!(future);
        futures::future::poll_fn(|cx| match future.as_mut().poll(cx) {
            Poll::Ready(output) => Poll::Ready(output),
            Poll::Pending if sim.has_pending_events() => {
                sim.step();
                cx.waker().wake_by_ref();
                Poll::Pending
            }
            Poll::Pending => {
                panic!("network conformance contract stalled without simulation events")
            }
        })
        .await
    })
}

#[test]
fn sim_network_contract() {
    let mut sim = SimWorld::new_with_seed(SEED);
    let ip: IpAddr = "10.0.1.1".parse().expect("valid simulation IP");
    let providers = SimProviders::new(sim.downgrade(), ip);
    drive(&mut sim, network_contract(&providers, &SimFixtures::new()));
}
