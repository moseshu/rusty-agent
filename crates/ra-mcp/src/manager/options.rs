//! Lifecycle configuration corresponding to Python's manager keyword arguments.

use ra_core::error::{Error, Result};
use std::time::Duration;

/// MCP manager configuration, with the Python lifecycle defaults.
#[derive(Clone, Debug)]
#[allow(clippy::struct_excessive_bools)]
pub struct McpServerManagerOptions {
    pub(super) connect_timeout: Option<Duration>,
    pub(super) cleanup_timeout: Option<Duration>,
    pub(super) drop_failed_servers: bool,
    pub(super) strict: bool,
    pub(super) suppress_cancelled_error: bool,
    pub(super) connect_in_parallel: bool,
}

impl Default for McpServerManagerOptions {
    fn default() -> Self {
        Self {
            connect_timeout: Some(Duration::from_secs(10)),
            cleanup_timeout: Some(Duration::from_secs(10)),
            drop_failed_servers: true,
            strict: false,
            suppress_cancelled_error: true,
            connect_in_parallel: false,
        }
    }
}

impl McpServerManagerOptions {
    /// The configured startup deadline, or `None` when startup is unbounded.
    #[must_use]
    pub const fn connect_timeout(&self) -> Option<Duration> {
        self.connect_timeout
    }

    /// Sets the connect deadline; `None` disables it and zero is rejected.
    pub fn with_connect_timeout(mut self, timeout: Option<Duration>) -> Result<Self> {
        validate_timeout(timeout)?;
        self.connect_timeout = timeout;
        Ok(self)
    }

    /// Sets the cleanup deadline independently of the connect deadline.
    pub fn with_cleanup_timeout(mut self, timeout: Option<Duration>) -> Result<Self> {
        validate_timeout(timeout)?;
        self.cleanup_timeout = timeout;
        Ok(self)
    }

    /// Whether failed servers are excluded from the active view; defaults to true.
    #[must_use]
    pub fn with_drop_failed_servers(mut self, enabled: bool) -> Self {
        self.drop_failed_servers = enabled;
        self
    }

    /// Whether connection failures propagate after recording; defaults to false.
    #[must_use]
    pub fn with_strict(mut self, enabled: bool) -> Self {
        self.strict = enabled;
        self
    }

    /// Whether server-returned cancellation errors are suppressed; defaults to true.
    /// Dropping a Rust future cannot be suppressed and does not stop its worker's cleanup.
    #[must_use]
    pub fn with_suppress_cancelled_error(mut self, enabled: bool) -> Self {
        self.suppress_cancelled_error = enabled;
        self
    }

    /// Whether different servers connect concurrently; defaults to false.
    /// Sequential connects also use workers to retain affinity across host tasks.
    #[must_use]
    pub fn with_connect_in_parallel(mut self, enabled: bool) -> Self {
        self.connect_in_parallel = enabled;
        self
    }
}

pub(super) fn validate_timeout(timeout: Option<Duration>) -> Result<()> {
    if timeout.is_some_and(|value| {
        value.is_zero() || tokio::time::Instant::now().checked_add(value).is_none()
    }) {
        return Err(Error::config(
            "MCP lifecycle timeout must be positive and representable, or None",
        ));
    }
    Ok(())
}
