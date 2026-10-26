//! Trouter session setup (spec §4.6 steps 1–4): the `/v4/a` negotiation,
//! the socket.io session GET, the websocket URL and registrar registration.
//!
//! Every shape here comes from purple-teams and is unverified (§9.3); each
//! guess lives in one marked item. `connectparams`, the `surl` and the
//! session query are credentials: they are never logged, and
//! [`TrouterSession`]'s `Debug` redacts them.

use std::fmt;

use serde_json::{Map, Value, json};

use crate::http::{Http, HttpRequest};
use crate::redact::{REDACTED, redact_url};
use crate::{Error, Result};

/// The `tc` session-query parameter (sent URL-encoded).
///
/// UNVERIFIED(§9.3): purple-teams' value; the server may want a newer
/// client version.
pub(crate) const TC: &str = r#"{"cv":"2024.23.01.2","ua":"TeamsCDL","hr":"","v":"49/25113001312"}"#;
/// `platformUIVersion` in registrar bodies (also the `v` inside [`TC`]).
pub(crate) const PLATFORM_UI_VERSION: &str = "49/25113001312";
/// Registration lifetime in seconds; Task 16 re-registers at TTL − 10 s.
const REGISTRATION_TTL_S: u64 = 86_400;

/// `con_num` for the session query: process start and a per-process
/// connection counter starting at 1.
///
/// UNVERIFIED(§9.3): con_num = "{process_start_ms}_{counter}" (purple-teams hard-codes 1234567890123_1).
pub(crate) fn con_num(process_start_ms: i64, counter: u32) -> String {
    format!("{process_start_ms}_{counter}")
}

/// A negotiated trouter session: the `/v4/a` answer plus the socket.io
/// session GET's `{sessionId}:{heartbeat}:{close}:…`.
#[derive(Clone, PartialEq)]
pub(crate) struct TrouterSession {
    /// socket.io base URL, always ending in `/`.
    pub socketio: String,
    /// This endpoint's trouter URL, registered with the registrar.
    pub surl: String,
    pub ccid: Option<String>,
    /// `sr`, `issuer`, `sp`, `se`, `st`, `sig`; echoed in the session query
    /// and in `user.authenticate`.
    pub connectparams: Map<String, Value>,
    pub session_id: String,
    pub heartbeat_s: u64,
    pub close_s: u64,
}

impl fmt::Debug for TrouterSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TrouterSession")
            .field("socketio", &redact_url(&self.socketio))
            .field("surl", &redact_url(&self.surl))
            .field("ccid", &self.ccid.as_ref().map(|_| REDACTED))
            .field("connectparams", &REDACTED)
            .field("session_id", &REDACTED)
            .field("heartbeat_s", &self.heartbeat_s)
            .field("close_s", &self.close_s)
            .finish()
    }
}

/// Steps 1–2: `POST {trouter}/v4/a?epid=…`, then `GET
/// {socketio}socket.io/1/?{session query}`.
///
/// UNVERIFIED(§9.3): the `/v4/a` answer `{socketio, surl, ccid,
/// connectparams}` and the plain-text session answer
/// `{sessionId}:{heartbeat}:{close}:{transports}` are purple-teams' shapes.
pub(crate) async fn negotiate(
    http: &Http,
    endpoint_id: &str,
    con_num: &str,
) -> Result<TrouterSession> {
    let trouter = http.endpoints().trouter.trim_end_matches('/');
    let v: Value = http
        .send(
            HttpRequest::post_empty(format!("{trouter}/v4/a?epid={}", enc(endpoint_id)))
                .label("trouter-v4a"),
        )
        .await?
        .ok()?
        .json()?;
    let mut session = parse_v4a(&v, trouter)?;
    let text = http
        .send(
            HttpRequest::get(format!(
                "{}socket.io/1/?{}",
                session.socketio,
                session_query(&session, endpoint_id, con_num)
            ))
            .label("trouter-session"),
        )
        .await?
        .ok()?
        .body;
    let (session_id, heartbeat_s, close_s) = parse_session(&text)?;
    session.session_id = session_id;
    session.heartbeat_s = heartbeat_s;
    session.close_s = close_s;
    tracing::debug!(heartbeat_s, close_s, "trouter session negotiated");
    Ok(session)
}

/// The `/v4/a` answer, with an empty session part. A missing `socketio`
/// defaults to the trouter endpoint.
fn parse_v4a(v: &Value, trouter: &str) -> Result<TrouterSession> {
    let field = |k: &str| v.get(k).and_then(Value::as_str).filter(|s| !s.is_empty());
    let mut socketio = field("socketio").map_or_else(|| trouter.to_string(), str::to_string);
    if !socketio.ends_with('/') {
        socketio.push('/');
    }
    Ok(TrouterSession {
        socketio,
        surl: field("surl")
            .ok_or_else(|| Error::Protocol("trouter /v4/a answer has no surl".into()))?
            .to_string(),
        ccid: field("ccid").map(str::to_string),
        connectparams: v
            .get("connectparams")
            .and_then(Value::as_object)
            .cloned()
            .ok_or_else(|| Error::Protocol("trouter /v4/a answer has no connectparams".into()))?,
        session_id: String::new(),
        heartbeat_s: 0,
        close_s: 0,
    })
}

/// `"{sessionId}:{heartbeat}:{close}:{transports}"` → its first three
/// fields.
fn parse_session(text: &str) -> Result<(String, u64, u64)> {
    let mut parts = text.trim().splitn(4, ':');
    let sid = parts.next().filter(|s| !s.is_empty());
    let hb = parts.next().and_then(|s| s.parse().ok());
    let close = parts.next().and_then(|s| s.parse().ok());
    match (sid, hb, close) {
        (Some(sid), Some(hb), Some(close)) => Ok((sid.to_string(), hb, close)),
        _ => Err(Error::Protocol(format!(
            "unexpected trouter session answer ({} bytes)",
            text.len()
        ))),
    }
}

/// Step 3: `{socketio as ws(s)}socket.io/1/websocket/{sessionId}?{session
/// query}`. A credential: log it only through `redact_url`.
pub(crate) fn websocket_url(s: &TrouterSession, endpoint_id: &str, con_num: &str) -> String {
    let base = if let Some(rest) = s.socketio.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = s.socketio.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        s.socketio.clone()
    };
    format!(
        "{base}socket.io/1/websocket/{}?{}",
        enc(&s.session_id),
        session_query(s, endpoint_id, con_num)
    )
}

/// The query shared by the session GET and the websocket URL:
/// `v=v4&{connectparams, in key order}&tc=…&con_num=…&epid=…&ccid=…&auth=true&timeout=40`
/// (`ccid` only when the `/v4/a` answer had one).
///
/// UNVERIFIED(§9.3): parameter set and order are purple-teams'.
fn session_query(s: &TrouterSession, endpoint_id: &str, con_num: &str) -> String {
    let mut q: Vec<(&str, String)> = vec![("v", "v4".into())];
    for (k, v) in &s.connectparams {
        let v = match v {
            Value::String(v) => v.clone(),
            other => other.to_string(),
        };
        q.push((k, v));
    }
    q.push(("tc", TC.into()));
    q.push(("con_num", con_num.into()));
    q.push(("epid", endpoint_id.into()));
    if let Some(ccid) = &s.ccid {
        q.push(("ccid", ccid.clone()));
    }
    q.push(("auth", "true".into()));
    q.push(("timeout", "40".into()));
    q.iter()
        .map(|(k, v)| format!("{}={}", enc(k), enc(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// Percent-encodes one query component.
fn enc(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

/// One registrar body (spec §4.6 step 4).
///
/// UNVERIFIED(§9.3): purple-teams' `clientDescription` / `transports` shape.
pub(crate) fn registration_body(
    app_id: &str,
    template_key: &str,
    registration_id: &str,
    path: &str,
) -> Value {
    json!({
        "clientDescription": {
            "appId": app_id,
            "aesKey": "",
            "languageId": "en-US",
            "platform": "edge",
            "templateKey": template_key,
            "platformUIVersion": PLATFORM_UI_VERSION,
        },
        "registrationId": registration_id,
        "nodeId": "",
        "transports": {
            "TROUTER": [{ "context": "", "path": path, "ttl": REGISTRATION_TTL_S }],
        },
    })
}

/// The registrations to make, as `(app, template, registrationId, path)`:
/// `TeamsCDLWebWorker` alone, or (`register_all_apps`) purple-teams'
/// calling apps first, each under a fresh uuid, then the CDL one.
///
/// UNVERIFIED(§9.3): whether `TeamsCDLWebWorker` alone gets `/messaging`
/// requests delivered.
pub(crate) fn registration_apps(
    register_all: bool,
    surl: &str,
    endpoint_id: &str,
) -> Vec<(String, String, String, String)> {
    let cdl = (
        "TeamsCDLWebWorker".to_string(),
        "TeamsCDLWebWorker_2.1".to_string(),
        endpoint_id.to_string(),
        surl.to_string(),
    );
    if !register_all {
        return vec![cdl];
    }
    vec![
        (
            "NextGenCalling".into(),
            "DesktopNgc_2.3:SkypeNgc".into(),
            uuid::Uuid::new_v4().to_string(),
            format!("{surl}NGCallManagerWin"),
        ),
        (
            "SkypeSpacesWeb".into(),
            "SkypeSpacesWeb_2.3".into(),
            uuid::Uuid::new_v4().to_string(),
            format!("{surl}SkypeSpacesWeb"),
        ),
        cdl,
    ]
}

/// Step 4's registration: `POST {teams}/registrar/prod/V2/registrations`
/// once per [`registration_apps`] entry, in order; the first non-2xx is the
/// error.
pub(crate) async fn register(
    http: &Http,
    endpoint_id: &str,
    surl: &str,
    register_all: bool,
) -> Result<()> {
    let url = format!(
        "{}/registrar/prod/V2/registrations",
        http.endpoints().teams.trim_end_matches('/')
    );
    for (app, template, registration_id, path) in registration_apps(register_all, surl, endpoint_id)
    {
        http.send(
            HttpRequest::post_json(
                url.clone(),
                registration_body(&app, &template, &registration_id, &path),
            )
            .label("registrar"),
        )
        .await?
        .ok()?;
        tracing::debug!(%app, "trouter registered");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use wiremock::matchers::{body_partial_json, header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::Error;
    use crate::test_support::{fixture_with_base, mount_auth, test_http};

    #[tokio::test]
    async fn negotiate_posts_v4a_then_gets_session() {
        let s = MockServer::start().await;
        mount_auth(&s).await;
        Mock::given(method("POST"))
            .and(path("/v4/a"))
            .and(query_param("epid", "ep-1"))
            .and(header("X-Skypetoken", "sk-1"))
            .and(header("Content-Length", "0"))
            .respond_with(ResponseTemplate::new(200).set_body_json(fixture_with_base(
                fixture!("9.03-trouter-connect/v4a.json"),
                &s,
            )))
            .expect(1)
            .mount(&s)
            .await;
        Mock::given(method("GET"))
            .and(path("/socket.io/1/"))
            .and(query_param("v", "v4"))
            .and(query_param("sr", "sr-value"))
            .and(query_param("sig", "sig-value"))
            .and(query_param("epid", "ep-1"))
            .and(query_param("ccid", "ccid-xyz"))
            .and(query_param("auth", "true"))
            .and(query_param("timeout", "40"))
            .and(query_param("con_num", "1760346000000_1"))
            .and(query_param("tc", TC))
            .and(header("X-Skypetoken", "sk-1"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(fixture!("9.03-trouter-connect/session.txt")),
            )
            .expect(1)
            .mount(&s)
            .await;
        let http = test_http(&s, None).await;
        let sess = negotiate(&http, "ep-1", &con_num(1_760_346_000_000, 1))
            .await
            .unwrap();
        assert_eq!(sess.session_id, "sessid-abc123");
        assert_eq!(sess.heartbeat_s, 40);
        assert_eq!(sess.close_s, 60);
        assert_eq!(
            sess.surl,
            "https://trouter2-azsc-ince-1-a.trouter.teams.microsoft.com:3443/v4/f/AbCdEf123456/"
        );
        let url = websocket_url(&sess, "ep-1", "1760346000000_1");
        assert!(url.starts_with(&format!(
            "{}/socket.io/1/websocket/sessid-abc123?v=v4&",
            s.uri().replace("http://", "ws://")
        )));
        assert!(url.contains("&sig=sig-value&") && url.contains("&auth=true&timeout=40"));
    }

    #[tokio::test]
    async fn register_single_app_by_default_and_three_when_asked() {
        let s = MockServer::start().await;
        mount_auth(&s).await;
        Mock::given(method("POST"))
            .and(path("/registrar/prod/V2/registrations"))
            .and(header("X-Skypetoken", "sk-1"))
            .and(header("Authorization", "Bearer at-1"))
            .and(body_partial_json(serde_json::json!({"clientDescription": {"appId": "TeamsCDLWebWorker", "aesKey": "", "languageId": "en-US", "platform": "edge", "templateKey": "TeamsCDLWebWorker_2.1", "platformUIVersion": "49/25113001312"}, "registrationId": "ep-1", "nodeId": "", "transports": {"TROUTER": [{"context": "", "path": "https://t/v4/f/x/", "ttl": 86400}]}})))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                fixture!("9.03-trouter-connect/registrar-ok.json"),
                "application/json",
            ))
            .expect(1)
            .mount(&s)
            .await;
        let http = test_http(&s, None).await;
        register(&http, "ep-1", "https://t/v4/f/x/", false)
            .await
            .unwrap();
        let apps = registration_apps(true, "https://t/v4/f/x/", "ep-1");
        assert_eq!(
            apps.iter().map(|a| a.0.as_str()).collect::<Vec<_>>(),
            vec!["NextGenCalling", "SkypeSpacesWeb", "TeamsCDLWebWorker"]
        );
        assert_eq!(apps[0].3, "https://t/v4/f/x/NGCallManagerWin");
        assert_eq!(apps[1].3, "https://t/v4/f/x/SkypeSpacesWeb");
        assert_eq!(apps[2].2, "ep-1");
        assert_ne!(apps[0].2, "ep-1", "non-CDL registrations use a fresh uuid");
    }

    #[tokio::test]
    async fn register_failure_is_error() {
        let s = MockServer::start().await;
        mount_auth(&s).await;
        Mock::given(method("POST"))
            .and(path("/registrar/prod/V2/registrations"))
            .respond_with(ResponseTemplate::new(400).set_body_string("bad"))
            .mount(&s)
            .await;
        let http = test_http(&s, None).await;
        assert!(matches!(
            register(&http, "ep-1", "https://t/", false).await,
            Err(Error::Http { status: 400, .. })
        ));
    }

    #[test]
    fn registration_body_is_the_spec_shape() {
        let spec: serde_json::Value = serde_json::from_str(
            r#"{"clientDescription":{"appId":"TeamsCDLWebWorker","aesKey":"","languageId":"en-US","platform":"edge","templateKey":"TeamsCDLWebWorker_2.1","platformUIVersion":"49/25113001312"},"registrationId":"ep-1","nodeId":"","transports":{"TROUTER":[{"context":"","path":"https://t/v4/f/x/","ttl":86400}]}}"#,
        )
        .unwrap();
        assert_eq!(
            registration_body(
                "TeamsCDLWebWorker",
                "TeamsCDLWebWorker_2.1",
                "ep-1",
                "https://t/v4/f/x/"
            ),
            spec
        );
    }

    #[tokio::test]
    async fn register_all_posts_three_apps_in_order() {
        let s = MockServer::start().await;
        mount_auth(&s).await;
        Mock::given(method("POST"))
            .and(path("/registrar/prod/V2/registrations"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .expect(3)
            .mount(&s)
            .await;
        let http = test_http(&s, None).await;
        register(&http, "ep-1", "https://t/v4/f/x/", true)
            .await
            .unwrap();
        let bodies: Vec<serde_json::Value> = s
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| r.url.path() == "/registrar/prod/V2/registrations")
            .map(|r| serde_json::from_slice(&r.body).unwrap())
            .collect();
        let apps: Vec<&str> = bodies
            .iter()
            .map(|b| b["clientDescription"]["appId"].as_str().unwrap())
            .collect();
        assert_eq!(
            apps,
            ["NextGenCalling", "SkypeSpacesWeb", "TeamsCDLWebWorker"]
        );
        assert_eq!(
            bodies[0]["clientDescription"]["templateKey"],
            "DesktopNgc_2.3:SkypeNgc"
        );
        assert_eq!(
            bodies[1]["transports"]["TROUTER"][0]["path"],
            "https://t/v4/f/x/SkypeSpacesWeb"
        );
    }

    #[tokio::test]
    async fn negotiate_defaults_socketio_and_rejects_bad_session_answers() {
        let s = MockServer::start().await;
        mount_auth(&s).await;
        Mock::given(method("POST"))
            .and(path("/v4/a"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "surl": "https://t/v4/f/x/", "connectparams": {"sig": "s"}
            })))
            .mount(&s)
            .await;
        Mock::given(method("GET"))
            .and(path("/socket.io/1/"))
            .and(query_param("sig", "s"))
            .respond_with(ResponseTemplate::new(200).set_body_string("sid:notanumber:60:websocket"))
            .expect(1)
            .mount(&s)
            .await;
        let http = test_http(&s, None).await;
        // No `socketio` ⇒ the trouter endpoint; no `ccid` ⇒ no parameter.
        let err = negotiate(&http, "ep-1", "1_1").await.unwrap_err();
        assert!(matches!(err, Error::Protocol(_)), "{err:?}");
        let get = s
            .received_requests()
            .await
            .unwrap()
            .into_iter()
            .find(|r| r.url.path() == "/socket.io/1/")
            .unwrap();
        assert!(!get.url.query().unwrap().contains("ccid="));
        assert!(parse_session("").is_err());
        assert_eq!(
            parse_session("a:1:2:websocket").unwrap(),
            ("a".to_string(), 1, 2)
        );
    }

    #[tokio::test]
    async fn negotiate_needs_surl_and_connectparams() {
        let s = MockServer::start().await;
        mount_auth(&s).await;
        Mock::given(method("POST"))
            .and(path("/v4/a"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"socketio": "x"})),
            )
            .mount(&s)
            .await;
        let http = test_http(&s, None).await;
        assert!(matches!(
            negotiate(&http, "ep-1", "1_1").await,
            Err(Error::Protocol(_))
        ));
    }

    #[test]
    fn websocket_url_uses_wss_for_https_and_debug_redacts() {
        let sess = TrouterSession {
            socketio: "https://pub-ent.trouter.teams.microsoft.com:443/".into(),
            surl: "https://t/v4/f/SECRETID/".into(),
            ccid: Some("ccid-secret".into()),
            connectparams: serde_json::json!({"sig": "sig-secret"})
                .as_object()
                .unwrap()
                .clone(),
            session_id: "sid-secret".into(),
            heartbeat_s: 40,
            close_s: 60,
        };
        assert!(
            websocket_url(&sess, "ep", "1_1").starts_with(
                "wss://pub-ent.trouter.teams.microsoft.com:443/socket.io/1/websocket/sid-secret?v=v4&sig=sig-secret&tc=%7B%22cv%22"
            )
        );
        let dbg = format!("{sess:?}");
        for secret in ["SECRETID", "ccid-secret", "sig-secret", "sid-secret"] {
            assert!(!dbg.contains(secret), "{dbg}");
        }
        assert!(dbg.contains("heartbeat_s: 40"));
    }
}
