//! The local (non-`Send`) provider traits.
//!
//! These checks pin the three promises of the local traits: every `Send`
//! backend implements its local trait through the blanket impl, a `!Send`
//! type can implement each local trait, and code written once against a
//! local trait still yields a `Send` future when the backend is `Send`.

use std::cell::{Cell, RefCell};
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll};
use std::time::Duration;

use futures::io::{AsyncRead, AsyncSeek, AsyncWrite};
use moonpool_core::{
    Detach, IoConstraints, JoinError, LocalNetworkProvider, LocalProviders, LocalStorageFile,
    LocalStorageProvider, LocalTaskProvider, LocalTcpListener, LocalTimeProvider, OpenOptions,
    TimeError,
};

// ---------------------------------------------------------------------------
// Generic code written once against the local traits.
// ---------------------------------------------------------------------------

async fn write_then_read<S: LocalStorageProvider>(storage: &S, path: &str) -> io::Result<Vec<u8>> {
    let file = storage
        .open(path, OpenOptions::create_write().read(true))
        .await?;
    file.write_at(0, b"local").await?;
    file.sync_data().await?;
    let mut buf = vec![0; 5];
    let read = file.read_at(0, &mut buf).await?;
    buf.truncate(read);
    Ok(buf)
}

async fn echo_once<N: LocalNetworkProvider>(network: &N) -> io::Result<String> {
    let listener = network.bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let (connected, accepted) = futures::join!(network.connect(&address), listener.accept());
    connected?;
    let (_stream, peer) = accepted?;
    Ok(peer)
}

async fn bounded<T: LocalTimeProvider>(time: &T) -> Result<u32, TimeError> {
    time.sleep(Duration::from_millis(1)).await?;
    time.timeout(Duration::from_secs(5), async { 7 }).await
}

async fn bundle_round_trip<P: LocalProviders>(providers: &P, path: &str) -> io::Result<Vec<u8>> {
    let _now = providers.time().now();
    write_then_read(providers.storage(), path).await
}

// ---------------------------------------------------------------------------
// Tokio backends implement the local traits through the blanket impls, and
// the generic futures stay `Send`.
// ---------------------------------------------------------------------------

#[cfg(feature = "tokio-providers")]
mod tokio_backends {
    use super::*;
    use moonpool_core::{
        TokioNetworkProvider, TokioProviders, TokioStorageFile, TokioStorageProvider,
        TokioTcpListener, TokioTimeProvider,
    };

    fn assert_send<T: Send>(_: &T) {}
    fn implements_local_storage<S: LocalStorageProvider>() {}
    fn implements_local_file<F: LocalStorageFile>() {}
    fn implements_local_network<N: LocalNetworkProvider>() {}
    fn implements_local_listener<L: LocalTcpListener>() {}
    fn implements_local_time<T: LocalTimeProvider>() {}
    fn implements_local_bundle<P: LocalProviders>() {}

    #[test]
    fn tokio_backends_implement_the_local_traits() {
        implements_local_storage::<TokioStorageProvider>();
        implements_local_file::<TokioStorageFile>();
        implements_local_network::<TokioNetworkProvider>();
        implements_local_listener::<TokioTcpListener>();
        implements_local_time::<TokioTimeProvider>();
        implements_local_bundle::<TokioProviders>();
    }

    /// Compile-time check: generic local code instantiated with a `Send`
    /// backend is a `Send` future, so it can still go to `tokio::spawn`.
    #[test]
    fn generic_local_code_on_send_backends_is_send() {
        let storage = TokioStorageProvider::new();
        let network = TokioNetworkProvider::new();
        let time = TokioTimeProvider::new();
        let providers = TokioProviders::new();
        assert_send(&write_then_read(&storage, "unused"));
        assert_send(&echo_once(&network));
        assert_send(&bounded(&time));
        assert_send(&bundle_round_trip(&providers, "unused"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn generic_local_code_runs_on_tokio_spawn() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir
            .path()
            .join("local.bin")
            .to_str()
            .expect("utf-8 path")
            .to_owned();
        let read = tokio::spawn(async move {
            let providers = TokioProviders::new();
            bundle_round_trip(&providers, &path).await
        })
        .await
        .expect("task completed")
        .expect("round trip");
        assert_eq!(read, b"local");

        let peer = tokio::spawn(async { echo_once(&TokioNetworkProvider::new()).await })
            .await
            .expect("task completed")
            .expect("echo");
        assert!(peer.starts_with("127.0.0.1:"));

        let timed = tokio::spawn(async { bounded(&TokioTimeProvider::new()).await })
            .await
            .expect("task completed");
        assert_eq!(timed, Ok(7));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn local_timeout_accepts_a_non_send_future() {
        let time = TokioTimeProvider::new();
        let shared = Rc::new(Cell::new(1));
        let inner = Rc::clone(&shared);
        let result = LocalTimeProvider::timeout(&time, Duration::from_secs(5), async move {
            tokio::task::yield_now().await;
            inner.set(inner.get() + 1);
            inner.get()
        })
        .await;
        assert_eq!(result, Ok(2));

        let elapsed = LocalTimeProvider::timeout(
            &time,
            Duration::from_millis(1),
            futures::future::pending::<()>(),
        )
        .await;
        assert_eq!(elapsed, Err(TimeError::Elapsed));
        assert_eq!(shared.get(), 2);
    }
}

// ---------------------------------------------------------------------------
// A `!Send` backend can implement every local trait.
// ---------------------------------------------------------------------------

/// A file that lives in an `Rc<RefCell<..>>`, like a thread-per-core
/// runtime's file holding an `Rc` to its descriptor.
#[derive(Default)]
struct RcFile {
    bytes: Rc<RefCell<Vec<u8>>>,
}

impl AsyncRead for RcFile {
    fn poll_read(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        _buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        Poll::Ready(Ok(0))
    }
}

impl AsyncWrite for RcFile {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

impl AsyncSeek for RcFile {
    fn poll_seek(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        _pos: io::SeekFrom,
    ) -> Poll<io::Result<u64>> {
        Poll::Ready(Ok(0))
    }
}

/// Holds an `Rc` across its await point, so its futures are `!Send`.
async fn rc_yield<T>(rc: Rc<T>) -> Rc<T> {
    futures::future::ready(()).await;
    rc
}

impl LocalStorageFile for RcFile {
    async fn sync_all(&self) -> io::Result<()> {
        rc_yield(Rc::clone(&self.bytes)).await;
        Ok(())
    }

    async fn sync_data(&self) -> io::Result<()> {
        rc_yield(Rc::clone(&self.bytes)).await;
        Ok(())
    }

    fn constraints(&self) -> IoConstraints {
        IoConstraints::NONE
    }

    fn is_direct_io(&self) -> bool {
        false
    }

    async fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        let bytes = rc_yield(Rc::clone(&self.bytes)).await;
        let bytes = bytes.borrow();
        let start = usize::try_from(offset)
            .unwrap_or(usize::MAX)
            .min(bytes.len());
        let n = buf.len().min(bytes.len() - start);
        buf[..n].copy_from_slice(&bytes[start..start + n]);
        Ok(n)
    }

    async fn write_at(&self, offset: u64, buf: &[u8]) -> io::Result<usize> {
        let bytes = rc_yield(Rc::clone(&self.bytes)).await;
        let mut bytes = bytes.borrow_mut();
        let start = usize::try_from(offset).map_err(|_| io::ErrorKind::InvalidInput)?;
        if bytes.len() < start + buf.len() {
            bytes.resize(start + buf.len(), 0);
        }
        bytes[start..start + buf.len()].copy_from_slice(buf);
        Ok(buf.len())
    }

    async fn size(&self) -> io::Result<u64> {
        Ok(self.bytes.borrow().len() as u64)
    }

    async fn set_len(&self, size: u64) -> io::Result<()> {
        let size = usize::try_from(size).map_err(|_| io::ErrorKind::InvalidInput)?;
        self.bytes.borrow_mut().resize(size, 0);
        Ok(())
    }
}

/// A one-file storage provider that is `!Send`.
#[derive(Clone, Default)]
struct RcStorage {
    file: Rc<RefCell<Vec<u8>>>,
}

impl LocalStorageProvider for RcStorage {
    type File = RcFile;

    async fn open(&self, _path: &str, _options: OpenOptions) -> io::Result<RcFile> {
        Ok(RcFile {
            bytes: rc_yield(Rc::clone(&self.file)).await,
        })
    }

    async fn exists(&self, _path: &str) -> io::Result<bool> {
        Ok(true)
    }

    async fn delete(&self, _path: &str) -> io::Result<()> {
        Ok(())
    }

    async fn rename(&self, _from: &str, _to: &str) -> io::Result<()> {
        Ok(())
    }

    async fn create_dir_all(&self, _path: &str) -> io::Result<()> {
        Ok(())
    }

    async fn list_dir(&self, _path: &str) -> io::Result<Vec<String>> {
        Ok(Vec::new())
    }

    async fn sync_dir(&self, _path: &str) -> io::Result<()> {
        Ok(())
    }
}

/// A stream that never transfers anything. The `Rc` makes it `!Send`.
struct RcStream {
    _not_send: Rc<()>,
}

impl AsyncRead for RcStream {
    fn poll_read(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        _buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        Poll::Ready(Ok(0))
    }
}

impl AsyncWrite for RcStream {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

struct RcListener(Rc<String>);

impl LocalTcpListener for RcListener {
    type TcpStream = RcStream;

    async fn accept(&self) -> io::Result<(RcStream, String)> {
        let address = rc_yield(Rc::clone(&self.0)).await;
        Ok((
            RcStream {
                _not_send: Rc::new(()),
            },
            address.to_string(),
        ))
    }

    fn local_addr(&self) -> io::Result<String> {
        Ok(self.0.to_string())
    }
}

#[derive(Clone, Default)]
struct RcNetwork(Rc<()>);

impl LocalNetworkProvider for RcNetwork {
    type TcpStream = RcStream;
    type TcpListener = RcListener;

    async fn bind(&self, addr: &str) -> io::Result<RcListener> {
        rc_yield(Rc::clone(&self.0)).await;
        Ok(RcListener(Rc::new(addr.replace(":0", ":1"))))
    }

    async fn connect(&self, _addr: &str) -> io::Result<RcStream> {
        Ok(RcStream {
            _not_send: rc_yield(Rc::clone(&self.0)).await,
        })
    }
}

/// A clock that only moves when it sleeps.
#[derive(Clone, Default)]
struct RcTime(Rc<Cell<Duration>>);

impl LocalTimeProvider for RcTime {
    async fn sleep(&self, duration: Duration) -> Result<(), TimeError> {
        let clock = rc_yield(Rc::clone(&self.0)).await;
        clock.set(clock.get() + duration);
        Ok(())
    }

    fn now(&self) -> Duration {
        self.0.get()
    }

    async fn timeout<F, T>(&self, _duration: Duration, future: F) -> Result<T, TimeError>
    where
        F: Future<Output = T>,
    {
        Ok(future.await)
    }
}

/// A join handle that is `!Send`: it holds the task and runs it when awaited.
struct RcJoinHandle(Pin<Box<dyn Future<Output = ()>>>);

impl Future for RcJoinHandle {
    type Output = Result<(), JoinError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.0.as_mut().poll(cx).map(Ok)
    }
}

impl Detach for RcJoinHandle {
    fn detach(self) {}
}

/// Hands each task to its handle, on the calling thread.
#[derive(Clone, Default)]
struct RcTasks;

impl LocalTaskProvider for RcTasks {
    type JoinHandle = RcJoinHandle;

    fn spawn_local_task<F>(&self, _name: &str, future: F) -> RcJoinHandle
    where
        F: Future<Output = ()> + 'static,
    {
        RcJoinHandle(Box::pin(future))
    }

    async fn yield_now(&self) {}
}

#[derive(Clone)]
struct RcRandom;

impl moonpool_core::RandomProvider for RcRandom {
    fn random<T>(&self) -> T
    where
        rand::distr::StandardUniform: rand::distr::Distribution<T>,
    {
        unreachable!("not drawn in this test")
    }

    fn random_range<T>(&self, range: std::ops::Range<T>) -> T
    where
        T: rand::distr::uniform::SampleUniform + PartialOrd,
    {
        range.start
    }
}

#[derive(Clone)]
struct RcSystem;

impl moonpool_core::SystemProvider for RcSystem {
    fn sample(&self, _data_dir: &str) -> io::Result<moonpool_core::SystemSample> {
        Err(io::ErrorKind::Unsupported.into())
    }
}

/// A `!Send` bundle, implemented by hand as a thread-per-core backend would.
#[derive(Clone, Default)]
struct RcProviders {
    network: RcNetwork,
    time: RcTime,
    storage: RcStorage,
}

impl LocalProviders for RcProviders {
    type Network = RcNetwork;
    type Time = RcTime;
    type Random = RcRandom;
    type Storage = RcStorage;
    type System = RcSystem;

    fn network(&self) -> &RcNetwork {
        &self.network
    }

    fn time(&self) -> &RcTime {
        &self.time
    }

    fn random(&self) -> &RcRandom {
        &RcRandom
    }

    fn storage(&self) -> &RcStorage {
        &self.storage
    }

    fn system(&self) -> &RcSystem {
        &RcSystem
    }
}

#[test]
fn a_non_send_backend_implements_every_local_trait() {
    futures::executor::block_on(async {
        let providers = RcProviders::default();
        assert_eq!(
            bundle_round_trip(&providers, "rc")
                .await
                .expect("round trip"),
            b"local"
        );

        let file = providers
            .storage()
            .open("rc", OpenOptions::read_write())
            .await
            .expect("open");
        assert_eq!(
            file.write_at_durable(5, b"!").await.expect("durable write"),
            1
        );
        assert_eq!(file.size().await.expect("size"), 6);

        assert_eq!(
            echo_once(providers.network()).await.expect("echo"),
            "127.0.0.1:1"
        );

        assert_eq!(bounded(providers.time()).await, Ok(7));
        assert_eq!(providers.time().timer(), Duration::from_millis(1));

        let tasks = RcTasks;
        let ran = Rc::new(Cell::new(false));
        let inner = Rc::clone(&ran);
        tasks
            .spawn_local_task("rc", async move { inner.set(true) })
            .await
            .expect("task completed");
        assert!(ran.get());
    });
}
