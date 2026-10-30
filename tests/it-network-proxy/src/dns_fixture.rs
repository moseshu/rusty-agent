//! Public DNS fixtures for policy tests; connection tests retain the real system resolver.
use std::{
    io,
    net::{IpAddr, Ipv4Addr, SocketAddr},
};
pub(crate) async fn lookup_host(
    (host, port): (&str, u16),
) -> io::Result<std::vec::IntoIter<SocketAddr>> {
    if host.ends_with(".invalid") {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "reserved invalid DNS fixture",
        ));
    }
    let ip = if let Ok(ip) = host.parse::<IpAddr>() {
        ip
    } else if host == "localhost" {
        IpAddr::V4(Ipv4Addr::LOCALHOST)
    } else if matches!(
        host,
        "example.com"
            | "api.example.com"
            | "openai.com"
            | "api.openai.com"
            | "github.com"
            | "api.github.com"
            | "api.astemu.ghe.com"
            | "google.com"
            | "managed.example.com"
            | "user.example.com"
            | "public.example"
            | "alias.example"
            | "example5.com"
    ) {
        IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8))
    } else {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "unregistered DNS fixture",
        ));
    };
    Ok(vec![SocketAddr::new(ip, port)].into_iter())
}
