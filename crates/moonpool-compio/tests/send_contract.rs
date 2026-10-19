//! The thread-affinity contract that lets compio's `!Send` futures satisfy
//! moonpool's `Send` bounds.

use std::future::Future;
use std::task::{Context, Poll};
use std::time::Duration;

use moonpool_compio::{
    CompioNetworkProvider, CompioProviders, CompioResolver, CompioStorageProvider,
    CompioTaskProvider, CompioTimeProvider,
};
use moonpool_core::{
    NetworkProvider, OpenOptions, Resolver, StorageFile, StorageProvider, TaskProvider,
    TcpListenerTrait, TimeProvider,
};

fn assert_send<T: Send>(_: &T) {}
fn assert_send_sync<T: Send + Sync>(_: &T) {}

/// Compile-time only: every provider future, the stream and the handles
/// name `Send`. This proves the trait bounds are met, through the wrapper's
/// `unsafe impl Send`; it says nothing about what happens when one actually
/// crosses a thread. The runtime tests below check that.
#[compio::test]
async fn every_provider_future_and_handle_is_send() {
    let time = CompioTimeProvider::new();
    let sleep = time.sleep(Duration::from_millis(1));
    assert_send(&sleep);
    let timeout = time.timeout(Duration::from_millis(1), async {});
    assert_send(&timeout);
    let tasks = CompioTaskProvider::new();
    assert_send(&tasks.yield_now());
    let handle = tasks.spawn_task("send", async {});
    assert_send_sync(&handle);
    handle.await.expect("joined");

    let network = CompioNetworkProvider::new();
    let bind = network.bind("127.0.0.1:0");
    assert_send(&bind);
    let listener = bind.await.expect("bind");
    assert_send_sync(&listener);
    let address = listener.local_addr().expect("address");
    let accept = listener.accept();
    assert_send(&accept);
    let connect = network.connect(&address);
    assert_send(&connect);
    let (client, accepted) = futures::join!(connect, accept);
    let client = client.expect("connect");
    let (server, _) = accepted.expect("accept");
    assert_send(&client);
    assert_send(&server);

    let dir = tempfile::TempDir::new_in(env!("CARGO_TARGET_TMPDIR")).expect("temp dir");
    let path = dir.path().join("send");
    let path = path.to_str().expect("UTF-8");
    let storage = CompioStorageProvider::new();
    let open = storage.open(path, OpenOptions::create_write().read(true));
    assert_send(&open);
    let file = open.await.expect("open");
    assert_send_sync(&file);
    assert_send(&file.write_at(0, b"x"));
    assert_send(&file.sync_data());
    assert_send(&storage.list_dir(dir.path().to_str().expect("UTF-8")));
    assert_send(&CompioResolver::new().resolve("127.0.0.1:1"));

    assert_send_sync(&CompioProviders::new());
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(ToString::to_string)
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_default()
}

/// The contract itself: a provider future created on the runtime thread,
/// moved to another thread and polled there, panics instead of touching
/// compio's thread-local driver. The panic happens before the inner future
/// ever ran, so no compio state exists yet, and the wrapper leaks the
/// inner value rather than dropping it on the wrong thread.
#[test]
fn polling_a_provider_future_on_another_thread_panics() {
    let runtime = compio::runtime::Runtime::new().expect("runtime");
    // The future borrows its provider; leak one so the future can move.
    let time: &'static CompioTimeProvider = Box::leak(Box::new(CompioTimeProvider::new()));
    let sleep = runtime.enter(|| time.sleep(Duration::from_millis(1)));
    let outcome = std::thread::spawn(move || {
        let mut sleep = Box::pin(sleep);
        let waker = futures::task::noop_waker();
        let polled = sleep.as_mut().poll(&mut Context::from_waker(&waker));
        matches!(polled, Poll::Ready(_))
    })
    .join();
    let payload = outcome.expect_err("polling off the runtime thread must panic");
    assert!(
        panic_message(payload.as_ref()).contains("thread different"),
        "unexpected panic: {:?}",
        panic_message(payload.as_ref())
    );
}

/// Dropping one on another thread panics too (without dropping the inner
/// future there).
#[test]
fn dropping_a_provider_future_on_another_thread_panics() {
    let runtime = compio::runtime::Runtime::new().expect("runtime");
    let time: &'static CompioTimeProvider = Box::leak(Box::new(CompioTimeProvider::new()));
    let sleep = runtime.enter(|| time.sleep(Duration::from_millis(1)));
    let outcome = std::thread::spawn(move || drop(sleep)).join();
    let payload = outcome.expect_err("dropping off the runtime thread must panic");
    assert!(panic_message(payload.as_ref()).contains("thread different"));
}

/// Which compio driver these tests run on (`--nocapture` shows it).
#[compio::test]
async fn reports_the_driver() {
    let driver = compio::runtime::Runtime::with_current(compio::runtime::Runtime::driver_type);
    eprintln!("compio driver: {driver:?}");
}

/// The handle types at rest (the listener, an idle storage file, a join
/// handle) are `Send + Sync` for real, not through the wrapper: compio's
/// `sync` feature makes them so. Dropping them on another thread works.
#[test]
fn idle_handles_drop_on_another_thread() {
    let runtime = compio::runtime::Runtime::new().expect("runtime");
    let dir = tempfile::TempDir::new_in(env!("CARGO_TARGET_TMPDIR")).expect("temp dir");
    let path = dir.path().join("idle");
    let path = path.to_str().expect("UTF-8").to_owned();
    let (listener, file, handle) = runtime.block_on(async {
        let listener = CompioNetworkProvider::new()
            .bind("127.0.0.1:0")
            .await
            .expect("bind");
        let file = CompioStorageProvider::new()
            .open(&path, OpenOptions::create_write().read(true))
            .await
            .expect("open");
        let tasks = CompioTaskProvider::new();
        let handle = tasks.spawn_task("idle", async {});
        tasks.yield_now().await;
        (listener, file, handle)
    });
    let outcome = std::thread::spawn(move || drop((listener, file, handle))).join();
    assert!(
        outcome.is_ok(),
        "an idle handle panicked when dropped off-thread"
    );
}
