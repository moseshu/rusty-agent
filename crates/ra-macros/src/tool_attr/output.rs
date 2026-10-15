//! Resolves explicit output declarations and programmatic return-type inference.

use proc_macro2::TokenStream;
use quote::quote;
use syn::{Expr, GenericArgument, PathArguments, ReturnType, Type};

use super::{Options, last_type_name};

pub(super) fn construction(
    options: &Options,
    signature: &syn::Signature,
    core: &TokenStream,
) -> TokenStream {
    if let Some(ty) = &options.output_type {
        return quote!(::core::option::Option::Some(#core::tool::FunctionToolOutputSchema::for_type::<#ty>()?));
    }
    if let Some(schema) = &options.output_schema {
        return quote!(::core::option::Option::Some(#core::tool::FunctionToolOutputSchema::json_schema(#schema)?));
    }
    let ReturnType::Type(_, ty) = &signature.output else {
        return quote!(::core::option::Option::None);
    };
    let ty = success_type(ty);
    let plain = matches!(
        last_type_name(ty).as_deref(),
        Some("String" | "str" | "Value" | "ToolOutput")
    ) || matches!(ty, Type::Tuple(tuple) if tuple.elems.is_empty());
    if plain {
        return quote!(::core::option::Option::None);
    }
    let infer = options.allowed_callers.as_ref().is_some_and(|callers| callers.iter().any(|caller| {
        !matches!(caller, Expr::Lit(literal) if matches!(&literal.lit, syn::Lit::Str(text) if text.value() == "direct"))
            && !matches!(caller, Expr::Path(path) if path.path.segments.last().is_some_and(|segment| segment.ident == "Direct"))
    }));
    if !infer {
        return quote!(::core::option::Option::None);
    }
    quote! {
        if options.allowed_callers().is_some_and(|callers| callers.contains(&#core::tool::ToolCaller::Programmatic)) {
            ::core::option::Option::Some(#core::tool::FunctionToolOutputSchema::for_type::<#ty>()
                .map_err(|error| #core::error::Error::config("A programmatic function tool return annotation must define a strict JSON object schema.").with_source(error))?)
        } else { ::core::option::Option::None }
    }
}

fn success_type(ty: &Type) -> &Type {
    if let Type::Path(path) = ty
        && let Some(segment) = path.path.segments.last()
        && segment.ident == "Result"
        && let PathArguments::AngleBracketed(arguments) = &segment.arguments
        && let Some(GenericArgument::Type(success)) = arguments.args.first()
    {
        return success;
    }
    ty
}
