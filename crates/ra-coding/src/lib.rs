//! # `ra-coding`
//!
//! The coding agent (product layer): prompt content, editing discipline, host assembly, dangerous
//! -action facts, and profile configuration.
//!
//! **It owns no tool.** Every entry it advertises, `apply_patch` included, comes from [`ra_tools`];
//! what this crate contributes is which of them are installed, what capability each is handed, and
//! what the prompt says about using them.
//!
//! **Boundary**: this is a consumer of the framework and assembles itself purely through the
//! public APIs of the other crates. It defines no reusable framework contract; downstream only
//! consumes product assembly entries such as `CodingHost`, `CodingProfile`, and `build_agent`.
//!
//! **Stability**: `Internal`. **It is a reference product, not a framework contract** — prompts,
//! tool set, and discipline criteria all change with the business, and no crate may depend on it
//! (enforced by the layering gate).

pub mod agent;
pub(crate) mod capabilities;
pub(crate) mod closeout;
pub mod dangerous_action;
pub(crate) mod final_answer;
pub(crate) mod guards;
pub mod host;
pub mod profile;
pub mod prompt;

pub use agent::{build_agent, build_agent_with_host};
pub use host::CodingHost;
pub use profile::CodingProfile;
