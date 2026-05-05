//! Shared test scaffolding for the rpc submodule tests.
//!
//! Provides a minimal `Providers` implementation backed by stubbed network
//! traits, plus helpers for registering server queues and dispatching canned
//! replies. The `#[cfg(test)] mod test_support;` declaration in `mod.rs`
//! gates compilation, so this file never ends up in the public crate.

#![allow(dead_code)] // each consumer uses a different subset of helpers

use std::net::{IpAddr, Ipv4Addr};
use std::rc::Rc;

use serde::{Deserialize, Serialize};

use crate::rpc::failure_monitor::FailureStatus;
use crate::rpc::net_notified_queue::NetNotifiedQueue;
use crate::rpc::request_stream::RequestEnvelope;
use crate::rpc::{NetTransport, ReplyError, ServiceEndpoint};
use crate::{
    Endpoint, JsonCodec, NetworkAddress, NetworkProvider, Providers, TokioRandomProvider,
    TokioStorageProvider, TokioTaskProvider, TokioTimeProvider, UID,
};

// ---- Stubbed network provider ----

#[derive(Clone)]
pub struct MockNetworkProvider;

pub struct DummyStream;

impl tokio::io::AsyncRead for DummyStream {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        _buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Err(std::io::Error::other("dummy")))
    }
}

impl tokio::io::AsyncWrite for DummyStream {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        _buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::task::Poll::Ready(Err(std::io::Error::other("dummy")))
    }
    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Err(std::io::Error::other("dummy")))
    }
    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Err(std::io::Error::other("dummy")))
    }
}

impl std::marker::Unpin for DummyStream {}

pub struct DummyListener;

#[async_trait::async_trait(?Send)]
impl crate::TcpListenerTrait for DummyListener {
    type TcpStream = DummyStream;
    async fn accept(&self) -> std::io::Result<(Self::TcpStream, String)> {
        Err(std::io::Error::other("dummy"))
    }
    fn local_addr(&self) -> std::io::Result<String> {
        Err(std::io::Error::other("dummy"))
    }
}

#[async_trait::async_trait(?Send)]
impl NetworkProvider for MockNetworkProvider {
    type TcpStream = DummyStream;
    type TcpListener = DummyListener;
    async fn bind(&self, _addr: &str) -> std::io::Result<Self::TcpListener> {
        Err(std::io::Error::other("mock"))
    }
    async fn connect(&self, _addr: &str) -> std::io::Result<Self::TcpStream> {
        Err(std::io::Error::other("mock"))
    }
}

#[derive(Clone)]
pub struct MockProviders {
    pub network: MockNetworkProvider,
    pub time: TokioTimeProvider,
    pub task: TokioTaskProvider,
    pub random: TokioRandomProvider,
    pub storage: TokioStorageProvider,
}

impl MockProviders {
    pub fn new() -> Self {
        Self {
            network: MockNetworkProvider,
            time: TokioTimeProvider::new(),
            task: TokioTaskProvider,
            random: TokioRandomProvider::new(),
            storage: TokioStorageProvider::new(),
        }
    }
}

impl Default for MockProviders {
    fn default() -> Self {
        Self::new()
    }
}

impl Providers for MockProviders {
    type Network = MockNetworkProvider;
    type Time = TokioTimeProvider;
    type Task = TokioTaskProvider;
    type Random = TokioRandomProvider;
    type Storage = TokioStorageProvider;
    fn network(&self) -> &Self::Network {
        &self.network
    }
    fn time(&self) -> &Self::Time {
        &self.time
    }
    fn task(&self) -> &Self::Task {
        &self.task
    }
    fn random(&self) -> &Self::Random {
        &self.random
    }
    fn storage(&self) -> &Self::Storage {
        &self.storage
    }
}

// ---- Echo request type used by fan-out / load-balance tests ----

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Echo(pub u32);

pub fn make_transport() -> Rc<NetTransport<MockProviders>> {
    let local = NetworkAddress::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), 4500);
    crate::NetTransportBuilder::new(MockProviders::new())
        .local_address(local)
        .build()
        .expect("build transport")
}

/// Server-side queue type used by the shared test helpers.
pub type TestQueue = Rc<NetNotifiedQueue<RequestEnvelope<Echo>, JsonCodec>>;

/// Return type for [`register_servers`]: a pair of parallel vectors holding
/// each server's queue and its matching client-side endpoint.
pub type ServerSetup = (
    Vec<TestQueue>,
    Vec<ServiceEndpoint<Echo, Echo, JsonCodec, MockProviders>>,
);

/// Register `tokens.len()` server queues on `transport` and return the
/// queues alongside their typed `ServiceEndpoint`s.
///
/// All endpoints share the same network address (`10.0.0.1:4500`) but
/// distinct UIDs, which is enough for the dispatch tests since the queues
/// are looked up by token.
pub fn register_servers(
    transport: &Rc<NetTransport<MockProviders>>,
    tokens: &[u64],
) -> ServerSetup {
    let mut queues = Vec::new();
    let mut endpoints = Vec::new();
    for &token in tokens {
        let addr = NetworkAddress::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), 4500);
        let ep = Endpoint::new(addr, UID::new(token, 1));
        let q: Rc<NetNotifiedQueue<RequestEnvelope<Echo>, JsonCodec>> =
            Rc::new(NetNotifiedQueue::new(ep.clone(), JsonCodec));
        transport.register(UID::new(token, 1), q.clone());
        queues.push(q);
        endpoints.push(ServiceEndpoint::new(ep, JsonCodec, Rc::clone(transport)));
    }
    transport
        .failure_monitor()
        .set_status("10.0.0.1:4500", FailureStatus::Available);
    (queues, endpoints)
}

/// Dispatch a canned reply to the client that originated `envelope`.
pub fn dispatch_reply(
    transport: &NetTransport<MockProviders>,
    envelope: &RequestEnvelope<Echo>,
    result: Result<Echo, ReplyError>,
) {
    let payload = serde_json::to_vec(&result).expect("serialize reply");
    transport
        .dispatch(&envelope.reply_to.token, &payload)
        .expect("dispatch reply");
}
