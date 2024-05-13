//! The server side API
use std::fmt::Debug;
use std::time::Duration;

use anyhow::{Context, Result};
use bao_tree::io::fsm::{encode_ranges_validated, Outboard};
use bao_tree::io::EncodeError;
use futures_lite::future::Boxed as BoxFuture;
use iroh_base::rpc::RpcError;
use iroh_io::stats::{
    SliceReaderStats, StreamWriterStats, TrackingSliceReader, TrackingStreamWriter,
};
use iroh_io::{AsyncSliceReader, AsyncStreamWriter, TokioStreamWriter};
use iroh_net::magic_endpoint::{self, RecvStream, SendStream};
use serde::{Deserialize, Serialize};
use tokio_util::task::LocalPoolHandle;
use tracing::{debug, debug_span, info, trace, warn};
use tracing_futures::Instrument;

use crate::hashseq::parse_hash_seq;
use crate::protocol::{GetRequest, RangeSpec, Request};
use crate::store::*;
use crate::util::Tag;
use crate::{BlobFormat, Hash};

/// Events emitted by the provider informing about the current status.
#[derive(Debug, Clone)]
pub enum Event {
    /// A new collection or tagged blob has been added
    TaggedBlobAdded {
        /// The hash of the added data
        hash: Hash,
        /// The format of the added data
        format: BlobFormat,
        /// The tag of the added data
        tag: Tag,
    },
    /// A new client connected to the node.
    ClientConnected {
        /// An unique connection id.
        connection_id: u64,
    },
    /// A request was received from a client.
    GetRequestReceived {
        /// An unique connection id.
        connection_id: u64,
        /// An identifier uniquely identifying this transfer request.
        request_id: u64,
        /// The hash for which the client wants to receive data.
        hash: Hash,
    },
    /// A request was received from a client.
    CustomGetRequestReceived {
        /// An unique connection id.
        connection_id: u64,
        /// An identifier uniquely identifying this transfer request.
        request_id: u64,
        /// The size of the custom get request.
        len: usize,
    },
    /// A sequence of hashes has been found and is being transferred.
    TransferHashSeqStarted {
        /// An unique connection id.
        connection_id: u64,
        /// An identifier uniquely identifying this transfer request.
        request_id: u64,
        /// The number of blobs in the sequence.
        num_blobs: u64,
    },
    /// A blob in a sequence was transferred.
    TransferBlobCompleted {
        /// An unique connection id.
        connection_id: u64,
        /// An identifier uniquely identifying this transfer request.
        request_id: u64,
        /// The hash of the blob
        hash: Hash,
        /// The index of the blob in the sequence.
        index: u64,
        /// The size of the blob transferred.
        size: u64,
    },
    /// A request was completed and the data was sent to the client.
    TransferCompleted {
        /// An unique connection id.
        connection_id: u64,
        /// An identifier uniquely identifying this transfer request.
        request_id: u64,
        /// statistics about the transfer
        stats: Box<TransferStats>,
    },
    /// A request was aborted because the client disconnected.
    TransferAborted {
        /// The quic connection id.
        connection_id: u64,
        /// An identifier uniquely identifying this request.
        request_id: u64,
        /// statistics about the transfer. This is None if the transfer
        /// was aborted before any data was sent.
        stats: Option<Box<TransferStats>>,
    },
}

/// The stats for a transfer of a collection or blob.
#[derive(Debug, Clone, Copy, Default)]
pub struct TransferStats {
    /// Stats for sending to the client.
    pub send: StreamWriterStats,
    /// Stats for reading from disk.
    pub read: SliceReaderStats,
    /// The total duration of the transfer.
    pub duration: Duration,
}

/// Progress updates for the add operation.
#[derive(Debug, Serialize, Deserialize)]
pub enum AddProgress {
    /// An item was found with name `name`, from now on referred to via `id`
    Found {
        /// A new unique id for this entry.
        id: u64,
        /// The name of the entry.
        name: String,
        /// The size of the entry in bytes.
        size: u64,
    },
    /// We got progress ingesting item `id`.
    Progress {
        /// The unique id of the entry.
        id: u64,
        /// The offset of the progress, in bytes.
        offset: u64,
    },
    /// We are done with `id`, and the hash is `hash`.
    Done {
        /// The unique id of the entry.
        id: u64,
        /// The hash of the entry.
        hash: Hash,
    },
    /// We are done with the whole operation.
    AllDone {
        /// The hash of the created data.
        hash: Hash,
        /// The format of the added data.
        format: BlobFormat,
        /// The tag of the added data.
        tag: Tag,
    },
    /// We got an error and need to abort.
    ///
    /// This will be the last message in the stream.
    Abort(RpcError),
}

/// Read the request from the getter.
///
/// Will fail if there is an error while reading, if the reader
/// contains more data than the Request, or if no valid request is sent.
///
/// When successful, the buffer is empty after this function call.
pub async fn read_request(mut reader: RecvStream) -> Result<Request> {
    let payload = reader
        .read_to_end(crate::protocol::MAX_MESSAGE_SIZE)
        .await?;
    let request: Request = postcard::from_bytes(&payload)?;
    Ok(request)
}

/// Transfers the collection & blob data.
///
/// First, it transfers the collection data & its associated outboard encoding data. Then it sequentially transfers each individual blob data & its associated outboard
/// encoding data.
///
/// Will fail if there is an error writing to the getter or reading from
/// the database.
///
/// If a blob from the collection cannot be found in the database, the transfer will gracefully
/// close the writer, and return with `Ok(SentStatus::NotFound)`.
///
/// If the transfer does _not_ end in error, the buffer will be empty and the writer is gracefully closed.
pub async fn transfer_collection<D: Map, E: EventSender>(
    request: GetRequest,
    // Store from which to fetch blobs.
    db: &D,
    // Response writer, containing the quinn stream.
    writer: &mut ResponseWriter<E>,
    // the collection to transfer
    mut outboard: impl Outboard,
    mut data: impl AsyncSliceReader,
    stats: &mut TransferStats,
) -> Result<SentStatus> {
    let hash = request.hash;

    // if the request is just for the root, we don't need to deserialize the collection
    let just_root = matches!(request.ranges.as_single(), Some((0, _)));
    let mut c = if !just_root {
        // parse the hash seq
        let (stream, num_blobs) = parse_hash_seq(&mut data).await?;
        writer
            .events
            .send(Event::TransferHashSeqStarted {
                connection_id: writer.connection_id(),
                request_id: writer.request_id(),
                num_blobs,
            })
            .await;
        Some(stream)
    } else {
        None
    };

    let mut prev = 0;
    for (offset, ranges) in request.ranges.iter_non_empty() {
        // create a tracking writer so we can get some stats for writing
        let mut tw = writer.tracking_writer();
        if offset == 0 {
            debug!("writing ranges '{:?}' of sequence {}", ranges, hash);
            // wrap the data reader in a tracking reader so we can get some stats for reading
            let mut tracking_reader = TrackingSliceReader::new(&mut data);
            // send the root
            tw.write(outboard.tree().size().to_le_bytes().as_slice())
                .await?;
            encode_ranges_validated(
                &mut tracking_reader,
                &mut outboard,
                &ranges.to_chunk_ranges(),
                &mut tw,
            )
            .await?;
            stats.read += tracking_reader.stats();
            stats.send += tw.stats();
            debug!(
                "finished writing ranges '{:?}' of collection {}",
                ranges, hash
            );
        } else {
            let c = c.as_mut().context("collection parser not available")?;
            debug!("wrtiting ranges '{:?}' of child {}", ranges, offset);
            // skip to the next blob if there is a gap
            if prev < offset - 1 {
                c.skip(offset - prev - 1).await?;
            }
            if let Some(hash) = c.next().await? {
                tokio::task::yield_now().await;
                let (status, size, blob_read_stats) = send_blob(db, hash, ranges, &mut tw).await?;
                stats.send += tw.stats();
                stats.read += blob_read_stats;
                if SentStatus::NotFound == status {
                    writer.inner.finish().await?;
                    return Ok(status);
                }

                writer
                    .events
                    .send(Event::TransferBlobCompleted {
                        connection_id: writer.connection_id(),
                        request_id: writer.request_id(),
                        hash,
                        index: offset - 1,
                        size,
                    })
                    .await;
            } else {
                // nothing more we can send
                break;
            }
            prev = offset;
        }
    }

    debug!("done writing");
    Ok(SentStatus::Sent)
}

/// Trait for sending events.
pub trait EventSender: Clone + Sync + Send + 'static {
    /// Send an event.
    fn send(&self, event: Event) -> BoxFuture<()>;
}

/// Handle a single connection.
pub async fn handle_connection<D: Map, E: EventSender>(
    connection: magic_endpoint::Connection,
    db: D,
    events: E,
    rt: LocalPoolHandle,
) {
    let remote_addr = connection.remote_address();
    let connection_id = connection.stable_id() as u64;
    let span = debug_span!("connection", connection_id, %remote_addr);
    async move {
        while let Ok((writer, reader)) = connection.accept_bi().await {
            // The stream ID index is used to identify this request.  Requests only arrive in
            // bi-directional RecvStreams initiated by the client, so this uniquely identifies them.
            let request_id = reader.id().index();
            let span = debug_span!("stream", stream_id = %request_id);
            let writer = ResponseWriter {
                connection_id,
                events: events.clone(),
                inner: writer,
            };
            events.send(Event::ClientConnected { connection_id }).await;
            let db = db.clone();
            rt.spawn_pinned(|| {
                async move {
                    if let Err(err) = handle_stream(db, reader, writer).await {
                        warn!("error: {err:#?}",);
                    }
                }
                .instrument(span)
            });
        }
    }
    .instrument(span)
    .await
}

async fn handle_stream<D: Map, E: EventSender>(
    db: D,
    reader: RecvStream,
    writer: ResponseWriter<E>,
) -> Result<()> {
    // 1. Decode the request.
    debug!("reading request");
    let request = match read_request(reader).await {
        Ok(r) => r,
        Err(e) => {
            writer.notify_transfer_aborted(None).await;
            return Err(e);
        }
    };

    match request {
        Request::Get(request) => handle_get(db, request, writer).await,
    }
}

/// Handle a single standard get request.
pub async fn handle_get<D: Map, E: EventSender>(
    db: D,
    request: GetRequest,
    mut writer: ResponseWriter<E>,
) -> Result<()> {
    let hash = request.hash;
    debug!(%hash, "received request");
    writer
        .events
        .send(Event::GetRequestReceived {
            hash,
            connection_id: writer.connection_id(),
            request_id: writer.request_id(),
        })
        .await;

    // 4. Attempt to find hash
    match db.get(&hash).await? {
        // Collection or blob request
        Some(entry) => {
            let mut stats = Box::<TransferStats>::default();
            let t0 = std::time::Instant::now();
            // 5. Transfer data!
            let res = transfer_collection(
                request,
                &db,
                &mut writer,
                entry.outboard().await?,
                entry.data_reader().await?,
                &mut stats,
            )
            .await;
            stats.duration = t0.elapsed();
            match res {
                Ok(SentStatus::Sent) => {
                    writer.notify_transfer_completed(&hash, stats).await;
                }
                Ok(SentStatus::NotFound) => {
                    writer.notify_transfer_aborted(Some(stats)).await;
                }
                Err(e) => {
                    writer.notify_transfer_aborted(Some(stats)).await;
                    return Err(e);
                }
            }

            debug!("finished response");
        }
        None => {
            debug!("not found {}", hash);
            writer.notify_transfer_aborted(None).await;
            writer.inner.finish().await?;
        }
    };

    Ok(())
}

/// A helper struct that combines a quinn::SendStream with auxiliary information
#[derive(Debug)]
pub struct ResponseWriter<E> {
    inner: SendStream,
    events: E,
    connection_id: u64,
}

impl<E: EventSender> ResponseWriter<E> {
    fn tracking_writer(&mut self) -> TrackingStreamWriter<TokioStreamWriter<&mut SendStream>> {
        TrackingStreamWriter::new(TokioStreamWriter(&mut self.inner))
    }

    fn connection_id(&self) -> u64 {
        self.connection_id
    }

    fn request_id(&self) -> u64 {
        self.inner.id().index()
    }

    fn print_stats(stats: &TransferStats) {
        let send = stats.send.total();
        let read = stats.read.total();
        let total_sent_bytes = send.size;
        let send_duration = send.stats.duration;
        let read_duration = read.stats.duration;
        let total_duration = stats.duration;
        let other_duration = total_duration
            .saturating_sub(send_duration)
            .saturating_sub(read_duration);
        let avg_send_size = total_sent_bytes.checked_div(send.stats.count).unwrap_or(0);
        info!(
            "sent {} bytes in {}s",
            total_sent_bytes,
            total_duration.as_secs_f64()
        );
        debug!(
            "{}s sending, {}s reading, {}s other",
            send_duration.as_secs_f64(),
            read_duration.as_secs_f64(),
            other_duration.as_secs_f64()
        );
        trace!(
            "send_count: {} avg_send_size {}",
            send.stats.count,
            avg_send_size,
        )
    }

    async fn notify_transfer_completed(&self, hash: &Hash, stats: Box<TransferStats>) {
        info!("transfer completed for {}", hash);
        Self::print_stats(&stats);
        self.events
            .send(Event::TransferCompleted {
                connection_id: self.connection_id(),
                request_id: self.request_id(),
                stats,
            })
            .await;
    }

    async fn notify_transfer_aborted(&self, stats: Option<Box<TransferStats>>) {
        if let Some(stats) = &stats {
            Self::print_stats(stats);
        };
        self.events
            .send(Event::TransferAborted {
                connection_id: self.connection_id(),
                request_id: self.request_id(),
                stats,
            })
            .await;
    }
}

/// Status  of a send operation
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SentStatus {
    /// The requested data was sent
    Sent,
    /// The requested data was not found
    NotFound,
}

/// Send a blob to the client.
pub async fn send_blob<D: Map, W: AsyncStreamWriter>(
    db: &D,
    hash: Hash,
    ranges: &RangeSpec,
    mut writer: W,
) -> Result<(SentStatus, u64, SliceReaderStats)> {
    match db.get(&hash).await? {
        Some(entry) => {
            let outboard = entry.outboard().await?;
            let size = outboard.tree().size();
            let mut file_reader = TrackingSliceReader::new(entry.data_reader().await?);
            writer.write(size.to_le_bytes().as_slice()).await?;
            encode_ranges_validated(
                &mut file_reader,
                outboard,
                &ranges.to_chunk_ranges(),
                writer,
            )
            .await
            .map_err(|e| encode_error_to_anyhow(e, &hash))?;

            Ok((SentStatus::Sent, size, file_reader.stats()))
        }
        _ => {
            debug!("blob not found {}", hash.to_hex());
            Ok((SentStatus::NotFound, 0, SliceReaderStats::default()))
        }
    }
}

fn encode_error_to_anyhow(err: EncodeError, hash: &Hash) -> anyhow::Error {
    match err {
        EncodeError::LeafHashMismatch(x) => anyhow::Error::from(EncodeError::LeafHashMismatch(x))
            .context(format!("hash {} offset {}", hash.to_hex(), x.to_bytes())),
        EncodeError::ParentHashMismatch(n) => {
            let r = n.chunk_range();
            anyhow::Error::from(EncodeError::ParentHashMismatch(n)).context(format!(
                "hash {} range {}..{}",
                hash.to_hex(),
                r.start.to_bytes(),
                r.end.to_bytes()
            ))
        }
        e => anyhow::Error::from(e).context(format!("hash {}", hash.to_hex())),
    }
}
