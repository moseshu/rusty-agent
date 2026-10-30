// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
use crate::policy::is_non_public_ip;
use crate::runtime::HostBlockDecision;
use crate::state::NetworkProxyState;
#[cfg(target_os = "macos")]
use crate::system_dns::SystemDnsResolver;
use rama_core::Service;
use rama_core::error::BoxError;
use rama_core::error::ErrorExt as _;
use rama_core::error::OpaqueError;
use rama_core::extensions::ExtensionsMut;
use rama_net::address::Host;
use rama_net::address::HostWithPort;
use rama_net::address::ProxyAddress;
use rama_net::client::EstablishedClientConnection;
use rama_net::transport::TryRefIntoTransportContext;
use rama_tcp::TcpStream;
use rama_tcp::client::TcpStreamConnector;
use rama_tcp::client::service::TcpConnector;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;

#[derive(Clone)]
pub(crate) struct TargetCheckedTcpConnector {
    state: Arc<NetworkProxyState>,
}

impl TargetCheckedTcpConnector {
    pub(crate) fn new(state: Arc<NetworkProxyState>) -> Self {
        Self { state }
    }
}

impl<Input> Service<Input> for TargetCheckedTcpConnector
where
    Input: TryRefIntoTransportContext + Send + ExtensionsMut + 'static,
    Input::Error: Into<BoxError> + Send + Sync + 'static,
{
    type Output = EstablishedClientConnection<TcpStream, Input>;
    type Error = BoxError;

    async fn serve(&self, input: Input) -> Result<Self::Output, Self::Error> {
        let connector = TcpConnector::new();
        #[cfg(target_os = "macos")]
        let connector = connector.with_dns(SystemDnsResolver);

        if input.extensions().get::<ProxyAddress>().is_some() {
            return connector.serve(input).await;
        }

        let target = input
            .try_ref_into_transport_ctx()
            .map_err(|err| OpaqueError::from_boxed(err.into()).context("read network target"))?
            .host_with_port()
            .ok_or_else(|| OpaqueError::from_display("network target is missing a port"))?;

        connector
            .with_connector(TargetCheckedStreamConnector {
                state: self.state.clone(),
                target,
            })
            .serve(input)
            .await
    }
}

#[derive(Clone)]
struct TargetCheckedStreamConnector {
    state: Arc<NetworkProxyState>,
    target: HostWithPort,
}

impl TcpStreamConnector for TargetCheckedStreamConnector {
    type Error = BoxError;

    async fn connect(&self, addr: SocketAddr) -> Result<TcpStream, Self::Error> {
        if is_non_public_ip(addr.ip()) && !self.allows_non_public_target(addr).await? {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "network target rejected by policy",
            )
            .into());
        }

        tokio::net::TcpStream::connect(addr)
            .await
            .map(TcpStream::from)
            .map_err(Into::into)
    }
}

impl TargetCheckedStreamConnector {
    async fn allows_non_public_target(&self, addr: SocketAddr) -> Result<bool, BoxError> {
        if self.state.allow_local_binding().await.map_err(|err| {
            let err: BoxError = err.into();
            OpaqueError::from_boxed(err)
                .context("read network proxy config")
                .into_boxed()
        })? {
            return Ok(true);
        }

        if !target_matches_non_public_addr(&self.target.host, addr.ip()) {
            return Ok(false);
        }

        self.state
            .host_blocked(&self.target.host.to_string(), self.target.port)
            .await
            .map(|decision| decision == HostBlockDecision::Allowed)
            .map_err(|err| {
                let err: BoxError = err.into();
                OpaqueError::from_boxed(err)
                    .context("evaluate network proxy target")
                    .into_boxed()
            })
    }
}

pub(crate) fn is_non_public_target(host: &Host) -> bool {
    match host {
        Host::Address(ip) => is_non_public_ip(*ip),
        Host::Name(name) => name
            .as_str()
            .trim_end_matches('.')
            .eq_ignore_ascii_case("localhost"),
    }
}

fn target_matches_non_public_addr(host: &Host, addr: std::net::IpAddr) -> bool {
    match host {
        Host::Address(ip) => *ip == addr,
        Host::Name(name) => {
            name.as_str()
                .trim_end_matches('.')
                .eq_ignore_ascii_case("localhost")
                && addr.is_loopback()
        }
    }
}
