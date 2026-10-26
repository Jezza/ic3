//! The shared HTTP layer (spec §4.4): one `reqwest::Client`, the per-host
//! header policy of §4.4.1, and the retry and 401 rules of §4.4.2.
//!
//! - Hosts are told apart by path (`classify`), so the same rules hold
//!   whether a service lives at its real host or behind a test server.
//! - A 401 is answered once: chatsvc and the `X-Skypetoken`-only trouter
//!   hosts get a new Skype token (`authz`), authsvc / registrar / `api/mt`
//!   a new access token. The token manager makes that single-flight, so
//!   concurrent 401s share one refresh. A second 401 after that refresh
//!   succeeded is an `Error::Http` 401: the session is alive, the endpoint
//!   refuses. Only the token manager ends a session (`invalid_grant`).
//! - `/api/csa/…` (CSA, spec A.3) carries only `Authorization: Bearer` with
//!   the CSA token (the main access token under `csa_audience = spaces`);
//!   a 401 renews that token once (decision #51).
//! - AMS (`/v1/objects…`, chat images) carries the Skype token; Microsoft
//!   Graph (`/v1.0/…`, shared files) a Graph bearer, granted lazily like the
//!   CSA token. Callers check those URLs' hosts first (`Api::fetch_media`):
//!   classification is by path, so it must never see a URL from elsewhere.
//!   Pre-authenticated URLs (Graph download and upload-session URLs) are
//!   sent with [`HttpRequest::without_tokens`].
//! - Media bodies are bytes ([`Body::Bytes`], [`HttpRequest::binary`]),
//!   size-capped while reading, and never written to captures.
//! - 429 and 5xx are retried with exponential backoff and jitter, honouring
//!   `Retry-After`, for at most three attempts; the last response is
//!   returned as-is.
//! - Every request has a 30 s timeout (spec §6.0.5).
//!
//! Log lines carry redacted URLs only; captures are redacted by `Capture`.

use std::sync::Arc;
use std::time::Duration;

use reqwest::Method;
use reqwest::header::{self, HeaderMap, HeaderName, HeaderValue};

use crate::auth::manager::{Scoped, TokenSnapshot, Tokens};
use crate::auth::{Endpoints, RefreshKind, USER_AGENT};
use crate::capture::{CapturedRequest, CapturedResponse};
use crate::redact::redact_url;
use crate::{Capture, Error, Result, Secret};

/// Per-request timeout (spec §6.0.5): the API has no long polls.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Attempts per request for 429 / 5xx, the first included (spec §4.4.2).
const MAX_ATTEMPTS: u32 = 3;
/// A `Retry-After` longer than this is not waited out: the response is
/// returned instead, so a caller is never stalled for minutes.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(60);
/// Teams web origin sent to chatsvc.
const TEAMS_ORIGIN: &str = "https://teams.microsoft.com";

/// Which service a URL belongs to, for the header policy and 401 rules.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Host {
    /// `{chatService}/v1/users/{ME or an MRI}/…`, `{chatService}/v1/threads/…`
    /// (a conversations `backwardLink` names the user by MRI).
    ChatSvc,
    /// `teams.microsoft.com/api/authsvc/…`.
    AuthSvc,
    /// `teams.microsoft.com/registrar/…`.
    Registrar,
    /// `teams.microsoft.com/api/mt/…`.
    Mt,
    /// `teams.microsoft.com/api/csa/…`: the chat service aggregator.
    Csa,
    /// AMS, the chat image store: `{ams}/v1/objects…`.
    Ams,
    /// Microsoft Graph: `graph.microsoft.com/v1.0/…`.
    Graph,
    /// `go.trouter…/v4/a` and the trouter `socket.io` session host.
    Trouter,
    /// Azure AD (`…/oauth2/…`); never sent tokens.
    Login,
    /// Anything else; never sent tokens.
    Other,
}

impl Host {
    fn needs_tokens(self) -> bool {
        !matches!(self, Host::Login | Host::Other)
    }

    /// What a 401 from this host renews (spec §4.4.2).
    fn refresh_kind(self) -> RefreshKind {
        match self {
            Host::ChatSvc | Host::Trouter | Host::Ams => RefreshKind::SkypeToken,
            _ => RefreshKind::AccessToken,
        }
    }
}

/// What one attempt is authenticated with.
enum Creds {
    /// Login and unknown hosts: nothing.
    None,
    /// The main snapshot (access and Skype tokens), boxed: it is far
    /// larger than the other variants (clippy `large_enum_variant`).
    Main(Box<TokenSnapshot>),
    /// A scoped bearer (CSA, Graph), what a 401 renews, and from which
    /// observed version.
    Scoped {
        bearer: Secret<String>,
        kind: RefreshKind,
        observed: u64,
    },
}

impl Creds {
    /// What a 401 renews, and the version the request saw; `None` without
    /// tokens.
    fn renewal(&self, host: Host) -> Option<(RefreshKind, u64)> {
        match self {
            Creds::None => None,
            Creds::Main(snap) => Some((host.refresh_kind(), snap.version)),
            Creds::Scoped { kind, observed, .. } => Some((*kind, *observed)),
        }
    }

    fn snapshot(&self) -> Option<&TokenSnapshot> {
        match self {
            Creds::Main(snap) => Some(&**snap),
            Creds::None | Creds::Scoped { .. } => None,
        }
    }
}

/// Classifies a URL by its path; unparseable URLs are `Other`.
pub(crate) fn classify(url: &str) -> Host {
    let Ok(url) = url::Url::parse(url) else {
        return Host::Other;
    };
    let p = url.path();
    if p.contains("/oauth2/") {
        Host::Login
    } else if p.starts_with("/api/authsvc/") {
        Host::AuthSvc
    } else if p.starts_with("/registrar/") {
        Host::Registrar
    } else if p.starts_with("/api/csa/") {
        Host::Csa
    } else if p.starts_with("/api/mt/") || p.starts_with("/ups/") {
        Host::Mt
    } else if p == "/v4/a" || p.starts_with("/v4/a/") || p.contains("/socket.io/") {
        Host::Trouter
    } else if p.starts_with("/v1/users/") || p.starts_with("/v1/threads") {
        Host::ChatSvc
    } else if p == "/v1/objects" || p.starts_with("/v1/objects/") {
        Host::Ams
    } else if p.starts_with("/v1.0/") {
        Host::Graph
    } else {
        Host::Other
    }
}

/// A request body.
pub(crate) enum Body {
    /// No body (`Content-Length: 0` on POST / PUT).
    Empty,
    /// JSON, with `Content-Type: application/json`.
    Json(serde_json::Value),
    /// `application/x-www-form-urlencoded`.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "request surface with no production caller yet")
    )]
    Form(Vec<(String, String)>),
    /// Plain text, sent as-is.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "request surface with no production caller yet")
    )]
    Text(String),
    /// Raw bytes (media), with their `Content-Type`; captured as a size only.
    Bytes { data: Vec<u8>, content_type: String },
}

/// One request, before the host's headers are applied.
pub(crate) struct HttpRequest {
    pub method: Method,
    pub url: String,
    pub body: Body,
    /// Extra headers; they replace the host's headers of the same name.
    pub headers: Vec<(String, String)>,
    /// Capture file label; empty means the URL path's last segment.
    pub label: &'static str,
    /// Overrides the path classification (`without_tokens`).
    host: Option<Host>,
    /// Read the response as bytes, at most this many (`binary`).
    max_body: Option<u64>,
    /// Overrides the 30 s timeout (media transfers).
    timeout: Option<Duration>,
}

impl HttpRequest {
    fn new(method: Method, url: impl Into<String>, body: Body) -> Self {
        HttpRequest {
            method,
            url: url.into(),
            body,
            headers: Vec::new(),
            label: "",
            host: None,
            max_body: None,
            timeout: None,
        }
    }

    pub fn get(url: impl Into<String>) -> Self {
        Self::new(Method::GET, url, Body::Empty)
    }

    pub fn post_json(url: impl Into<String>, v: serde_json::Value) -> Self {
        Self::new(Method::POST, url, Body::Json(v))
    }

    pub fn put_json(url: impl Into<String>, v: serde_json::Value) -> Self {
        Self::new(Method::PUT, url, Body::Json(v))
    }

    pub fn delete_json(url: impl Into<String>, v: serde_json::Value) -> Self {
        Self::new(Method::DELETE, url, Body::Json(v))
    }

    pub fn delete(url: impl Into<String>) -> Self {
        Self::new(Method::DELETE, url, Body::Empty)
    }

    pub fn post_empty(url: impl Into<String>) -> Self {
        Self::new(Method::POST, url, Body::Empty)
    }

    /// A `PUT` of raw bytes.
    pub fn put_bytes(url: impl Into<String>, data: Vec<u8>, content_type: &str) -> Self {
        Self::new(
            Method::PUT,
            url,
            Body::Bytes {
                data,
                content_type: content_type.to_string(),
            },
        )
    }

    /// Sends no tokens and no host headers whatever the path looks like:
    /// for pre-authenticated URLs (a Graph download or upload-session URL),
    /// which must never see a bearer.
    pub fn without_tokens(mut self) -> Self {
        self.host = Some(Host::Other);
        self
    }

    /// Reads the response as bytes into [`HttpResponse::bytes`], refusing
    /// (`Error::Invalid`) a body longer than `max` — by `Content-Length`
    /// before reading, else as soon as the count passes it. A 2xx body is
    /// never captured, only its size; another status keeps its (text) body
    /// in [`HttpResponse::body`] as usual.
    pub fn binary(mut self, max: u64) -> Self {
        self.max_body = Some(max);
        self
    }

    /// Replaces the 30 s per-request timeout (media transfers).
    pub fn timeout(mut self, t: Duration) -> Self {
        self.timeout = Some(t);
        self
    }

    pub fn label(mut self, l: &'static str) -> Self {
        self.label = l;
        self
    }

    pub fn header(mut self, k: &str, v: &str) -> Self {
        self.headers.push((k.to_string(), v.to_string()));
        self
    }

    /// The capture label: the explicit one, else the path's last segment.
    fn capture_label(&self) -> String {
        if !self.label.is_empty() {
            return self.label.to_string();
        }
        url::Url::parse(&self.url)
            .ok()
            .and_then(|u| {
                u.path_segments()?
                    .rev()
                    .find(|s| !s.is_empty())
                    .map(str::to_string)
            })
            .unwrap_or_default()
    }
}

/// A response, read in full.
pub(crate) struct HttpResponse {
    pub status: u16,
    pub headers: HeaderMap,
    /// The body as text; empty for a 2xx [`HttpRequest::binary`] response.
    pub body: String,
    /// A 2xx [`HttpRequest::binary`] response's body; empty otherwise.
    pub bytes: Vec<u8>,
}

impl std::fmt::Debug for HttpResponse {
    /// Status and size only: bodies hold chat content.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpResponse")
            .field("status", &self.status)
            .field("body_len", &self.body.len())
            .field("bytes_len", &self.bytes.len())
            .finish()
    }
}

impl HttpResponse {
    /// Parses the body as JSON (`Error::Protocol` if it is not).
    pub fn json<T: serde::de::DeserializeOwned>(&self) -> Result<T> {
        Ok(serde_json::from_str(&self.body)?)
    }

    /// Itself on 2xx; otherwise `Error::Http` with the status and body.
    pub fn ok(self) -> Result<HttpResponse> {
        if self.is_success() {
            Ok(self)
        } else {
            Err(Error::Http {
                status: self.status,
                body: self.body,
            })
        }
    }

    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

/// The shared client plus everything a request needs: tokens, endpoints and
/// the optional capture.
pub(crate) struct Http {
    client: reqwest::Client,
    tokens: Arc<Tokens>,
    endpoints: Endpoints,
    capture: Option<Arc<Capture>>,
}

impl Http {
    pub(crate) fn new(
        client: reqwest::Client,
        tokens: Arc<Tokens>,
        endpoints: Endpoints,
        capture: Option<Arc<Capture>>,
    ) -> Http {
        Http {
            client,
            tokens,
            endpoints,
            capture,
        }
    }

    /// The one client everything shares: Teams desktop `User-Agent`, 30 s
    /// timeout per request.
    pub(crate) fn build_client() -> Result<reqwest::Client> {
        Ok(reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(REQUEST_TIMEOUT)
            .build()?)
    }

    /// Sends `req` with its host's headers, answering a 401 once and retrying
    /// 429 / 5xx (spec §4.4.2) and connections that could not be made (the
    /// request never left, so even a send is safe to repeat). Any status
    /// that is not handled here comes back as `Ok`; use
    /// [`HttpResponse::ok`] to turn it into an error.
    pub(crate) async fn send(&self, req: HttpRequest) -> Result<HttpResponse> {
        let host = req.host.unwrap_or_else(|| classify(&req.url));
        let mut attempts = 0u32;
        let mut refreshed = false;
        loop {
            let creds = self.creds(host).await?;
            let resp = match self.send_once(&req, host, &creds).await? {
                Sent::Response(resp) => resp,
                Sent::NotConnected(e) => {
                    attempts += 1;
                    if attempts >= MAX_ATTEMPTS {
                        return Err(e);
                    }
                    let delay = backoff(attempts);
                    tracing::debug!(error = %e, attempts, ?delay, url = %redact_url(&req.url), "could not connect; retrying");
                    tokio::time::sleep(delay).await;
                    continue;
                }
            };
            if resp.status == 401
                && let Some((kind, observed)) = creds.renewal(host)
            {
                if refreshed {
                    // The refresh succeeded (a failed one returned above), so
                    // the session is fine: this endpoint refuses us.
                    tracing::warn!(url = %redact_url(&req.url), "401 again after a token refresh");
                    return Err(Error::Http {
                        status: resp.status,
                        body: resp.body,
                    });
                }
                refreshed = true;
                tracing::debug!(url = %redact_url(&req.url), ?host, "401; refreshing tokens once");
                self.tokens
                    .handle()
                    .await
                    .force_refresh(kind, observed)
                    .await?;
                continue;
            }
            // Only 429 / 5xx attempts count; an answered 401 does not.
            attempts += 1;
            match retry_delay(resp.status, &resp.headers, attempts) {
                Some(delay) => {
                    tracing::debug!(status = resp.status, attempts, ?delay, url = %redact_url(&req.url), "retrying");
                    tokio::time::sleep(delay).await;
                }
                None => return Ok(resp),
            }
        }
    }

    /// The tokens `host` needs: none, the main snapshot (waiting for
    /// `ready()`), or a scoped bearer (spec A.3: CSA waits only on what
    /// `csa_token()` needs; Graph likewise).
    async fn creds(&self, host: Host) -> Result<Creds> {
        if !host.needs_tokens() {
            return Ok(Creds::None);
        }
        let handle = self.tokens.handle().await;
        match host {
            Host::Csa => {
                let (bearer, observed) = handle.csa_token_versioned().await?;
                Ok(Creds::Scoped {
                    bearer,
                    kind: handle.csa_refresh_kind(),
                    observed,
                })
            }
            Host::Graph => {
                let (bearer, observed) = handle.scoped_token_versioned(Scoped::Graph).await?;
                Ok(Creds::Scoped {
                    bearer,
                    kind: Scoped::Graph.refresh_kind(),
                    observed,
                })
            }
            _ => Ok(Creds::Main(Box::new(handle.ready().await?))),
        }
    }

    /// The chat-service base URL from `authz`.
    pub(crate) async fn chat_service(&self) -> Result<String> {
        self.tokens
            .handle()
            .await
            .ready()
            .await?
            .chat_service
            .ok_or_else(|| Error::Protocol("no chatService".into()))
    }

    pub(crate) fn tokens(&self) -> &Arc<Tokens> {
        &self.tokens
    }

    pub(crate) fn endpoints(&self) -> &Endpoints {
        &self.endpoints
    }

    pub(crate) fn capture(&self) -> Option<&Arc<Capture>> {
        self.capture.as_ref()
    }

    /// The bare client, for callers that need no header policy.
    pub(crate) fn raw(&self) -> &reqwest::Client {
        &self.client
    }

    /// One attempt: build, send, read the whole body, capture, log. A
    /// connection that could not be made is `Sent::NotConnected`: the
    /// request never left, so it is safe to try again.
    async fn send_once(&self, req: &HttpRequest, host: Host, creds: &Creds) -> Result<Sent> {
        let mut builder = self.client.request(req.method.clone(), &req.url);
        builder = match &req.body {
            Body::Empty if req.method == Method::POST || req.method == Method::PUT => {
                builder.header(header::CONTENT_LENGTH, "0")
            }
            Body::Empty => builder,
            Body::Json(v) => builder.json(v),
            Body::Form(pairs) => builder.form(pairs),
            Body::Text(s) => builder.body(s.clone()),
            Body::Bytes { data, content_type } => builder
                .header(header::CONTENT_TYPE, content_type.as_str())
                .body(data.clone()),
        };
        if let Some(t) = req.timeout {
            builder = builder.timeout(t);
        }
        let mut headers = host_headers(host, creds)?;
        for (k, v) in &req.headers {
            let name = HeaderName::from_bytes(k.as_bytes())
                .map_err(|e| Error::Protocol(format!("bad header name {k:?}: {e}")))?;
            let value = HeaderValue::from_str(v)
                .map_err(|e| Error::Protocol(format!("bad value for header {k}: {e}")))?;
            headers.insert(name, value);
        }
        // `headers` replaces same-named headers (e.g. `.json()`'s
        // Content-Type) instead of adding a second one.
        let request = builder.headers(headers).build()?;
        let captured = self.capture.as_ref().map(|_| CapturedRequest {
            method: req.method.to_string(),
            url: req.url.clone(),
            headers: header_pairs(request.headers()),
            body: match &req.body {
                Body::Bytes { data, .. } => Some(not_captured(data.len())),
                _ => request
                    .body()
                    .and_then(|b| b.as_bytes())
                    .map(|b| String::from_utf8_lossy(b).into_owned()),
            },
        });

        let resp = match self.client.execute(request).await {
            Ok(resp) => resp,
            Err(e) if e.is_connect() => return Ok(Sent::NotConnected(e.into())),
            Err(e) => return Err(e.into()),
        };
        let status = resp.status().as_u16();
        let headers = resp.headers().clone();
        let binary = req.max_body.filter(|_| resp.status().is_success());
        let (body, bytes) = match binary {
            Some(max) => (String::new(), read_capped(resp, max).await?),
            None => (resp.text().await?, Vec::new()),
        };
        tracing::debug!(method = %req.method, url = %redact_url(&req.url), status, "http");

        if let (Some(capture), Some(captured)) = (&self.capture, captured) {
            let captured_body = match binary {
                Some(_) => not_captured(bytes.len()),
                None => body.clone(),
            };
            capture.record_http(
                &req.capture_label(),
                &captured,
                &CapturedResponse {
                    status,
                    headers: header_pairs(&headers),
                    body: captured_body,
                },
            );
        }
        Ok(Sent::Response(HttpResponse {
            status,
            headers,
            body,
            bytes,
        }))
    }
}

/// How one attempt ended.
enum Sent {
    Response(HttpResponse),
    /// The connection could not be made (refused, reset while connecting,
    /// DNS, TLS handshake): nothing was sent.
    NotConnected(Error),
}

/// What a capture holds instead of a media body.
fn not_captured(len: usize) -> String {
    format!("<{len} bytes, not captured>")
}

/// Reads a body as bytes, refusing one longer than `max`: by its
/// `Content-Length` before reading anything, else as soon as the count
/// passes `max` (the rest is never read).
async fn read_capped(mut resp: reqwest::Response, max: u64) -> Result<Vec<u8>> {
    const TOO_LARGE: Error = Error::Invalid("the media is larger than the size limit");
    if resp.content_length().is_some_and(|n| n > max) {
        return Err(TOO_LARGE);
    }
    let mut out = Vec::new();
    while let Some(chunk) = resp.chunk().await? {
        if (out.len() + chunk.len()) as u64 > max {
            return Err(TOO_LARGE);
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

/// The headers of spec §4.4.1 for `host`.
fn host_headers(host: Host, creds: &Creds) -> Result<HeaderMap> {
    let snap = creds.snapshot();
    let mut h = HeaderMap::new();
    let skype = || secret_header(snap.and_then(|s| s.skype_token.as_ref()), "", "Skype token");
    let bearer = || {
        secret_header(
            snap.and_then(|s| s.access_token.as_ref()),
            "Bearer ",
            "access token",
        )
    };
    match host {
        Host::ChatSvc => {
            h.insert("x-skypetoken", skype()?);
            h.insert(
                header::ACCEPT,
                HeaderValue::from_static("application/json; ver=1.0;"),
            );
            h.insert(
                "behavioroverride",
                HeaderValue::from_static("redirectAs404"),
            );
            h.insert("x-stratus-caller", HeaderValue::from_static("skypeteams"));
            h.insert("x-stratus-request", stratus_request_id());
            h.insert(header::ORIGIN, HeaderValue::from_static(TEAMS_ORIGIN));
            h.insert(
                header::REFERER,
                HeaderValue::from_static("https://teams.microsoft.com/"),
            );
        }
        Host::AuthSvc => {
            h.insert(header::AUTHORIZATION, bearer()?);
            h.insert(
                header::ACCEPT,
                HeaderValue::from_static("application/json; ver=1.0"),
            );
        }
        Host::Registrar => {
            h.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            );
            h.insert("x-skypetoken", skype()?);
            h.insert(header::AUTHORIZATION, bearer()?);
        }
        Host::Mt => {
            h.insert(header::AUTHORIZATION, bearer()?);
            h.insert("x-skypetoken", skype()?);
            h.insert(header::ACCEPT, HeaderValue::from_static("application/json"));
        }
        Host::Trouter => {
            h.insert("x-skypetoken", skype()?);
        }
        // UNVERIFIED(§9.13): CSA is assumed to need only the bearer (and our User-Agent): no Skype token, no Origin / Referer / x-ms-* headers.
        Host::Csa => {
            let Creds::Scoped { bearer, .. } = creds else {
                return Err(Error::Protocol("no CSA token".into()));
            };
            h.insert(
                header::AUTHORIZATION,
                secret_header(Some(bearer), "Bearer ", "CSA token")?,
            );
        }
        // Verified 2026-10-23 against a live account: `…asyncgw…/v1/objects/…`
        // answers 200 to `Authorization: skype_token <token>`, 401 to the main
        // access token as a Bearer (Teams web's own Bearer is some other
        // audience) and to `X-Skypetoken`.
        Host::Ams => {
            h.insert(header::AUTHORIZATION, skype_auth(snap)?);
        }
        Host::Graph => {
            let Creds::Scoped { bearer, .. } = creds else {
                return Err(Error::Protocol("no Graph token".into()));
            };
            h.insert(
                header::AUTHORIZATION,
                secret_header(Some(bearer), "Bearer ", "Graph token")?,
            );
        }
        Host::Login | Host::Other => {}
    }
    Ok(h)
}

/// `skype_token <Skype token>`, the AMS `Authorization` value.
fn skype_auth(snap: Option<&TokenSnapshot>) -> Result<HeaderValue> {
    secret_header(
        snap.and_then(|s| s.skype_token.as_ref()),
        "skype_token ",
        "Skype token",
    )
}

/// A token as a header value, marked sensitive so the HTTP stack never logs
/// it.
fn secret_header(token: Option<&Secret<String>>, prefix: &str, what: &str) -> Result<HeaderValue> {
    let token = token.ok_or_else(|| Error::Protocol(format!("no {what}")))?;
    let mut v = HeaderValue::from_str(&format!("{prefix}{}", token.expose()))
        .map_err(|_| Error::Protocol(format!("the {what} is not a valid header value")))?;
    v.set_sensitive(true);
    Ok(v)
}

/// `X-Stratus-Request`: eight random hex digits.
fn stratus_request_id() -> HeaderValue {
    let id = format!("{:08x}", rand::random::<u32>());
    HeaderValue::from_str(&id).unwrap_or_else(|_| HeaderValue::from_static("00000000"))
}

fn header_pairs(h: &HeaderMap) -> Vec<(String, String)> {
    h.iter()
        .map(|(k, v)| {
            let v = v
                .to_str()
                .map_or_else(|_| "<binary>".to_string(), str::to_string);
            (k.as_str().to_string(), v)
        })
        .collect()
}

/// How long to wait before the next attempt, after `attempts` attempts that
/// ended in `status`; `None` to stop and return the response. 429 and 5xx
/// are retried up to [`MAX_ATTEMPTS`] in all, after `Retry-After` (seconds)
/// when given — unless it is longer than [`MAX_RETRY_AFTER`] — or else
/// exponential backoff.
fn retry_delay(status: u16, headers: &HeaderMap, attempts: u32) -> Option<Duration> {
    let retryable = status == 429 || (500..=599).contains(&status);
    if !retryable || attempts >= MAX_ATTEMPTS {
        return None;
    }
    match retry_after(headers) {
        Some(d) if d > MAX_RETRY_AFTER => None,
        Some(d) => Some(d),
        None => Some(backoff(attempts)),
    }
}

/// 0.5 s, 1 s, 2 s, … after the 1st, 2nd, 3rd attempt, plus up to 250 ms of
/// jitter.
fn backoff(attempts: u32) -> Duration {
    let base = 500u64.saturating_mul(1u64 << attempts.saturating_sub(1).min(16));
    Duration::from_millis(base + rand::random_range(0..250))
}

/// `Retry-After` in seconds; the HTTP-date form is not used by these
/// services and falls back to backoff.
fn retry_after(h: &HeaderMap) -> Option<Duration> {
    let secs = h
        .get(header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()?;
    Some(Duration::from_secs(secs))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use reqwest::header::{HeaderMap, HeaderValue};
    use tokio_util::sync::CancellationToken;
    use wiremock::matchers::{header, header_regex, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::auth::manager::tests::{
        grants, mount_csa_grant, mount_success_mocks, quiet_client, session,
    };
    use crate::auth::manager::{ManagerDeps, Tokens};
    use crate::auth::{CsaAudience, MemoryTokenStore, TokenStore};

    /// wiremock 0.6 has no "header is missing" matcher.
    struct NoHeader(&'static str);
    impl wiremock::Match for NoHeader {
        fn matches(&self, r: &wiremock::Request) -> bool {
            !r.headers.contains_key(self.0)
        }
    }

    /// Token mocks (`at-{n}`, `sk-{n}`, chat service = the mock server), a
    /// seeded store and an `Http` over `client`.
    async fn rig_with(
        client: reqwest::Client,
        capture: Option<Arc<Capture>>,
    ) -> (MockServer, Http) {
        let s = MockServer::start().await;
        mount_success_mocks(&s, 3800, 3600).await;
        let store = Arc::new(MemoryTokenStore::default());
        store.save(&session("rt-0")).await.unwrap();
        let tokens = Tokens::new(ManagerDeps {
            http: quiet_client(),
            endpoints: Endpoints::all(&s.uri()),
            store,
            cancel: CancellationToken::new(),
            csa_audience: CsaAudience::default(),
        });
        let http = Http::new(client, Arc::new(tokens), Endpoints::all(&s.uri()), capture);
        (s, http)
    }

    async fn rig() -> (MockServer, Http) {
        rig_with(Http::build_client().unwrap(), None).await
    }

    /// For `start_paused` tests. The paused runtime auto-advances to the next
    /// timer whenever it parks on wiremock I/O, so the client must not own
    /// timers of its own: no 30 s total timeout and no pool idle timer.
    async fn paused_rig() -> (MockServer, Http) {
        let client = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .pool_idle_timeout(None)
            .build()
            .unwrap();
        rig_with(client, None).await
    }

    async fn count(s: &MockServer, pred: impl Fn(&str) -> bool) -> usize {
        s.received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| pred(r.url.path()))
            .count()
    }

    #[test]
    fn classify_by_path() {
        assert_eq!(
            classify("https://login.microsoftonline.com/x/oauth2/token"),
            Host::Login
        );
        assert_eq!(
            classify("https://teams.microsoft.com/api/authsvc/v1.0/authz"),
            Host::AuthSvc
        );
        assert_eq!(
            classify("https://teams.microsoft.com/registrar/prod/V2/registrations"),
            Host::Registrar
        );
        assert_eq!(
            classify("https://teams.microsoft.com/api/mt/apac/beta/users/fetchShortProfile"),
            Host::Mt
        );
        assert_eq!(
            classify("https://go.trouter.teams.microsoft.com/v4/a?epid=1"),
            Host::Trouter
        );
        assert_eq!(
            classify("https://trouter2-x.trouter.teams.microsoft.com:443/socket.io/1/?v=v4"),
            Host::Trouter
        );
        assert_eq!(
            classify("https://de.ng.msg.teams.microsoft.com/v1/users/ME/conversations"),
            Host::ChatSvc
        );
        assert_eq!(
            classify("https://de.ng.msg.teams.microsoft.com/v1/threads/19:x"),
            Host::ChatSvc
        );
        // Live 2026-10-14: the conversations `backwardLink` names the user by
        // MRI, not `ME`; sent without the Skype token it 401'd (errorCode 911).
        assert_eq!(
            classify(
                "https://de.ng.msg.teams.microsoft.com/v1/users/8:orgid:00000000-0000-4000-8000-000000000001/conversations?syncState=x"
            ),
            Host::ChatSvc
        );
        assert_eq!(
            classify(
                "https://teams.microsoft.com/api/csa/emea/api/v3/teams/users/me?isPrefetch=false"
            ),
            Host::Csa
        );
        assert_eq!(
            classify("https://eu-api.asm.skype.com/v1/objects/0-weu-d1-x/views/imgo"),
            Host::Ams
        );
        assert_eq!(
            classify("https://eu-api.asm.skype.com/v1/objects"),
            Host::Ams
        );
        assert_eq!(
            classify("https://graph.microsoft.com/v1.0/me/drive/items/1/invite"),
            Host::Graph
        );
        assert_eq!(classify("https://example.com/v1/objectsX"), Host::Other);
        assert_eq!(classify("https://example.com/v1/usersX"), Host::Other);
        assert_eq!(classify("https://example.com/other"), Host::Other);
        assert_eq!(classify("not a url"), Host::Other);
    }

    #[tokio::test]
    async fn chatsvc_headers() {
        let (s, http) = rig().await;
        Mock::given(method("GET"))
            .and(path("/v1/users/ME/conversations"))
            .and(header("X-Skypetoken", "sk-1"))
            .and(header("Accept", "application/json; ver=1.0;"))
            .and(header("BehaviorOverride", "redirectAs404"))
            .and(header("X-Stratus-Caller", "skypeteams"))
            .and(header_regex("X-Stratus-Request", "^[0-9a-f]{8}$"))
            .and(header("Origin", "https://teams.microsoft.com"))
            .and(header("Referer", "https://teams.microsoft.com/"))
            .and(header_regex("User-Agent", "Teams/24165"))
            .and(NoHeader("Authorization"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"conversations": []})),
            )
            .expect(1)
            .mount(&s)
            .await;
        let r = http
            .send(HttpRequest::get(format!(
                "{}/v1/users/ME/conversations",
                s.uri()
            )))
            .await
            .unwrap();
        assert_eq!(r.status, 200);
        assert!(r.is_success());
        let v: serde_json::Value = r.json().unwrap();
        assert_eq!(v, serde_json::json!({"conversations": []}));
    }

    #[tokio::test]
    async fn authsvc_headers() {
        let (s, http) = rig().await;
        Mock::given(method("GET"))
            .and(path("/api/authsvc/v1.0/other"))
            .and(header("Authorization", "Bearer at-1"))
            .and(header("Accept", "application/json; ver=1.0"))
            .and(NoHeader("X-Skypetoken"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&s)
            .await;
        let r = http
            .send(HttpRequest::get(format!(
                "{}/api/authsvc/v1.0/other",
                s.uri()
            )))
            .await
            .unwrap();
        assert_eq!(r.status, 200);
    }

    #[tokio::test]
    async fn other_hosts_headers() {
        let (s, http) = rig().await;
        Mock::given(method("POST"))
            .and(path("/registrar/prod/V2/registrations"))
            .and(header("Content-Type", "application/json"))
            .and(header("X-Skypetoken", "sk-1"))
            .and(header("Authorization", "Bearer at-1"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&s)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/mt/apac/beta/users/fetchShortProfile"))
            .and(header("Authorization", "Bearer at-1"))
            .and(header("X-Skypetoken", "sk-1"))
            .and(header("Accept", "application/json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
            .expect(1)
            .mount(&s)
            .await;
        Mock::given(method("POST"))
            .and(path("/v4/a"))
            .and(header("X-Skypetoken", "sk-1"))
            .and(header("Content-Length", "0"))
            .and(NoHeader("Authorization"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .expect(1)
            .mount(&s)
            .await;
        http.send(HttpRequest::post_json(
            format!("{}/registrar/prod/V2/registrations", s.uri()),
            serde_json::json!({}),
        ))
        .await
        .unwrap();
        http.send(HttpRequest::post_json(
            format!("{}/api/mt/apac/beta/users/fetchShortProfile", s.uri()),
            serde_json::json!([""]),
        ))
        .await
        .unwrap();
        http.send(HttpRequest::post_empty(format!("{}/v4/a?epid=ep", s.uri())))
            .await
            .unwrap();
        // A single Content-Type, not one from `.json()` plus one from the table.
        let reg = s.received_requests().await.unwrap();
        let reg = reg
            .iter()
            .find(|r| r.url.path() == "/registrar/prod/V2/registrations")
            .unwrap();
        assert_eq!(reg.headers.get_all("content-type").iter().count(), 1);
    }

    #[tokio::test]
    async fn unknown_hosts_get_no_tokens_and_401_passes_through() {
        let (s, http) = rig().await;
        Mock::given(method("GET"))
            .and(path("/other"))
            .and(NoHeader("X-Skypetoken"))
            .and(NoHeader("Authorization"))
            .respond_with(ResponseTemplate::new(401))
            .expect(1)
            .mount(&s)
            .await;
        let r = http
            .send(HttpRequest::get(format!("{}/other", s.uri())))
            .await
            .unwrap();
        assert_eq!(r.status, 401);
        assert_eq!(
            count(&s, |p| p.ends_with("/oauth2/v2.0/token")
                || p == "/api/authsvc/v1.0/authz")
            .await,
            0,
            "no token needed, none fetched"
        );
    }

    #[tokio::test]
    async fn request_headers_override_the_table() {
        let (s, http) = rig().await;
        Mock::given(method("PUT"))
            .and(path("/v1/users/ME/conversations/c/properties"))
            .and(header("Accept", "text/plain"))
            .and(header("X-Extra", "1"))
            .and(header("X-Skypetoken", "sk-1"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&s)
            .await;
        Mock::given(method("DELETE"))
            .and(path("/v1/users/ME/conversations/c/messages/1/properties"))
            .and(header("Content-Type", "application/json"))
            .and(wiremock::matchers::body_json(serde_json::json!({"k": 1})))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&s)
            .await;
        http.send(
            HttpRequest::put_json(
                format!("{}/v1/users/ME/conversations/c/properties", s.uri()),
                serde_json::json!({}),
            )
            .header("Accept", "text/plain")
            .header("X-Extra", "1"),
        )
        .await
        .unwrap()
        .ok()
        .unwrap();
        http.send(HttpRequest::delete_json(
            format!(
                "{}/v1/users/ME/conversations/c/messages/1/properties",
                s.uri()
            ),
            serde_json::json!({"k": 1}),
        ))
        .await
        .unwrap()
        .ok()
        .unwrap();
    }

    #[tokio::test]
    async fn form_and_text_bodies() {
        let (s, http) = rig().await;
        Mock::given(method("POST"))
            .and(path("/form"))
            .and(header("Content-Type", "application/x-www-form-urlencoded"))
            .and(wiremock::matchers::body_string("a=1&b=x+y"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&s)
            .await;
        Mock::given(method("POST"))
            .and(path("/text"))
            .and(wiremock::matchers::body_string("5:1+::{}"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&s)
            .await;
        let mut form = HttpRequest::post_empty(format!("{}/form", s.uri()));
        form.body = Body::Form(vec![("a".into(), "1".into()), ("b".into(), "x y".into())]);
        http.send(form).await.unwrap().ok().unwrap();
        let mut text = HttpRequest::post_empty(format!("{}/text", s.uri()));
        text.body = Body::Text("5:1+::{}".into());
        http.send(text).await.unwrap().ok().unwrap();
    }

    #[tokio::test]
    async fn accessors() {
        let (s, http) = rig().await;
        assert_eq!(http.endpoints(), &Endpoints::all(&s.uri()));
        assert!(http.capture().is_none());
        assert!(
            http.tokens().try_handle().is_none(),
            "nothing sent yet: lazy"
        );
        let r = http
            .raw()
            .get(format!("{}/raw", s.uri()))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 404, "no mock, no policy, no tokens");
        assert!(http.tokens().try_handle().is_none());
    }

    #[tokio::test]
    async fn chat_service_comes_from_authz() {
        let (s, http) = rig().await;
        assert_eq!(http.chat_service().await.unwrap(), s.uri());
    }

    #[tokio::test]
    async fn chatsvc_401_reruns_authz_once_then_retries() {
        let (s, http) = rig().await;
        let p = || path("/v1/users/ME/properties");
        Mock::given(method("GET"))
            .and(p())
            .and(header("X-Skypetoken", "sk-1"))
            .respond_with(ResponseTemplate::new(401))
            .expect(1)
            .mount(&s)
            .await;
        Mock::given(method("GET"))
            .and(p())
            .and(header("X-Skypetoken", "sk-2"))
            .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
            .expect(1)
            .mount(&s)
            .await;
        let r = http
            .send(HttpRequest::get(format!(
                "{}/v1/users/ME/properties",
                s.uri()
            )))
            .await
            .unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(
            count(&s, |p| p == "/api/authsvc/v1.0/authz").await,
            2,
            "initial + one re-run"
        );
        assert_eq!(
            count(&s, |p| p.ends_with("/oauth2/v2.0/token")).await,
            1,
            "access token not in its last 20 %, so not refreshed"
        );
    }

    #[tokio::test]
    async fn a_second_401_after_a_good_refresh_is_an_http_error() {
        // The refresh worked, so the session is alive: the endpoint itself
        // refuses (an /api/mt region that is not ours, a revoked
        // permission). That is an HTTP error, never a sign-in.
        let (s, http) = rig().await;
        Mock::given(method("GET"))
            .and(path("/v1/users/ME/properties"))
            .respond_with(ResponseTemplate::new(401).set_body_string("nope"))
            .mount(&s)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/mt/apac/beta/users/fetchShortProfile"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&s)
            .await;
        let r = http
            .send(HttpRequest::get(format!(
                "{}/v1/users/ME/properties",
                s.uri()
            )))
            .await;
        assert!(
            matches!(&r, Err(Error::Http { status: 401, body }) if body == "nope"),
            "{r:?}"
        );
        assert_eq!(
            count(&s, |p| p == "/v1/users/ME/properties").await,
            2,
            "one retry only"
        );
        let r = http
            .send(HttpRequest::post_json(
                format!("{}/api/mt/apac/beta/users/fetchShortProfile", s.uri()),
                serde_json::json!([""]),
            ))
            .await;
        assert!(matches!(r, Err(Error::Http { status: 401, .. })), "{r:?}");
        assert_eq!(
            http.tokens().handle().await.current().state,
            crate::auth::TokenState::Ready,
            "the session is kept"
        );
    }

    #[tokio::test]
    async fn a_connection_that_cannot_be_made_is_tried_three_times() {
        let (_s, http) = rig().await;
        // A port nothing listens on: the connection is refused.
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let started = std::time::Instant::now();
        let r = http
            .send(HttpRequest::get(format!("http://127.0.0.1:{port}/x")))
            .await;
        assert!(matches!(r, Err(Error::Network(_))), "{r:?}");
        // Two backoffs (0.5 s and 1 s, plus jitter) between three attempts.
        assert!(
            started.elapsed() >= Duration::from_millis(1_400),
            "{:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn mt_401_refreshes_access_token() {
        let (s, http) = rig().await;
        let p = || path("/api/mt/apac/beta/users/fetchShortProfile");
        Mock::given(method("POST"))
            .and(p())
            .and(header("Authorization", "Bearer at-1"))
            .respond_with(ResponseTemplate::new(401))
            .expect(1)
            .mount(&s)
            .await;
        Mock::given(method("POST"))
            .and(p())
            .and(header("Authorization", "Bearer at-2"))
            .respond_with(ResponseTemplate::new(200).set_body_string("[]"))
            .expect(1)
            .mount(&s)
            .await;
        let r = http
            .send(HttpRequest::post_json(
                format!("{}/api/mt/apac/beta/users/fetchShortProfile", s.uri()),
                serde_json::json!([""]),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(
            count(&s, |p| p == "/api/authsvc/v1.0/authz").await,
            1,
            "access token only"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn retry_after_and_backoff() {
        let (s, http) = paused_rig().await;
        let p = || path("/v1/users/ME/conversations");
        Mock::given(method("GET"))
            .and(p())
            .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "2"))
            .up_to_n_times(1)
            .mount(&s)
            .await;
        Mock::given(method("GET"))
            .and(p())
            .respond_with(ResponseTemplate::new(503))
            .up_to_n_times(1)
            .mount(&s)
            .await;
        Mock::given(method("GET"))
            .and(p())
            .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
            .mount(&s)
            .await;
        let t0 = tokio::time::Instant::now();
        let r = http
            .send(HttpRequest::get(format!(
                "{}/v1/users/ME/conversations",
                s.uri()
            )))
            .await
            .unwrap();
        assert_eq!(r.status, 200);
        assert!(
            t0.elapsed() >= Duration::from_secs(2),
            "honoured Retry-After"
        );
        assert_eq!(count(&s, |p| p == "/v1/users/ME/conversations").await, 3);
    }

    #[tokio::test(start_paused = true)]
    async fn gives_up_after_three_attempts() {
        let (s, http) = paused_rig().await;
        Mock::given(method("GET"))
            .and(path("/v1/users/ME/conversations"))
            .respond_with(ResponseTemplate::new(503).set_body_string("down"))
            .expect(3)
            .mount(&s)
            .await;
        let r = http
            .send(HttpRequest::get(format!(
                "{}/v1/users/ME/conversations",
                s.uri()
            )))
            .await
            .unwrap();
        assert_eq!(r.status, 503);
        assert!(!r.is_success());
        assert!(matches!(r.ok(), Err(Error::Http { status: 503, body }) if body == "down"));
    }

    #[tokio::test(start_paused = true)]
    async fn a_401_does_not_use_up_the_retry_budget() {
        let (s, http) = paused_rig().await;
        let p = || path("/v1/users/ME/conversations");
        Mock::given(method("GET"))
            .and(p())
            .and(header("X-Skypetoken", "sk-1"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&s)
            .await;
        Mock::given(method("GET"))
            .and(p())
            .respond_with(ResponseTemplate::new(503))
            .up_to_n_times(2)
            .mount(&s)
            .await;
        Mock::given(method("GET"))
            .and(p())
            .respond_with(ResponseTemplate::new(200))
            .mount(&s)
            .await;
        let r = http
            .send(HttpRequest::get(format!(
                "{}/v1/users/ME/conversations",
                s.uri()
            )))
            .await
            .unwrap();
        assert_eq!(r.status, 200, "401, then three attempts: 503, 503, 200");
        assert_eq!(count(&s, |p| p == "/v1/users/ME/conversations").await, 4);
    }

    #[test]
    fn retry_delays() {
        let none = HeaderMap::new();
        let mut after = HeaderMap::new();
        after.insert("retry-after", HeaderValue::from_static(" 2 "));
        let mut too_long = HeaderMap::new();
        too_long.insert("retry-after", HeaderValue::from_static("3600"));
        let mut date = HeaderMap::new();
        date.insert(
            "retry-after",
            HeaderValue::from_static("Wed, 21 Oct 2015 07:28:00 GMT"),
        );
        assert_eq!(retry_delay(429, &after, 1), Some(Duration::from_secs(2)));
        assert_eq!(retry_delay(503, &after, 2), Some(Duration::from_secs(2)));
        assert_eq!(retry_delay(503, &after, 3), None, "three attempts made");
        assert_eq!(
            retry_delay(429, &too_long, 1),
            None,
            "longer than we will wait: surface it"
        );
        for status in [429, 500, 502, 503, 599] {
            let d = retry_delay(status, &none, 1).unwrap();
            assert!(
                d >= Duration::from_millis(500) && d < Duration::from_millis(750),
                "{d:?}"
            );
            let d = retry_delay(status, &none, 2).unwrap();
            assert!(
                d >= Duration::from_millis(1000) && d < Duration::from_millis(1250),
                "{d:?}"
            );
        }
        let d = retry_delay(503, &date, 1).unwrap();
        assert!(
            d < Duration::from_millis(750),
            "unparseable Retry-After falls back to backoff"
        );
        for status in [200, 204, 400, 401, 403, 404, 409] {
            assert_eq!(retry_delay(status, &none, 1), None, "{status}");
        }
    }

    #[tokio::test]
    async fn concurrent_401s_share_one_authz() {
        let (s, http) = rig().await;
        let p = || path("/v1/users/ME/properties");
        Mock::given(method("GET"))
            .and(p())
            .and(header("X-Skypetoken", "sk-1"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&s)
            .await;
        Mock::given(method("GET"))
            .and(p())
            .and(header("X-Skypetoken", "sk-2"))
            .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
            .mount(&s)
            .await;
        let url = format!("{}/v1/users/ME/properties", s.uri());
        let (a, b, c) = tokio::join!(
            http.send(HttpRequest::get(&url)),
            http.send(HttpRequest::get(&url)),
            http.send(HttpRequest::get(&url))
        );
        assert!(a.is_ok() && b.is_ok() && c.is_ok());
        assert_eq!(
            count(&s, |p| p == "/api/authsvc/v1.0/authz").await,
            2,
            "initial + exactly one shared re-run"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn requests_time_out_after_30s() {
        // The real client: its 30 s timeout is the only timer the request
        // owns, so the paused clock jumps straight to it.
        let (s, http) = rig_with(Http::build_client().unwrap(), None).await;
        http.chat_service().await.unwrap(); // tokens first, so only the request is timed
        Mock::given(method("GET"))
            .and(path("/v1/users/ME/conversations"))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(120)))
            .mount(&s)
            .await;
        let t0 = tokio::time::Instant::now();
        let r = http
            .send(HttpRequest::get(format!(
                "{}/v1/users/ME/conversations?secret=q",
                s.uri()
            )))
            .await;
        assert!(
            matches!(&r, Err(Error::Network(m)) if m.contains("timed out") && !m.contains("secret")),
            "{r:?}"
        );
        assert!(t0.elapsed() >= Duration::from_secs(30));
    }

    #[tokio::test]
    async fn captures_are_written_when_enabled() {
        let dir = tempfile::tempdir().unwrap();
        let capture = Arc::new(Capture::new(dir.path()).unwrap());
        let (s, http) = rig_with(Http::build_client().unwrap(), Some(capture.clone())).await;
        Mock::given(method("GET"))
            .and(path("/v1/users/ME/conversations"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"conversations":[]}"#))
            .mount(&s)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/mt/apac/beta/users/fetchShortProfile"))
            .respond_with(ResponseTemplate::new(200).set_body_string("[]"))
            .mount(&s)
            .await;
        http.send(HttpRequest::get(format!(
            "{}/v1/users/ME/conversations?startTime=0",
            s.uri()
        )))
        .await
        .unwrap();
        let files = |suffix: &'static str| {
            let mut v: Vec<_> = std::fs::read_dir(dir.path())
                .unwrap()
                .map(|e| e.unwrap().file_name().into_string().unwrap())
                .filter(|n| n.contains("-http-") && n.ends_with(suffix))
                .collect();
            v.sort();
            v
        };
        assert_eq!(
            files(".json"),
            ["000001-http-conversations.json"],
            "label = last path segment"
        );
        let text = std::fs::read_to_string(capture.last_path().unwrap()).unwrap();
        assert!(!text.contains("sk-1") && !text.contains("at-1"), "{text}");
        assert!(
            text.contains("X-Skypetoken") || text.contains("x-skypetoken"),
            "{text}"
        );
        assert!(text.contains(r#""conversations""#), "{text}");

        http.send(
            HttpRequest::post_json(
                format!("{}/api/mt/apac/beta/users/fetchShortProfile", s.uri()),
                serde_json::json!(["", "8:orgid:x"]),
            )
            .label("short-profile"),
        )
        .await
        .unwrap();
        assert_eq!(files(".json").len(), 2);
        assert_eq!(
            files("-http-short-profile.json"),
            ["000002-http-short-profile.json"]
        );
        let text = std::fs::read_to_string(capture.last_path().unwrap()).unwrap();
        assert!(!text.contains("sk-1") && !text.contains("at-1"), "{text}");
        assert!(text.contains("8:orgid:x"), "request body recorded: {text}");
    }

    const CSA_ME: &str = "/api/csa/emea/api/v3/teams/users/me";

    /// Like [`rig`], plus CSA grants (`csa-{n}`), for `audience`.
    async fn csa_rig(audience: CsaAudience) -> (MockServer, Http) {
        let s = MockServer::start().await;
        mount_csa_grant(&s, 3600).await;
        mount_success_mocks(&s, 3800, 3600).await;
        let store = Arc::new(MemoryTokenStore::default());
        store.save(&session("rt-0")).await.unwrap();
        let tokens = Tokens::new(ManagerDeps {
            http: quiet_client(),
            endpoints: Endpoints::all(&s.uri()),
            store,
            cancel: CancellationToken::new(),
            csa_audience: audience,
        });
        let http = Http::new(
            Http::build_client().unwrap(),
            Arc::new(tokens),
            Endpoints::all(&s.uri()),
            None,
        );
        (s, http)
    }

    #[tokio::test]
    async fn csa_requests_carry_only_the_csa_bearer() {
        let (s, http) = csa_rig(CsaAudience::Chatsvcagg).await;
        Mock::given(method("GET"))
            .and(path(CSA_ME))
            .and(header("Authorization", "Bearer csa-1"))
            .and(header_regex("User-Agent", "Teams/24165"))
            .and(NoHeader("X-Skypetoken"))
            .and(NoHeader("Origin"))
            .and(NoHeader("Referer"))
            .and(NoHeader("BehaviorOverride"))
            .and(NoHeader("X-Stratus-Caller"))
            .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
            .expect(1)
            .mount(&s)
            .await;
        let r = http
            .send(HttpRequest::get(format!(
                "{}{CSA_ME}?isPrefetch=false",
                s.uri()
            )))
            .await
            .unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(grants(&s).await, (1, 1));
    }

    #[tokio::test]
    async fn a_csa_401_renews_the_csa_token_once_and_retries() {
        let (s, http) = csa_rig(CsaAudience::Chatsvcagg).await;
        Mock::given(method("GET"))
            .and(path(CSA_ME))
            .and(header("Authorization", "Bearer csa-1"))
            .respond_with(ResponseTemplate::new(401))
            .expect(1)
            .mount(&s)
            .await;
        Mock::given(method("GET"))
            .and(path(CSA_ME))
            .and(header("Authorization", "Bearer csa-2"))
            .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
            .expect(1)
            .mount(&s)
            .await;
        let r = http
            .send(HttpRequest::get(format!("{}{CSA_ME}", s.uri())))
            .await
            .unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(grants(&s).await, (2, 1), "one CSA renewal, no main refresh");
        assert_eq!(
            count(&s, |p| p == "/api/authsvc/v1.0/authz").await,
            1,
            "the Skype token is not involved"
        );
    }

    #[tokio::test]
    async fn a_second_csa_401_is_an_http_error_and_keeps_the_session() {
        let (s, http) = csa_rig(CsaAudience::Chatsvcagg).await;
        Mock::given(method("GET"))
            .and(path(CSA_ME))
            .respond_with(ResponseTemplate::new(401).set_body_string("nope"))
            .mount(&s)
            .await;
        let r = http
            .send(HttpRequest::get(format!("{}{CSA_ME}", s.uri())))
            .await;
        assert!(
            matches!(&r, Err(Error::Http { status: 401, body }) if body == "nope"),
            "{r:?}"
        );
        assert_eq!(count(&s, |p| p == CSA_ME).await, 2, "one retry only");
        assert_eq!(
            http.tokens().handle().await.current().state,
            crate::auth::TokenState::Ready
        );
    }

    /// Review Focus #2: concurrent 401s share one CSA renewal.
    #[tokio::test]
    async fn concurrent_csa_401s_share_one_renewal() {
        let (s, http) = csa_rig(CsaAudience::Chatsvcagg).await;
        Mock::given(method("GET"))
            .and(path(CSA_ME))
            .and(header("Authorization", "Bearer csa-1"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&s)
            .await;
        Mock::given(method("GET"))
            .and(path(CSA_ME))
            .and(header("Authorization", "Bearer csa-2"))
            .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
            .mount(&s)
            .await;
        let url = format!("{}{CSA_ME}", s.uri());
        let (a, b, c) = tokio::join!(
            http.send(HttpRequest::get(&url)),
            http.send(HttpRequest::get(&url)),
            http.send(HttpRequest::get(&url))
        );
        assert!(a.is_ok() && b.is_ok() && c.is_ok());
        assert_eq!(
            grants(&s).await,
            (2, 1),
            "initial grant + exactly one shared renewal"
        );
    }

    #[tokio::test]
    async fn under_spaces_csa_carries_the_access_token_and_a_401_renews_it() {
        let (s, http) = csa_rig(CsaAudience::Spaces).await;
        Mock::given(method("GET"))
            .and(path(CSA_ME))
            .and(header("Authorization", "Bearer at-1"))
            .and(NoHeader("X-Skypetoken"))
            .respond_with(ResponseTemplate::new(401))
            .expect(1)
            .mount(&s)
            .await;
        Mock::given(method("GET"))
            .and(path(CSA_ME))
            .and(header("Authorization", "Bearer at-2"))
            .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
            .expect(1)
            .mount(&s)
            .await;
        let r = http
            .send(HttpRequest::get(format!("{}{CSA_ME}", s.uri())))
            .await
            .unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(
            grants(&s).await,
            (0, 2),
            "no CSA grant; the access token renewed"
        );
    }
}
