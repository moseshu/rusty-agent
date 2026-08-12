//! Provider-neutral message content blocks.
//!
//! # Message content carries content, not tool calls
//!
//! A tool call is **never** a content block here. It lives exactly once, as
//! [`RunItemKind::ToolCall`](super::RunItemKind::ToolCall) /
//! [`ToolCallOutput`](super::RunItemKind::ToolCallOutput) at the item level, paired by
//! [`CallId`](super::CallId).
//!
//! Both reference implementations converge on this shape:
//!
//! | Wire format | Where a tool call lives |
//! | --- | --- |
//! | `OpenAI` Responses | a top-level output item, sibling to the message |
//! | `OpenAI` Chat | `tool_calls`, a sibling field of `content` |
//! | Anthropic Messages | a `tool_use` block **inside** message content |
//!
//! Anthropic is the outlier, and that is a wire detail: splitting an inbound message into a
//! [`Message`](super::Message) plus separate tool-call items — and merging them again on the way
//! out — is exactly the lowering work that belongs to the `ra-model` codec, with the untouched
//! original kept in [`RawProviderItem`](super::RawProviderItem) for replay.
//!
//! Modelling both shapes in `ra-core` would mean every consumer has to look in two places for the
//! same fact. Pairing and orphan pruning walk item-level `CallId`s; a call hidden inside message
//! content would be silently invisible to them.
//!
//! # Refusals are not text
//!
//! [`Refusal`](ContentBlock::Refusal) is a distinct block rather than prose in a text block,
//! because model fallback escalates to a stronger model on refusal. That decision needs a
//! mechanical signal, not a string match against whatever wording the provider chose.
//!
//! Provider adapters lower these blocks into their wire representation. Local image paths stay
//! inert data in `ra-core`; reading and encoding the file belongs to an I/O layer such as
//! `ra-model` or a tool implementation.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::compat::{SchemaVersion, Unknown};

/// Current content-block schema version.
pub const CONTENT_BLOCK_SCHEMA_VERSION: SchemaVersion = SchemaVersion::new(1);

/// A provider-neutral message content block.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum ContentBlock {
    /// UTF-8 text.
    Text(TextBlock),
    /// Model reasoning together with its replay signature.
    Thinking(ThinkingBlock),
    /// An image supplied inline or through a local path.
    Image(ImageBlock),
    /// A refusal to produce the requested output.
    Refusal(RefusalBlock),
}

impl ContentBlock {
    /// Creates a text block.
    #[must_use]
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text(TextBlock::new(text))
    }

    /// Creates a thinking block.
    #[must_use]
    pub fn thinking(thinking: impl Into<String>, signature: impl Into<String>) -> Self {
        Self::Thinking(ThinkingBlock::new(thinking, signature))
    }

    /// Creates an image block.
    #[must_use]
    pub fn image(source: ImageSource) -> Self {
        Self::Image(ImageBlock::new(source))
    }

    /// Creates an inline base64 image block.
    #[must_use]
    pub fn image_base64(media_type: impl Into<String>, data: impl Into<String>) -> Self {
        Self::image(ImageSource::base64(media_type, data))
    }

    /// Creates a local-path image block.
    #[must_use]
    pub fn image_path(path: impl Into<PathBuf>) -> Self {
        Self::image(ImageSource::local_path(path))
    }

    /// Creates a refusal block.
    #[must_use]
    pub fn refusal(refusal: impl Into<String>) -> Self {
        Self::Refusal(RefusalBlock::new(refusal))
    }

    /// Stable machine-readable discriminator.
    #[must_use]
    pub const fn label(&self) -> &'static str {
        match self {
            Self::Text(_) => "text",
            Self::Thinking(_) => "thinking",
            Self::Image(_) => "image",
            Self::Refusal(_) => "refusal",
        }
    }

    /// Returns the text when this is a text block.
    ///
    /// A refusal is deliberately not text: surfacing it here would let it be concatenated into
    /// ordinary output and lose the signal model fallback needs.
    #[must_use]
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text(block) => Some(block.text()),
            _ => None,
        }
    }

    /// Returns the block when this is a thinking block.
    #[must_use]
    pub const fn as_thinking(&self) -> Option<&ThinkingBlock> {
        match self {
            Self::Thinking(block) => Some(block),
            _ => None,
        }
    }

    /// Returns the block when this is an image block.
    #[must_use]
    pub const fn as_image(&self) -> Option<&ImageBlock> {
        match self {
            Self::Image(block) => Some(block),
            _ => None,
        }
    }

    /// Returns the refusal explanation when this is a refusal block.
    #[must_use]
    pub fn as_refusal(&self) -> Option<&str> {
        match self {
            Self::Refusal(block) => Some(block.refusal()),
            _ => None,
        }
    }
}

/// A text content block.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextBlock {
    schema_version: SchemaVersion,
    text: String,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

impl TextBlock {
    /// Creates a text block.
    #[must_use]
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            schema_version: CONTENT_BLOCK_SCHEMA_VERSION,
            text: text.into(),
            unknown: Unknown::new(),
        }
    }

    /// Schema version.
    #[must_use]
    pub const fn schema_version(&self) -> SchemaVersion {
        self.schema_version
    }

    /// Text content.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Unknown fields retained during deserialization.
    #[must_use]
    pub const fn unknown(&self) -> &Unknown {
        &self.unknown
    }
}

/// A model-thinking content block.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThinkingBlock {
    schema_version: SchemaVersion,
    thinking: String,
    signature: String,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

impl ThinkingBlock {
    /// Creates a thinking block with the opaque provider replay signature.
    #[must_use]
    pub fn new(thinking: impl Into<String>, signature: impl Into<String>) -> Self {
        Self {
            schema_version: CONTENT_BLOCK_SCHEMA_VERSION,
            thinking: thinking.into(),
            signature: signature.into(),
            unknown: Unknown::new(),
        }
    }

    /// Schema version.
    #[must_use]
    pub const fn schema_version(&self) -> SchemaVersion {
        self.schema_version
    }

    /// Visible model reasoning.
    #[must_use]
    pub fn thinking(&self) -> &str {
        &self.thinking
    }

    /// Opaque provider signature required to replay the thinking block.
    ///
    /// It must be resent byte for byte. Rewriting or dropping it invalidates the block.
    #[must_use]
    pub fn signature(&self) -> &str {
        &self.signature
    }

    /// Unknown fields retained during deserialization.
    #[must_use]
    pub const fn unknown(&self) -> &Unknown {
        &self.unknown
    }
}

/// A refusal to produce the requested output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefusalBlock {
    schema_version: SchemaVersion,
    refusal: String,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

impl RefusalBlock {
    /// Creates a refusal block.
    #[must_use]
    pub fn new(refusal: impl Into<String>) -> Self {
        Self {
            schema_version: CONTENT_BLOCK_SCHEMA_VERSION,
            refusal: refusal.into(),
            unknown: Unknown::new(),
        }
    }

    /// Schema version.
    #[must_use]
    pub const fn schema_version(&self) -> SchemaVersion {
        self.schema_version
    }

    /// Provider-supplied refusal explanation.
    #[must_use]
    pub fn refusal(&self) -> &str {
        &self.refusal
    }

    /// Unknown fields retained during deserialization.
    #[must_use]
    pub const fn unknown(&self) -> &Unknown {
        &self.unknown
    }
}

/// An image content block.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageBlock {
    schema_version: SchemaVersion,
    source: ImageSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    detail: Option<ImageDetail>,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

impl ImageBlock {
    /// Creates an image block.
    #[must_use]
    pub fn new(source: ImageSource) -> Self {
        Self {
            schema_version: CONTENT_BLOCK_SCHEMA_VERSION,
            source,
            detail: None,
            unknown: Unknown::new(),
        }
    }

    /// Sets how much vision detail the provider should spend.
    #[must_use]
    pub const fn with_detail(mut self, detail: ImageDetail) -> Self {
        self.detail = Some(detail);
        self
    }

    /// Schema version.
    #[must_use]
    pub const fn schema_version(&self) -> SchemaVersion {
        self.schema_version
    }

    /// Image data location.
    #[must_use]
    pub const fn source(&self) -> &ImageSource {
        &self.source
    }

    /// Requested vision detail, when the producer asked for one.
    #[must_use]
    pub const fn detail(&self) -> Option<ImageDetail> {
        self.detail
    }

    /// Unknown fields retained during deserialization.
    #[must_use]
    pub const fn unknown(&self) -> &Unknown {
        &self.unknown
    }
}

/// Storage used by an image block.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum ImageSource {
    /// Image bytes already encoded as base64.
    Base64(Base64ImageSource),
    /// A local file path to be read by an I/O-capable layer.
    LocalPath(LocalImageSource),
    /// A URL the provider fetches, including a `data:` URL.
    Url(UrlSource),
    /// A file already uploaded to the provider, referenced by its identifier.
    ProviderFile(ProviderFileSource),
}

impl ImageSource {
    /// Creates an inline base64 image source.
    #[must_use]
    pub fn base64(media_type: impl Into<String>, data: impl Into<String>) -> Self {
        Self::Base64(Base64ImageSource::new(media_type, data))
    }

    /// Creates a local-path image source.
    #[must_use]
    pub fn local_path(path: impl Into<PathBuf>) -> Self {
        Self::LocalPath(LocalImageSource::new(path))
    }

    /// Creates a URL source.
    #[must_use]
    pub fn url(url: impl Into<String>) -> Self {
        Self::Url(UrlSource::new(url))
    }

    /// Creates a provider-file source.
    #[must_use]
    pub fn provider_file(file_id: impl Into<String>) -> Self {
        Self::ProviderFile(ProviderFileSource::new(file_id))
    }

    /// Returns the source when it contains inline base64 data.
    #[must_use]
    pub const fn as_base64(&self) -> Option<&Base64ImageSource> {
        match self {
            Self::Base64(source) => Some(source),
            _ => None,
        }
    }

    /// Returns the source when it contains a local path.
    #[must_use]
    pub const fn as_local_path(&self) -> Option<&LocalImageSource> {
        match self {
            Self::LocalPath(source) => Some(source),
            _ => None,
        }
    }

    /// Returns the source when it is a URL.
    #[must_use]
    pub const fn as_url(&self) -> Option<&UrlSource> {
        match self {
            Self::Url(source) => Some(source),
            _ => None,
        }
    }

    /// Returns the source when it references an uploaded provider file.
    #[must_use]
    pub const fn as_provider_file(&self) -> Option<&ProviderFileSource> {
        match self {
            Self::ProviderFile(source) => Some(source),
            _ => None,
        }
    }
}

/// Vision detail a provider should spend on an image.
///
/// A cost knob, not a content field: `High` buys resolution with tokens. Absent means the provider
/// decides, which is the right default because the ceiling differs per model.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageDetail {
    /// Fewest tokens.
    Low,
    /// Most tokens, most resolution.
    High,
    /// Provider's choice.
    Auto,
}

impl ImageDetail {
    /// Stable machine-readable label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::High => "high",
            Self::Auto => "auto",
        }
    }
}

impl core::fmt::Display for ImageDetail {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.label())
    }
}

/// A URL the provider fetches for itself.
///
/// Kept opaque on purpose: a `data:` URL and an `https:` URL are the same thing to every adapter
/// that forwards it, and parsing one here would only produce a second opinion about a string the
/// provider is the authority on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UrlSource {
    schema_version: SchemaVersion,
    url: String,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

impl UrlSource {
    /// Creates a URL source.
    #[must_use]
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            schema_version: CONTENT_BLOCK_SCHEMA_VERSION,
            url: url.into(),
            unknown: Unknown::new(),
        }
    }

    /// Schema version.
    #[must_use]
    pub const fn schema_version(&self) -> SchemaVersion {
        self.schema_version
    }

    /// The URL, forwarded verbatim.
    #[must_use]
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Unknown fields retained during deserialization.
    #[must_use]
    pub const fn unknown(&self) -> &Unknown {
        &self.unknown
    }
}

/// A file already uploaded to the provider.
///
/// The identifier is provider-scoped, so a record carrying one is only replayable against the
/// provider that minted it. That is a property of the reference, not a defect: the alternative is
/// re-uploading bytes the provider already has.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderFileSource {
    schema_version: SchemaVersion,
    file_id: String,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

impl ProviderFileSource {
    /// Creates a provider-file reference.
    #[must_use]
    pub fn new(file_id: impl Into<String>) -> Self {
        Self {
            schema_version: CONTENT_BLOCK_SCHEMA_VERSION,
            file_id: file_id.into(),
            unknown: Unknown::new(),
        }
    }

    /// Schema version.
    #[must_use]
    pub const fn schema_version(&self) -> SchemaVersion {
        self.schema_version
    }

    /// Provider-scoped file identifier.
    #[must_use]
    pub fn file_id(&self) -> &str {
        &self.file_id
    }

    /// Unknown fields retained during deserialization.
    #[must_use]
    pub const fn unknown(&self) -> &Unknown {
        &self.unknown
    }
}

/// Storage used by a file block.
///
/// Mirrors [`ImageSource`] rather than reusing it: a file carries a `filename` the model reads as
/// a hint, and an image carries a media type and a detail level. One enum covering both would make
/// every consumer handle fields that cannot apply.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum FileSource {
    /// Inline base64 file bytes.
    Base64(Base64FileSource),
    /// A URL the provider fetches.
    Url(UrlSource),
    /// A file already uploaded to the provider.
    ProviderFile(ProviderFileSource),
}

impl FileSource {
    /// Creates an inline base64 file source.
    #[must_use]
    pub fn base64(data: impl Into<String>) -> Self {
        Self::Base64(Base64FileSource::new(data))
    }

    /// Creates a URL source.
    #[must_use]
    pub fn url(url: impl Into<String>) -> Self {
        Self::Url(UrlSource::new(url))
    }

    /// Creates a provider-file source.
    #[must_use]
    pub fn provider_file(file_id: impl Into<String>) -> Self {
        Self::ProviderFile(ProviderFileSource::new(file_id))
    }
}

/// Inline base64 file data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Base64FileSource {
    schema_version: SchemaVersion,
    data: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    filename: Option<String>,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

impl Base64FileSource {
    /// Creates an inline source. Encoding validation is deferred to the provider adapter.
    #[must_use]
    pub fn new(data: impl Into<String>) -> Self {
        Self {
            schema_version: CONTENT_BLOCK_SCHEMA_VERSION,
            data: data.into(),
            filename: None,
            unknown: Unknown::new(),
        }
    }

    /// Sets the name the model sees for this file.
    #[must_use]
    pub fn with_filename(mut self, filename: impl Into<String>) -> Self {
        self.filename = Some(filename.into());
        self
    }

    /// Schema version.
    #[must_use]
    pub const fn schema_version(&self) -> SchemaVersion {
        self.schema_version
    }

    /// Base64-encoded file bytes without a data-URL prefix.
    #[must_use]
    pub fn data(&self) -> &str {
        &self.data
    }

    /// Name hint, when one was supplied.
    #[must_use]
    pub fn filename(&self) -> Option<&str> {
        self.filename.as_deref()
    }

    /// Unknown fields retained during deserialization.
    #[must_use]
    pub const fn unknown(&self) -> &Unknown {
        &self.unknown
    }
}

/// A file content block.
///
/// Deliberately **not** a [`ContentBlock`] variant yet: whether a message may carry a file is the
/// model-protocol layer's contract to change, and adding the variant later costs nothing because
/// that enum is `#[non_exhaustive]`. Tool results are the only producer today.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileBlock {
    schema_version: SchemaVersion,
    source: FileSource,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

impl FileBlock {
    /// Creates a file block.
    #[must_use]
    pub fn new(source: FileSource) -> Self {
        Self {
            schema_version: CONTENT_BLOCK_SCHEMA_VERSION,
            source,
            unknown: Unknown::new(),
        }
    }

    /// Schema version.
    #[must_use]
    pub const fn schema_version(&self) -> SchemaVersion {
        self.schema_version
    }

    /// File data location.
    #[must_use]
    pub const fn source(&self) -> &FileSource {
        &self.source
    }

    /// Unknown fields retained during deserialization.
    #[must_use]
    pub const fn unknown(&self) -> &Unknown {
        &self.unknown
    }
}

/// Inline base64 image data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Base64ImageSource {
    schema_version: SchemaVersion,
    media_type: String,
    data: String,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

impl Base64ImageSource {
    /// Creates an inline source. Encoding validation is deferred to the provider adapter.
    #[must_use]
    pub fn new(media_type: impl Into<String>, data: impl Into<String>) -> Self {
        Self {
            schema_version: CONTENT_BLOCK_SCHEMA_VERSION,
            media_type: media_type.into(),
            data: data.into(),
            unknown: Unknown::new(),
        }
    }

    /// Schema version.
    #[must_use]
    pub const fn schema_version(&self) -> SchemaVersion {
        self.schema_version
    }

    /// MIME media type, for example `image/png`.
    #[must_use]
    pub fn media_type(&self) -> &str {
        &self.media_type
    }

    /// Base64-encoded image bytes without a data-URL prefix.
    #[must_use]
    pub fn data(&self) -> &str {
        &self.data
    }

    /// Unknown fields retained during deserialization.
    #[must_use]
    pub const fn unknown(&self) -> &Unknown {
        &self.unknown
    }
}

/// A local image path.
///
/// The path must be valid UTF-8: `PathBuf`'s `Serialize` fails on anything else, and that failure
/// would surface as the whole session record failing to persist rather than as one bad block. Use
/// [`LocalImageSource::try_new`] when the path comes from the filesystem rather than from a
/// literal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalImageSource {
    schema_version: SchemaVersion,
    path: PathBuf,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

impl LocalImageSource {
    /// Creates a local-path source without reading the file.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            schema_version: CONTENT_BLOCK_SCHEMA_VERSION,
            path: path.into(),
            unknown: Unknown::new(),
        }
    }

    /// Creates a local-path source, rejecting a path that could not be serialized later.
    ///
    /// Returns the original path back to the caller on rejection, so the diagnostic can name it.
    pub fn try_new(path: impl Into<PathBuf>) -> Result<Self, PathBuf> {
        let path = path.into();
        if path.to_str().is_none() {
            return Err(path);
        }
        Ok(Self::new(path))
    }

    /// Schema version.
    #[must_use]
    pub const fn schema_version(&self) -> SchemaVersion {
        self.schema_version
    }

    /// Local filesystem path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Unknown fields retained during deserialization.
    #[must_use]
    pub const fn unknown(&self) -> &Unknown {
        &self.unknown
    }
}
