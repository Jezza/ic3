//! socket.io 0.9 framing as trouter speaks it (spec §4.6, frame table), the
//! client frames we send, and request-body decoding.
//!
//! A frame is `kind:id:endpoint:data`. Raw frames carry the access token
//! (`user.authenticate`) and trouter credentials, so they are never logged
//! as-is: callers pass them through `redact_text` first.

use std::io::Read;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::{Error, Result};

/// Request header that marks a whole body as gzip + base64.
const CONTENT_ENCODING: &str = "X-Microsoft-Skype-Content-Encoding";
/// Largest body we inflate; anything bigger is a protocol error, not an
/// allocation.
const MAX_INFLATED: u64 = 16 * 1024 * 1024;

/// One parsed frame.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Frame {
    /// `0::` — the server closes the session.
    Disconnect,
    /// `1::` — socket.io connected.
    Connected,
    /// `2::` — heartbeat.
    Heartbeat,
    /// `3:::{json}` — a request the server pushes to us; ack it at once.
    Request {
        id: i64,
        method: String,
        url: String,
        headers: Map<String, Value>,
        /// Still encoded; see [`decode_request_body`].
        body: String,
    },
    /// `5:[seq[+]]::{json}` — a server event.
    Event {
        seq: Option<String>,
        name: String,
        args: Value,
    },
    /// `6:::…` — the server acks one of our sequenced frames.
    Ack(String),
    /// `7:::…` — the server reports an error.
    Error(String),
    /// Anything else, kept whole.
    Unknown(String),
}

/// The JSON of a `5:` event frame.
#[derive(Deserialize)]
struct EventWire {
    name: String,
    #[serde(default)]
    args: Value,
}

/// Parses one text frame; never fails (malformed frames are `Unknown`).
pub(crate) fn parse_frame(s: &str) -> Frame {
    let mut parts = s.splitn(4, ':');
    let kind = parts.next().unwrap_or_default();
    let id = parts.next();
    let _endpoint = parts.next();
    let data = parts.next().unwrap_or_default();
    let unknown = || Frame::Unknown(s.to_string());
    match (kind, id) {
        // A bare kind with no `:` is not a frame.
        (_, None) => unknown(),
        ("0", _) => Frame::Disconnect,
        ("1", _) => Frame::Connected,
        ("2", _) => Frame::Heartbeat,
        ("3", _) => parse_request(data).unwrap_or_else(unknown),
        ("5", Some(id)) => match serde_json::from_str::<EventWire>(data) {
            Ok(e) => Frame::Event {
                seq: (!id.is_empty()).then(|| id.trim_end_matches('+').to_string()),
                name: e.name,
                args: e.args,
            },
            Err(_) => unknown(),
        },
        ("6", _) => Frame::Ack(data.to_string()),
        ("7", _) => Frame::Error(data.to_string()),
        _ => unknown(),
    }
}

/// The JSON of a `3:::` request. Only an integer `id` is required: every
/// request that has one must be acked, so the other fields are lenient
/// (missing or mistyped `method` / `url` / `headers` are empty; a non-string
/// `body` is kept as its JSON text, `null` as empty).
fn parse_request(data: &str) -> Option<Frame> {
    let v: Value = serde_json::from_str(data).ok()?;
    let id = v.get("id")?.as_i64()?;
    let text = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    Some(Frame::Request {
        id,
        method: text("method"),
        url: text("url"),
        headers: v
            .get("headers")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default(),
        body: match v.get("body") {
            None | Some(Value::Null) => String::new(),
            Some(Value::String(s)) => s.clone(),
            Some(other) => other.to_string(),
        },
    })
}

/// `3:::{"id":<id>,"status":200,"body":""}` — the ack for request `id`.
pub(crate) fn encode_ack(id: i64) -> String {
    // Written by hand: a `json!` object would sort the keys.
    format!("3:::{{\"id\":{id},\"status\":200,\"body\":\"\"}}")
}

/// `5:::<json>` — an event that needs no ack.
pub(crate) fn encode_ephemeral(v: &Value) -> String {
    format!("5:::{v}")
}

/// `5:<seq>+::<json>` — an event the server acks with `6:::<seq>…`.
pub(crate) fn encode_sequenced(seq: u64, v: &Value) -> String {
    format!("5:{seq}+::{v}")
}

/// Decodes a request body (spec §4.6): the whole body is gzip + base64 when
/// `X-Microsoft-Skype-Content-Encoding: gzip`; then a JSON object with `cp`
/// holds gzip + base64 JSON, one with `gp` base64 JSON. Errors are
/// `Error::Protocol`.
pub(crate) fn decode_request_body(headers: &Map<String, Value>, body: &str) -> Result<Value> {
    let gzipped = headers.iter().any(|(k, v)| {
        k.eq_ignore_ascii_case(CONTENT_ENCODING)
            && v.as_str().is_some_and(|v| v.eq_ignore_ascii_case("gzip"))
    });
    let text = if gzipped {
        gunzip(&unbase64(body)?)?
    } else {
        body.to_string()
    };
    let v: Value = serde_json::from_str(&text)
        .map_err(|e| Error::Protocol(format!("trouter request body is not JSON: {e}")))?;
    let inner = if let Some(cp) = v.get("cp").and_then(Value::as_str) {
        gunzip(&unbase64(cp)?)?
    } else if let Some(gp) = v.get("gp").and_then(Value::as_str) {
        String::from_utf8(unbase64(gp)?)
            .map_err(|_| Error::Protocol("trouter `gp` body is not UTF-8".into()))?
    } else {
        return Ok(v);
    };
    serde_json::from_str(&inner)
        .map_err(|e| Error::Protocol(format!("trouter inner body is not JSON: {e}")))
}

fn unbase64(s: &str) -> Result<Vec<u8>> {
    STANDARD
        .decode(s.trim())
        .map_err(|e| Error::Protocol(format!("trouter body is not base64: {e}")))
}

fn gunzip(bytes: &[u8]) -> Result<String> {
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(bytes)
        .take(MAX_INFLATED + 1)
        .read_to_end(&mut out)
        .map_err(|e| Error::Protocol(format!("trouter body is not gzip: {e}")))?;
    if out.len() as u64 > MAX_INFLATED {
        return Err(Error::Protocol("trouter body inflates past 16 MiB".into()));
    }
    String::from_utf8(out).map_err(|_| Error::Protocol("trouter gzip body is not UTF-8".into()))
}

/// The `user.authenticate` event, sent ephemeral after `1::`.
///
/// UNVERIFIED(§9.3): the header set (`X-Ms-Test-User`, `Authorization`,
/// `X-MS-Migration`) and the echoed `connectparams` are purple-teams'.
/// The result carries the access token: never log it unredacted.
pub(crate) fn user_authenticate(access_token: &str, connectparams: &Value) -> Value {
    json!({
        "name": "user.authenticate",
        "args": [{
            "headers": {
                "X-Ms-Test-User": "False",
                "Authorization": format!("Bearer {access_token}"),
                "X-MS-Migration": "True",
            },
            "connectparams": connectparams,
        }],
    })
}

/// The `user.activity` event (sequenced): marks the endpoint active.
///
/// UNVERIFIED(§9.3): shape from purple-teams; `cv` is a
/// [`correlation_vector`] with `.0.1` appended.
pub(crate) fn user_activity(cv: &str) -> Value {
    json!({
        "name": "user.activity",
        "args": [{ "state": "active", "cv": format!("{cv}.0.1") }],
    })
}

/// The keep-alive event, sent sequenced every 30 s.
pub(crate) fn ping() -> Value {
    json!({ "name": "ping" })
}

/// A fresh MS-CV base: 16 random bytes as unpadded base64, i.e. 21 chars of
/// `[A-Za-z0-9+/]` and a last char from `AgQw` (it holds only 2 bits).
pub(crate) fn correlation_vector() -> String {
    base64::engine::general_purpose::STANDARD_NO_PAD.encode(rand::random::<[u8; 16]>())
}

/// gzip, then base64: builds encoded trouter bodies from readable JSON in
/// tests (spec §10).
#[cfg(test)]
pub(crate) fn gzip_base64(json: &str) -> String {
    use std::io::Write;
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    enc.write_all(json.as_bytes()).expect("write to a Vec");
    STANDARD.encode(enc.finish().expect("finish a Vec"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_each_frame_kind() {
        let lines: Vec<&str> = fixture!("9.03-trouter-connect/frames.txt")
            .lines()
            .collect();
        assert!(matches!(parse_frame(lines[0]), Frame::Connected));
        assert!(matches!(parse_frame(lines[1]), Frame::Heartbeat));
        match parse_frame(lines[2]) {
            Frame::Request {
                id,
                method,
                url,
                body,
                ..
            } => {
                assert_eq!(id, 7001);
                assert_eq!(method, "POST");
                assert!(url.ends_with("/messaging"));
                assert!(body.starts_with("{\"type\""));
            }
            f => panic!("{f:?}"),
        }
        assert!(
            matches!(parse_frame(lines[3]), Frame::Event { name, .. } if name == "trouter.message_loss")
        );
        assert!(
            matches!(parse_frame(lines[4]), Frame::Event { seq: Some(s), name, .. } if s == "3" && name == "other.event")
        );
        assert!(matches!(parse_frame(lines[5]), Frame::Ack(_)));
        assert!(matches!(parse_frame(lines[6]), Frame::Disconnect));
        assert!(matches!(parse_frame(lines[7]), Frame::Error(e) if e == "some error text"));
        assert!(matches!(parse_frame(lines[8]), Frame::Unknown(_)));
        assert!(matches!(parse_frame(""), Frame::Unknown(_)));
        assert!(matches!(parse_frame("3:::not json"), Frame::Unknown(_)));
    }

    #[test]
    fn encode_frames() {
        assert_eq!(
            encode_ack(7001),
            "3:::{\"id\":7001,\"status\":200,\"body\":\"\"}"
        );
        assert_eq!(
            encode_ephemeral(&serde_json::json!({"name":"x"})),
            "5:::{\"name\":\"x\"}"
        );
        assert_eq!(encode_sequenced(2, &ping()), "5:2+::{\"name\":\"ping\"}");
        let a = user_authenticate("tok", &serde_json::json!({"sr":"1"}));
        assert_eq!(a["name"], "user.authenticate");
        assert_eq!(a["args"][0]["headers"]["Authorization"], "Bearer tok");
        assert_eq!(a["args"][0]["headers"]["X-Ms-Test-User"], "False");
        assert_eq!(a["args"][0]["headers"]["X-MS-Migration"], "True");
        assert_eq!(a["args"][0]["connectparams"]["sr"], "1");
        let act = user_activity("abc");
        assert_eq!(act["args"][0]["state"], "active");
        assert_eq!(act["args"][0]["cv"], "abc.0.1");
        let cv = correlation_vector();
        assert_eq!(cv.len(), 22);
        assert!("AgQw".contains(cv.chars().last().unwrap()));
    }

    #[test]
    fn decode_bodies() {
        let plain = fixture!("9.03-trouter-connect/request-messaging.json");
        let req: serde_json::Value = serde_json::from_str(plain).unwrap();
        let headers = req["headers"].as_object().unwrap().clone();
        let body = decode_request_body(&headers, req["body"].as_str().unwrap()).unwrap();
        assert_eq!(body["type"], "EventMessage");
        // gzip + base64 whole body
        let mut gz_headers = headers.clone();
        gz_headers.insert("X-Microsoft-Skype-Content-Encoding".into(), "gzip".into());
        let body2 =
            decode_request_body(&gz_headers, &gzip_base64(req["body"].as_str().unwrap())).unwrap();
        assert_eq!(body2, body);
        // cp (gzip+base64 inner) and gp (base64 inner)
        let cp =
            serde_json::json!({"cp": gzip_base64("{\"type\":\"EventMessage\",\"inner\":true}")})
                .to_string();
        assert_eq!(decode_request_body(&headers, &cp).unwrap()["inner"], true);
        use base64::Engine;
        let gp = serde_json::json!({"gp": base64::engine::general_purpose::STANDARD.encode("{\"g\":1}")}).to_string();
        assert_eq!(decode_request_body(&headers, &gp).unwrap()["g"], 1);
        assert!(decode_request_body(&headers, "not json").is_err());
    }

    /// Any `3:::` request with an integer `id` must be acked, so only the
    /// id is required; every other field is taken leniently.
    #[test]
    fn requests_with_an_integer_id_always_parse() {
        assert!(matches!(
            parse_frame("3:::{\"id\":1}"),
            Frame::Request { id: 1, method, url, headers, body }
                if method.is_empty() && url.is_empty() && headers.is_empty() && body.is_empty()
        ));
        assert!(matches!(
            parse_frame("3:::{\"id\":2,\"method\":\"POST\",\"url\":\"/x/messaging\",\"headers\":null,\"body\":null}"),
            Frame::Request { id: 2, method, url, headers, body }
                if method == "POST" && url == "/x/messaging" && headers.is_empty() && body.is_empty()
        ));
        // A non-string body is kept as its JSON text, so it still decodes.
        match parse_frame(
            "3:::{\"id\":3,\"url\":7,\"headers\":[1],\"body\":{\"type\":\"EventMessage\"}}",
        ) {
            Frame::Request {
                id: 3,
                url,
                headers,
                body,
                ..
            } => {
                assert!(url.is_empty() && headers.is_empty());
                assert_eq!(
                    decode_request_body(&headers, &body).unwrap()["type"],
                    "EventMessage"
                );
            }
            f => panic!("{f:?}"),
        }
        assert!(matches!(
            parse_frame("3:::{\"id\":4,\"body\":12}"),
            Frame::Request { id: 4, body, .. } if body == "12"
        ));
        // Without an integer id there is nothing to ack.
        for bad in [
            "3:::{}",
            "3:::{\"id\":\"5\"}",
            "3:::{\"id\":1.5}",
            "3:::[1]",
        ] {
            assert!(matches!(parse_frame(bad), Frame::Unknown(_)), "{bad}");
        }
    }

    #[test]
    fn malformed_requests_and_events_are_unknown() {
        assert!(matches!(
            parse_frame("3:::{\"id\":1,\"method\":\"GET\",\"url\":\"/x\"}"),
            Frame::Request { body, headers, .. } if body.is_empty() && headers.is_empty()
        ));
        assert!(matches!(
            parse_frame("5:::{\"args\":[]}"),
            Frame::Unknown(_)
        ));
        assert!(matches!(parse_frame("5:::not json"), Frame::Unknown(_)));
        assert!(matches!(parse_frame("1"), Frame::Unknown(_)));
        assert_eq!(
            parse_frame("5:4+::{\"name\":\"n\"}"),
            Frame::Event {
                seq: Some("4".into()),
                name: "n".into(),
                args: serde_json::Value::Null
            }
        );
    }

    #[test]
    fn bad_encodings_are_protocol_errors() {
        let gz = serde_json::Map::from_iter([(
            "x-microsoft-skype-content-encoding".to_string(),
            serde_json::Value::from("GZIP"),
        )]);
        let none = serde_json::Map::new();
        // Header match is case-insensitive.
        assert_eq!(
            decode_request_body(&gz, &gzip_base64("{\"a\":1}")).unwrap()["a"],
            1
        );
        for (headers, body) in [
            (&gz, "%%%"),
            (&gz, "aGVsbG8="),
            (&none, "{\"cp\":\"aGVsbG8=\"}"),
            (&none, "{\"gp\":\"%%%\"}"),
        ] {
            assert!(matches!(
                decode_request_body(headers, body),
                Err(crate::Error::Protocol(_))
            ));
        }
    }

    #[test]
    fn authenticate_frame_redacts_for_logs() {
        let cp = serde_json::json!({"sr": "sr-secret", "sig": "sig-secret"});
        let frame = encode_ephemeral(&user_authenticate("at-secret", &cp));
        let logged = crate::redact::redact_text(&frame);
        for secret in ["at-secret", "sr-secret", "sig-secret"] {
            assert!(!logged.contains(secret), "{logged}");
        }
    }
}
