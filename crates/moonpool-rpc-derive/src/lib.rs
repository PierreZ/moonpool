//! # moonpool-rpc-derive
//!
//! The optional `#[service]` attribute of `moonpool-rpc`. It is re-exported
//! as `moonpool_rpc::service` behind the `derive` feature of `moonpool-rpc`.
//! Do not depend on this crate directly.
//!
//! From one trait, the attribute generates a typed interface that is built
//! **only** on the manual API of `moonpool-rpc`. The attribute owns no
//! registry, protocol, delivery or retry semantics. Everything it generates
//! can be written by hand, and `moonpool-rpc/tests/derive.rs` tests both
//! forms for the same ids, bytes and behavior.
//!
//! # Quick example
//!
//! ```rust
//! # use moonpool_rpc::__private::Providers;
//! use moonpool_rpc::{AccessClass, RpcError, RpcHandle, RpcInterface, RpcMethod};
//!
//! #[moonpool_rpc::service(id = 0x6b76_0001, version = 1)]
//! pub trait Kv {
//!     /// Read a key.
//!     #[method(id = 1, schema = 1)]
//!     async fn get(&self, key: String) -> String;
//!     /// Record a key; nothing to answer.
//!     #[method(id = 2, schema = 1)]
//!     async fn touch(&self, key: String);
//! }
//!
//! // The handler implements the rewritten trait.
//! struct Store;
//! impl Kv for Store {
//!     async fn get(&self, key: String) -> String {
//!         format!("value of {key}")
//!     }
//!     async fn touch(&self, _key: String) {}
//! }
//!
//! /// Register the group and serve it until every request stream ends.
//! async fn serve<P: Providers>(rpc: &RpcHandle<P>) -> Result<(), RpcError> {
//!     let server = KvServer::register(rpc, AccessClass::Public)?;
//!     // Hand this reference (protobuf bytes) to the callers.
//!     let _published: KvRef = server.interface_ref();
//!     server.serve(&Store).await;
//!     Ok(())
//! }
//!
//! /// Call one method; every delivery mode of `ServiceClient` is there.
//! async fn call<P: Providers>(rpc: &RpcHandle<P>, kv: &KvRef) -> Result<String, RpcError> {
//!     KvClient::bind(kv, rpc)?.get().try_get_reply(&"a".to_string()).await
//! }
//!
//! assert_eq!(KvInterface::INTERFACE.get(), 0x6b76_0001);
//! assert_eq!(KvTouch::METHOD.get(), 2);
//! ```
//!
//! # Generated items
//!
//! For a trait `Kv`, the attribute keeps the handler trait `Kv` (each
//! method rewritten to return a `Send` future) and adds:
//!
//! | Item | What it is |
//! |---|---|
//! | `KvInterface` | The [`RpcInterface`] marker, with the explicit id and version |
//! | `KvGet`, `KvTouch` | One [`RpcMethod`] and `InterfaceMethod<KvInterface>` marker per method, with the explicit method id and schema |
//! | `KvRef` | `InterfaceRef<KvInterface>`: the serialisable reference |
//! | `KvClient<P>` | A bound reference: `KvClient::bind(&reference, &rpc)`, then `kv.get()` is a `ServiceClient` with every delivery mode |
//! | `KvRequest` | One variant per method, holding the `IncomingRequest` (and so its `ReplyHandle`) |
//! | `KvServer` | The registered group and its request streams: `register`, `interface_ref`, `group`, `next` (the next request of any method), `dispatch` (run a handler and reply), `serve` (loop) |
//!
//! # Rules
//!
//! - **Explicit ids.** The attribute takes `id` (the `u32` interface id)
//!   and `version` (the `u16` interface version). Each method takes
//!   `#[method(id = <u32>, schema = <u16>)]`. Ids never come from names or
//!   order.
//! - **No duplicates.** A duplicate method id is a compile error, also
//!   when it hides behind a constant.
//! - **Wire types.** Every method is `async fn name(&self, request: T) ->
//!   R`, where `T` and `R` implement `moonpool_rpc::Wire`. A method with no
//!   return type replies `()`.
//! - **Nothing hidden.** A server that needs another reply policy (never
//!   reply, reply later, look at the peer) matches on `KvRequest` and uses
//!   the `ReplyHandle` itself.
//!
//! # Feature flags
//!
//! None. Enable the `derive` feature of `moonpool-rpc` instead.
//!
//! [`RpcInterface`]: https://docs.rs/moonpool-rpc/latest/moonpool_rpc/trait.RpcInterface.html
//! [`RpcMethod`]: https://docs.rs/moonpool-rpc/latest/moonpool_rpc/trait.RpcMethod.html
#![deny(missing_docs)]
#![deny(clippy::unwrap_used)]

mod expand;

use proc_macro::TokenStream;

/// Generate a typed `moonpool-rpc` interface from a trait.
///
/// Arguments: `id` (the `u32` interface id) and `version` (the `u16`
/// interface version). Every method is `async fn name(&self, request: T)
/// -> R` with `#[method(id = <u32>, schema = <u16>)]`; `T` and `R` must be
/// `moonpool_rpc::Wire`. See the crate docs for what is generated.
#[proc_macro_attribute]
pub fn service(attr: TokenStream, item: TokenStream) -> TokenStream {
    expand::expand(attr.into(), item.into())
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}
