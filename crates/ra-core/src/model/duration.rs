//! Millisecond wire representation for durations.
//!
//! `Duration`'s own `Serialize` emits `{"secs": 0, "nanos": 100000000}`, which is a Rust-shaped
//! record: a host written in another language has to know how the two fields combine. These values
//! travel in config files and trace payloads that non-Rust hosts read, and the same requirement
//! already applies to [`SchemaVersion`](crate::compat::SchemaVersion), so durations go over the
//! wire as a plain integer count of milliseconds.
//!
//! Sub-millisecond precision is dropped on purpose. These are request timeouts and retry backoff
//! delays; no provider honours a nanosecond.

use std::time::Duration;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Serde adapter for `Option<Duration>` fields measured in milliseconds.
pub(crate) mod option_millis {
    use super::{Deserialize, Deserializer, Duration, Serialize, Serializer};

    // `&Option<T>` is not idiomatic, but `#[serde(with)]` dictates this signature.
    #[allow(clippy::ref_option)]
    pub(crate) fn serialize<S: Serializer>(
        value: &Option<Duration>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        // Saturating rather than failing: a duration this large is already nonsense, and a
        // serialization error here would take the whole enclosing record down with it.
        value
            .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
            .serialize(serializer)
    }

    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<Duration>, D::Error> {
        Ok(Option::<u64>::deserialize(deserializer)?.map(Duration::from_millis))
    }
}
