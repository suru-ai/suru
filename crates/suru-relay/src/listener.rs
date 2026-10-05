//! What a Relay listens with: a listener holding the connections it takes to
//! the Relay's cap on connections at once, however the Relay listens — for
//! plain HTTP, or serving HTTPS itself, where the cap is applied before any
//! TLS handshake is made.
//!
//! A connection takes its place the moment it is taken and holds it until it
//! ends, however far it got: sending its request, proving its Server's key,
//! waiting to be reached, or carrying a join. One past the cap is let go as it
//! comes, before anything else is done with it, so nothing the Relay holds is
//! disturbed, and a Server let go connects again once there is room.
//!
//! A connection must begin its conversation with the Relay — make its TLS
//! handshake, where the Relay serves HTTPS, and open its WebSocket — within the
//! greeting timeout of being taken, or it is let go: one that says nothing, or
//! asks for something else and is kept open for its next request as HTTP
//! keeps one, would otherwise hold its place for good. From then on the
//! conversation bounds how long it may go saying nothing.

use std::{
    io,
    net::SocketAddr,
    num::NonZeroU32,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};

use axum::serve::Listener;
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    sync::{OwnedSemaphorePermit, Semaphore},
    time::Sleep,
};

/// A listener taking no more connections at once than its cap.
pub(crate) struct Capped<L> {
    listener: L,
    places: Arc<Semaphore>,
    cap: NonZeroU32,
    /// How long each connection taken has to begin its conversation.
    begin_within: Duration,
    /// Whether the last connection that came was let go, so the Relay's log
    /// says once that it is at its cap, and once that it is no longer.
    refusing: AtomicBool,
}

impl<L> Capped<L> {
    /// Takes the connections `listener` takes, no more than `cap` at once,
    /// letting go of each that has not begun its conversation within
    /// `begin_within` of being taken.
    pub(crate) fn new(listener: L, cap: NonZeroU32, begin_within: Duration) -> Self {
        Self {
            listener,
            places: Arc::new(Semaphore::new(cap.get() as usize)),
            cap,
            begin_within,
            refusing: AtomicBool::new(false),
        }
    }
}

impl<L> Listener for Capped<L>
where
    L: Listener<Addr = SocketAddr>,
{
    type Io = Counted<L::Io>;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            let (io, address) = self.listener.accept().await;
            match self.places.clone().try_acquire_owned() {
                Ok(place) => {
                    if self.refusing.swap(false, Ordering::Relaxed) {
                        tracing::info!(
                            "the Relay takes connections again, holding fewer than its cap"
                        );
                    }
                    let counted = Counted {
                        io,
                        _place: place,
                        begun: Begun::default(),
                        beginning_by: Some(Box::pin(tokio::time::sleep(self.begin_within))),
                    };
                    return (counted, address);
                }
                Err(_) => {
                    if !self.refusing.swap(true, Ordering::Relaxed) {
                        tracing::warn!(
                            "the Relay holds as many connections as it may at once, {}, so it lets \
                             each that comes go until one ends; raise `connections_at_once` if \
                             its Servers need more",
                            self.cap
                        );
                    }
                }
            }
        }
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.listener.local_addr()
    }
}

/// Says that a connection has begun its conversation with the Relay, so it
/// is no longer let go for not having.
#[derive(Clone, Debug, Default)]
pub(crate) struct Begun(Arc<AtomicBool>);

impl Begun {
    pub(crate) fn begin(&self) {
        self.0.store(true, Ordering::Release);
    }

    fn has(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

/// A connection the Relay took, holding its place against the cap until it
/// is dropped, and failing as it is read or written once it has gone past
/// the time it had to begin its conversation without beginning it.
pub(crate) struct Counted<S> {
    io: S,
    _place: OwnedSemaphorePermit,
    begun: Begun,
    /// When the connection must have begun its conversation by, until it
    /// has.
    beginning_by: Option<Pin<Box<Sleep>>>,
}

impl<S> Counted<S> {
    /// What says the connection has begun its conversation.
    pub(crate) fn begun(&self) -> &Begun {
        &self.begun
    }

    /// Whether the connection has gone past the time it had to begin its
    /// conversation without beginning it — waking `context` when it does,
    /// where it has yet to.
    fn too_late(&mut self, context: &mut Context<'_>) -> bool {
        let Some(beginning_by) = &mut self.beginning_by else {
            return false;
        };
        if self.begun.has() {
            self.beginning_by = None;
            return false;
        }
        beginning_by.as_mut().poll(context).is_ready()
    }
}

/// The failure of a connection that did not begin its conversation in time.
fn not_begun() -> io::Error {
    io::Error::new(
        io::ErrorKind::TimedOut,
        "the connection did not begin its conversation with the Relay in time",
    )
}

impl<S: AsyncRead + Unpin> AsyncRead for Counted<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.too_late(context) {
            return Poll::Ready(Err(not_begun()));
        }
        Pin::new(&mut self.io).poll_read(context, buffer)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Counted<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        if self.too_late(context) {
            return Poll::Ready(Err(not_begun()));
        }
        Pin::new(&mut self.io).poll_write(context, bytes)
    }

    fn poll_flush(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_flush(context)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_shutdown(context)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffers: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        if self.too_late(context) {
            return Poll::Ready(Err(not_begun()));
        }
        Pin::new(&mut self.io).poll_write_vectored(context, buffers)
    }

    fn is_write_vectored(&self) -> bool {
        self.io.is_write_vectored()
    }
}
