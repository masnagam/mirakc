use std::fmt;
use std::io;
use std::pin::Pin;

use actix_web::web::Bytes;
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::stream::{Stream, StreamExt};

use crate::error::Error;

pub struct MpegTsStream<T, S> {
    id: T,
    stream: S,
    range: Option<MpegTsStreamRange>,
    decoded: bool,
}

impl<T, S> MpegTsStream<T, S> {
    pub fn new(id: T, stream: S) -> Self {
        MpegTsStream { id, stream, decoded: false, range: None, }
    }

    pub fn with_range(id: T, stream: S, range: MpegTsStreamRange) -> Self {
        MpegTsStream { id, stream, decoded: false, range: Some(range), }
    }

    pub fn decoded(mut self) -> Self {
        self.decoded = true;
        self
    }

    pub fn is_decoded(&self) -> bool {
        self.decoded
    }

    pub fn range(&self) -> Option<MpegTsStreamRange> {
        self.range.clone()
    }
}

impl<T, S> MpegTsStream<T, S>
where
    T: Clone,
{
    pub fn id(&self) -> T {
        self.id.clone()
    }
}

impl<T, S> MpegTsStream<T, S>
where
    T: fmt::Display + Clone + Unpin,
    S: Stream<Item = io::Result<Bytes>> + Unpin,
{
    pub async fn pipe<W>(self, writer: W)
    where
        W: AsyncWrite + Unpin,
    {
        pipe(self, writer).await;
    }
}

impl<T, S> Stream for MpegTsStream<T, S>
where
    T: Unpin,
    S: Stream + Unpin,
{
    type Item = S::Item;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context
    ) -> std::task::Poll<Option<Self::Item>> {
        Pin::new(&mut self.stream).poll_next(cx)
    }
}

#[derive(Clone)]
pub struct MpegTsStreamRange {
    pub first: u64,
    pub last: u64,
    pub size: Option<u64>,
}

impl MpegTsStreamRange {
    pub fn bound(first: u64, size: u64) -> Result<Self, Error> {
        if size == 0 {
            return Err(Error::NoContent);
        }
        if first >= size {
            return Err(Error::OutOfRange);
        }
        Ok(Self::new(first, size - 1, Some(size)))
    }

    pub fn unbound(first: u64, size: u64) -> Result<Self, Error> {
        if size == 0 {
            return Err(Error::NoContent);
        }
        if first >= size {
            return Err(Error::OutOfRange);
        }
        Ok(Self::new(first, size - 1, None))
    }

    pub fn is_partial(&self) -> bool {
        if let Some(size) = self.size {
            self.first != 0 || self.last + 1 != size
        } else {
            true
        }
    }

    pub fn bytes(&self) -> u64 {
        self.last - self.first + 1
    }

    pub fn make_content_range(&self) -> String {
        if let Some(size) = self.size {
            format!("bytes {}-{}/{}", self.first, self.last, size)
        } else  {
            format!("bytes {}-{}/*", self.first, self.last)
        }
    }

    fn new(first: u64, last: u64, size: Option<u64>) -> Self {
        MpegTsStreamRange { first, last, size, }
    }
}

// terminator
//
// A terminator is attached on the output-side endpoint of an MPEG-TS packets
// filtering pipeline in order to shutting down streaming quickly when a HTTP
// transaction ends.
//
// There is a delay from the HTTP transaction end to the tuner release when
// using filters.  On some environments, the delay is about 40ms.  On those
// environments, the next streaming request may be processed before the tuner is
// released.
//
// It's impossible to eliminate the delay completely, but it's possible to
// reduce the delay as much as possible.
//
// See a discussion in Japanese on:
// https://github.com/mirakc/mirakc/issues/4#issuecomment-583818912.

pub struct MpegTsStreamTerminator<S, T> {
    inner: S,
    _stop_trigger: T,
}

impl<S, T> MpegTsStreamTerminator<S, T> {
    pub fn new(inner: S, _stop_trigger: T) -> Self {
        Self { inner, _stop_trigger }
    }
}

impl<S, T> Stream for MpegTsStreamTerminator<S, T>
where
    S: Stream + Unpin,
    T: Unpin,
{
    type Item = S::Item;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context
    ) -> std::task::Poll<Option<Self::Item>> {
        Pin::new(&mut self.inner).poll_next(cx)
    }
}

async fn pipe<T, S, W>(mut stream: MpegTsStream<T, S>, mut writer: W)
where
    T: fmt::Display + Clone + Unpin,
    S: Stream<Item = io::Result<Bytes>> + Unpin,
    W: AsyncWrite + Unpin,
{
    loop {
        match stream.next().await {
            Some(Ok(chunk)) => {
                log::trace!("{}: Received a chunk of {} bytes",
                            stream.id(), chunk.len());
                if let Err(err) = writer.write_all(&chunk).await {
                    if err.kind() == io::ErrorKind::BrokenPipe {
                        log::debug!("{}: Downstream has been closed",
                                    stream.id());
                    } else {
                        log::error!("{}: Failed to write to downstream: {}",
                                    stream.id(), err);
                    }
                    break;
                }
            }
            Some(Err(err)) => {
                if err.kind() == io::ErrorKind::BrokenPipe {
                    log::debug!("{}: Upstream has been closed", stream.id());
                } else {
                    log::error!("{}: Failed to read from upstream: {}",
                                stream.id(), err);
                }
                break;
            }
            None => {
                log::debug!("{}: EOF reached", stream.id());
                break;
            }
        }

        // TODO: Should yield here like web::streaming()?
    }

    if let Err(err) = writer.shutdown().await {
        log::warn!("{}: Failed to shutdown: {}", stream.id(), err);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[actix_rt::test]
    async fn test_pipe() {
        let (mut tx, rx) = tokio::sync::mpsc::channel(1);

        let stream = MpegTsStream::new(0, rx);
        let writer = TestWriter::new(b"hello");
        let handle = tokio::spawn(stream.pipe(writer));

        let result = tx.send(Ok(Bytes::from("hello"))).await;
        assert!(result.is_ok());

        drop(tx);
        let _ = handle.await.unwrap();
    }

    struct TestWriter {
        buf: Vec<u8>,
        expected: &'static [u8],
    }

    impl TestWriter {
        fn new(expected: &'static [u8]) -> Self {
            Self { buf: Vec::new(), expected }
        }
    }

    impl AsyncWrite for TestWriter {
        fn poll_write(
            mut self: Pin<&mut Self>,
            _: &mut std::task::Context,
            buf: &[u8]
        ) -> std::task::Poll<io::Result<usize>> {
            self.buf.extend_from_slice(buf);
            std::task::Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(
            self: Pin<&mut Self>,
            _: &mut std::task::Context
        ) -> std::task::Poll<io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn poll_shutdown(
            self: Pin<&mut Self>,
            _: &mut std::task::Context
        ) -> std::task::Poll<io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    impl Drop for TestWriter {
        fn drop(&mut self) {
            assert_eq!(self.buf.as_slice(), self.expected);
        }
    }
}
