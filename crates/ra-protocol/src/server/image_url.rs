//! Which image URLs the server takes from a client: Codex's `app-server/src/image_url.rs`.
//!
//! An image reaches the model inline, as a `data:` URL; one the provider would have to fetch over
//! the network is refused, in user input and in what a dynamic tool returns alike.

/// Codex's `REMOTE_IMAGE_URL_ERROR`.
pub(crate) const REMOTE_IMAGE_URL_ERROR: &str =
    "remote image URLs are not supported; use an inline data URL instead";

/// Whether `image_url` is fetched over the network rather than inline: Codex's
/// `is_remote_image_url`.
pub(crate) fn is_remote_image_url(image_url: &str) -> bool {
    image_url.split_once(':').is_some_and(|(scheme, _)| {
        scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https")
    })
}
