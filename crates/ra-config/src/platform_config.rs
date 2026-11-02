//! Native platform facts required by product configuration bootstrap.
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub(crate) use macos::{load_managed_preference, synchronize_managed_preferences};
#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub(crate) use windows::*;

#[cfg(windows)]
mod windows_acl;
#[cfg(windows)]
pub(crate) use windows_acl::{
    path_has_standard_user_mutation_allow, path_has_trusted_system_owner,
    path_or_child_file_has_standard_user_mutation_allow,
};

#[cfg(all(target_os = "macos", feature = "test-api"))]
#[doc(hidden)]
pub use macos::test_load_managed_preference_with;
