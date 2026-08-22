//! Bundle contract: all five slots resolve from one `Providers` and compose.

use crate::fixtures::Fixtures;
use futures::io::{AsyncReadExt, AsyncWriteExt};
use moonpool_core::{
    NetworkProvider, OpenOptions, Providers, RandomProvider, StorageFile, StorageProvider,
    TaskProvider, TcpListenerTrait, TimeProvider,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// A small cross-slot flow proving the bundle is internally consistent and that
/// every accessor returns a usable provider from a single `&P`.
pub(crate) async fn bundle_contract<P: Providers, F: Fixtures>(p: &P, fixtures: &F) {
    // The bundle is cloneable (Providers: Clone).
    let _clone = p.clone();

    // time + random slots.
    p.time()
        .sleep(Duration::from_millis(1))
        .await
        .expect("time slot");
    let _value: u64 = p.random().random();

    // storage slot: write something durably.
    let path = fixtures.path("bundle.txt");
    let mut file = p
        .storage()
        .open(&path, OpenOptions::create_write())
        .await
        .expect("storage slot");
    file.write_all(b"bundle").await.expect("write");
    file.sync_all().await.expect("sync");
    drop(file);

    // network slot: an echo exchange from the same bundle.
    let net = p.network();
    let listener = net.bind(&fixtures.bind_addr()).await.expect("bind");
    let local = listener.local_addr().expect("local_addr");
    let mut client = net
        .connect(&fixtures.connect_addr(&local))
        .await
        .expect("connect");
    let (mut server, _peer) = listener.accept().await.expect("accept");
    client.write_all(b"hi").await.expect("client write");
    let mut buf = [0u8; 2];
    server.read_exact(&mut buf).await.expect("server read");
    assert_eq!(&buf, b"hi", "bundle network echo");

    // task slot: spawn and join.
    let ran = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&ran);
    p.task()
        .spawn_task("bundle-task", async move {
            flag.store(true, Ordering::SeqCst);
        })
        .await
        .expect("task slot join");
    assert!(ran.load(Ordering::SeqCst), "bundle task effect visible");
}
