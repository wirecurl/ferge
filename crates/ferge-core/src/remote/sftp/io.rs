//! Pipelined file contents over SFTP.
//!
//! SFTP is request/response: a client that waits for each READ or WRITE before
//! sending the next is limited to one chunk per round trip. Like OpenSSH's
//! `sftp`, Ferge keeps a window of requests in flight, so throughput is bound
//! by bandwidth instead of latency.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use futures::future::BoxFuture;
use futures::stream::{FuturesOrdered, FuturesUnordered};
use futures::{FutureExt as _, StreamExt as _};
use russh_sftp::client::RawSftpSession;
use russh_sftp::client::error::Error as SftpError;
use russh_sftp::protocol::{FileAttributes, OpenFlags};
use tokio::sync::mpsc;

use super::{is_eof, sftp_error};
use crate::error::Result;
use crate::path::RemotePath;
use crate::remote::{ByteStream, FileWriter};

/// Requests kept in flight per file and direction.
const WINDOW: usize = 16;

/// Chunk sizes for one data channel, from the server's `limits@openssh.com`
/// or the SFTPv3 minimum every server must accept.
#[derive(Clone, Copy, Debug)]
pub(super) struct ChunkSizes {
    pub read: u32,
    pub write: usize,
}

impl ChunkSizes {
    /// 32 KiB: the size every SFTPv3 server must support.
    pub(super) const SAFE: Self = Self {
        read: 32 * 1024,
        write: 32 * 1024,
    };

    /// Upper bound even if a server allows more; larger chunks only add latency.
    const MAX: u64 = 256 * 1024;

    /// Sizes allowed by the server's limits (`0` means "no limit").
    pub(super) fn from_limits(max_read: u64, max_write: u64) -> Self {
        let pick = |limit: u64, fallback: u64| {
            if limit == 0 {
                fallback
            } else {
                limit.min(Self::MAX)
            }
        };
        Self {
            read: u32::try_from(pick(max_read, Self::MAX)).unwrap_or(32 * 1024),
            write: usize::try_from(pick(max_write, Self::MAX)).unwrap_or(32 * 1024),
        }
    }
}

/// Streams `path` from `offset`, keeping [`WINDOW`] reads in flight.
pub(super) fn read_stream(
    session: Arc<RawSftpSession>,
    path: RemotePath,
    offset: u64,
    chunk: u32,
) -> ByteStream {
    // A small buffer is enough: the window on the server side does the work,
    // and the consumer's pace limits memory use.
    let (sender, receiver) = mpsc::channel(4);
    tokio::spawn(read_task(session, path, offset, chunk, sender));
    futures::stream::unfold(receiver, |mut receiver| async move {
        receiver.recv().await.map(|item| (item, receiver))
    })
    .boxed()
}

type ReadReply = (u64, u32, std::result::Result<Vec<u8>, SftpError>);

async fn read_task(
    session: Arc<RawSftpSession>,
    path: RemotePath,
    offset: u64,
    chunk: u32,
    sender: mpsc::Sender<Result<Bytes>>,
) {
    let handle = match session
        .open(path.as_str(), OpenFlags::READ, FileAttributes::empty())
        .await
    {
        Ok(handle) => handle.handle,
        Err(error) => {
            let _ = sender.send(Err(sftp_error(&path, error))).await;
            return;
        }
    };
    // The size decides how many reads to issue ahead; reads past it would only
    // come back as EOF. If the file grows meanwhile, reading simply continues.
    let size = session
        .fstat(handle.as_str())
        .await
        .ok()
        .and_then(|attrs| attrs.attrs.size);

    let read = |at: u64, len: u32| -> BoxFuture<'static, ReadReply> {
        let session = Arc::clone(&session);
        let handle = handle.clone();
        async move {
            let reply = session.read(handle, at, len).await.map(|data| data.data);
            (at, len, reply)
        }
        .boxed()
    };

    let mut next = offset;
    let mut in_flight: FuturesOrdered<BoxFuture<'static, ReadReply>> = FuturesOrdered::new();
    let mut ended = false;
    loop {
        while !ended
            && in_flight.len() < WINDOW
            && (size.is_some_and(|size| next < size) || in_flight.is_empty())
        {
            in_flight.push_back(read(next, chunk));
            next += u64::from(chunk);
        }
        let reply = futures::select_biased! {
            // Stop as soon as the consumer goes away.
            () = sender.closed().fuse() => None,
            reply = in_flight.next().fuse() => Some(reply),
        };
        // Either the consumer left, or nothing is in flight and nothing left
        // to request.
        let Some(Some((at, len, reply))) = reply else {
            break;
        };
        match reply {
            Ok(data) if !data.is_empty() => {
                let got = data.len() as u64;
                if sender.send(Ok(Bytes::from(data))).await.is_err() {
                    break;
                }
                // A short read leaves a gap before the replies already in
                // flight; fill it before passing anything else on.
                if got < u64::from(len) {
                    match fill_gap(
                        &session,
                        &handle,
                        &path,
                        at + got,
                        at + u64::from(len),
                        &sender,
                    )
                    .await
                    {
                        Gap::Filled => {}
                        Gap::EndOfFile => ended = true,
                        Gap::Stop => break,
                    }
                }
            }
            Ok(_) => ended = true,
            Err(error) if is_eof(&error) => ended = true,
            Err(error) => {
                let _ = sender.send(Err(sftp_error(&path, error))).await;
                break;
            }
        }
        if ended {
            // Everything still in flight lies past the end of the file.
            break;
        }
    }
    drop(in_flight);
    if let Err(error) = session.close(handle).await {
        tracing::debug!(%path, %error, "closing a file handle failed");
    }
}

enum Gap {
    Filled,
    EndOfFile,
    Stop,
}

/// Reads `[from, to)` one request at a time and passes it on.
async fn fill_gap(
    session: &RawSftpSession,
    handle: &str,
    path: &RemotePath,
    mut from: u64,
    to: u64,
    sender: &mpsc::Sender<Result<Bytes>>,
) -> Gap {
    while from < to {
        let len = u32::try_from(to - from).unwrap_or(u32::MAX);
        match session.read(handle, from, len).await {
            Ok(data) if !data.data.is_empty() => {
                from += data.data.len() as u64;
                if sender.send(Ok(Bytes::from(data.data))).await.is_err() {
                    return Gap::Stop;
                }
            }
            Ok(_) => return Gap::EndOfFile,
            Err(error) if is_eof(&error) => return Gap::EndOfFile,
            Err(error) => {
                let _ = sender.send(Err(sftp_error(path, error))).await;
                return Gap::Stop;
            }
        }
    }
    Gap::Filled
}

/// Opens `path` for writing at `offset` and returns a pipelined writer.
pub(super) async fn open_writer(
    session: Arc<RawSftpSession>,
    path: &RemotePath,
    offset: u64,
    chunk: usize,
) -> Result<Box<dyn FileWriter>> {
    let mut flags = OpenFlags::WRITE | OpenFlags::CREATE;
    if offset == 0 {
        flags |= OpenFlags::TRUNCATE;
    }
    let handle = session
        .open(path.as_str(), flags, FileAttributes::empty())
        .await
        .map_err(|error| sftp_error(path, error))?
        .handle;
    if offset > 0 {
        // Drop anything past the resume point, e.g. a partly written chunk.
        let attrs = FileAttributes {
            size: Some(offset),
            ..FileAttributes::empty()
        };
        if let Err(error) = session.fsetstat(handle.as_str(), attrs).await {
            let _ = session.close(handle).await;
            return Err(sftp_error(path, error));
        }
    }
    Ok(Box::new(SftpWriter {
        session,
        handle: Some(handle),
        path: path.clone(),
        offset,
        chunk: chunk.max(1),
        in_flight: FuturesUnordered::new(),
        confirmed: offset,
        acknowledged: BTreeMap::new(),
    }))
}

struct SftpWriter {
    session: Arc<RawSftpSession>,
    /// `None` once closed.
    handle: Option<String>,
    path: RemotePath,
    /// Where the next write goes.
    offset: u64,
    chunk: usize,
    in_flight: FuturesUnordered<BoxFuture<'static, Result<(u64, u64)>>>,
    /// End of the gap-free prefix that the server acknowledged.
    confirmed: u64,
    /// Acknowledged ranges beyond `confirmed`, by start offset.
    acknowledged: BTreeMap<u64, u64>,
}

impl SftpWriter {
    async fn wait_for_one(&mut self) -> Result<()> {
        match self.in_flight.next().await {
            Some(result) => {
                let (start, end) = result?;
                self.acknowledge(start, end);
                Ok(())
            }
            None => Ok(()),
        }
    }

    /// Records an acknowledged write and advances the gap-free prefix.
    fn acknowledge(&mut self, start: u64, end: u64) {
        self.acknowledged.insert(start, end);
        while let Some(end) = self.acknowledged.remove(&self.confirmed) {
            self.confirmed = end;
        }
    }
}

#[async_trait]
impl FileWriter for SftpWriter {
    async fn write(&mut self, data: Bytes) -> Result<()> {
        let Some(handle) = self.handle.clone() else {
            return Ok(());
        };
        let mut start = 0;
        while start < data.len() {
            while self.in_flight.len() >= WINDOW {
                self.wait_for_one().await?;
            }
            let end = (start + self.chunk).min(data.len());
            let piece = data.slice(start..end);
            let session = Arc::clone(&self.session);
            let handle = handle.clone();
            let path = self.path.clone();
            let at = self.offset;
            let len = (end - start) as u64;
            self.in_flight.push(
                async move {
                    session
                        .write(handle, at, piece.to_vec())
                        .await
                        .map(|_| (at, at + len))
                        .map_err(|error| sftp_error(&path, error))
                }
                .boxed(),
            );
            self.offset += (end - start) as u64;
            start = end;
        }
        Ok(())
    }

    fn confirmed(&self) -> u64 {
        self.confirmed
    }

    async fn finish(mut self: Box<Self>) -> Result<u64> {
        let mut first_error = None;
        while let Some(result) = self.in_flight.next().await {
            match result {
                Ok((start, end)) => self.acknowledge(start, end),
                Err(error) => {
                    first_error.get_or_insert(error);
                }
            }
        }
        if let Some(handle) = self.handle.take() {
            let closed = self
                .session
                .close(handle)
                .await
                .map(drop)
                .map_err(|error| sftp_error(&self.path, error));
            if let Err(error) = closed {
                first_error.get_or_insert(error);
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(self.offset),
        }
    }
}

impl Drop for SftpWriter {
    fn drop(&mut self) {
        // Abandoned (cancelled or failed): release the server-side handle.
        if let Some(handle) = self.handle.take()
            && let Ok(runtime) = tokio::runtime::Handle::try_current()
        {
            let session = Arc::clone(&self.session);
            runtime.spawn(async move {
                let _ = session.close(handle).await;
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_sizes_respect_limits() {
        let sizes = ChunkSizes::from_limits(0, 0);
        assert_eq!((sizes.read, sizes.write), (256 * 1024, 256 * 1024));
        let sizes = ChunkSizes::from_limits(261_120, 1024 * 1024);
        assert_eq!((sizes.read, sizes.write), (261_120, 256 * 1024));
        let sizes = ChunkSizes::from_limits(16_384, 8_192);
        assert_eq!((sizes.read, sizes.write), (16_384, 8_192));
    }
}
