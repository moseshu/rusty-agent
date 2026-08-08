//! Parser for `#[tool_input(...)]` derive options.

use syn::{Attribute, LitBool, LitStr};

pub(crate) struct ToolInputOptions {
    pub(crate) strict: bool,
    pub(crate) description: Option<LitStr>,
}

impl ToolInputOptions {
    pub(crate) fn parse(attributes: &[Attribute]) -> syn::Result<Self> {
        let mut strict = None;
        let mut description = None;
        for attribute in attributes
            .iter()
            .filter(|attribute| attribute.path().is_ident("tool_input"))
        {
            attribute.parse_nested_meta(|meta| {
                if meta.path.is_ident("strict") {
                    if strict.is_some() {
                        return Err(meta.error("duplicate `strict` option"));
                    }
                    strict = Some(meta.value()?.parse::<LitBool>()?.value);
                    return Ok(());
                }
                if meta.path.is_ident("description") {
                    if description.is_some() {
                        return Err(meta.error("duplicate `description` option"));
                    }
                    description = Some(meta.value()?.parse::<LitStr>()?);
                    return Ok(());
                }
                Err(meta.error("unsupported tool_input option; expected `strict` or `description`"))
            })?;
        }
        Ok(Self {
            strict: strict.unwrap_or(true),
            description,
        })
    }
}
