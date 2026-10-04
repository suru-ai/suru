//! The local API's listener, whose connections a stop outlasting its deadline
//! can cut.
//!
//! A graceful stop lets every request in flight finish before the listener
//! is done, and a request can take as long as its client does: an Attachment
//! upload whose client stops sending its body, or a client that never reads
//! an answer, holds the stop up for as long as the client lives. So each
//! connection the listener accepts can be cut at once, as the Serving
//! listener's can (`serving`): every read or write after it is cut fails,
//! which ends the connection and drops whatever request was in flight on it.

use std::{
    io,
    net::SocketAddr,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use axum::serve::Listener;
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::{TcpListener, TcpStream},
};

use crate::serving::{ConnectionRevocation, RevocableConnections};

/// The local API's loopback listener, holding on to what cuts each
/// connection it has accepted.
pub(super) struct LoopbackListener {
    listener: TcpListener,
    connections: Arc<RevocableConnections>,
}

impl LoopbackListener {
    pub(super) fn new(listener: TcpListener, connections: Arc<RevocableConnections>) -> Self {
        Self {
            listener,
            connections,
        }
    }
}

impl Listener for LoopbackListener {
    type Io = CuttableStream;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        // Accepting as axum accepts on a bare TCP listener, so a failure to
        // accept is retried just as it was.
        let (stream, address) = Listener::accept(&mut self.listener).await;
        (
            CuttableStream {
                stream,
                revocation: self.connections.register(),
            },
            address,
        )
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.listener.local_addr()
    }
}

/// One accepted connection, which fails every read and write once it is cut.
pub(super) struct CuttableStream {
    stream: TcpStream,
    revocation: Arc<ConnectionRevocation>,
}

impl CuttableStream {
    fn cut_error() -> io::Error {
        io::Error::new(
            io::ErrorKind::ConnectionAborted,
            "the server's stop outlasted its deadline and cut this connection",
        )
    }
}

impl AsyncRead for CuttableStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.revocation.poll(context) {
            return Poll::Ready(Err(Self::cut_error()));
        }
        Pin::new(&mut self.stream).poll_read(context, buffer)
    }
}

impl AsyncWrite for CuttableStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<io::Result<usize>> {
        if self.revocation.poll(context) {
            return Poll::Ready(Err(Self::cut_error()));
        }
        Pin::new(&mut self.stream).poll_write(context, buffer)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffers: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        if self.revocation.poll(context) {
            return Poll::Ready(Err(Self::cut_error()));
        }
        Pin::new(&mut self.stream).poll_write_vectored(context, buffers)
    }

    fn is_write_vectored(&self) -> bool {
        self.stream.is_write_vectored()
    }

    fn poll_flush(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self.revocation.poll(context) {
            return Poll::Ready(Err(Self::cut_error()));
        }
        Pin::new(&mut self.stream).poll_flush(context)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self.revocation.poll(context) {
            return Poll::Ready(Err(Self::cut_error()));
        }
        Pin::new(&mut self.stream).poll_shutdown(context)
    }
}
