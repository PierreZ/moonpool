//! Simple test to verify TimeProvider imports work correctly.

#[test]
fn test_time_provider_imports() {
    // This should compile if imports work
    let sim = moonpool_simulation::SimWorld::new();
    let _network = sim.network_provider();
    let _time = sim.time_provider();

    let _tokio_time = moonpool_simulation::TokioTimeProvider::new();
    let _tokio_net = moonpool_simulation::TokioNetworkProvider::new();
}
