//! # `ra-model`
//!
//! Provider implementations, streaming, retry, per-request usage.
//!
//! All three wire protocols are first-class citizens: `OpenAI` Responses, `OpenAI` Chat
//! Completions, and Anthropic Messages. Their capability differences are modeled explicitly in
//! [`protocol`], and the layer above (`ra-runtime`) must not assume the semantics of any one of
//! them.
//!
//! **Boundary**: the public surface exposes provider construction and registration, auth and
//! credential acquisition ([`openai::auth`], which the CLI login flow goes through), protocol
//! capabilities, retry facts, and usage. Request lowering, response conversion, SSE reassembly,
//! and vendor error-body mapping are all crate-internal codecs. It does not run the agent loop and
//! does not put wire-protocol-specific fields into `ra-core`.
//!
//! **Stability**: `Evolving`. Provider registrations and quirks grow as vendors are added and may
//! grow but not shrink; **each provider's codec is `Internal`** — once a protocol lowering
//! intermediate leaks out, this layer can no longer be refactored.

#[cfg(feature = "anthropic")]
pub mod anthropic;
#[cfg(feature = "compat")]
pub mod compat;
pub mod fallback;
#[cfg(feature = "openai")]
pub mod openai;
pub mod protocol;
pub mod provider;
pub mod retry;
pub mod usage;
