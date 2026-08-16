//! # `ra-coding`
//!
//! The coding agent (product layer): tools, prompt content, host assembly, and profile configuration.
//!
//! **Boundary**: this is a consumer of the framework and assembles itself purely through the
//! public APIs of the other crates. It defines no reusable framework contract; downstream only
//! consumes product assembly entries such as `CodingHost`, `CodingProfile`, and `build_agent`.
//!
//! **Stability**: `Internal`. **It is a reference product, not a framework contract** — prompts,
//! tool set, and discipline criteria all change with the business, and no crate may depend on it
//! (enforced by the layering gate).

pub(crate) mod capabilities;
pub(crate) mod closeout;
pub(crate) mod final_answer;
pub(crate) mod guards;
pub mod host;
pub mod profile;
pub(crate) mod prompt;
pub(crate) mod tools;

pub use host::CodingHost;
pub use profile::CodingProfile;
