//! Tail delta reminder attachments and runtime reminder channel.
//!
//! Following the attachment model, runtime reminders produce volatile tail messages
//! only and never mutate the stable system instruction prefix.

use std::fmt::{self, Write as _};

use ra_core::error::Result;
use ra_core::item::{Message, ModelInputItem};
use ra_core::prompt::{
    PromptSection, PromptSectionName, PromptSource, SectionPosition, SectionStability,
};
use serde::{Deserialize, Serialize};

/// A single delta attachment representing runtime state changes.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ReminderAttachment {
    /// Delta notification of newly added types or code lines.
    AgentListingDelta {
        /// Names of newly registered types or agents.
        added_types: Vec<String>,
        /// Number of newly added source lines.
        added_lines: usize,
    },
    /// Notification of system date/time change.
    DateChange {
        /// Current date/time string.
        new_date: String,
    },
    /// Compact summary reference for a long file.
    CompactFileReference {
        /// Path to the referenced file.
        path: String,
        /// Compact summary of the file contents.
        summary: String,
        /// Token count of the uncompacted file.
        original_tokens: usize,
    },
    /// Current task/TODO list delta.
    TodoReminder {
        /// Outstanding pending task descriptions.
        pending: Vec<String>,
        /// Completed task descriptions.
        completed: Vec<String>,
    },
    /// Custom key-value delta payload.
    CustomDelta {
        /// Identifier key for the delta.
        key: String,
        /// Content payload.
        content: String,
    },
}

/// Escapes text destined for an element body.
///
/// **Every value interpolated into a reminder is escaped, body as well as attribute.** The bodies
/// carry the larger and less trustworthy surface: a file summary, the model's own todo text, a
/// caller's custom payload. Left raw, a summary containing `</compact-file-reference>` followed by
/// `</system-reminder>` closes both tags, and everything after it reads to the model as the
/// framework speaking rather than as quoted file content — the reminder channel becomes a way for
/// the material it describes to issue instructions in the host's voice.
///
/// The cost is that a summary quoting code renders `<` as `&lt;`. That is the right side to lose
/// on: entities are unambiguous to read and merely ugly, while unescaped angle brackets are
/// indistinguishable from structure.
fn escape_text(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            other => escaped.push(other),
        }
    }
    escaped
}

/// Escapes text destined for an XML attribute value.
///
/// Everything [`escape_text`] handles, plus the quotes that would otherwise end the attribute
/// early and turn the remainder of a path or key into markup.
fn escape_attribute(value: &str) -> String {
    escape_text(value)
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

impl ReminderAttachment {
    /// Renders this attachment into a structured XML-like tag string.
    #[must_use]
    pub fn render_text(&self) -> String {
        match self {
            Self::AgentListingDelta {
                added_types,
                added_lines,
            } => {
                let types = added_types
                    .iter()
                    .map(|name| escape_text(name))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!(
                    "<agent-listing-delta added-lines=\"{added_lines}\">\n{types}\n</agent-listing-delta>"
                )
            }
            Self::DateChange { new_date } => {
                format!(
                    "<date-change current-date=\"{}\" />",
                    escape_attribute(new_date)
                )
            }
            Self::CompactFileReference {
                path,
                summary,
                original_tokens,
            } => {
                format!(
                    "<compact-file-reference path=\"{}\" original-tokens=\"{original_tokens}\">\n{}\n</compact-file-reference>",
                    escape_attribute(path),
                    escape_text(summary)
                )
            }
            Self::TodoReminder { pending, completed } => {
                let mut out = String::from("<todo-reminder>\n");
                if !completed.is_empty() {
                    out.push_str("  <completed>\n");
                    for item in completed {
                        let _ = writeln!(out, "    - {}", escape_text(item));
                    }
                    out.push_str("  </completed>\n");
                }
                if !pending.is_empty() {
                    out.push_str("  <pending>\n");
                    for item in pending {
                        let _ = writeln!(out, "    - {}", escape_text(item));
                    }
                    out.push_str("  </pending>\n");
                }
                out.push_str("</todo-reminder>");
                out
            }
            Self::CustomDelta { key, content } => {
                format!(
                    "<custom-delta key=\"{}\">\n{}\n</custom-delta>",
                    escape_attribute(key),
                    escape_text(content)
                )
            }
        }
    }
}

/// Collection of runtime reminders to be emitted at the tail of model input items.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeReminder {
    attachments: Vec<ReminderAttachment>,
}

impl RuntimeReminder {
    /// Creates an empty reminder collection.
    #[must_use]
    pub fn new() -> Self {
        Self {
            attachments: Vec::new(),
        }
    }

    /// Adds an attachment to the reminder.
    #[must_use]
    pub fn add_attachment(mut self, attachment: ReminderAttachment) -> Self {
        self.attachments.push(attachment);
        self
    }

    /// Adds an agent listing delta attachment.
    #[must_use]
    pub fn with_agent_listing_delta(
        mut self,
        added_types: Vec<String>,
        added_lines: usize,
    ) -> Self {
        self.attachments
            .push(ReminderAttachment::AgentListingDelta {
                added_types,
                added_lines,
            });
        self
    }

    /// Adds a date change attachment.
    #[must_use]
    pub fn with_date_change(mut self, new_date: impl Into<String>) -> Self {
        self.attachments.push(ReminderAttachment::DateChange {
            new_date: new_date.into(),
        });
        self
    }

    /// Adds a compact file reference attachment.
    #[must_use]
    pub fn with_compact_file_reference(
        mut self,
        path: impl Into<String>,
        summary: impl Into<String>,
        original_tokens: usize,
    ) -> Self {
        self.attachments
            .push(ReminderAttachment::CompactFileReference {
                path: path.into(),
                summary: summary.into(),
                original_tokens,
            });
        self
    }

    /// Adds a TODO reminder attachment.
    #[must_use]
    pub fn with_todo_reminder(mut self, pending: Vec<String>, completed: Vec<String>) -> Self {
        self.attachments
            .push(ReminderAttachment::TodoReminder { pending, completed });
        self
    }

    /// Adds a custom delta attachment.
    #[must_use]
    pub fn with_custom_delta(mut self, key: impl Into<String>, content: impl Into<String>) -> Self {
        self.attachments.push(ReminderAttachment::CustomDelta {
            key: key.into(),
            content: content.into(),
        });
        self
    }

    /// Returns whether there are no attachments.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.attachments.is_empty()
    }

    /// Returns the number of attachments.
    #[must_use]
    pub fn len(&self) -> usize {
        self.attachments.len()
    }

    /// Renders all attachments into a `<system-reminder>` XML container.
    #[must_use]
    pub fn render_text(&self) -> String {
        if self.attachments.is_empty() {
            return String::new();
        }
        let inner = self
            .attachments
            .iter()
            .map(ReminderAttachment::render_text)
            .collect::<Vec<_>>()
            .join("\n\n");
        format!("<system-reminder>\n{inner}\n</system-reminder>")
    }

    /// Converts the reminder into a tail message [`ModelInputItem`].
    #[must_use]
    pub fn to_input_item(&self) -> Option<ModelInputItem> {
        if self.is_empty() {
            return None;
        }
        let text = self.render_text();
        Some(ModelInputItem::Message(Message::user(text)))
    }

    /// Converts the reminder into a volatile [`PromptSection`].
    pub fn to_prompt_section(&self) -> Result<Option<PromptSection>> {
        if self.is_empty() {
            return Ok(None);
        }
        let text = self.render_text();
        let section = PromptSection::new(
            PromptSectionName::new("system_reminder"),
            "Volatile delta reminder for runtime state changes",
            PromptSource::Dynamic("reminder".to_string()),
            SectionStability::Volatile,
            SectionPosition::TailMessage,
            text,
        )?;
        Ok(Some(section))
    }

    /// Returns the slice of attachments.
    #[must_use]
    pub fn attachments(&self) -> &[ReminderAttachment] {
        &self.attachments
    }
}

impl fmt::Display for RuntimeReminder {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.render_text())
    }
}
