//! # moonpool-rpc-derive
//!
//! The optional `#[service]` attribute of `moonpool-rpc`, re-exported as
//! `moonpool_rpc::service` behind its `derive` feature. Do not depend on
//! this crate directly.
//!
//! From one trait it generates a typed interface built **only** on the
//! manual API — the attribute owns no registry, protocol, delivery or
//! retry semantics:
//!
//! ```text
//! #[moonpool_rpc::service(id = 0x6b76_0001, version = 1)]
//! pub trait Kv {
//!     #[method(id = 1, schema = 1)]
//!     async fn get(&self, request: GetRequest) -> GetReply;
//!     #[method(id = 2, schema = 1)]
//!     async fn put(&self, request: PutRequest) -> PutReply;
//! }
//! ```
//!
//! expands to, beside the rewritten handler trait `Kv` (each method
//! returning a `Send` future):
//!
//! | Item | What it is |
//! |---|---|
//! | `KvInterface` | the [`RpcInterface`] marker, with the explicit id and version |
//! | `KvGet`, `KvPut` | one [`RpcMethod`] + `InterfaceMethod<KvInterface>` marker per method, with the explicit method id and schema |
//! | `KvRef` | `InterfaceRef<KvInterface>`: the serialisable reference |
//! | `KvClient<P>` | a bound reference: `kv.get()` is a `ServiceClient` with every delivery mode |
//! | `KvRequest` | one variant per method, holding the `IncomingRequest` (and so its `ReplyHandle`) |
//! | `KvServer` | the registered group and its request streams: `next()` pulls the next request of any method, `dispatch` runs a handler and replies, `serve` loops |
//!
//! Ids are required and explicit, never derived from names or order; a
//! duplicate method id is a compile error. Nothing is hidden: a server
//! that needs another reply policy (never reply, reply later, look at the
//! peer) matches on `KvRequest` and uses the `ReplyHandle` itself.
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
