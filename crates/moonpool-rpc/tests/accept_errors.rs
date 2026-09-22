//! Listener accept errors: transient ones are retried with backoff, a fatal
//! one ends `RpcDriver::run` with that error.
//!
//! Scripted over the real Tokio providers: the listener fails the scripted
//! number of `accept` calls before accepting for real.

#![cfg(feature = "prost")]

use std::collections::VecDeque;
use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use moonpool_core::{
    NetworkProvider, Providers, TcpListenerTrait, TokioNetworkProvider, TokioRandomProvider,
    TokioStorageProvider, TokioTaskProvider, TokioTcpListener, TokioTimeProvider,
};
use moonpool_rpc::{
    AccessClass, IncomingRequest, MethodId, RpcConfig, RpcDriver, RpcMethod, SchemaVersion,
};

#[derive(Clone, PartialEq, prost::Message)]
struct Ping {
    #[prost(uint64, tag = "1")]
    id: u64,
}

struct Echo;
impl RpcMethod for Echo {
    type Request = Ping;
    type Reply = Ping;
    const METHOD: MethodId = MethodId::new(1);
    const SCHEMA: SchemaVersion = SchemaVersion::new(1);
    const NAME: &'static str = "echo";
}

type Script = Arc<Mutex<VecDeque<io::ErrorKind>>>;

#[derive(Clone)]
struct ScriptedNetwork {
    inner: TokioNetworkProvider,
    script: Script,
}

struct ScriptedListener {
    inner: TokioTcpListener,
    script: Script,
}

impl NetworkProvider for ScriptedNetwork {
    type TcpStream = <TokioNetworkProvider as NetworkProvider>::TcpStream;
    type TcpListener = ScriptedListener;

    async fn bind(&self, addr: &str) -> io::Result<ScriptedListener> {
        Ok(ScriptedListener {
            inner: self.inner.bind(addr).await?,
            script: Arc::clone(&self.script),
        })
    }

    async fn connect(&self, addr: &str) -> io::Result<Self::TcpStream> {
        self.inner.connect(addr).await
    }
}

impl TcpListenerTrait for ScriptedListener {
    type TcpStream = <TokioNetworkProvider as NetworkProvider>::TcpStream;

    async fn accept(&self) -> io::Result<(Self::TcpStream, String)> {
        let scripted = self
            .script
            .lock()
            .expect("Mutex poisoned: prior task panicked")
            .pop_front();
        match scripted {
            Some(kind) => Err(io::Error::new(kind, "scripted accept failure")),
            None => self.inner.accept().await,
        }
    }

    fn local_addr(&self) -> io::Result<String> {
        self.inner.local_addr()
    }
}

#[derive(Clone)]
struct ScriptedProviders {
    network: ScriptedNetwork,
    time: TokioTimeProvider,
    task: TokioTaskProvider,
    random: TokioRandomProvider,
    storage: TokioStorageProvider,
}

impl Providers for ScriptedProviders {
    type Network = ScriptedNetwork;
    type Time = TokioTimeProvider;
    type Task = TokioTaskProvider;
    type Random = TokioRandomProvider;
    type Storage = TokioStorageProvider;

    fn network(&self) -> &ScriptedNetwork {
        &self.network
    }
    fn time(&self) -> &TokioTimeProvider {
        &self.time
    }
    fn task(&self) -> &TokioTaskProvider {
        &self.task
    }
    fn random(&self) -> &TokioRandomProvider {
        &self.random
    }
    fn storage(&self) -> &TokioStorageProvider {
        &self.storage
    }
}

fn providers(script: &[io::ErrorKind]) -> ScriptedProviders {
    ScriptedProviders {
        network: ScriptedNetwork {
            inner: TokioNetworkProvider::new(),
            script: Arc::new(Mutex::new(script.iter().copied().collect())),
        },
        time: TokioTimeProvider::new(),
        task: TokioTaskProvider,
        random: TokioRandomProvider::new(),
        storage: TokioStorageProvider::new(),
    }
}

#[tokio::test(flavor = "current_thread")]
async fn transient_accept_errors_are_retried_and_the_listener_keeps_serving() {
    let transient = [
        io::ErrorKind::ConnectionAborted,
        io::ErrorKind::ConnectionReset,
        io::ErrorKind::OutOfMemory,
        io::ErrorKind::Interrupted,
    ];
    let (driver, server) =
        RpcDriver::listen(providers(&transient), "127.0.0.1:0", RpcConfig::default())
            .await
            .expect("bind");
    let driver = tokio::spawn(driver.run());
    let (service, mut stream) = server
        .register::<Echo>(AccessClass::Public)
        .expect("register");
    let handler = tokio::spawn(async move {
        while let Some(IncomingRequest { request, reply }) = stream.recv().await {
            let _ = reply.send(&request);
        }
    });
    let (client_driver, client) =
        RpcDriver::client_only(providers(&[]), RpcConfig::default()).expect("valid config");
    let client_driver = tokio::spawn(client_driver.run());

    let reply = tokio::time::timeout(
        Duration::from_secs(10),
        service.bind(&client).try_get_reply(&Ping { id: 7 }),
    )
    .await
    .expect("served after the transient errors")
    .expect("reply");
    assert_eq!(reply.id, 7);
    assert_eq!(server.stats().expect("running").accept_errors, 4);
    assert!(
        !driver.is_finished(),
        "transient errors never end the runtime"
    );

    handler.abort();
    driver.abort();
    client_driver.abort();
}

#[tokio::test(flavor = "current_thread")]
async fn a_fatal_accept_error_ends_run_with_that_error() {
    let script = [
        io::ErrorKind::ConnectionAborted,
        io::ErrorKind::InvalidInput,
    ];
    let (driver, server) =
        RpcDriver::listen(providers(&script), "127.0.0.1:0", RpcConfig::default())
            .await
            .expect("bind");
    let probe = server.probe().expect("running");
    let error = tokio::time::timeout(Duration::from_secs(10), driver.run())
        .await
        .expect("run ends");
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    assert!(probe.is_released(), "the runtime is gone once run returns");
    assert!(!moonpool_rpc::is_transient_accept_error(&error));
}

#[test]
fn configurations_are_validated_before_anything_runs() {
    for max_frame_bytes in [
        0,
        moonpool_rpc::MIN_FRAME_BYTES - 1,
        moonpool_rpc::MAX_FRAME_BYTES + 1,
    ] {
        let config = RpcConfig {
            max_frame_bytes,
            ..RpcConfig::default()
        };
        assert!(config.validate().is_err(), "{max_frame_bytes}");
        let error = RpcDriver::client_only(providers(&[]), config).expect_err("refused");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }
    let zero_queue = RpcConfig {
        endpoint_queue_capacity: 0,
        ..RpcConfig::default()
    };
    assert!(zero_queue.validate().is_err());
    assert!(RpcConfig::default().validate().is_ok());
}
