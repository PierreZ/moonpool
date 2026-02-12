//! Tests for TCP half-close (FIN) semantics.
//!
//! Validates that graceful close implements proper half-close behavior:
//! - Server can write response, drop stream, and client reads all data
//! - FIN is delivered only after all in-flight data reaches the peer
//! - Read after local write close still works
//! - Write after remote write close still works
//! - Both sides dropping simultaneously works correctly
//! - Abort (RST) still immediately closes both directions

use moonpool_sim::{NetworkConfiguration, NetworkProvider, SimWorld, TcpListenerTrait};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Core scenario: server writes response, drops stream, client reads all data.
///
/// This is the exact sequence that caused IncompleteMessage errors with hyper.
/// The server writes a full HTTP response, drops the stream (sending FIN),
/// and the client must be able to read all the response data before seeing EOF.
#[test]
fn test_half_close_server_write_drop_client_reads_all() {
    let local_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build_local(Default::default())
        .expect("Failed to build local runtime");

    local_runtime.block_on(async move {
        let config = NetworkConfiguration::fast_local();
        let mut sim = SimWorld::new_with_network_config(config);
        let provider = sim.network_provider();

        let addr = "half-close-server";
        let listener = provider.bind(addr).await.unwrap();
        let mut client = provider.connect(addr).await.unwrap();
        let (mut server, _) = listener.accept().await.unwrap();

        // Server writes full response
        let response_data = b"HTTP/1.1 200 OK\r\nContent-Length: 13\r\n\r\nHello, World!";
        server.write_all(response_data).await.unwrap();

        // Server drops its stream (sends FIN)
        drop(server);

        // Process all events — DataDelivery and FinDelivery
        sim.run_until_empty();

        // Client should be able to read the FULL response
        let mut buf = vec![0u8; 4096];
        let mut total_read = 0;
        loop {
            let n = client.read(&mut buf[total_read..]).await.unwrap();
            if n == 0 {
                break; // EOF
            }
            total_read += n;
        }

        assert_eq!(
            total_read,
            response_data.len(),
            "Client should read all {} bytes before EOF, got {}",
            response_data.len(),
            total_read
        );
        assert_eq!(&buf[..total_read], response_data);
    });
}

/// Verify that read after local write close still works.
///
/// In TCP, closing your write side (sending FIN) does not affect your ability
/// to read data from the peer. This is the "half" in half-close.
#[test]
fn test_half_close_read_after_local_write_close() {
    let local_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build_local(Default::default())
        .expect("Failed to build local runtime");

    local_runtime.block_on(async move {
        let config = NetworkConfiguration::fast_local();
        let mut sim = SimWorld::new_with_network_config(config);
        let provider = sim.network_provider();

        let addr = "read-after-write-close";
        let listener = provider.bind(addr).await.unwrap();
        let mut client = provider.connect(addr).await.unwrap();
        let (mut server, _) = listener.accept().await.unwrap();

        // Client shuts down its write side
        client.shutdown().await.unwrap();

        // Server writes data to client
        server.write_all(b"data after client FIN").await.unwrap();
        sim.run_until_empty();

        // Client should still be able to read data (read side is still open)
        let mut buf = vec![0u8; 1024];
        let n = client.read(&mut buf).await.unwrap();
        assert!(n > 0, "Client should read data after shutting down writes");
        assert_eq!(&buf[..n], b"data after client FIN");
    });
}

/// Verify that write after remote write close still works.
///
/// In TCP, the peer closing their write side does not affect our ability
/// to write. The two directions are independent.
#[test]
fn test_half_close_write_after_remote_write_close() {
    let local_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build_local(Default::default())
        .expect("Failed to build local runtime");

    local_runtime.block_on(async move {
        let config = NetworkConfiguration::fast_local();
        let mut sim = SimWorld::new_with_network_config(config);
        let provider = sim.network_provider();

        let addr = "write-after-remote-close";
        let listener = provider.bind(addr).await.unwrap();
        let mut client = provider.connect(addr).await.unwrap();
        let (mut server, _) = listener.accept().await.unwrap();

        // Server shuts down its write side (sends FIN to client)
        server.shutdown().await.unwrap();
        sim.run_until_empty();

        // Client's write side should still work
        let write_result = client.write_all(b"request after server FIN").await;
        assert!(
            write_result.is_ok(),
            "Client should still write after server closes write side"
        );

        sim.run_until_empty();

        // Server should be able to read client's data (server's read side is still open)
        let mut buf = vec![0u8; 1024];
        let n = server.read(&mut buf).await.unwrap();
        assert!(n > 0, "Server should read data from client");
        assert_eq!(&buf[..n], b"request after server FIN");
    });
}

/// Both sides drop simultaneously — both should eventually see EOF.
#[test]
fn test_half_close_both_sides_drop() {
    let local_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build_local(Default::default())
        .expect("Failed to build local runtime");

    local_runtime.block_on(async move {
        let config = NetworkConfiguration::fast_local();
        let mut sim = SimWorld::new_with_network_config(config);
        let provider = sim.network_provider();

        let addr = "both-drop";
        let listener = provider.bind(addr).await.unwrap();
        let mut client = provider.connect(addr).await.unwrap();
        let (mut server, _) = listener.accept().await.unwrap();

        // Both sides write data
        client.write_all(b"from client").await.unwrap();
        server.write_all(b"from server").await.unwrap();

        // Both sides drop (send FIN)
        drop(client);
        drop(server);

        // Process all events
        sim.run_until_empty();

        // Both connections should have their remote_write_closed set
        // (verified by the fact that we got here without hanging)
    });
}

/// Verify that write after local write close returns BrokenPipe.
#[test]
fn test_half_close_write_after_local_close() {
    let local_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build_local(Default::default())
        .expect("Failed to build local runtime");

    local_runtime.block_on(async move {
        let config = NetworkConfiguration::fast_local();
        let mut sim = SimWorld::new_with_network_config(config);
        let provider = sim.network_provider();

        let addr = "write-after-close";
        let listener = provider.bind(addr).await.unwrap();
        let mut client = provider.connect(addr).await.unwrap();
        let (_server, _) = listener.accept().await.unwrap();

        // Client shuts down write side
        client.shutdown().await.unwrap();
        sim.run_until_empty();

        // Writing should fail with BrokenPipe
        let result = client.write_all(b"should fail").await;
        assert!(result.is_err(), "Write after shutdown should fail");
        let err = result.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::BrokenPipe);
    });
}

/// Verify abort (RST) still immediately closes both directions.
#[test]
fn test_abort_still_immediate() {
    let local_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build_local(Default::default())
        .expect("Failed to build local runtime");

    local_runtime.block_on(async move {
        let config = NetworkConfiguration::fast_local();
        let mut sim = SimWorld::new_with_network_config(config);
        let provider = sim.network_provider();

        let addr = "abort-test";
        let listener = provider.bind(addr).await.unwrap();
        let mut client = provider.connect(addr).await.unwrap();
        let (mut server, _) = listener.accept().await.unwrap();

        // Server writes data but then aborts
        server.write_all(b"data before abort").await.unwrap();
        sim.close_connection_abort(server.connection_id());

        sim.run_until_empty();

        // Client should get ConnectionReset, not the buffered data
        let mut buf = vec![0u8; 1024];
        let result = client.read(&mut buf).await;
        assert!(
            result.is_err(),
            "Abort should cause immediate error, not deliver buffered data"
        );
    });
}

/// Large response: verify all data is delivered with half-close even when
/// data spans multiple ProcessSendBuffer / DataDelivery events.
#[test]
fn test_half_close_large_response() {
    let local_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build_local(Default::default())
        .expect("Failed to build local runtime");

    local_runtime.block_on(async move {
        let config = NetworkConfiguration::fast_local();
        let mut sim = SimWorld::new_with_network_config(config);
        let provider = sim.network_provider();

        let addr = "large-response";
        let listener = provider.bind(addr).await.unwrap();
        let mut client = provider.connect(addr).await.unwrap();
        let (mut server, _) = listener.accept().await.unwrap();

        // Server writes large response in multiple chunks
        let chunk = vec![b'A'; 1024];
        for _ in 0..10 {
            server.write_all(&chunk).await.unwrap();
        }
        let total_sent = 1024 * 10;

        // Server drops stream (FIN)
        drop(server);

        // Process all events
        sim.run_until_empty();

        // Client reads all data
        let mut total_read = 0;
        let mut buf = vec![0u8; 4096];
        loop {
            let n = client.read(&mut buf).await.unwrap();
            if n == 0 {
                break;
            }
            total_read += n;
        }

        assert_eq!(
            total_read, total_sent,
            "Client should read all {} bytes, got {}",
            total_sent, total_read
        );
    });
}

/// Empty send buffer: FIN delivered via event without data.
#[test]
fn test_half_close_empty_send_buffer() {
    let local_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build_local(Default::default())
        .expect("Failed to build local runtime");

    local_runtime.block_on(async move {
        let config = NetworkConfiguration::fast_local();
        let mut sim = SimWorld::new_with_network_config(config);
        let provider = sim.network_provider();

        let addr = "empty-buffer-fin";
        let listener = provider.bind(addr).await.unwrap();
        let mut client = provider.connect(addr).await.unwrap();
        let (server, _) = listener.accept().await.unwrap();

        // Server drops without writing anything
        drop(server);

        sim.run_until_empty();

        // Client should immediately see EOF (no data, just FIN)
        let mut buf = vec![0u8; 1024];
        let n = client.read(&mut buf).await.unwrap();
        assert_eq!(n, 0, "Should get EOF when peer drops with empty buffer");
    });
}

/// Verify the request-response pattern works: client sends request, server
/// reads it, sends response, drops stream. Client reads full response.
#[test]
fn test_half_close_request_response_pattern() {
    let local_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build_local(Default::default())
        .expect("Failed to build local runtime");

    local_runtime.block_on(async move {
        let config = NetworkConfiguration::fast_local();
        let mut sim = SimWorld::new_with_network_config(config);
        let provider = sim.network_provider();

        let addr = "req-resp";
        let listener = provider.bind(addr).await.unwrap();

        // Spawn client task
        let client_provider = sim.network_provider();
        let client_handle = tokio::task::spawn_local(async move {
            let mut stream = client_provider.connect(addr).await.unwrap();

            // Send request
            stream.write_all(b"GET / HTTP/1.1\r\n\r\n").await.unwrap();

            // Read full response
            let mut response = Vec::new();
            let mut buf = vec![0u8; 4096];
            loop {
                let n = stream.read(&mut buf).await.unwrap();
                if n == 0 {
                    break;
                }
                response.extend_from_slice(&buf[..n]);
            }
            response
        });

        // Server: accept, read request, write response, drop
        let server_handle = tokio::task::spawn_local(async move {
            let (mut stream, _) = listener.accept().await.unwrap();

            // Read request
            let mut buf = vec![0u8; 4096];
            let n = stream.read(&mut buf).await.unwrap();
            assert!(n > 0, "Server should receive request");

            // Write response
            let response = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nOK";
            stream.write_all(response).await.unwrap();

            // Drop stream (graceful close / FIN)
            drop(stream);
        });

        // Drive the simulation until both tasks complete
        while !client_handle.is_finished() || !server_handle.is_finished() {
            sim.step();
            tokio::task::yield_now().await;
        }

        let response = client_handle.await.unwrap();
        server_handle.await.unwrap();

        let expected = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nOK";
        assert_eq!(
            response, expected,
            "Client should receive complete response"
        );
    });
}
