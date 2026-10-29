// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
#![allow(
    clippy::unnecessary_debug_formatting,
    reason = "Preserve the upstream configuration surface, branch order and diagnostic spelling."
)]
//! Validates declarative proxy hooks without resolving secrets or starting the proxy.
use super::domain_matcher::normalize_host;
use super::network::NetworkProxyConfig;
use anyhow::{Context as _, Result, anyhow};
use globset::GlobBuilder;
use http::header::HeaderName;
use ra_config::network_values::InjectedHeaderConfig;
use ra_paths::AbsolutePathBuf;
use std::{collections::BTreeMap, path::Path};
const PATTERN_PREFIX: &str = "pattern:";
const LITERAL_PREFIX: &str = "literal:";
enum MatcherPattern<'a> {
    Literal(&'a str),
    Glob(&'a str),
}
pub(super) fn validate_mitm_hook_config(config: &NetworkProxyConfig) -> Result<()> {
    let hooks = &config.mitm_hooks;
    if hooks.is_empty() {
        return Ok(());
    }

    if !config.mitm {
        return Err(anyhow!("network.mitm_hooks requires network.mitm = true"));
    }

    for (hook_index, hook) in hooks.iter().enumerate() {
        let host = normalize_hook_host(&hook.host)
            .with_context(|| format!("invalid network.mitm_hooks[{hook_index}].host"))?;

        let methods = normalize_methods(&hook.matcher.methods)
            .with_context(|| format!("invalid network.mitm_hooks[{hook_index}].match.methods"))?;
        if methods.is_empty() {
            return Err(anyhow!(
                "network.mitm_hooks[{hook_index}].match.methods must not be empty"
            ));
        }

        let path_prefixes =
            compile_path_matchers(&hook.matcher.path_prefixes).with_context(|| {
                format!("invalid network.mitm_hooks[{hook_index}].match.path_prefixes")
            })?;
        if path_prefixes.is_empty() {
            return Err(anyhow!(
                "network.mitm_hooks[{hook_index}].match.path_prefixes must not be empty"
            ));
        }

        if let Some(body) = hook.matcher.body.as_ref() {
            let _ = body;
            return Err(anyhow!(
                "network.mitm_hooks[{hook_index}].match.body is reserved for a future release and is not yet supported"
            ));
        }

        validate_query_constraints(&hook.matcher.query)
            .with_context(|| format!("invalid network.mitm_hooks[{hook_index}].match.query"))?;
        validate_header_constraints(&hook.matcher.headers)
            .with_context(|| format!("invalid network.mitm_hooks[{hook_index}].match.headers"))?;
        validate_strip_request_headers(&hook.actions.strip_request_headers).with_context(|| {
            format!("invalid network.mitm_hooks[{hook_index}].actions.strip_request_headers")
        })?;
        validate_injected_headers(&hook.actions.inject_request_headers).with_context(|| {
            format!("invalid network.mitm_hooks[{hook_index}].actions.inject_request_headers")
        })?;

        if host.is_empty() {
            return Err(anyhow!(
                "network.mitm_hooks[{hook_index}].host must not be empty"
            ));
        }
    }

    Ok(())
}

fn compile_path_matchers(path_prefixes: &[String]) -> Result<Vec<()>> {
    path_prefixes
        .iter()
        .map(|prefix| {
            match parse_matcher_pattern(prefix)? {
                MatcherPattern::Literal(prefix) => {
                    if prefix.is_empty() {
                        return Err(anyhow!("path_prefixes must not contain empty entries"));
                    }
                    Ok(())
                }
                MatcherPattern::Glob(glob_pattern) => Ok(compile_glob_matcher(
                    glob_pattern,
                    /*literal_separator*/ true,
                )?),
            }
        })
        .collect()
}

fn compile_value_matchers(values: &[String]) -> Result<Vec<()>> {
    values
        .iter()
        .map(|value| match parse_matcher_pattern(value)? {
            MatcherPattern::Literal(_) => Ok(()),
            MatcherPattern::Glob(glob_pattern) => Ok(compile_glob_matcher(
                glob_pattern,
                /*literal_separator*/ false,
            )?),
        })
        .collect()
}

fn parse_matcher_pattern(pattern: &str) -> Result<MatcherPattern<'_>> {
    if let Some(literal) = pattern.strip_prefix(LITERAL_PREFIX) {
        return Ok(MatcherPattern::Literal(literal));
    }
    let Some(glob_pattern) = pattern.strip_prefix(PATTERN_PREFIX) else {
        return Ok(MatcherPattern::Literal(pattern));
    };
    if glob_pattern.is_empty() {
        return Err(anyhow!("glob pattern must not be empty"));
    }
    Ok(MatcherPattern::Glob(glob_pattern))
}

fn compile_glob_matcher(pattern: &str, literal_separator: bool) -> Result<()> {
    let mut builder = GlobBuilder::new(pattern);
    builder
        .backslash_escape(true)
        .literal_separator(literal_separator);
    builder
        .build()
        .map(|_| ())
        .map_err(|err| anyhow!("invalid glob pattern {pattern:?}: {err}"))
}
fn normalize_hook_host(host: &str) -> Result<String> {
    let normalized = normalize_host(host);
    if normalized.is_empty() {
        return Err(anyhow!("host must not be empty"));
    }
    if normalized.contains('*') {
        return Err(anyhow!(
            "MITM hook hosts must be exact hosts and cannot contain wildcards"
        ));
    }
    Ok(normalized)
}

fn normalize_methods(methods: &[String]) -> Result<Vec<String>> {
    methods
        .iter()
        .map(|method| {
            let normalized = method.trim().to_ascii_uppercase();
            if normalized.is_empty() {
                return Err(anyhow!("methods must not contain empty entries"));
            }
            Ok(normalized)
        })
        .collect()
}

fn validate_query_constraints(query: &BTreeMap<String, Vec<String>>) -> Result<()> {
    for (name, values) in query {
        let normalized = normalize_query_name(name)?;
        if normalized.is_empty() {
            return Err(anyhow!("query keys must not be empty"));
        }
        if values.is_empty() {
            return Err(anyhow!(
                "query key {name:?} must list at least one allowed value"
            ));
        }
        let _ = compile_value_matchers(values)
            .with_context(|| format!("invalid matcher for query key {name:?}"))?;
    }
    Ok(())
}

fn normalize_query_name(name: &str) -> Result<String> {
    if name.is_empty() {
        return Err(anyhow!("query keys must not be empty"));
    }
    Ok(name.to_string())
}

fn validate_header_constraints(headers: &BTreeMap<String, Vec<String>>) -> Result<()> {
    for (name, values) in headers {
        let _ = parse_header_name(name)?;
        let _ = compile_value_matchers(values)
            .with_context(|| format!("invalid matcher for header {name:?}"))?;
    }
    Ok(())
}

fn validate_strip_request_headers(header_names: &[String]) -> Result<()> {
    for name in header_names {
        let _ = parse_header_name(name)?;
    }
    Ok(())
}

fn validate_injected_headers(headers: &[InjectedHeaderConfig]) -> Result<()> {
    for header in headers {
        let _ = parse_header_name(&header.name)?;
        match (
            header.secret_env_var.as_deref(),
            header.secret_file.as_deref(),
        ) {
            (Some(secret_env_var), None) => {
                if secret_env_var.trim().is_empty() {
                    return Err(anyhow!("secret_env_var must not be empty"));
                }
            }
            (None, Some(secret_file)) => {
                let _ = parse_secret_file(secret_file)?;
            }
            _ => {
                return Err(anyhow!(
                    "expected exactly one of secret_env_var or secret_file"
                ));
            }
        }
    }
    Ok(())
}

fn parse_header_name(name: &str) -> Result<HeaderName> {
    HeaderName::from_bytes(name.as_bytes())
        .map_err(|err| anyhow!("invalid header name {name:?}: {err}"))
}

fn parse_secret_file(path: &str) -> Result<AbsolutePathBuf> {
    if path.trim().is_empty() {
        return Err(anyhow!("secret_file must not be empty"));
    }
    let path = Path::new(path);
    if !path.is_absolute() {
        return Err(anyhow!("secret_file must be an absolute path: {path:?}"));
    }
    AbsolutePathBuf::from_absolute_path(path)
        .with_context(|| format!("secret_file must be an absolute path: {path:?}"))
}
