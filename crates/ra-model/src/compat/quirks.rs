//! Codec-level gateway differences: how a compatible endpoint's response has to be read.
//!
//! The other half of the same question — which request fields an endpoint accepts — lives on the
//! provider registration in [`ProviderQuirks`](crate::provider::quirks::ProviderQuirks), because
//! it is asked of every protocol and not only of compatible gateways. What is here is asked only
//! when reading, and only a compatible endpoint can answer it differently from the protocol.

use crate::openai::sse::Terminator;

/// How an endpoint says a streamed response is finished.
///
/// The protocol answer is `[DONE]`, and an adapter that guessed otherwise from what it happened to
/// receive would be inventing the one fact the check below exists to establish: a stream that ends
/// without any terminal signal is indistinguishable from a connection a proxy dropped mid-turn.
/// So the endpoint declares its own spelling, and the default assumes nothing unusual.
#[non_exhaustive]
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub enum DoneMarker {
    /// `data: [DONE]`, which both `OpenAI` protocols send.
    #[default]
    Standard,
    /// A different literal payload, sent in place of `[DONE]`.
    ///
    /// `[DONE]` stays recognized alongside it: an endpoint that sends both terminates either way,
    /// and no valid chunk could have been meant by that payload.
    ///
    /// The payload is compared after trimming, so the marker must be non-empty and carry no
    /// surrounding whitespace. Both are refused when the endpoint is built rather than at the
    /// stream that would misbehave: a blank marker would end the stream on any empty `data:` frame,
    /// and a padded one could never match at all.
    Literal(String),
    /// No terminator: this endpoint's stream ends when the body does.
    ///
    /// A missing terminator is normally still caught, because a terminal `finish_reason` is
    /// evidence too and nearly every endpoint sends one. Declaring this is for the endpoint that
    /// sends neither, and it is not free: for that endpoint a completed turn and a connection cut
    /// after the last forwarded chunk are the same bytes, so a truncated turn will settle as a
    /// successful one. A stream that delivered nothing at all is still refused.
    Absent,
}

impl DoneMarker {
    /// Lowers the declaration into the decoder's terminal-evidence rule.
    pub(crate) fn terminator(&self) -> Terminator {
        match self {
            Self::Standard => Terminator::default(),
            Self::Literal(marker) => Terminator::with_marker(marker.clone()),
            Self::Absent => Terminator::end_of_body(),
        }
    }
}
