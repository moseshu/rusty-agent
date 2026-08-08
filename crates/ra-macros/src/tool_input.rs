//! `#[derive(ToolInput)]`: type docs and strictness metadata.

use proc_macro_crate::{FoundCrate, crate_name};
use proc_macro2::{Span, TokenStream};
use quote::{format_ident, quote};
use syn::{Data, DeriveInput, Expr, ExprLit, Lit, LitStr, parse_quote};

use crate::strict::ToolInputOptions;

pub(crate) fn expand(input: TokenStream) -> syn::Result<TokenStream> {
    let input = syn::parse2::<DeriveInput>(input)?;
    if !matches!(input.data, Data::Struct(_)) {
        return Err(syn::Error::new_spanned(
            &input.ident,
            "ToolInput can only be derived for structs",
        ));
    }

    let options = ToolInputOptions::parse(&input.attrs)?;
    let description = options
        .description
        .or_else(|| documentation(&input.attrs).map(|text| LitStr::new(&text, input.ident.span())));
    let description = if let Some(description) = description {
        quote!(::core::option::Option::Some(#description))
    } else {
        quote!(::core::option::Option::None)
    };
    let strict = options.strict;
    let ra_core = ra_core_path()?;

    let ident = &input.ident;
    let mut generics = input.generics.clone();
    let (_, type_generics, _) = input.generics.split_for_impl();
    let self_type = quote!(#ident #type_generics);
    generics.make_where_clause().predicates.push(parse_quote!(
        #self_type: #ra_core::tool::schema::ToolInputRequirements
    ));
    let (impl_generics, _, where_clause) = generics.split_for_impl();

    Ok(quote! {
        impl #impl_generics #ra_core::tool::ToolInput for #ident #type_generics #where_clause {
            const DESCRIPTION: ::core::option::Option<&'static str> = #description;
            const STRICT_JSON_SCHEMA: bool = #strict;
        }
    })
}

fn ra_core_path() -> syn::Result<TokenStream> {
    match crate_name("ra-core") {
        Ok(FoundCrate::Itself) => Ok(quote!(crate)),
        Ok(FoundCrate::Name(name)) => {
            let ident = format_ident!("{name}");
            Ok(quote!(::#ident))
        }
        Err(error) => Err(syn::Error::new(
            Span::call_site(),
            format!("ToolInput derive could not find the `ra-core` dependency: {error}"),
        )),
    }
}

fn documentation(attributes: &[syn::Attribute]) -> Option<String> {
    let mut lines = Vec::new();
    for attribute in attributes
        .iter()
        .filter(|attribute| attribute.path().is_ident("doc"))
    {
        let syn::Meta::NameValue(name_value) = &attribute.meta else {
            continue;
        };
        let Expr::Lit(ExprLit {
            lit: Lit::Str(text),
            ..
        }) = &name_value.value
        else {
            continue;
        };
        lines.push(
            text.value()
                .strip_prefix(' ')
                .unwrap_or(&text.value())
                .to_owned(),
        );
    }

    while lines.first().is_some_and(String::is_empty) {
        lines.remove(0);
    }
    while lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    (!lines.is_empty()).then(|| lines.join("\n"))
}
