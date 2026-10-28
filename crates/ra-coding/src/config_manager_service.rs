// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Reads the effective product configuration and writes validated edits to the user layer.
//!
//! The source implements these operations on the app-server's configuration manager; here
//! they belong to [`PluginConfigManager`], the product loader that already captures the home,
//! process cwd, session overrides and loader options used by every other product reload.
use crate::plugins::PluginConfigManager;
use ra_config::AskForApproval;
use ra_config::CONFIG_TOML_FILE;
use ra_config::ConfigEditsBuilder;
use ra_config::ConfigLayerEntry;
use ra_config::ConfigLayerMetadata;
use ra_config::ConfigLayerSource;
use ra_config::ConfigLayerStack;
use ra_config::ConfigRequirementsToml;
use ra_config::ConfigToml;
use ra_config::ShellEnvironmentPolicyFilterRepresentation;
use ra_config::deserialize_config_toml_with_base;
use ra_config::is_structured_feature_path;
use ra_config::merge_toml_values;
use ra_config::shell_environment_filter_entry;
use ra_config::validate_feature_requirements_for_config_toml;
use ra_config::validate_shell_environment_policy_filter_config;
use ra_paths::AbsolutePathBuf;
use ra_paths::SymlinkWritePaths;
use ra_paths::resolve_symlink_write_paths;
use ra_paths::write_atomically;
use ra_protocol::server::protocol::Config as ApiConfig;
use ra_protocol::server::protocol::ConfigBatchWriteParams;
use ra_protocol::server::protocol::ConfigReadParams;
use ra_protocol::server::protocol::ConfigReadResponse;
use ra_protocol::server::protocol::ConfigValueWriteParams;
use ra_protocol::server::protocol::ConfigWriteErrorCode;
use ra_protocol::server::protocol::ConfigWriteResponse;
use ra_protocol::server::protocol::MergeStrategy;
use ra_protocol::server::protocol::OverriddenMetadata;
use ra_protocol::server::protocol::WriteStatus;
use serde_json::Value as JsonValue;
use std::borrow::Cow;
use std::path::Path;
use std::path::PathBuf;
use thiserror::Error;
use tokio::task;
use toml::Value as TomlValue;
use toml_edit::Item as TomlItem;

mod credential_edits;

use credential_edits::CredentialProviderEdits;

#[derive(Debug, Error)]
/// A failed configuration read or write, with the source write-error code when it has one.
pub enum ConfigManagerError {
    /// A rejected write, reported to clients with its protocol code.
    #[error("{message}")]
    Write {
        /// Protocol error code.
        code: ConfigWriteErrorCode,
        /// Client-facing explanation.
        message: String,
    },

    /// A filesystem or loader failure.
    #[error("{context}: {source}")]
    Io {
        /// The step that failed.
        context: &'static str,
        /// The underlying error.
        #[source]
        source: std::io::Error,
    },

    /// A JSON conversion failure.
    #[error("{context}: {source}")]
    Json {
        /// The step that failed.
        context: &'static str,
        /// The underlying error.
        #[source]
        source: serde_json::Error,
    },

    /// A TOML parse failure.
    #[error("{context}: {source}")]
    Toml {
        /// The step that failed.
        context: &'static str,
        /// The underlying error.
        #[source]
        source: toml::de::Error,
    },

    /// A persistence failure.
    #[error("{context}: {source}")]
    Anyhow {
        /// The step that failed.
        context: &'static str,
        /// The underlying error.
        #[source]
        source: anyhow::Error,
    },
}

impl ConfigManagerError {
    fn write(code: ConfigWriteErrorCode, message: impl Into<String>) -> Self {
        Self::Write {
            code,
            message: message.into(),
        }
    }

    fn io(context: &'static str, source: std::io::Error) -> Self {
        Self::Io { context, source }
    }

    fn json(context: &'static str, source: serde_json::Error) -> Self {
        Self::Json { context, source }
    }

    fn toml(context: &'static str, source: toml::de::Error) -> Self {
        Self::Toml { context, source }
    }

    fn anyhow(context: &'static str, source: anyhow::Error) -> Self {
        Self::Anyhow { context, source }
    }

    /// The protocol write-error code, absent for I/O and serialization failures.
    #[must_use]
    pub fn write_error_code(&self) -> Option<ConfigWriteErrorCode> {
        match self {
            Self::Write { code, .. } => Some(code.clone()),
            _ => None,
        }
    }
}

impl PluginConfigManager {
    /// Reads effective configuration, origins and optionally every layer.
    pub async fn read(
        &self,
        params: ConfigReadParams,
    ) -> Result<ConfigReadResponse, ConfigManagerError> {
        let layers = match params.cwd.as_deref() {
            Some(cwd) => {
                let cwd = AbsolutePathBuf::try_from(PathBuf::from(cwd)).map_err(|err| {
                    ConfigManagerError::io("failed to resolve config cwd to an absolute path", err)
                })?;
                self.load_config_layers_for_cwd(Some(cwd))
                    .await
                    .map_err(|err| {
                        ConfigManagerError::io("failed to read configuration layers", err)
                    })?
            }
            None => self.load_thread_agnostic_config().await.map_err(|err| {
                ConfigManagerError::io("failed to read configuration layers", err)
            })?,
        };

        let effective = layers.effective_config();
        let mut effective_config_toml: ConfigToml = effective
            .try_into()
            .map_err(|err| ConfigManagerError::toml("invalid configuration", err))?;
        layers
            .requirements_toml()
            .apply_exact_to_config(&mut effective_config_toml);
        effective_config_toml.allow_login_shell.get_or_insert(true);

        let json_value = serde_json::to_value(&effective_config_toml)
            .map_err(|err| ConfigManagerError::json("failed to serialize configuration", err))?;
        let config: ApiConfig = serde_json::from_value(json_value)
            .map_err(|err| ConfigManagerError::json("failed to deserialize configuration", err))?;

        let mut origins = layers.origins_with_path_filter(|segments| {
            layers
                .requirements_toml()
                .exact_requirement_for_config_path(segments)
                .is_none()
        });
        origins.retain(|_, metadata| {
            !matches!(&metadata.name, ConfigLayerSource::PackagedDefaults { .. })
        });

        Ok(ConfigReadResponse {
            config,
            origins: origins
                .into_iter()
                .map(|(path, metadata)| (path, metadata.into()))
                .collect(),
            layers: params.include_layers.then(|| {
                layers
                    .all_layers_high_to_low()
                    .filter(|layer| {
                        !matches!(&layer.name, ConfigLayerSource::PackagedDefaults { .. })
                    })
                    .map(|layer| layer.as_layer().into())
                    .collect()
            }),
        })
    }

    /// Reads the composed requirements, absent when no requirement is configured.
    pub async fn read_requirements(
        &self,
    ) -> Result<Option<ConfigRequirementsToml>, ConfigManagerError> {
        let layers = self
            .load_thread_agnostic_config()
            .await
            .map_err(|err| ConfigManagerError::io("failed to read configuration layers", err))?;

        let requirements = layers.requirements_toml().clone();
        if requirements.is_empty() {
            Ok(None)
        } else {
            Ok(Some(requirements))
        }
    }

    /// Writes one value to the active user configuration layer.
    pub async fn write_value(
        &self,
        params: ConfigValueWriteParams,
    ) -> Result<ConfigWriteResponse, ConfigManagerError> {
        let edits = vec![(params.key_path, params.value, params.merge_strategy)];
        Box::pin(self.apply_edits(params.file_path, params.expected_version, edits)).await
    }

    /// Writes ordered edits to the active user configuration layer.
    pub async fn batch_write(
        &self,
        params: ConfigBatchWriteParams,
    ) -> Result<ConfigWriteResponse, ConfigManagerError> {
        let edits = params
            .edits
            .into_iter()
            .map(|edit| (edit.key_path, edit.value, edit.merge_strategy))
            .collect();

        Box::pin(self.apply_edits(params.file_path, params.expected_version, edits)).await
    }

    #[allow(
        clippy::too_many_lines,
        reason = "Keep the source function whole; splitting would obscure the port."
    )]
    async fn apply_edits(
        &self,
        file_path: Option<String>,
        expected_version: Option<String>,
        edits: Vec<(String, JsonValue, MergeStrategy)>,
    ) -> Result<ConfigWriteResponse, ConfigManagerError> {
        let allowed_path = self
            .user_config_path()
            .map_err(|err| ConfigManagerError::io("failed to resolve user config path", err))?;
        let provided_path = match file_path {
            Some(path) => AbsolutePathBuf::from_absolute_path(PathBuf::from(path))
                .map_err(|err| ConfigManagerError::io("failed to resolve user config path", err))?,
            None => allowed_path.clone(),
        };

        if !paths_match(&allowed_path, &provided_path) {
            return Err(ConfigManagerError::write(
                ConfigWriteErrorCode::ConfigLayerReadonly,
                "Only writes to the user config are allowed",
            ));
        }

        let layers = self
            .load_thread_agnostic_config()
            .await
            .map_err(|err| ConfigManagerError::io("failed to load configuration", err))?;
        let user_layer = match layers.get_active_user_layer() {
            Some(layer) => Cow::Borrowed(layer),
            None => Cow::Owned(create_empty_user_layer(&allowed_path).await?),
        };

        if let Some(expected) = expected_version.as_deref()
            && expected != user_layer.version
        {
            return Err(ConfigManagerError::write(
                ConfigWriteErrorCode::ConfigVersionConflict,
                "Configuration was modified since last read. Fetch latest version and retry.",
            ));
        }

        let mut user_config = user_layer.config.clone();
        let mut credential_provider_edits = CredentialProviderEdits::new(&user_config);
        let mut parsed_segments = Vec::new();
        let mut config_edits = Vec::new();

        for (key_path, value, strategy) in edits {
            let mut segments = parse_key_path(&key_path).map_err(|message| {
                ConfigManagerError::write(ConfigWriteErrorCode::ConfigValidationError, message)
            })?;
            if let Some(field) = layers
                .requirements_toml()
                .exact_requirement_for_config_path(&segments)
            {
                return Err(ConfigManagerError::write(
                    ConfigWriteErrorCode::ConfigRequirementReadonly,
                    format!("`{field}` is managed by requirements and cannot be changed"),
                ));
            }
            if (value.is_null() || matches!(strategy, MergeStrategy::Upsert))
                && let Some(pattern) = shell_environment_filter_entry(&user_config, &segments)
                    .map(|(pattern, _)| pattern.clone())
            {
                segments[2] = pattern;
            }
            if !value.is_null() {
                match segments.as_slice() {
                    [segment] if segment == "profile" => {
                        return Err(ConfigManagerError::write(
                            ConfigWriteErrorCode::ConfigValidationError,
                            "`profile` is a legacy config selector and can no longer be written; use `--profile <name>` with `<name>.config.toml` instead",
                        ));
                    }
                    [segment, ..] if segment == "profiles" => {
                        return Err(ConfigManagerError::write(
                            ConfigWriteErrorCode::ConfigValidationError,
                            "`profiles` contains legacy config profile tables and can no longer be written; use `--profile <name>` with `<name>.config.toml` instead",
                        ));
                    }
                    _ => {}
                }
            }
            let preserves_network_proxy_settings = value.is_null()
                && is_structured_feature_path(&segments)
                && segments
                    .last()
                    .is_some_and(|feature| feature == "network_proxy")
                && {
                    let has_broker_settings = |config: &TomlValue| {
                        value_at_path(config, &segments)
                            .and_then(TomlValue::as_table)
                            .is_some_and(|feature| {
                                feature.contains_key("credential_broker")
                                    || feature.contains_key("credentials")
                            })
                    };
                    has_broker_settings(&user_config)
                        || !has_broker_settings(&user_layer.config)
                            && has_broker_settings(&layers.effective_config())
                };
            let parsed_value = if preserves_network_proxy_settings {
                Some(TomlValue::Boolean(false))
            } else {
                parse_value(value).map_err(|message| {
                    ConfigManagerError::write(ConfigWriteErrorCode::ConfigValidationError, message)
                })?
            };
            if matches!(strategy, MergeStrategy::Upsert)
                && let Some(value) = parsed_value.as_ref()
                && matches!(segments.as_slice(), [policy, ..] if policy == "shell_environment_policy")
            {
                validate_shell_environment_policy_filter_config(&sparse_overlay(&segments, value))
                    .map_err(|err| {
                        ConfigManagerError::write(
                            ConfigWriteErrorCode::ConfigValidationError,
                            format!("Invalid configuration: {err}"),
                        )
                    })?;
            }

            let structured_feature_toggle = preserves_network_proxy_settings
                || is_structured_feature_path(&segments)
                    && parsed_value.as_ref().is_some_and(|value| {
                        value.is_bool()
                            || matches!(strategy, MergeStrategy::Upsert)
                                && value.as_table().is_some_and(|table| {
                                    table.len() == 1 && table.contains_key("enabled")
                                })
                    });

            if let Some(edit) = credential_provider_edits.apply(
                &mut user_config,
                &segments,
                parsed_value.as_ref(),
                strategy,
            )? {
                config_edits.push(edit);
            }

            if structured_feature_toggle {
                segments.push("enabled".to_string());
            }
            parsed_segments.push(segments);
        }
        config_edits.extend(credential_provider_edits.remapping_edits(&user_config)?);

        validate_config(&user_config).map_err(|err| {
            ConfigManagerError::write(
                ConfigWriteErrorCode::ConfigValidationError,
                format!("Invalid configuration: {err}"),
            )
        })?;
        let user_config_toml =
            deserialize_config_toml_with_base(user_config.clone(), self.home().as_path()).map_err(
                |err| {
                    ConfigManagerError::write(
                        ConfigWriteErrorCode::ConfigValidationError,
                        format!("Invalid configuration: {err}"),
                    )
                },
            )?;
        validate_feature_requirements_for_config_toml(
            &user_config_toml,
            layers.requirements().feature_requirements.as_ref(),
        )
        .map_err(|err| {
            ConfigManagerError::write(
                ConfigWriteErrorCode::ConfigValidationError,
                format!("Invalid configuration: {err}"),
            )
        })?;
        let updated_layers = layers
            .with_user_config(&provided_path, user_config.clone())
            .map_err(|err| {
                ConfigManagerError::write(
                    ConfigWriteErrorCode::ConfigValidationError,
                    format!("Invalid configuration: {err}"),
                )
            })?;
        let effective = updated_layers.effective_config();
        validate_config(&effective)
            .and_then(|()| {
                credential_provider_edits.validate_remapping(
                    &user_layer.config,
                    &user_config,
                    &updated_layers,
                )
            })
            .map_err(|err| {
                ConfigManagerError::write(
                    ConfigWriteErrorCode::ConfigValidationError,
                    format!("Invalid configuration: {err}"),
                )
            })?;

        if !config_edits.is_empty() {
            ConfigEditsBuilder::for_config_path(provided_path.as_path())
                .with_edits(config_edits)
                .apply()
                .await
                .map_err(|err| ConfigManagerError::anyhow("failed to persist config.toml", err))?;
        }

        let overridden = first_overridden_edit(&updated_layers, &effective, &parsed_segments);
        let status = overridden
            .as_ref()
            .map_or(WriteStatus::Ok, |_| WriteStatus::OkOverridden);

        Ok(ConfigWriteResponse {
            status,
            version: updated_layers
                .get_active_user_layer()
                .ok_or_else(|| {
                    ConfigManagerError::write(
                        ConfigWriteErrorCode::UserLayerNotFound,
                        "user layer not found in updated layers",
                    )
                })?
                .version
                .clone(),
            file_path: provided_path,
            overridden_metadata: overridden,
        })
    }

    /// Loads a "thread-agnostic" config, which means the config layers do not
    /// include any project configuration folders because there is no cwd/project root
    /// associated with this query.
    async fn load_thread_agnostic_config(&self) -> std::io::Result<ConfigLayerStack> {
        self.load_config_layers_for_cwd(/*cwd*/ None).await
    }
}

async fn create_empty_user_layer(
    config_toml: &AbsolutePathBuf,
) -> Result<ConfigLayerEntry, ConfigManagerError> {
    let SymlinkWritePaths {
        read_path,
        write_path,
    } = resolve_symlink_write_paths(config_toml.as_path())
        .map_err(|err| ConfigManagerError::io("failed to resolve user config path", err))?;
    let toml_value = if let Some(path) = read_path {
        match tokio::fs::read_to_string(&path).await {
            Ok(contents) => toml::from_str(&contents).map_err(|e| {
                ConfigManagerError::toml("failed to parse existing user config.toml", e)
            })?,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                write_empty_user_config(write_path.clone()).await?;
                TomlValue::Table(toml::map::Map::new())
            }
            Err(err) => {
                return Err(ConfigManagerError::io(
                    "failed to read user config.toml",
                    err,
                ));
            }
        }
    } else {
        write_empty_user_config(write_path).await?;
        TomlValue::Table(toml::map::Map::new())
    };
    Ok(ConfigLayerEntry::new(
        ConfigLayerSource::User {
            file: config_toml.clone(),
            profile: None,
        },
        toml_value,
    ))
}

async fn write_empty_user_config(write_path: PathBuf) -> Result<(), ConfigManagerError> {
    task::spawn_blocking(move || write_atomically(&write_path, ""))
        .await
        .map_err(|err| ConfigManagerError::anyhow("config persistence task panicked", err.into()))?
        .map_err(|err| ConfigManagerError::io("failed to create empty user config.toml", err))
}

fn parse_value(value: JsonValue) -> Result<Option<TomlValue>, String> {
    if value.is_null() {
        return Ok(None);
    }

    serde_json::from_value::<TomlValue>(value)
        .map(Some)
        .map_err(|err| format!("invalid value: {err}"))
}

fn parse_key_path(path: &str) -> Result<Vec<String>, String> {
    if path.trim().is_empty() {
        return Err("keyPath must not be empty".to_string());
    }

    let mut segments = Vec::new();
    let mut segment = String::new();
    let mut chars = path.chars();
    let mut quoted = false;

    // Split on dots unless they appear inside a quoted segment. Bare segments
    // intentionally stay permissive so existing paths like `sample@catalog`
    // remain valid.
    while let Some(ch) = chars.next() {
        match ch {
            '"' if segment.is_empty() && !quoted => quoted = true,
            '"' if quoted => quoted = false,
            '\\' if quoted => {
                // Quoted segments may escape punctuation that would otherwise
                // participate in parsing, such as `.` or `"`.
                let Some(escaped) = chars.next() else {
                    return Err("unterminated escape in keyPath".to_string());
                };
                segment.push(escaped);
            }
            '.' if !quoted => {
                if segment.is_empty() {
                    return Err("keyPath segments must not be empty".to_string());
                }
                segments.push(std::mem::take(&mut segment));
            }
            '"' => return Err("invalid quoted keyPath segment".to_string()),
            _ => segment.push(ch),
        }
    }

    if quoted {
        return Err("unterminated quoted keyPath segment".to_string());
    }
    if segment.is_empty() {
        return Err("keyPath segments must not be empty".to_string());
    }

    segments.push(segment);
    Ok(segments)
}

#[derive(Debug)]
enum MergeError {
    Validation(String),
}

fn structured_feature_depth(segments: &[String]) -> Option<usize> {
    let depth = match segments {
        [profiles, _, _, _, ..] if profiles == "profiles" => 4,
        [_, _, ..] => 2,
        _ => return None,
    };

    is_structured_feature_path(&segments[..depth]).then_some(depth)
}

fn credential_providers_path(segments: &[String]) -> Option<&[String]> {
    match segments {
        [features, proxy, credentials, ..]
            if features == "features"
                && proxy == "network_proxy"
                && credentials == "credentials" =>
        {
            Some(&segments[..3])
        }
        _ => None,
    }
}

#[allow(
    clippy::needless_pass_by_value,
    reason = "Retain the source signature; callers hand over ownership."
)]
fn apply_merge(
    root: &mut TomlValue,
    segments: &[String],
    value: Option<&TomlValue>,
    strategy: MergeStrategy,
) -> Result<bool, MergeError> {
    let Some(value) = value else {
        return clear_path(root, segments);
    };

    let Some((last, parents)) = segments.split_last() else {
        return Err(MergeError::Validation(
            "keyPath must not be empty".to_string(),
        ));
    };

    let preserves_structured_feature_config =
        structured_feature_depth(segments).is_some_and(|feature_depth| {
            match value_at_path(root, &segments[..feature_depth]) {
                Some(TomlValue::Boolean(_)) => {
                    segments.len() > feature_depth || matches!(value, TomlValue::Table(_))
                }
                Some(TomlValue::Table(_)) => {
                    segments.len() == feature_depth && matches!(value, TomlValue::Boolean(_))
                }
                _ => false,
            }
        });

    if preserves_structured_feature_config
        || matches!(strategy, MergeStrategy::Upsert)
            && (credential_providers_path(segments).is_some()
                || shell_environment_policy_representation_switch(root, segments, value)
                || (matches!(value_at_path(root, segments), Some(TomlValue::Table(_)))
                    && matches!(value, TomlValue::Table(_))))
    {
        let overlay = sparse_overlay(segments, value);
        merge_toml_values(root, &overlay);
        return Ok(true);
    }

    let mut current = root;

    for segment in parents {
        if let TomlValue::Table(table) = current {
            current = table
                .entry(segment.clone())
                .or_insert_with(|| TomlValue::Table(toml::map::Map::new()));
        } else {
            *current = TomlValue::Table(toml::map::Map::new());
            if let TomlValue::Table(table) = current {
                current = table
                    .entry(segment.clone())
                    .or_insert_with(|| TomlValue::Table(toml::map::Map::new()));
            }
        }
    }

    let table = current.as_table_mut().ok_or_else(|| {
        MergeError::Validation("cannot set value on non-table parent".to_string())
    })?;

    let changed = table
        .get(last)
        .is_none_or(|existing| Some(existing) != Some(value));
    table.insert(last.clone(), value.clone());
    Ok(changed)
}

fn sparse_overlay(path: &[String], value: &TomlValue) -> TomlValue {
    path.iter().rev().fold(value.clone(), |value, segment| {
        TomlValue::Table(toml::map::Map::from_iter([(segment.clone(), value)]))
    })
}

fn shell_environment_policy_representation_switch(
    root: &TomlValue,
    segments: &[String],
    value: &TomlValue,
) -> bool {
    let current = root
        .get("shell_environment_policy")
        .and_then(ShellEnvironmentPolicyFilterRepresentation::from_policy);
    let edited = ShellEnvironmentPolicyFilterRepresentation::from_edit(segments, value);
    current
        .zip(edited)
        .is_some_and(|(current, edited)| current != edited)
}

fn clear_path(root: &mut TomlValue, segments: &[String]) -> Result<bool, MergeError> {
    let Some((last, parents)) = segments.split_last() else {
        return Err(MergeError::Validation(
            "keyPath must not be empty".to_string(),
        ));
    };

    let mut current = root;
    for segment in parents {
        match current {
            TomlValue::Table(table) => {
                let Some(next) = table.get_mut(segment) else {
                    return Ok(false);
                };
                current = next;
            }
            _ => return Ok(false),
        }
    }

    let Some(parent) = current.as_table_mut() else {
        return Ok(false);
    };

    Ok(parent.remove(last).is_some())
}

fn toml_value_to_item(value: &TomlValue) -> anyhow::Result<TomlItem> {
    match value {
        TomlValue::Table(table) => {
            let mut table_item = toml_edit::Table::new();
            table_item.set_implicit(false);
            for (key, val) in table {
                table_item.insert(key, toml_value_to_item(val)?);
            }
            Ok(TomlItem::Table(table_item))
        }
        other => Ok(TomlItem::Value(toml_value_to_value(other)?)),
    }
}

fn toml_value_to_value(value: &TomlValue) -> anyhow::Result<toml_edit::Value> {
    match value {
        TomlValue::String(val) => Ok(toml_edit::Value::from(val.clone())),
        TomlValue::Integer(val) => Ok(toml_edit::Value::from(*val)),
        TomlValue::Float(val) => Ok(toml_edit::Value::from(*val)),
        TomlValue::Boolean(val) => Ok(toml_edit::Value::from(*val)),
        TomlValue::Datetime(val) => Ok(toml_edit::Value::from(*val)),
        TomlValue::Array(items) => {
            let mut array = toml_edit::Array::new();
            for item in items {
                array.push(toml_value_to_value(item)?);
            }
            Ok(toml_edit::Value::Array(array))
        }
        TomlValue::Table(table) => {
            let mut inline = toml_edit::InlineTable::new();
            for (key, val) in table {
                inline.insert(key, toml_value_to_value(val)?);
            }
            Ok(toml_edit::Value::InlineTable(inline))
        }
    }
}

fn validate_config(value: &TomlValue) -> anyhow::Result<()> {
    let config: ConfigToml = value.clone().try_into()?;
    if config.approval_policy == Some(AskForApproval::UnlessTrusted) {
        anyhow::bail!(
            "approval_policy = \"untrusted\" is no longer supported; remove this setting"
        );
    }
    Ok(())
}

fn paths_match(expected: impl AsRef<Path>, provided: impl AsRef<Path>) -> bool {
    ra_paths::paths_match_after_normalization(expected, provided)
}

fn value_at_path<'a>(root: &'a TomlValue, segments: &[String]) -> Option<&'a TomlValue> {
    let mut current = root;
    for segment in segments {
        match current {
            TomlValue::Table(table) => {
                current = table.get(segment)?;
            }
            TomlValue::Array(items) => {
                let idx = segment.parse::<i64>().ok()?;
                let idx = usize::try_from(idx).ok()?;
                current = items.get(idx)?;
            }
            _ => return None,
        }
    }
    Some(current)
}

fn value_at_semantic_path<'a>(root: &'a TomlValue, segments: &[String]) -> Option<&'a TomlValue> {
    shell_environment_filter_entry(root, segments)
        .map(|(_, value)| value)
        .or_else(|| value_at_path(root, segments))
        .or_else(|| {
            let (field, parents) = segments.split_last()?;
            if field != "enabled" {
                return None;
            }
            if structured_feature_depth(parents) != Some(parents.len()) {
                return None;
            }
            let feature = value_at_path(root, parents)?;
            matches!(feature, TomlValue::Boolean(_)).then_some(feature)
        })
}

fn override_message(layer: &ConfigLayerSource) -> String {
    match layer {
        ConfigLayerSource::PackagedDefaults { file } => {
            format!("Overridden by packaged defaults: {}", file.display())
        }
        ConfigLayerSource::Mdm { domain, key: _ } => {
            format!("Overridden by managed policy (MDM): {domain}")
        }
        ConfigLayerSource::System { file } => {
            format!("Overridden by managed config (system): {}", file.display())
        }
        ConfigLayerSource::EnterpriseManaged { id: _, name } => {
            format!("Overridden by enterprise-managed config: {name}")
        }
        ConfigLayerSource::Project { dot_codex_folder } => format!(
            "Overridden by project config: {}/{CONFIG_TOML_FILE}",
            dot_codex_folder.display(),
        ),
        ConfigLayerSource::SessionFlags => "Overridden by session flags".to_string(),
        ConfigLayerSource::User { file, .. } => {
            format!("Overridden by user config: {}", file.display())
        }
        ConfigLayerSource::LegacyManagedConfigTomlFromFile { file } => {
            format!(
                "Overridden by legacy managed_config.toml: {}",
                file.display()
            )
        }
        ConfigLayerSource::LegacyManagedConfigTomlFromMdm => {
            "Overridden by legacy managed configuration from MDM".to_string()
        }
    }
}

fn compute_override_metadata(
    layers: &ConfigLayerStack,
    effective: &TomlValue,
    segments: &[String],
) -> Option<OverriddenMetadata> {
    let user_layer = layers.get_active_user_layer()?;
    let user_value = value_at_semantic_path(&user_layer.config, segments);
    let effective_value = value_at_semantic_path(effective, segments);

    if user_value.is_some() && user_value == effective_value {
        return None;
    }

    if user_value.is_none() && effective_value.is_none() {
        return None;
    }

    let overriding_layer = find_effective_layer(layers, segments)?;
    if overriding_layer.name.precedence() <= user_layer.name.precedence() {
        return None;
    }
    let message = override_message(&overriding_layer.name);

    Some(OverriddenMetadata {
        message,
        overriding_layer: overriding_layer.into(),
        effective_value: effective_value
            .and_then(|value| serde_json::to_value(value).ok())
            .unwrap_or(JsonValue::Null),
    })
}

fn first_overridden_edit(
    layers: &ConfigLayerStack,
    effective: &TomlValue,
    edits: &[Vec<String>],
) -> Option<OverriddenMetadata> {
    for segments in edits {
        if let Some(meta) = compute_override_metadata(layers, effective, segments) {
            return Some(meta);
        }
    }
    None
}

fn find_effective_layer(
    layers: &ConfigLayerStack,
    segments: &[String],
) -> Option<ConfigLayerMetadata> {
    if credential_providers_path(segments).is_some() {
        // A provider can be displaced by another provider's environment sources,
        // even when that layer never writes the edited path itself.
        let mut merged = TomlValue::Table(toml::map::Map::default());
        let mut origin = None;
        for layer in layers.layers_low_to_high() {
            let previous = value_at_path(&merged, segments).cloned();
            merge_toml_values(&mut merged, &layer.config);
            if value_at_path(&layer.config, segments).is_some()
                || previous.as_ref() != value_at_path(&merged, segments)
            {
                origin = Some(layer.metadata());
            }
        }
        return origin;
    }

    for layer in layers.layers_high_to_low() {
        if value_at_semantic_path(&layer.config, segments).is_some() {
            return Some(layer.metadata());
        }

        let Some(layer_representation) = layer
            .config
            .get("shell_environment_policy")
            .and_then(ShellEnvironmentPolicyFilterRepresentation::from_policy)
        else {
            continue;
        };
        if ShellEnvironmentPolicyFilterRepresentation::from_path(segments)
            .is_some_and(|edit_representation| edit_representation != layer_representation)
        {
            return Some(layer.metadata());
        }
    }

    None
}

/// Source unit-test entry points for the external integration-test workspace.
#[cfg(feature = "test-api")]
#[doc(hidden)]
pub mod test_api {
    use super::ConfigManagerError;
    use super::JsonValue;
    use super::MergeStrategy;
    use super::TomlItem;
    use super::TomlValue;

    /// Calls the private source key-path parser.
    pub fn parse_key_path(path: &str) -> Result<Vec<String>, String> {
        super::parse_key_path(path)
    }

    /// Calls the private source JSON-to-TOML value parser.
    pub fn parse_value(value: JsonValue) -> Result<Option<TomlValue>, String> {
        super::parse_value(value)
    }

    /// Calls the private source TOML item conversion.
    pub fn toml_value_to_item(value: &TomlValue) -> anyhow::Result<TomlItem> {
        super::toml_value_to_item(value)
    }

    /// Wraps the private source credential-provider batch state.
    pub struct CredentialProviderEdits(super::credential_edits::CredentialProviderEdits);

    impl CredentialProviderEdits {
        /// Captures provider definitions from the initial user config.
        #[must_use]
        pub fn new(config: &TomlValue) -> Self {
            Self(super::credential_edits::CredentialProviderEdits::new(
                config,
            ))
        }

        /// Applies one edit and returns the persistence edit it requires.
        pub fn apply(
            &mut self,
            config: &mut TomlValue,
            segments: &[String],
            value: Option<&TomlValue>,
            strategy: MergeStrategy,
        ) -> Result<Option<ra_config::ConfigEdit>, ConfigManagerError> {
            self.0.apply(config, segments, value, strategy)
        }

        /// Validates providers whose final definitions or sources changed in the batch.
        pub fn validate_remapping(
            &self,
            original_user_config: &TomlValue,
            user_config: &TomlValue,
            layers: &ra_config::ConfigLayerStack,
        ) -> anyhow::Result<()> {
            self.0
                .validate_remapping(original_user_config, user_config, layers)
        }

        /// Returns the edits that persist remapped provider definitions.
        pub fn remapping_edits(
            &self,
            config: &TomlValue,
        ) -> Result<Vec<ra_config::ConfigEdit>, ConfigManagerError> {
            self.0.remapping_edits(config)
        }
    }
}
