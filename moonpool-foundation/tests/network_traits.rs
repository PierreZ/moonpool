use moonpool_foundation::{NetworkProvider, TcpListenerTrait, TokioNetworkProvider};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

// Generic echo client that works with any NetworkProvider
async fn echo_client<P>(provider: P, server_addr: &str, message: &str) -> std::io::Result<String>
where
    P: NetworkProvider,
{
    let mut stream = provider.connect(server_addr).await?;

    // Send message
    stream.write_all(message.as_bytes()).await?;

    // Read echo response
    let mut buf = [0; 1024];
    let n = stream.read(&mut buf).await?;

    Ok(String::from_utf8_lossy(&buf[..n]).to_string())
}

#[test]
fn test_tokio_echo_server() {
    // Use local runtime for tests with async traits without Send - following Pierre's article
    let local_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build_local(Default::default())
        .expect("Failed to build local runtime");

    local_runtime.block_on(async move {
        let provider = TokioNetworkProvider::new();

        // Bind to any available port
        let listener = provider.bind("127.0.0.1:0").await.unwrap();
        let server_addr = listener.local_addr().unwrap();

        // Use spawn_local with local runtime
        let server_task = tokio::task::spawn_local(async move {
            let (mut stream, _peer_addr) = listener.accept().await.unwrap();

            let mut buf = [0; 1024];
            let n = stream.read(&mut buf).await.unwrap();
            stream.write_all(&buf[..n]).await.unwrap();
        });

        // Give server time to start accepting
        tokio::time::sleep(tokio::time::Duration::from_millis(1)).await;

        // Connect client and test echo
        let message = "Hello, echo server!";
        let response = echo_client(provider, &server_addr, message).await.unwrap();

        assert_eq!(response, message);

        // Wait for server to complete
        server_task.await.unwrap();
    });
}

#[test]
fn test_provider_default() {
    let local_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build_local(Default::default())
        .expect("Failed to build local runtime");

    local_runtime.block_on(async move {
        // Test that TokioNetworkProvider implements Default
        let provider = TokioNetworkProvider;

        // This should work without issues
        let listener = provider.bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        println!("Bound to {}", addr);
    });
}

#[test]
fn test_provider_clone() {
    let local_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build_local(Default::default())
        .expect("Failed to build local runtime");

    local_runtime.block_on(async move {
        // Test that TokioNetworkProvider can be cloned
        let provider = TokioNetworkProvider::new();
        let cloned_provider = provider.clone();

        // Both should work independently
        let listener1 = provider.bind("127.0.0.1:0").await.unwrap();
        let listener2 = cloned_provider.bind("127.0.0.1:0").await.unwrap();

        let addr1 = listener1.local_addr().unwrap();
        let addr2 = listener2.local_addr().unwrap();

        println!("Provider 1 bound to {}", addr1);
        println!("Provider 2 bound to {}", addr2);

        // Addresses should be different since they're on different ports
        assert_ne!(addr1, addr2);
    });
}
