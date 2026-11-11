//! Input fields and context injection for function tools.

use super::{documentation, last_type_name};
use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::{Expr, FnArg, GenericArgument, LitStr, Pat, PathArguments, Signature, Type};

#[derive(Default)]
pub(super) struct Parameters {
    pub(super) fields: Vec<TokenStream>,
    pub(super) defaults: Vec<TokenStream>,
    pub(super) arguments: Vec<TokenStream>,
    pub(super) required: Vec<LitStr>,
    pub(super) approval_checks: Vec<TokenStream>,
    pub(super) inject_context: bool,
}

pub(super) fn expand(
    signature: &mut Signature,
    use_docs: bool,
    support: &TokenStream,
) -> syn::Result<Parameters> {
    let mut expansion = Parameters::default();
    let Parameters {
        fields,
        defaults,
        arguments,
        required,
        approval_checks,
        inject_context,
    } = &mut expansion;
    for (index, argument) in signature.inputs.iter_mut().enumerate() {
        let FnArg::Typed(argument) = argument else {
            return Err(syn::Error::new_spanned(
                argument,
                "tool functions cannot have a `self` receiver",
            ));
        };
        let field = parameter_name(&argument.pat)?;
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
        let name = LitStr::new(field.to_string().trim_start_matches("r#"), field.span());
        let unchanged =
            argument_unchanged(ty, &quote!(::core::option::Option::Some(argument)), support);
        approval_checks.push(quote!(input.get(#name).is_some_and(|argument| #unchanged)));
        let default_attribute = if let Some(default) = default {
            let default_fn = format_ident!("__ra_default_{index}");
            let path = LitStr::new(&default_fn.to_string(), field.span());
            defaults.push(quote!(fn #default_fn() -> #ty { #default }));
            quote!(#[serde(default = #path)])
        } else {
            required.push(name);
            quote!()
        };
        fields.push(quote!(#doc #default_attribute #field: #ty));
        arguments.push(quote!(__ra_input.#field));
    }
    Ok(expansion)
}

fn parameter_name(pattern: &Pat) -> syn::Result<&syn::Ident> {
    let Pat::Ident(binding) = pattern else {
        return Err(syn::Error::new_spanned(
            pattern,
            "tool arguments must use named bindings, not destructuring patterns",
        ));
    };
    if binding.by_ref.is_some() || binding.subpat.is_some() {
        return Err(syn::Error::new_spanned(
            binding,
            "tool arguments must use plain named bindings",
        ));
    }
    Ok(&binding.ident)
}

fn argument_unchanged(ty: &Type, value: &TokenStream, support: &TokenStream) -> TokenStream {
    if let Type::Path(path) = ty
        && let Some(segment) = path.path.segments.last()
        && let PathArguments::AngleBracketed(arguments) = &segment.arguments
    {
        let types: Vec<_> = arguments
            .args
            .iter()
            .filter_map(|argument| match argument {
                GenericArgument::Type(ty) => Some(ty),
                _ => None,
            })
            .collect();
        match (segment.ident.to_string().as_str(), types.as_slice()) {
            ("Option", [item]) => {
                let present = quote!(#value.filter(|argument| !argument.is_null()));
                let unchanged = argument_unchanged(item, &present, support);
                return quote!(
                    ::core::any::TypeId::of::<#ty>()
                        == ::core::any::TypeId::of::<::core::option::Option<#item>>()
                        && #unchanged
                );
            }
            ("Vec", [item]) => {
                let unchanged = argument_unchanged(item, &quote!(element), support);
                return quote!({
                    let element_unchanged = |element: ::core::option::Option<&#support::serde_json::Value>| #unchanged;
                    ::core::any::TypeId::of::<#ty>()
                        == ::core::any::TypeId::of::<::std::vec::Vec<#item>>()
                        && element_unchanged(::core::option::Option::None)
                        && #value.is_none_or(|argument| {
                            argument.as_array().is_some_and(|items| {
                                items.iter().all(|item| element_unchanged(::core::option::Option::Some(item)))
                            })
                        })
                });
            }
            ("HashMap" | "BTreeMap", [key, item]) => {
                let name = &segment.ident;
                let unchanged = argument_unchanged(item, &quote!(element), support);
                return quote!({
                    let element_unchanged = |element: ::core::option::Option<&#support::serde_json::Value>| #unchanged;
                    ::core::any::TypeId::of::<#ty>()
                        == ::core::any::TypeId::of::<::std::collections::#name<::std::string::String, #item>>()
                        && ::core::any::TypeId::of::<#key>()
                            == ::core::any::TypeId::of::<::std::string::String>()
                        && element_unchanged(::core::option::Option::None)
                        && #value.is_none_or(|argument| {
                            argument.as_object().is_some_and(|items| {
                                items.values().all(|item| element_unchanged(::core::option::Option::Some(item)))
                            })
                        })
                });
            }
            _ => {}
        }
    }
    quote!(scalar_unchanged::<#ty>(#value))
}

pub(super) fn approval_inspection(checks: &[TokenStream], support: &TokenStream) -> TokenStream {
    let field_count = checks.len();
    quote! {
        fn approval_arguments_inspectable(&self, value: &#support::serde_json::Value) -> bool {
            fn scalar_unchanged<T: 'static>(
                value: ::core::option::Option<&#support::serde_json::Value>,
            ) -> bool {
                let argument = ::core::any::TypeId::of::<T>();
                let is_f32 = argument == ::core::any::TypeId::of::<::core::primitive::f32>();
                let is_f64 = argument == ::core::any::TypeId::of::<::core::primitive::f64>();
                if is_f32 || is_f64 {
                    return value.is_none_or(|value| {
                        let ::core::option::Option::Some(number) = value.as_number().filter(|number| number.is_f64()) else {
                            return false;
                        };
                        if is_f64 {
                            return true;
                        }
                        #support::serde_json::from_value::<::core::primitive::f32>(value.clone())
                            .is_ok_and(|decoded| {
                                number.as_f64().is_some_and(|original| {
                                    original.to_bits() == ::core::primitive::f64::from(decoded).to_bits()
                                })
                            })
                    });
                }
                [
                    ::core::any::TypeId::of::<()>(),
                    ::core::any::TypeId::of::<::core::primitive::bool>(),
                    ::core::any::TypeId::of::<::std::string::String>(),
                    ::core::any::TypeId::of::<#support::serde_json::Value>(),
                    ::core::any::TypeId::of::<::core::primitive::i8>(),
                    ::core::any::TypeId::of::<::core::primitive::i16>(),
                    ::core::any::TypeId::of::<::core::primitive::i32>(),
                    ::core::any::TypeId::of::<::core::primitive::i64>(),
                    ::core::any::TypeId::of::<::core::primitive::i128>(),
                    ::core::any::TypeId::of::<::core::primitive::isize>(),
                    ::core::any::TypeId::of::<::core::primitive::u8>(),
                    ::core::any::TypeId::of::<::core::primitive::u16>(),
                    ::core::any::TypeId::of::<::core::primitive::u32>(),
                    ::core::any::TypeId::of::<::core::primitive::u64>(),
                    ::core::any::TypeId::of::<::core::primitive::u128>(),
                    ::core::any::TypeId::of::<::core::primitive::usize>(),
                ]
                .contains(&argument)
            }

            // Checking every field before deserialization avoids application default factories.
            value.as_object().is_some_and(|input| {
                input.len() == #field_count #(&& #checks)*
            })
                && self.function.decode_value_diagnostic(value.clone()).is_ok()
        }
    }
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
