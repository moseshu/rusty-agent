//! What a manifest looks like when it is shown to a model.
//!
//! A tree, with each line's full path and description off to the right:
//!
//! ```text
//! /workspace
//! ├── repo/          # /workspace/repo — my repo
//! │   └── README.md  # /workspace/repo/README.md
//! ├── mount-data/    # /workspace/mount-data
//! └── notes.txt      # /workspace/notes.txt
//! ```
//!
//! This goes into a model's instructions, so it is bounded. Past the limit it is cut and the cut is
//! *stated*, with a line telling the model to `ls` rather than trust the listing — a silently
//! truncated tree is one a model will read as complete and then plan against.

use std::collections::BTreeMap;

use super::entries::{Entry, EntryContent};
use super::error::SandboxError;
use super::workspace_paths::PosixPath;

/// How much of a manifest description a model is shown before it is cut.
pub const MAX_MANIFEST_DESCRIPTION_CHARS: usize = 5000;

/// How many times the truncation marker is re-measured before settling.
///
/// The marker states how many characters were dropped, so its own length depends on that number,
/// which depends on the marker's length. The reference iterates to a fixed point with nothing
/// stopping it; two lengths that differ by one digit can trade places forever, so this settles for
/// the last measurement rather than spinning in a protocol crate.
const MARKER_MEASUREMENT_ROUNDS: usize = 8;

/// One node of the tree being rendered.
#[derive(Default)]
struct Node {
    children: BTreeMap<String, Node>,
    description: Option<String>,
    is_dir: bool,
    full_path: Option<PosixPath>,
}

/// Renders a manifest as a tree, cut to `max_chars`.
///
/// `depth` bounds how far down the tree is shown; `None` shows all of it. `max_chars` bounds the
/// rendered length; `None` leaves it unbounded.
///
/// # Errors
///
/// Returns a [`SandboxError`] when an entry path does not name somewhere inside the workspace, or a
/// mount attaches outside it.
pub fn render_manifest_description(
    root: &str,
    entries: &BTreeMap<String, Entry>,
    mount_paths: &BTreeMap<String, PosixPath>,
    depth: Option<usize>,
    max_chars: Option<usize>,
) -> Result<String, SandboxError> {
    let root_display = {
        let trimmed = root.trim_end_matches('/');
        if trimmed.is_empty() {
            "/".to_owned()
        } else {
            trimmed.to_owned()
        }
    };
    let root_path = PosixPath::coerce(&root_display);

    let mut tree = Node::default();
    for (declared, entry) in entries {
        let path = PosixPath::coerce(declared);
        // An absolute declaration is shown relative to the root; the tree has one root already.
        let path = if path.is_absolute() {
            PosixPath::new(
                path.parts()
                    .into_iter()
                    .skip(1)
                    .collect::<Vec<_>>()
                    .join("/"),
            )
        } else {
            path
        };
        insert_entry(&mut tree, &path, entry, mount_paths.get(declared), depth);
    }

    let mut lines = vec![root_display];
    let collected = collect(&tree, "", depth, &[], &root_path);
    if !collected.is_empty() {
        let width = collected
            .iter()
            .map(|line| line.prefix.chars().count() + line.name.chars().count())
            .max()
            .unwrap_or(0);
        for line in collected {
            let spacer =
                " ".repeat(width - line.prefix.chars().count() - line.name.chars().count() + 2);
            let comment = match &line.description {
                Some(description) => format!("# {} — {description}", line.full_path),
                None => format!("# {}", line.full_path),
            };
            lines.push(format!("{}{}{spacer}{comment}", line.prefix, line.name));
        }
    }

    let description = lines.join("\n") + "\n";
    Ok(truncate_manifest_description(&description, max_chars))
}

/// Inserts one entry, and everything declared inside it.
fn insert_entry(
    tree: &mut Node,
    path: &PosixPath,
    entry: &Entry,
    full_path: Option<&PosixPath>,
    depth: Option<usize>,
) {
    insert_path(
        tree,
        path,
        entry.description(),
        // The permission bits decide, not the `is_dir` flag: what is drawn with a trailing slash is
        // what the filesystem will hold a directory, and those are the bits that say so.
        entry.permissions().directory,
        full_path.cloned(),
        depth,
    );

    let EntryContent::Dir { children } = entry.content() else {
        return;
    };
    if depth.is_some_and(|depth| path.parts().len() >= depth) {
        return;
    }
    for (name, child) in children {
        let child_path = path.join(name);
        let child_full = full_path.map(|full_path| full_path.join(name));
        insert_entry(tree, &child_path, child, child_full.as_ref(), depth);
    }
}

/// Inserts one path, creating the nodes above it.
fn insert_path(
    tree: &mut Node,
    path: &PosixPath,
    description: Option<&str>,
    is_dir: bool,
    full_path: Option<PosixPath>,
    depth: Option<usize>,
) {
    let parts = path.parts();
    if parts.is_empty() {
        return;
    }
    let limit = depth.map_or(parts.len(), |depth| depth.min(parts.len()));

    let mut node = tree;
    for (index, part) in parts[..limit].iter().enumerate() {
        node = node.children.entry((*part).to_owned()).or_default();
        if index < parts.len() - 1 {
            // Something is declared below this, so it holds things whatever it says it is.
            node.is_dir = true;
        }
    }

    let complete = limit == parts.len();
    if node.description.is_none()
        && complete
        && let Some(description) = description
    {
        node.description = Some(description.to_owned());
    }
    if complete && full_path.is_some() {
        node.full_path = full_path;
    }
    if is_dir || !complete {
        node.is_dir = true;
    }
}

/// One rendered line, before it is padded.
struct Line {
    prefix: String,
    name: String,
    full_path: String,
    description: Option<String>,
}

/// Walks the tree, producing one line per node in the order it is drawn.
fn collect(
    node: &Node,
    prefix: &str,
    remaining: Option<usize>,
    rel_parts: &[String],
    root: &PosixPath,
) -> Vec<Line> {
    let mut lines = Vec::new();
    if remaining == Some(0) {
        return lines;
    }
    let next_remaining = remaining.map(|remaining| remaining - 1);

    let names: Vec<&String> = node.children.keys().collect();
    for (index, name) in names.iter().enumerate() {
        let child = &node.children[*name];
        let is_last = index + 1 == names.len();
        let connector = if is_last { "└── " } else { "├── " };
        let extension = if is_last { "    " } else { "│   " };

        let mut child_parts = rel_parts.to_vec();
        child_parts.push((*name).clone());

        let is_dir = child.is_dir || !child.children.is_empty();
        let full_path = child.full_path.as_ref().map_or_else(
            || root.join(&child_parts.join("/")).to_string(),
            PosixPath::to_string,
        );
        lines.push(Line {
            prefix: format!("{prefix}{connector}"),
            name: if is_dir {
                format!("{name}/")
            } else {
                (*name).clone()
            },
            full_path,
            description: child.description.clone(),
        });

        if next_remaining.is_none_or(|remaining| remaining > 0) {
            lines.extend(collect(
                child,
                &format!("{prefix}{extension}"),
                next_remaining,
                &child_parts,
                root,
            ));
        }
    }
    lines
}

/// Cuts a description to `max_chars`, saying that it was cut.
///
/// `Internal`, as the reference's `_truncate_manifest_description` is: public, and hidden from the
/// documentation, only so the separate test workspace can reach it.
#[doc(hidden)]
#[must_use]
pub fn truncate_manifest_description(description: &str, max_chars: Option<usize>) -> String {
    let Some(max_chars) = max_chars else {
        return description.to_owned();
    };
    if description.len() <= max_chars {
        return description.to_owned();
    }
    if max_chars == 0 {
        return String::new();
    }

    let mut omitted = description.len() - max_chars;
    let mut marker = truncation_marker(omitted);
    let mut keep = max_chars.saturating_sub(marker.len());
    for _ in 0..MARKER_MEASUREMENT_ROUNDS {
        let measured = description.len() - keep;
        if measured == omitted {
            break;
        }
        omitted = measured;
        marker = truncation_marker(omitted);
        keep = max_chars.saturating_sub(marker.len());
    }

    if marker.len() >= max_chars {
        return take_chars(&marker, max_chars);
    }
    let truncated = format!("{}{marker}", take_chars(description, keep).trim_end());
    if truncated.len() > max_chars {
        return take_chars(&truncated, max_chars);
    }
    truncated
}

/// The note that replaces what was cut.
fn truncation_marker(omitted: usize) -> String {
    format!(
        "\n... (truncated {omitted} chars)\n\nThe filesystem layout above was truncated. \
         Use `ls` to explore specific directories before relying on omitted paths.\n"
    )
}

/// Takes at most `budget` bytes, without splitting a character in half.
///
/// The budget is in bytes because the limit it serves is a byte limit, and a tree can hold any
/// filename a filesystem does. Cutting mid-character would produce a string Rust cannot hold.
fn take_chars(text: &str, budget: usize) -> String {
    if text.len() <= budget {
        return text.to_owned();
    }
    let mut end = budget;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}
