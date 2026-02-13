//! Proc-macro for FDB-style Interface pattern.
//!
//! This crate provides the `#[interface]` attribute macro that generates
//! server and client types from a trait definition.
//!
//! # Example
//!
//! ```rust,ignore
//! use moonpool_transport::{interface, RpcError};
//!
//! #[interface(id = 0xCA1C_0000)]
//! trait Calculator {
//!     async fn add(&self, req: AddRequest) -> Result<AddResponse, RpcError>;
//!     async fn sub(&self, req: SubRequest) -> Result<SubResponse, RpcError>;
//! }
//! ```
//!
//! This generates:
//! - `CalculatorServer<C>` with `RequestStream` fields and `init()` method
//! - `CalculatorClient` with endpoint accessors and `bind()` method
//! - `BoundCalculatorClient<P, C>` implementing the `Calculator` trait
//! - The trait itself with `#[async_trait(?Send)]`

use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::{
    Expr, ExprLit, FnArg, GenericArgument, Ident, ItemTrait, Lit, PathArguments, ReturnType,
    TraitItem, Type, parse_macro_input,
};

/// Attribute macro for FDB-style Interface pattern.
///
/// Generates server, client, and bound client types from the trait definition.
///
/// # Attributes
///
/// - `#[interface(id = 0x...)]` - Required. Sets the interface ID (u64).
///
/// # Methods
///
/// Each method must be async with signature:
/// `async fn name(&self, req: ReqType) -> Result<RespType, RpcError>`
///
/// Method indices are derived from method declaration order, starting at 1.
/// Index 0 is reserved for virtual actor dispatch.
#[proc_macro_attribute]
pub fn interface(attr: TokenStream, item: TokenStream) -> TokenStream {
    let attr = parse_macro_input!(attr as InterfaceAttr);
    let item = parse_macro_input!(item as ItemTrait);

    match interface_impl(attr, item) {
        Ok(tokens) => tokens.into(),
        Err(err) => err.to_compile_error().into(),
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

    let expanded = quote! {
        // Emit the original trait with async_trait(?Send)
        #[async_trait::async_trait(?Send)]
        #trait_vis trait #name {
            #(#trait_items)*
        }

        /// Server-side interface with RequestStreams.
        ///
        /// Generated by `#[interface]`.
        pub struct #server_name<C: moonpool_transport::MessageCodec> {
            #(#server_fields,)*
        }

        impl<C: moonpool_transport::MessageCodec + Clone> #server_name<C> {
            /// Interface identifier.
            pub const INTERFACE_ID: u64 = #interface_id;

            /// Number of methods in this interface.
            pub const METHOD_COUNT: u32 = #method_count;

            /// Initialize the server interface, registering all handlers.
            pub fn init<P>(transport: &std::rc::Rc<moonpool_transport::NetTransport<P>>, codec: C) -> Self
            where
                P: moonpool_transport::Providers,
            {
                #(#server_inits)*
                Self { #(#server_field_names,)* }
            }
        }

        /// Client-side interface with Endpoints.
        ///
        /// Generated by `#[interface]`.
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
        /// Generated by `#[interface]`.
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
