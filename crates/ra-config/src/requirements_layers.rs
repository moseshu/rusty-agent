// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
#![allow(
    missing_docs,
    reason = "Preserve source product configuration and validation records."
)]
mod hooks;
mod layer;
mod models;
mod permissions;
mod rules;
mod stack;

pub use layer::RequirementsLayerEntry;
pub(crate) use layer::strip_cloud_auth_requirements;
pub use stack::compose_requirements;
pub use stack::compose_requirements_for_hostname;

pub use stack::RequirementsCompositionError;
#[cfg(feature = "test-api")]
#[doc(hidden)]
pub use stack::test_api;
