//! Transport abstraction for the gateway's stream servers (#1122).
//!
//! The JSON-RPC ingress and the OFP federation listener both hard-coded
//! `tokio::net::TcpListener`/`TcpStream`, leaving no seam for a Unix-socket
//! listener, a TLS-native transport, or an in-process transport for tests.
//! The codecs (line-delimited JSON, 4-byte length framing) were already
//! transport-agnostic — only the byte streams were coupled.
//!
//! This module is that seam, deliberately minimal:
//!
//! - [`Connection`] — blanket trait over anything that is
//!   `AsyncRead + AsyncWrite + Unpin + Send`; no new methods to implement
//! - [`TransportListener`] — accept loop + local addr
//! - [`TcpListenerAdapter`] — the production impl (behavior unchanged)
//! - [`memory_transport`] — an in-process listener/connector pair for tests
//!   and embedders; proves the trait is real, not decorative
//!
//! Handlers own a `BoxedConnection` and split it with `tokio::io::split`
//! (works for any `AsyncRead + AsyncWrite`, unlike the TcpStream-only
//! `into_split`).

use std::io;
use std::net::SocketAddr;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpListener;

/// Anything a server handler or client can read and write framed messages
/// over. Blanket-implemented for all qualifying types, so `TcpStream`,
/// `DuplexStream`, TLS streams, etc. need zero adapter code.
pub trait Connection: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Connection for T {}

/// Owned, type-erased connection.
pub type BoxedConnection = Box<dyn Connection>;

/// Accepted-stream source for the server accept loops.
///
/// `&mut self`: accept loops own the listener exclusively, and channel-based
/// transports need `&mut` to drain their receiver.
#[async_trait::async_trait]
pub trait TransportListener: Send + 'static {
    /// Await the next inbound connection.
    async fn accept(&mut self) -> io::Result<(BoxedConnection, SocketAddr)>;
    /// Address the listener is bound to (for logs and startup banners).
    fn local_addr(&self) -> io::Result<SocketAddr>;
}

/// Production transport: TCP, exactly the pre-#1122 behavior.
pub struct TcpListenerAdapter {
    inner: TcpListener,
}

impl TcpListenerAdapter {
    pub async fn bind(addr: SocketAddr) -> io::Result<Self> {
        Ok(Self {
            inner: TcpListener::bind(addr).await?,
        })
    }

    /// Adopt an already-bound listener (tests bind port 0 and read the addr).
    pub fn new(inner: TcpListener) -> Self {
        Self { inner }
    }
}

#[async_trait::async_trait]
impl TransportListener for TcpListenerAdapter {
    async fn accept(&mut self) -> io::Result<(BoxedConnection, SocketAddr)> {
        let (stream, peer) = self.inner.accept().await?;
        let _ = stream.set_nodelay(true);
        Ok((Box::new(stream), peer))
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.local_addr()
    }
}

/// In-process transport (#1122): a listener/connector pair wired by a channel
/// and `tokio::io::duplex`. Server code cannot tell it apart from TCP, which
/// is the point — integration tests can drive a full JSON-RPC handshake with
/// no sockets, no ports, no `serial_test`.
pub struct MemoryListener {
    rx: tokio::sync::mpsc::Receiver<(BoxedConnection, SocketAddr)>,
    local: SocketAddr,
}

/// Client side of [`memory_transport`]: hands out duplex streams whose other
/// ends the paired listener will accept.
pub struct MemoryConnector {
    tx: tokio::sync::mpsc::Sender<(BoxedConnection, SocketAddr)>,
    local: SocketAddr,
}

impl MemoryConnector {
    /// Open a new in-memory connection. The server end is queued for the
    /// listener's next `accept`.
    pub async fn connect(&self) -> io::Result<tokio::io::DuplexStream> {
        let (client, server) = tokio::io::duplex(64 * 1024);
        self.tx
            .send((Box::new(server), self.local))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::ConnectionAborted, "listener closed"))?;
        Ok(client)
    }
}

/// Create a connected in-memory transport pair.
pub fn memory_transport() -> (MemoryListener, MemoryConnector) {
    // Fairly generous queue: a test client may open several connections
    // before the server accept loop drains them.
    let (tx, rx) = tokio::sync::mpsc::channel(32);
    let local = SocketAddr::from(([127, 0, 0, 1], 0));
    (
        MemoryListener { rx, local },
        MemoryConnector { tx, local },
    )
}

#[async_trait::async_trait]
impl TransportListener for MemoryListener {
    async fn accept(&mut self) -> io::Result<(BoxedConnection, SocketAddr)> {
        self.rx
            .recv()
            .await
            .ok_or_else(|| io::Error::new(io::ErrorKind::ConnectionAborted, "transport closed"))
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.local)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn memory_transport_roundtrips_bytes() {
        let (listener, connector) = memory_transport();
        let mut client = connector.connect().await.expect("connect");

        let mut listener = listener;
        let server = tokio::spawn(async move {
            let (mut conn, _peer) = listener.accept().await.expect("accept");
            let mut buf = [0u8; 5];
            conn.read_exact(&mut buf).await.expect("server read");
            conn.write_all(b"pong!").await.expect("server write");
        });

        client.write_all(b"ping!").await.expect("client write");
        let mut buf = [0u8; 5];
        client.read_exact(&mut buf).await.expect("client read");
        assert_eq!(&buf, b"pong!");
        server.await.expect("server task");
    }

    #[tokio::test]
    async fn tcp_adapter_accepts_real_sockets() {
        let listener = TcpListenerAdapter::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("local addr");
        let mut listener = listener;
        let server = tokio::spawn(async move {
            let (mut conn, peer) = listener.accept().await.expect("accept");
            assert!(peer.port() != 0);
            let mut buf = [0u8; 2];
            conn.read_exact(&mut buf).await.expect("read");
        });
        let mut client = tokio::net::TcpStream::connect(addr).await.expect("connect");
        client.write_all(b"hi").await.expect("write");
        server.await.expect("server task");
    }
}
