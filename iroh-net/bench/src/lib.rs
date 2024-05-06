use std::{
    num::ParseIntError,
    str::FromStr,
    sync::{Arc, Mutex},
    time::Instant,
};

use ::quinn::{Connection, RecvStream, SendStream, TransportConfig};
use anyhow::{Context, Result};
use bytes::Bytes;
use clap::Parser;
use stats::{Stats, TransferResult};
use tokio::runtime::{Builder, Runtime};
use tokio::sync::Semaphore;
use tracing::{info, trace};

pub mod iroh;
pub mod quinn;
pub mod s2n;
pub mod stats;

#[derive(Parser, Debug, Clone, Copy)]
#[clap(name = "bulk")]
pub enum Commands {
    Iroh(Opt),
    Quinn(Opt),
    S2n(s2n::Opt),
}

#[derive(Parser, Debug, Clone, Copy)]
#[clap(name = "options")]
pub struct Opt {
    /// The total number of clients which should be created
    #[clap(long = "clients", short = 'c', default_value = "1")]
    pub clients: usize,
    /// The total number of streams which should be created
    #[clap(long = "streams", short = 'n', default_value = "1")]
    pub streams: usize,
    /// The amount of concurrent streams which should be used
    #[clap(long = "max_streams", short = 'm', default_value = "1")]
    pub max_streams: usize,
    /// Number of bytes to transmit from server to client
    ///
    /// This can use SI prefixes for sizes. E.g. 1M will transfer 1MiB, 10G
    /// will transfer 10GiB.
    #[clap(long, default_value = "1G", value_parser = parse_byte_size)]
    pub download_size: u64,
    /// Number of bytes to transmit from client to server
    ///
    /// This can use SI prefixes for sizes. E.g. 1M will transfer 1MiB, 10G
    /// will transfer 10GiB.
    #[clap(long, default_value = "0", value_parser = parse_byte_size)]
    pub upload_size: u64,
    /// Show connection stats the at the end of the benchmark
    #[clap(long = "stats")]
    pub stats: bool,
    /// Whether to use the unordered read API
    #[clap(long = "unordered")]
    pub read_unordered: bool,
    /// Starting guess for maximum UDP payload size
    #[clap(long, default_value = "1200")]
    pub initial_mtu: u16,
}

pub enum Endpoint {
    Iroh(iroh_net::MagicEndpoint),
    Quinn(::quinn::Endpoint),
}

pub fn configure_tracing_subscriber() {
    tracing::subscriber::set_global_default(
        tracing_subscriber::FmtSubscriber::builder()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .finish(),
    )
    .unwrap();
}

pub fn rt() -> Runtime {
    Builder::new_current_thread().enable_all().build().unwrap()
}

fn parse_byte_size(s: &str) -> Result<u64, ParseIntError> {
    let s = s.trim();

    let multiplier = match s.chars().last() {
        Some('T') => 1024 * 1024 * 1024 * 1024,
        Some('G') => 1024 * 1024 * 1024,
        Some('M') => 1024 * 1024,
        Some('k') => 1024,
        _ => 1,
    };

    let s = if multiplier != 1 {
        &s[..s.len() - 1]
    } else {
        s
    };

    let base: u64 = u64::from_str(s)?;

    Ok(base * multiplier)
}

#[derive(Default)]
pub struct ClientStats {
    upload_stats: Stats,
    download_stats: Stats,
}

impl ClientStats {
    pub fn print(&self, client_id: usize) {
        println!();
        println!("Client {client_id} stats:");

        if self.upload_stats.total_size != 0 {
            self.upload_stats.print("upload");
        }

        if self.download_stats.total_size != 0 {
            self.download_stats.print("download");
        }
    }
}

async fn drain_stream(stream: &mut RecvStream, read_unordered: bool) -> Result<usize> {
    let mut read = 0;

    if read_unordered {
        while let Some(chunk) = stream.read_chunk(usize::MAX, false).await? {
            read += chunk.bytes.len();
        }
    } else {
        // These are 32 buffers, for reading approximately 32kB at once
        #[rustfmt::skip]
        let mut bufs = [
            Bytes::new(), Bytes::new(), Bytes::new(), Bytes::new(),
            Bytes::new(), Bytes::new(), Bytes::new(), Bytes::new(),
            Bytes::new(), Bytes::new(), Bytes::new(), Bytes::new(),
            Bytes::new(), Bytes::new(), Bytes::new(), Bytes::new(),
            Bytes::new(), Bytes::new(), Bytes::new(), Bytes::new(),
            Bytes::new(), Bytes::new(), Bytes::new(), Bytes::new(),
            Bytes::new(), Bytes::new(), Bytes::new(), Bytes::new(),
            Bytes::new(), Bytes::new(), Bytes::new(), Bytes::new(),
        ];

        while let Some(n) = stream.read_chunks(&mut bufs[..]).await? {
            read += bufs.iter().take(n).map(|buf| buf.len()).sum::<usize>();
        }
    }

    Ok(read)
}

async fn send_data_on_stream(stream: &mut SendStream, stream_size: u64) -> Result<()> {
    const DATA: &[u8] = &[0xAB; 1024 * 1024];
    let bytes_data = Bytes::from_static(DATA);

    let full_chunks = stream_size / (DATA.len() as u64);
    let remaining = (stream_size % (DATA.len() as u64)) as usize;

    for _ in 0..full_chunks {
        stream
            .write_chunk(bytes_data.clone())
            .await
            .context("failed sending data")?;
    }

    if remaining != 0 {
        stream
            .write_chunk(bytes_data.slice(0..remaining))
            .await
            .context("failed sending data")?;
    }

    stream.finish().await.context("failed finishing stream")?;

    Ok(())
}

async fn handle_client_stream(
    connection: Arc<Connection>,
    upload_size: u64,
    read_unordered: bool,
) -> Result<(TransferResult, TransferResult)> {
    let start = Instant::now();

    let (mut send_stream, mut recv_stream) = connection
        .open_bi()
        .await
        .context("failed to open stream")?;

    send_data_on_stream(&mut send_stream, upload_size).await?;

    let upload_result = TransferResult::new(start.elapsed(), upload_size);

    let start = Instant::now();
    let size = drain_stream(&mut recv_stream, read_unordered).await?;
    let download_result = TransferResult::new(start.elapsed(), size as u64);

    Ok((upload_result, download_result))
}

pub fn transport_config(max_streams: usize, initial_mtu: u16) -> TransportConfig {
    // High stream windows are chosen because the amount of concurrent streams
    // is configurable as a parameter.
    let mut config = TransportConfig::default();
    config.max_concurrent_uni_streams(max_streams.try_into().unwrap());
    config.initial_mtu(initial_mtu);

    // TODO: reenable when we upgrade quinn version
    // let mut acks = quinn::AckFrequencyConfig::default();
    // acks.ack_eliciting_threshold(10u32.into());
    // config.ack_frequency_config(Some(acks));

    config
}

/// Take the provided endpoint and run the server
pub async fn server(endpoint: Endpoint, opt: Opt) -> Result<()> {
    let mut server_tasks = Vec::new();

    // Handle only the expected amount of clients
    for _ in 0..opt.clients {
        let handshake = match endpoint {
            Endpoint::Iroh(ref endpoint) => endpoint.accept().await.unwrap(),
            Endpoint::Quinn(ref endpoint) => endpoint.accept().await.unwrap(),
        };
        let connection = handshake.await.context("handshake failed")?;

        server_tasks.push(tokio::spawn(async move {
            loop {
                let (mut send_stream, mut recv_stream) = match connection.accept_bi().await {
                    Err(::quinn::ConnectionError::ApplicationClosed(_)) => break,
                    Err(e) => {
                        eprintln!("accepting stream failed: {e:?}");
                        break;
                    }
                    Ok(stream) => stream,
                };
                trace!("stream established");

                tokio::spawn(async move {
                    drain_stream(&mut recv_stream, opt.read_unordered).await?;
                    send_data_on_stream(&mut send_stream, opt.download_size).await?;
                    Ok::<_, anyhow::Error>(())
                });
            }

            if opt.stats {
                println!("\nServer connection stats:\n{:#?}", connection.stats());
            }
        }));
    }

    // Await all the tasks. We have to do this to prevent the runtime getting dropped
    // and all server tasks to be cancelled
    for handle in server_tasks {
        if let Err(e) = handle.await {
            eprintln!("Server task error: {e:?}");
        };
    }

    Ok(())
}

/// Take the provided endpoint and run the client benchmark
pub async fn client_handler(
    endpoint: Endpoint,
    connection: ::quinn::Connection,
    opt: Opt,
) -> Result<ClientStats> {
    let start = Instant::now();

    let connection = Arc::new(connection);

    let mut stats = ClientStats::default();
    let mut first_error = None;

    let sem = Arc::new(Semaphore::new(opt.max_streams));
    let results = Arc::new(Mutex::new(Vec::new()));
    for _ in 0..opt.streams {
        let permit = sem.clone().acquire_owned().await.unwrap();
        let results = results.clone();
        let connection = connection.clone();
        tokio::spawn(async move {
            let result =
                handle_client_stream(connection, opt.upload_size, opt.read_unordered).await;
            info!("stream finished: {:?}", result);
            results.lock().unwrap().push(result);
            drop(permit);
        });
    }

    // Wait for remaining streams to finish
    let _ = sem.acquire_many(opt.max_streams as u32).await.unwrap();

    for result in results.lock().unwrap().drain(..) {
        match result {
            Ok((upload_result, download_result)) => {
                stats.upload_stats.stream_finished(upload_result);
                stats.download_stats.stream_finished(download_result);
            }
            Err(e) => {
                if first_error.is_none() {
                    first_error = Some(e);
                }
            }
        }
    }

    stats.upload_stats.total_duration = start.elapsed();
    stats.download_stats.total_duration = start.elapsed();

    // Explicit close of the connection, since handles can still be around due
    // to `Arc`ing them
    connection.close(0u32.into(), b"Benchmark done");

    match endpoint {
        Endpoint::Iroh(endpoint) => {
            endpoint.close(0u32.into(), b"").await?;
        }
        Endpoint::Quinn(endpoint) => {
            endpoint.close(0u32.into(), b"");
        }
    }

    if opt.stats {
        println!("\nClient connection stats:\n{:#?}", connection.stats());
    }

    match first_error {
        None => Ok(stats),
        Some(e) => Err(e),
    }
}
