//! The web endpoint: Teams images and files behind media links, and
//! `soju.im/FILEHOST` uploads.
//!
//! - `GET /media/{token}/{name}` fetches the Teams URL behind `token` with
//!   the user's sign-in and passes it through.
//! - `POST /upload` takes a file (`Content-Type`, `Content-Disposition`,
//!   Basic auth with the IRC password when one is set) and answers `201`
//!   with its link in `Location`; `GET /upload/{token}/{name}` serves it.
//!
//! Browsers upload from the IRC client's own origin, so every response
//! allows any origin (CORS) and exposes `Location`.

use std::sync::Arc;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use base64::Engine;

use crate::backend::Backend;
use crate::media::{MediaTable, sanitize_name};

/// Largest upload accepted.
pub const MAX_UPLOAD_BYTES: usize = 100 * 1024 * 1024;

#[derive(Clone)]
struct AppState {
    backend: Arc<dyn Backend>,
    media: Arc<MediaTable>,
    password: Option<String>,
}

/// The router for the web endpoint.
pub fn router(
    backend: Arc<dyn Backend>,
    media: Arc<MediaTable>,
    password: Option<String>,
) -> Router {
    let state = AppState {
        backend,
        media,
        password,
    };
    Router::new()
        .route("/media/{token}/{name}", get(media_get))
        .route("/upload", axum::routing::post(upload).options(preflight))
        .route("/upload/{token}/{name}", get(upload_get))
        .layer(DefaultBodyLimit::max(MAX_UPLOAD_BYTES))
        .layer(axum::middleware::map_response(cors))
        .with_state(state)
}

async fn cors(mut res: Response) -> Response {
    let h = res.headers_mut();
    h.insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
    h.insert(
        header::ACCESS_CONTROL_EXPOSE_HEADERS,
        HeaderValue::from_static("Location"),
    );
    res
}

async fn preflight() -> Response {
    (
        StatusCode::NO_CONTENT,
        [
            (header::ACCESS_CONTROL_ALLOW_METHODS, "POST, GET, OPTIONS"),
            (
                header::ACCESS_CONTROL_ALLOW_HEADERS,
                "Authorization, Content-Type, Content-Disposition",
            ),
            (header::ACCESS_CONTROL_MAX_AGE, "86400"),
        ],
    )
        .into_response()
}

async fn media_get(
    State(s): State<AppState>,
    Path((token, name)): Path<(String, String)>,
) -> Response {
    let Some(remote) = s.media.remote(&token) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match s.backend.fetch_media(&remote.url).await {
        Ok(m) => {
            let ct = m
                .content_type
                .filter(|c| !c.is_empty() && c != "application/octet-stream")
                .unwrap_or_else(|| guess_type(&name).to_string());
            file_response(m.bytes, &ct, &remote.name)
        }
        Err(e) => {
            tracing::warn!(error = %e, "media fetch failed");
            (StatusCode::BAD_GATEWAY, "Teams refused the file").into_response()
        }
    }
}

async fn upload_get(
    State(s): State<AppState>,
    Path((token, _)): Path<(String, String)>,
) -> Response {
    match s.media.upload(&token) {
        Some(u) => file_response(u.bytes, &u.content_type, &u.name),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

fn file_response(bytes: Vec<u8>, content_type: &str, name: &str) -> Response {
    let inline = content_type.starts_with("image/")
        || content_type.starts_with("video/")
        || content_type.starts_with("audio/")
        || content_type == "application/pdf";
    let disposition = format!(
        "{}; filename*=UTF-8''{}",
        if inline { "inline" } else { "attachment" },
        encode_rfc5987(name)
    );
    let mut res = Response::new(Body::from(bytes));
    let h = res.headers_mut();
    if let Ok(v) = HeaderValue::from_str(content_type) {
        h.insert(header::CONTENT_TYPE, v);
    }
    if let Ok(v) = HeaderValue::from_str(&disposition) {
        h.insert(header::CONTENT_DISPOSITION, v);
    }
    h.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, max-age=3600"),
    );
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    res
}

async fn upload(
    State(s): State<AppState>,
    method: Method,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    debug_assert_eq!(method, Method::POST);
    if let Some(p) = &s.password
        && basic_password(&headers).as_deref() != Some(p.as_str())
    {
        return (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Basic realm=\"ic3-irc\"")],
        )
            .into_response();
    }
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_string();
    let name = headers
        .get(header::CONTENT_DISPOSITION)
        .and_then(|v| v.to_str().ok())
        .and_then(disposition_filename)
        .map(|n| sanitize_name(&n))
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "upload".into());
    match s.media.add_upload(&name, &content_type, body.to_vec()) {
        Some(link) => (StatusCode::CREATED, [(header::LOCATION, link)]).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// The password of a Basic `Authorization` header.
fn basic_password(headers: &HeaderMap) -> Option<String> {
    let v = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let b64 = v
        .strip_prefix("Basic ")
        .or_else(|| v.strip_prefix("basic "))?;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(b64.trim())
        .ok()?;
    let s = String::from_utf8(decoded).ok()?;
    s.split_once(':').map(|(_, p)| p.to_string())
}

/// The file name of a `Content-Disposition`: `filename*=UTF-8''…` first,
/// else `filename="…"`.
fn disposition_filename(v: &str) -> Option<String> {
    let parts = v.split(';').map(str::trim);
    let mut plain = None;
    for p in parts {
        if let Some(enc) = p.strip_prefix("filename*=") {
            let rest = enc.split_once("''").map_or(enc, |(_, r)| r);
            return Some(percent_decode(rest));
        }
        if let Some(n) = p.strip_prefix("filename=") {
            plain = Some(n.trim_matches('"').to_string());
        }
    }
    plain
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && let Some(hex) = s.get(i + 1..i + 3)
            && let Ok(v) = u8::from_str_radix(hex, 16)
        {
            out.push(v);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn encode_rfc5987(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

/// A content type from a file name's extension.
pub fn guess_type(name: &str) -> &'static str {
    let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mp3" => "audio/mpeg",
        "ogg" => "audio/ogg",
        "pdf" => "application/pdf",
        "txt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_disposition_names() {
        assert_eq!(
            disposition_filename("attachment; filename=\"a b.png\""),
            Some("a b.png".into())
        );
        assert_eq!(
            disposition_filename("attachment; filename=\"x\"; filename*=UTF-8''caf%C3%A9.jpg"),
            Some("café.jpg".into())
        );
        assert_eq!(disposition_filename("inline"), None);
    }

    #[test]
    fn basic_auth_password() {
        let mut h = HeaderMap::new();
        let v = base64::engine::general_purpose::STANDARD.encode("ada:pw:x");
        h.insert(header::AUTHORIZATION, format!("Basic {v}").parse().unwrap());
        assert_eq!(basic_password(&h), Some("pw:x".into()));
    }

    #[test]
    fn types_from_names() {
        assert_eq!(guess_type("A.PNG"), "image/png");
        assert_eq!(guess_type("noext"), "application/octet-stream");
    }
}
