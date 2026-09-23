//! The Tokio network provider disables Nagle's algorithm on both ends of
//! every connection, as `FoundationDB`'s `Net2` does (`FLOW_TCP_NODELAY`).

#![cfg(feature = "tokio-net")]

use moonpool_core::{NetworkProvider, TcpListenerTrait, TokioNetworkProvider};

#[tokio::test]
async fn connected_and_accepted_streams_disable_nagle() {
    let provider = TokioNetworkProvider::new();
    let listener = provider.bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("address");
    let (connected, accepted) = tokio::join!(provider.connect(&address), listener.accept());
    let connected = connected.expect("connect");
    let (accepted, _) = accepted.expect("accept");
    assert!(connected.get_ref().nodelay().expect("read option"));
    assert!(accepted.get_ref().nodelay().expect("read option"));
}
