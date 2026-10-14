//! Input fields and context injection for function tools.

use super::{documentation, last_type_name};
use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::{Expr, FnArg, LitStr, Pat, Signature, Type};

#[derive(Default)]
pub(super) struct Parameters {
    pub(super) fields: Vec<TokenStream>,
    pub(super) defaults: Vec<TokenStream>,
    pub(super) arguments: Vec<TokenStream>,
    pub(super) required: Vec<LitStr>,
    pub(super) inject_context: bool,
}

pub(super) fn expand(signature: &mut Signature, use_docs: bool) -> syn::Result<Parameters> {
    let mut expansion = Parameters::default();
    let Parameters {
        fields,
        defaults,
        arguments,
        required,
        inject_context,
    } = &mut expansion;
    for (index, argument) in signature.inputs.iter_mut().enumerate() {
        let FnArg::Typed(argument) = argument else {
            return Err(syn::Error::new_spanned(
                argument,
                "tool functions cannot have a `self` receiver",
            ));
        };
        let Pat::Ident(pattern) = &*argument.pat else {
            return Err(syn::Error::new_spanned(
                &argument.pat,
                "tool arguments must use named bindings, not destructuring patterns",
            ));
        };
        if pattern.by_ref.is_some() || pattern.subpat.is_some() {
            return Err(syn::Error::new_spanned(
                pattern,
                "tool arguments must use plain named bindings",
            ));
        }
        let field = &pattern.ident;
        let ty = &argument.ty;
        let context_type = last_type_name(ty);
        if matches!(context_type.as_deref(), Some("ToolContext" | "RunContext")) {
            if index != 0 {
                return Err(syn::Error::new_spanned(
                    argument,
                    "ToolContext/RunContext must be the first parameter",
                ));
            }
            if argument
                .attrs
                .iter()
                .any(|attribute| !attribute.path().is_ident("doc"))
            {
                return Err(syn::Error::new_spanned(
                    argument,
                    "injected context does not accept model parameter metadata",
                ));
            }
            *inject_context = true;
            arguments.push(match (context_type.as_deref(), &**ty) {
                (Some("RunContext"), Type::Reference(reference))
                    if reference.mutability.is_none() =>
                {
                    quote!(__ra_context.run())
                }
                (Some("ToolContext"), Type::Reference(reference))
                    if reference.mutability.is_none() =>
                {
                    quote!(&__ra_context)
                }
                (Some("ToolContext"), Type::Path(_)) => quote!(__ra_context),
                _ => {
                    return Err(syn::Error::new_spanned(
                        ty,
                        "inject ToolContext<'_>, &ToolContext<'_>, or &RunContext",
                    ));
                }
            });
            argument.attrs.clear();
            continue;
        }
        if matches!(
            &**ty,
            Type::Reference(_) | Type::ImplTrait(_) | Type::Infer(_)
        ) {
            return Err(syn::Error::new_spanned(
                ty,
                "model arguments must have concrete owned Deserialize + JsonSchema + Send + Sync + 'static types",
            ));
        }
        let (default, parameter_description, docs) = metadata(&argument.attrs, use_docs)?;
        argument.attrs.clear();
        let field_description =
            parameter_description.or_else(|| docs.map(|doc| LitStr::new(&doc, field.span())));
        let doc = field_description.map(|description| quote!(#[doc = #description]));
        let default_attribute = if let Some(default) = default {
            let default_fn = format_ident!("__ra_default_{index}");
            let path = LitStr::new(&default_fn.to_string(), field.span());
            defaults.push(quote!(fn #default_fn() -> #ty { #default }));
            quote!(#[serde(default = #path)])
        } else {
            required.push(LitStr::new(
                field.to_string().trim_start_matches("r#"),
                field.span(),
            ));
            quote!()
        };
        fields.push(quote!(#doc #default_attribute #field: #ty));
        arguments.push(quote!(__ra_input.#field));
    }
    Ok(expansion)
}

fn metadata(
    attributes: &[syn::Attribute],
    use_docs: bool,
) -> syn::Result<(Option<Expr>, Option<LitStr>, Option<String>)> {
    let mut default = None;
    let mut parameter_description = None;
    let docs = if use_docs {
        documentation(attributes)
    } else {
        None
    };
    for attr in attributes {
        if attr.path().is_ident("doc") {
            continue;
        }
        if !attr.path().is_ident("tool") {
            return Err(syn::Error::new_spanned(
                attr,
                "use parameter `#[tool(description = \"...\", default = expression)]` metadata",
            ));
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("default") {
                if default.is_some() {
                    return Err(meta.error("duplicate parameter default"));
                }
                default = Some(meta.value()?.parse::<Expr>()?);
                Ok(())
            } else if meta.path.is_ident("description") {
                if parameter_description.is_some() {
                    return Err(meta.error("duplicate parameter description"));
                }
                parameter_description = Some(meta.value()?.parse::<LitStr>()?);
                Ok(())
            } else {
                Err(meta.error("unsupported parameter option; expected description or default"))
            }
        })?;
    }
    Ok((default, parameter_description, docs))
}
