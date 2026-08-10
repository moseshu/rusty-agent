//! # `ra-tools`
//!
//! The tools every product needs and no product owns.
//!
//! **Boundary**: a tool belongs here when swapping the product out would not change a line of it —
//! only force it to be written again. `exec_command`, `write_stdin`, `read_file`, `grep`, `glob`,
//! `view_image`, `web_search`, `web_fetch`, `ask_user`, `update_plan`, `skill`, `tool_search`, and
//! the `agent.*` / `mcp.*` families all meet that test. What does not: the coding agent's
//! `apply_patch` wrapper and its editing discipline, command preferences, sandbox profile, and
//! dangerous-command judgement — those are product content and stay in the product crate (R2-12).
//!
//! **This crate exists before its contents on purpose.** R2-12's cost argument is that moving
//! tools out later means touching every guard and profile that has come to reference them; the
//! cheap moment is before the first one is written. Everything below therefore lands here
//! directly rather than being relocated.
//!
//! **The module list is R2-8's advertised set of 15 minus `apply_patch`**, and it is complete
//! before the tools are: a module that does not exist yet is a module the next implementer fills
//! in somewhere else. Most are still one-line stubs, which is a statement about scheduling
//! (R8-1..R8-6) rather than about where they belong.
//!
//! **Boundary in the other direction**: these are tool *entry points*. The work itself belongs to
//! the service crates — process execution to [`ra_exec`], MCP transport and lifecycle to
//! [`ra_mcp`] — and this crate only binds it to a [`Tool`](ra_core::tool::Tool) implementation
//! with a schema, an identity, and a model-facing result contract.
//!
//! **Stability**: `Evolving`. Which tools ship and what their schemas look like is exactly what
//! R2-8 through R2-11 are still deciding, and a tool schema is a wire format the moment a model
//! sees it. The `Tool` trait it implements is `ra-core`'s and is `Stable`.

pub mod agent_ns;
pub mod ask_user;
pub mod exec_command;
pub mod glob;
pub mod grep;
pub mod mcp_ns;
pub mod read_file;
pub mod skill;
pub mod tool_search;
pub mod update_plan;
pub mod view_image;
pub mod web;
pub mod write_stdin;
