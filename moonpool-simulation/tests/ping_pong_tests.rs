use moonpool_simulation::{
    NetworkConfiguration, NetworkProvider, SimWorld, TcpListenerTrait, TokioNetworkProvider,
};
use std::io;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[test]
fn test_simulation_ping_pong() {
    let local_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build_local(Default::default())
        .expect("Failed to build local runtime");

    local_runtime.block_on(async move {
        let config = NetworkConfiguration::fast_local();
        let mut sim = SimWorld::new_with_network_config(config);
        let provider = sim.network_provider();

        // Simulation-specific ping-pong that processes events
        async fn sim_ping_pong_session(
            provider: impl NetworkProvider + Clone,
            sim: &mut SimWorld,
            rounds: usize,
        ) -> io::Result<Vec<String>> {
            let listener = provider.bind("ping-pong-server").await?;
            let mut client = provider.connect("ping-pong-server").await?;

            // Accept server connection
            let (mut server, _peer_addr) = listener.accept().await?;

            let mut responses = Vec::new();

            for i in 0..rounds {
                let ping_msg = format!("PING-{}", i);

                // Client sends ping
                client.write_all(ping_msg.as_bytes()).await?;

                // Process events to deliver data
                sim.run_until_empty();

                // Server receives ping
                let mut buf = [0; 1024];
                let n = server.read(&mut buf).await?;
                let received_ping = String::from_utf8_lossy(&buf[..n]);

                // Verify ping message
                assert_eq!(received_ping, ping_msg);

                // Server responds with pong
                let pong_msg = format!("PONG-{}", i);
                server.write_all(pong_msg.as_bytes()).await?;

                // Process events to deliver response
                sim.run_until_empty();

                // Client receives pong
                let n = client.read(&mut buf).await?;
                let response = String::from_utf8_lossy(&buf[..n]).to_string();
                responses.push(response);
            }

            Ok(responses)
        }

        let responses = sim_ping_pong_session(provider, &mut sim, 5).await.unwrap();

        // Verify responses
        let expected: Vec<String> = (0..5).map(|i| format!("PONG-{}", i)).collect();
        assert_eq!(responses, expected);

        // Verify simulation time advanced
        assert!(sim.current_time() > std::time::Duration::ZERO);

        println!("Simulation ping-pong completed in {:?}", sim.current_time());
        println!("Responses: {:?}", responses);
    });
}

#[test]
fn test_tokio_ping_pong() {
    let local_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build_local(Default::default())
        .expect("Failed to build local runtime");

    local_runtime.block_on(async move {
        let provider = TokioNetworkProvider::new();

        // Use a real TCP server and client approach for Tokio
        let server_addr = "127.0.0.1:0"; // Let OS choose port
        let listener = provider.bind(server_addr).await.unwrap();
        let actual_addr = listener.local_addr().unwrap();

        // Spawn server task
        let server_task = tokio::task::spawn_local(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut server_responses = Vec::new();

            for i in 0..5 {
                let expected_ping = format!("PING-{}", i);

                // Receive ping
                let mut buf = [0; 1024];
                let n = stream.read(&mut buf).await.unwrap();
                let received = String::from_utf8_lossy(&buf[..n]);
                assert_eq!(received, expected_ping);

                // Send pong
                let pong = format!("PONG-{}", i);
                stream.write_all(pong.as_bytes()).await.unwrap();
                server_responses.push(pong);
            }
            server_responses
        });

        // Give server time to start listening
        tokio::task::yield_now().await;

        // Client connects and does ping-pong
        let mut client = provider.connect(&actual_addr).await.unwrap();
        let mut responses = Vec::new();

        for i in 0..5 {
            let ping = format!("PING-{}", i);

            // Send ping
            client.write_all(ping.as_bytes()).await.unwrap();

            // Receive pong
            let mut buf = [0; 1024];
            let n = client.read(&mut buf).await.unwrap();
            let response = String::from_utf8_lossy(&buf[..n]).to_string();
            responses.push(response);
        }

        let _server_responses = server_task.await.unwrap();

        // Verify responses
        let expected: Vec<String> = (0..5).map(|i| format!("PONG-{}", i)).collect();
        assert_eq!(responses, expected);

        println!("Tokio ping-pong completed successfully");
        println!("Responses: {:?}", responses);
    });
}

#[test]
fn test_provider_equivalence() {
    let local_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build_local(Default::default())
        .expect("Failed to build local runtime");

    local_runtime.block_on(async move {
        // Test simulation provider with event processing
        let config = NetworkConfiguration::fast_local();
        let mut sim = SimWorld::new_with_network_config(config);
        let provider = sim.network_provider();
        let sim_responses = sim_ping_pong_session(provider, &mut sim, 3).await.unwrap();

        // Test Tokio provider with separate tasks
        let tokio_provider = TokioNetworkProvider::new();
        let tokio_responses = tokio_ping_pong_session(tokio_provider, 3).await.unwrap();

        // Both providers should produce identical results
        assert_eq!(sim_responses, tokio_responses);

        println!("Provider equivalence verified!");
        println!("Simulation responses: {:?}", sim_responses);
        println!("Tokio responses: {:?}", tokio_responses);

        // Helper function for simulation ping-pong
        async fn sim_ping_pong_session(
            provider: impl NetworkProvider + Clone,
            sim: &mut SimWorld,
            rounds: usize,
        ) -> io::Result<Vec<String>> {
            let listener = provider.bind("ping-pong-server").await?;
            let mut client = provider.connect("ping-pong-server").await?;
            let (mut server, _) = listener.accept().await?;

            let mut responses = Vec::new();
            for i in 0..rounds {
                let ping = format!("PING-{}", i);
                client.write_all(ping.as_bytes()).await?;
                sim.run_until_empty();

                let mut buf = [0; 1024];
                let n = server.read(&mut buf).await?;
                assert_eq!(String::from_utf8_lossy(&buf[..n]), ping);

                let pong = format!("PONG-{}", i);
                server.write_all(pong.as_bytes()).await?;
                sim.run_until_empty();

                let n = client.read(&mut buf).await?;
                responses.push(String::from_utf8_lossy(&buf[..n]).to_string());
            }
            Ok(responses)
        }

        // Helper function for Tokio ping-pong
        async fn tokio_ping_pong_session(
            provider: TokioNetworkProvider,
            rounds: usize,
        ) -> io::Result<Vec<String>> {
            let listener = provider.bind("127.0.0.1:0").await?;
            let addr = listener.local_addr()?;

            let server_task = tokio::task::spawn_local(async move {
                let (mut stream, _) = listener.accept().await?;
                for i in 0..rounds {
                    let expected = format!("PING-{}", i);
                    let mut buf = [0; 1024];
                    let n = stream.read(&mut buf).await?;
                    assert_eq!(String::from_utf8_lossy(&buf[..n]), expected);

                    let pong = format!("PONG-{}", i);
                    stream.write_all(pong.as_bytes()).await?;
                }
                Ok::<(), io::Error>(())
            });

            tokio::task::yield_now().await;
            let mut client = provider.connect(&addr).await?;
            let mut responses = Vec::new();

            for i in 0..rounds {
                let ping = format!("PING-{}", i);
                client.write_all(ping.as_bytes()).await?;

                let mut buf = [0; 1024];
                let n = client.read(&mut buf).await?;
                responses.push(String::from_utf8_lossy(&buf[..n]).to_string());
            }

            server_task.await??;
            Ok(responses)
        }
    });
}

#[test]
fn test_extended_ping_pong_session() {
    let local_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build_local(Default::default())
        .expect("Failed to build local runtime");

    local_runtime.block_on(async move {
        // Test longer session with configurable latency
        let config = NetworkConfiguration::wan_simulation(); // Higher latency
        let mut sim = SimWorld::new_with_network_config(config);
        let provider = sim.network_provider();

        let rounds = 10;

        // Use simulation-specific ping-pong approach
        let listener = provider.bind("extended-ping-pong-server").await.unwrap();
        let mut client = provider.connect("extended-ping-pong-server").await.unwrap();
        let (mut server, _) = listener.accept().await.unwrap();

        let mut responses = Vec::new();
        for i in 0..rounds {
            let ping = format!("PING-{}", i);
            client.write_all(ping.as_bytes()).await.unwrap();
            sim.run_until_empty();

            let mut buf = [0; 1024];
            let n = server.read(&mut buf).await.unwrap();
            assert_eq!(String::from_utf8_lossy(&buf[..n]), ping);

            let pong = format!("PONG-{}", i);
            server.write_all(pong.as_bytes()).await.unwrap();
            sim.run_until_empty();

            let n = client.read(&mut buf).await.unwrap();
            responses.push(String::from_utf8_lossy(&buf[..n]).to_string());
        }

        // Verify all responses received correctly
        assert_eq!(responses.len(), rounds);
        for (i, response) in responses.iter().enumerate() {
            assert_eq!(response, &format!("PONG-{}", i));
        }

        // With WAN simulation config, should take significant time
        assert!(sim.current_time().as_millis() > 50);

        println!(
            "Extended session ({} rounds) completed in {:?}",
            rounds,
            sim.current_time()
        );
    });
}

#[test]
fn test_large_message_ping_pong() {
    let local_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build_local(Default::default())
        .expect("Failed to build local runtime");

    local_runtime.block_on(async move {
        let config = NetworkConfiguration::fast_local();
        let mut sim = SimWorld::new_with_network_config(config);
        let provider = sim.network_provider();

        // Simulation-specific large message test
        let listener = provider.bind("large-message-server").await.unwrap();
        let mut client = provider.connect("large-message-server").await.unwrap();
        let (mut server, _) = listener.accept().await.unwrap();

        // Send a large message (1KB)
        let large_data: Vec<u8> = (0..1024).map(|i| (i % 256) as u8).collect();
        client.write_all(&large_data).await.unwrap();
        sim.run_until_empty();

        // Server receives in chunks (testing partial reads)
        let mut received = Vec::new();
        let mut buf = [0; 256]; // Smaller buffer to test partial reads

        while received.len() < 1024 {
            let n = server.read(&mut buf).await.unwrap();
            if n == 0 {
                break;
            }
            received.extend_from_slice(&buf[..n]);
        }

        // Verify received data matches sent data
        assert_eq!(received, large_data);

        // Server echoes back
        server.write_all(&received).await.unwrap();
        sim.run_until_empty();

        // Client receives echo in chunks
        let mut echo_received = Vec::new();
        while echo_received.len() < 1024 {
            let n = client.read(&mut buf).await.unwrap();
            if n == 0 {
                break;
            }
            echo_received.extend_from_slice(&buf[..n]);
        }

        let integrity_ok = large_data == echo_received;
        sim.run_until_empty();

        assert!(integrity_ok, "Large message data integrity failed");

        println!("Large message ping-pong integrity test passed");
        println!("Simulation time: {:?}", sim.current_time());
    });
}

#[test]
fn test_deterministic_ping_pong() {
    let local_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build_local(Default::default())
        .expect("Failed to build local runtime");

    local_runtime.block_on(async move {
        let mut execution_times = Vec::new();
        let mut all_responses = Vec::new();

        // Run simulation multiple times
        for _run in 0..3 {
            let config = NetworkConfiguration::wan_simulation();
            let mut sim = SimWorld::new_with_network_config(config);
            let provider = sim.network_provider();

            // Use simulation-specific ping-pong approach
            let listener = provider
                .bind("deterministic-ping-pong-server")
                .await
                .unwrap();
            let mut client = provider
                .connect("deterministic-ping-pong-server")
                .await
                .unwrap();
            let (mut server, _) = listener.accept().await.unwrap();

            let mut responses = Vec::new();
            for i in 0..3 {
                let ping = format!("PING-{}", i);
                client.write_all(ping.as_bytes()).await.unwrap();
                sim.run_until_empty();

                let mut buf = [0; 1024];
                let n = server.read(&mut buf).await.unwrap();
                assert_eq!(String::from_utf8_lossy(&buf[..n]), ping);

                let pong = format!("PONG-{}", i);
                server.write_all(pong.as_bytes()).await.unwrap();
                sim.run_until_empty();

                let n = client.read(&mut buf).await.unwrap();
                responses.push(String::from_utf8_lossy(&buf[..n]).to_string());
            }

            execution_times.push(sim.current_time());
            all_responses.push(responses);
        }

        // All runs should be identical
        let first_time = execution_times[0];
        let first_responses = &all_responses[0];

        for (i, (&time, responses)) in execution_times.iter().zip(all_responses.iter()).enumerate()
        {
            assert_eq!(time, first_time, "Run {} had different execution time", i);
            assert_eq!(
                responses, first_responses,
                "Run {} had different responses",
                i
            );
        }

        println!(
            "Deterministic ping-pong verified across {} runs",
            execution_times.len()
        );
        println!("Execution time: {:?}", first_time);
    });
}
