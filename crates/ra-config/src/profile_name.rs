// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Validated user-profile selection.
use std::{fmt, ops::Deref, str::FromStr};

/// Validated plain profile-v2 name used to select `$RA_HOME/<name>.config.toml`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProfileV2Name(String);

impl ProfileV2Name {
    /// Returns the validated profile name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Rejected profile name with the original input for diagnostics.
#[derive(Debug, PartialEq, Eq)]
pub struct ProfileV2NameParseError {
    value: String,
}

impl fmt::Display for ProfileV2NameParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid --profile value `{}`; pass a plain name such as `work`",
            self.value
        )
    }
}

impl std::error::Error for ProfileV2NameParseError {}

impl FromStr for ProfileV2Name {
    type Err = ProfileV2NameParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.is_empty()
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        {
            return Err(ProfileV2NameParseError {
                value: value.to_string(),
            });
        }

        Ok(Self(value.to_string()))
    }
}

impl Deref for ProfileV2Name {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        self.as_str()
    }
}

impl fmt::Display for ProfileV2Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}
