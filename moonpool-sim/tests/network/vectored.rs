use futures::io::{AsyncRead, AsyncWriteExt};
use moonpool_sim::{NetworkConfiguration, NetworkProvider, SimWorld, TcpListenerTrait};
use std::io::IoSlice;
use std::pin::Pin;
use std::task::{Context, Poll};

fn try_read(server: &mut (impl AsyncRead + Unpin), buf: &mut [u8]) -> Option<usize> {
    let waker = futures::task::noop_waker();
    let mut cx = Context::from_waker(&waker);
    match Pin::new(server).poll_read(&mut cx, buf) {
        Poll::Ready(Ok(n)) if n > 0 => Some(n),
        _ => None,
    }
}

#[test]
fn vectored_write_preserves_slice_boundaries_as_delivery_events() {
    let local_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .expect("Failed to build local runtime");

    local_runtime.block_on(async move {
        let mut sim = SimWorld::new_with_network_config(NetworkConfiguration::fast_local());
        let provider = sim.network_provider();
        let addr = "vectored-write-test";

        let listener = provider.bind(addr).await.expect("bind should succeed");
        let mut client = provider
            .connect(addr)
            .await
            .expect("connect should succeed");
        let (mut server, _peer_addr) = listener.accept().await.expect("accept should succeed");

        assert!(client.is_write_vectored());

        let seg0 = b"AAAA";
        let seg1 = b"BBBBBB";
        let seg2 = b"CC";
        let bufs = [IoSlice::new(seg0), IoSlice::new(seg1), IoSlice::new(seg2)];
        let total = seg0.len() + seg1.len() + seg2.len();

        let accepted = client
            .write_vectored(&bufs)
            .await
            .expect("vectored write should succeed");
        assert_eq!(accepted, total);

        let mut chunks: Vec<Vec<u8>> = Vec::new();
        let mut buf = vec![0; 4096];
        while sim.pending_event_count() > 0 {
            sim.step();
            if let Some(n) = try_read(&mut server, &mut buf) {
                chunks.push(buf[..n].to_vec());
            }
        }

        assert_eq!(chunks, vec![seg0.to_vec(), seg1.to_vec(), seg2.to_vec()]);
    });
}
