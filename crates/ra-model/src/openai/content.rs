//! Content lowering shared by the two `OpenAI` protocols.
//!
//! Responses and Chat Completions disagree about the *shape* of a content part — `input_image`
//! against `image_url` — but not about how a neutral source becomes a wire value: base64 bytes and
//! a local file both become a `data:` URL, an uploaded file stays a reference. That resolution
//! involves filesystem I/O and media-type inference, and a second copy of it is what makes a
//! base64 image work on one protocol and fail on the other.

use std::path::Path;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use ra_core::{
    error::{Error, Result},
    item::ImageSource,
};
use serde_json::Value;

use super::error::behavior_error;

/// An image source resolved to what a wire field can hold.
pub(crate) enum ResolvedImage {
    /// A URL the provider fetches, including an inline `data:` URL.
    Url(String),
    /// A file already uploaded to the provider.
    ProviderFile(String),
}

/// Reads and encodes an image source, leaving the wire shape to the caller.
pub(crate) async fn resolve_image(source: &ImageSource) -> Result<ResolvedImage> {
    match source {
        ImageSource::Base64(source) => Ok(ResolvedImage::Url(format!(
            "data:{};base64,{}",
            source.media_type(),
            source.data()
        ))),
        ImageSource::LocalPath(source) => {
            let bytes = tokio::fs::read(source.path()).await.map_err(|error| {
                Error::caller(format!(
                    "could not read local image `{}`",
                    source.path().display()
                ))
                .with_source(error)
            })?;
            Ok(ResolvedImage::Url(format!(
                "data:{};base64,{}",
                infer_media_type(source.path()),
                STANDARD.encode(bytes)
            )))
        }
        ImageSource::Url(source) => Ok(ResolvedImage::Url(source.url().to_owned())),
        ImageSource::ProviderFile(source) => {
            Ok(ResolvedImage::ProviderFile(source.file_id().to_owned()))
        }
        _ => Err(Error::caller("unsupported image source for OpenAI")),
    }
}

/// Guesses a media type from a file extension, defaulting to PNG.
pub(crate) fn infer_media_type(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(std::ffi::OsStr::to_str)
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        _ => "image/png",
    }
}

/// Renders a host-supplied tool payload that predates the structured tool-output contract.
pub(crate) fn stringify_tool_output(output: &Value) -> Result<String> {
    output.as_str().map_or_else(
        || {
            serde_json::to_string(output).map_err(|error| {
                behavior_error("tool output could not be serialized").with_source(error)
            })
        },
        |text| Ok(text.to_owned()),
    )
}
