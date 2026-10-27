// Adapted from openai-oss-forks/tungstenite-rs (MIT) @ 4fffad30fe373adbdcffab9545e9e9bf4f2fc19f, modified.
//! Proxy support for blocking client connections.
//!
//! Async users can reuse `ProxyConfig::from_env` to resolve proxy settings and then
//! establish proxy streams with their preferred async crates.

#![allow(
    clippy::trivially_copy_pass_by_ref,
    reason = "Keep source protocol helper signatures unchanged."
)]
#![allow(
    clippy::unnecessary_wraps,
    reason = "Keep the source fallible API shared across platforms."
)]
use std::env;

use http::Uri;

use super::proxy_error::{invalid_proxy_config, proxy_connect, unsupported_proxy_scheme};
use tokio_tungstenite::tungstenite::{Error, Result, error::UrlError, stream::Mode};

/// Proxy scheme supported by tungstenite.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ProxyScheme {
    /// HTTP CONNECT proxy.
    Http,
    /// SOCKS5 proxy with remote DNS resolution.
    Socks5,
    /// SOCKS5 proxy with local DNS resolution.
    Socks5h,
}

/// Proxy authentication credentials.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ProxyAuth {
    /// Username for basic or SOCKS5 auth.
    pub username: String,
    /// Password for basic or SOCKS5 auth.
    pub password: String,
}

/// Resolved proxy configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ProxyConfig {
    /// Proxy scheme.
    pub scheme: ProxyScheme,
    /// Proxy host.
    pub host: String,
    /// Proxy port.
    pub port: u16,
    /// Proxy authentication credentials.
    pub auth: Option<ProxyAuth>,
}

impl ProxyConfig {
    /// Resolve proxy configuration for the given WebSocket URI using environment variables.
    ///
    /// Honors `HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY`, and `NO_PROXY` (case-insensitive).
    /// Supported proxy schemes: `http://`, `socks5://`, `socks5h://`.
    pub(super) fn from_env(uri: &Uri) -> Result<Option<Self>> {
        let mode = tokio_tungstenite::tungstenite::client::uri_mode(uri)?;
        let host = uri.host().ok_or(Error::Url(UrlError::NoHostName))?;
        let port = uri.port_u16().unwrap_or(match mode {
            Mode::Plain => 80,
            Mode::Tls => 443,
        });

        proxy_from_env_for_host(host, port, mode)
    }

    /// Parse a proxy configuration from a proxy URL.
    pub(super) fn parse(value: &str) -> Result<Self> {
        parse_proxy_config(value)
    }

    /// Render the proxy authority as `host:port`.
    pub(super) fn authority(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

fn proxy_from_env_for_host(host: &str, port: u16, mode: Mode) -> Result<Option<ProxyConfig>> {
    if should_bypass_proxy(host, port)? {
        return Ok(None);
    }

    let proxy = match mode {
        Mode::Plain => get_env_first(&["HTTP_PROXY", "http_proxy"]),
        Mode::Tls => get_env_first(&["HTTPS_PROXY", "https_proxy"])
            .or_else(|| get_env_first(&["HTTP_PROXY", "http_proxy"])),
    }
    .or_else(|| get_env_first(&["ALL_PROXY", "all_proxy"]));

    let Some(proxy) = proxy else {
        return Ok(None);
    };

    parse_proxy_config(&proxy).map(Some)
}

fn get_env_first(keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| env::var(key).ok())
        .filter(|value| !value.is_empty())
}

fn should_bypass_proxy(host: &str, port: u16) -> Result<bool> {
    let no_proxy = get_env_first(&["NO_PROXY", "no_proxy"]);
    let Some(no_proxy) = no_proxy else {
        return Ok(false);
    };

    let host = normalize_host(host);
    let no_proxy = no_proxy.trim();
    if no_proxy.is_empty() {
        return Ok(false);
    }
    if no_proxy == "*" {
        return Ok(true);
    }

    for token in no_proxy.split(',') {
        let token = token.trim();
        if token.is_empty() {
            continue;
        }

        let (token_host, token_port) = split_host_port(token);
        if let Some(token_port) = token_port
            && token_port != port
        {
            continue;
        }

        let token_host = normalize_host(token_host);

        if host == token_host {
            return Ok(true);
        }

        if let Some(token_host) = token_host.strip_prefix('.') {
            if host == token_host || host.ends_with(&format!(".{token_host}")) {
                return Ok(true);
            }
        } else if host.ends_with(&format!(".{token_host}")) {
            return Ok(true);
        }
    }

    Ok(false)
}

fn split_host_port(token: &str) -> (&str, Option<u16>) {
    let token = token.trim();
    if token.starts_with('[') {
        if let Some(close) = token.find(']') {
            let host = &token[..=close];
            let remainder = &token[close + 1..];
            if let Some(port) = remainder.strip_prefix(':') {
                return (host, port.parse().ok());
            }
            return (host, None);
        }
        return (token, None);
    }

    if token.matches(':').count() == 1
        && let Some((host, port)) = token.rsplit_once(':')
    {
        return (host, port.parse().ok());
    }

    (token, None)
}

fn normalize_host(host: &str) -> &str {
    host.strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host)
}

fn parse_proxy_config(value: &str) -> Result<ProxyConfig> {
    let value = value.trim();
    let uri: Uri = value
        .parse()
        .map_err(|_| invalid_proxy_config(value.into()))?;

    let scheme = match uri.scheme_str() {
        Some("http") => ProxyScheme::Http,
        Some("socks5") => ProxyScheme::Socks5,
        Some("socks5h") => ProxyScheme::Socks5h,
        Some(_) | None => return Err(unsupported_proxy_scheme()),
    };

    let authority = uri
        .authority()
        .ok_or_else(|| invalid_proxy_config(value.into()))?
        .as_str();

    let (userinfo, hostport) = split_userinfo(authority);
    let (host, port) = parse_host_port(hostport, &scheme)?;

    let auth = userinfo.map(parse_userinfo).transpose()?;

    Ok(ProxyConfig {
        scheme,
        host,
        port,
        auth,
    })
}

fn split_userinfo(authority: &str) -> (Option<&str>, &str) {
    let mut iter = authority.rsplitn(2, '@');
    let hostport = iter.next().unwrap_or(authority);
    let userinfo = iter.next();
    (userinfo, hostport)
}

fn parse_host_port(hostport: &str, scheme: &ProxyScheme) -> Result<(String, u16)> {
    let uri: Uri = format!("http://{hostport}")
        .parse()
        .map_err(|_| invalid_proxy_config(hostport.into()))?;

    let host = uri
        .host()
        .ok_or_else(|| invalid_proxy_config(hostport.into()))?
        .to_string();

    let port = uri.port_u16().unwrap_or(match scheme {
        ProxyScheme::Http => 80,
        ProxyScheme::Socks5 | ProxyScheme::Socks5h => 1080,
    });

    Ok((host, port))
}

fn parse_userinfo(userinfo: &str) -> Result<ProxyAuth> {
    let (user, pass) = userinfo.split_once(':').unwrap_or((userinfo, ""));
    let username = percent_decode(user)?;
    let password = percent_decode(pass)?;
    Ok(ProxyAuth { username, password })
}

fn percent_decode(value: &str) -> Result<String> {
    let mut output = Vec::with_capacity(value.len());
    let mut chars = value.as_bytes().iter().copied();
    while let Some(byte) = chars.next() {
        if byte == b'%' {
            let hi = chars
                .next()
                .ok_or_else(|| invalid_proxy_config(value.into()))?;
            let lo = chars
                .next()
                .ok_or_else(|| invalid_proxy_config(value.into()))?;
            let decoded = (from_hex(hi)? << 4) | from_hex(lo)?;
            output.push(decoded);
        } else {
            output.push(byte);
        }
    }
    String::from_utf8(output).map_err(|_| invalid_proxy_config(value.into()))
}

fn from_hex(byte: u8) -> Result<u8> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err(invalid_proxy_config("invalid percent-encoding".into())),
    }
}

fn basic_auth_header(auth: &ProxyAuth) -> Result<String> {
    let token = format!("{}:{}", auth.username, auth.password);
    let encoded = data_encoding::BASE64.encode(token.as_bytes());
    Ok(format!("Basic {encoded}"))
}

/// Build the bytes for an HTTP CONNECT request.
pub(super) fn build_http_connect_request(
    authority: &str,
    auth: Option<&ProxyAuth>,
) -> Result<Vec<u8>> {
    let mut request = Vec::new();
    request.extend_from_slice(format!("CONNECT {authority} HTTP/1.1\r\n").as_bytes());
    request.extend_from_slice(format!("Host: {authority}\r\n").as_bytes());
    request.extend_from_slice(b"Proxy-Connection: Keep-Alive\r\n");
    if let Some(auth) = auth {
        let token = basic_auth_header(auth)?;
        request.extend_from_slice(format!("Proxy-Authorization: {token}\r\n").as_bytes());
    }
    request.extend_from_slice(b"\r\n");
    Ok(request)
}

/// Parse an HTTP CONNECT response and return the status code.
pub(super) fn parse_http_connect_response(response: &[u8]) -> Result<u16> {
    let text = std::str::from_utf8(response)
        .map_err(|_| proxy_connect("HTTP CONNECT response not valid UTF-8".into()))?;

    let mut lines = text.lines();
    let status_line = lines
        .next()
        .ok_or_else(|| proxy_connect("HTTP CONNECT response missing status line".into()))?;

    let mut parts = status_line.split_whitespace();
    let _version = parts.next();
    let code = parts
        .next()
        .ok_or_else(|| proxy_connect("HTTP CONNECT response missing status code".into()))?;
    code.parse::<u16>()
        .map_err(|_| proxy_connect("HTTP CONNECT response invalid status code".into()))
}
