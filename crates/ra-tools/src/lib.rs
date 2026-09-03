//! # `ra-tools`
//!
//! The tools every product needs and no product owns.
//!
//! **Boundary**: a tool belongs here when swapping the product out would not change a line of it —
//! only force it to be written again. `exec_command`, `write_stdin`, `read_file`, `grep`, `glob`,
//! `view_image`, `web_search`, `web_fetch`, `ask_user`, `update_plan`, `skill`, `tool_search`,
//! `apply_patch`, and the `agent.*` / `mcp.*` families all meet that test. What does not: a
//! product's editing discipline, command preferences, sandbox profile, and dangerous-command
//! judgement — those are product content and stay in the product crate.
//!
//! **`apply_patch` was the one entry originally excluded, and that call was wrong.** The reasoning
//! had been that "the V4A wrapper and the editing discipline around it" is what a second product
//! would write differently. Those are two things, and only the second one is product content:
//! applying a diff to a file is what a research agent revising a report needs too, and the module
//! that moved carries no editing discipline at all — no prompt text, no product branch, nothing but
//! a schema, a filesystem capability, and [`ra_patch`]. The discipline it was bundled with stayed
//! behind in the product crate, which is the evidence that the two were separable all along.
//!
//! **This crate exists before its contents on purpose.** Moving tools out later means touching
//! every guard and profile that has come to reference them; the cheap moment is before the first
//! one is written. Everything below therefore lands here directly rather than being relocated.
//!
//! **The module list is the complete advertised tool set**, and it is complete before the tools
//! are: a module that does not exist yet is a module the next implementer fills in somewhere else.
//! Most are still one-line stubs, which is a statement about what has been scheduled rather than
//! about where it belongs.
//!
//! [`capability`] is the one module that advertises nothing. It packages the tools below into the
//! installable units the assembly layer works in, which is where a fact such as "`write_stdin` can
//! only address a session `exec_command` started" becomes structural instead of something every
//! host has to know.
//!
//! **Boundary in the other direction**: these are tool *entry points*. The work itself belongs to
//! the service crates — process execution to [`ra_exec`], MCP transport and lifecycle to
//! [`ra_mcp`], V4A parsing and hunk application to [`ra_patch`] — and this crate only binds it to a
//! [`Tool`](ra_core::tool::Tool) implementation with a schema, an identity, and a model-facing
//! result contract.
//!
//! **Stability**: `Evolving`. The advertised tool set is still only partly implemented, and each
//! implemented tool's model-facing description is reviewed as a wire contract. A tool schema is a
//! wire format the moment a model sees it. The `Tool` trait it implements is `ra-core`'s and is
//! `Stable`.

pub mod agent_ns;
pub mod apply_patch;
pub mod ask_user;
pub mod capability;
pub mod exec_command;
pub mod glob;
pub mod grep;
pub mod mcp_ns;
pub mod read_file;
mod search;
pub mod skill;
pub mod tool_search;
pub mod update_plan;
pub mod view_image;
pub mod web;
pub mod write_stdin;
