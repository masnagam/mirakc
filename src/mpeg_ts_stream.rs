use std::io;
use std::pin::Pin;

use actix_web::web::Bytes;
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::stream::{Stream, StreamExt};

pub use crate::tuner::TunerSubscriptionId as MpegTsStreamId;

pub struct MpegTsStream<S> {
    id: MpegTsStreamId,
    stream: S,
}

impl<S> MpegTsStream<S> {
    pub fn new(id: MpegTsStreamId, stream: S) -> Self {
        MpegTsStream { id, stream, }
    }

    pub fn id(&self) -> MpegTsStreamId {
        self.id
    }
}

impl<S> MpegTsStream<S>
where
    S: Stream<Item = io::Result<Bytes>> + Unpin,
{
    pub async fn pipe<W>(self, writer: W)
    where
        W: AsyncWrite + Unpin,
    {
        pipe(self, writer).await;
    }
}

impl<S> Stream for MpegTsStream<S>
where
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

async fn pipe<S, W>(mut stream: MpegTsStream<S>, mut writer: W)
where
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

        let stream = MpegTsStream::new(Default::default(), rx);
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
