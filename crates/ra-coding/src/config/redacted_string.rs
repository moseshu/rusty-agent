// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Strings that preserve serialization while keeping secrets out of debug output.

use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use std::fmt;
use std::ops::Deref;
use std::ops::DerefMut;

/// A string whose `Debug` output is redacted.
#[derive(Clone, Default, Deserialize, Serialize, PartialEq, Eq, JsonSchema)]
#[serde(transparent)]
pub struct RedactedString(String);

impl RedactedString {
    /// Returns the secret as an owned string; callers control any subsequent disclosure.
    #[must_use]
    pub fn into_inner(self) -> String {
        self.0
    }
}

impl Deref for RedactedString {
    type Target = String;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for RedactedString {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl From<String> for RedactedString {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for RedactedString {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

impl fmt::Debug for RedactedString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}
