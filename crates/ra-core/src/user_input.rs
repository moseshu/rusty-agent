//! Questions a tool asks the user while its run waits: Codex's `request_user_input` protocol types
//! (`protocol/src/request_user_input.rs`) and the session call its tool makes
//! (`Session::request_user_input`).
//!
//! A run reaches the user through the [`UserInputPort`] its host installed in
//! [`ToolServices`](crate::tool::ToolServices); a host that can ask nobody installs none. The port
//! is to a run what Codex's session is to its tool: it asks the questions wherever the host shows
//! them, and hands back the answers, or nothing when the request ended without them.
//!
//! # Differences from Codex
//!
//! - The answers are kept in a `BTreeMap` rather than a `HashMap`: the same JSON object, written
//!   in a stable order.
//! - Codex's session also hands back the order in which the host accepted the answers, which only
//!   its Guardian reviewer reads; Guardian is not ported, so neither is the order.

use std::collections::BTreeMap;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::{context::RunContext, item::CallId};

/// One option a question offers: Codex's `RequestUserInputQuestionOption`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestUserInputQuestionOption {
    label: String,
    description: String,
}

impl RequestUserInputQuestionOption {
    /// The option `label`, explained by `description`.
    #[must_use]
    pub fn new(label: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            description: description.into(),
        }
    }

    /// The label.
    #[must_use]
    pub fn label(&self) -> &str {
        &self.label
    }

    /// The explanation.
    #[must_use]
    pub fn description(&self) -> &str {
        &self.description
    }
}

/// One question for the user: Codex's `RequestUserInputQuestion`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestUserInputQuestion {
    id: String,
    header: String,
    question: String,
    #[serde(rename = "isOther", default)]
    is_other: bool,
    #[serde(rename = "isSecret", default)]
    is_secret: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    options: Option<Vec<RequestUserInputQuestionOption>>,
}

impl RequestUserInputQuestion {
    /// The question `id`, headed `header`, asking `question`, without options.
    #[must_use]
    pub fn new(
        id: impl Into<String>,
        header: impl Into<String>,
        question: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            header: header.into(),
            question: question.into(),
            is_other: false,
            is_secret: false,
            options: None,
        }
    }

    /// Lets the user answer other than with an option.
    #[must_use]
    pub const fn with_is_other(mut self, is_other: bool) -> Self {
        self.is_other = is_other;
        self
    }

    /// Marks the answer secret, so the host hides it.
    #[must_use]
    pub const fn with_is_secret(mut self, is_secret: bool) -> Self {
        self.is_secret = is_secret;
        self
    }

    /// Sets the options offered.
    #[must_use]
    pub fn with_options(
        mut self,
        options: impl IntoIterator<Item = RequestUserInputQuestionOption>,
    ) -> Self {
        self.options = Some(options.into_iter().collect());
        self
    }

    /// The question's id, which its answer is keyed by.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The header.
    #[must_use]
    pub fn header(&self) -> &str {
        &self.header
    }

    /// The question.
    #[must_use]
    pub fn question(&self) -> &str {
        &self.question
    }

    /// Whether the user may answer other than with an option.
    #[must_use]
    pub const fn is_other(&self) -> bool {
        self.is_other
    }

    /// Whether the answer is secret.
    #[must_use]
    pub const fn is_secret(&self) -> bool {
        self.is_secret
    }

    /// The options offered, if any.
    #[must_use]
    pub fn options(&self) -> Option<&[RequestUserInputQuestionOption]> {
        self.options.as_deref()
    }
}

/// What a tool asks the user: Codex's `RequestUserInputArgs`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestUserInputArgs {
    questions: Vec<RequestUserInputQuestion>,
    #[serde(rename = "isBlocking")]
    is_blocking: bool,
    #[serde(rename = "autoResolutionMs", skip_serializing_if = "Option::is_none")]
    auto_resolution_ms: Option<u64>,
}

impl RequestUserInputArgs {
    /// Asks `questions`; `is_blocking` tells the host whether the user's work waits on them.
    #[must_use]
    pub fn new(
        questions: impl IntoIterator<Item = RequestUserInputQuestion>,
        is_blocking: bool,
    ) -> Self {
        Self {
            questions: questions.into_iter().collect(),
            is_blocking,
            auto_resolution_ms: None,
        }
    }

    /// Sets after how long the request resolves by itself. Codex deprecates it in favour of
    /// `isBlocking`.
    #[must_use]
    pub const fn with_auto_resolution_ms(mut self, auto_resolution_ms: u64) -> Self {
        self.auto_resolution_ms = Some(auto_resolution_ms);
        self
    }

    /// The questions.
    #[must_use]
    pub fn questions(&self) -> &[RequestUserInputQuestion] {
        &self.questions
    }

    /// Whether the user's work waits on the questions.
    #[must_use]
    pub const fn is_blocking(&self) -> bool {
        self.is_blocking
    }

    /// After how long the request resolves by itself, if it does.
    #[must_use]
    pub const fn auto_resolution_ms(&self) -> Option<u64> {
        self.auto_resolution_ms
    }
}

/// The user's answer to one question: Codex's `RequestUserInputAnswer`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestUserInputAnswer {
    answers: Vec<String>,
}

impl RequestUserInputAnswer {
    /// The answer made of `answers`.
    #[must_use]
    pub fn new(answers: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            answers: answers.into_iter().map(Into::into).collect(),
        }
    }

    /// What the user answered.
    #[must_use]
    pub fn answers(&self) -> &[String] {
        &self.answers
    }
}

/// The user's answers, keyed by question id: Codex's `RequestUserInputResponse`.
#[non_exhaustive]
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestUserInputResponse {
    answers: BTreeMap<String, RequestUserInputAnswer>,
}

impl RequestUserInputResponse {
    /// No answers.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds the answer to the question `id`.
    #[must_use]
    pub fn with_answer(mut self, id: impl Into<String>, answer: RequestUserInputAnswer) -> Self {
        self.answers.insert(id.into(), answer);
        self
    }

    /// The answers, by question id.
    #[must_use]
    pub const fn answers(&self) -> &BTreeMap<String, RequestUserInputAnswer> {
        &self.answers
    }
}

/// Where a run's questions for the user go: the half of Codex's session that its
/// `request_user_input` tool calls.
#[async_trait]
pub trait UserInputPort: Send + Sync {
    /// Asks the user `args` for the call `call_id` of `run`, and waits for the answers: Codex's
    /// `Session::request_user_input`.
    ///
    /// `None` means the request ended without answers — withdrawn because its turn moved on — as
    /// Codex's session returns nothing when its waiter is dropped. A request the user could not
    /// answer is answered with no answers, not `None`.
    async fn request_user_input(
        &self,
        run: &RunContext,
        call_id: &CallId,
        args: RequestUserInputArgs,
    ) -> Option<RequestUserInputResponse>;
}
