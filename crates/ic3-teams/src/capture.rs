//! Capture mode (spec §4.11): one pretty-printed JSON file per HTTP exchange
//! or trouter frame, redacted before it is written — nothing secret reaches
//! disk. Captures still contain real chat content, so the directory is
//! created `0700`, files `0600`, and the repo's `captures/` is git-ignored.
//!
//! Capture never breaks the client: write failures are logged at `warn` and
//! the record call returns `None`.

use std::fmt;
use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};

use serde_json::{Value, json};

use crate::now_ms;
use crate::redact::{REDACTED, redact_form, redact_header, redact_json, redact_text, redact_url};

/// Longest sanitised label used in a file name.
const MAX_LABEL: usize = 64;

/// Writes capture files into one directory, numbered in record order.
#[derive(Debug)]
pub struct Capture {
    dir: PathBuf,
    seq: AtomicU64,
    last: Mutex<Option<PathBuf>>,
}

/// Which way a trouter frame went.
#[derive(Clone, Copy, Debug)]
pub enum Direction {
    /// Received from the server.
    In,
    /// Sent to the server.
    Out,
}

impl Direction {
    fn as_str(self) -> &'static str {
        match self {
            Direction::In => "in",
            Direction::Out => "out",
        }
    }
}

/// An HTTP request as sent, before redaction.
pub struct CapturedRequest {
    /// HTTP method, e.g. `POST`.
    pub method: String,
    /// Full request URL.
    pub url: String,
    /// Request headers (name, value).
    pub headers: Vec<(String, String)>,
    /// Request body, if any.
    pub body: Option<String>,
}

/// An HTTP response as received, before redaction.
pub struct CapturedResponse {
    /// HTTP status code.
    pub status: u16,
    /// Response headers (name, value).
    pub headers: Vec<(String, String)>,
    /// Response body text.
    pub body: String,
}

impl fmt::Debug for CapturedRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CapturedRequest")
            .field("method", &self.method)
            .field("url", &redact_url(&self.url))
            .field("headers", &header_names(&self.headers))
            .field("body_len", &self.body.as_ref().map(String::len))
            .finish()
    }
}

impl fmt::Debug for CapturedResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CapturedResponse")
            .field("status", &self.status)
            .field("headers", &header_names(&self.headers))
            .field("body_len", &self.body.len())
            .finish()
    }
}

fn header_names(h: &[(String, String)]) -> Vec<&str> {
    h.iter().map(|(name, _)| name.as_str()).collect()
}

impl Capture {
    /// Opens (creating if needed) a capture directory. Numbering continues
    /// after the highest-numbered file already there, so earlier captures
    /// are never overwritten.
    pub fn new(dir: impl Into<PathBuf>) -> io::Result<Capture> {
        let dir = dir.into();
        DirBuilder::new().recursive(true).mode(0o700).create(&dir)?;
        let start = fs::read_dir(&dir)?
            .filter_map(|e| e.ok())
            .filter_map(|e| existing_seq(e.file_name().to_str()?))
            .max()
            .unwrap_or(0);
        Ok(Capture {
            dir,
            seq: AtomicU64::new(start),
            last: Mutex::new(None),
        })
    }

    /// The capture directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Records one HTTP exchange as `{seq:06}-http-{label}.json`.
    pub fn record_http(
        &self,
        label: &str,
        req: &CapturedRequest,
        resp: &CapturedResponse,
    ) -> Option<PathBuf> {
        // Labels may be derived from URLs: redact before writing them anywhere.
        let label = redact_text(label);
        let doc = json!({
            "at": now_ms(),
            "kind": "http",
            "label": label,
            "request": {
                "method": req.method,
                "url": redact_url(&req.url),
                "headers": headers_json(&req.headers),
                "body": req.body.as_deref().map_or(Value::Null, |b| body_json(b, is_form(&req.headers))),
            },
            "response": {
                "status": resp.status,
                "headers": headers_json(&resp.headers),
                "body": body_json(&resp.body, is_form(&resp.headers)),
            },
        });
        self.write("http", &label, &doc)
    }

    /// Records one trouter frame as `{seq:06}-ws-{in|out}-{label}.json`.
    pub fn record_frame(
        &self,
        label: &str,
        dir: Direction,
        raw: &str,
        decoded: Option<&Value>,
    ) -> Option<PathBuf> {
        let label = redact_text(label);
        let decoded = decoded.map_or(Value::Null, |d| {
            let mut d = d.clone();
            redact_json(&mut d);
            d
        });
        let doc = json!({
            "at": now_ms(),
            "kind": "ws",
            "label": label,
            "direction": dir.as_str(),
            "raw": redact_text(raw),
            "decoded": decoded,
        });
        self.write(&format!("ws-{}", dir.as_str()), &label, &doc)
    }

    /// The most recently written capture file, if any.
    pub fn last_path(&self) -> Option<PathBuf> {
        self.last
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn write(&self, kind: &str, label: &str, doc: &Value) -> Option<PathBuf> {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed) + 1;
        let path = self
            .dir
            .join(format!("{seq:06}-{kind}-{}.json", sanitise_label(label)));
        match write_new(&path, doc) {
            Ok(()) => {
                *self.last.lock().unwrap_or_else(PoisonError::into_inner) = Some(path.clone());
                Some(path)
            }
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "capture write failed");
                None
            }
        }
    }
}

/// Creates `path` (`0600`, never overwriting) and writes `doc` pretty-printed.
fn write_new(path: &Path, doc: &Value) -> io::Result<()> {
    let mut bytes = serde_json::to_vec_pretty(doc)?;
    bytes.push(b'\n');
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(&bytes)
}

/// The sequence number of an existing capture file (`NNNNNN-…`).
fn existing_seq(name: &str) -> Option<u64> {
    let (digits, _) = name.split_once('-')?;
    (digits.len() == 6).then(|| digits.parse().ok()).flatten()
}

/// Lowercase, `[a-z0-9-]` only (anything else becomes `-`), at most
/// [`MAX_LABEL`] characters; `item` if empty.
fn sanitise_label(label: &str) -> String {
    let s: String = label
        .chars()
        .map(|c| c.to_ascii_lowercase())
        .map(|c| {
            if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .take(MAX_LABEL)
        .collect();
    if s.is_empty() { "item".to_string() } else { s }
}

fn headers_json(h: &[(String, String)]) -> Value {
    h.iter()
        .map(|(name, value)| {
            let value = redact_header(name, value);
            let value = if value == REDACTED {
                value
            } else {
                redact_text(&value)
            };
            json!([name, value])
        })
        .collect()
}

fn is_form(h: &[(String, String)]) -> bool {
    h.iter().any(|(name, value)| {
        name.eq_ignore_ascii_case("content-type")
            && value
                .to_ascii_lowercase()
                .starts_with("application/x-www-form-urlencoded")
    })
}

/// A body as JSON when it parses (redacted by key), otherwise as a redacted
/// string. Form redaction also applies to unlabelled bodies that look like
/// a form (`k=v&…`, no whitespace), since callers may not pass the
/// `Content-Type` that `reqwest` adds itself.
fn body_json(body: &str, form: bool) -> Value {
    if let Ok(mut v) = serde_json::from_str::<Value>(body) {
        redact_json(&mut v);
        return v;
    }
    let looks_like_form = body.contains('=') && !body.contains(char::is_whitespace);
    let body = if form || looks_like_form {
        redact_form(body)
    } else {
        body.to_string()
    };
    Value::String(redact_text(&body))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::redact::REDACTED;
    use std::os::unix::fs::PermissionsExt;

    fn all_text(dir: &Path) -> String {
        let mut out = String::new();
        for e in std::fs::read_dir(dir).unwrap() {
            out.push_str(&std::fs::read_to_string(e.unwrap().path()).unwrap());
        }
        out
    }

    #[test]
    fn writes_redacted_files_in_sequence() {
        let dir = tempfile::tempdir().unwrap();
        let c = Capture::new(dir.path().join("cap")).unwrap();
        let p1 = c
            .record_http(
                "authz",
                &CapturedRequest {
                    method: "POST".into(),
                    url: "https://teams.microsoft.com/api/authsvc/v1.0/authz".into(),
                    headers: vec![("Authorization".into(), "Bearer at".into())],
                    body: None,
                },
                &CapturedResponse {
                    status: 200,
                    headers: vec![],
                    body: "{\"tokens\":{\"skypeToken\":\"sk\"}}".into(),
                },
            )
            .unwrap();
        let p2 = c
            .record_frame(
                "ws",
                Direction::In,
                "3:::{\"id\":1}",
                Some(&serde_json::json!({"cp":"x"})),
            )
            .unwrap();
        assert!(
            p1.file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("000001-http-authz")
        );
        assert!(
            p2.file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("000002-ws-in")
        );
        let txt = std::fs::read_to_string(&p1).unwrap();
        assert!(!txt.contains("Bearer at") && !txt.contains("\"sk\"") && txt.contains(REDACTED));
        assert_eq!(c.last_path().unwrap(), p2);
    }

    #[test]
    fn http_file_shape() {
        let dir = tempfile::tempdir().unwrap();
        let c = Capture::new(dir.path()).unwrap();
        let p = c
            .record_http(
                "Conversations",
                &CapturedRequest {
                    method: "GET".into(),
                    url: "https://t.x/socket.io/1/?sr=1&sig=2".into(),
                    headers: vec![("Accept".into(), "application/json".into())],
                    body: Some("plain Bearer tok".into()),
                },
                &CapturedResponse {
                    status: 201,
                    headers: vec![("Set-Cookie".into(), "authtoken=q".into())],
                    body: "not json".into(),
                },
            )
            .unwrap();
        assert_eq!(p.file_name().unwrap(), "000001-http-conversations.json");
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert!(v["at"].as_i64().unwrap() > 1_700_000_000_000);
        assert_eq!(v["kind"], "http");
        assert_eq!(v["label"], "Conversations");
        assert_eq!(v["request"]["method"], "GET");
        assert_eq!(v["request"]["url"], "https://t.x/socket.io/1/?<redacted>");
        assert_eq!(
            v["request"]["headers"],
            serde_json::json!([["Accept", "application/json"]])
        );
        assert_eq!(v["request"]["body"], "plain Bearer <redacted>");
        assert_eq!(v["response"]["status"], 201);
        assert_eq!(
            v["response"]["headers"],
            serde_json::json!([["Set-Cookie", REDACTED]])
        );
        assert_eq!(v["response"]["body"], "not json");
    }

    #[test]
    fn json_bodies_are_stored_as_json() {
        let dir = tempfile::tempdir().unwrap();
        let c = Capture::new(dir.path()).unwrap();
        let p = c
            .record_http(
                "registrations",
                &CapturedRequest { method: "POST".into(), url: "https://teams.microsoft.com/registrar/prod/V2/registrations".into(), headers: vec![], body: Some(r#"{"registrationId":"ep","transports":{"TROUTER":[{"context":"","path":"https://h:3443/v4/f/SURLID/","ttl":86400}]}}"#.into()) },
                &CapturedResponse { status: 202, headers: vec![], body: String::new() },
            )
            .unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(v["request"]["body"]["registrationId"], "ep");
        assert_eq!(
            v["request"]["body"]["transports"]["TROUTER"][0]["path"],
            REDACTED
        );
        assert_eq!(v["response"]["body"], "");
        assert!(!all_text(dir.path()).contains("SURLID"));
    }

    #[test]
    fn form_bodies_are_redacted_with_or_without_content_type() {
        let dir = tempfile::tempdir().unwrap();
        let c = Capture::new(dir.path()).unwrap();
        let resp = CapturedResponse {
            status: 200,
            headers: vec![],
            body: "{}".into(),
        };
        let form = "client_id=1&grant_type=device_code&code=DEVCODE&refresh_token=RTOK&scope=a b";
        c.record_http(
            "token",
            &CapturedRequest {
                method: "POST".into(),
                url: "https://login/x/oauth2/v2.0/token".into(),
                headers: vec![(
                    "content-type".into(),
                    "application/x-www-form-urlencoded".into(),
                )],
                body: Some(form.into()),
            },
            &resp,
        )
        .unwrap();
        let bare = "client_id=1&grant_type=device_code&code=DEVCODE2";
        c.record_http(
            "token",
            &CapturedRequest {
                method: "POST".into(),
                url: "https://login/x/oauth2/v2.0/token".into(),
                headers: vec![],
                body: Some(bare.into()),
            },
            &resp,
        )
        .unwrap();
        let all = all_text(dir.path());
        for s in ["DEVCODE", "RTOK"] {
            assert!(!all.contains(s), "{s} leaked in {all}");
        }
        assert!(all.contains("client_id=1"));
    }

    #[test]
    fn frames_redact_raw_and_decoded() {
        let dir = tempfile::tempdir().unwrap();
        let c = Capture::new(dir.path()).unwrap();
        let raw = r#"5:::{"name":"user.authenticate","args":[{"headers":{"Authorization":"Bearer AT.AT.AT"},"connectparams":{"sr":"SRV","sig":"SIGV","se":"SEV","st":"STV"}}]}"#;
        let decoded: serde_json::Value = serde_json::from_str(&raw[4..]).unwrap();
        let p = c
            .record_frame("user.authenticate", Direction::Out, raw, Some(&decoded))
            .unwrap();
        assert_eq!(
            p.file_name().unwrap(),
            "000001-ws-out-user-authenticate.json"
        );
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(v["kind"], "ws");
        assert_eq!(v["direction"], "out");
        assert_eq!(v["label"], "user.authenticate");
        assert_eq!(v["decoded"]["args"][0]["connectparams"]["sr"], REDACTED);
        assert_eq!(
            v["decoded"]["args"][0]["headers"]["Authorization"],
            REDACTED
        );
        let all = all_text(dir.path());
        for s in ["AT.AT", "SRV", "SIGV", "SEV", "STV"] {
            assert!(!all.contains(s), "{s} leaked in {all}");
        }
        let q = c.record_frame("", Direction::In, "2::", None).unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&q).unwrap()).unwrap();
        assert_eq!(v["decoded"], serde_json::Value::Null);
        assert_eq!(v["raw"], "2::");
    }

    #[test]
    fn numbering_continues_in_an_existing_dir() {
        let dir = tempfile::tempdir().unwrap();
        let c = Capture::new(dir.path()).unwrap();
        let first = c.record_frame("a", Direction::In, "1::", None).unwrap();
        std::fs::write(dir.path().join("notes.txt"), "x").unwrap();
        let c2 = Capture::new(dir.path()).unwrap();
        let second = c2.record_frame("a", Direction::In, "1::", None).unwrap();
        assert!(
            second
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("000002-")
        );
        assert!(first.exists(), "the earlier capture is untouched");
    }

    #[test]
    fn labels_are_redacted_in_content_and_file_name() {
        let dir = tempfile::tempdir().unwrap();
        let c = Capture::new(dir.path()).unwrap();
        let label = "https://h:3443/v4/f/FIDX/socket.io/1/?v=v4&sr=SRVX&sig=SIGVX";
        let resp = CapturedResponse {
            status: 200,
            headers: vec![],
            body: String::new(),
        };
        let req = CapturedRequest {
            method: "GET".into(),
            url: "https://h/".into(),
            headers: vec![],
            body: None,
        };
        let p1 = c.record_http(label, &req, &resp).unwrap();
        let p2 = c
            .record_frame("x/v4/f/FIDX?sr=SRVX&sig=SIGVX", Direction::Out, "1::", None)
            .unwrap();
        for p in [&p1, &p2] {
            let name = p.file_name().unwrap().to_str().unwrap().to_string();
            let text = std::fs::read_to_string(p).unwrap();
            for s in ["FIDX", "SRVX", "SIGVX"] {
                assert!(
                    !name.to_uppercase().contains(s),
                    "{s} leaked in file name {name}"
                );
                assert!(!text.contains(s), "{s} leaked in {text}");
            }
        }
    }

    #[test]
    fn labels_are_sanitised() {
        assert_eq!(sanitise_label("Send Message/../x"), "send-message----x");
        assert_eq!(sanitise_label(""), "item");
        assert_eq!(sanitise_label(&"a".repeat(200)).len(), 64);
    }

    #[test]
    fn write_failure_returns_none_and_keeps_last() {
        let dir = tempfile::tempdir().unwrap();
        let c = Capture::new(dir.path().join("cap")).unwrap();
        let first = c.record_frame("a", Direction::In, "1::", None).unwrap();
        std::fs::remove_dir_all(c.dir()).unwrap();
        assert!(c.record_frame("b", Direction::In, "1::", None).is_none());
        assert_eq!(c.last_path(), Some(first));
    }

    #[test]
    fn files_are_private() {
        let dir = tempfile::tempdir().unwrap();
        let c = Capture::new(dir.path().join("cap")).unwrap();
        let p = c.record_frame("a", Direction::In, "1::", None).unwrap();
        assert_eq!(
            std::fs::metadata(c.dir()).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn capture_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Capture>();
        assert!(
            Capture::new(tempfile::tempdir().unwrap().path())
                .unwrap()
                .last_path()
                .is_none()
        );
    }
}
