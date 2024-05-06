use std::{net::SocketAddr, sync::Arc};

use anyhow::{Context, Result};
use quinn::{Connection, TokioRuntime};
use socket2::{Domain, Protocol, Socket, Type};
use tracing::warn;

use crate::{client_handler, transport_config, ClientStats, Endpoint, Opt};

/// Derived from the iroh-net udp SOCKET_BUFFER_SIZE
const SOCKET_BUFFER_SIZE: usize = 7 << 20;
pub const ALPN: &[u8] = b"n0/quinn-bench/0";

/// Creates a server endpoint which runs on the given runtime
pub fn server_endpoint(rt: &tokio::runtime::Runtime, opt: &Opt) -> (SocketAddr, quinn::Endpoint) {
    let secret_key = iroh_net::key::SecretKey::generate();
    let crypto =
        iroh_net::tls::make_server_config(&secret_key, vec![ALPN.to_vec()], false).unwrap();

    let transport = transport_config(opt.max_streams, opt.initial_mtu);

    let mut server_config = quinn::ServerConfig::with_crypto(Arc::new(crypto));
    server_config.transport_config(Arc::new(transport));

    let addr = SocketAddr::new("127.0.0.1".parse().unwrap(), 0);

    let socket = bind_socket(addr).unwrap();

    let _guard = rt.enter();
    rt.block_on(async move {
        let ep = quinn::Endpoint::new(
            Default::default(),
            Some(server_config),
            socket,
            Arc::new(TokioRuntime),
        )
        .unwrap();
        let addr = ep.local_addr().unwrap();
        (addr, ep)
    })
}

/// Create and run a client
pub async fn client(server_addr: SocketAddr, opt: Opt) -> Result<ClientStats> {
    let (endpoint, connection) = connect_client(server_addr, opt).await?;
    client_handler(Endpoint::Quinn(endpoint), connection, opt).await
}

/// Create a client endpoint and client connection
pub async fn connect_client(
    server_addr: SocketAddr,
    opt: Opt,
) -> Result<(::quinn::Endpoint, Connection)> {
    let secret_key = iroh_net::key::SecretKey::generate();
    let tls_client_config =
        iroh_net::tls::make_client_config(&secret_key, None, vec![ALPN.to_vec()], false)?;
    let mut config = quinn::ClientConfig::new(Arc::new(tls_client_config));

    let transport = transport_config(opt.max_streams, opt.initial_mtu);

    // let mut config = quinn::ClientConfig::new(Arc::new(crypto));
    config.transport_config(Arc::new(transport));

    let addr = SocketAddr::new("127.0.0.1".parse().unwrap(), 0);

    let socket = bind_socket(addr).unwrap();

    let ep =
        quinn::Endpoint::new(Default::default(), None, socket, Arc::new(TokioRuntime)).unwrap();
    let connection = ep
        .connect_with(config, server_addr, "local")?
        .await
        .context("connecting")
        .unwrap();
    Ok((ep, connection))
}

fn bind_socket(addr: SocketAddr) -> Result<std::net::UdpSocket> {
    let socket = Socket::new(Domain::for_address(addr), Type::DGRAM, Some(Protocol::UDP))
        .context("create socket")?;

    if addr.is_ipv6() {
        socket.set_only_v6(false).context("set_only_v6")?;
    }

    socket
        .bind(&socket2::SockAddr::from(addr))
        .context("binding endpoint")?;
    socket
        .set_send_buffer_size(SOCKET_BUFFER_SIZE)
        .context("send buffer size")?;
    socket
        .set_recv_buffer_size(SOCKET_BUFFER_SIZE)
        .context("recv buffer size")?;

    let buf_size = socket.send_buffer_size().context("send buffer size")?;
    if buf_size < SOCKET_BUFFER_SIZE {
        warn!(
            "Unable to set desired send buffer size. Desired: {}, Actual: {}",
            SOCKET_BUFFER_SIZE, buf_size
        );
    }

    let buf_size = socket.recv_buffer_size().context("recv buffer size")?;
    if buf_size < SOCKET_BUFFER_SIZE {
        warn!(
            "Unable to set desired recv buffer size. Desired: {}, Actual: {}",
            SOCKET_BUFFER_SIZE, buf_size
        );
    }

    Ok(socket.into())
}
