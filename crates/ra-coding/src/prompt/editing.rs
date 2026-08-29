//! Editing guidance: which entry writes files, and what has to survive the agent's turn.
//!
//! The section has two halves with different scopes, which is why the builder takes both a role
//! and the advertised surface. Preserving a dirty worktree and staying off destructive Git is
//! role-neutral — any agent that reaches a shell can violate it. Naming `apply_patch` is not, and
//! it has to clear two separate bars. The tool must actually be advertised, or the text promises
//! an entry the provider request does not carry. And the role must not be one whose own guidance
//! says it has no editing tools: this section outranks the role section, so an unconditional "use
//! `apply_patch`" would be the instruction such an agent reads *first*, with the denial arriving
//! later.
//!
//! The slot is `editing_verification` and the verification half of that name is still unwritten.
//! It belongs in this builder when it lands: the assembler refuses a second claim on a registered
//! section name, so it cannot arrive as a module of its own.

use ra_core::error::Result;
use ra_core::prompt::{
    PromptRole, PromptSection, PromptSectionName, PromptSource, SectionPosition, SectionStability,
};

/// Builder for the coding agent's editing and worktree-safety prompt section.
pub(crate) struct EditingPromptBuilder;

const HEADING: &str = "Editing and Git Safety:\n";

/// Which entry writes files, and which mechanisms must not be reached for instead.
///
/// The prohibition and the carve-out are what make this a rule rather than a slogan. Without the
/// first, an agent holding an execution entry writes files through a heredoc and violates nothing.
/// Without the second, the rule reads as absolute and a formatter or codemod touching a hundred
/// files becomes a violation — a rule that gets ignored in ordinary work takes the credibility of
/// the surrounding section with it.
const EDITING_ENTRY: &str = "- Use `apply_patch` for direct workspace edits. It is the dedicated \
                             tool for creating, modifying, renaming, and deleting files.\n\
                             - Do not create or edit files with `cat`, heredocs, or other shell \
                             write tricks. Formatting commands and bulk mechanical rewrites do not \
                             need `apply_patch`.\n";

/// What has to survive the turn, whatever the role is allowed to run.
///
/// The named commands are the ones this product also detects at execution time, so the text and
/// the dangerous-action report point at the same operations rather than at two overlapping sets.
const WORKTREE_SAFETY: &str = "- Treat pre-existing changes in the worktree as the user's unless \
                               they are clearly part of the current work. Preserve unrelated \
                               changes and work carefully around overlapping edits.\n\
                               - Do not use destructive Git commands such as `git reset --hard`, \
                               `git clean`, or `git checkout --`. Do not discard, overwrite, or \
                               revert worktree changes unless the user explicitly asks for that \
                               exact operation.";

impl EditingPromptBuilder {
    /// Builds the stable section that constrains workspace edits and Git operations.
    pub(crate) fn build_editing_section(
        role: &PromptRole,
        apply_patch_is_advertised: bool,
    ) -> Result<PromptSection> {
        let mut content = String::from(HEADING);
        if names_the_editing_entry(role, apply_patch_is_advertised) {
            content.push_str(EDITING_ENTRY);
        }
        content.push_str(WORKTREE_SAFETY);

        PromptSection::new(
            PromptSectionName::EDITING_VERIFICATION,
            "Workspace edit discipline and Git worktree safety rules",
            PromptSource::Agent,
            SectionStability::Stable,
            SectionPosition::Prefix,
            content,
        )
    }
}

/// Whether this role's prefix should name the editing entry at all.
///
/// `Custom`, and any variant added upstream, follows its advertised tool surface. Their guidance
/// text is the caller's, so nothing here can tell whether they edit; the actual advertised tool
/// list decides whether the entry exists at all.
fn names_the_editing_entry(role: &PromptRole, apply_patch_is_advertised: bool) -> bool {
    apply_patch_is_advertised && !role.is_read_only() && !role.is_one_off()
}
