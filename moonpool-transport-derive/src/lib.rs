//! Proc-macros for moonpool RPC interfaces and virtual actors.
//!
//! This crate provides the `#[service]` attribute macro (unified entry point)
//! and `#[actor_impl]` for auto-generating `ActorHandler` boilerplate.
//!
//! # RPC Example (`&self` methods)
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
//! - `CalculatorClient` with endpoint accessors and `bind()` method
//! - `BoundCalculatorClient<P, C>` implementing the `Calculator` trait
//! - The trait itself with `#[async_trait(?Send)]`
//!
//! # Actor Example (`&mut self` methods)
//!
//! ```rust,ignore
//! use moonpool_transport::{service, RpcError};
//!
//! #[service(id = 0xBA4E_4B00)]
//! trait BankAccount {
//!     async fn deposit(&mut self, req: DepositRequest) -> Result<BalanceResponse, RpcError>;
//! }
//! ```
//!
//! This generates:
//! - `BankAccountRef<P>` for typed actor calls
//! - `dispatch_bank_account()` function for routing method calls
//! - `bank_account_methods` module with `ACTOR_TYPE` and method constants

use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::{
    Expr, ExprLit, FnArg, GenericArgument, Ident, ImplItem, ItemImpl, ItemTrait, Lit,
    PathArguments, ReturnType, TraitItem, Type, parse_macro_input,
};

/// Unified attribute macro for defining RPC interfaces and virtual actors.
///
/// Generates all boilerplate from a trait definition. The mode is auto-detected
/// from the method receivers:
///
/// - **`&self` methods** → RPC mode: generates `{Name}Server`, `{Name}Client`,
///   `Bound{Name}Client`, and a `serve()` method for automatic request dispatch.
/// - **`&mut self` methods** → Actor mode: generates `{Name}Ref`, `dispatch_{name}()`,
///   and a `{name}_methods` module with `ACTOR_TYPE` and method constants.
///
/// All methods must use the same receiver type (mixing `&self` and `&mut self` is
/// a compile error).
///
/// # Attributes
///
/// - `#[service(id = 0x...)]` - Required. Sets the interface / actor type ID (u64).
///
/// # RPC Example
///
/// ```rust,ignore
/// #[service(id = 0x5049_4E47)]
/// trait PingPong {
///     async fn ping(&self, req: PingRequest) -> Result<PingResponse, RpcError>;
/// }
/// ```
///
/// # Actor Example
///
/// ```rust,ignore
/// #[service(id = 0xBA4E_4B00)]
/// trait BankAccount {
///     async fn deposit(&mut self, req: DepositRequest) -> Result<BalanceResponse, RpcError>;
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
            "all methods must use either `&self` (RPC mode) or `&mut self` (actor mode), not both",
        ));
    }

    if has_mut_ref {
        virtual_actor_impl(attr, item)
    } else {
        interface_impl(attr, item)
    }
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
    let bound_client_name = format_ident!("Bound{}Client", name);

    // Parse trait methods
    let mut method_infos: Vec<MethodInfo> = Vec::new();
    for (index, trait_item) in item.items.iter().enumerate() {
        if let TraitItem::Fn(method) = trait_item {
            let method_name = &method.sig.ident;

            // Extract request and response types from method signature
            let (req_type, resp_type) = extract_method_types(&method.sig)?;

            // Method indices start at 1; index 0 is reserved for virtual actor dispatch.
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

    // Generate client endpoint methods (for manual usage)
    let client_endpoint_methods = method_infos.iter().map(|m| {
        let name = &m.name;
        let endpoint_name = format_ident!("{}_endpoint", m.name);
        let idx = m.index;
        quote! {
            /// Get endpoint for this method (for manual send_request usage).
            pub fn #endpoint_name(&self) -> moonpool_transport::Endpoint {
                moonpool_transport::Endpoint::new(
                    self.base.address.clone(),
                    moonpool_transport::UID::new(Self::INTERFACE_ID, #idx as u64)
                )
            }

            #[doc(hidden)]
            #[deprecated(note = "use bind() for ergonomic calls, or method_endpoint() for manual usage")]
            pub fn #name(&self) -> moonpool_transport::Endpoint {
                self.#endpoint_name()
            }
        }
    });

    // Generate bound client trait implementation methods
    let bound_client_methods = method_infos.iter().map(|m| {
        let name = &m.name;
        let idx = m.index;
        let req_type = &m.req_type;
        let resp_type = &m.resp_type;
        quote! {
            async fn #name(&self, req: #req_type) -> Result<#resp_type, moonpool_transport::RpcError> {
                let endpoint = moonpool_transport::Endpoint::new(
                    self.base.address.clone(),
                    moonpool_transport::UID::new(Self::INTERFACE_ID, #idx as u64)
                );
                let future = moonpool_transport::send_request(
                    &self.transport,
                    &endpoint,
                    req,
                    self.codec.clone()
                )?;
                Ok(future.await?)
            }
        }
    });

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
                                Err(_) => {}
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

        /// Client-side interface with Endpoints.
        ///
        /// Generated by `#[service]`.
        /// Only the base endpoint is serialized; method endpoints are derived.
        ///
        /// Use `bind()` to create a bound client that implements the trait.
        #[derive(Clone)]
        pub struct #client_name {
            base: moonpool_transport::Endpoint,
        }

        impl #client_name {
            /// Interface identifier.
            pub const INTERFACE_ID: u64 = #interface_id;

            /// Number of methods in this interface.
            pub const METHOD_COUNT: u32 = #method_count;

            /// Create a new client interface from a network address.
            pub fn new(address: moonpool_transport::NetworkAddress) -> Self {
                Self {
                    base: moonpool_transport::Endpoint::new(address, moonpool_transport::UID::new(Self::INTERFACE_ID, 0)),
                }
            }

            /// Create a client interface from an existing endpoint.
            pub fn from_endpoint(base: moonpool_transport::Endpoint) -> Self {
                Self { base }
            }

            /// Get the base endpoint (method 0).
            pub fn base_endpoint(&self) -> &moonpool_transport::Endpoint {
                &self.base
            }

            /// Bind this client to a transport and codec for making RPC calls.
            ///
            /// The bound client implements the interface trait, allowing
            /// direct method calls like `bound_client.method(req).await?`.
            ///
            /// # Example
            ///
            /// ```rust,ignore
            /// let client = CalculatorClient::new(server_addr);
            /// let bound = client.bind(transport.clone(), JsonCodec);
            /// let result = bound.add(AddRequest { a: 1, b: 2 }).await?;
            /// ```
            pub fn bind<P, C>(
                &self,
                transport: std::rc::Rc<moonpool_transport::NetTransport<P>>,
                codec: C,
            ) -> #bound_client_name<P, C>
            where
                P: moonpool_transport::Providers,
                C: moonpool_transport::MessageCodec + Clone,
            {
                #bound_client_name {
                    base: self.base.clone(),
                    transport,
                    codec,
                }
            }

            #(#client_endpoint_methods)*
        }

        impl serde::Serialize for #client_name {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                self.base.serialize(serializer)
            }
        }

        impl<'de> serde::Deserialize<'de> for #client_name {
            fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let base = moonpool_transport::Endpoint::deserialize(deserializer)?;
                Ok(Self { base })
            }
        }

        /// Bound client that implements the interface trait.
        ///
        /// Generated by `#[service]`.
        /// Created by calling `bind()` on the client.
        pub struct #bound_client_name<P, C>
        where
            P: moonpool_transport::Providers,
            C: moonpool_transport::MessageCodec + Clone,
        {
            base: moonpool_transport::Endpoint,
            transport: std::rc::Rc<moonpool_transport::NetTransport<P>>,
            codec: C,
        }

        impl<P, C> #bound_client_name<P, C>
        where
            P: moonpool_transport::Providers,
            C: moonpool_transport::MessageCodec + Clone,
        {
            /// Interface identifier.
            pub const INTERFACE_ID: u64 = #interface_id;

            /// Number of methods in this interface.
            pub const METHOD_COUNT: u32 = #method_count;

            /// Get the server address this client is bound to.
            pub fn address(&self) -> &moonpool_transport::NetworkAddress {
                &self.base.address
            }
        }

        #[async_trait::async_trait(?Send)]
        impl<P, C> #name for #bound_client_name<P, C>
        where
            P: moonpool_transport::Providers,
            C: moonpool_transport::MessageCodec + Clone,
        {
            #(#bound_client_methods)*
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

// ============================================================================
// Virtual Actor Implementation (internal, called by #[service])
// ============================================================================

/// Virtual actor method info.
struct VirtualActorMethodInfo {
    index: u32,
    name: Ident,
    req_type: Type,
    resp_type: Type,
}

fn virtual_actor_impl(
    attr: InterfaceAttr,
    item: ItemTrait,
) -> syn::Result<proc_macro2::TokenStream> {
    let actor_type_id = attr.id;
    let trait_name = &item.ident;
    let trait_vis = &item.vis;

    // Derive names
    let ref_name = format_ident!("{}Ref", trait_name);
    let dispatch_fn_name = format_ident!("dispatch_{}", to_snake_case(&trait_name.to_string()));
    let methods_mod_name = format_ident!("{}_methods", to_snake_case(&trait_name.to_string()));

    // Parse trait methods
    let mut methods: Vec<VirtualActorMethodInfo> = Vec::new();
    for (index, trait_item) in item.items.iter().enumerate() {
        if let TraitItem::Fn(method) = trait_item {
            let method_name = &method.sig.ident;
            let (req_type, resp_type) = extract_virtual_actor_method_types(&method.sig)?;

            methods.push(VirtualActorMethodInfo {
                index: (index + 1) as u32,
                name: method_name.clone(),
                req_type,
                resp_type,
            });
        }
    }

    // Generate method constants module
    let method_constants = methods.iter().map(|m| {
        let const_name = format_ident!("{}", m.name.to_string().to_uppercase());
        let idx = m.index;
        let doc = format!("Method discriminant for `{}`.", m.name);
        quote! {
            #[doc = #doc]
            pub const #const_name: u32 = #idx;
        }
    });

    // Generate the trait items — rewrite each method to add generic params and ctx
    let trait_items: Vec<_> = item.items.iter().map(|ti| {
        if let TraitItem::Fn(method) = ti {
            let name = &method.sig.ident;
            let req_param = method.sig.inputs.iter().nth(1).expect("method has req param");
            let ret = &method.sig.output;
            quote! {
                async fn #name<P: moonpool_transport::Providers, C: moonpool_transport::MessageCodec>(
                    &mut self,
                    ctx: &moonpool::actors::ActorContext<P, C>,
                    #req_param,
                ) #ret;
            }
        } else {
            quote! { #ti }
        }
    }).collect();

    // Generate ActorRef methods
    let ref_methods = methods.iter().map(|m| {
        let name = &m.name;
        let idx = m.index;
        let req_type = &m.req_type;
        let resp_type = &m.resp_type;
        let doc = format!(
            "Send a `{}` request to this actor.",
            name
        );
        quote! {
            #[doc = #doc]
            pub async fn #name(&self, req: #req_type) -> Result<#resp_type, moonpool_transport::RpcError> {
                self.router.send_actor_request(&self.id, #idx, &req).await
                    .map_err(|e| match e {
                        moonpool::actors::ActorError::Messaging(m) => moonpool_transport::RpcError::Messaging(m),
                        moonpool::actors::ActorError::Reply(r) => moonpool_transport::RpcError::Reply(r),
                        other => moonpool_transport::RpcError::Reply(
                            moonpool_transport::ReplyError::Serialization { message: other.to_string() }
                        ),
                    })
            }
        }
    });

    // Generate dispatch function match arms
    let dispatch_arms = methods.iter().map(|m| {
        let idx = m.index;
        let name = &m.name;
        let req_type = &m.req_type;
        quote! {
            #idx => {
                let req: #req_type = codec.decode(body).map_err(moonpool::actors::ActorError::Codec)?;
                let resp = actor.#name(ctx, req).await.map_err(|e| match e {
                    moonpool_transport::RpcError::Messaging(m) => moonpool::actors::ActorError::Messaging(m),
                    moonpool_transport::RpcError::Reply(r) => moonpool::actors::ActorError::Reply(r),
                })?;
                codec.encode(&resp).map_err(moonpool::actors::ActorError::Codec)
            }
        }
    });

    let expanded = quote! {
        // Emit the trait with async_trait(?Send), generic ctx on each method
        #[async_trait::async_trait(?Send)]
        #trait_vis trait #trait_name {
            #(#trait_items)*
        }


        /// Actor type and method discriminant constants for this virtual actor.
        #trait_vis mod #methods_mod_name {
            /// The actor type ID for this virtual actor.
            pub const ACTOR_TYPE: moonpool::actors::ActorType = moonpool::actors::ActorType(#actor_type_id);
            #(#method_constants)*
        }

        /// Typed reference to a virtual actor instance (caller-side handle).
        ///
        /// Created cheaply without any network call. The actor is activated
        /// on first message, not when the ref is created (Orleans pattern).
        ///
        /// Generated by `#[virtual_actor]`.
        #trait_vis struct #ref_name<P: moonpool_transport::Providers, C: moonpool_transport::MessageCodec = moonpool_transport::JsonCodec> {
            id: moonpool::actors::ActorId,
            router: std::rc::Rc<moonpool::actors::ActorRouter<P, C>>,
        }

        impl<P: moonpool_transport::Providers, C: moonpool_transport::MessageCodec> #ref_name<P, C> {
            /// The actor type ID for this virtual actor.
            pub const ACTOR_TYPE: moonpool::actors::ActorType = moonpool::actors::ActorType(#actor_type_id);

            /// Get a reference to a virtual actor by identity.
            ///
            /// Does NOT activate the actor — just creates a handle.
            /// The actor is activated on first message.
            pub fn new(identity: impl Into<String>, router: &std::rc::Rc<moonpool::actors::ActorRouter<P, C>>) -> Self {
                Self {
                    id: moonpool::actors::ActorId::new(
                        moonpool::actors::ActorType(#actor_type_id),
                        identity.into(),
                    ),
                    router: router.clone(),
                }
            }

            /// Get the actor ID for this reference.
            pub fn id(&self) -> &moonpool::actors::ActorId {
                &self.id
            }

            #(#ref_methods)*
        }

        impl<P: moonpool_transport::Providers, C: moonpool_transport::MessageCodec> moonpool::actors::ActorRef<P, C> for #ref_name<P, C> {
            fn from_router(
                identity: impl Into<String>,
                router: &std::rc::Rc<moonpool::actors::ActorRouter<P, C>>,
            ) -> Self {
                Self::new(identity, router)
            }
        }

        /// Generated dispatch function for types implementing the [`#trait_name`] trait.
        ///
        /// Routes method calls by discriminant to the appropriate trait method,
        /// handling serialization/deserialization automatically.
        ///
        /// Use this in your `ActorHandler::dispatch` implementation:
        ///
        /// ```rust,ignore
        /// async fn dispatch<P: Providers, C: MessageCodec>(
        ///     &mut self, ctx: &ActorContext<P, C>, method: u32, body: &[u8],
        /// ) -> Result<Vec<u8>, ActorError> {
        ///     #dispatch_fn_name(self, ctx, method, body).await
        /// }
        /// ```
        #trait_vis async fn #dispatch_fn_name<T: #trait_name, P: moonpool_transport::Providers, C: moonpool_transport::MessageCodec>(
            actor: &mut T,
            ctx: &moonpool::actors::ActorContext<P, C>,
            method: u32,
            body: &[u8],
        ) -> Result<Vec<u8>, moonpool::actors::ActorError> {
            let codec = moonpool_transport::JsonCodec;
            match method {
                #(#dispatch_arms)*
                _ => Err(moonpool::actors::ActorError::UnknownMethod(method)),
            }
        }
    };

    Ok(expanded)
}

/// Extract request and response types from virtual actor method signature.
///
/// Expected signature: `async fn name(&mut self, req: ReqType) -> Result<RespType, RpcError>`
fn extract_virtual_actor_method_types(sig: &syn::Signature) -> syn::Result<(Type, Type)> {
    let mut inputs = sig.inputs.iter();

    // First should be &mut self
    match inputs.next() {
        Some(FnArg::Receiver(_)) => {}
        _ => {
            return Err(syn::Error::new_spanned(
                sig,
                "Virtual actor method must have &mut self as first parameter",
            ));
        }
    }

    // Second should be the request parameter
    let req_type = match inputs.next() {
        Some(FnArg::Typed(pat_type)) => (*pat_type.ty).clone(),
        _ => {
            return Err(syn::Error::new_spanned(
                sig,
                "Virtual actor method must have a request parameter: async fn name(&mut self, req: ReqType) -> Result<RespType, RpcError>",
            ));
        }
    };

    // Extract response type from return type: Result<RespType, RpcError>
    let resp_type = match &sig.output {
        ReturnType::Type(_, ty) => extract_result_ok_type(ty)?,
        ReturnType::Default => {
            return Err(syn::Error::new_spanned(
                sig,
                "Virtual actor method must return Result<RespType, RpcError>",
            ));
        }
    };

    Ok((req_type, resp_type))
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

// ============================================================================
// #[actor_impl] Attribute Macro
// ============================================================================

/// Attribute macro for auto-generating `ActorHandler` boilerplate.
///
/// Applied on `impl ActorHandler for MyStruct` blocks. Takes the actor trait
/// name as argument and auto-generates `actor_type()` and `dispatch()`.
/// User-provided methods (`deactivation_hint`, `on_activate`, `on_deactivate`)
/// are preserved; missing lifecycle methods get defaults.
///
/// # Example
///
/// ```rust,ignore
/// #[actor_impl(BankAccount)]
/// impl ActorHandler for BankAccountImpl {
///     fn deactivation_hint(&self) -> DeactivationHint {
///         DeactivationHint::DeactivateAfterIdle(Duration::from_secs(30))
///     }
///
///     async fn on_activate<P: Providers, C: MessageCodec>(
///         &mut self, ctx: &ActorContext<P, C>,
///     ) -> Result<(), ActorError> {
///         // load persistent state...
///         Ok(())
///     }
/// }
/// ```
///
/// For the simplest case (all defaults):
///
/// ```rust,ignore
/// #[actor_impl(BankAccount)]
/// impl ActorHandler for BankAccountImpl {}
/// ```
#[proc_macro_attribute]
pub fn actor_impl(attr: TokenStream, item: TokenStream) -> TokenStream {
    let trait_name = parse_macro_input!(attr as Ident);
    let item = parse_macro_input!(item as ItemImpl);

    match actor_impl_impl(trait_name, item) {
        Ok(tokens) => tokens.into(),
        Err(err) => err.to_compile_error().into(),
    }
}

fn actor_impl_impl(trait_name: Ident, mut item: ItemImpl) -> syn::Result<proc_macro2::TokenStream> {
    let snake = to_snake_case(&trait_name.to_string());
    let methods_mod = format_ident!("{}_methods", snake);
    let dispatch_fn = format_ident!("dispatch_{}", snake);

    // Collect names of user-provided methods
    let user_methods: Vec<String> = item
        .items
        .iter()
        .filter_map(|item| {
            if let ImplItem::Fn(method) = item {
                Some(method.sig.ident.to_string())
            } else {
                None
            }
        })
        .collect();

    let has_actor_type = user_methods.iter().any(|m| m == "actor_type");
    let has_dispatch = user_methods.iter().any(|m| m == "dispatch");
    let has_deactivation_hint = user_methods.iter().any(|m| m == "deactivation_hint");
    let has_on_activate = user_methods.iter().any(|m| m == "on_activate");
    let has_on_deactivate = user_methods.iter().any(|m| m == "on_deactivate");

    // Always inject actor_type()
    if !has_actor_type {
        let method: ImplItem = syn::parse_quote! {
            fn actor_type() -> moonpool::actors::ActorType {
                #methods_mod::ACTOR_TYPE
            }
        };
        item.items.insert(0, method);
    }

    // Always inject dispatch()
    if !has_dispatch {
        let method: ImplItem = syn::parse_quote! {
            async fn dispatch<P: moonpool_transport::Providers, C: moonpool_transport::MessageCodec>(
                &mut self,
                ctx: &moonpool::actors::ActorContext<P, C>,
                method: u32,
                body: &[u8],
            ) -> Result<Vec<u8>, moonpool::actors::ActorError> {
                #dispatch_fn(self, ctx, method, body).await
            }
        };
        item.items.push(method);
    }

    // Inject default deactivation_hint() if not provided
    if !has_deactivation_hint {
        let method: ImplItem = syn::parse_quote! {
            fn deactivation_hint(&self) -> moonpool::actors::DeactivationHint {
                moonpool::actors::DeactivationHint::KeepAlive
            }
        };
        item.items.push(method);
    }

    // Inject default on_activate() if not provided
    if !has_on_activate {
        let method: ImplItem = syn::parse_quote! {
            async fn on_activate<P: moonpool_transport::Providers, C: moonpool_transport::MessageCodec>(
                &mut self,
                _ctx: &moonpool::actors::ActorContext<P, C>,
            ) -> Result<(), moonpool::actors::ActorError> {
                Ok(())
            }
        };
        item.items.push(method);
    }

    // Inject default on_deactivate() if not provided
    if !has_on_deactivate {
        let method: ImplItem = syn::parse_quote! {
            async fn on_deactivate<P: moonpool_transport::Providers, C: moonpool_transport::MessageCodec>(
                &mut self,
                _ctx: &moonpool::actors::ActorContext<P, C>,
            ) -> Result<(), moonpool::actors::ActorError> {
                Ok(())
            }
        };
        item.items.push(method);
    }

    // Emit with #[async_trait(?Send)]
    Ok(quote! {
        #[async_trait::async_trait(?Send)]
        #item
    })
}
