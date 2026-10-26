//! The redaction set of spec §4.11, applied at capture time and to every
//! `tracing` line (spec §4.1).
//!
//! Structured data (headers, JSON, URLs, form bodies) is redacted by name;
//! free text (log lines, raw frames) by the regex set in [`redact_text`].
//! Over-redaction is acceptable; a leak is not.

use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;

/// What every redacted value is replaced with.
pub const REDACTED: &str = "<redacted>";

/// Header names whose values are always secret (compared lowercase).
const SECRET_HEADERS: [&str; 4] = ["authorization", "x-skypetoken", "cookie", "set-cookie"];

/// JSON keys whose values are always secret (compared lowercase).
const SECRET_KEYS: [&str; 15] = [
    "access_token",
    "refresh_token",
    "id_token",
    "device_code",
    "skypetoken",
    "surl",
    "ccid",
    "sr",
    "se",
    "st",
    "sig",
    "authorization",
    "authtoken",
    // Pre-authenticated Graph URLs (download, upload session): `tempauth`.
    "@microsoft.graph.downloadurl",
    "uploadurl",
];

/// Query parameters that carry credentials (compared lowercase): trouter's,
/// and SharePoint's pre-authenticated `tempauth`.
const SECRET_QUERY_PARAMS: [&str; 7] = ["sig", "st", "se", "sr", "ccid", "authtoken", "tempauth"];

/// Form fields that carry OAuth secrets (compared lowercase).
const SECRET_FORM_FIELDS: [&str; 5] = [
    "refresh_token",
    "code",
    "client_secret",
    "access_token",
    "device_code",
];

/// The redacted value of one header: secret headers become [`REDACTED`],
/// everything else is returned unchanged.
pub fn redact_header(name: &str, value: &str) -> String {
    if is_secret_header(name) {
        REDACTED.to_string()
    } else {
        value.to_string()
    }
}

/// Redacts the values of secret headers in place; names are kept.
pub fn redact_headers(h: &mut [(String, String)]) {
    for (name, value) in h.iter_mut() {
        if is_secret_header(name) {
            *value = REDACTED.to_string();
        }
    }
}

/// Redacts a JSON value in place, recursively:
/// - a key in the secret set (body tokens, trouter credentials, or a secret
///   header name) has its whole value replaced;
/// - every leaf under `connectparams` is replaced;
/// - `path` under `transports` is replaced (the registrar body carries the
///   trouter `surl` as `transports.TROUTER[].path`);
/// - every other string goes through [`redact_text`].
pub fn redact_json(v: &mut Value) {
    walk_json(v, false);
}

fn walk_json(v: &mut Value, in_transports: bool) {
    match v {
        Value::Object(map) => {
            for (key, child) in map.iter_mut() {
                let key = key.to_ascii_lowercase();
                if key == "connectparams" {
                    redact_leaves(child);
                } else if is_secret_key(&key) || (in_transports && key == "path") {
                    *child = Value::String(REDACTED.to_string());
                } else {
                    walk_json(child, in_transports || key == "transports");
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                walk_json(item, in_transports);
            }
        }
        Value::String(s) => {
            let r = redact_text(s);
            if r != *s {
                *s = r;
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

fn redact_leaves(v: &mut Value) {
    match v {
        Value::Object(map) => map.values_mut().for_each(redact_leaves),
        Value::Array(items) => items.iter_mut().for_each(redact_leaves),
        _ => *v = Value::String(REDACTED.to_string()),
    }
}

/// Redacts a URL: the whole query of a `socket.io` URL, otherwise the values
/// of the trouter credential parameters (`sig`, `st`, `se`, `sr`, `ccid`,
/// `authtoken`). The result then goes through [`redact_text`], which covers
/// the surl id in a `/v4/f/{id}` path and secrets in a URL nested inside a
/// non-secret query parameter. URLs without secrets are returned unchanged.
pub fn redact_url(url: &str) -> String {
    let redacted = match url.split_once('?') {
        None => url.to_string(),
        Some((base, _)) if base.contains("socket.io/") => format!("{base}?{REDACTED}"),
        Some((base, query)) => format!("{base}?{}", redact_pairs(query, &SECRET_QUERY_PARAMS)),
    };
    redact_text(&redacted)
}

/// Redacts an `application/x-www-form-urlencoded` body: the values of
/// `refresh_token`, `code`, `client_secret`, `access_token` and `device_code`.
pub fn redact_form(body: &str) -> String {
    redact_pairs(body, &SECRET_FORM_FIELDS)
}

fn redact_pairs(pairs: &str, secret: &[&str]) -> String {
    pairs
        .split('&')
        .map(|pair| match pair.split_once('=') {
            Some((key, _)) if secret.contains(&key.to_ascii_lowercase().as_str()) => {
                format!("{key}={REDACTED}")
            }
            _ => pair.to_string(),
        })
        .collect::<Vec<_>>()
        .join("&")
}

/// The free-text rules, compiled once, applied in order.
static TEXT_RULES: LazyLock<Vec<(Regex, &'static str)>> = LazyLock::new(|| {
    [
        (r#"(?i)(bearer\s+)[A-Za-z0-9._~+/=-]+"#, "${1}<redacted>"),
        (
            r#"(?i)(x-skypetoken["':=\s]+)[A-Za-z0-9._~+/=-]+"#,
            "${1}<redacted>",
        ),
        (
            r#"(?i)((?:access|refresh|id)_token["':=\s]+)[^\s&",}]+"#,
            "${1}<redacted>",
        ),
        (r#"(?i)(skype_?token["':=\s]+)[^\s&",}]+"#, "${1}<redacted>"),
        (r#"(?i)(device_code["':=\s]+)[^\s&",}]+"#, "${1}<redacted>"),
        (r#"(?i)(surl["':=\s]+)[^\s&",}]+"#, "${1}<redacted>"),
        (r#"(?i)(socket\.io/[^\s?"']*\?)[^\s"']*"#, "${1}<redacted>"),
        (
            r#"(?i)([?&](?:sig|st|se|sr|ccid|authtoken|tempauth)=)[^&\s"']+"#,
            "${1}<redacted>",
        ),
        (r#"(?i)(\bsig=)[^&\s"']+"#, "${1}<redacted>"),
        // The `authtoken` cookie value, wherever it appears (not only after `?`/`&`).
        (r#"(?i)(\bauthtoken=)[^&\s"';]+"#, "${1}<redacted>"),
        // `connectparams` objects in JSON text (e.g. raw `user.authenticate` frames).
        (
            r#"(?i)("connectparams"\s*:\s*)\{[^{}]*\}"#,
            "${1}\"<redacted>\"",
        ),
        // Trouter credential keys as JSON fields outside a `connectparams` object.
        (
            r#"(?i)("(?:sr|se|st|sig|ccid|authtoken)"\s*:\s*")[^"]*"#,
            "${1}<redacted>",
        ),
        // The id segment of a trouter `surl` (`…/v4/f/{id}/`), e.g. in the registrar body.
        (r#"(?i)(/v4/f/)[^\s"'/?&]+"#, "${1}<redacted>"),
        // Any JWT-shaped string (access and Skype tokens are JWTs), wherever
        // it appears without a telltale key (e.g. echoed in a parse error).
        (r#"eyJ[\w-]{10,}\.[\w-]+\.[\w-]*"#, "<redacted>"),
    ]
    .into_iter()
    .map(|(re, rep)| (Regex::new(re).expect("redaction regex compiles"), rep))
    .collect()
});

/// Redacts secrets from free text (log lines, raw frames, non-JSON bodies).
pub fn redact_text(line: &str) -> String {
    let mut out = line.to_string();
    for (re, rep) in TEXT_RULES.iter() {
        if let std::borrow::Cow::Owned(s) = re.replace_all(&out, *rep) {
            out = s;
        }
    }
    out
}

fn is_secret_header(name: &str) -> bool {
    SECRET_HEADERS.contains(&name.to_ascii_lowercase().as_str())
}

fn is_secret_key(lower: &str) -> bool {
    SECRET_KEYS.contains(&lower) || SECRET_HEADERS.contains(&lower)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headers() {
        for h in [
            "Authorization",
            "authorization",
            "X-Skypetoken",
            "x-skypetoken",
            "Cookie",
            "Set-Cookie",
        ] {
            assert_eq!(redact_header(h, "v"), REDACTED, "{h}");
        }
        assert_eq!(redact_header("Accept", "a/b"), "a/b");
    }

    #[test]
    fn headers_in_place() {
        let mut h = vec![
            ("authorization".to_string(), "Bearer x".to_string()),
            ("Accept".to_string(), "a/b".to_string()),
            ("SET-COOKIE".to_string(), "authtoken=q".to_string()),
        ];
        redact_headers(&mut h);
        assert_eq!(h[0].1, REDACTED);
        assert_eq!(h[1].1, "a/b");
        assert_eq!(h[2].1, REDACTED);
        assert_eq!(h[0].0, "authorization", "names are kept");
    }

    #[test]
    fn json_keys_and_connectparams() {
        let mut v = serde_json::json!({"access_token":"a","refresh_token":"b","id_token":"c","device_code":"d","skypeToken":"e","skypetoken":"f",
            "surl":"https://t/v4/f/abc/","ccid":"g","connectparams":{"sr":"1","issuer":"prod-2","sig":"2"},"headers":{"Authorization":"Bearer x","X-Ms-Test-User":"False"},"keep":"me","nested":[{"refresh_token":"z"}]});
        redact_json(&mut v);
        for k in [
            "access_token",
            "refresh_token",
            "id_token",
            "device_code",
            "skypeToken",
            "skypetoken",
            "surl",
            "ccid",
        ] {
            assert_eq!(v[k], REDACTED, "{k}");
        }
        assert_eq!(v["connectparams"]["sr"], REDACTED);
        assert_eq!(v["connectparams"]["issuer"], REDACTED);
        assert_eq!(v["headers"]["Authorization"], REDACTED);
        assert_eq!(v["headers"]["X-Ms-Test-User"], "False");
        assert_eq!(v["keep"], "me");
        assert_eq!(v["nested"][0]["refresh_token"], REDACTED);
    }

    #[test]
    fn json_secret_header_names_are_redacted() {
        let mut v = serde_json::json!({"headers":{"X-Skypetoken":"sk","Cookie":"authtoken=q","Accept":"a/b"}});
        redact_json(&mut v);
        assert_eq!(v["headers"]["X-Skypetoken"], REDACTED);
        assert_eq!(v["headers"]["Cookie"], REDACTED);
        assert_eq!(v["headers"]["Accept"], "a/b");
    }

    /// Ruling D14 / spec §4.11: the registrar body carries the surl as
    /// `transports.TROUTER[].path`.
    #[test]
    fn registrar_body_surl_under_transports_path() {
        let surl = "https://pub-ent-euno-07-f.trouter.teams.microsoft.com:3443/v4/f/SECRETID/";
        let mut v = serde_json::json!({
            "clientDescription": {"appId":"TeamsCDLWebWorker","aesKey":"","languageId":"en-US","platform":"edge",
                "templateKey":"TeamsCDLWebWorker_2.1","platformUIVersion":"49/25113001312"},
            "registrationId": "ep-1", "nodeId": "",
            "transports": {"TROUTER": [{"context":"","path":surl,"ttl":86400}]}
        });
        redact_json(&mut v);
        assert_eq!(v["transports"]["TROUTER"][0]["path"], REDACTED);
        assert_eq!(v["transports"]["TROUTER"][0]["ttl"], 86400);
        assert_eq!(v["clientDescription"]["appId"], "TeamsCDLWebWorker");
        assert_eq!(v["registrationId"], "ep-1");
        assert!(!v.to_string().contains("SECRETID"));

        // An unrelated `path` key outside `transports` is left alone.
        let mut other = serde_json::json!({"path": "/v1/users/ME"});
        redact_json(&mut other);
        assert_eq!(other["path"], "/v1/users/ME");

        // The same body as a log line.
        let line = format!(
            "registrar body {}",
            serde_json::json!({"transports":{"TROUTER":[{"path":surl}]}})
        );
        assert!(
            !redact_text(&line).contains("SECRETID"),
            "{}",
            redact_text(&line)
        );
    }

    #[test]
    fn urls_forms_and_text() {
        assert_eq!(
            redact_url("https://t.x/socket.io/1/?v=v4&sr=1&sig=2&tc=x"),
            "https://t.x/socket.io/1/?<redacted>"
        );
        assert_eq!(
            redact_url("https://t.x/socket.io/1/websocket/abc?sig=2"),
            "https://t.x/socket.io/1/websocket/abc?<redacted>"
        );
        assert_eq!(
            redact_url("https://go.trouter.teams.microsoft.com/v4/a?epid=ep"),
            "https://go.trouter.teams.microsoft.com/v4/a?epid=ep"
        );
        assert_eq!(
            redact_form("client_id=1&refresh_token=rt&code=dc&grant_type=x"),
            "client_id=1&refresh_token=<redacted>&code=<redacted>&grant_type=x"
        );
        let line = "GET https://t.x/socket.io/1/?sr=1 Authorization: Bearer abc.def X-Skypetoken: sk123 {\"access_token\":\"zz\"} cookie authtoken=qq&x=1 sig=sss";
        let r = redact_text(line);
        for s in ["abc.def", "sk123", "zz", "sr=1", "qq", "sss"] {
            assert!(!r.contains(s), "{s} leaked in {r}");
        }
        assert!(r.contains("socket.io/1/?<redacted>"));
    }

    #[test]
    fn url_surl_path_segment() {
        assert_eq!(
            redact_url("https://h:3443/v4/f/ID/socket.io/1/?x"),
            "https://h:3443/v4/f/<redacted>/socket.io/1/?<redacted>"
        );
        assert_eq!(
            redact_url("https://h:3443/v4/f/ID/"),
            "https://h:3443/v4/f/<redacted>/"
        );
    }

    #[test]
    fn url_nested_in_a_query_param() {
        let r = redact_url("https://h/x?redirect=https://y/?sig=SIGV&a=1");
        assert!(!r.contains("SIGV"), "{r}");
        assert!(r.starts_with("https://h/x?redirect=https://y/?sig="), "{r}");
        let r = redact_url("https://h/x?next=https://t/socket.io/1/?sr=SRV");
        assert!(!r.contains("SRV"), "{r}");
    }

    #[test]
    fn url_query_params() {
        assert_eq!(
            redact_url("https://h/x?a=1&SIG=s&se=2&ccid=c&authtoken=t&b=2"),
            "https://h/x?a=1&SIG=<redacted>&se=<redacted>&ccid=<redacted>&authtoken=<redacted>&b=2"
        );
        assert_eq!(
            redact_url("https://h/socket.io/1/"),
            "https://h/socket.io/1/"
        );
        assert_eq!(
            redact_url("https://h/v1/users/ME/conversations"),
            "https://h/v1/users/ME/conversations"
        );
    }

    #[test]
    fn form_keys_are_case_insensitive_and_cover_device_code() {
        assert_eq!(
            redact_form("Device_Code=d&client_secret=s&access_token=a&scope=x"),
            "Device_Code=<redacted>&client_secret=<redacted>&access_token=<redacted>&scope=x"
        );
    }

    /// Spec §4.11: connectparams and the `Authorization` inside
    /// `user.authenticate` frames, seen as a raw frame or a log line.
    #[test]
    fn user_authenticate_frame_text() {
        let frame = r#"5:::{"name":"user.authenticate","args":[{"headers":{"X-Ms-Test-User":"False","Authorization":"Bearer eyJ0.eyJ1.sigpart","X-MS-Migration":"True"},"connectparams":{"sr":"SRV","issuer":"prod-2","sp":"connect","se":"1760346000000","st":"1760342400000","sig":"SIGV"}}]}"#;
        let r = redact_text(frame);
        for s in [
            "eyJ0",
            "SRV",
            "1760346000000",
            "1760342400000",
            "SIGV",
            "prod-2",
        ] {
            assert!(!r.contains(s), "{s} leaked in {r}");
        }
        assert!(r.contains("user.authenticate"), "{r}");
        assert!(r.contains("X-Ms-Test-User"), "{r}");
    }

    #[test]
    fn authtoken_cookie_value() {
        let r = redact_text(
            "Cookie: authtoken=Bearer%3DeyJabc%26Origin%3Dhttps%3A%2F%2Fteams.microsoft.com; other=1",
        );
        assert!(!r.contains("eyJabc"), "{r}");
        assert!(r.contains("other=1"), "{r}");
    }

    #[test]
    fn bare_jwts_are_redacted_wherever_they_appear() {
        let jwt = "eyJhbGciOiJSUzI1NiJ9.eyJzdWIiOiJhZGEifQ.c2lnbmF0dXJl-_x";
        let r = redact_text(&format!(
            "authz response: invalid type: string \"{jwt}\", expected"
        ));
        assert!(!r.contains("eyJzdWIiOiJhZGEifQ"), "{r}");
        assert!(!r.contains("c2lnbmF0dXJl"), "{r}");
        assert!(r.contains("authz response"), "{r}");
        // A short `eyJ` word is not a token.
        assert_eq!(redact_text("eyJabc.x"), "eyJabc.x");
    }

    /// Graph's pre-authenticated download and upload-session URLs carry
    /// `tempauth`, in a URL, a JSON field or free text.
    #[test]
    fn graph_preauthenticated_urls() {
        let url = "https://t-my.sharepoint.com/personal/a/_layouts/15/download.aspx?UniqueId=1&tempauth=v1.eyJxx.yy&ApiVersion=2.0";
        let r = redact_url(url);
        assert!(!r.contains("eyJxx") && r.contains("UniqueId=1"), "{r}");
        let mut v = serde_json::json!({"@microsoft.graph.downloadUrl": url, "uploadUrl": "https://u/x?guid=1", "name": "a.pdf"});
        redact_json(&mut v);
        assert_eq!(v["@microsoft.graph.downloadUrl"], REDACTED);
        assert_eq!(v["uploadUrl"], REDACTED);
        assert_eq!(v["name"], "a.pdf");
        assert!(!redact_text(&format!("GET {url}")).contains("eyJxx"));
        assert!(!redact_text("Authorization: skype_token abc.def").contains("abc.def"));
    }

    #[test]
    fn text_without_secrets_is_unchanged() {
        let line =
            "GET https://h/v1/users/ME/conversations?view=msnp24Equivalent&pageSize=100 status=200";
        assert_eq!(redact_text(line), line);
    }
}
