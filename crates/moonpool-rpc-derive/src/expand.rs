//! Parsing and code generation for `#[service]`.

use proc_macro2::{Span, TokenStream};
use quote::{format_ident, quote};
use syn::parse::Parser;
use syn::punctuated::Punctuated;
use syn::spanned::Spanned;
use syn::{
    Attribute, Expr, FnArg, Ident, ItemTrait, Lit, MetaNameValue, ReturnType, Token, TraitItem,
    TraitItemFn, Type,
};

/// Names generated next to the trait; a method whose marker would collide
/// with one of them is refused.
const RESERVED_SUFFIXES: [&str; 5] = ["Interface", "Ref", "Client", "Server", "Request"];

/// `key = value` arguments, each required exactly once.
struct Args {
    values: Vec<(String, Expr)>,
}

impl Args {
    fn parse(tokens: TokenStream, allowed: &[&str], what: &str) -> syn::Result<Self> {
        let pairs = Punctuated::<MetaNameValue, Token![,]>::parse_terminated.parse2(tokens)?;
        let mut values: Vec<(String, Expr)> = Vec::new();
        for pair in pairs {
            let Some(key) = pair.path.get_ident().map(ToString::to_string) else {
                return Err(syn::Error::new(pair.path.span(), "expected a plain key"));
            };
            if !allowed.contains(&key.as_str()) {
                return Err(syn::Error::new(
                    pair.path.span(),
                    format!(
                        "unknown {what} argument `{key}` (expected {})",
                        allowed.join(", ")
                    ),
                ));
            }
            if values.iter().any(|(existing, _)| *existing == key) {
                return Err(syn::Error::new(
                    pair.path.span(),
                    format!("duplicate {what} argument `{key}`"),
                ));
            }
            values.push((key, pair.value));
        }
        Ok(Self { values })
    }

    fn required(&self, key: &str, span: Span, what: &str) -> syn::Result<Expr> {
        self.values
            .iter()
            .find(|(existing, _)| existing == key)
            .map(|(_, value)| value.clone())
            .ok_or_else(|| {
                syn::Error::new(
                    span,
                    format!("{what} needs an explicit `{key} = ...`: ids are never derived from names or order"),
                )
            })
    }
}

/// One parsed method.
struct Method {
    ident: Ident,
    marker: Ident,
    variant: Ident,
    stream: Ident,
    id: Expr,
    schema: Expr,
    request: Type,
    reply: Type,
    docs: Vec<Attribute>,
    request_name: Ident,
}

fn camel(snake: &str) -> String {
    snake
        .split('_')
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            chars.next().map_or_else(String::new, |first| {
                first.to_uppercase().chain(chars).collect::<String>()
            })
        })
        .collect()
}

fn literal_u64(expr: &Expr) -> Option<u64> {
    match expr {
        Expr::Lit(literal) => match &literal.lit {
            Lit::Int(int) => int.base10_parse().ok(),
            _ => None,
        },
        _ => None,
    }
}

fn is_doc(attr: &Attribute) -> bool {
    attr.path().is_ident("doc")
}

fn parse_method(service: &Ident, item: TraitItemFn) -> syn::Result<Method> {
    let span = item.sig.ident.span();
    let mut method_args = None;
    let mut docs = Vec::new();
    for attr in item.attrs {
        if attr.path().is_ident("method") {
            if method_args.is_some() {
                return Err(syn::Error::new(attr.span(), "duplicate #[method(...)]"));
            }
            let list = attr.meta.require_list()?;
            method_args = Some(Args::parse(
                list.tokens.clone(),
                &["id", "schema"],
                "method",
            )?);
        } else if is_doc(&attr) {
            docs.push(attr);
        } else {
            return Err(syn::Error::new(
                attr.span(),
                "only doc comments and #[method(...)] are allowed on a service method",
            ));
        }
    }
    let Some(args) = method_args else {
        return Err(syn::Error::new(
            span,
            "every service method needs #[method(id = <u32>, schema = <u16>)]",
        ));
    };
    let signature = &item.sig;
    if signature.asyncness.is_none() {
        return Err(syn::Error::new(span, "service methods are `async fn`"));
    }
    if !signature.generics.params.is_empty() || signature.generics.where_clause.is_some() {
        return Err(syn::Error::new(
            signature.generics.span(),
            "service methods cannot be generic",
        ));
    }
    if item.default.is_some() {
        return Err(syn::Error::new(
            span,
            "service methods cannot have a default body",
        ));
    }
    let mut inputs = signature.inputs.iter();
    match inputs.next() {
        Some(FnArg::Receiver(receiver))
            if receiver.reference.is_some() && receiver.mutability.is_none() => {}
        _ => {
            return Err(syn::Error::new(
                signature.inputs.span(),
                "service methods take `&self` first",
            ));
        }
    }
    let request = match (inputs.next(), inputs.next()) {
        (Some(FnArg::Typed(argument)), None) => (*argument.ty).clone(),
        _ => {
            return Err(syn::Error::new(
                signature.inputs.span(),
                "service methods take exactly one request argument after `&self`",
            ));
        }
    };
    let reply = match &signature.output {
        ReturnType::Default => syn::parse_quote!(()),
        ReturnType::Type(_, ty) => (**ty).clone(),
    };
    let variant = format_ident!("{}", camel(&signature.ident.to_string()));
    if RESERVED_SUFFIXES.contains(&variant.to_string().as_str()) {
        return Err(syn::Error::new(
            span,
            format!(
                "a method named `{}` would collide with the generated `{service}{variant}`",
                signature.ident
            ),
        ));
    }
    Ok(Method {
        marker: format_ident!("{service}{variant}"),
        stream: format_ident!("stream_{}", signature.ident),
        id: args.required("id", span, "a service method")?,
        schema: args.required("schema", span, "a service method")?,
        request_name: format_ident!("request"),
        ident: signature.ident.clone(),
        variant,
        request,
        reply,
        docs,
    })
}

pub(crate) fn expand(attr: TokenStream, item: TokenStream) -> syn::Result<TokenStream> {
    let args = Args::parse(attr, &["id", "version"], "service")?;
    let service: ItemTrait = syn::parse2(item)?;
    let span = service.ident.span();
    let interface_id = args.required("id", span, "a service")?;
    let version = args.required("version", span, "a service")?;
    if !service.generics.params.is_empty() || service.generics.where_clause.is_some() {
        return Err(syn::Error::new(
            service.generics.span(),
            "service traits cannot be generic",
        ));
    }
    if !service.supertraits.is_empty() {
        return Err(syn::Error::new(
            service.supertraits.span(),
            "service traits cannot declare supertraits (Send + Sync is added)",
        ));
    }
    if service.unsafety.is_some() || service.auto_token.is_some() {
        return Err(syn::Error::new(span, "service traits are plain traits"));
    }
    let ident = service.ident.clone();
    let mut methods = Vec::new();
    for item in service.items {
        match item {
            TraitItem::Fn(function) => methods.push(parse_method(&ident, function)?),
            other => {
                return Err(syn::Error::new(
                    other.span(),
                    "a service trait holds only `async fn` methods",
                ));
            }
        }
    }
    if methods.is_empty() {
        return Err(syn::Error::new(span, "a service needs at least one method"));
    }
    let mut literal_ids: Vec<(u64, &Ident)> = Vec::new();
    for method in &methods {
        if let Some(id) = literal_u64(&method.id) {
            if let Some((_, first)) = literal_ids.iter().find(|(seen, _)| *seen == id) {
                return Err(syn::Error::new(
                    method.ident.span(),
                    format!("method id {id} is already used by `{first}`"),
                ));
            }
            literal_ids.push((id, &method.ident));
        }
    }
    Ok(generate(
        &Service {
            ident,
            vis: service.vis,
            attrs: service.attrs,
            interface_id,
            version,
        },
        &methods,
    ))
}

struct Service {
    ident: Ident,
    vis: syn::Visibility,
    attrs: Vec<Attribute>,
    interface_id: Expr,
    version: Expr,
}

/// The generated names of one service.
struct Names {
    rpc: TokenStream,
    name: String,
    interface: Ident,
    reference: Ident,
    client: Ident,
    server: Ident,
    request: Ident,
}

impl Names {
    fn of(ident: &Ident) -> Self {
        Self {
            rpc: quote!(::moonpool_rpc),
            name: ident.to_string(),
            interface: format_ident!("{ident}Interface"),
            reference: format_ident!("{ident}Ref"),
            client: format_ident!("{ident}Client"),
            server: format_ident!("{ident}Server"),
            request: format_ident!("{ident}Request"),
        }
    }
}

fn generate(service: &Service, methods: &[Method]) -> TokenStream {
    let names = Names::of(&service.ident);
    let definitions = definitions(service, &names, methods);
    let client = client(service, &names, methods);
    let server = server(service, &names, methods);
    quote! {
        #definitions
        #client
        #server
    }
}

/// The handler trait, the interface and method markers and the id check.
fn definitions(service: &Service, names: &Names, methods: &[Method]) -> TokenStream {
    let Service {
        ident,
        vis,
        attrs,
        interface_id,
        version,
    } = service;
    let Names {
        rpc,
        name,
        interface,
        reference,
        ..
    } = names;
    let count = methods.len();
    let interface_doc =
        format!("The interface marker of [`{ident}`]: its explicit id and version.");
    let reference_doc = format!("A serialisable reference to a [`{ident}`] group.");
    let trait_methods = methods.iter().map(|method| {
        let Method {
            ident,
            request,
            reply,
            docs,
            request_name,
            ..
        } = method;
        quote! {
            #(#docs)*
            fn #ident(&self, #request_name: #request)
                -> impl ::core::future::Future<Output = #reply> + ::core::marker::Send;
        }
    });
    let markers = methods.iter().map(|method| {
        let Method {
            ident: method_ident,
            marker,
            id,
            schema,
            request,
            reply,
            ..
        } = method;
        let doc = format!("The [`{ident}::{method_ident}`] method: its explicit id and schema.");
        let label = format!("{name}.{method_ident}");
        quote! {
            #[doc = #doc]
            #vis struct #marker;

            impl #rpc::RpcMethod for #marker {
                type Request = #request;
                type Reply = #reply;
                const METHOD: #rpc::MethodId = #rpc::MethodId::new(#id);
                const SCHEMA: #rpc::SchemaVersion = #rpc::SchemaVersion::new(#schema);
                const NAME: &'static str = #label;
            }

            impl #rpc::InterfaceMethod<#interface> for #marker {}
        }
    });
    let ids = methods.iter().map(|method| &method.id);
    let duplicate_message = format!("duplicate method id in service {name}");
    quote! {
        #(#attrs)*
        #vis trait #ident: ::core::marker::Send + ::core::marker::Sync {
            #(#trait_methods)*
        }

        #[doc = #interface_doc]
        #vis struct #interface;

        impl #rpc::RpcInterface for #interface {
            const INTERFACE: #rpc::InterfaceId = #rpc::InterfaceId::new(#interface_id);
            const VERSION: #rpc::SchemaVersion = #rpc::SchemaVersion::new(#version);
            const NAME: &'static str = #name;
        }

        #(#markers)*

        // Method ids given as constants are checked here, at compile time.
        const _: () = {
            let ids: [u32; #count] = [#(#ids),*];
            let mut first = 0;
            while first < #count {
                let mut second = first + 1;
                while second < #count {
                    if ids[first] == ids[second] {
                        panic!(#duplicate_message);
                    }
                    second += 1;
                }
                first += 1;
            }
        };

        #[doc = #reference_doc]
        #vis type #reference = #rpc::InterfaceRef<#interface>;
    }
}

/// The typed client of a bound reference.
fn client(service: &Service, names: &Names, methods: &[Method]) -> TokenStream {
    let Service { ident, vis, .. } = service;
    let Names {
        rpc,
        interface,
        reference,
        client,
        ..
    } = names;
    let client_doc = format!(
        "A [`{reference}`] bound to a runtime: one `ServiceClient` per method, with every delivery mode."
    );
    let client_methods = methods.iter().map(|method| {
        let Method {
            ident: method_ident,
            marker,
            ..
        } = method;
        let doc = format!("A client of [`{ident}::{method_ident}`].");
        quote! {
            #[doc = #doc]
            #[must_use]
            pub fn #method_ident(&self) -> #rpc::ServiceClient<P, #marker> {
                self.inner.method::<#marker>()
            }
        }
    });
    quote! {
        #[doc = #client_doc]
        #vis struct #client<P: #rpc::__private::Providers> {
            inner: #rpc::InterfaceClient<P, #interface>,
        }

        impl<P: #rpc::__private::Providers> #client<P> {
            /// Bind a reference to a runtime after checking it.
            ///
            /// # Errors
            ///
            /// `InvalidReference` (never admitted) when the reference is
            /// malformed or names another interface.
            pub fn bind(
                target: &#reference,
                rpc: &#rpc::RpcHandle<P>,
            ) -> ::core::result::Result<Self, #rpc::RpcError> {
                ::core::result::Result::Ok(Self {
                    inner: target.bind(rpc)?,
                })
            }

            /// The reference this client calls.
            #[must_use]
            pub fn target(&self) -> &#reference {
                self.inner.target()
            }

            #(#client_methods)*
        }

        impl<P: #rpc::__private::Providers> ::core::clone::Clone for #client<P> {
            fn clone(&self) -> Self {
                Self {
                    inner: self.inner.clone(),
                }
            }
        }

        impl<P: #rpc::__private::Providers> ::core::fmt::Debug for #client<P> {
            fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                ::core::fmt::Debug::fmt(&self.inner, f)
            }
        }
    }
}

/// The request enum and the server: registration, the multiplexed pull,
/// dispatch.
fn server(service: &Service, names: &Names, methods: &[Method]) -> TokenStream {
    let Service { ident, vis, .. } = service;
    let Names {
        rpc,
        interface,
        reference,
        server,
        request: request_enum,
        ..
    } = names;
    let server_doc = format!(
        "A registered [`{ident}`] group and its request streams (the pull primitive, multiplexed)."
    );
    let request_doc =
        format!("One admitted request to a [`{ident}`] method, with its reply handle.");
    let variants = methods.iter().map(|method| {
        let Method {
            ident: method_ident,
            marker,
            variant,
            ..
        } = method;
        let doc = format!("A request to [`{ident}::{method_ident}`].");
        quote! {
            #[doc = #doc]
            #variant(#rpc::IncomingRequest<#marker>)
        }
    });
    let stream_fields = methods.iter().map(|method| {
        let Method { stream, marker, .. } = method;
        quote!(#stream: #rpc::RequestStream<#marker>)
    });
    let stream_inits = methods.iter().map(|method| {
        let Method { stream, marker, .. } = method;
        quote!(let #stream = group.serve::<#marker>()?;)
    });
    let stream_names: Vec<&Ident> = methods.iter().map(|method| &method.stream).collect();
    let poll_next = poll_next(names, methods);
    let dispatch_arms = dispatch_arms(names, methods);
    quote! {
        #[doc = #request_doc]
        #vis enum #request_enum {
            #(#variants),*
        }

        #[doc = #server_doc]
        #vis struct #server {
            group: #rpc::ServiceGroup<#interface>,
            #(#stream_fields,)*
            cursor: usize,
        }

        impl #server {
            /// Register a fresh group in this runtime's incarnation and
            /// serve every method.
            ///
            /// # Errors
            ///
            /// As for `RpcHandle::register_group`.
            pub fn register<P: #rpc::__private::Providers>(
                rpc: &#rpc::RpcHandle<P>,
                access: #rpc::AccessClass,
            ) -> ::core::result::Result<Self, #rpc::RpcError> {
                let group = rpc.register_group::<#interface>(access)?;
                #(#stream_inits)*
                ::core::result::Result::Ok(Self {
                    group,
                    #(#stream_names,)*
                    cursor: 0,
                })
            }

            /// The serialisable reference to publish.
            #[must_use]
            pub fn interface_ref(&self) -> #reference {
                self.group.interface_ref()
            }

            /// The underlying group.
            #[must_use]
            pub fn group(&self) -> &#rpc::ServiceGroup<#interface> {
                &self.group
            }

            #poll_next

            /// The next request of any method; `None` once the group or the
            /// runtime is gone.
            pub async fn next(&mut self) -> ::core::option::Option<#request_enum> {
                ::core::future::poll_fn(|cx| self.poll_next(cx)).await
            }

            /// Run the handler for one request and reply with its answer
            /// (one-way requests are run and never answered).
            pub async fn dispatch<S: #ident>(service: &S, request: #request_enum) {
                match request {
                    #(#dispatch_arms)*
                }
            }

            /// Pull and dispatch requests one at a time until the group or
            /// the runtime is gone. For concurrency, or any other reply
            /// policy, drive [`next`](Self::next) yourself.
            pub async fn serve<S: #ident>(mut self, service: &S) {
                while let ::core::option::Option::Some(request) = self.next().await {
                    Self::dispatch(service, request).await;
                }
            }
        }

        impl ::core::fmt::Debug for #server {
            fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                ::core::fmt::Debug::fmt(&self.group, f)
            }
        }
    }
}

/// `KvServer::poll_next`: every stream in rotation.
fn poll_next(names: &Names, methods: &[Method]) -> TokenStream {
    let request_enum = &names.request;
    let count = methods.len();
    let polls = methods.iter().enumerate().map(|(index, method)| {
        let Method {
            stream, variant, ..
        } = method;
        quote! {
            #index => self.#stream.poll_recv(cx).map(|polled| polled.map(#request_enum::#variant)),
        }
    });
    quote! {
        /// Poll every method's stream, starting after the one that
        /// yielded last so no method starves; `None` once the group or
        /// the runtime is gone.
        pub fn poll_next(
            &mut self,
            cx: &mut ::core::task::Context<'_>,
        ) -> ::core::task::Poll<::core::option::Option<#request_enum>> {
            let mut ended = 0;
            for offset in 0..#count {
                let slot = (self.cursor + offset) % #count;
                let polled = match slot {
                    #(#polls)*
                    _ => ::core::task::Poll::Ready(::core::option::Option::None),
                };
                match polled {
                    ::core::task::Poll::Ready(::core::option::Option::Some(request)) => {
                        self.cursor = (slot + 1) % #count;
                        return ::core::task::Poll::Ready(::core::option::Option::Some(request));
                    }
                    ::core::task::Poll::Ready(::core::option::Option::None) => ended += 1,
                    ::core::task::Poll::Pending => {}
                }
            }
            if ended == #count {
                ::core::task::Poll::Ready(::core::option::Option::None)
            } else {
                ::core::task::Poll::Pending
            }
        }
    }
}

/// `KvServer::dispatch`: one arm per method.
fn dispatch_arms(names: &Names, methods: &[Method]) -> Vec<TokenStream> {
    let Names {
        rpc,
        request: request_enum,
        ..
    } = names;
    methods
        .iter()
        .map(|method| {
            let Method {
                ident: method_ident,
                variant,
                ..
            } = method;
            quote! {
                #request_enum::#variant(#rpc::IncomingRequest { request, reply }) => {
                    let answer = service.#method_ident(request).await;
                    // A one-way request carries no route: nothing is sent.
                    if reply.expects_reply() {
                        let _ = reply.send(&answer);
                    }
                }
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use proc_macro2::TokenStream;
    use quote::quote;

    use super::expand;

    fn error(attr: TokenStream, item: TokenStream) -> String {
        match expand(attr, item) {
            Ok(tokens) => panic!("expected an error, got {tokens}"),
            Err(error) => error.to_string(),
        }
    }

    #[test]
    fn a_valid_service_expands_to_parseable_items() {
        let tokens = expand(
            quote!(id = 0x6b76, version = 1),
            quote! {
                /// A store.
                pub trait Kv {
                    /// Read.
                    #[method(id = 1, schema = 1)]
                    async fn get(&self, request: GetRequest) -> GetReply;
                    #[method(id = 2, schema = 3)]
                    async fn put_many(&self, request: PutRequest);
                }
            },
        )
        .expect("expands");
        let file: syn::File = syn::parse2(tokens).expect("valid items");
        let names: Vec<String> = file
            .items
            .iter()
            .filter_map(|item| match item {
                syn::Item::Struct(item) => Some(item.ident.to_string()),
                syn::Item::Enum(item) => Some(item.ident.to_string()),
                syn::Item::Type(item) => Some(item.ident.to_string()),
                syn::Item::Trait(item) => Some(item.ident.to_string()),
                _ => None,
            })
            .collect();
        for expected in [
            "Kv",
            "KvInterface",
            "KvGet",
            "KvPutMany",
            "KvRef",
            "KvClient",
            "KvRequest",
            "KvServer",
        ] {
            assert!(
                names.iter().any(|name| name == expected),
                "{expected} in {names:?}"
            );
        }
    }

    #[test]
    fn ids_are_required_and_explicit() {
        let item = quote! {
            trait Kv {
                #[method(id = 1, schema = 1)]
                async fn get(&self, request: u64) -> u64;
            }
        };
        assert!(error(quote!(version = 1), item.clone()).contains("explicit `id"));
        assert!(error(quote!(id = 1), item.clone()).contains("explicit `version"));
        assert!(
            error(quote!(id = 1, version = 1, name = 2), item).contains("unknown service argument")
        );
        let missing = error(
            quote!(id = 1, version = 1),
            quote! {
                trait Kv {
                    async fn get(&self, request: u64) -> u64;
                }
            },
        );
        assert!(missing.contains("#[method("), "{missing}");
        let no_schema = error(
            quote!(id = 1, version = 1),
            quote! {
                trait Kv {
                    #[method(id = 1)]
                    async fn get(&self, request: u64) -> u64;
                }
            },
        );
        assert!(no_schema.contains("explicit `schema"), "{no_schema}");
    }

    #[test]
    fn duplicate_method_ids_are_refused() {
        let message = error(
            quote!(id = 1, version = 1),
            quote! {
                trait Kv {
                    #[method(id = 7, schema = 1)]
                    async fn get(&self, request: u64) -> u64;
                    #[method(id = 7, schema = 1)]
                    async fn put(&self, request: u64) -> u64;
                }
            },
        );
        assert!(message.contains("already used by `get`"), "{message}");
    }

    #[test]
    fn signatures_outside_the_contract_are_refused() {
        let cases = [
            (
                quote!(
                    trait Kv {
                        #[method(id = 1, schema = 1)]
                        fn get(&self, r: u64) -> u64;
                    }
                ),
                "async fn",
            ),
            (
                quote!(
                    trait Kv {
                        #[method(id = 1, schema = 1)]
                        async fn get<T>(&self, r: T) -> u64;
                    }
                ),
                "generic",
            ),
            (
                quote!(
                    trait Kv {
                        #[method(id = 1, schema = 1)]
                        async fn get(&mut self, r: u64) -> u64;
                    }
                ),
                "&self",
            ),
            (
                quote!(
                    trait Kv {
                        #[method(id = 1, schema = 1)]
                        async fn get(&self, a: u64, b: u64) -> u64;
                    }
                ),
                "exactly one",
            ),
            (
                quote!(
                    trait Kv {
                        #[method(id = 1, schema = 1)]
                        async fn get(&self, r: u64) -> u64 {
                            r
                        }
                    }
                ),
                "default body",
            ),
            (
                quote!(
                    trait Kv<T> {}
                ),
                "generic",
            ),
            (
                quote!(
                    trait Kv: Clone {}
                ),
                "supertraits",
            ),
            (
                quote!(
                    trait Kv {
                        const X: u32;
                    }
                ),
                "only `async fn`",
            ),
            (
                quote!(
                    trait Kv {}
                ),
                "at least one method",
            ),
            (
                quote!(
                    trait Kv {
                        #[method(id = 1, schema = 1)]
                        async fn client(&self, r: u64) -> u64;
                    }
                ),
                "collide",
            ),
            (
                quote!(
                    trait Kv {
                        #[inline]
                        #[method(id = 1, schema = 1)]
                        async fn get(&self, r: u64) -> u64;
                    }
                ),
                "only doc comments",
            ),
        ];
        for (item, expected) in cases {
            let message = error(quote!(id = 1, version = 1), item);
            assert!(message.contains(expected), "{expected}: {message}");
        }
    }

    #[test]
    fn method_names_become_camel_case_markers() {
        assert_eq!(super::camel("put_many"), "PutMany");
        assert_eq!(super::camel("get"), "Get");
        assert_eq!(super::camel("a__b"), "AB");
    }
}
