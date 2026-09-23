# moonpool-rpc-derive

The optional `#[moonpool_rpc::service]` attribute: from one annotated trait
it generates a typed `moonpool-rpc` interface — the interface and method
markers with their explicit ids, a request enum and dispatcher built on the
pull-style `RequestStream` / `ReplyHandle` primitive, and a typed client of
an `InterfaceRef`.

Enable it through `moonpool-rpc`'s `derive` feature; do not depend on this
crate directly. Everything it generates can be written by hand with the
manual API, and both are tested for the same behavior.
