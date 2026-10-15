//! Hooks and declarative policies use the ordinary Tool lifecycle.

use super::{Options, bool_literal, callback, timeout_duration};
use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::{Expr, ExprLit, Lit};

pub(super) fn expand(
    options: &Options,
    core: &TokenStream,
) -> syn::Result<(Vec<TokenStream>, Vec<TokenStream>)> {
    let mut configure = Vec::new();
    let mut hooks = Vec::new();
    for (policy, method, dynamic, enabled) in [
        (
            &options.enabled,
            "with_availability",
            quote!(#core::tool::ToolAvailability::Dynamic),
            true,
        ),
        (
            &options.approval,
            "with_approval",
            quote!(#core::tool::ToolApprovalPolicy::Dynamic),
            false,
        ),
    ] {
        let Some(policy) = policy else { continue };
        let method = format_ident!("{method}");
        if let Ok(value) = bool_literal(policy) {
            let value = match (enabled, value) {
                (true, true) => quote!(#core::tool::ToolAvailability::Enabled),
                (true, false) => quote!(#core::tool::ToolAvailability::Disabled),
                (false, true) => quote!(#core::tool::ToolApprovalPolicy::Always),
                (false, false) => quote!(#core::tool::ToolApprovalPolicy::Never),
            };
            configure.push(quote!(__ra_options = __ra_options.#method(#value);));
        } else {
            let call = callback(policy, &quote!(__ra_context))?;
            configure.push(quote!(__ra_options = __ra_options.#method(#dynamic);));
            if enabled {
                hooks.push(quote! {
                    async fn is_enabled(&self, __ra_context: &#core::context::RunContext) -> #core::error::Result<bool> {
                        match self.options.availability() {
                            #core::tool::ToolAvailability::Enabled => ::core::result::Result::Ok(true),
                            #core::tool::ToolAvailability::Disabled => ::core::result::Result::Ok(false),
                            _ => #call,
                        }
                    }
                });
            } else {
                hooks.push(quote! {
                    async fn needs_approval(&self, __ra_context: &#core::tool::ToolContext<'_>) -> #core::error::Result<bool> {
                        match self.options.approval() {
                            #core::tool::ToolApprovalPolicy::Never => ::core::result::Result::Ok(false),
                            #core::tool::ToolApprovalPolicy::Always => ::core::result::Result::Ok(true),
                            _ => #call,
                        }
                    }
                });
            }
        }
    }
    if let Some(failure) = &options.failure {
        if matches!(failure, Expr::Path(path) if path.path.is_ident("None")) {
            configure.push(quote!(__ra_options = __ra_options.with_failure_handling(#core::tool::ToolFailureHandling::Propagate);));
        } else {
            let call = callback(failure, &quote!(__ra_context, __ra_error))?;
            configure.push(quote!(__ra_options = __ra_options.with_failure_handling(#core::tool::ToolFailureHandling::Custom);));
            hooks.push(quote! {
                async fn handle_failure(&self, __ra_context: &#core::tool::ToolContext<'_>, __ra_error: &#core::error::Error) -> #core::error::Result<::core::option::Option<#core::tool::ToolOutput>> {
                    #call
                }
            });
        }
    } else {
        hooks.push(quote! {
            fn uses_default_failure_handler(&self) -> bool { true }
            async fn handle_failure(&self, __ra_context: &#core::tool::ToolContext<'_>, _error: &#core::error::Error) -> #core::error::Result<::core::option::Option<#core::tool::ToolOutput>> {
                if self.output_schema.is_some() && __ra_context.caller() == #core::tool::ToolCaller::Programmatic {
                    return ::core::result::Result::Ok(::core::option::Option::None);
                }
                ::core::result::Result::Ok(::core::option::Option::Some(#core::tool::ToolOutput::text("An error occurred while running the tool. Please try again.")))
            }
        });
    }
    if let Some(timeout) = &options.timeout_error
        && !matches!(timeout, Expr::Path(path) if path.path.is_ident("None"))
    {
        let call = callback(timeout, &quote!(__ra_context, __ra_error))?;
        hooks.push(quote! {
            async fn handle_timeout(&self, __ra_context: &#core::tool::ToolContext<'_>, __ra_error: &#core::tool::ToolTimeoutError) -> #core::error::Result<::core::option::Option<#core::tool::ToolOutput>> {
                #core::tool::__private::function_output(#call?).map(::core::option::Option::Some)
            }
        });
    }
    if let Some(extractor) = &options.custom_data
        && !matches!(extractor, Expr::Path(path) if path.path.is_ident("None"))
    {
        let call = callback(extractor, &quote!(__ra_context))?;
        hooks.push(quote! {
            async fn extract_custom_data(&self, __ra_context: &mut #core::tool::FunctionToolCustomDataContext<'_>) -> #core::error::Result<::core::option::Option<#core::tool::__private::serde_json::Map<::std::string::String, #core::tool::__private::serde_json::Value>>> { #call }
        });
    }
    configure.extend(execution(options, core)?);
    Ok((configure, hooks))
}

/// Applies the `options` adjustment to the defaults and guards synchronous timeouts.
///
/// The guard runs on every construction, so an adjustment or a replacement option set cannot
/// attach a timeout that would release the execution gate while a blocking body keeps running.
pub(super) fn construction(
    options: &Options,
    core: &TokenStream,
    synchronous: bool,
) -> (TokenStream, TokenStream) {
    let defaults = function_options(core);
    let adjust = options.execution.as_ref().map(|adjustment| {
        quote! {
            fn __ra_adjust(
                options: #core::tool::ToolOptions,
                adjustment: impl ::core::ops::FnOnce(#core::tool::ToolOptions) -> #core::tool::ToolOptions,
            ) -> #core::tool::ToolOptions {
                adjustment(options)
            }
            __ra_options = __ra_adjust(__ra_options, #adjustment);
        }
    });
    let synchronous_timeout = synchronous.then(|| {
        quote! {
            return ::core::result::Result::Err(#core::error::Error::config(
                "function tool timeouts are only supported for async functions",
            ));
        }
    });
    (
        quote! {
            let mut __ra_options = #defaults;
            #adjust
        },
        quote! {
            fn __ra_validate_options(options: &#core::tool::ToolOptions) -> #core::error::Result<()> {
                options.validate()?;
                if let ::core::option::Option::Some(timeout) = options.timeout() {
                    if timeout.is_zero() {
                        return ::core::result::Result::Err(#core::error::Error::config(
                            "function tool timeout must be greater than zero",
                        ));
                    }
                    #synchronous_timeout
                }
                ::core::result::Result::Ok(())
            }
        },
    )
}

/// Upstream function-tool defaults: sanitized failures, parallel eligibility, and no product
/// no-progress breaker.
fn function_options(core: &TokenStream) -> TokenStream {
    quote!(#core::tool::ToolOptions::new()
        .with_failure_handling(#core::tool::ToolFailureHandling::Custom)
        .with_concurrency(#core::tool::ToolConcurrency::Parallel)
        .without_no_progress_limit())
}

/// Upstream's direct execution keywords, applied over the function-tool defaults.
fn execution(options: &Options, core: &TokenStream) -> syn::Result<Vec<TokenStream>> {
    let mut configure = Vec::new();
    if let Some(timeout) = &options.timeout {
        let duration = if let Some(duration) = timeout_duration(timeout)? {
            let seconds = duration.as_secs();
            let nanos = duration.subsec_nanos();
            quote!(::core::time::Duration::new(#seconds, #nanos))
        } else {
            quote!(#timeout)
        };
        configure.push(quote!(__ra_options = __ra_options.with_timeout(#duration);));
    }
    if let Some(behavior) = &options.timeout_behavior {
        let behavior = match string(behavior).as_deref() {
            Some("error_as_result") => quote!(#core::tool::ToolTimeoutBehavior::ModelVisible),
            Some(_) => quote!(#core::tool::ToolTimeoutBehavior::Propagate),
            None => quote!(#behavior),
        };
        configure.push(quote!(__ra_options = __ra_options.with_timeout_behavior(#behavior);));
    }
    for (guardrails, method) in [
        (&options.input_guardrails, "with_input_guardrail"),
        (&options.output_guardrails, "with_output_guardrail"),
    ] {
        let method = format_ident!("{method}");
        for guardrail in guardrails.iter().flatten() {
            let guardrail = if string(guardrail).is_some() {
                quote!(#core::tool::ToolGuardrailId::new(#guardrail)?)
            } else {
                quote!(#guardrail)
            };
            configure.push(quote!(__ra_options = __ra_options.#method(#guardrail);));
        }
    }
    if let Some(deferred) = options.defer_loading {
        let exposure = if deferred {
            quote!(#core::tool::ToolExposure::Deferred)
        } else {
            quote!(#core::tool::ToolExposure::Advertised)
        };
        configure.push(quote!(__ra_options = __ra_options.with_exposure(#exposure);));
    }
    if let Some(callers) = &options.allowed_callers {
        let count = callers.len();
        let callers = callers
            .iter()
            .map(|caller| match string(caller).as_deref() {
                Some("direct") => quote!(#core::tool::ToolCaller::Direct),
                Some(_) => quote!(#core::tool::ToolCaller::Programmatic),
                None => quote!(#caller),
            });
        configure.push(quote! {
            let __ra_callers: [#core::tool::ToolCaller; #count] = [#(#callers),*];
            let mut __ra_seen_callers = ::std::collections::BTreeSet::new();
            for caller in &__ra_callers {
                if !__ra_seen_callers.insert(*caller) {
                    return ::core::result::Result::Err(#core::error::Error::config(
                        "allowed_callers contains a duplicate caller",
                    ));
                }
            }
            __ra_options = __ra_options.with_allowed_callers(__ra_callers);
        });
    }
    Ok(configure)
}

fn string(expression: &Expr) -> Option<String> {
    match expression {
        Expr::Lit(ExprLit {
            lit: Lit::Str(value),
            ..
        }) => Some(value.value()),
        _ => None,
    }
}
