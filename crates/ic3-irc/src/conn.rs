//! IRC connections: accepting them, registration (`CAP`, `PASS`, `NICK`,
//! `USER`, SASL `PLAIN`), keepalive, and handing registered clients to the
//! gateway.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use base64::Engine;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::caps::{self, Caps};
use crate::gateway::{CLIENT_QUEUE, ClientId, Input};
use crate::irc::{MAX_LINE, Message};

/// How long a connection may take to register.
const REGISTER_TIMEOUT: Duration = Duration::from_secs(60);
/// A client silent this long is pinged; silent for twice as long, dropped.
const IDLE_PING: Duration = Duration::from_secs(120);

/// What connections need to know.
#[derive(Clone, Debug)]
pub struct ConnConfig {
    pub server_name: String,
    /// When set, clients must give it with `PASS` or SASL `PLAIN`.
    pub password: Option<String>,
}

/// Accepts connections until the listener fails.
pub async fn serve(
    listener: TcpListener,
    inputs: mpsc::Sender<Input>,
    cfg: Arc<ConnConfig>,
) -> std::io::Result<()> {
    static NEXT_ID: AtomicU64 = AtomicU64::new(1);
    loop {
        let (stream, peer) = listener.accept().await?;
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        tracing::info!(client = id, %peer, "connection");
        let _ = stream.set_nodelay(true);
        let (r, w) = stream.into_split();
        let inputs = inputs.clone();
        let cfg = cfg.clone();
        tokio::spawn(async move {
            handle(id, r, w, inputs, cfg).await;
            tracing::info!(client = id, "connection closed");
        });
    }
}

/// Registration state.
#[derive(Debug, Default)]
struct Reg {
    nick: Option<String>,
    user: bool,
    /// `CAP LS` seen and `CAP END` not yet: registration waits.
    negotiating: bool,
    caps: Caps,
    authed: bool,
    sasl: Sasl,
}

#[derive(Debug, Default, PartialEq)]
enum Sasl {
    #[default]
    Idle,
    /// `AUTHENTICATE PLAIN` accepted; collecting the payload.
    Plain(String),
}

/// Serves one connection: registers it, then relays lines to the gateway.
pub async fn handle<R, W>(
    id: ClientId,
    r: R,
    w: W,
    inputs: mpsc::Sender<Input>,
    cfg: Arc<ConnConfig>,
) where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (tx, mut rx) = mpsc::channel::<String>(CLIENT_QUEUE);
    let writer = tokio::spawn(async move {
        // Buffered, flushed whenever the queue runs dry: a burst (an attach,
        // a history batch) goes out in large writes, not two per line.
        let mut w = tokio::io::BufWriter::with_capacity(64 * 1024, w);
        while let Some(line) = rx.recv().await {
            if w.write_all(line.as_bytes()).await.is_err() || w.write_all(b"\r\n").await.is_err() {
                break;
            }
            if rx.is_empty() && w.flush().await.is_err() {
                break;
            }
        }
        let _ = w.shutdown().await;
    });
    let mut lines = BufReader::new(r);
    let mut reg = Reg {
        authed: cfg.password.is_none(),
        ..Reg::default()
    };
    let srv = cfg.server_name.clone();
    let send = |m: Message| {
        let _ = tx.try_send(m.to_string());
    };

    // ----- Registration -----
    let deadline = tokio::time::sleep(REGISTER_TIMEOUT);
    tokio::pin!(deadline);
    let registered = loop {
        let line = tokio::select! {
            l = read_line(&mut lines) => l,
            _ = &mut deadline => {
                send(Message::new("ERROR", ["Registration timed out"]));
                break false;
            }
        };
        let Some(line) = line else { break false };
        let Ok(msg) = Message::parse(&line) else {
            continue;
        };
        let nick = reg.nick.clone().unwrap_or_else(|| "*".into());
        match msg.command.as_str() {
            "CAP" => {
                for m in cap(&srv, &nick, &msg, &mut reg.caps, &mut reg.negotiating) {
                    send(m);
                }
            }
            "PASS" => {
                if cfg.password.as_deref() == Some(msg.param(0)) {
                    reg.authed = true;
                }
            }
            "AUTHENTICATE" => {
                for m in authenticate(&srv, &nick, &msg, &mut reg, cfg.password.as_deref()) {
                    send(m);
                }
            }
            "NICK" if !msg.param(0).is_empty() => reg.nick = Some(msg.param(0).to_string()),
            "USER" => reg.user = true,
            "PING" => send(
                Message::new("PONG", [srv.clone(), msg.param(0).to_string()])
                    .with_source(srv.clone()),
            ),
            "QUIT" => break false,
            _ => {}
        }
        if reg.nick.is_some() && reg.user && !reg.negotiating {
            if reg.authed {
                break true;
            }
            send(Message::new("464", [nick, "Password incorrect".into()]).with_source(srv.clone()));
            send(Message::new("ERROR", ["Password incorrect"]));
            break false;
        }
    };

    if registered {
        let nick = reg.nick.clone().unwrap_or_default();
        let close = CancellationToken::new();
        if inputs
            .send(Input::Attach {
                id,
                nick,
                caps: reg.caps.clone(),
                out: tx.clone(),
                close: close.clone(),
            })
            .await
            .is_ok()
        {
            tokio::select! {
                () = serve_registered(id, &mut lines, &tx, &inputs, &srv, reg) => {}
                () = close.cancelled() => {
                    // The queue is full of lines the client is not reading:
                    // drop them with the socket rather than wait.
                    writer.abort();
                }
            }
        }
        let _ = inputs.send(Input::Detach { id }).await;
    }
    drop(tx);
    let _ = writer.await;
}

/// Relays a registered client's lines; keepalive, `CAP` and `QUIT` are
/// handled here.
async fn serve_registered<R: AsyncRead + Unpin>(
    id: ClientId,
    lines: &mut BufReader<R>,
    tx: &mpsc::Sender<String>,
    inputs: &mpsc::Sender<Input>,
    srv: &str,
    mut reg: Reg,
) {
    let mut pinged = false;
    loop {
        let line = match tokio::time::timeout(IDLE_PING, read_line(lines)).await {
            Ok(Some(l)) => l,
            Ok(None) => return,
            Err(_) if pinged => {
                let _ = tx.try_send(Message::new("ERROR", ["Ping timeout"]).to_string());
                return;
            }
            Err(_) => {
                pinged = true;
                let _ = tx.try_send(Message::new("PING", [srv.to_string()]).to_string());
                continue;
            }
        };
        pinged = false;
        let Ok(msg) = Message::parse(&line) else {
            continue;
        };
        match msg.command.as_str() {
            "PING" => {
                let pong = Message::new("PONG", [srv.to_string(), msg.param(0).to_string()])
                    .with_source(srv.to_string());
                let mut pong = pong;
                if let Some(l) = msg.tag("label") {
                    pong.set_tag("label", Some(l.to_string()));
                }
                let _ = tx.try_send(pong.to_string());
            }
            "PONG" => {}
            "QUIT" => {
                let _ = tx.try_send(Message::new("ERROR", ["Closing link"]).to_string());
                return;
            }
            "CAP" => {
                let nick = reg.nick.clone().unwrap_or_default();
                let before = reg.caps.clone();
                for m in cap(srv, &nick, &msg, &mut reg.caps, &mut reg.negotiating) {
                    let _ = tx.try_send(m.to_string());
                }
                if reg.caps != before
                    && inputs
                        .send(Input::Caps {
                            id,
                            caps: reg.caps.clone(),
                        })
                        .await
                        .is_err()
                {
                    return;
                }
            }
            "NICK" => {
                reg.nick = Some(msg.param(0).to_string());
                if inputs.send(Input::Line { id, msg }).await.is_err() {
                    return;
                }
            }
            "AUTHENTICATE" | "PASS" | "USER" => {
                let m = Message::new(
                    "462",
                    [
                        reg.nick.clone().unwrap_or_default(),
                        "You may not reregister".into(),
                    ],
                )
                .with_source(srv.to_string());
                let _ = tx.try_send(m.to_string());
            }
            _ => {
                if inputs.send(Input::Line { id, msg }).await.is_err() {
                    return;
                }
            }
        }
        if tx.is_closed() {
            return;
        }
    }
}

/// Reads one line (without its line ending); `None` at end of stream or on
/// an error. An overlong line is cut at [`MAX_LINE`]; invalid UTF-8 is
/// replaced.
async fn read_line<R: AsyncRead + Unpin>(r: &mut BufReader<R>) -> Option<String> {
    let mut buf = Vec::new();
    loop {
        let n = r.read_until(b'\n', &mut buf).await.ok()?;
        if n == 0 {
            return None;
        }
        if buf.len() > MAX_LINE {
            buf.truncate(MAX_LINE);
        }
        let s = String::from_utf8_lossy(&buf)
            .trim_end_matches(['\r', '\n'])
            .to_string();
        if s.is_empty() {
            buf.clear();
            continue;
        }
        return Some(s);
    }
}

/// Answers a `CAP` subcommand.
fn cap(
    srv: &str,
    nick: &str,
    msg: &Message,
    caps: &mut Caps,
    negotiating: &mut bool,
) -> Vec<Message> {
    let reply = |params: Vec<String>| {
        let mut p = vec![nick.to_string()];
        p.extend(params);
        Message::new("CAP", p).with_source(srv.to_string())
    };
    match msg.param(0).to_ascii_uppercase().as_str() {
        "LS" => {
            *negotiating = true;
            let version: u32 = msg.param(1).parse().unwrap_or(0);
            if version >= 302 {
                caps.insert("cap-notify");
            }
            let lists = caps::ls_lists(version);
            let n = lists.len();
            lists
                .into_iter()
                .enumerate()
                .map(|(i, l)| {
                    if i + 1 < n {
                        reply(vec!["LS".into(), "*".into(), l])
                    } else {
                        reply(vec!["LS".into(), l])
                    }
                })
                .collect()
        }
        "LIST" => vec![reply(vec![
            "LIST".into(),
            caps.names().collect::<Vec<_>>().join(" "),
        ])],
        "REQ" => {
            *negotiating = true;
            let list = msg.param(1);
            let ok = list
                .split_whitespace()
                .all(|c| caps::is_offered(c.trim_start_matches('-')));
            if !ok {
                return vec![reply(vec!["NAK".into(), list.to_string()])];
            }
            for c in list.split_whitespace() {
                match c.strip_prefix('-') {
                    Some(c) => caps.remove(c),
                    None => caps.insert(c),
                }
            }
            vec![reply(vec!["ACK".into(), list.to_string()])]
        }
        "END" => {
            *negotiating = false;
            Vec::new()
        }
        other => vec![
            Message::new(
                "410",
                [
                    nick.to_string(),
                    other.to_string(),
                    "Invalid CAP command".into(),
                ],
            )
            .with_source(srv.to_string()),
        ],
    }
}

/// Runs one step of SASL `PLAIN`.
fn authenticate(
    srv: &str,
    nick: &str,
    msg: &Message,
    reg: &mut Reg,
    password: Option<&str>,
) -> Vec<Message> {
    let num = |code: &str, params: Vec<String>| {
        let mut p = vec![nick.to_string()];
        p.extend(params);
        Message::new(code, p).with_source(srv.to_string())
    };
    let arg = msg.param(0);
    if arg == "*" {
        reg.sasl = Sasl::Idle;
        return vec![num("906", vec!["SASL authentication aborted".into()])];
    }
    match &mut reg.sasl {
        Sasl::Idle => {
            if arg.eq_ignore_ascii_case("PLAIN") {
                reg.sasl = Sasl::Plain(String::new());
                vec![Message::new("AUTHENTICATE", ["+"])]
            } else {
                vec![
                    num(
                        "908",
                        vec!["PLAIN".into(), "are available SASL mechanisms".into()],
                    ),
                    num("904", vec!["SASL authentication failed".into()]),
                ]
            }
        }
        Sasl::Plain(acc) => {
            if arg != "+" {
                acc.push_str(arg);
            }
            if arg.len() == 400 {
                return Vec::new(); // more to come
            }
            let payload = std::mem::take(acc);
            reg.sasl = Sasl::Idle;
            let decoded = base64::engine::general_purpose::STANDARD
                .decode(payload.as_bytes())
                .ok()
                .and_then(|b| String::from_utf8(b).ok());
            let parts: Option<(String, String)> = decoded.and_then(|s| {
                let mut it = s.split('\0');
                let _authz = it.next()?;
                let user = it.next()?.to_string();
                let pass = it.next()?.to_string();
                Some((user, pass))
            });
            match parts {
                Some((user, pass)) if password.is_none_or(|p| p == pass) => {
                    reg.authed = true;
                    let account = if user.is_empty() {
                        nick.to_string()
                    } else {
                        user
                    };
                    vec![
                        num(
                            "900",
                            vec![
                                format!("{nick}!{nick}@{}", crate::names::USER_HOST),
                                account.clone(),
                                format!("You are now logged in as {account}"),
                            ],
                        ),
                        num("903", vec!["SASL authentication successful".into()]),
                    ]
                }
                _ => vec![num("904", vec!["SASL authentication failed".into()])],
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, duplex};

    fn cfg(password: Option<&str>) -> Arc<ConnConfig> {
        Arc::new(ConnConfig {
            server_name: "srv".into(),
            password: password.map(str::to_owned),
        })
    }

    /// Runs a connection on in-memory pipes; returns the client's ends and
    /// the gateway's input receiver.
    fn start(password: Option<&str>) -> (tokio::io::DuplexStream, mpsc::Receiver<Input>) {
        let (client, server) = duplex(1 << 16);
        let (r, w) = tokio::io::split(server);
        let (in_tx, in_rx) = mpsc::channel(16);
        tokio::spawn(handle(1, r, w, in_tx, cfg(password)));
        (client, in_rx)
    }

    async fn read_until(c: &mut tokio::io::DuplexStream, needle: &str) -> String {
        let mut out = String::new();
        let mut buf = [0u8; 4096];
        while !out.contains(needle) {
            let n = tokio::time::timeout(Duration::from_secs(2), c.read(&mut buf))
                .await
                .expect("timed out")
                .unwrap();
            assert!(n > 0, "closed; got {out:?}");
            out.push_str(&String::from_utf8_lossy(&buf[..n]));
        }
        out
    }

    #[tokio::test]
    async fn registers_with_caps_and_sasl() {
        let (mut c, mut inputs) = start(Some("hunter2"));
        c.write_all(b"CAP LS 302\r\nNICK ada\r\nUSER ada 0 * :Ada\r\n")
            .await
            .unwrap();
        let ls = read_until(&mut c, "\r\n").await;
        assert!(ls.contains("CAP * LS") && ls.contains("sasl=PLAIN"), "{ls}");
        c.write_all(b"CAP REQ :sasl message-tags server-time\r\n")
            .await
            .unwrap();
        read_until(&mut c, "ACK").await;
        c.write_all(b"AUTHENTICATE PLAIN\r\n").await.unwrap();
        read_until(&mut c, "AUTHENTICATE +").await;
        let payload = base64::engine::general_purpose::STANDARD.encode("ada\0ada\0hunter2");
        c.write_all(format!("AUTHENTICATE {payload}\r\n").as_bytes())
            .await
            .unwrap();
        read_until(&mut c, " 903 ").await;
        c.write_all(b"CAP END\r\n").await.unwrap();
        match inputs.recv().await.unwrap() {
            Input::Attach { nick, caps, .. } => {
                assert_eq!(nick, "ada");
                assert!(
                    caps.has("message-tags") && caps.has("server-time") && caps.has("cap-notify")
                );
            }
            other => panic!("{other:?}"),
        }
        c.write_all(b"PRIVMSG #x :hi\r\n").await.unwrap();
        match inputs.recv().await.unwrap() {
            Input::Line { msg, .. } => assert_eq!(msg.params, vec!["#x", "hi"]),
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn wrong_password_is_refused() {
        let (mut c, mut inputs) = start(Some("hunter2"));
        c.write_all(b"PASS nope\r\nNICK ada\r\nUSER ada 0 * :Ada\r\n")
            .await
            .unwrap();
        let out = read_until(&mut c, "ERROR").await;
        assert!(out.contains(" 464 "), "{out}");
        assert!(
            inputs.recv().await.is_none(),
            "a refused client is never attached"
        );
    }

    #[tokio::test]
    async fn pass_registers_without_cap() {
        let (mut c, mut inputs) = start(Some("hunter2"));
        c.write_all(b"PASS hunter2\r\nNICK ada\r\nUSER ada 0 * :Ada\r\n")
            .await
            .unwrap();
        assert!(matches!(inputs.recv().await.unwrap(), Input::Attach { .. }));
    }

    #[tokio::test]
    async fn unknown_caps_are_refused() {
        let (mut c, _inputs) = start(None);
        c.write_all(b"CAP REQ :message-tags account-notify\r\n")
            .await
            .unwrap();
        let out = read_until(&mut c, "NAK").await;
        assert!(out.contains("message-tags account-notify"), "{out}");
    }
}
