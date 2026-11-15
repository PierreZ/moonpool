//! Deterministic runner for the generic network provider contract.

use crate::contract_network::network_contract;
use crate::fixtures::SimFixtures;
use moonpool_sim::{SimProviders, SimWorld};
use std::{future::Future, net::IpAddr};

const SEED: u64 = 20_260_916;

/// Drive a provider future and simulation events together without a Tokio runtime.
fn drive<F: Future>(sim: &mut SimWorld, future: F) -> F::Output {
    sim.block_on(SEED, future)
}

#[test]
fn sim_network_contract() {
    let mut sim = SimWorld::new_with_seed(SEED);
    let ip: IpAddr = "10.0.1.1".parse().expect("valid simulation IP");
    let providers = SimProviders::new(sim.downgrade(), ip);
    drive(&mut sim, network_contract(&providers, &SimFixtures::new()));
}
