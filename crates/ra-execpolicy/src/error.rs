//! Parse and validation errors: Codex's `execpolicy/src/error.rs`.

use starlark::Error as StarlarkError;
use thiserror::Error;

/// A result whose error is a policy [`enum@Error`].
pub type Result<T> = std::result::Result<T, Error>;

/// A one-based line and column in a rules file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextPosition {
    /// One-based line.
    pub line: usize,
    /// One-based column.
    pub column: usize,
}

/// A span of a rules file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextRange {
    /// Where the span starts.
    pub start: TextPosition,
    /// Where the span ends.
    pub end: TextPosition,
}

/// Where in which rules file an error arose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorLocation {
    /// The identifier the file was parsed under, normally its path.
    pub path: String,
    /// The span within it.
    pub range: TextRange,
}

/// Why a rules file or rule was rejected.
#[derive(Debug, Error)]
pub enum Error {
    /// A decision other than `allow`, `prompt` or `forbidden`.
    #[error("invalid decision: {0}")]
    InvalidDecision(String),
    /// A pattern that is empty or holds something other than strings.
    #[error("invalid pattern element: {0}")]
    InvalidPattern(String),
    /// A `match` or `not_match` example that is not a command.
    #[error("invalid example: {0}")]
    InvalidExample(String),
    /// A rule whose arguments are invalid.
    #[error("invalid rule: {0}")]
    InvalidRule(String),
    /// A `match` example no rule of its `prefix_rule` call matches.
    #[error(
        "expected every example to match at least one rule. rules: {rules:?}; unmatched examples: \
         {examples:?}"
    )]
    ExampleDidNotMatch {
        /// The rules the call added.
        rules: Vec<String>,
        /// The examples none of them matched.
        examples: Vec<String>,
        /// Where the call is.
        location: Option<ErrorLocation>,
    },
    /// A `not_match` example a rule of its `prefix_rule` call matches.
    #[error("expected example to not match rule `{rule}`: {example}")]
    ExampleDidMatch {
        /// The rule that matched.
        rule: String,
        /// The example it matched.
        example: String,
        /// Where the call is.
        location: Option<ErrorLocation>,
    },
    /// The file failed to parse or evaluate as Starlark.
    #[error("starlark error: {0}")]
    Starlark(StarlarkError),
}

impl Error {
    /// Attaches `location` to an example error that has none.
    #[must_use]
    pub fn with_location(self, location: ErrorLocation) -> Self {
        match self {
            Error::ExampleDidNotMatch {
                rules,
                examples,
                location: None,
            } => Error::ExampleDidNotMatch {
                rules,
                examples,
                location: Some(location),
            },
            Error::ExampleDidMatch {
                rule,
                example,
                location: None,
            } => Error::ExampleDidMatch {
                rule,
                example,
                location: Some(location),
            },
            other => other,
        }
    }

    /// Where the error arose, when known.
    #[must_use]
    pub fn location(&self) -> Option<ErrorLocation> {
        match self {
            Error::ExampleDidNotMatch { location, .. }
            | Error::ExampleDidMatch { location, .. } => location.clone(),
            Error::Starlark(err) => err.span().map(|span| {
                let resolved = span.resolve_span();
                ErrorLocation {
                    path: span.filename().to_string(),
                    range: TextRange {
                        start: TextPosition {
                            line: resolved.begin.line + 1,
                            column: resolved.begin.column + 1,
                        },
                        end: TextPosition {
                            line: resolved.end.line + 1,
                            column: resolved.end.column + 1,
                        },
                    },
                }
            }),
            _ => None,
        }
    }
}
