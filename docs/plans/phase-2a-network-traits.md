# Phase 2a: Network Traits + Tokio Implementation

## Overview

Phase 2a focuses on establishing the trait-based network abstraction and proving it works with real Tokio networking. The goal is to validate the trait design before adding simulation complexity.

## Target Use Case: Echo Server

```rust
// This exact code must work with TokioNetworkProvider
async fn echo_server<P>(provider: P, addr: &str) -> io::Result<()>
where 
    P: NetworkProvider
{
    let listener = provider.bind(addr).await?;
    println!("Echo server listening on {}", listener.local_addr()?);
    
    let (mut stream, peer_addr) = listener.accept().await?;
    println!("Connection from {}", peer_addr);
    
    let mut buf = [0; 1024];
    let n = stream.read(&mut buf).await?;
    println!("Received: {:?}", &buf[..n]);
    
    stream.write_all(&buf[..n]).await?;
    println!("Echoed back {} bytes", n);
    
    Ok(())
}

// Production usage
let provider = TokioNetworkProvider::new();
echo_server(provider, "127.0.0.1:8080").await?;
```

## Core Design: NetworkProvider Traits

### Simple NetworkProvider Traits

**File: `moonpool-foundation/src/network/traits.rs`**

```rust
use async_trait::async_trait;
use std::io;
use tokio::io::{AsyncRead, AsyncWrite};

/// Provider trait for creating network connections and listeners
/// Single-core design - no Send bounds needed
#[async_trait(?Send)]
pub trait NetworkProvider {
    type TcpStream: AsyncRead + AsyncWrite + Unpin + 'static;
    type TcpListener: TcpListenerTrait<TcpStream = Self::TcpStream> + 'static;
    
    /// Create a TCP listener bound to the given address
    async fn bind(&self, addr: &str) -> io::Result<Self::TcpListener>;
    
    /// Connect to a remote address  
    async fn connect(&self, addr: &str) -> io::Result<Self::TcpStream>;
}

/// Trait for TCP listeners that can accept connections
#[async_trait(?Send)]
pub trait TcpListenerTrait {
    type TcpStream: AsyncRead + AsyncWrite + Unpin + 'static;
    
    /// Accept a single incoming connection
    async fn accept(&self) -> io::Result<(Self::TcpStream, String)>;
    
    /// Get the local address this listener is bound to
    fn local_addr(&self) -> io::Result<String>;
}
```

## Implementation Plan

### 1. Network Module Organization

**File: `moonpool-foundation/src/network/mod.rs`**

```rust
//! Network simulation and abstraction layer.
//!
//! This module provides trait-based networking that allows seamless swapping
//! between real Tokio networking and simulated networking for testing.

/// Core networking traits and abstractions
pub mod traits;

/// Real networking implementation using Tokio
pub mod tokio;

// Re-export main traits
pub use traits::{NetworkProvider, TcpListenerTrait};

// Re-export Tokio implementation
pub use tokio::TokioNetworkProvider;
```

### 2. Tokio Implementation

**File: `moonpool-foundation/src/network/tokio/mod.rs`**

```rust
//! Real Tokio networking implementation.

mod provider;

pub use provider::{TokioNetworkProvider, TokioTcpListener};
```

**File: `moonpool-foundation/src/network/tokio/provider.rs`**

```rust
use crate::network::traits::{NetworkProvider, TcpListenerTrait};
use async_trait::async_trait;
use std::io;

/// Real Tokio networking implementation
#[derive(Debug, Clone)]
pub struct TokioNetworkProvider;

impl TokioNetworkProvider {
    /// Create a new Tokio network provider
    pub fn new() -> Self {
        Self
    }
}

impl Default for TokioNetworkProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait(?Send)]
impl NetworkProvider for TokioNetworkProvider {
    type TcpStream = tokio::net::TcpStream;
    type TcpListener = TokioTcpListener;
    
    async fn bind(&self, addr: &str) -> io::Result<Self::TcpListener> {
        let listener = tokio::net::TcpListener::bind(addr).await?;
        Ok(TokioTcpListener { inner: listener })
    }
    
    async fn connect(&self, addr: &str) -> io::Result<Self::TcpStream> {
        tokio::net::TcpStream::connect(addr).await
    }
}

/// Wrapper for Tokio TcpListener to implement our trait
#[derive(Debug)]
pub struct TokioTcpListener {
    inner: tokio::net::TcpListener,
}

#[async_trait(?Send)]
impl TcpListenerTrait for TokioTcpListener {
    type TcpStream = tokio::net::TcpStream;
    
    async fn accept(&self) -> io::Result<(Self::TcpStream, String)> {
        let (stream, addr) = self.inner.accept().await?;
        Ok((stream, addr.to_string()))
    }
    
    fn local_addr(&self) -> io::Result<String> {
        Ok(self.inner.local_addr()?.to_string())
    }
}
```

### 3. Library Integration

**File: `moonpool-foundation/src/lib.rs`** (update to add network module)

```rust
#![deny(missing_docs)]
#![deny(clippy::unwrap_used)]

/// Error types and utilities for simulation operations.
pub mod error;
/// Event scheduling and processing for the simulation engine.
pub mod events;
/// Core simulation world and coordination logic.
pub mod sim;
/// Network simulation and abstraction layer.
pub mod network;

// Public API exports
pub use error::{SimulationError, SimulationResult};
pub use events::{Event, EventQueue, ScheduledEvent};
pub use sim::{SimWorld, WeakSimWorld};
// Network exports
pub use network::{NetworkProvider, TcpListenerTrait, TokioNetworkProvider};
```

## File Structure for Phase 2a

```
moonpool-foundation/
├── src/
│   ├── lib.rs              # Add network module export
│   ├── network/
│   │   ├── mod.rs          # Network module organization
│   │   ├── traits.rs       # NetworkProvider and TcpListenerTrait
│   │   └── tokio/
│   │       ├── mod.rs      # Tokio module exports
│   │       └── provider.rs # TokioNetworkProvider + TokioTcpListener
│   ├── error.rs            # Existing Phase 1 files
│   ├── events.rs           
│   └── sim.rs              
├── tests/
│   └── network_traits.rs   # Integration tests
└── Cargo.toml              # Updated dependencies
```

## Dependencies

```toml
[dependencies]
async-trait = "0.1"
tokio = { version = "1.0", features = ["net", "io-util"] }

# Existing Phase 1 dependencies
thiserror = "1.0"
```

## Testing Strategy

### Focus on Trait Validation

**File: `moonpool-foundation/tests/network_traits.rs`**

```rust
use moonpool_simulation::{NetworkProvider, TokioNetworkProvider};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

// Generic echo server that works with any NetworkProvider
async fn echo_server<P>(provider: P, addr: &str) -> std::io::Result<()>
where 
    P: NetworkProvider
{
    let listener = provider.bind(addr).await?;
    println!("Echo server listening on {}", listener.local_addr()?);
    
    let (mut stream, peer_addr) = listener.accept().await?;
    println!("Connection from {}", peer_addr);
    
    let mut buf = [0; 1024];
    let n = stream.read(&mut buf).await?;
    println!("Received: {:?}", std::str::from_utf8(&buf[..n]).unwrap_or("invalid utf8"));
    
    stream.write_all(&buf[..n]).await?;
    println!("Echoed back {} bytes", n);
    
    Ok(())
}

// Generic echo client that works with any NetworkProvider  
async fn echo_client<P>(provider: P, server_addr: &str, message: &str) -> std::io::Result<String>
where
    P: NetworkProvider
{
    let mut stream = provider.connect(server_addr).await?;
    
    // Send message
    stream.write_all(message.as_bytes()).await?;
    
    // Read echo response
    let mut buf = [0; 1024];
    let n = stream.read(&mut buf).await?;
    
    Ok(String::from_utf8_lossy(&buf[..n]).to_string())
}

#[tokio::test]
async fn test_tokio_echo_server() {
    let provider = TokioNetworkProvider::new();
    
    // Start server in background
    let server_provider = provider.clone();
    let server_handle = tokio::spawn(async move {
        echo_server(server_provider, "127.0.0.1:0").await
    });
    
    // Give server time to start
    tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
    
    // Connect client and test echo
    let message = "Hello, echo server!";
    let response = echo_client(provider, "127.0.0.1:8080", message).await.unwrap();
    
    assert_eq!(response, message);
    
    // Clean up server
    server_handle.abort();
}

#[tokio::test]
async fn test_trait_object_usage() {
    // Test that traits work as trait objects (dynamic dispatch)
    let provider: Box<dyn NetworkProvider<TcpStream = tokio::net::TcpStream, TcpListener = _>> = 
        Box::new(TokioNetworkProvider::new());
    
    // This should compile and work
    let listener = provider.bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    println!("Bound to {}", addr);
}
```

## Phase 2a Success Criteria

- ✅ All quality gates pass (fmt, clippy, test)
- ✅ `NetworkProvider` and `TcpListenerTrait` are well-designed and documented
- ✅ `TokioNetworkProvider` works with real TCP connections
- ✅ Echo server code works with the trait abstraction
- ✅ Tests demonstrate trait flexibility and usage patterns
- ✅ Foundation is ready for Phase 2b simulation implementation

## What's NOT in Phase 2a

- Simulation implementation (that's Phase 2b)
- Random delays or network conditions
- Event system integration
- Complex async state management

## What IS in Phase 2a

- ✅ **Trait design**: Clean, simple network provider abstraction
- ✅ **Tokio integration**: Real networking implementation
- ✅ **Documentation**: All public APIs documented
- ✅ **Testing**: Comprehensive test coverage for trait usage
- ✅ **Foundation**: Ready for simulation implementation

Phase 2a proves the trait design is sound and works with real networking before we add simulation complexity in Phase 2b.