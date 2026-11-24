//! `AsyncRead`/`AsyncWrite` adapter over a bridged worker WebSocket.
//!
//! Protocol-critical rule: after the `CONNECTED` handshake the server MUST
//! send only Binary frames (Close at teardown). The client bridge loop
//! (`src-tauri/src/tunnel.rs`) terminates on ANY non-Binary frame, so this
//! type never emits Ping or Text.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket};
use futures_util::{Sink, SinkExt, Stream};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::time::{sleep_until, Instant};

use super::registry::TunnelError;

/// Max bytes placed into a single Binary frame on write.
const MAX_FRAME: usize = 16_384;

/// Byte stream over a worker WebSocket after the CONNECT handshake.
pub struct WsByteStream {
    ws: WebSocket,
    /// Leftover bytes from a Binary frame larger than the caller's buffer.
    read_buf: Vec<u8>,
    read_pos: usize,
    eof: bool,
}

impl WsByteStream {
    pub(crate) fn new(ws: WebSocket) -> Self {
        Self { ws, read_buf: Vec::new(), read_pos: 0, eof: false }
    }

    /// Perform the client `CONNECT {host} {port}` handshake on a freshly
    /// handed-over worker socket, returning a bridged byte stream on
    /// `CONNECTED`.
    pub(crate) async fn establish(
        mut ws: WebSocket,
        host: &str,
        port: u16,
        timeout: Duration,
    ) -> Result<Self, TunnelError> {
        let cmd = format!("CONNECT {host} {port}");
        ws.send(Message::Text(cmd.into()))
            .await
            .map_err(|e| TunnelError::Io(e.to_string()))?;
        let deadline = Instant::now() + timeout;
        loop {
            tokio::select! {
                msg = ws.recv() => {
                    match msg {
                        Some(Ok(Message::Text(t))) => {
                            let s = t.as_str();
                            if s == "CONNECTED" {
                                return Ok(Self::new(ws));
                            }
                            if let Some(rest) = s.strip_prefix("ERROR") {
                                let detail = rest.trim();
                                let detail = if detail.is_empty() {
                                    "worker reported error".to_string()
                                } else {
                                    detail.to_string()
                                };
                                return Err(TunnelError::RemoteConnectFailed(detail));
                            }
                            // Any other Text frame: ignore, keep waiting.
                        }
                        Some(Ok(Message::Ping(_))) => {
                            // tungstenite queued an automatic Pong response.
                            let _ = ws.flush().await;
                        }
                        Some(Ok(Message::Pong(_))) | Some(Ok(Message::Binary(_))) => {}
                        Some(Ok(Message::Close(_))) | None | Some(Err(_)) => {
                            return Err(TunnelError::RemoteConnectFailed(
                                "worker closed connection during handshake".into(),
                            ));
                        }
                    }
                }
                _ = sleep_until(deadline) => {
                    return Err(TunnelError::HandshakeTimeout);
                }
            }
        }
    }
}

impl AsyncRead for WsByteStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        loop {
            if this.read_pos < this.read_buf.len() {
                let avail = &this.read_buf[this.read_pos..];
                let n = avail.len().min(buf.remaining());
                buf.initialize_unfilled_to(n).copy_from_slice(&avail[..n]);
                buf.advance(n);
                this.read_pos += n;
                if this.read_pos == this.read_buf.len() {
                    this.read_buf.clear();
                    this.read_pos = 0;
                }
                return Poll::Ready(Ok(()));
            }
            if this.eof || buf.remaining() == 0 {
                return Poll::Ready(Ok(()));
            }
            match Pin::new(&mut this.ws).poll_next(cx) {
                Poll::Ready(Some(Ok(Message::Binary(b)))) => {
                    if b.is_empty() {
                        // Empty data frame: not EOF, keep polling for more.
                        continue;
                    }
                    let n = b.len().min(buf.remaining());
                    buf.initialize_unfilled_to(n).copy_from_slice(&b[..n]);
                    buf.advance(n);
                    if n < b.len() {
                        this.read_buf = b[n..].to_vec();
                        this.read_pos = 0;
                    }
                    return Poll::Ready(Ok(()));
                }
                Poll::Ready(Some(Ok(Message::Close(_)))) | Poll::Ready(None) => {
                    this.eof = true;
                    return Poll::Ready(Ok(()));
                }
                Poll::Ready(Some(Err(e))) => {
                    // Surface transport failures instead of masking them as a
                    // clean EOF (which would silently truncate the stream).
                    this.eof = true;
                    return Poll::Ready(Err(io::Error::other(e.to_string())));
                }
                Poll::Ready(Some(Ok(_))) => {
                    // Unexpected Text/Ping/Pong from the worker after the
                    // handshake: protocol violation, treat as EOF to be safe.
                    this.eof = true;
                    return Poll::Ready(Ok(()));
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

impl AsyncWrite for WsByteStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let n = buf.len().min(MAX_FRAME);
        futures_util::ready!(Pin::new(&mut this.ws)
            .poll_ready(cx)
            .map_err(|e| io::Error::other(e.to_string())))?;
        Pin::new(&mut this.ws)
            .start_send(Message::Binary(buf[..n].to_vec().into()))
            .map_err(|e| io::Error::other(e.to_string()))?;
        Poll::Ready(Ok(n))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        Pin::new(&mut this.ws)
            .poll_flush(cx)
            .map_err(|e| io::Error::other(e.to_string()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // Best-effort Close frame. A close handshake that cannot complete is
        // finished (or the socket dropped) when the WebSocket is dropped, so
        // Pending is treated as done to avoid hanging teardown on a dead peer.
        let this = self.get_mut();
        let mut cx = Context::from_waker(Waker::noop());
        let _ = Pin::new(&mut this.ws).poll_close(&mut cx);
        Poll::Ready(Ok(()))
    }
}
