//! The trouter state machine against a fake transport and wiremock, all
//! under paused time.
//!
//! Every wait goes through `hold_clock` (see there): the clock moves only
//! when a test calls `tokio::time::advance`, so pings, the silence watchdog
//! and backoff fire exactly when a test says so, never while an HTTP call
//! is in flight.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::frames::{encode_ack, encode_sequenced, ping};
use super::transport::{FakeConn, FakeFactory};
use super::{TrouterConfig, TrouterDeps, run};
use crate::Capture;
use crate::auth::{RefreshKind, TokenHandle};
use crate::events::{CatchUpReason, DisconnectReason, Event, RawEvent};
use crate::http::Http;
use crate::test_support::{hold_clock, hold_clock_for, mount_auth, mount_trouter_http, test_http};

const START_MS: i64 = 1_760_346_000_000;
const REGISTRAR: &str = "/registrar/prod/V2/registrations";

struct Rig {
    server: MockServer,
    #[expect(dead_code, reason = "keeps the rig's shape uniform for later tests")]
    http: Arc<Http>,
    tokens: TokenHandle,
    factory: Arc<FakeFactory>,
    conns: mpsc::UnboundedReceiver<FakeConn>,
    events: mpsc::Receiver<Event>,
    cancel: CancellationToken,
    task: JoinHandle<()>,
    raw: Arc<Mutex<Vec<RawEvent>>>,
}

impl Rig {
    /// The next connection the client opens.
    async fn conn(&mut self) -> FakeConn {
        hold_clock(self.conns.recv())
            .await
            .expect("factory dropped")
    }

    /// The next connection, after the server has sent `1::` on it.
    async fn connected_conn(&mut self) -> FakeConn {
        let conn = self.conn().await;
        conn.to_client.send(Ok(Some("1::".into()))).await.unwrap();
        conn
    }

    async fn registrations(&self) -> usize {
        self.server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|q| q.url.path() == REGISTRAR)
            .count()
    }

    /// Waits (real time, clock held) until the registrar has seen `n`
    /// requests.
    async fn wait_registrations(&self, n: usize) {
        hold_clock(async {
            while self.registrations().await < n {
                tokio::task::spawn_blocking(|| std::thread::sleep(Duration::from_millis(2)))
                    .await
                    .unwrap();
            }
        })
        .await;
    }

    async fn stop(self) {
        self.cancel.cancel();
        hold_clock(self.task).await.unwrap();
    }
}

/// Mounts auth and trouter HTTP on `server` (after anything the test
/// mounted itself, which therefore wins) and spawns `run`.
async fn build(
    server: MockServer,
    capacity: usize,
    capture: Option<Arc<Capture>>,
    cfg_mod: impl FnOnce(&mut TrouterConfig),
) -> Rig {
    mount_auth(&server).await;
    mount_trouter_http(&server).await;
    let http = test_http(&server, None).await;
    let tokens = http.tokens().handle().await.clone();
    let (factory, conns) = FakeFactory::new();
    let (tx, events) = mpsc::channel(capacity);
    let cancel = CancellationToken::new();
    let raw = Arc::new(Mutex::new(Vec::new()));
    let sink = raw.clone();
    let mut cfg = TrouterConfig::new("ep-1".into());
    cfg.process_start_ms = START_MS;
    cfg_mod(&mut cfg);
    let deps = TrouterDeps {
        http: http.clone(),
        factory: factory.clone(),
        tx,
        cancel: cancel.clone(),
        capture,
        raw_hook: Some(Arc::new(move |e: &RawEvent| {
            sink.lock().unwrap().push(e.clone());
        })),
        push_address: Arc::new(std::sync::Mutex::new(None)),
    };
    let task = tokio::spawn(run(cfg, deps));
    Rig {
        server,
        http,
        tokens,
        factory,
        conns,
        events,
        cancel,
        task,
        raw,
    }
}

async fn rig(cfg_mod: impl FnOnce(&mut TrouterConfig)) -> Rig {
    build(MockServer::start().await, 1024, None, cfg_mod).await
}

async fn rig_with_capacity(n: usize) -> Rig {
    build(MockServer::start().await, n, None, |_| {}).await
}

async fn rig_on(s: MockServer) -> Rig {
    build(s, 1024, None, |_| {}).await
}

/// The next frame the client sends that matches `pred` (others skipped).
async fn expect_frame(conn: &mut FakeConn, pred: impl Fn(&str) -> bool) -> String {
    hold_clock(async {
        loop {
            let f = conn.from_client.recv().await.expect("client closed");
            if pred(&f) {
                return f;
            }
        }
    })
    .await
}

/// The next event that matches `pred` (others skipped).
async fn expect_event(rx: &mut mpsc::Receiver<Event>, pred: impl Fn(&Event) -> bool) -> Event {
    hold_clock(async {
        loop {
            let e = rx.recv().await.expect("event channel closed");
            if pred(&e) {
                return e;
            }
        }
    })
    .await
}

/// Like [`expect_event`], but for waits that include HTTP retry sleeps:
/// advances the clock 100 ms whenever nothing arrives for 20 ms of real
/// time (at most 10 s of paused time).
async fn expect_event_advancing(
    rx: &mut mpsc::Receiver<Event>,
    pred: impl Fn(&Event) -> bool,
) -> Event {
    for _ in 0..100 {
        while let Some(e) = hold_clock_for(Duration::from_millis(20), rx.recv()).await {
            let e = e.expect("event channel closed");
            if pred(&e) {
                return e;
            }
        }
        tokio::time::advance(Duration::from_millis(100)).await;
    }
    panic!("no matching event within 10 s of paused time");
}

/// The fixture `/messaging` request with its EventMessage `id` and `time`
/// replaced.
fn messaging_request(id: i64, time: &str) -> String {
    let req = fixture!("9.03-trouter-connect/request-messaging.json");
    let mut v: serde_json::Value = serde_json::from_str(req).unwrap();
    let mut body: serde_json::Value = serde_json::from_str(v["body"].as_str().unwrap()).unwrap();
    body["id"] = id.into();
    body["time"] = time.into();
    v["body"] = body.to_string().into();
    format!("3:::{v}")
}

#[tokio::test(start_paused = true)]
async fn connect_auth_register_then_event() {
    let mut r = rig(|_| {}).await;
    let mut conn = r.conn().await;
    assert!(conn.url.contains("/socket.io/1/websocket/sessid-abc123?"));
    assert_eq!(conn.skype_token, "sk-1");
    conn.to_client.send(Ok(Some("1::".into()))).await.unwrap();
    let auth = expect_frame(&mut conn, |f| f.starts_with("5:::")).await;
    assert!(auth.contains("user.authenticate") && auth.contains("Bearer at-1"));
    let act = expect_frame(&mut conn, |f| f.starts_with("5:1+::")).await;
    assert!(act.contains("user.activity"));
    expect_event(&mut r.events, |e| matches!(e, Event::Connected)).await;
    assert_eq!(r.registrations().await, 1);
    let req = fixture!("9.03-trouter-connect/request-messaging.json");
    conn.to_client
        .send(Ok(Some(format!("3:::{req}"))))
        .await
        .unwrap();
    assert_eq!(
        expect_frame(&mut conn, |f| f.starts_with("3:::")).await,
        encode_ack(7001)
    );
    expect_event(
        &mut r.events,
        |e| matches!(e, Event::NewMessage(m) if m.id.0 == "1760346000000"),
    )
    .await;
    // An exact duplicate is acked but produces no event.
    conn.to_client
        .send(Ok(Some(format!("3:::{req}"))))
        .await
        .unwrap();
    expect_frame(&mut conn, |f| f.starts_with("3:::")).await;
    tokio::time::advance(Duration::from_millis(50)).await;
    assert!(r.events.try_recv().is_err(), "duplicate dropped");
    r.stop().await;
}

#[tokio::test(start_paused = true)]
async fn pings_every_30s_with_sequence_and_silence_reconnects() {
    let mut r = rig(|_| {}).await;
    let mut conn = r.connected_conn().await;
    expect_frame(&mut conn, |f| f.starts_with("5:1+::")).await;
    expect_event(&mut r.events, |e| matches!(e, Event::Connected)).await;
    tokio::time::advance(Duration::from_secs(31)).await;
    assert_eq!(
        expect_frame(&mut conn, |f| f.contains("ping")).await,
        encode_sequenced(2, &ping())
    );
    tokio::time::advance(Duration::from_secs(30)).await;
    assert_eq!(
        expect_frame(&mut conn, |f| f.contains("ping")).await,
        encode_sequenced(3, &ping())
    );
    // The 90 s ping tick sees ≥ 90 s without inbound traffic.
    tokio::time::advance(Duration::from_secs(31)).await;
    let d = expect_event(&mut r.events, |e| {
        matches!(
            e,
            Event::Disconnected {
                reason: DisconnectReason::Silence,
                ..
            }
        )
    })
    .await;
    let Event::Disconnected { retry_in, .. } = d else {
        unreachable!()
    };
    tokio::time::advance(retry_in + Duration::from_millis(1)).await;
    let conn2 = r.conn().await;
    assert!(
        conn2.url.contains("con_num=") && conn2.url.contains("_2&"),
        "connection counter increments: {}",
        conn2.url
    );
}

#[tokio::test(start_paused = true)]
async fn server_disconnect_and_error_reconnect_with_backoff() {
    let mut r = rig(|_| {}).await;
    let conn = r.connected_conn().await;
    expect_event(&mut r.events, |e| matches!(e, Event::Connected)).await;
    conn.to_client.send(Ok(Some("0::".into()))).await.unwrap();
    let d = expect_event(&mut r.events, |e| {
        matches!(
            e,
            Event::Disconnected {
                reason: DisconnectReason::ServerDisconnect,
                ..
            }
        )
    })
    .await;
    let Event::Disconnected { retry_in, .. } = d else {
        unreachable!()
    };
    assert!(retry_in >= Duration::from_secs(1) && retry_in < Duration::from_secs(3));
    tokio::time::advance(retry_in + Duration::from_millis(1)).await;
    let conn2 = r.connected_conn().await;
    expect_event(&mut r.events, |e| matches!(e, Event::Connected)).await;
    conn2
        .to_client
        .send(Ok(Some("7:::boom".into())))
        .await
        .unwrap();
    expect_event(&mut r.events, |e| {
        matches!(
            e,
            Event::Disconnected {
                reason: DisconnectReason::SocketError,
                ..
            }
        )
    })
    .await;
    r.stop().await;
}

#[tokio::test(start_paused = true)]
async fn backoff_grows_until_a_registered_session_lasts_30s() {
    let mut r = rig(|_| {}).await;
    // The socket closes before `1::`: a socket error, retried after 1 s.
    let conn = r.conn().await;
    conn.to_client.send(Ok(None)).await.unwrap();
    let first = expect_event(&mut r.events, |e| matches!(e, Event::Disconnected { .. })).await;
    assert_eq!(
        first,
        Event::Disconnected {
            reason: DisconnectReason::SocketError,
            retry_in: Duration::from_secs(1)
        }
    );
    // The next connect fails: the delay doubles (plus ≤ 250 ms jitter).
    r.factory.fail_next.store(true, Ordering::SeqCst);
    tokio::time::advance(Duration::from_millis(1001)).await;
    let second = expect_event(&mut r.events, |e| matches!(e, Event::Disconnected { .. })).await;
    let Event::Disconnected {
        reason: DisconnectReason::SocketError,
        retry_in,
    } = second
    else {
        panic!("{second:?}")
    };
    assert!(
        retry_in >= Duration::from_secs(2) && retry_in <= Duration::from_millis(2250),
        "{retry_in:?}"
    );
    tokio::time::advance(retry_in + Duration::from_millis(1)).await;
    let conn3 = r.connected_conn().await;
    assert!(
        conn3.url.contains(&format!("con_num={START_MS}_3&")),
        "third attempt: {}",
        conn3.url
    );
    expect_event(&mut r.events, |e| matches!(e, Event::Connected)).await;
    // Registered, but dropped right away: no reset, so a server that drops
    // every session after registering cannot cause a reconnect storm.
    conn3
        .to_client
        .send(Err(crate::Error::Network("reset".into())))
        .await
        .unwrap();
    let third = expect_event(&mut r.events, |e| matches!(e, Event::Disconnected { .. })).await;
    let Event::Disconnected { retry_in, .. } = third else {
        panic!("{third:?}")
    };
    assert!(retry_in >= Duration::from_secs(4), "{retry_in:?}");
    tokio::time::advance(retry_in + Duration::from_millis(1)).await;
    let conn4 = r.connected_conn().await;
    expect_event(&mut r.events, |e| matches!(e, Event::Connected)).await;
    // A session that lasted 30 s resets the backoff to 1 s.
    tokio::time::advance(Duration::from_secs(31)).await;
    conn4
        .to_client
        .send(Err(crate::Error::Network("reset".into())))
        .await
        .unwrap();
    let fourth = expect_event(&mut r.events, |e| matches!(e, Event::Disconnected { .. })).await;
    assert_eq!(
        fourth,
        Event::Disconnected {
            reason: DisconnectReason::SocketError,
            retry_in: Duration::from_secs(1)
        }
    );
    r.stop().await;
}

const MESSAGE_LOSS: &str = "5:::{\"name\":\"trouter.message_loss\",\"args\":[{\"droppedIndicators\":[{\"tag\":\"\",\"etag\":\"x\"}]}]}";

async fn expect_message_loss_catch_up(r: &mut Rig) {
    expect_event(&mut r.events, |e| {
        matches!(
            e,
            Event::NeedsCatchUp {
                reason: CatchUpReason::MessageLoss
            }
        )
    })
    .await;
}

/// Live 2026-10-14: trouter sent `message_loss` five times in ~30 s right
/// after connecting. Each asks for a catch-up, but re-registration happens
/// only when no registration is in flight and none succeeded in the last
/// 30 s.
#[tokio::test(start_paused = true)]
async fn message_loss_asks_for_catch_up_and_reregisters_at_most_every_30s() {
    let mut r = rig(|_| {}).await;
    let conn = r.connected_conn().await;
    // Two losses while the first registration is (or just was) in flight.
    for _ in 0..2 {
        conn.to_client
            .send(Ok(Some(MESSAGE_LOSS.into())))
            .await
            .unwrap();
    }
    expect_message_loss_catch_up(&mut r).await;
    expect_message_loss_catch_up(&mut r).await;
    expect_event(&mut r.events, |e| matches!(e, Event::Connected)).await;
    r.wait_registrations(1).await;
    // Another loss 10 s after registering: catch-up only.
    tokio::time::advance(Duration::from_secs(10)).await;
    conn.to_client
        .send(Ok(Some(MESSAGE_LOSS.into())))
        .await
        .unwrap();
    expect_message_loss_catch_up(&mut r).await;
    tokio::time::advance(Duration::from_millis(100)).await;
    assert_eq!(r.registrations().await, 1, "no re-registration within 30 s");
    // 31 s after registering: re-register (once, silently).
    tokio::time::advance(Duration::from_secs(21)).await;
    conn.to_client
        .send(Ok(Some(MESSAGE_LOSS.into())))
        .await
        .unwrap();
    expect_message_loss_catch_up(&mut r).await;
    r.wait_registrations(2).await;
    tokio::time::advance(Duration::from_millis(100)).await;
    conn.to_client
        .send(Ok(Some(MESSAGE_LOSS.into())))
        .await
        .unwrap();
    expect_message_loss_catch_up(&mut r).await;
    tokio::time::advance(Duration::from_millis(100)).await;
    assert_eq!(r.registrations().await, 2);
    // Re-registration is silent: no second Connected.
    assert!(
        std::iter::from_fn(|| r.events.try_recv().ok()).all(|e| !matches!(e, Event::Connected))
    );
    r.stop().await;
}

#[tokio::test(start_paused = true)]
async fn registration_failure_reconnects() {
    // Mounted before the rig's registrar `{}` (and at a higher priority), so
    // it answers every registration.
    let s = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(REGISTRAR))
        .respond_with(ResponseTemplate::new(500))
        .with_priority(1)
        .mount(&s)
        .await;
    let mut r = rig_on(s).await;
    let _conn = r.connected_conn().await;
    // The 500 is retried (0.5 s, 1 s backoff) before registration fails;
    // Connected is never emitted for an unregistered connection.
    let first = expect_event_advancing(&mut r.events, |_| true).await;
    assert!(
        matches!(
            first,
            Event::Disconnected {
                reason: DisconnectReason::RegistrationFailed,
                ..
            }
        ),
        "{first:?}"
    );
    assert_eq!(r.registrations().await, 3);
    r.stop().await;
}

#[tokio::test(start_paused = true)]
async fn skype_token_renewal_reconnects_by_default_but_not_when_disabled() {
    for (flag, expect_reconnect) in [(true, true), (false, false)] {
        let mut r = rig(|c| c.reconnect_on_token_renewal = flag).await;
        let _conn = r.connected_conn().await;
        expect_event(&mut r.events, |e| matches!(e, Event::Connected)).await;
        let v = r.tokens.current().version;
        let snap = hold_clock(r.tokens.force_refresh(RefreshKind::SkypeToken, v))
            .await
            .unwrap();
        assert_eq!(snap.skype_token.unwrap().expose(), "sk-2");
        if expect_reconnect {
            expect_event(&mut r.events, |e| {
                matches!(e, Event::Disconnected { reason: DisconnectReason::TokenRenewal, retry_in } if *retry_in == Duration::ZERO)
            })
            .await;
            let mut c2 = r.connected_conn().await;
            assert_eq!(c2.skype_token, "sk-2");
            let auth = expect_frame(&mut c2, |f| f.contains("user.authenticate")).await;
            assert!(auth.contains("Bearer at-"));
            expect_event(&mut r.events, |e| matches!(e, Event::Connected)).await;
        } else {
            tokio::time::advance(Duration::from_secs(1)).await;
            assert!(r.events.try_recv().is_err());
            assert!(r.conns.try_recv().is_err());
        }
        r.stop().await;
    }
}

#[tokio::test(start_paused = true)]
async fn overflow_counts_and_emits_needs_catch_up_when_drained() {
    let mut r = rig_with_capacity(2).await;
    let mut conn = r.connected_conn().await;
    expect_event(&mut r.events, |e| matches!(e, Event::Connected)).await;
    for i in 0..5 {
        let frame = messaging_request(2000 + i, &format!("2026-10-13T09:00:0{i}.000Z"));
        conn.to_client.send(Ok(Some(frame))).await.unwrap();
        // Every request is acked, delivered or not.
        assert_eq!(
            expect_frame(&mut conn, |f| f.starts_with("3:::")).await,
            encode_ack(7001)
        );
    }
    // Capacity 2: the first two are delivered, three are dropped; as soon as
    // the consumer drains (no timer needed), NeedsCatchUp { Overflow }
    // follows, once.
    let a = hold_clock(r.events.recv()).await.unwrap();
    let b = hold_clock(r.events.recv()).await.unwrap();
    assert!(matches!(a, Event::NewMessage(_)) && matches!(b, Event::NewMessage(_)));
    let c = hold_clock(r.events.recv()).await.unwrap();
    assert_eq!(
        c,
        Event::NeedsCatchUp {
            reason: CatchUpReason::Overflow
        }
    );
    // Pings kept going throughout.
    tokio::time::advance(Duration::from_secs(31)).await;
    expect_frame(&mut conn, |f| f == encode_sequenced(2, &ping())).await;
    assert!(r.events.try_recv().is_err(), "NeedsCatchUp is sent once");
    // The next event after an overflow is delivered normally.
    conn.to_client
        .send(Ok(Some(messaging_request(
            3000,
            "2026-10-13T09:01:00.000Z",
        ))))
        .await
        .unwrap();
    expect_event(&mut r.events, |e| matches!(e, Event::NewMessage(_))).await;
    r.stop().await;
}

#[tokio::test(start_paused = true)]
async fn dropping_the_receiver_stops_the_task_promptly() {
    let mut r = rig(|_| {}).await;
    let _conn = r.connected_conn().await;
    let t0 = tokio::time::Instant::now();
    drop(r.events);
    hold_clock(r.task).await.unwrap();
    assert_eq!(t0.elapsed(), Duration::ZERO, "no timer needed to stop");
}

#[tokio::test(start_paused = true)]
async fn cancel_during_setup_stops_the_task() {
    let mut r = rig(|_| {}).await;
    // Cancelled before the first connection exists (tokens / negotiation).
    r.cancel.cancel();
    hold_clock(r.task).await.unwrap();
    assert!(r.events.try_recv().is_err(), "no event on a plain stop");
}

#[tokio::test(start_paused = true)]
async fn reauth_required_emits_event_and_stops() {
    let s = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/organizations/oauth2/v2.0/token"))
        .respond_with(
            ResponseTemplate::new(400).set_body_json(serde_json::json!({"error": "invalid_grant"})),
        )
        .mount(&s)
        .await;
    let mut r = rig_on(s).await;
    expect_event(&mut r.events, |e| matches!(e, Event::ReauthRequired)).await;
    hold_clock(r.task).await.unwrap();
    assert!(r.conns.try_recv().is_err(), "never connected");
}

#[tokio::test(start_paused = true)]
async fn raw_hook_sees_each_event_message() {
    let mut r = rig(|_| {}).await;
    let conn = r.connected_conn().await;
    expect_event(&mut r.events, |e| matches!(e, Event::Connected)).await;
    conn.to_client
        .send(Ok(Some(format!(
            "3:::{}",
            fixture!("9.03-trouter-connect/request-messaging.json")
        ))))
        .await
        .unwrap();
    expect_event(&mut r.events, |e| matches!(e, Event::NewMessage(_))).await;
    let raw = r.raw.lock().unwrap();
    assert_eq!(raw.len(), 1);
    assert_eq!(raw[0].resource_type, "NewMessage");
    assert_eq!(raw[0].mapped, vec!["NewMessage".to_string()]);
    assert!(raw[0].url.ends_with("/messaging"));
    assert_eq!(raw[0].id, Some(1001));
    assert_eq!(raw[0].body["resource"]["imdisplayname"], "Bea Example");
}

#[tokio::test(start_paused = true)]
async fn odd_requests_are_acked_and_dropped() {
    let mut r = rig(|_| {}).await;
    let mut conn = r.connected_conn().await;
    expect_event(&mut r.events, |e| matches!(e, Event::Connected)).await;
    for (frame, id) in [
        // null body (carry-in: ack even when the body is not a string)
        (
            "3:::{\"id\":11,\"method\":\"POST\",\"url\":\"/v4/f/x/messaging\",\"body\":null}",
            11,
        ),
        // a non-messaging URL
        (
            "3:::{\"id\":12,\"method\":\"POST\",\"url\":\"/v4/f/x/presence\",\"body\":\"{}\"}",
            12,
        ),
        // an undecodable body
        (
            "3:::{\"id\":13,\"method\":\"POST\",\"url\":\"/v4/f/x/messaging\",\"headers\":{\"X-Microsoft-Skype-Content-Encoding\":\"gzip\"},\"body\":\"%%%\"}",
            13,
        ),
        // not an EventMessage
        (
            "3:::{\"id\":14,\"method\":\"POST\",\"url\":\"/v4/f/x/messaging\",\"body\":\"{\\\"type\\\":\\\"Other\\\"}\"}",
            14,
        ),
    ] {
        conn.to_client.send(Ok(Some(frame.into()))).await.unwrap();
        assert_eq!(
            expect_frame(&mut conn, |f| f.starts_with("3:::")).await,
            encode_ack(id)
        );
    }
    // Ignored frames keep the session up.
    for frame in ["2::", "6:::1+[]", "5:::{\"name\":\"other\"}", "9::junk"] {
        conn.to_client.send(Ok(Some(frame.into()))).await.unwrap();
    }
    // A gzipped messaging body is delivered.
    let req: serde_json::Value =
        serde_json::from_str(fixture!("9.03-trouter-connect/request-messaging.json")).unwrap();
    let gz = serde_json::json!({
        "id": 15, "method": "POST", "url": req["url"],
        "headers": {"X-Microsoft-Skype-Content-Encoding": "gzip"},
        "body": super::frames::gzip_base64(req["body"].as_str().unwrap()),
    });
    conn.to_client
        .send(Ok(Some(format!("3:::{gz}"))))
        .await
        .unwrap();
    assert_eq!(
        expect_frame(&mut conn, |f| f.starts_with("3:::")).await,
        encode_ack(15)
    );
    let e = expect_event(&mut r.events, |_| true).await;
    assert!(matches!(e, Event::NewMessage(_)), "{e:?}");
    assert_eq!(r.raw.lock().unwrap().len(), 1);
    r.stop().await;
}

#[tokio::test(start_paused = true)]
async fn reregisters_at_ttl_minus_10s_silently() {
    let mut r = rig(|c| c.register_ttl = Duration::from_secs(45)).await;
    let mut conn = r.connected_conn().await;
    expect_event(&mut r.events, |e| matches!(e, Event::Connected)).await;
    tokio::time::advance(Duration::from_secs(34)).await;
    expect_frame(&mut conn, |f| f.contains("ping")).await;
    assert_eq!(r.registrations().await, 1);
    tokio::time::advance(Duration::from_secs(2)).await;
    r.wait_registrations(2).await;
    tokio::time::advance(Duration::from_millis(10)).await;
    assert!(r.events.try_recv().is_err(), "no second Connected");
    r.stop().await;
}

#[tokio::test(start_paused = true)]
async fn frames_are_captured_redacted() {
    let dir = tempfile::tempdir().unwrap();
    let capture = Arc::new(Capture::new(dir.path()).unwrap());
    let mut r = build(MockServer::start().await, 1024, Some(capture), |_| {}).await;
    let mut conn = r.connected_conn().await;
    expect_event(&mut r.events, |e| matches!(e, Event::Connected)).await;
    conn.to_client
        .send(Ok(Some(format!(
            "3:::{}",
            fixture!("9.03-trouter-connect/request-messaging.json")
        ))))
        .await
        .unwrap();
    expect_frame(&mut conn, |f| f.starts_with("3:::")).await;
    expect_event(&mut r.events, |e| matches!(e, Event::NewMessage(_))).await;
    r.stop().await;
    let mut names = Vec::new();
    let mut texts = Vec::new();
    for f in std::fs::read_dir(dir.path()).unwrap() {
        let path = f.unwrap().path();
        names.push(path.file_name().unwrap().to_string_lossy().into_owned());
        texts.push(std::fs::read_to_string(path).unwrap());
    }
    names.sort();
    for label in [
        "ws-in-trouter-connected",
        "ws-out-trouter-authenticate",
        "ws-out-trouter-activity",
        "ws-in-trouter-request",
        "ws-out-trouter-ack",
    ] {
        assert!(
            names.iter().any(|n| n.contains(label)),
            "{label}: {names:?}"
        );
    }
    let request = texts
        .iter()
        .find(|t| t.contains("\"label\": \"trouter-request\""))
        .unwrap();
    let doc: serde_json::Value = serde_json::from_str(request).unwrap();
    assert_eq!(
        doc["decoded"]["type"], "EventMessage",
        "decoded body captured"
    );
    for t in &texts {
        for secret in ["at-1", "sr-value", "sig-value"] {
            assert!(!t.contains(secret), "{secret} leaked: {t}");
        }
    }
}

/// A `/messaging` request carrying the typing fixture with its EventMessage
/// `id` and `time` replaced.
fn typing_request(id: i64, time: &str) -> String {
    let mut v: serde_json::Value =
        serde_json::from_str(fixture!("9.03-trouter-connect/request-messaging.json")).unwrap();
    let mut body: serde_json::Value =
        serde_json::from_str(fixture!("9.04-event-payloads/typing.json")).unwrap();
    body["id"] = id.into();
    body["time"] = time.into();
    v["body"] = body.to_string().into();
    format!("3:::{v}")
}

#[tokio::test(start_paused = true)]
async fn typing_is_dropped_on_overflow_without_a_catch_up() {
    let mut r = rig_with_capacity(2).await;
    let mut conn = r.connected_conn().await;
    expect_event(&mut r.events, |e| matches!(e, Event::Connected)).await;
    for i in 0..5 {
        let frame = typing_request(4000 + i, &format!("2026-10-13T09:00:0{i}.000Z"));
        conn.to_client.send(Ok(Some(frame))).await.unwrap();
        assert_eq!(
            expect_frame(&mut conn, |f| f.starts_with("3:::")).await,
            encode_ack(7001)
        );
    }
    // Capacity 2: two delivered, three dropped — and no NeedsCatchUp for them.
    let a = hold_clock(r.events.recv()).await.unwrap();
    let b = hold_clock(r.events.recv()).await.unwrap();
    assert!(matches!(a, Event::Typing { .. }) && matches!(b, Event::Typing { .. }));
    tokio::time::advance(Duration::from_millis(50)).await;
    assert!(r.events.try_recv().is_err(), "no catch-up owed for typing");
    assert!(
        r.raw.lock().unwrap().iter().any(|e| e.mapped == ["Typing"]),
        "event_name knows the variant"
    );
    conn.to_client
        .send(Ok(Some(messaging_request(
            5000,
            "2026-10-13T09:01:00.000Z",
        ))))
        .await
        .unwrap();
    expect_event(&mut r.events, |e| matches!(e, Event::NewMessage(_))).await;
    r.stop().await;
}

#[tokio::test(start_paused = true)]
async fn presence_pushes_become_presence_events() {
    let mut r = rig(|_| {}).await;
    let conn = r.connected_conn().await;
    expect_event(&mut r.events, |e| matches!(e, Event::Connected)).await;
    conn.to_client
        .send(Ok(Some(format!(
            "3:::{}",
            fixture!("9.03-trouter-connect/request-presence.json")
        ))))
        .await
        .unwrap();
    let ev = expect_event(&mut r.events, |e| matches!(e, Event::Presence(_))).await;
    let Event::Presence(list) = ev else {
        unreachable!()
    };
    assert_eq!(
        (list[0].availability.as_str(), list[0].activity.as_str()),
        ("Busy", "InAMeeting")
    );
}
