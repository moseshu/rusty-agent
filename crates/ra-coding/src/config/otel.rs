// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
#![allow(missing_docs, reason = "Preserve upstream configuration records.")]
use std::collections::BTreeMap;
use std::fmt::Display;

use ra_config::types::DEFAULT_OTEL_ENVIRONMENT;
use ra_config::types::OtelConfig;
use ra_config::types::OtelConfigToml;
use ra_config::types::OtelExporterKind;

pub(crate) fn resolve_config(
    config: OtelConfigToml,
    startup_warnings: &mut Vec<String>,
) -> OtelConfig {
    let log_user_prompt = config.log_user_prompt.unwrap_or(false);
    let environment = config
        .environment
        .unwrap_or_else(|| DEFAULT_OTEL_ENVIRONMENT.to_string());
    let exporter = config.exporter.unwrap_or(OtelExporterKind::None);
    // OTLP HTTP endpoints are signal-specific in our config, so enabling log
    // export must not implicitly send spans to a /v1/logs endpoint.
    let trace_exporter = config.trace_exporter.unwrap_or(OtelExporterKind::None);
    let metrics_exporter = config.metrics_exporter.unwrap_or(OtelExporterKind::Statsig);
    // Provider initialization installs process-global OTEL state. Sanitize
    // user-editable trace metadata here so malformed config is reported as a
    // startup warning instead of making startup fail.
    let span_attributes = resolve_span_attributes(config.span_attributes, startup_warnings);
    let tracestate = resolve_tracestate(config.tracestate, startup_warnings);

    OtelConfig {
        tool_result: config.tool_result,
        log_user_prompt,
        log_agent_responses: config.log_agent_responses.unwrap_or(false),
        log_guardian_assessments: config.log_guardian_assessments.unwrap_or(false),
        environment,
        exporter,
        trace_exporter,
        metrics_exporter,
        span_attributes,
        tracestate,
    }
}

fn resolve_span_attributes(
    span_attributes: Option<BTreeMap<String, String>>,
    startup_warnings: &mut Vec<String>,
) -> BTreeMap<String, String> {
    let Some(span_attributes) = span_attributes else {
        return BTreeMap::new();
    };

    let mut valid_attributes = BTreeMap::new();
    for (key, value) in span_attributes {
        let attribute = BTreeMap::from([(key.clone(), value.clone())]);
        if let Err(err) = validate_span_attributes(&attribute) {
            push_invalid_config_warning("otel.span_attributes", err, startup_warnings);
            continue;
        }
        valid_attributes.insert(key, value);
    }

    valid_attributes
}

fn resolve_tracestate(
    tracestate: Option<BTreeMap<String, BTreeMap<String, String>>>,
    startup_warnings: &mut Vec<String>,
) -> BTreeMap<String, BTreeMap<String, String>> {
    let Some(tracestate) = tracestate else {
        return BTreeMap::new();
    };

    let mut valid_entries = BTreeMap::new();
    for (member_key, fields) in tracestate {
        let fields = resolve_tracestate_member_fields(&member_key, fields, startup_warnings);
        if fields.is_empty() {
            continue;
        }
        if let Err(err) = validate_tracestate_member(&member_key, &fields) {
            push_invalid_config_warning("otel.tracestate", err, startup_warnings);
            continue;
        }
        valid_entries.insert(member_key, fields);
    }

    // Tracestate members can be valid individually while the combined W3C
    // tracestate header is not, so validate the filtered set before handing it
    // to provider initialization.
    if let Err(err) = validate_tracestate_entries(&valid_entries) {
        push_invalid_config_warning("otel.tracestate", err, startup_warnings);
        return BTreeMap::new();
    }

    valid_entries
}

fn resolve_tracestate_member_fields(
    member_key: &str,
    fields: BTreeMap<String, String>,
    startup_warnings: &mut Vec<String>,
) -> BTreeMap<String, String> {
    let mut valid_fields = BTreeMap::new();
    for (field_key, value) in fields {
        let field = BTreeMap::from([(field_key.clone(), value.clone())]);
        if let Err(err) = validate_tracestate_member(member_key, &field) {
            push_invalid_config_warning("otel.tracestate", err, startup_warnings);
            continue;
        }
        valid_fields.insert(field_key, value);
    }
    valid_fields
}

fn push_invalid_config_warning(
    config_key: &str,
    err: impl Display,
    startup_warnings: &mut Vec<String>,
) {
    let message = format!("Ignoring invalid `{config_key}` config: {err}");
    tracing::warn!("{message}");
    startup_warnings.push(message);
}

use opentelemetry::trace::TraceState;
pub(super) fn validate_span_attributes(
    attributes: &BTreeMap<String, String>,
) -> std::io::Result<()> {
    if attributes.keys().any(String::is_empty) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "configured span attribute key must not be empty",
        ));
    }

    Ok(())
}
pub(super) fn validate_tracestate_entries(
    entries: &BTreeMap<String, BTreeMap<String, String>>,
) -> Result<(), Box<dyn std::error::Error>> {
    // Reject malformed entries before installing them so propagated trace
    // context remains acceptable to other W3C Trace Context extractors. The
    // SDK validates member keys and list structure, but configured member
    // fields are joined into header values here and need stricter validation.
    let entries = entries
        .iter()
        .map(|(key, fields)| encode_tracestate_member_fields(key, fields))
        .collect::<Result<Vec<_>, _>>()?;
    TraceState::from_key_value(
        entries
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str())),
    )
    .map_err(|err| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("invalid configured tracestate: {err}"),
        )
    })?;
    Ok(())
}
pub(super) fn validate_tracestate_member(
    member_key: &str,
    fields: &BTreeMap<String, String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let (key, value) = encode_tracestate_member_fields(member_key, fields)?;
    TraceState::from_key_value([(key.as_str(), value.as_str())]).map_err(|err| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("invalid configured tracestate: {err}"),
        )
    })?;
    Ok(())
}
fn encode_tracestate_member_fields(
    member_key: &str,
    fields: &BTreeMap<String, String>,
) -> Result<(String, String), Box<dyn std::error::Error>> {
    // Configured fields are encoded into one opaque tracestate member value.
    // Validate both the field grammar and the final header value so malformed
    // config cannot produce propagated trace context that downstream W3C
    // extractors reject.
    let mut encoded = Vec::with_capacity(fields.len());
    for (field_key, value) in fields {
        if !is_configured_tracestate_field_key(field_key) {
            return Err(invalid_tracestate_config(format!(
                "invalid configured tracestate field key {member_key}.{field_key}"
            )));
        }
        if !is_configured_tracestate_field_value(value) {
            return Err(invalid_tracestate_config(format!(
                "invalid configured tracestate value for {member_key}.{field_key}"
            )));
        }
        encoded.push(format!("{field_key}:{value}"));
    }
    let value = encoded.join(";");
    if !is_header_safe_tracestate_member_value(&value) {
        return Err(invalid_tracestate_config(format!(
            "invalid configured tracestate value for {member_key}"
        )));
    }
    Ok((member_key.to_string(), value))
}
fn is_configured_tracestate_field_key(field_key: &str) -> bool {
    !field_key.is_empty()
        && field_key
            .bytes()
            .all(|byte| matches!(byte, b'!'..=b'~') && !matches!(byte, b':' | b';' | b',' | b'='))
}
fn is_configured_tracestate_field_value(value: &str) -> bool {
    value
        .bytes()
        .all(|byte| is_tracestate_member_value_byte(byte) && byte != b';')
}
fn is_header_safe_tracestate_member_value(value: &str) -> bool {
    value.is_empty()
        || (value.bytes().all(is_tracestate_member_value_byte)
            && value.as_bytes().last().is_some_and(|byte| *byte != b' '))
}
fn invalid_tracestate_config(message: String) -> Box<dyn std::error::Error> {
    Box::new(std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        message,
    ))
}

fn is_tracestate_member_value_byte(byte: u8) -> bool {
    matches!(byte, b' '..=b'~') && !matches!(byte, b',' | b'=')
}
