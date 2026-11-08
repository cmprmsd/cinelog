//! Streamed response bodies produced on a blocking thread: memory stays bounded by one
//! chunk per channel slot, and production stops when the client goes away.

use std::convert::Infallible;

use axum::body::Body;
use bytes::Bytes;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use crate::error::{Result, WebError};

const CHUNK: usize = 64 << 10;
const SLOTS: usize = 8;

/// Buffered writer into a streamed body.
pub(crate) struct Sink {
    buf: Vec<u8>,
    tx: mpsc::Sender<Result<Bytes, Infallible>>,
}

impl Sink {
    pub fn buf(&mut self) -> &mut Vec<u8> {
        &mut self.buf
    }

    pub fn write(&mut self, data: &[u8]) -> Result<()> {
        self.buf.extend_from_slice(data);
        self.maybe_flush()
    }

    /// Sends the buffer once it holds a chunk. `Err` when the client is gone.
    pub fn maybe_flush(&mut self) -> Result<()> {
        if self.buf.len() >= CHUNK { self.flush() } else { Ok(()) }
    }

    fn flush(&mut self) -> Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let chunk = Bytes::from(std::mem::replace(&mut self.buf, Vec::with_capacity(CHUNK)));
        self.tx
            .blocking_send(Ok(chunk))
            .map_err(|_| WebError::Internal("client went away".into()))
    }
}

/// A body whose bytes `produce` writes on a blocking thread. `produce` runs to its first
/// error; an error after the response started ends the body early (logged).
pub(crate) fn streamed<F>(what: &'static str, produce: F) -> Body
where
    F: FnOnce(&mut Sink) -> Result<()> + Send + 'static,
{
    let (tx, rx) = mpsc::channel(SLOTS);
    tokio::task::spawn_blocking(move || {
        let mut sink = Sink {
            buf: Vec::with_capacity(CHUNK),
            tx,
        };
        match produce(&mut sink).and_then(|()| sink.flush()) {
            Ok(()) => {}
            Err(e) if sink.tx.is_closed() => tracing::debug!("{what}: client went away ({e})"),
            Err(e) => tracing::warn!("{what} ended early: {e}"),
        }
    });
    Body::from_stream(ReceiverStream::new(rx))
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt;

    #[tokio::test]
    async fn streams_everything_in_order() {
        let body = streamed("test", |s| {
            for i in 0..100_000u32 {
                s.write(format!("{i}\n").as_bytes())?;
            }
            Ok(())
        });
        let bytes = body.collect().await.unwrap().to_bytes();
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        assert_eq!(text.lines().count(), 100_000);
        assert!(text.ends_with("99999\n"));
    }
}
