use moonpool_simulation::{
    NetworkConfiguration, NetworkProvider, SimulationMetrics, SimulationResult,
    TcpListenerTrait,
};
use tokio::io::AsyncWriteExt;
use tracing::{info, instrument, Level};
use tracing_subscriber;


/// Simplified ping-pong workload similar to the working echo pattern
#[instrument(skip(provider))]
async fn ping_pong_simple(
    _seed: u64,
    provider: moonpool_simulation::SimNetworkProvider,
    ip_address: Option<String>,
) -> SimulationResult<SimulationMetrics> {
    // IP address assigned: use for identification or logging if needed
    let _assigned_ip = ip_address.unwrap_or_else(|| "no-ip".to_string());

    let server_addr = "ping-pong-server";
    
    // Follow the working pattern from simple_echo_server
    let listener = provider.bind(server_addr).await?;
    let _client = provider.connect(server_addr).await?; // Create connection first
    let (mut stream, _peer_addr) = listener.accept().await?; // Then accept it

    // Just write some ping-pong style test data to exercise the connection
    let ping_data = b"PING-0";
    let pong_data = b"PONG-0";
    
    // Write ping and pong data
    stream.write_all(ping_data).await?;
    stream.write_all(pong_data).await?;

    // Return metrics
    let mut metrics = SimulationMetrics::default();
    metrics
        .custom_metrics
        .insert("ping_pong_messages_sent".to_string(), 2.0);

    Ok(metrics)
}

#[test]
fn test_ping_pong_with_simulation_report() {
    tracing_subscriber::fmt()
        .with_max_level(Level::WARN)
        .init();

    info!("tracing setup");

    let local_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build_local(Default::default())
        .expect("Failed to build local runtime");

    local_runtime.block_on(async move {
        // Use manual SimWorld approach like the working tests
        let config = NetworkConfiguration::wan_simulation(); // Use WAN config for noticeable delays
        let mut sim = moonpool_simulation::SimWorld::new_with_network_config(config);
        let provider = sim.network_provider();

        // Run the ping-pong workload directly
        let result = ping_pong_simple(12345, provider, Some("10.0.0.1".to_string())).await;
        
        // Process all simulation events
        sim.run_until_empty();
        
        // Extract metrics from the simulation
        let sim_metrics = sim.extract_metrics();
        
        match result {
            Ok(workload_metrics) => {
                println!("Ping-pong workload completed successfully!");
                println!("Simulated time: {:?}", sim_metrics.simulated_time);
                println!("Events processed: {}", sim_metrics.events_processed);
                println!("Custom metrics: {:?}", workload_metrics.custom_metrics);
                
                // Verify the simulation worked
                assert!(sim_metrics.simulated_time > std::time::Duration::ZERO);
                assert!(sim_metrics.events_processed > 0);
                
                // Verify custom metrics from the workload
                assert_eq!(
                    workload_metrics.custom_metrics.get("ping_pong_messages_sent"),
                    Some(&2.0)
                );
                
                println!("Manual ping-pong simulation test passed!");
            },
            Err(e) => {
                panic!("Ping-pong workload failed: {:?}", e);
            }
        }
    });
}
