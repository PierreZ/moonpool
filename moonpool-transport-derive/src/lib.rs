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
//! #[service(id = 0xCA1C_0000)]
//! trait Calculator {
//!     async fn add(&self, req: AddRequest) -> Result<AddResponse, RpcError>;
//!     async fn sub(&self, req: SubRequest) -> Result<SubResponse, RpcError>;
//! }
//! ```
//!
//! This generates:
//! - `CalculatorServer<C>` with `RequestStream` fields, `init()`, and `serve()`
//! - `CalculatorClient` with `ServiceEndpoint` fields for each method
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
/// - `#[service(id = 0x...)]` - Required. Sets the interface ID (u64).
///
/// # Example
///
/// ```rust,ignore
/// #[service(id = 0x5049_4E47)]
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
    let interface_id = attr.id;
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

    // Generate server fields
    let server_fields = method_infos.iter().map(|m| {
        let name = &m.name;
        let req_type = &m.req_type;
        quote! { pub #name: moonpool_transport::RequestStream<#req_type, C> }
    });

    // Generate server init - clone codec for all but the last field
    let server_inits: Vec<_> = method_infos
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let name = &m.name;
            let idx = m.index;
            let is_last = i == method_infos.len() - 1;
            if is_last {
                quote! {
                    let (#name, _) = transport.register_handler_at(Self::INTERFACE_ID, #idx as u64, codec);
                }
            } else {
                quote! {
                    let (#name, _) = transport.register_handler_at(Self::INTERFACE_ID, #idx as u64, codec.clone());
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
            /// Typed endpoint for this method. Call delivery methods directly:
            /// `.get_reply()`, `.try_get_reply()`, `.send()`, `.get_reply_unless_failed_for()`.
            pub #name: moonpool_transport::ServiceEndpoint<#req_type, #resp_type, C>
        }
    });

    // Generate client field constructors
    let client_field_inits = method_infos.iter().map(|m| {
        let name = &m.name;
        let idx = m.index;
        quote! {
            #name: moonpool_transport::ServiceEndpoint::new(
                moonpool_transport::Endpoint::new(
                    address.clone(),
                    moonpool_transport::UID::new(Self::INTERFACE_ID, #idx as u64),
                ),
                codec.clone(),
            )
        }
    });

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
            let resp_type = &m.resp_type;
            let task_name = format!("{}_{}", trait_name_snake, m.name);
            quote! {
                {
                    let stream = self.#method_name;
                    let t = transport.clone();
                    let h = handler.clone();
                    providers.task().spawn_task(#task_name, async move {
                        while let Some((req, reply)) = stream
                            .recv_with_transport::<_, #resp_type>(&t)
                            .await
                        {
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

    let expanded = quote! {
        // Emit the original trait with async_trait(?Send)
        #[async_trait::async_trait(?Send)]
        #trait_vis trait #name {
            #(#trait_items)*
        }

        /// Server-side interface with RequestStreams.
        ///
        /// Generated by `#[service]`.
        pub struct #server_name<C: moonpool_transport::MessageCodec> {
            #(#server_fields,)*
        }

        impl<C: moonpool_transport::MessageCodec + Clone> #server_name<C> {
            /// Interface identifier.
            pub const INTERFACE_ID: u64 = #interface_id;

            /// Number of methods in this interface.
            pub const METHOD_COUNT: u32 = #method_count;

            /// Initialize the server interface, registering all handlers.
            ///
            /// Returns the server with individual `RequestStream` fields for
            /// manual control. For a simpler pattern, use [`serve()`](Self::serve).
            pub fn init<P>(transport: &std::rc::Rc<moonpool_transport::NetTransport<P>>, codec: C) -> Self
            where
                P: moonpool_transport::Providers,
            {
                #(#server_inits)*
                Self { #(#server_field_names,)* }
            }

            /// Consume this server and spawn handler tasks for all methods.
            ///
            /// Each method gets its own task that loops on `recv_with_transport`
            /// and dispatches to the handler. Returns a [`ServerHandle`](moonpool_transport::ServerHandle)
            /// that stops all tasks when dropped.
            ///
            /// # Example
            ///
            /// ```rust,ignore
            /// let server = MyServer::init(&transport, JsonCodec);
            /// let handle = server.serve(transport.clone(), Rc::new(handler), &providers);
            /// // Tasks run until handle is dropped or stop() is called
            /// ```
            pub fn serve<P, H>(
                self,
                transport: std::rc::Rc<moonpool_transport::NetTransport<P>>,
                handler: std::rc::Rc<H>,
                providers: &P,
            ) -> moonpool_transport::ServerHandle
            where
                P: moonpool_transport::Providers,
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
        ///
        /// FDB equivalent: interface structs like `StorageServerInterface`.
        ///
        /// # Example
        ///
        /// ```rust,ignore
        /// let calc = CalculatorClient::new(server_addr, JsonCodec);
        ///
        /// // Choose delivery mode at call site:
        /// let resp = calc.add.get_reply(&transport, req).await?;
        /// let resp = calc.add.try_get_reply(&transport, req).await?;
        /// calc.add.send(&transport, req)?;
        /// ```
        #[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
        #[serde(bound(
            serialize = "",
            deserialize = "C: moonpool_transport::MessageCodec + Default",
        ))]
        pub struct #client_name<C: moonpool_transport::MessageCodec> {
            #(#client_fields,)*
        }

        impl<C: moonpool_transport::MessageCodec + Clone> #client_name<C> {
            /// Interface identifier.
            pub const INTERFACE_ID: u64 = #interface_id;

            /// Number of methods in this interface.
            pub const METHOD_COUNT: u32 = #method_count;

            /// Create a new client interface from a network address and codec.
            pub fn new(address: moonpool_transport::NetworkAddress, codec: C) -> Self {
                Self {
                    #(#client_field_inits,)*
                }
            }

            /// Get the address this client points to.
            pub fn address(&self) -> &moonpool_transport::NetworkAddress {
                // All fields share the same address; use the first one.
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

/// Parsed interface attribute.
struct InterfaceAttr {
    id: u64,
}

impl syn::parse::Parse for InterfaceAttr {
    fn parse(input: syn::parse::ParseStream) -> syn::Result<Self> {
        let ident: Ident = input.parse()?;
        if ident != "id" {
            return Err(syn::Error::new_spanned(
                ident,
                "expected `id` in interface attribute",
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

        Ok(InterfaceAttr { id })
    }
}
