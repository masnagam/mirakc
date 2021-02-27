use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use actix_web::web::{Bytes, BytesMut};
use tokio::io::AsyncRead;
use tokio::stream::Stream;

// ChunkStream provides a stream of data chunks with a specific maximum size.
//
// There is a similar types in tokio like tokio_codec::FramedRead, but these
// types don't have methods to change the internal chunk size
// `INITIAL_CAPACITY` that is fixed to 8 KB.
pub struct ChunkStream<R> {
    reader: R,
    chunk_size: usize,
}

impl<R> ChunkStream<R> {
    pub fn new(reader: R, chunk_size: usize) -> Self {
        ChunkStream { reader, chunk_size }
    }

    #[cfg(test)]
    pub fn new_for_test(reader: R) -> Self {
        ChunkStream { reader, chunk_size: 4096 }
    }
}

impl<R> Stream for ChunkStream<R>
where
    R: AsyncRead + Unpin
{
    type Item = io::Result<Bytes>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut Context
    ) -> Poll<Option<Self::Item>> {
        let mut chunk = BytesMut::with_capacity(self.chunk_size);
        loop {
            match Pin::new(&mut self.reader).poll_read_buf(cx, &mut chunk) {
                Poll::Ready(Ok(0)) => {
                    if chunk.is_empty() {
                        return Poll::Ready(None);
                    } else {
                        return Poll::Ready(Some(Ok(chunk.freeze())));
                    }
                }
                Poll::Ready(Ok(_)) => {
                    if chunk.len() >= self.chunk_size {
                        return Poll::Ready(Some(Ok(chunk.freeze())));
                    }
                }
                Poll::Ready(Err(err)) => {
                    return Poll::Ready(Some(Err(err)));
                }
                Poll::Pending => {
                    if chunk.is_empty() {
                        return Poll::Pending;
                    } else {
                        return Poll::Ready(Some(Ok(chunk.freeze())));
                    }
                }
            }
        }
    }
}
