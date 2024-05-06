use std::net::SocketAddr;

use anyhow::{Context, Result};
use iroh_net::{relay::RelayMode, MagicEndpoint, NodeAddr};
use tracing::trace;

use crate::{client_handler, transport_config, ClientStats, Endpoint, Opt};

pub const ALPN: &[u8] = b"n0/iroh-net-bench/0";

/// Creates a server endpoint which runs on the given runtime
pub fn server_endpoint(rt: &tokio::runtime::Runtime, opt: &Opt) -> (NodeAddr, MagicEndpoint) {
    let _guard = rt.enter();
    rt.block_on(async move {
        let ep = MagicEndpoint::builder()
            .alpns(vec![ALPN.to_vec()])
            .relay_mode(RelayMode::Disabled)
            .transport_config(transport_config(opt.max_streams, opt.initial_mtu))
            .bind(0)
            .await
            .unwrap();
        let addr = ep.local_addr();
        let addr = SocketAddr::new("127.0.0.1".parse().unwrap(), addr.0.port());
        let addr = NodeAddr::new(ep.node_id()).with_direct_addresses([addr]);
        (addr, ep)
    })
}

/// Create and run a client
pub async fn client(server_addr: NodeAddr, opt: Opt) -> Result<ClientStats> {
    let (endpoint, connection) = connect_client(server_addr, opt).await?;
    client_handler(Endpoint::Iroh(endpoint), connection, opt).await
}

/// Create a client endpoint and client connection
pub async fn connect_client(
    server_addr: NodeAddr,
    opt: Opt,
) -> Result<(MagicEndpoint, quinn::Connection)> {
    let endpoint = MagicEndpoint::builder()
        .alpns(vec![ALPN.to_vec()])
        .relay_mode(RelayMode::Disabled)
        .transport_config(transport_config(opt.max_streams, opt.initial_mtu))
        .bind(0)
        .await
        .unwrap();

    // TODO: We don't support passing client transport config currently
    // let mut client_config = quinn::ClientConfig::new(Arc::new(crypto));
    // client_config.transport_config(Arc::new(transport_config(&opt)));

    let connection = endpoint
        .connect(server_addr, ALPN)
        .await
        .context("unable to connect")?;
    trace!("connected");

    Ok((endpoint, connection))
}
