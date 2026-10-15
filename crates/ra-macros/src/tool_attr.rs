//! Function declarations expanded into the existing tool contract.

use std::{collections::BTreeSet, time::Duration};

use proc_macro2::{Span, TokenStream};
use quote::{format_ident, quote};
use syn::{
    Expr, ExprLit, ItemFn, Lit, LitStr, Meta, ReturnType, Type, parse::Parser,
    punctuated::Punctuated,
};

use crate::tool_input::{documentation, ra_core_path};

mod output;
mod parameters;
mod policies;

#[derive(Default)]
struct Options {
    name: Option<LitStr>,
    description: Option<LitStr>,
    strict: Option<bool>,
    use_docs: Option<bool>,
    enabled: Option<Expr>,
    approval: Option<Expr>,
    failure: Option<Expr>,
    execution: Option<Expr>,
    timeout: Option<Expr>,
    timeout_behavior: Option<Expr>,
    input_guardrails: Option<Vec<Expr>>,
    output_guardrails: Option<Vec<Expr>>,
    defer_loading: Option<bool>,
    allowed_callers: Option<Vec<Expr>>,
    timeout_error: Option<Expr>,
    custom_data: Option<Expr>,
    output_type: Option<Type>,
    output_schema: Option<Expr>,
}

impl Options {
    fn parse(tokens: TokenStream) -> syn::Result<Self> {
        let mut options = Self::default();
        let mut seen = BTreeSet::new();
        for meta in Punctuated::<Meta, syn::Token![,]>::parse_terminated.parse2(tokens)? {
            let Meta::NameValue(meta) = meta else {
                return Err(syn::Error::new_spanned(meta, "expected `option = value`"));
            };
            let key = meta
                .path
                .get_ident()
                .map(ToString::to_string)
                .unwrap_or_default();
            if !seen.insert(key.clone()) {
                return Err(syn::Error::new_spanned(
                    meta,
                    format!("duplicate `{key}` option"),
                ));
            }
            match key.as_str() {
                "name" => options.name = Some(string_literal(&meta.value)?),
                "description" => options.description = Some(string_literal(&meta.value)?),
                "strict" => options.strict = Some(bool_literal(&meta.value)?),
                "use_docstring_info" => options.use_docs = Some(bool_literal(&meta.value)?),
                "is_enabled" => options.enabled = Some(policy(&meta.value)?),
                "needs_approval" => options.approval = Some(policy(&meta.value)?),
                "failure_error_function" => {
                    callback(&meta.value, &TokenStream::new())?;
                    options.failure = Some(meta.value);
                }
                "options" => options.execution = Some(adjustment(meta.value)?),
                "timeout" => options.timeout = Some(timeout(meta.value)?),
                "timeout_behavior" => {
                    options.timeout_behavior = Some(timeout_behavior(meta.value)?);
                }
                "tool_input_guardrails" => {
                    options.input_guardrails = Some(list(meta.value)?);
                }
                "tool_output_guardrails" => {
                    options.output_guardrails = Some(list(meta.value)?);
                }
                "defer_loading" => options.defer_loading = Some(bool_literal(&meta.value)?),
                "allowed_callers" => options.allowed_callers = Some(callers(meta.value)?),
                "timeout_error_function" | "custom_data_extractor" => {
                    if !matches!(&meta.value, Expr::Path(path) if path.path.is_ident("None")) {
                        callback(&meta.value, &TokenStream::new())?;
                    }
                    if key == "timeout_error_function" {
                        options.timeout_error = Some(meta.value);
                    } else {
                        options.custom_data = Some(meta.value);
                    }
                }
                "output_type" => {
                    let value = meta.value;
                    if !matches!(&value, Expr::Path(path) if path.path.is_ident("None")) {
                        options.output_type = Some(syn::parse2(quote!(#value))?);
                    }
                }
                "output_json_schema" => {
                    if !matches!(&meta.value, Expr::Path(path) if path.path.is_ident("None")) {
                        options.output_schema = Some(meta.value);
                    }
                }
                "docstring_style" => {
                    return Err(syn::Error::new_spanned(
                        meta,
                        "Rust uses doc comments and parameter `#[tool(description = \"...\")]`, not Python docstring styles",
                    ));
                }
                _ => {
                    return Err(syn::Error::new_spanned(
                        meta,
                        "unsupported tool option; expected name, description, strict, use_docstring_info, is_enabled, needs_approval, failure_error_function, timeout, timeout_behavior, timeout_error_function, custom_data_extractor, output_type, output_json_schema, tool_input_guardrails, tool_output_guardrails, defer_loading, allowed_callers, or options",
                    ));
                }
            }
        }
        if options.output_type.is_some() && options.output_schema.is_some() {
            return Err(syn::Error::new_spanned(
                options.output_schema.as_ref(),
                "output_type and output_json_schema cannot both be provided.",
            ));
        }
        Ok(options)
    }
}

fn string_literal(expression: &Expr) -> syn::Result<LitStr> {
    if let Expr::Lit(ExprLit {
        lit: Lit::Str(value),
        ..
    }) = expression
    {
        return Ok(value.clone());
    }
    Err(syn::Error::new_spanned(
        expression,
        "expected a string literal",
    ))
}

fn bool_literal(expression: &Expr) -> syn::Result<bool> {
    if let Expr::Lit(ExprLit {
        lit: Lit::Bool(value),
        ..
    }) = expression
    {
        return Ok(value.value);
    }
    Err(syn::Error::new_spanned(
        expression,
        "expected `true` or `false`",
    ))
}

/// Accepts the Python timeout in seconds or a `Duration` expression.
///
/// Literal seconds follow upstream's positive, finite rule at compile time.
fn timeout(expression: Expr) -> syn::Result<Expr> {
    timeout_duration(&expression)?;
    Ok(expression)
}

/// Checks literal seconds before code generation, including Rust's duration range and precision.
fn timeout_duration(expression: &Expr) -> syn::Result<Option<Duration>> {
    let seconds = match expression {
        Expr::Lit(ExprLit {
            lit: Lit::Int(value),
            ..
        }) => value.base10_parse::<f64>()?,
        Expr::Lit(ExprLit {
            lit: Lit::Float(value),
            ..
        }) => value.base10_parse::<f64>()?,
        Expr::Lit(_) => {
            return Err(syn::Error::new_spanned(
                expression,
                "expected timeout seconds or a `Duration` expression",
            ));
        }
        // A negative literal parses as negation, not a literal; report upstream's range rule
        // instead of a type mismatch against `Duration`.
        Expr::Unary(syn::ExprUnary {
            op: syn::UnOp::Neg(_),
            expr,
            ..
        }) if matches!(
            &**expr,
            Expr::Lit(ExprLit {
                lit: Lit::Int(_) | Lit::Float(_),
                ..
            })
        ) =>
        {
            return Err(syn::Error::new_spanned(
                expression,
                "function tool timeout must be a finite number of seconds greater than 0",
            ));
        }
        _ => return Ok(None),
    };
    if !seconds.is_finite() || seconds <= 0.0 {
        return Err(syn::Error::new_spanned(
            expression,
            "function tool timeout must be a finite number of seconds greater than 0",
        ));
    }
    let duration = Duration::try_from_secs_f64(seconds).map_err(|error| {
        syn::Error::new_spanned(
            expression,
            format!("function tool timeout cannot be represented as a Rust Duration: {error}"),
        )
    })?;
    if duration.is_zero() {
        return Err(syn::Error::new_spanned(
            expression,
            "function tool timeout is too small to represent as a nonzero Rust Duration",
        ));
    }
    Ok(Some(duration))
}

fn timeout_behavior(expression: Expr) -> syn::Result<Expr> {
    if let Expr::Lit(ExprLit {
        lit: Lit::Str(value),
        ..
    }) = &expression
        && !matches!(
            value.value().as_str(),
            "error_as_result" | "raise_exception"
        )
    {
        return Err(syn::Error::new_spanned(
            expression,
            "timeout_behavior must be \"error_as_result\", \"raise_exception\", or a ToolTimeoutBehavior expression",
        ));
    }
    Ok(expression)
}

fn list(expression: Expr) -> syn::Result<Vec<Expr>> {
    let Expr::Array(array) = expression else {
        return Err(syn::Error::new_spanned(
            expression,
            "expected a list such as `[\"id\"]`",
        ));
    };
    Ok(array.elems.into_iter().collect())
}

/// Ports upstream's nonempty, duplicate-free caller list for literal callers.
fn callers(expression: Expr) -> syn::Result<Vec<Expr>> {
    let span = expression.clone();
    let callers = list(expression)?;
    if callers.is_empty() {
        return Err(syn::Error::new_spanned(
            span,
            "allowed_callers must be a nonempty list containing \"direct\", \"programmatic\", or both",
        ));
    }
    let mut seen = BTreeSet::new();
    for caller in &callers {
        if let Expr::Lit(ExprLit {
            lit: Lit::Str(value),
            ..
        }) = caller
        {
            let value = value.value();
            if !matches!(value.as_str(), "direct" | "programmatic") {
                return Err(syn::Error::new_spanned(
                    caller,
                    "unsupported caller; expected \"direct\" or \"programmatic\"",
                ));
            }
            if !seen.insert(value) {
                return Err(syn::Error::new_spanned(
                    caller,
                    "allowed_callers contains a duplicate caller",
                ));
            }
        }
    }
    Ok(callers)
}

/// The `options` adjustment receives the declared function-tool defaults, so setting one policy
/// cannot silently reset the others.
fn adjustment(expression: Expr) -> syn::Result<Expr> {
    if matches!(expression, Expr::Closure(_) | Expr::Path(_)) {
        return Ok(expression);
    }
    Err(syn::Error::new_spanned(
        expression,
        "`options` takes an adjustment such as `|options| options.with_concurrency(...)`; it receives the function-tool defaults",
    ))
}

fn policy(expression: &Expr) -> syn::Result<Expr> {
    if bool_literal(expression).is_ok() || callback(expression, &TokenStream::new()).is_ok() {
        return Ok(expression.clone());
    }
    Err(syn::Error::new_spanned(
        expression,
        "expected a boolean literal, an async callback path, or sync(callback)",
    ))
}

fn last_type_name(ty: &Type) -> Option<String> {
    match ty {
        Type::Path(path) => path
            .path
            .segments
            .last()
            .map(|segment| segment.ident.to_string()),
        Type::Reference(reference) => last_type_name(&reference.elem),
        _ => None,
    }
}

pub(crate) fn expand(attributes: TokenStream, input: TokenStream) -> syn::Result<TokenStream> {
    let options = Options::parse(attributes)?;
    let mut function = syn::parse2::<ItemFn>(input)?;
    let signature = &function.sig;
    validate_signature(signature, &options)?;
    let synchronous = signature.asyncness.is_none();
    let core = ra_core_path()?;
    let support = quote!(#core::tool::__private);
    let (serde_path, schemars_path) = derive_paths(&support, signature.ident.span());
    let name = &signature.ident;
    let tool_name = options
        .name
        .clone()
        .filter(|name| !name.value().is_empty())
        .unwrap_or_else(|| LitStr::new(name.to_string().trim_start_matches("r#"), name.span()));
    let use_docs = options.use_docs.unwrap_or(true);
    let description = description(&options, &function);
    let strict = options.strict.unwrap_or(true);
    let output_type = return_type(signature);
    let parameters::Parameters {
        fields,
        defaults,
        arguments,
        required,
        inject_context,
    } = parameters::expand(&mut function.sig, use_docs)?;
    let execute = invocation(&function, &arguments, inject_context, &support, &tool_name);
    let implementation = format_ident!(
        "__ra_tool_{}",
        function.sig.ident.to_string().trim_start_matches("r#")
    );
    let (configure, hooks) = policies::expand(&options, &core)?;
    let output_schema = output::construction(&options, &function.sig, &core);
    let (declared, validate_options) = policies::construction(&options, &core, synchronous);
    let cfg = configurations(&function)?;
    let companion = companion(&function, &implementation, &cfg);
    Ok(quote! {
        #[allow(clippy::needless_pass_by_value)]
        #function
        #companion
        #(#cfg)*
        #[doc(hidden)]
        mod #implementation {
            #[allow(unused_imports)]
            use super::*;
            #(#defaults)*
            #[derive(#support::serde::Deserialize, #support::schemars::JsonSchema)]
            #[serde(crate = #serde_path)]
            #[schemars(crate = #schemars_path)]
            struct __RaInput { #(#fields),* }
            impl #core::tool::ToolInput for __RaInput {
                const DESCRIPTION: ::core::option::Option<&'static str> = #description;
                const STRICT_JSON_SCHEMA: bool = #strict;
            }
            struct __RaTool {
                origin: #core::tool::ToolOrigin,
                function: #core::tool::FuncSchema,
                options: #core::tool::ToolOptions,
                output_schema: ::core::option::Option<#core::tool::FunctionToolOutputSchema>,
            }
            #validate_options
            /// Returns the declared policies: function-tool defaults, the `options` adjustment,
            /// then explicit attribute policies.
            pub fn __ra_options() -> #core::error::Result<#core::tool::ToolOptions> {
                #declared
                #(#configure)*
                __ra_validate_options(&__ra_options)?;
                ::core::result::Result::Ok(__ra_options)
            }
            /// Builds the function tool with its declared policies.
            pub fn __ra_tool() -> #core::error::Result<impl #core::tool::Tool> {
                __ra_tool_with_options(__ra_options()?)
            }
            /// Builds one instance with replacement policies; start from the declared options to keep defaults.
            pub fn __ra_tool_with_options(options: #core::tool::ToolOptions) -> #core::error::Result<impl #core::tool::Tool> {
                __ra_validate_options(&options)?;
                let output_schema = #output_schema;
                ::core::result::Result::Ok(__RaTool {
                    origin: #core::tool::ToolOrigin::new(#tool_name)?,
                    function: #core::tool::FuncSchema::for_function_input::<__RaInput>(#tool_name, &[#(#required),*])?
                        .with_tool_context(#inject_context)
                        .with_return_type::<#output_type>(),
                    options,
                    output_schema,
                })
            }
            #[#support::async_trait]
            impl #core::tool::Tool for __RaTool {
                fn origin(&self) -> &#core::tool::ToolOrigin { &self.origin }
                fn schema(&self) -> &#core::tool::ToolSchema { self.function.tool_schema() }
                fn func_schema(&self) -> ::core::option::Option<&#core::tool::FuncSchema> { ::core::option::Option::Some(&self.function) }
                fn options(&self) -> #core::tool::ToolOptions { self.options.clone() }
                fn output_schema(&self) -> ::core::option::Option<&#core::tool::FunctionToolOutputSchema> { self.output_schema.as_ref() }
                async fn call(&self, mut __ra_context: #core::tool::ToolContext<'_>) -> #core::error::Result<#core::tool::ToolOutput> {
                    let __ra_input = match __ra_context.take_decoded_input::<__RaInput>()? {
                        ::core::option::Option::Some(input) => input,
                        ::core::option::Option::None => self.function.decode_value(__ra_context.arguments().clone())?.downcast::<__RaInput>()?,
                    };
                    #execute
                }
                #(#hooks)*
            }
        }
    })
}

fn invocation(
    function: &ItemFn,
    arguments: &[TokenStream],
    inject_context: bool,
    support: &TokenStream,
    tool_name: &LitStr,
) -> TokenStream {
    let async_function = function.sig.asyncness.is_some();
    let fallible = matches!(&function.sig.output, ReturnType::Type(_, ty) if last_type_name(ty).as_deref() == Some("Result"));
    let name = &function.sig.ident;
    let invoke = quote!(super::#name(#(#arguments),*));
    let invoke = if async_function {
        quote!(#invoke.await)
    } else {
        invoke
    };
    let invoke = if fallible {
        quote!(let __ra_output = #invoke.map_err(|error| #support::function_error(#tool_name, error))?;)
    } else {
        quote!(let __ra_output = #invoke;)
    };
    let body = quote! {
        #invoke
        #support::function_output_with_schema(__ra_output, __ra_output_schema.as_ref(), #tool_name)
    };
    if async_function {
        quote! { let __ra_output_schema = self.output_schema.clone(); #body }
    } else {
        let context = if inject_context {
            quote!(let mut __ra_owned_context = __ra_context.into_owned();)
        } else {
            quote!()
        };
        let borrow = if inject_context {
            quote!(let __ra_context = __ra_owned_context.as_context();)
        } else {
            quote!()
        };
        quote! {
            let __ra_output_schema = self.output_schema.clone();
            #context
            #support::tokio::task::spawn_blocking(move || {
                #borrow
                #body
            }).await.map_err(|error| #support::function_error(#tool_name, error))?
        }
    }
}

fn validate_signature(signature: &syn::Signature, options: &Options) -> syn::Result<()> {
    if signature.asyncness.is_none()
        && let Some(timeout) = &options.timeout
    {
        return Err(syn::Error::new_spanned(
            timeout,
            "timeout is only supported for async function tools, as upstream requires; a timeout cannot stop a running synchronous body",
        ));
    }
    if signature.unsafety.is_some()
        || signature.variadic.is_some()
        || !signature.generics.params.is_empty()
        || signature.generics.where_clause.is_some()
    {
        return Err(syn::Error::new_spanned(
            signature,
            "tool functions must be safe free functions with concrete argument types",
        ));
    }
    Ok(())
}

pub(super) fn callback(expression: &Expr, arguments: &TokenStream) -> syn::Result<TokenStream> {
    if let Expr::Path(path) = expression {
        return Ok(quote!(#path(#arguments).await));
    }
    if let Expr::Call(call) = expression
        && matches!(&*call.func, Expr::Path(path) if path.path.is_ident("sync"))
        && call.args.len() == 1
        && let Some(Expr::Path(path)) = call.args.first()
    {
        return Ok(quote!(#path(#arguments)));
    }
    Err(syn::Error::new_spanned(
        expression,
        "expected an async callback path or sync(callback)",
    ))
}

fn description(options: &Options, function: &ItemFn) -> TokenStream {
    let use_docs = options.use_docs.unwrap_or(true);
    let name = &function.sig.ident;
    let description = options
        .description
        .clone()
        .filter(|description| !description.value().is_empty())
        .or_else(|| {
            use_docs
                .then(|| documentation(&function.attrs))
                .flatten()
                .map(|text| LitStr::new(&text, name.span()))
        });
    description.map_or_else(
        || quote!(::core::option::Option::Some("")),
        |description| quote!(::core::option::Option::Some(#description)),
    )
}

/// Carries the function's `cfg` gates to its companion module.
fn configurations(function: &ItemFn) -> syn::Result<Vec<TokenStream>> {
    function
        .attrs
        .iter()
        .filter_map(|attribute| configuration(&attribute.meta).transpose())
        .map(|meta| meta.map(|meta| quote!(#[#meta])))
        .collect()
}

fn configuration(meta: &Meta) -> syn::Result<Option<Meta>> {
    if meta.path().is_ident("cfg") {
        return Ok(Some(meta.clone()));
    }
    if let Meta::List(list) = meta
        && list.path.is_ident("cfg_attr")
    {
        let nested = list.parse_args_with(Punctuated::<Meta, syn::Token![,]>::parse_terminated)?;
        let mut nested = nested.iter();
        let Some(condition) = nested.next() else {
            return Ok(None);
        };
        let attributes = nested
            .filter_map(|meta| configuration(meta).transpose())
            .collect::<syn::Result<Vec<_>>>()?;
        if !attributes.is_empty() {
            return Ok(Some(
                syn::parse_quote!(cfg_attr(#condition, #(#attributes),*)),
            ));
        }
    }
    Ok(None)
}

fn return_type(signature: &syn::Signature) -> TokenStream {
    match &signature.output {
        ReturnType::Default => quote!(()),
        ReturnType::Type(_, ty) => quote!(#ty),
    }
}

/// Keeps public constructor names out of the scope that resolves application expressions.
fn companion(function: &ItemFn, implementation: &syn::Ident, cfg: &[TokenStream]) -> TokenStream {
    let name = &function.sig.ident;
    let vis = &function.vis;
    quote! {
        #(#cfg)*
        #[doc = "Generated function-tool constructors. The original function remains callable."]
        #vis mod #name {
            #[doc(inline)]
            pub use super::#implementation::{
                __ra_options as options,
                __ra_tool as tool,
                __ra_tool_with_options as tool_with_options,
            };
        }
    }
}

fn derive_paths(support: &TokenStream, span: Span) -> (LitStr, LitStr) {
    let path =
        |dependency: &str| LitStr::new(&format!("{support}::{dependency}").replace(' ', ""), span);
    (path("serde"), path("schemars"))
}
