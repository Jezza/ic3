//! The websocket behind a trait (spec §4.6 "`Transport` trait"), so the
//! trouter state machine runs against a fake in tests.

use std::time::Duration;

use async_trait::async_trait;
use futures::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::{HeaderValue, header};
use tokio_tungstenite::tungstenite::{Error as WsError, Message};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tracing::debug;

use crate::auth::USER_AGENT;
use crate::redact::{redact_text, redact_url};
use crate::{Error, Result, Secret};

/// One open text-frame connection.
#[async_trait]
pub trait Transport: Send {
    /// Sends one text frame.
    async fn send(&mut self, text: String) -> Result<()>;
    /// The next text frame; `Ok(None)` once the connection has closed.
    /// Must be cancel-safe: the state machine polls it inside `select!`.
    async fn recv(&mut self) -> Result<Option<String>>;
    /// Closes the connection; errors are ignored.
    async fn close(&mut self);
}

/// Opens [`Transport`]s.
#[async_trait]
pub trait TransportFactory: Send + Sync {
    /// Connects to `url` (a credential: never log it unredacted), sending
    /// the Skype token as `X-Skypetoken`.
    async fn connect(&self, url: &str, skype_token: &Secret<String>) -> Result<Box<dyn Transport>>;
}

/// How long the websocket handshake (TCP, TLS and HTTP upgrade) may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// How long [`Transport::close`] waits for the close handshake. The state
/// machine awaits `close` on cancel, so it must never hang on a silent peer.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(1);

/// The real websocket (tokio-tungstenite, rustls with webpki roots), sending
/// `X-Skypetoken` and the Teams `User-Agent` on the upgrade (spec §4.6 step 3).
#[derive(Debug, Clone, Copy, Default)]
pub struct WsTransportFactory;

#[async_trait]
impl TransportFactory for WsTransportFactory {
    async fn connect(&self, url: &str, skype_token: &Secret<String>) -> Result<Box<dyn Transport>> {
        debug!(url = %redact_url(url), "websocket connecting");
        let mut req = url
            .into_client_request()
            .map_err(|e| ws_error(&e, Some(url)))?;
        let token = HeaderValue::from_str(skype_token.expose())
            .map_err(|_| Error::Protocol("skype token is not a valid header value".into()))?;
        let headers = req.headers_mut();
        headers.insert("X-Skypetoken", token);
        headers.insert(header::USER_AGENT, HeaderValue::from_static(USER_AGENT));
        let (ws, _) = tokio::time::timeout(CONNECT_TIMEOUT, tokio_tungstenite::connect_async(req))
            .await
            .map_err(|_| Error::Network("websocket connect timed out".into()))?
            .map_err(|e| ws_error(&e, Some(url)))?;
        Ok(Box::new(WsTransport { ws }))
    }
}

/// Error text for a websocket failure, with `url` (whose socket.io query is
/// a credential) and anything else secret-shaped redacted (spec §4.11).
fn ws_error(e: &WsError, url: Option<&str>) -> Error {
    let mut text = e.to_string();
    if let Some(url) = url.filter(|u| !u.is_empty()) {
        text = text.replace(url, &redact_url(url));
    }
    Error::Network(format!("websocket: {}", redact_text(&text)))
}

struct WsTransport {
    ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
}

#[async_trait]
impl Transport for WsTransport {
    async fn send(&mut self, text: String) -> Result<()> {
        self.ws
            .send(Message::text(text))
            .await
            .map_err(|e| ws_error(&e, None))
    }

    /// Cancel-safe: `StreamExt::next` holds no partial state across awaits,
    /// and skipped non-text frames are already consumed.
    async fn recv(&mut self) -> Result<Option<String>> {
        loop {
            match self.ws.next().await {
                None | Some(Ok(Message::Close(_))) => return Ok(None),
                Some(Ok(Message::Text(t))) => return Ok(Some(t.to_string())),
                // Ping/pong (tungstenite answers pings itself), binary, raw frames.
                Some(Ok(_)) => {}
                Some(Err(WsError::ConnectionClosed | WsError::AlreadyClosed)) => return Ok(None),
                Some(Err(e)) => return Err(ws_error(&e, None)),
            }
        }
    }

    async fn close(&mut self) {
        let handshake = async {
            if self.ws.close(None).await.is_ok() {
                // Drain until the peer's close reply (or EOF) completes the handshake.
                while let Some(Ok(_)) = self.ws.next().await {}
            }
        };
        if tokio::time::timeout(CLOSE_TIMEOUT, handshake)
            .await
            .is_err()
        {
            debug!("websocket close handshake timed out");
        }
    }
}

#[cfg(test)]
pub(crate) use fake::{FakeConn, FakeFactory};

#[cfg(test)]
mod fake {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use async_trait::async_trait;
    use tokio::sync::mpsc;

    use super::{Transport, TransportFactory};
    use crate::{Error, Result, Secret};

    /// The test's end of one fake connection.
    #[derive(Debug)]
    pub(crate) struct FakeConn {
        /// Frames for the client to receive; `Ok(None)` or dropping this
        /// sender closes the connection, `Err` is a socket error.
        pub to_client: mpsc::Sender<Result<Option<String>>>,
        /// Frames the client sent.
        pub from_client: mpsc::UnboundedReceiver<String>,
        /// The URL the client connected to.
        pub url: String,
        /// The Skype token the client connected with.
        pub skype_token: String,
    }

    /// Hands every connection the client opens to the test, as a
    /// [`FakeConn`].
    #[derive(Debug)]
    pub(crate) struct FakeFactory {
        conns: mpsc::UnboundedSender<FakeConn>,
        /// When set, the next `connect` fails (once).
        pub fail_next: AtomicBool,
    }

    impl FakeFactory {
        pub fn new() -> (Arc<FakeFactory>, mpsc::UnboundedReceiver<FakeConn>) {
            let (conns, rx) = mpsc::unbounded_channel();
            let factory = FakeFactory {
                conns,
                fail_next: AtomicBool::new(false),
            };
            (Arc::new(factory), rx)
        }
    }

    struct FakeTransport {
        rx: mpsc::Receiver<Result<Option<String>>>,
        tx: mpsc::UnboundedSender<String>,
        closed: bool,
    }

    #[async_trait]
    impl Transport for FakeTransport {
        async fn send(&mut self, text: String) -> Result<()> {
            if self.closed {
                return Err(Error::Network("fake transport closed".into()));
            }
            self.tx
                .send(text)
                .map_err(|_| Error::Network("fake peer gone".into()))
        }

        async fn recv(&mut self) -> Result<Option<String>> {
            if self.closed {
                return Ok(None);
            }
            self.rx.recv().await.unwrap_or(Ok(None))
        }

        async fn close(&mut self) {
            self.closed = true;
        }
    }

    #[async_trait]
    impl TransportFactory for FakeFactory {
        async fn connect(
            &self,
            url: &str,
            skype_token: &Secret<String>,
        ) -> Result<Box<dyn Transport>> {
            if self.fail_next.swap(false, Ordering::SeqCst) {
                return Err(Error::Network("fake connect failure".into()));
            }
            let (to_client, rx) = mpsc::channel(64);
            let (tx, from_client) = mpsc::unbounded_channel();
            self.conns
                .send(FakeConn {
                    to_client,
                    from_client,
                    url: url.to_string(),
                    skype_token: skype_token.expose().clone(),
                })
                .map_err(|_| Error::Network("fake factory: test gone".into()))?;
            Ok(Box::new(FakeTransport {
                rx,
                tx,
                closed: false,
            }))
        }
    }
}

#[cfg(test)]
mod ws_tests {
    use std::time::Duration;

    use futures::{SinkExt, StreamExt};
    use tokio::net::TcpListener;
    use tokio_tungstenite::tungstenite;

    use super::{TransportFactory, WsTransportFactory};
    use crate::{Error, Secret};

    #[tokio::test]
    #[expect(
        clippy::result_large_err,
        reason = "tungstenite's accept_hdr callback signature fixes the error type"
    )]
    async fn ws_transport_sends_headers_and_round_trips_text() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_hdr_async(
                stream,
                |req: &tungstenite::handshake::server::Request, resp| {
                    assert_eq!(req.headers().get("X-Skypetoken").unwrap(), "sk");
                    assert!(
                        req.headers()
                            .get("User-Agent")
                            .unwrap()
                            .to_str()
                            .unwrap()
                            .contains("Teams/")
                    );
                    Ok(resp)
                },
            )
            .await
            .unwrap();
            ws.send(tungstenite::Message::text("1::")).await.unwrap();
            if let Some(Ok(tungstenite::Message::Text(t))) = ws.next().await {
                ws.send(tungstenite::Message::text(format!("echo:{t}")))
                    .await
                    .unwrap();
            }
            ws.close(None).await.ok();
        });
        let f = WsTransportFactory;
        let mut t = f
            .connect(
                &format!("ws://{addr}/socket.io/1/websocket/sid?v=v4&sig=x"),
                &Secret::new("sk".into()),
            )
            .await
            .unwrap();
        assert_eq!(t.recv().await.unwrap(), Some("1::".into()));
        t.send("5:1+::{\"name\":\"ping\"}".into()).await.unwrap();
        assert_eq!(
            t.recv().await.unwrap(),
            Some("echo:5:1+::{\"name\":\"ping\"}".into())
        );
        assert_eq!(t.recv().await.unwrap(), None);
    }

    /// Ping/pong and binary frames are skipped; only text reaches the caller.
    #[tokio::test]
    async fn recv_skips_non_text_frames() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            ws.send(tungstenite::Message::Ping(vec![1].into()))
                .await
                .unwrap();
            ws.send(tungstenite::Message::Binary(vec![2].into()))
                .await
                .unwrap();
            ws.send(tungstenite::Message::text("2::")).await.unwrap();
            ws.close(None).await.ok();
        });
        let mut t = WsTransportFactory
            .connect(
                &format!("ws://{addr}/socket.io/1/websocket/sid?v=v4"),
                &Secret::new("sk".into()),
            )
            .await
            .unwrap();
        assert_eq!(t.recv().await.unwrap(), Some("2::".into()));
        assert_eq!(t.recv().await.unwrap(), None);
    }

    /// `close` must not hang when the peer never answers the close
    /// handshake: the state machine awaits it on cancel (§6.0.5).
    #[tokio::test]
    async fn close_is_bounded_when_peer_is_silent() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (hold_tx, hold_rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            // Keep the socket open without ever reading from it.
            let _ = hold_rx.await;
            drop(ws);
        });
        let mut t = WsTransportFactory
            .connect(
                &format!("ws://{addr}/socket.io/1/websocket/sid?v=v4"),
                &Secret::new("sk".into()),
            )
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), t.close())
            .await
            .expect("close returned within its bound");
        drop(hold_tx);
    }

    /// tungstenite's `UnableToConnect` carries the full URL; it is redacted.
    #[test]
    fn ws_error_redacts_a_url_inside_the_error() {
        let url = "wss://h.example/socket.io/1/websocket/sid?v=v4&sig=SECRETSIG&tc=%7B%7D";
        let e = tungstenite::Error::Url(tungstenite::error::UrlError::UnableToConnect(
            url.to_string(),
        ));
        let text = super::ws_error(&e, Some(url)).to_string();
        assert!(!text.contains("SECRET"), "{text}");
        assert!(
            text.contains("socket.io/1/websocket/sid?<redacted>"),
            "{text}"
        );
    }

    /// Neither a TLS failure nor a refused connection may leak the
    /// socket.io query into the error text (§4.11); building the TLS
    /// config must not panic (a single rustls crypto provider is enabled).
    #[tokio::test]
    async fn connect_errors_never_contain_the_query() {
        // TLS handshake against a peer that hangs up immediately.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                drop(stream);
            }
        });
        let query = "v=v4&sr=SECRETSR&sig=SECRETSIG&ccid=SECRETCCID";
        let refused = {
            let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
            l.local_addr().unwrap()
        };
        for url in [
            format!("wss://{addr}/socket.io/1/websocket/sid?{query}"),
            format!("ws://{addr}/socket.io/1/websocket/sid?{query}"),
            format!("ws://{refused}/socket.io/1/websocket/sid?{query}"),
            format!("ftp://{addr}/socket.io/1/websocket/sid?{query}"),
        ] {
            let Err(e) = WsTransportFactory
                .connect(&url, &Secret::new("SECRETTOKEN".into()))
                .await
            else {
                panic!("connect to {url} should fail");
            };
            let text = e.to_string();
            assert!(matches!(e, Error::Network(_)), "{url}: {text}");
            assert!(!text.contains("SECRET"), "{url}: {text}");
        }
    }
}
