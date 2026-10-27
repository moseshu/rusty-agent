// Adapted from openai-oss-forks/tokio-tungstenite (MIT) @ 0e5b2d73aa18dd9f0a50ee9ff199d5aef7594186, modified.
//! Proxy support for async client connections.

#![allow(
    clippy::cast_possible_truncation,
    reason = "Preserve the source bounded protocol and native ABI conversions."
)]
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use super::proxy_config::{
    ProxyAuth, ProxyConfig, ProxyScheme, build_http_connect_request, parse_http_connect_response,
};
use super::proxy_error::proxy_connect;
use tokio_tungstenite::tungstenite::{Error, Result};

const MAX_CONNECT_RESPONSE_SIZE: usize = 8192;

/// Establish a proxy tunnel over an async stream before WebSocket handshake.
pub(super) async fn connect_via_proxy<S>(
    mut stream: S,
    proxy: &ProxyConfig,
    host: &str,
    port: u16,
) -> Result<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    match proxy.scheme {
        ProxyScheme::Http => {
            http_connect(&mut stream, host, port, proxy.auth.as_ref()).await?;
        }
        ProxyScheme::Socks5 | ProxyScheme::Socks5h => {
            socks5_handshake(&mut stream, host, port, proxy.auth.as_ref()).await?;
        }
    }

    Ok(stream)
}

async fn http_connect<S>(
    stream: &mut S,
    host: &str,
    port: u16,
    auth: Option<&ProxyAuth>,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let authority = format!("{host}:{port}");
    let request = build_http_connect_request(&authority, auth)?;
    stream.write_all(&request).await.map_err(Error::Io)?;
    stream.flush().await.map_err(Error::Io)?;

    let response = read_connect_response(stream).await?;
    let status = parse_http_connect_response(&response)?;
    if !(200..300).contains(&status) {
        return Err(proxy_connect(format!(
            "HTTP CONNECT failed with status {status}"
        )));
    }

    Ok(())
}

async fn read_connect_response<S>(stream: &mut S) -> Result<Vec<u8>>
where
    S: AsyncRead + Unpin,
{
    let mut buf = Vec::new();
    let mut chunk = [0u8; 512];
    loop {
        if buf.len() >= MAX_CONNECT_RESPONSE_SIZE {
            return Err(proxy_connect("HTTP CONNECT response too large".into()));
        }

        let read = stream.read(&mut chunk).await.map_err(Error::Io)?;
        if read == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..read]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }

    Ok(buf)
}

async fn socks5_handshake<S>(
    stream: &mut S,
    host: &str,
    port: u16,
    auth: Option<&ProxyAuth>,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut methods = vec![0x00];
    if auth.is_some() {
        methods.push(0x02);
    }

    stream
        .write_all(&[0x05, methods.len() as u8])
        .await
        .map_err(Error::Io)?;
    stream.write_all(&methods).await.map_err(Error::Io)?;
    stream.flush().await.map_err(Error::Io)?;

    let mut choice = [0u8; 2];
    stream.read_exact(&mut choice).await.map_err(Error::Io)?;
    if choice[0] != 0x05 {
        return Err(proxy_connect("SOCKS5: invalid response version".into()));
    }

    match choice[1] {
        0x00 => {}
        0x02 => {
            let auth = auth.ok_or_else(|| {
                proxy_connect("SOCKS5: proxy requested auth, but none provided".into())
            })?;
            socks5_userpass_auth(stream, auth).await?;
        }
        0xFF => {
            return Err(proxy_connect(
                "SOCKS5: no acceptable authentication method".into(),
            ));
        }
        _ => {
            return Err(proxy_connect(
                "SOCKS5: unsupported authentication method".into(),
            ));
        }
    }

    send_socks5_connect(stream, host, port).await?;
    Ok(())
}

async fn socks5_userpass_auth<S>(stream: &mut S, auth: &ProxyAuth) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let username = auth.username.as_bytes();
    let password = auth.password.as_bytes();

    if username.len() > u8::MAX as usize || password.len() > u8::MAX as usize {
        return Err(proxy_connect("SOCKS5 auth credentials too long".into()));
    }

    let mut buf = Vec::with_capacity(3 + username.len() + password.len());
    buf.push(0x01);
    buf.push(username.len() as u8);
    buf.extend_from_slice(username);
    buf.push(password.len() as u8);
    buf.extend_from_slice(password);

    stream.write_all(&buf).await.map_err(Error::Io)?;
    stream.flush().await.map_err(Error::Io)?;

    let mut response = [0u8; 2];
    stream.read_exact(&mut response).await.map_err(Error::Io)?;
    if response[0] != 0x01 || response[1] != 0x00 {
        return Err(proxy_connect("SOCKS5 authentication failed".into()));
    }

    Ok(())
}

async fn send_socks5_connect<S>(stream: &mut S, host: &str, port: u16) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut request = Vec::new();
    request.push(0x05);
    request.push(0x01);
    request.push(0x00);

    if let Ok(addr) = host.parse::<std::net::Ipv4Addr>() {
        request.push(0x01);
        request.extend_from_slice(&addr.octets());
    } else if let Ok(addr) = host.parse::<std::net::Ipv6Addr>() {
        request.push(0x04);
        request.extend_from_slice(&addr.octets());
    } else {
        let host_bytes = host.as_bytes();
        if host_bytes.len() > u8::MAX as usize {
            return Err(proxy_connect("SOCKS5 domain name too long".into()));
        }
        request.push(0x03);
        request.push(host_bytes.len() as u8);
        request.extend_from_slice(host_bytes);
    }

    request.extend_from_slice(&port.to_be_bytes());
    stream.write_all(&request).await.map_err(Error::Io)?;
    stream.flush().await.map_err(Error::Io)?;

    let mut header = [0u8; 4];
    stream.read_exact(&mut header).await.map_err(Error::Io)?;
    if header[0] != 0x05 {
        return Err(proxy_connect("SOCKS5: invalid response version".into()));
    }

    if header[1] != 0x00 {
        return Err(proxy_connect(format!(
            "SOCKS5: connection failed with code {}",
            header[1]
        )));
    }

    let addr_len = match header[3] {
        0x01 => 4,
        0x03 => {
            let mut len = [0u8; 1];
            stream.read_exact(&mut len).await.map_err(Error::Io)?;
            len[0] as usize
        }
        0x04 => 16,
        _ => return Err(proxy_connect("SOCKS5: invalid address type".into())),
    };

    let mut discard = vec![0u8; addr_len + 2];
    stream.read_exact(&mut discard).await.map_err(Error::Io)?;
    Ok(())
}
