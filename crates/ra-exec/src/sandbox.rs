//! Sandbox backends.
//!
//! A backend is gated twice, by feature and by platform: the feature says "I want this backend"
//! and `target_os` says "it only exists on this platform". A default build gets [`seatbelt`] on
//! macOS and [`bwrap`] on Linux, with [`unix_local`] as the baseline present on both.
//!
//! **No silent downgrade**: when a platform has no real sandbox backend available, construction
//! must fail and say what is missing. It must not quietly fall back to [`unix_local`] while the
//! caller believes it is isolated.

#[cfg(all(feature = "bwrap", target_os = "linux"))]
pub mod bwrap;
#[cfg(feature = "docker")]
pub mod docker;
pub mod manifest;
pub mod network;
#[cfg(all(feature = "seatbelt", target_os = "macos"))]
pub mod seatbelt;
pub mod unix_local;
