//! Fixed-slot contract for a portable compaction summary.
//!
//! The summarizing model supplies the factual content, while this type owns the durable shape.
//! In particular, user messages travel as individual entries rather than being folded into a
//! prose paragraph, so a later summary cannot silently lose an instruction that was present in an
//! earlier turn.

use ra_core::error::{Error, Result};

/// One required section in a compaction summary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SummarySlot {
    /// The user's requested outcome and constraints.
    PrimaryRequestAndIntent,
    /// Technical terms, invariants, and architecture relevant to the work.
    KeyTechnicalConcepts,
    /// Files, modules, and code locations relevant to the work.
    FilesAndCodeSections,
    /// Observed errors and their confirmed or proposed fixes.
    ErrorsAndFixes,
    /// Reasoning already completed and decisions it produced.
    ProblemSolving,
    /// Every user message, retained as separate entries.
    AllUserMessages,
    /// Work that remains incomplete.
    PendingTasks,
    /// The precise state at the compaction boundary.
    CurrentWork,
    /// A safe next action, if one is known.
    OptionalNextStep,
}

impl SummarySlot {
    /// All slots in rendered order.
    pub const ALL: [Self; 9] = [
        Self::PrimaryRequestAndIntent,
        Self::KeyTechnicalConcepts,
        Self::FilesAndCodeSections,
        Self::ErrorsAndFixes,
        Self::ProblemSolving,
        Self::AllUserMessages,
        Self::PendingTasks,
        Self::CurrentWork,
        Self::OptionalNextStep,
    ];

    /// One-based stable position in the rendered summary.
    #[must_use]
    pub const fn number(self) -> usize {
        match self {
            Self::PrimaryRequestAndIntent => 1,
            Self::KeyTechnicalConcepts => 2,
            Self::FilesAndCodeSections => 3,
            Self::ErrorsAndFixes => 4,
            Self::ProblemSolving => 5,
            Self::AllUserMessages => 6,
            Self::PendingTasks => 7,
            Self::CurrentWork => 8,
            Self::OptionalNextStep => 9,
        }
    }

    /// Stable human-readable heading.
    #[must_use]
    pub const fn heading(self) -> &'static str {
        match self {
            Self::PrimaryRequestAndIntent => "Primary Request and Intent",
            Self::KeyTechnicalConcepts => "Key Technical Concepts",
            Self::FilesAndCodeSections => "Files and Code Sections",
            Self::ErrorsAndFixes => "Errors and Fixes",
            Self::ProblemSolving => "Problem Solving",
            Self::AllUserMessages => "All User Messages",
            Self::PendingTasks => "Pending Tasks",
            Self::CurrentWork => "Current Work",
            Self::OptionalNextStep => "Optional Next Step",
        }
    }

    const fn storage_index(self) -> Option<usize> {
        match self {
            Self::PrimaryRequestAndIntent => Some(0),
            Self::KeyTechnicalConcepts => Some(1),
            Self::FilesAndCodeSections => Some(2),
            Self::ErrorsAndFixes => Some(3),
            Self::ProblemSolving => Some(4),
            Self::AllUserMessages => None,
            Self::PendingTasks => Some(5),
            Self::CurrentWork => Some(6),
            Self::OptionalNextStep => Some(7),
        }
    }
}

/// Builder that requires every fixed summary slot before rendering.
#[derive(Debug, Default)]
pub struct CompactionSummaryBuilder {
    sections: [Option<String>; 8],
    user_messages: Option<Vec<String>>,
}

impl CompactionSummaryBuilder {
    /// Starts an empty fixed-slot summary.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Supplies content for one non-user-message summary slot.
    ///
    /// Blank content is refused. A slot with nothing to report has to say so in words — `None.`
    /// costs one line and survives being read back, while an empty slot is indistinguishable from
    /// a summary request that came back truncated or refused.
    pub fn with_section(mut self, slot: SummarySlot, content: impl Into<String>) -> Result<Self> {
        let Some(index) = slot.storage_index() else {
            return Err(Error::caller(
                "use with_user_messages to fill the All User Messages summary slot",
            ));
        };
        let content = content.into();
        if content.trim().is_empty() {
            return Err(Error::caller(format!(
                "the {} summary slot needs content; a slot with nothing to report states that, such as `None.`",
                slot.heading()
            )));
        }
        self.sections[index] = Some(content);
        Ok(self)
    }

    /// Supplies every user message in original chronological order.
    #[must_use]
    pub fn with_user_messages(mut self, messages: Vec<String>) -> Self {
        self.user_messages = Some(messages);
        self
    }

    /// Completes a summary after every one of its nine slots was explicitly supplied.
    ///
    /// A run with no user turns is representable, so an empty message list is accepted; a *blank*
    /// entry in that list is not. Slot 6 exists to keep every instruction the user actually gave,
    /// and an entry that renders as an empty fence loses one turn while still counting towards a
    /// complete-looking summary.
    pub fn build(self) -> Result<CompactionSummary> {
        let missing: Vec<&str> = SummarySlot::ALL
            .into_iter()
            .filter(|slot| match slot.storage_index() {
                Some(index) => self.sections[index].is_none(),
                None => self.user_messages.is_none(),
            })
            .map(SummarySlot::heading)
            .collect();
        if !missing.is_empty() {
            return Err(Error::caller(format!(
                "a compaction summary is missing required sections: {}",
                missing.join(", ")
            )));
        }

        let user_messages = self.user_messages.unwrap_or_default();
        if let Some(position) = user_messages
            .iter()
            .position(|message| message.trim().is_empty())
        {
            return Err(Error::caller(format!(
                "retained user message {} is blank; the All User Messages slot carries what the user actually said",
                position.saturating_add(1)
            )));
        }

        let sections = self.sections.map(|section| {
            // The preceding missing-slot check proves every entry is present.
            section.unwrap_or_default()
        });
        Ok(CompactionSummary {
            sections,
            user_messages,
        })
    }
}

/// Complete structured summary ready to become a compacted history item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactionSummary {
    sections: [String; 8],
    user_messages: Vec<String>,
}

impl CompactionSummary {
    /// Returns content for one non-user-message slot.
    #[must_use]
    pub fn section(&self, slot: SummarySlot) -> Option<&str> {
        slot.storage_index()
            .map(|index| self.sections[index].as_str())
    }

    /// Returns each user message separately and in original order.
    #[must_use]
    pub fn user_messages(&self) -> &[String] {
        &self.user_messages
    }

    /// Renders the fixed nine-section summary for a portable compaction record.
    ///
    /// User messages use individually sized Markdown code fences. A message therefore cannot turn
    /// a heading-looking line or its own backticks into summary structure, while its original text
    /// remains intact for the next model call.
    #[must_use]
    pub fn render(&self) -> String {
        let mut rendered = String::new();
        for slot in SummarySlot::ALL {
            if !rendered.is_empty() {
                rendered.push_str("\n\n");
            }
            rendered.push_str("## ");
            rendered.push_str(&slot.number().to_string());
            rendered.push(' ');
            rendered.push_str(slot.heading());
            rendered.push_str("\n\n");
            match slot.storage_index() {
                Some(index) => rendered.push_str(&self.sections[index]),
                None => render_user_messages(&mut rendered, &self.user_messages),
            }
        }
        rendered
    }
}

fn render_user_messages(rendered: &mut String, messages: &[String]) {
    if messages.is_empty() {
        rendered.push_str("- No user messages were recorded.");
        return;
    }

    for (index, message) in messages.iter().enumerate() {
        if index > 0 {
            rendered.push_str("\n\n");
        }
        let fence = code_fence(message);
        rendered.push_str(&(index + 1).to_string());
        rendered.push_str(".\n");
        rendered.push_str(&fence);
        rendered.push('\n');
        rendered.push_str(message);
        rendered.push('\n');
        rendered.push_str(&fence);
    }
}

/// Sizes a Markdown code fence that the enclosed text cannot terminate early.
///
/// Shared with [`crate::preflight`], which encloses verbatim user text for the same reason. A
/// second copy would let the two disagree about what a run of backticks costs, and the disagreement
/// would only show up as a model reading part of one section as another.
pub(crate) fn code_fence(text: &str) -> String {
    let mut longest = 0_usize;
    let mut run = 0_usize;
    for character in text.chars() {
        if character == '`' {
            run += 1;
            longest = longest.max(run);
        } else {
            run = 0;
        }
    }
    "`".repeat(longest.saturating_add(1).max(3))
}
