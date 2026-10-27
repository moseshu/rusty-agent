//! Typed compatibility for URL errors supplied by the upstream proxy fork.
use std::io;
use tokio_tungstenite::tungstenite::Error;
#[derive(Debug, thiserror::Error)]
enum ProxyError {
    #[error("proxy connection failed: {0}")]
    Connect(String),
    #[error("invalid proxy configuration: {0}")]
    Invalid(String),
    #[error("unsupported proxy scheme")]
    UnsupportedScheme,
}
pub(super) fn proxy_connect(message: String) -> Error {
    Error::Io(io::Error::other(ProxyError::Connect(message)))
}
pub(super) fn invalid_proxy_config(message: String) -> Error {
    Error::Io(io::Error::new(
        io::ErrorKind::InvalidInput,
        ProxyError::Invalid(message),
    ))
}
pub(super) fn unsupported_proxy_scheme() -> Error {
    Error::Io(io::Error::new(
        io::ErrorKind::Unsupported,
        ProxyError::UnsupportedScheme,
    ))
}
pub(super) fn is_unsupported_proxy_scheme(error: &Error) -> bool {
    matches!(error, Error::Io(error) if error.get_ref().and_then(|error| error.downcast_ref::<ProxyError>()).is_some_and(|error| matches!(error, ProxyError::UnsupportedScheme)))
}
