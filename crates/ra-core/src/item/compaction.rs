//! Context compaction items.

use core::fmt;
use std::borrow::Cow;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::ItemId;
use crate::{
    compat::{SchemaVersion, Unknown},
    error::{Error, Result},
    session::SessionId,
};

/// Current compaction schema version.
///
/// The archive fields arrived as optional additions under compatibility policy 2
/// (`#[serde(default)]`, omitted from the wire when absent), so a record written before them reads
/// back unchanged. Bumping the version for a purely additive field would make every already-stored
/// compaction report [`Compatibility::needs_migration`](crate::compat::Compatibility) against a
/// migration that does not exist and is not needed.
pub const COMPACTION_SCHEMA_VERSION: SchemaVersion = SchemaVersion::new(1);

/// Stable address of the authoritative records represented by a compaction.
///
/// The address names both the session that minted it and the compaction item that owns it. A
/// provider call ID is not sufficient here: it need only be unique within one provider
/// conversation, while archive lookup can cross resumed runs and separately stored sessions.
///
/// **An address is carried verbatim, never rejected on read.** One this build cannot take apart —
/// written in a form a newer build introduced, or percent encoded by a host that reserves different
/// characters — still round-trips through serialization and still selects its record by exact
/// match; only [`Self::session_id`] and [`Self::compaction_item_id`] report `None`. Failing the
/// read instead would fail the whole [`Compaction`], and through it the session record carrying it,
/// which is precisely what the compatibility rules in [`crate::compat`] exist to prevent: a build
/// must be able to open a session that contains something it does not understand.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ArchiveRef {
    value: String,
    target: Option<ArchiveTarget>,
}

/// The components of an address this build was able to take apart.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct ArchiveTarget {
    session_id: SessionId,
    compaction_item_id: ItemId,
}

impl ArchiveRef {
    /// Creates the archive address owned by one stored compaction item.
    pub fn new(session_id: &SessionId, compaction_item_id: &ItemId) -> Result<Self> {
        let session = session_id.as_str();
        let compaction_item = compaction_item_id.as_str();
        validate_component(session, "session ID")?;
        validate_component(compaction_item, "compaction item ID")?;

        Ok(Self {
            value: format!(
                "archive:v1/{}/{}",
                percent_encode(session),
                percent_encode(compaction_item)
            ),
            target: Some(ArchiveTarget {
                session_id: session_id.clone(),
                compaction_item_id: compaction_item_id.clone(),
            }),
        })
    }

    /// Opaque, serialized form of this reference.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.value
    }

    /// Session that minted the address, when this build can take the address apart.
    ///
    /// This is **provenance, not a lookup key**. History copied into another session — a grafted
    /// sub-agent transcript, a fork, an imported log — keeps the address it was written with, and
    /// resolves against whichever session actually stores the record. A host that keeps one store
    /// per session can use this to route a retrieval request; a host that finds nothing there
    /// should still try the session in hand.
    #[must_use]
    pub fn session_id(&self) -> Option<&SessionId> {
        self.target.as_ref().map(|target| &target.session_id)
    }

    /// Compaction item that defines the archived range, when this build can take the address apart.
    #[must_use]
    pub fn compaction_item_id(&self) -> Option<&ItemId> {
        self.target
            .as_ref()
            .map(|target| &target.compaction_item_id)
    }

    /// Reads a stored address without rejecting a form this build does not recognize.
    fn from_stored(value: String) -> Self {
        let target = parse_target(&value);
        Self { value, target }
    }
}

/// Takes a stored address apart, or reports that this build cannot.
fn parse_target(value: &str) -> Option<ArchiveTarget> {
    let encoded = value.strip_prefix("archive:v1/")?;
    // Three-way split rather than two: a third component means the address has a shape this build
    // does not know, which is not the same as a session ID that happens to contain a slash.
    let mut parts = encoded.splitn(3, '/');
    let (Some(session), Some(compaction_item), None) = (parts.next(), parts.next(), parts.next())
    else {
        return None;
    };

    let session_id = SessionId::new(percent_decode(session)?);
    let compaction_item_id = ItemId::new(percent_decode(compaction_item)?);
    // A non-canonical encoding addresses nothing this build would ever mint. Accepting `%2f` beside
    // `%2F` would let two references that compare unequal select the same record, and record
    // selection is exactly what equality decides.
    let canonical = ArchiveRef::new(&session_id, &compaction_item_id).ok()?;
    if canonical.value != value {
        return None;
    }
    Some(ArchiveTarget {
        session_id,
        compaction_item_id,
    })
}

impl Serialize for ArchiveRef {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ArchiveRef {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(Self::from_stored(String::deserialize(deserializer)?))
    }
}

impl fmt::Display for ArchiveRef {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

fn validate_component(value: &str, name: &str) -> Result<()> {
    if value.is_empty() || value.trim() != value || value.chars().any(char::is_control) {
        return Err(Error::caller(format!(
            "an archive reference {name} must be non-empty, trimmed, and contain no control characters"
        )));
    }
    Ok(())
}

fn percent_encode(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(char::from(byte));
        } else {
            encoded.push('%');
            encoded.push(hex_digit(byte >> 4));
            encoded.push(hex_digit(byte & 0x0f));
        }
    }
    encoded
}

fn hex_digit(value: u8) -> char {
    match value {
        0..=9 => char::from(b'0' + value),
        _ => char::from(b'A' + value - 10),
    }
}

fn percent_decode(value: &str) -> Option<String> {
    let mut bytes = Vec::with_capacity(value.len());
    let source = value.as_bytes();
    let mut index = 0_usize;
    while index < source.len() {
        if source[index] != b'%' {
            bytes.push(source[index]);
            index += 1;
            continue;
        }
        let high = source.get(index + 1).copied().and_then(hex_value)?;
        let low = source.get(index + 2).copied().and_then(hex_value)?;
        bytes.push((high << 4) | low);
        index += 3;
    }
    String::from_utf8(bytes).ok()
}

const fn hex_value(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'A'..=b'F' => Some(value - b'A' + 10),
        b'a'..=b'f' => Some(value - b'a' + 10),
        _ => None,
    }
}

/// The result of a provider-neutral compaction operation.
///
/// Provider-specific opaque compact items belong in [`super::RawProviderItem`]. This type stores
/// the portable summary and the session item IDs it covers, keeping local sessions independent of
/// any one server format.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Compaction {
    schema_version: SchemaVersion,
    summary: String,
    #[serde(default)]
    compacted_items: Vec<ItemId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    archive_ref: Option<ArchiveRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    archive_notice: Option<String>,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

impl Compaction {
    /// Creates a compaction summary.
    #[must_use]
    pub fn new(summary: impl Into<String>, compacted_items: Vec<ItemId>) -> Self {
        Self {
            schema_version: COMPACTION_SCHEMA_VERSION,
            summary: summary.into(),
            compacted_items,
            archive_ref: None,
            archive_notice: None,
            unknown: Unknown::new(),
        }
    }

    /// Attaches the address of the complete authoritative history this summary represents.
    ///
    /// `notice` is the model-visible retrieval instruction, and it belongs to the host because only
    /// the host knows whether the run exposes any way to retrieve an archive, under what tool name,
    /// and in which language. `None` records the address while telling the model nothing: an
    /// instruction to retrieve history that no tool can fetch buys either a tool call that cannot
    /// succeed or a claim that history was recovered when it was not.
    ///
    /// The notice is stored on the record rather than assembled while lowering a request, so that
    /// what the model was told is priced by
    /// `ContextUsage::estimate_model_input`, replayed verbatim by a resume, and visible to anyone
    /// reading the session.
    #[must_use]
    pub fn with_archive_ref(mut self, archive_ref: ArchiveRef, notice: Option<String>) -> Self {
        self.set_archive_ref(archive_ref, notice);
        self
    }

    /// Attaches the archive address in place; see [`Self::with_archive_ref`].
    pub fn set_archive_ref(&mut self, archive_ref: ArchiveRef, notice: Option<String>) {
        self.archive_ref = Some(archive_ref);
        self.archive_notice = notice;
    }

    /// Schema version.
    #[must_use]
    pub const fn schema_version(&self) -> SchemaVersion {
        self.schema_version
    }

    /// Portable summary.
    #[must_use]
    pub fn summary(&self) -> &str {
        &self.summary
    }

    /// Session item IDs covered by this summary.
    #[must_use]
    pub fn compacted_items(&self) -> &[ItemId] {
        &self.compacted_items
    }

    /// Address of the complete authoritative history, when this summary has been archived.
    #[must_use]
    pub const fn archive_ref(&self) -> Option<&ArchiveRef> {
        self.archive_ref.as_ref()
    }

    /// Host-authored retrieval instruction shown to the model beneath the summary.
    #[must_use]
    pub fn archive_notice(&self) -> Option<&str> {
        self.archive_notice.as_deref()
    }

    /// Summary text sent to the model.
    ///
    /// This never expands to archived records: an archived summary costs its own text plus whatever
    /// notice the host wrote, so context budgeting still governs every byte the model receives.
    #[must_use]
    pub fn model_text(&self) -> Cow<'_, str> {
        match &self.archive_notice {
            None => Cow::Borrowed(&self.summary),
            Some(notice) => Cow::Owned(format!("{}\n\n{notice}", self.summary)),
        }
    }

    /// Unknown fields retained during deserialization.
    #[must_use]
    pub const fn unknown(&self) -> &Unknown {
        &self.unknown
    }
}
