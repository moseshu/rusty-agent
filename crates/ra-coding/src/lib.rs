//! # `ra-coding`
//!
//! The coding agent (product layer): 15 tools, prompt content, coding discipline, assembly.
//!
//! **Boundary**: this is a consumer of the framework and assembles itself purely through the
//! public APIs of the other crates. It defines no reusable framework contract; downstream only
//! ever consumes an assembly entry such as `build_agent`, and the module tree stays crate-internal.
//!
//! **Stability**: `Internal`. **It is a reference product, not a framework contract** — prompts,
//! tool set, and discipline criteria all change with the business, and no crate may depend on it
//! (enforced by the layering gate).

pub(crate) mod capabilities;
pub(crate) mod closeout;
pub(crate) mod final_answer;
pub(crate) mod guards;
pub(crate) mod profile;
pub(crate) mod prompt;
pub(crate) mod tools;
