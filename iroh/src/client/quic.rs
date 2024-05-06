//! Type declarations and utility functions for an RPC client to an iroh node running in a separate process.

use std::{
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    path::Path,
    sync::Arc,
    time::Duration,
};

use anyhow::{bail, Context};
use quic_rpc::transport::quinn::QuinnConnection;

use crate::{
    node::RpcStatus,
    rpc_protocol::{NodeStatusRequest, RpcService},
};

/// ALPN used by irohs RPC mechanism.
// TODO: Change to "/iroh-rpc/1"
pub(crate) const RPC_ALPN: [u8; 17] = *b"n0/provider-rpc/1";

/// RPC client to an iroh node running in a separate process.
pub type RpcClient = quic_rpc::RpcClient<RpcService, QuinnConnection<RpcService>>;

/// Client to an iroh node running in a separate process.
///
/// This is obtained from [`Iroh::connect`].
pub type Iroh = super::Iroh<QuinnConnection<RpcService>>;

/// RPC document client to an iroh node running in a separate process.
pub type Doc = super::docs::Doc<QuinnConnection<RpcService>>;

impl Iroh {
    /// Connect to an iroh node running on the same computer, but in a different process.
    pub async fn connect(root: impl AsRef<Path>) -> anyhow::Result<Self> {
        let rpc_status = RpcStatus::load(root).await?;
        match rpc_status {
            RpcStatus::Stopped => {
                bail!("iroh is not running, please start it");
            }
            RpcStatus::Running { client, .. } => Ok(Iroh::new(client)),
        }
    }
}

/// Create a raw RPC client to an iroh node running on the same computer, but in a different
/// process.
pub(crate) async fn connect_raw(rpc_port: u16) -> anyhow::Result<RpcClient> {
    let bind_addr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0).into();
    let endpoint = create_quinn_client(bind_addr, vec![RPC_ALPN.to_vec()], false)?;
    let addr = SocketAddr::new(Ipv4Addr::LOCALHOST.into(), rpc_port);
    let server_name = "localhost".to_string();
    let connection = QuinnConnection::<RpcService>::new(endpoint, addr, server_name);
    let client = RpcClient::new(connection);
    // Do a status request to check if the server is running.
    let _version = tokio::time::timeout(Duration::from_secs(1), client.rpc(NodeStatusRequest))
        .await
        .context("Iroh node is not running")??;
    Ok(client)
}

fn create_quinn_client(
    bind_addr: SocketAddr,
    alpn_protocols: Vec<Vec<u8>>,
    keylog: bool,
) -> anyhow::Result<quinn::Endpoint> {
    let secret_key = iroh_net::key::SecretKey::generate();
    let tls_client_config =
        iroh_net::tls::make_client_config(&secret_key, None, alpn_protocols, keylog)?;
    let mut client_config = quinn::ClientConfig::new(Arc::new(tls_client_config));
    let mut endpoint = quinn::Endpoint::client(bind_addr)?;
    let mut transport_config = quinn::TransportConfig::default();
    transport_config.keep_alive_interval(Some(Duration::from_secs(1)));
    client_config.transport_config(Arc::new(transport_config));
    endpoint.set_default_client_config(client_config);
    Ok(endpoint)
}
