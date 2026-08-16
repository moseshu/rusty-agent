//! Event timestamps with millisecond precision and stable serialization.

use std::{
    fmt,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A millisecond-precision UTC timestamp for host events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EventTimestamp(u64);

impl EventTimestamp {
    /// Returns the current system time as an [`EventTimestamp`].
    #[must_use]
    pub fn now() -> Self {
        Self::from_system_time(SystemTime::now())
    }

    /// Creates a timestamp from milliseconds since the Unix epoch.
    #[must_use]
    pub const fn from_millis(millis: u64) -> Self {
        Self(millis)
    }

    /// Converts a [`SystemTime`] into an [`EventTimestamp`].
    #[must_use]
    pub fn from_system_time(time: SystemTime) -> Self {
        let millis = time
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::ZERO)
            .as_millis();
        let clamped = u64::try_from(millis).unwrap_or(u64::MAX);
        Self(clamped)
    }

    /// Converts this timestamp into a [`SystemTime`].
    #[must_use]
    pub fn to_system_time(self) -> SystemTime {
        UNIX_EPOCH + Duration::from_millis(self.0)
    }

    /// Returns milliseconds since the Unix epoch.
    #[must_use]
    pub const fn as_millis(self) -> u64 {
        self.0
    }
}

impl Default for EventTimestamp {
    fn default() -> Self {
        Self::from_millis(0)
    }
}

impl From<SystemTime> for EventTimestamp {
    fn from(time: SystemTime) -> Self {
        Self::from_system_time(time)
    }
}

impl From<EventTimestamp> for SystemTime {
    fn from(timestamp: EventTimestamp) -> Self {
        timestamp.to_system_time()
    }
}

impl fmt::Display for EventTimestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}ms", self.0)
    }
}

impl Serialize for EventTimestamp {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_u64(self.0)
    }
}

impl<'de> Deserialize<'de> for EventTimestamp {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let millis = u64::deserialize(deserializer)?;
        Ok(Self::from_millis(millis))
    }
}
