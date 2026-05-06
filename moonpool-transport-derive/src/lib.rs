//! Proc-macros for moonpool RPC interfaces.
//!
//! This crate provides the `#[service]` attribute macro for generating
//! RPC server/client boilerplate from a trait definition.
//!
//! # Example
//!
//! ```rust,ignore
//! use moonpool_transport::{service, RpcError};
//!
//! // Dynamic endpoints (tokens allocated at runtime):
//! #[service]
//! trait Calculator {
//!     async fn add(&self, req: AddRequest) -> Result<AddResponse, RpcError>;
//!     async fn sub(&self, req: SubRequest) -> Result<SubResponse, RpcError>;
//! }
//!
//! // Well-known endpoints (opt-in, deterministic addressing):
//! // Use transport.serve_well_known::<Calculator>(token_id, codec)
//! ```
//!
//! This generates:
//! - `CalculatorServer<C, P>` with `RequestStream` fields, `init()`, `init_at()`, and `serve()`
//! - `CalculatorClient<C, P>` with `ServiceEndpoint` fields and `from_base()`
//! - The trait itself with `#[async_trait(?Send)]`

use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::{
    Expr, ExprLit, FnArg, GenericArgument, Ident, ItemTrait, Lit, PathArguments, ReturnType,
    TraitItem, Type, parse_macro_input,
};

/// Attribute macro for defining RPC service interfaces.
///
/// Generates server and client types from a trait definition.
/// All methods must use `&self` receivers.
///
/// # Attributes
///
/// - `#[service]` - Dynamic endpoints (tokens allocated at runtime).
/// - `#[service(id = 0x...)]` - Legacy static endpoints (deterministic tokens).
///
/// # Example
///
/// ```rust,ignore
/// #[service]
/// trait PingPong {
///     async fn ping(&self, req: PingRequest) -> Result<PingResponse, RpcError>;
/// }
/// ```
#[proc_macro_attribute]
pub fn service(attr: TokenStream, item: TokenStream) -> TokenStream {
    let attr = parse_macro_input!(attr as InterfaceAttr);
    let item = parse_macro_input!(item as ItemTrait);

    match service_impl(attr, item) {
        Ok(tokens) => tokens.into(),
        Err(err) => err.to_compile_error().into(),
    }
}

/// Auto-detect mode from method receivers and delegate.
fn service_impl(attr: InterfaceAttr, item: ItemTrait) -> syn::Result<proc_macro2::TokenStream> {
    let mut has_ref = false;
    let mut has_mut_ref = false;

    for trait_item in &item.items {
        if let TraitItem::Fn(method) = trait_item
            && let Some(FnArg::Receiver(recv)) = method.sig.inputs.first()
        {
            if recv.mutability.is_some() {
                has_mut_ref = true;
            } else {
                has_ref = true;
            }
        }
    }

    if has_ref && has_mut_ref {
        return Err(syn::Error::new_spanned(
            &item.ident,
            "all methods must use `&self` receivers",
        ));
    }

    if has_mut_ref {
        return Err(syn::Error::new_spanned(
            &item.ident,
            "`&mut self` methods (virtual actor mode) have been removed. Use `&self` for RPC services.",
        ));
    }

    interface_impl(attr, item)
}

/// Method info extracted from trait methods.
struct MethodInfo {
    index: u32,
    name: Ident,
    req_type: Type,
    resp_type: Type,
}

fn interface_impl(attr: InterfaceAttr, item: ItemTrait) -> syn::Result<proc_macro2::TokenStream> {
    let name = &item.ident;
    let server_name = format_ident!("{}Server", name);
    let client_name = format_ident!("{}Client", name);

    // Parse trait methods
    let mut method_infos: Vec<MethodInfo> = Vec::new();
    for (index, trait_item) in item.items.iter().enumerate() {
        if let TraitItem::Fn(method) = trait_item {
            let method_name = &method.sig.ident;

            // Extract request and response types from method signature
            let (req_type, resp_type) = extract_method_types(&method.sig)?;

            // Method indices start at 1; index 0 is reserved.
            method_infos.push(MethodInfo {
                index: (index + 1) as u32,
                name: method_name.clone(),
                req_type,
                resp_type,
            });
        }
    }

    let method_count = method_infos.len() as u32;

    // Generate server fields — RequestStream<Req, Resp>
    let server_fields = method_infos.iter().map(|m| {
        let name = &m.name;
        let req_type = &m.req_type;
        let resp_type = &m.resp_type;
        quote! { pub #name: moonpool_transport::RequestStream<#req_type, #resp_type> }
    });

    // Generate server init_at — registers at base.adjusted(idx) per method
    let server_init_at_fields: Vec<_> = method_infos
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let name = &m.name;
            let idx = m.index;
            let is_last = i == method_infos.len() - 1;
            if is_last {
                quote! {
                    let #name = moonpool_transport::NetTransport::register_handler(transport, base_token.adjusted(#idx), codec);
                }
            } else {
                quote! {
                    let #name = moonpool_transport::NetTransport::register_handler(transport, base_token.adjusted(#idx), codec.clone());
                }
            }
        })
        .collect();

    let server_field_names: Vec<_> = method_infos.iter().map(|m| &m.name).collect();

    // Generate client fields — typed ServiceEndpoint per method
    let client_fields = method_infos.iter().map(|m| {
        let name = &m.name;
        let req_type = &m.req_type;
        let resp_type = &m.resp_type;
        quote! {
            pub #name: moonpool_transport::ServiceEndpoint<#req_type, #resp_type>
        }
    });

    // Generate client from_base field constructors
    let client_from_base_inits: Vec<_> = method_infos
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let name = &m.name;
            let idx = m.index;
            let is_last = i == method_infos.len() - 1;
            if is_last {
                quote! {
                    #name: moonpool_transport::ServiceEndpoint::new(
                        moonpool_transport::Endpoint::new(
                            address.clone(),
                            base_token.adjusted(#idx),
                        ),
                        codec,
                        std::rc::Rc::clone(&handle),
                    )
                }
            } else {
                quote! {
                    #name: moonpool_transport::ServiceEndpoint::new(
                        moonpool_transport::Endpoint::new(
                            address.clone(),
                            base_token.adjusted(#idx),
                        ),
                        codec.clone(),
                        std::rc::Rc::clone(&handle),
                    )
                }
            }
        })
        .collect();

    let first_field_name = &method_infos[0].name;

    // Generate the trait with async_trait attribute
    let trait_vis = &item.vis;
    let trait_items = &item.items;
    let trait_name_snake = to_snake_case(&name.to_string());

    // Generate serve() method blocks — one close handle + one spawned task per method
    let serve_close_handles: Vec<_> = method_infos
        .iter()
        .map(|m| {
            let method_name = &m.name;
            quote! {
                let queue = self.#method_name.queue();
                close_fns.push(Box::new(move || queue.close()));
            }
        })
        .collect();

    let serve_spawn_tasks: Vec<_> = method_infos
        .iter()
        .map(|m| {
            let method_name = &m.name;
            let task_name = format!("{}_{}", trait_name_snake, m.name);
            quote! {
                {
                    let stream = self.#method_name;
                    let h = handler.clone();
                    providers.task().spawn_task(#task_name, async move {
                        while let Some((req, reply)) = stream.recv().await {
                            match h.#method_name(req).await {
                                Ok(resp) => reply.send(resp),
                                Err(e) => {
                                    tracing::warn!(error = %e, method = #task_name, "handler error");
                                    reply.send_error(moonpool_transport::ReplyError::BrokenPromise);
                                }
                            }
                        }
                    });
                }
            }
        })
        .collect();

    // Generate init() — dynamic allocation when no id, legacy UID::new when id present
    let init_body = if let Some(id) = attr.id {
        quote! {
            Self::init_at(transport, moonpool_transport::UID::new(#id, 0), codec)
        }
    } else {
        quote! {
            let base_token = transport.allocate_interface_token();
            Self::init_at(transport, base_token, codec)
        }
    };

    // Conditionally generate INTERFACE_ID and Client::new for legacy mode
    let legacy_server_consts = attr.id.map(|id| {
        quote! {
            /// Interface identifier (legacy static mode).
            pub const INTERFACE_ID: u64 = #id;
        }
    });

    let legacy_client_items = attr.id.map(|id| {
        let client_new_inits: Vec<_> = method_infos
            .iter()
            .enumerate()
            .map(|(i, m)| {
                let name = &m.name;
                let idx = m.index;
                let is_last = i == method_infos.len() - 1;
                if is_last {
                    quote! {
                        #name: moonpool_transport::ServiceEndpoint::new(
                            moonpool_transport::Endpoint::new(
                                address.clone(),
                                moonpool_transport::UID::new(#id, 0).adjusted(#idx),
                            ),
                            codec,
                            std::rc::Rc::clone(&handle),
                        )
                    }
                } else {
                    quote! {
                        #name: moonpool_transport::ServiceEndpoint::new(
                            moonpool_transport::Endpoint::new(
                                address.clone(),
                                moonpool_transport::UID::new(#id, 0).adjusted(#idx),
                            ),
                            codec.clone(),
                            std::rc::Rc::clone(&handle),
                        )
                    }
                }
            })
            .collect();
        quote! {
            /// Interface identifier (legacy static mode).
            pub const INTERFACE_ID: u64 = #id;

            /// Create a client with legacy static token addressing.
            pub fn new<C: moonpool_transport::MessageCodec + Clone, P: moonpool_transport::Providers>(
                address: moonpool_transport::NetworkAddress,
                codec: C,
                transport: &std::rc::Rc<moonpool_transport::NetTransport<P>>,
            ) -> Self {
                let handle: std::rc::Rc<dyn moonpool_transport::TransportHandle> =
                    transport.clone() as std::rc::Rc<dyn moonpool_transport::TransportHandle>;
                Self {
                    #(#client_new_inits,)*
                }
            }
        }
    });

    let expanded = quote! {
        // Emit the original trait with async_trait(?Send)
        #[async_trait::async_trait(?Send)]
        #trait_vis trait #name {
            #(#trait_items)*
        }

        /// Server-side interface with RequestStreams.
        ///
        /// Generated by `#[service]`.
        pub struct #server_name {
            #(#server_fields,)*
            base_token: moonpool_transport::UID,
        }

        impl #server_name {
            #legacy_server_consts

            /// Number of methods in this interface.
            pub const METHOD_COUNT: u32 = #method_count;

            /// Initialize the server interface, registering all handlers.
            ///
            /// Allocates a dynamic base token (or uses the legacy static ID if present).
            pub fn init<C: moonpool_transport::MessageCodec + Clone, P: moonpool_transport::Providers>(
                transport: &std::rc::Rc<moonpool_transport::NetTransport<P>>,
                codec: C,
            ) -> Self {
                #init_body
            }

            /// Initialize at a specific base token.
            ///
            /// Methods are registered at `base_token.adjusted(1)`, `.adjusted(2)`, etc.
            pub fn init_at<C: moonpool_transport::MessageCodec + Clone, P: moonpool_transport::Providers>(
                transport: &std::rc::Rc<moonpool_transport::NetTransport<P>>,
                base_token: moonpool_transport::UID,
                codec: C,
            ) -> Self {
                #(#server_init_at_fields)*
                Self { #(#server_field_names,)* base_token }
            }

            /// Initialize at a well-known token (deterministic addressing without discovery).
            ///
            /// Methods are registered at `UID::well_known(token_id).adjusted(1)`, etc.
            pub fn well_known<C: moonpool_transport::MessageCodec + Clone, P: moonpool_transport::Providers>(
                transport: &std::rc::Rc<moonpool_transport::NetTransport<P>>,
                token_id: u32,
                codec: C,
            ) -> Self {
                Self::init_at(transport, moonpool_transport::UID::well_known(token_id), codec)
            }

            /// Get the base token for this server instance.
            ///
            /// Use with `Client::from_base()` or serialize for client discovery.
            pub fn base_token(&self) -> moonpool_transport::UID {
                self.base_token
            }

            /// Consume this server and spawn handler tasks for all methods.
            ///
            /// Each method gets its own task that loops on `recv()` and dispatches
            /// to the handler. Returns a [`ServerHandle`](moonpool_transport::ServerHandle)
            /// that stops all tasks when dropped.
            pub fn serve<H, P: moonpool_transport::Providers>(
                self,
                handler: std::rc::Rc<H>,
                providers: &P,
            ) -> moonpool_transport::ServerHandle
            where
                H: #name + 'static,
            {
                use moonpool_transport::TaskProvider as _;
                let mut close_fns: Vec<Box<dyn Fn()>> = Vec::new();
                #(#serve_close_handles)*
                #(#serve_spawn_tasks)*
                moonpool_transport::ServerHandle::new(close_fns)
            }
        }

        /// Client-side interface with typed [`ServiceEndpoint`](moonpool_transport::ServiceEndpoint)
        /// fields.
        ///
        /// Generated by `#[service]`. Each field provides delivery mode methods
        /// directly: `.get_reply()`, `.try_get_reply()`, `.send()`,
        /// `.get_reply_unless_failed_for()`.
        #[derive(Debug, Clone)]
        pub struct #client_name {
            #(#client_fields,)*
        }

        impl #client_name {
            #legacy_client_items

            /// Number of methods in this interface.
            pub const METHOD_COUNT: u32 = #method_count;

            /// Create a client from a base token (for well-known or discovered interfaces).
            ///
            /// Methods target `base_token.adjusted(1)`, `.adjusted(2)`, etc.
            pub fn from_base<C: moonpool_transport::MessageCodec + Clone, P: moonpool_transport::Providers>(
                address: moonpool_transport::NetworkAddress,
                base_token: moonpool_transport::UID,
                codec: C,
                transport: &std::rc::Rc<moonpool_transport::NetTransport<P>>,
            ) -> Self {
                let handle: std::rc::Rc<dyn moonpool_transport::TransportHandle> =
                    transport.clone() as std::rc::Rc<dyn moonpool_transport::TransportHandle>;
                Self {
                    #(#client_from_base_inits,)*
                }
            }

            /// Create a client from a well-known token (deterministic addressing).
            ///
            /// Methods target `UID::well_known(token_id).adjusted(1)`, etc.
            pub fn well_known<C: moonpool_transport::MessageCodec + Clone, P: moonpool_transport::Providers>(
                address: moonpool_transport::NetworkAddress,
                token_id: u32,
                codec: C,
                transport: &std::rc::Rc<moonpool_transport::NetTransport<P>>,
            ) -> Self {
                Self::from_base(address, moonpool_transport::UID::well_known(token_id), codec, transport)
            }

            /// Get the address this client points to.
            pub fn address(&self) -> &moonpool_transport::NetworkAddress {
                &self.#first_field_name.endpoint().address
            }
        }

    };

    Ok(expanded)
}

/// Extract request and response types from method signature.
///
/// Expected signature: `async fn name(&self, req: ReqType) -> Result<RespType, RpcError>`
fn extract_method_types(sig: &syn::Signature) -> syn::Result<(Type, Type)> {
    // Skip &self, get the second argument
    let mut inputs = sig.inputs.iter();

    // First should be &self
    match inputs.next() {
        Some(FnArg::Receiver(_)) => {}
        _ => {
            return Err(syn::Error::new_spanned(
                sig,
                "Interface method must have &self as first parameter",
            ));
        }
    }

    // Second should be the request parameter
    let req_type = match inputs.next() {
        Some(FnArg::Typed(pat_type)) => (*pat_type.ty).clone(),
        _ => {
            return Err(syn::Error::new_spanned(
                sig,
                "Interface method must have a request parameter: async fn name(&self, req: ReqType) -> Result<RespType, RpcError>",
            ));
        }
    };

    // Extract response type from return type: Result<RespType, RpcError>
    let resp_type = match &sig.output {
        ReturnType::Type(_, ty) => extract_result_ok_type(ty)?,
        ReturnType::Default => {
            return Err(syn::Error::new_spanned(
                sig,
                "Interface method must return Result<RespType, RpcError>",
            ));
        }
    };

    Ok((req_type, resp_type))
}

/// Extract the Ok type from `Result<T, E>`.
fn extract_result_ok_type(ty: &Type) -> syn::Result<Type> {
    if let Type::Path(type_path) = ty
        && let Some(segment) = type_path.path.segments.last()
        && segment.ident == "Result"
        && let PathArguments::AngleBracketed(args) = &segment.arguments
        && let Some(GenericArgument::Type(ok_type)) = args.args.first()
    {
        return Ok(ok_type.clone());
    }

    Err(syn::Error::new_spanned(
        ty,
        "Interface method must return Result<RespType, RpcError>",
    ))
}

/// Convert a PascalCase name to snake_case.
fn to_snake_case(s: &str) -> String {
    let mut result = String::new();
    for (i, c) in s.chars().enumerate() {
        if c.is_uppercase() {
            if i > 0 {
                result.push('_');
            }
            result.push(c.to_ascii_lowercase());
        } else {
            result.push(c);
        }
    }
    result
}

// ============================================================================
// Shared Attribute Parsing
// ============================================================================

/// Parsed interface attribute (id is optional).
struct InterfaceAttr {
    id: Option<u64>,
}

impl syn::parse::Parse for InterfaceAttr {
    fn parse(input: syn::parse::ParseStream) -> syn::Result<Self> {
        if input.is_empty() {
            return Ok(InterfaceAttr { id: None });
        }

        let ident: Ident = input.parse()?;
        if ident != "id" {
            return Err(syn::Error::new_spanned(
                ident,
                "expected `id` in service attribute",
            ));
        }
        let _eq: syn::Token![=] = input.parse()?;
        let value: Expr = input.parse()?;

        // Extract the numeric value
        let id = match &value {
            Expr::Lit(ExprLit {
                lit: Lit::Int(lit_int),
                ..
            }) => lit_int.base10_parse::<u64>()?,
            _ => {
                return Err(syn::Error::new_spanned(
                    value,
                    "expected integer literal for interface id",
                ));
            }
        };

        Ok(InterfaceAttr { id: Some(id) })
    }
}
