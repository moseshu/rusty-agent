//! A runner deadline, distinct from an error returned by a tool body.

use std::{fmt, time::Duration};

/// The timeout delivered to a function tool's timeout formatter.
#[derive(Debug)]
pub struct ToolTimeoutError {
    tool_name: String,
    timeout: Duration,
}

impl ToolTimeoutError {
    /// Records the configured invocation deadline.
    #[must_use]
    pub fn new(tool_name: impl Into<String>, timeout: Duration) -> Self {
        Self {
            tool_name: tool_name.into(),
            timeout,
        }
    }
    /// The function's unqualified public name.
    #[must_use]
    pub fn tool_name(&self) -> &str {
        &self.tool_name
    }
    /// The configured deadline in seconds.
    #[must_use]
    pub fn timeout_seconds(&self) -> f64 {
        self.timeout.as_secs_f64()
    }
}

impl fmt::Display for ToolTimeoutError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "Tool '{}' timed out after {} seconds.",
            self.tool_name,
            general_seconds(self.timeout_seconds())
        )
    }
}
impl std::error::Error for ToolTimeoutError {}

// Python's `:g` uses six significant digits and switches to scientific notation outside this
// exponent range. Formatting the rounded exponent first also covers a carry across a boundary.
fn general_seconds(seconds: f64) -> String {
    let scientific = format!("{seconds:.5e}");
    let (mantissa, exponent) = scientific.split_once('e').unwrap_or((&scientific, "0"));
    let exponent: i32 = exponent.parse().unwrap_or(0);
    if (-4..6).contains(&exponent) {
        let precision = usize::try_from(5 - exponent).unwrap_or(0);
        let fixed = format!("{seconds:.precision$}");
        if fixed.contains('.') {
            fixed.trim_end_matches('0').trim_end_matches('.').to_owned()
        } else {
            fixed
        }
    } else {
        format!(
            "{}e{exponent:+03}",
            mantissa.trim_end_matches('0').trim_end_matches('.')
        )
    }
}
