// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Command-backed bearer credentials for product model providers.

use std::{path::Path, process::Stdio, time::Instant};

use ra_config::ModelProviderAuthInfo;
use ra_core::error::{Error, ProviderErrorKind, Result};
use ra_model::openai::responses::ResponsesBearerToken;
use tokio::{process::Command, sync::Mutex};

pub(super) struct CommandBearerToken {
    config: ModelProviderAuthInfo,
    cached: Mutex<Option<CachedToken>>,
}

struct CachedToken {
    value: String,
    fetched_at: Instant,
}

impl CommandBearerToken {
    pub(super) const fn new(config: ModelProviderAuthInfo) -> Self {
        Self {
            config,
            cached: Mutex::const_new(None),
        }
    }

    async fn run(&self) -> Result<String> {
        let config = &self.config;
        let name = &config.command;
        let command = Path::new(name);
        let program = if command.is_relative() && command.components().count() > 1 {
            config.cwd.as_path().join(command)
        } else {
            command.to_path_buf()
        };
        let mut command = Command::from(ra_exec::process::background_command(program));
        command
            .args(config.args.iter().map(|arg| arg.as_str()))
            .current_dir(config.cwd.as_path())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let result = tokio::time::timeout(config.timeout(), command.output())
            .await
            .map_err(|_| {
                auth_error(format!(
                    "provider auth command `{name}` timed out after {} ms",
                    config.timeout_ms,
                ))
            })?
            .map_err(|error| {
                auth_error(format!("provider auth command `{name}` failed to start"))
                    .with_source(error)
            })?;
        if !result.status.success() {
            return Err(auth_error(format!(
                "provider auth command `{name}` exited with {}",
                result.status
            )));
        }
        let token = String::from_utf8(result.stdout).map_err(|_| {
            auth_error(format!(
                "provider auth command `{name}` wrote non-UTF-8 data to stdout"
            ))
        })?;
        let token = token.trim();
        if token.is_empty() {
            return Err(auth_error(format!(
                "provider auth command `{name}` produced an empty token"
            )));
        }
        if http::HeaderValue::from_str(token).is_err() {
            return Err(auth_error(format!(
                "provider auth command `{name}` produced an invalid bearer token"
            )));
        }
        Ok(token.to_owned())
    }
}

#[async_trait::async_trait]
impl ResponsesBearerToken for CommandBearerToken {
    async fn token(&self, rejected: Option<&str>) -> Result<String> {
        // Keep concurrent requests on the same provider from running duplicate refresh commands.
        let mut cache = self.cached.lock().await;
        if let Some(cached) = cache.as_ref()
            && rejected != Some(cached.value.as_str())
            && self
                .config
                .refresh_interval()
                .is_none_or(|interval| cached.fetched_at.elapsed() < interval)
        {
            return Ok(cached.value.clone());
        }
        let value = self.run().await?;
        *cache = Some(CachedToken {
            value: value.clone(),
            fetched_at: Instant::now(),
        });
        Ok(value)
    }
}

fn auth_error(message: impl Into<String>) -> Error {
    Error::provider(ProviderErrorKind::Auth, message)
}
