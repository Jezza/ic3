//! The raw wire calls behind sign-in: device code start/poll, refresh and
//! `authz` (access token → Skype token). All take a bare `reqwest::Client`:
//! they run before any token exists.
//!
//! Error bodies from these endpoints carry no secrets, so a non-success
//! status becomes `Error::Http` with the raw body; the request form (which
//! does carry secrets) is never part of any error. Successful bodies, which
//! hold tokens, never appear in errors either: a 200 that fails to parse is
//! reported by serde's message alone.

use std::time::Duration;

use reqwest::header;
use serde::Deserialize;
use serde_json::Value;

use super::{CLIENT_ID, Endpoints, RESOURCE, SCOPE, USER_AGENT};
use crate::{Error, Result, Secret, Timestamp, now_ms};

/// Lifetime assumed when a token response omits (or garbles) `expires_in`.
/// Azure AD issues hour-long access tokens; `authz` ~3800 s. Being short is
/// harmless (an early refresh), being long is not.
const DEFAULT_LIFETIME_SECS: u64 = 3600;
/// Device-code defaults when the endpoint omits or garbles them (spec §4.3).
const DEFAULT_POLL_INTERVAL_SECS: u64 = 5;
const DEFAULT_CODE_LIFETIME_SECS: u64 = 900;
const DEVICE_CODE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";

/// What to show the user during device-code sign-in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceCodePrompt {
    /// The short code the user types at `verification_url`.
    pub user_code: String,
    /// Where the user goes to sign in.
    pub verification_url: String,
    /// The service's own wording, or a sentence built from the two above.
    pub message: String,
    /// When the code stops working (local clock, milliseconds).
    pub expires_at: Timestamp,
}

/// A started device-code flow: what to display plus what to poll with.
#[derive(Debug)]
pub(crate) struct DeviceCodeStart {
    /// The secret the poll presents; never shown.
    pub device_code: Secret<String>,
    pub prompt: DeviceCodePrompt,
    /// Minimum gap between polls, as asked by the service.
    pub interval: Duration,
}

/// A successful token response (device-code grant or refresh).
#[derive(Clone, Debug)]
pub(crate) struct TokenResponse {
    pub access_token: Secret<String>,
    /// Absent when the service did not rotate it.
    pub refresh_token: Option<Secret<String>>,
    /// Seconds.
    pub expires_in: u64,
}

/// One poll of the device-code token endpoint.
#[derive(Debug)]
pub(crate) enum PollOutcome {
    /// `authorization_pending`: keep polling.
    Pending,
    /// `slow_down`: keep polling, less often.
    SlowDown,
    Granted(TokenResponse),
}

/// The raw outcome of a refresh-token grant ([`refresh_grant`]): no error
/// mapping, so each caller applies its own rules.
#[derive(Debug)]
pub(crate) enum GrantOutcome {
    Granted(TokenResponse),
    /// A non-200 answer. `error` / `description` are the OAuth envelope's
    /// `error` / `error_description` when the body is one; `body` is the raw
    /// body (error bodies carry no secrets).
    Refused {
        status: u16,
        error: Option<String>,
        description: Option<String>,
        body: String,
    },
}

/// A successful `authz` response: the Skype token and where chat lives.
#[derive(Clone, Debug)]
pub(crate) struct AuthzResponse {
    pub skype_token: Secret<String>,
    /// Seconds.
    pub expires_in: u64,
    pub region: Option<String>,
    /// Chat-service base URL, without a trailing slash.
    pub chat_service: String,
    /// AMS (chat image store) base URL from `regionGtms.ams`, without a
    /// trailing slash, when given (§9.27).
    pub ams_service: Option<String>,
}

/// Device-code response, v1 or v2 shape (spec §4.3).
#[derive(Deserialize)]
struct DeviceCodeWire {
    device_code: Secret<String>,
    user_code: String,
    /// v1 endpoint key.
    verification_url: Option<String>,
    /// v2 endpoint key.
    verification_uri: Option<String>,
    #[serde(default)]
    interval: Value,
    #[serde(default)]
    expires_in: Value,
    message: Option<String>,
}

/// The OAuth error envelope: `{"error": "...", "error_description": "..."}`.
#[derive(Deserialize)]
struct OauthError {
    error: String,
    error_description: Option<String>,
}

/// Token response; `expires_in` is a string from the v1 endpoint and a number
/// from v2.
#[derive(Deserialize)]
struct TokenWire {
    access_token: Secret<String>,
    refresh_token: Option<Secret<String>>,
    #[serde(default)]
    expires_in: Value,
}

/// `authz` response: current shape (`tokens.skypeToken`) and the legacy one
/// (`skypeToken.skypetoken`) side by side. Both are read as raw values and
/// matched by shape ([`token_in`]): a typed field would put an unexpected
/// value (a token where an object was expected) into the error message.
#[derive(Deserialize)]
struct AuthzWire {
    tokens: Option<Value>,
    #[serde(rename = "skypeToken")]
    legacy: Option<Value>,
    region: Option<String>,
    #[serde(rename = "regionGtms")]
    region_gtms: Option<RegionGtms>,
}

/// `{key: "<token>", "expiresIn": …}` inside `v`, if `v` has that shape.
fn token_in(v: Option<&Value>, key: &str) -> Option<(Secret<String>, Value)> {
    let o = v?.as_object()?;
    let token = o.get(key)?.as_str()?;
    let expires_in = o.get("expiresIn").cloned().unwrap_or(Value::Null);
    Some((Secret::new(token.to_owned()), expires_in))
}

#[derive(Deserialize)]
struct RegionGtms {
    #[serde(rename = "chatService")]
    chat_service: Option<String>,
    #[serde(default)]
    ams: Option<String>,
}

/// A number, or a numeric string (the v1 endpoints send `"5"`), else `default`.
fn lenient_u64(v: &Value, default: u64) -> u64 {
    v.as_u64()
        .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
        .unwrap_or(default)
}

/// `now + secs`, saturating rather than wrapping on absurd lifetimes.
pub(crate) fn expiry_from_now(secs: u64) -> Timestamp {
    let ms = i64::try_from(secs.saturating_mul(1000)).unwrap_or(i64::MAX);
    now_ms().saturating_add(ms)
}

/// POST a form with our User-Agent; returns the status and the raw body.
async fn post_form(
    http: &reqwest::Client,
    url: &str,
    form: &[(&str, &str)],
) -> Result<(u16, String)> {
    let resp = http
        .post(url)
        .header(header::USER_AGENT, USER_AGENT)
        .form(form)
        .send()
        .await?;
    let status = resp.status().as_u16();
    let body = resp.text().await?;
    Ok((status, body))
}

fn parse_token(body: &str) -> Result<TokenResponse> {
    let w: TokenWire =
        serde_json::from_str(body).map_err(|e| Error::Protocol(format!("token response: {e}")))?;
    Ok(TokenResponse {
        access_token: w.access_token,
        refresh_token: w.refresh_token,
        expires_in: lenient_u64(&w.expires_in, DEFAULT_LIFETIME_SECS),
    })
}

fn parse_oauth_error(body: &str) -> Option<OauthError> {
    serde_json::from_str(body).ok()
}

fn is_reauth(error: &str) -> bool {
    matches!(error, "invalid_grant" | "interaction_required")
}

/// `POST {login}/{tenant}/oauth2/devicecode` (spec §2).
pub(crate) async fn start_device_code(
    http: &reqwest::Client,
    ep: &Endpoints,
    tenant: &str,
) -> Result<DeviceCodeStart> {
    let url = format!("{}/{tenant}/oauth2/devicecode", ep.login);
    let (status, body) = post_form(
        http,
        &url,
        &[("client_id", CLIENT_ID), ("resource", RESOURCE)],
    )
    .await?;
    if status != 200 {
        return Err(Error::Http { status, body });
    }
    let w: DeviceCodeWire = serde_json::from_str(&body)
        .map_err(|e| Error::Protocol(format!("device code response: {e}")))?;
    let verification_url = w.verification_url.or(w.verification_uri).ok_or_else(|| {
        Error::Protocol("device code response: no verification_url/verification_uri".into())
    })?;
    // A 0 s interval would poll in a hot loop.
    let interval = lenient_u64(&w.interval, DEFAULT_POLL_INTERVAL_SECS).max(1);
    let expires_in = lenient_u64(&w.expires_in, DEFAULT_CODE_LIFETIME_SECS);
    let message = w.message.unwrap_or_else(|| {
        format!(
            "To sign in, use a web browser to open {verification_url} and enter the code {}.",
            w.user_code
        )
    });
    Ok(DeviceCodeStart {
        device_code: w.device_code,
        prompt: DeviceCodePrompt {
            user_code: w.user_code,
            verification_url,
            message,
            expires_at: expiry_from_now(expires_in),
        },
        interval: Duration::from_secs(interval),
    })
}

/// One poll of `POST {login}/{tenant}/oauth2/token` with the device-code
/// grant (spec §2). `invalid_grant` / `interaction_required` mean the flow is
/// dead: start again.
pub(crate) async fn poll_device_code(
    http: &reqwest::Client,
    ep: &Endpoints,
    tenant: &str,
    device_code: &Secret<String>,
) -> Result<PollOutcome> {
    let url = format!("{}/{tenant}/oauth2/token", ep.login);
    let form = [
        ("client_id", CLIENT_ID),
        ("grant_type", DEVICE_CODE_GRANT),
        ("code", device_code.expose().as_str()),
    ];
    let (status, body) = post_form(http, &url, &form).await?;
    if status == 200 {
        return Ok(PollOutcome::Granted(parse_token(&body)?));
    }
    let Some(oauth) = parse_oauth_error(&body) else {
        return Err(Error::Http { status, body });
    };
    match oauth.error.as_str() {
        "authorization_pending" => Ok(PollOutcome::Pending),
        "slow_down" => Ok(PollOutcome::SlowDown),
        e if is_reauth(e) => Err(Error::ReauthRequired),
        e => {
            let mut msg = format!("device code: {e}");
            if let Some(d) = oauth.error_description {
                msg.push_str(": ");
                msg.push_str(&d);
            }
            Err(Error::Protocol(msg))
        }
    }
}

/// `POST {login}/{tenant}/oauth2/v2.0/token` with the refresh-token grant
/// for `scope` (spec §2, A.2). Transport failures are `Network`, a 200 that
/// is not a token response `Protocol`; every non-200 is
/// [`GrantOutcome::Refused`], unmapped.
pub(crate) async fn refresh_grant(
    http: &reqwest::Client,
    ep: &Endpoints,
    tenant: &str,
    refresh_token: &Secret<String>,
    scope: &str,
) -> Result<GrantOutcome> {
    let url = format!("{}/{tenant}/oauth2/v2.0/token", ep.login);
    let form = [
        ("client_id", CLIENT_ID),
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token.expose().as_str()),
        ("scope", scope),
    ];
    let (status, body) = post_form(http, &url, &form).await?;
    if status == 200 {
        return Ok(GrantOutcome::Granted(parse_token(&body)?));
    }
    let (error, description) = match parse_oauth_error(&body) {
        Some(e) => (Some(e.error), e.error_description),
        None => (None, None),
    };
    Ok(GrantOutcome::Refused {
        status,
        error,
        description,
        body,
    })
}

/// [`refresh_grant`] for the main [`SCOPE`] (spec §2): `invalid_grant` /
/// `interaction_required` ⇒ `ReauthRequired`; any other failure is `Http`
/// with the raw error body.
pub(crate) async fn refresh_access_token(
    http: &reqwest::Client,
    ep: &Endpoints,
    tenant: &str,
    refresh_token: &Secret<String>,
) -> Result<TokenResponse> {
    match refresh_grant(http, ep, tenant, refresh_token, SCOPE).await? {
        GrantOutcome::Granted(t) => Ok(t),
        GrantOutcome::Refused { error: Some(e), .. } if is_reauth(&e) => Err(Error::ReauthRequired),
        GrantOutcome::Refused { status, body, .. } => Err(Error::Http { status, body }),
    }
}

/// `POST {teams}/api/authsvc/v1.0/authz` with the access token, empty body
/// (spec §2): trades the access token for the Skype token and the chat
/// service's base URL (falling back to `https://{region}.ng.msg.teams.microsoft.com`).
pub(crate) async fn authz(
    http: &reqwest::Client,
    ep: &Endpoints,
    access_token: &Secret<String>,
) -> Result<AuthzResponse> {
    let url = format!("{}/api/authsvc/v1.0/authz", ep.teams);
    let resp = http
        .post(&url)
        .header(header::USER_AGENT, USER_AGENT)
        .header(
            header::AUTHORIZATION,
            format!("Bearer {}", access_token.expose()),
        )
        .header(header::ACCEPT, "application/json; ver=1.0")
        // hyper sends no Content-Length for a body-less POST; front ends
        // may answer 411 to that, so say "empty" explicitly.
        .header(header::CONTENT_LENGTH, "0")
        .send()
        .await?;
    let status = resp.status().as_u16();
    let body = resp.text().await?;
    if status != 200 {
        return Err(Error::Http { status, body });
    }
    let w: AuthzWire =
        serde_json::from_str(&body).map_err(|e| Error::Protocol(format!("authz response: {e}")))?;
    let Some((skype_token, expires_in)) = token_in(w.tokens.as_ref(), "skypeToken")
        .or_else(|| token_in(w.legacy.as_ref(), "skypetoken"))
    else {
        return Err(Error::Protocol("authz: no skypeToken".into()));
    };
    let (chat_service, ams_service) = match w.region_gtms {
        Some(g) => (g.chat_service, g.ams),
        None => (None, None),
    };
    let chat_service = match chat_service {
        Some(url) => url.trim_end_matches('/').to_string(),
        None => match &w.region {
            Some(region) => format!("https://{region}.ng.msg.teams.microsoft.com"),
            None => {
                return Err(Error::Protocol(
                    "authz: no chatService and no region".into(),
                ));
            }
        },
    };
    Ok(AuthzResponse {
        skype_token,
        expires_in: lenient_u64(&expires_in, DEFAULT_LIFETIME_SECS),
        region: w.region,
        chat_service,
        ams_service: ams_service
            .filter(|u| !u.is_empty())
            .map(|u| u.trim_end_matches('/').to_string()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{
        HeaderExactMatcher, body_string_contains, header, headers, method, path,
    };
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Matches our exact User-Agent. wiremock's exact matcher splits header
    /// values on commas, and ours contains "(KHTML, like Gecko)".
    fn user_agent() -> HeaderExactMatcher {
        headers("User-Agent", USER_AGENT.split(',').map(str::trim).collect())
    }

    async fn server() -> (MockServer, Endpoints, reqwest::Client) {
        let s = MockServer::start().await;
        let ep = Endpoints::all(&s.uri());
        (s, ep, reqwest::Client::new())
    }

    #[test]
    fn lenient_u64_accepts_numbers_and_numeric_strings() {
        use serde_json::json;
        assert_eq!(lenient_u64(&json!(5), 9), 5);
        assert_eq!(lenient_u64(&json!("5"), 9), 5);
        assert_eq!(lenient_u64(&json!(" 900 "), 9), 900);
        assert_eq!(lenient_u64(&json!(null), 9), 9);
        assert_eq!(lenient_u64(&json!("soon"), 9), 9);
        assert_eq!(lenient_u64(&json!(-1), 9), 9);
        assert_eq!(lenient_u64(&json!(2.5), 9), 9);
    }

    #[tokio::test]
    async fn device_code_accepts_string_numbers_and_either_url_key() {
        let (s, ep, http) = server().await;
        Mock::given(method("POST"))
            .and(path("/acme.io/oauth2/devicecode"))
            .and(body_string_contains(
                "client_id=1fec8e78-bce4-4aaf-ab1b-5451cc387264",
            ))
            .and(body_string_contains(
                "resource=https%3A%2F%2Fapi.spaces.skype.com",
            ))
            .and(user_agent())
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "user_code": "ABCD1234", "device_code": "dc-secret",
                "verification_uri": "https://microsoft.com/devicelogin",
                "interval": "5", "expires_in": "900",
                "message": "To sign in, use a web browser…" })))
            .expect(1)
            .mount(&s)
            .await;
        let start = start_device_code(&http, &ep, "acme.io").await.unwrap();
        assert_eq!(start.prompt.user_code, "ABCD1234");
        assert_eq!(
            start.prompt.verification_url,
            "https://microsoft.com/devicelogin"
        );
        assert_eq!(start.prompt.message, "To sign in, use a web browser…");
        assert_eq!(start.interval, std::time::Duration::from_secs(5));
        assert_eq!(start.device_code.expose(), "dc-secret");
        assert!(start.prompt.expires_at > crate::now_ms() + 800_000);
        assert!(!format!("{start:?}").contains("dc-secret"));
    }

    #[tokio::test]
    async fn device_code_v1_key_numeric_fields_and_default_message() {
        let (s, ep, http) = server().await;
        Mock::given(method("POST"))
            .and(path("/organizations/oauth2/devicecode"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "user_code": "WXYZ", "device_code": "dc",
                "verification_url": "https://microsoft.com/devicelogin",
                "interval": 7, "expires_in": 600 })))
            .mount(&s)
            .await;
        let start = start_device_code(&http, &ep, "organizations")
            .await
            .unwrap();
        assert_eq!(
            start.prompt.verification_url,
            "https://microsoft.com/devicelogin"
        );
        assert_eq!(start.interval, std::time::Duration::from_secs(7));
        assert!(start.prompt.expires_at > crate::now_ms() + 500_000);
        assert!(start.prompt.expires_at <= crate::now_ms() + 600_000);
        assert_eq!(
            start.prompt.message,
            "To sign in, use a web browser to open https://microsoft.com/devicelogin and enter the code WXYZ."
        );
    }

    #[tokio::test]
    async fn device_code_defaults_interval_and_expiry_when_absent_or_junk() {
        let (s, ep, http) = server().await;
        Mock::given(method("POST"))
            .and(path("/organizations/oauth2/devicecode"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "user_code": "WXYZ", "device_code": "dc",
                "verification_uri": "https://microsoft.com/devicelogin",
                "interval": "soon" })))
            .mount(&s)
            .await;
        let start = start_device_code(&http, &ep, "organizations")
            .await
            .unwrap();
        assert_eq!(start.interval, std::time::Duration::from_secs(5));
        assert!(start.prompt.expires_at > crate::now_ms() + 800_000);
        assert!(start.prompt.expires_at <= crate::now_ms() + 900_000);
    }

    #[tokio::test]
    async fn device_code_interval_is_floored_at_one_second() {
        let (s, ep, http) = server().await;
        Mock::given(method("POST"))
            .and(path("/organizations/oauth2/devicecode"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "user_code": "WXYZ", "device_code": "dc",
                "verification_uri": "https://microsoft.com/devicelogin",
                "interval": 0 })))
            .mount(&s)
            .await;
        let start = start_device_code(&http, &ep, "organizations")
            .await
            .unwrap();
        assert_eq!(
            start.interval,
            std::time::Duration::from_secs(1),
            "0 would poll in a hot loop"
        );
    }

    #[tokio::test]
    async fn device_code_errors() {
        let (s, ep, http) = server().await;
        let p = || path("/organizations/oauth2/devicecode");
        Mock::given(method("POST"))
            .and(p())
            .respond_with(
                ResponseTemplate::new(400).set_body_string("{\"error\":\"invalid_request\"}"),
            )
            .up_to_n_times(1)
            .mount(&s)
            .await;
        Mock::given(method("POST"))
            .and(p())
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "user_code": "WXYZ", "device_code": "dc" })))
            .up_to_n_times(1)
            .mount(&s)
            .await;
        Mock::given(method("POST"))
            .and(p())
            .respond_with(ResponseTemplate::new(200).set_body_string("not json"))
            .mount(&s)
            .await;
        assert!(matches!(
            start_device_code(&http, &ep, "organizations").await,
            Err(Error::Http { status: 400, ref body }) if body.contains("invalid_request")
        ));
        // No verification URL at all: nothing to show the user.
        assert!(matches!(
            start_device_code(&http, &ep, "organizations").await,
            Err(Error::Protocol(_))
        ));
        assert!(matches!(
            start_device_code(&http, &ep, "organizations").await,
            Err(Error::Protocol(_))
        ));
    }

    #[tokio::test]
    async fn poll_outcomes() {
        let (s, ep, http) = server().await;
        let dc = Secret::new("dc".to_string());
        let tok = || path("/organizations/oauth2/token");
        Mock::given(method("POST"))
            .and(tok())
            .respond_with(
                ResponseTemplate::new(400)
                    .set_body_json(serde_json::json!({"error": "authorization_pending"})),
            )
            .up_to_n_times(1)
            .mount(&s)
            .await;
        Mock::given(method("POST"))
            .and(tok())
            .respond_with(
                ResponseTemplate::new(400).set_body_json(serde_json::json!({"error": "slow_down"})),
            )
            .up_to_n_times(1)
            .mount(&s)
            .await;
        Mock::given(method("POST"))
            .and(tok())
            .and(body_string_contains(
                "grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code",
            ))
            .and(body_string_contains("code=dc"))
            .and(body_string_contains(
                "client_id=1fec8e78-bce4-4aaf-ab1b-5451cc387264",
            ))
            .and(user_agent())
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "at", "refresh_token": "rt", "expires_in": 3600})))
            .up_to_n_times(1)
            .mount(&s)
            .await;
        Mock::given(method("POST"))
            .and(tok())
            .respond_with(
                ResponseTemplate::new(400)
                    .set_body_json(serde_json::json!({"error": "expired_token"})),
            )
            .mount(&s)
            .await;
        assert!(matches!(
            poll_device_code(&http, &ep, "organizations", &dc)
                .await
                .unwrap(),
            PollOutcome::Pending
        ));
        assert!(matches!(
            poll_device_code(&http, &ep, "organizations", &dc)
                .await
                .unwrap(),
            PollOutcome::SlowDown
        ));
        match poll_device_code(&http, &ep, "organizations", &dc)
            .await
            .unwrap()
        {
            PollOutcome::Granted(t) => {
                assert_eq!(t.access_token.expose(), "at");
                assert_eq!(t.refresh_token.unwrap().expose(), "rt");
                assert_eq!(t.expires_in, 3600);
            }
            other => panic!("expected Granted, got {other:?}"),
        }
        assert!(matches!(
            poll_device_code(&http, &ep, "organizations", &dc).await,
            Err(Error::Protocol(_))
        ));
    }

    #[tokio::test]
    async fn poll_reauth_and_non_oauth_errors() {
        let (s, ep, http) = server().await;
        let dc = Secret::new("dc".to_string());
        let tok = || path("/organizations/oauth2/token");
        Mock::given(method("POST"))
            .and(tok())
            .respond_with(
                ResponseTemplate::new(400)
                    .set_body_json(serde_json::json!({"error": "invalid_grant"})),
            )
            .up_to_n_times(1)
            .mount(&s)
            .await;
        Mock::given(method("POST"))
            .and(tok())
            .respond_with(ResponseTemplate::new(400).set_body_json(
                serde_json::json!({"error": "interaction_required", "error_description": "MFA"}),
            ))
            .up_to_n_times(1)
            .mount(&s)
            .await;
        Mock::given(method("POST"))
            .and(tok())
            .respond_with(ResponseTemplate::new(503).set_body_string("<html>gateway</html>"))
            .up_to_n_times(1)
            .mount(&s)
            .await;
        // The v1 token endpoint returns expires_in as a string.
        Mock::given(method("POST"))
            .and(tok())
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "at", "expires_in": "3599"})))
            .mount(&s)
            .await;
        assert!(matches!(
            poll_device_code(&http, &ep, "organizations", &dc).await,
            Err(Error::ReauthRequired)
        ));
        assert!(matches!(
            poll_device_code(&http, &ep, "organizations", &dc).await,
            Err(Error::ReauthRequired)
        ));
        assert!(matches!(
            poll_device_code(&http, &ep, "organizations", &dc).await,
            Err(Error::Http { status: 503, ref body }) if body == "<html>gateway</html>"
        ));
        match poll_device_code(&http, &ep, "organizations", &dc)
            .await
            .unwrap()
        {
            PollOutcome::Granted(t) => {
                assert!(t.refresh_token.is_none());
                assert_eq!(t.expires_in, 3599);
            }
            other => panic!("expected Granted, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn refresh_maps_errors() {
        let (s, ep, http) = server().await;
        let p = || path("/acme.io/oauth2/v2.0/token");
        Mock::given(method("POST"))
            .and(p())
            .and(body_string_contains("grant_type=refresh_token"))
            .and(body_string_contains("refresh_token=rt"))
            .and(body_string_contains(
                "client_id=1fec8e78-bce4-4aaf-ab1b-5451cc387264",
            ))
            .and(body_string_contains(
                "scope=https%3A%2F%2Fapi.spaces.skype.com%2F.default+openid+profile+offline_access",
            ))
            .and(user_agent())
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "at2", "refresh_token": "rt2", "expires_in": 3600})))
            .up_to_n_times(1)
            .mount(&s)
            .await;
        Mock::given(method("POST"))
            .and(p())
            .respond_with(
                ResponseTemplate::new(400)
                    .set_body_json(serde_json::json!({"error": "invalid_grant"})),
            )
            .up_to_n_times(1)
            .mount(&s)
            .await;
        Mock::given(method("POST"))
            .and(p())
            .respond_with(ResponseTemplate::new(400).set_body_json(
                serde_json::json!({"error": "interaction_required", "error_description": "x"}),
            ))
            .up_to_n_times(1)
            .mount(&s)
            .await;
        Mock::given(method("POST"))
            .and(p())
            .respond_with(ResponseTemplate::new(400).set_body_json(
                serde_json::json!({"error": "invalid_client", "error_description": "AADSTS7000"}),
            ))
            .up_to_n_times(1)
            .mount(&s)
            .await;
        Mock::given(method("POST"))
            .and(p())
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&s)
            .await;
        let rt = Secret::new("rt".to_string());
        let t = refresh_access_token(&http, &ep, "acme.io", &rt)
            .await
            .unwrap();
        assert_eq!(t.access_token.expose(), "at2");
        assert_eq!(t.refresh_token.unwrap().expose(), "rt2");
        assert_eq!(t.expires_in, 3600);
        assert!(matches!(
            refresh_access_token(&http, &ep, "acme.io", &rt).await,
            Err(Error::ReauthRequired)
        ));
        assert!(matches!(
            refresh_access_token(&http, &ep, "acme.io", &rt).await,
            Err(Error::ReauthRequired)
        ));
        assert!(matches!(
            refresh_access_token(&http, &ep, "acme.io", &rt).await,
            Err(Error::Http { status: 400, ref body }) if body.contains("AADSTS7000")
        ));
        assert!(matches!(
            refresh_access_token(&http, &ep, "acme.io", &rt).await,
            Err(Error::Http { status: 500, ref body }) if body == "boom"
        ));
    }

    #[tokio::test]
    async fn authz_parses_tokens_region_and_chat_service() {
        let (s, ep, http) = server().await;
        Mock::given(method("POST"))
            .and(path("/api/authsvc/v1.0/authz"))
            .and(header("Authorization", "Bearer at"))
            .and(header("Accept", "application/json; ver=1.0"))
            .and(user_agent())
            .and(header("Content-Length", "0"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "tokens": {"skypeToken": "sk", "expiresIn": 3800},
                "region": "de",
                "regionGtms": {"chatService": "https://de.ng.msg.teams.microsoft.com",
                    "ams": "https://eu-api.asm.skype.com/"}})))
            .mount(&s)
            .await;
        let a = authz(&http, &ep, &Secret::new("at".into())).await.unwrap();
        assert_eq!(a.skype_token.expose(), "sk");
        assert_eq!(a.expires_in, 3800);
        assert_eq!(a.region.as_deref(), Some("de"));
        assert_eq!(a.chat_service, "https://de.ng.msg.teams.microsoft.com");
        assert_eq!(
            a.ams_service.as_deref(),
            Some("https://eu-api.asm.skype.com")
        );
        assert!(!format!("{a:?}").contains("\"sk\""));
        assert!(format!("{a:?}").contains("Secret(<redacted>)"));
    }

    #[tokio::test]
    async fn authz_falls_back_to_region_host_and_legacy_shape() {
        let (s, ep, http) = server().await;
        Mock::given(method("POST"))
            .and(path("/api/authsvc/v1.0/authz"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "skypeToken": {"skypetoken": "sk", "expiresIn": 100}, "region": "apac"})))
            .mount(&s)
            .await;
        let a = authz(&http, &ep, &Secret::new("at".into())).await.unwrap();
        assert_eq!(a.skype_token.expose(), "sk");
        assert_eq!(a.expires_in, 100);
        assert_eq!(a.region.as_deref(), Some("apac"));
        assert_eq!(a.chat_service, "https://apac.ng.msg.teams.microsoft.com");
        assert_eq!(a.ams_service, None);
    }

    #[tokio::test]
    async fn authz_shape_errors_never_echo_a_token() {
        let (s, ep, http) = server().await;
        // The current shape's token where the legacy object is expected,
        // and a string where the `tokens` object is expected.
        for body in [
            serde_json::json!({"skypeToken": "SECRET-SKYPE-TOKEN", "region": "de"}),
            serde_json::json!({"tokens": "SECRET-SKYPE-TOKEN", "region": "de"}),
        ] {
            s.reset().await;
            Mock::given(method("POST"))
                .and(path("/api/authsvc/v1.0/authz"))
                .respond_with(ResponseTemplate::new(200).set_body_json(body))
                .mount(&s)
                .await;
            let err = authz(&http, &ep, &Secret::new("at".to_string()))
                .await
                .unwrap_err();
            assert!(matches!(err, Error::Protocol(_)), "{err:?}");
            assert!(!err.to_string().contains("SECRET"), "{err}");
        }
    }

    #[tokio::test]
    async fn authz_errors() {
        let (s, ep, http) = server().await;
        let p = || path("/api/authsvc/v1.0/authz");
        Mock::given(method("POST"))
            .and(p())
            .respond_with(ResponseTemplate::new(401).set_body_string("nope"))
            .up_to_n_times(1)
            .mount(&s)
            .await;
        // No chatService and no region.
        Mock::given(method("POST"))
            .and(p())
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "tokens": {"skypeToken": "sk", "expiresIn": 3800}})))
            .up_to_n_times(1)
            .mount(&s)
            .await;
        // No Skype token in either shape.
        Mock::given(method("POST"))
            .and(p())
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "region": "de", "regionGtms": {"chatService": "https://x"}})))
            .mount(&s)
            .await;
        let at = Secret::new("at".to_string());
        assert!(matches!(
            authz(&http, &ep, &at).await,
            Err(Error::Http { status: 401, ref body }) if body == "nope"
        ));
        assert!(matches!(
            authz(&http, &ep, &at).await,
            Err(Error::Protocol(ref m)) if m == "authz: no chatService and no region"
        ));
        assert!(matches!(
            authz(&http, &ep, &at).await,
            Err(Error::Protocol(_))
        ));
    }

    #[tokio::test]
    async fn refresh_grant_returns_the_raw_outcome() {
        let (s, ep, http) = server().await;
        let p = || path("/acme.io/oauth2/v2.0/token");
        Mock::given(method("POST"))
            .and(p())
            .and(body_string_contains("grant_type=refresh_token"))
            .and(body_string_contains("refresh_token=rt"))
            .and(body_string_contains(
                "client_id=1fec8e78-bce4-4aaf-ab1b-5451cc387264",
            ))
            .and(body_string_contains(
                "scope=https%3A%2F%2Fchatsvcagg.teams.microsoft.com%2F.default+openid+profile+offline_access",
            ))
            .and(user_agent())
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "csa", "refresh_token": "rt2", "expires_in": 3599})))
            .up_to_n_times(1)
            .mount(&s)
            .await;
        Mock::given(method("POST"))
            .and(p())
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "error": "invalid_grant", "error_description": "AADSTS65001: consent"})))
            .up_to_n_times(1)
            .mount(&s)
            .await;
        Mock::given(method("POST"))
            .and(p())
            .respond_with(ResponseTemplate::new(502).set_body_string("<html>proxy</html>"))
            .up_to_n_times(1)
            .mount(&s)
            .await;
        Mock::given(method("POST"))
            .and(p())
            .respond_with(ResponseTemplate::new(200).set_body_string("<html>login</html>"))
            .mount(&s)
            .await;
        let rt = Secret::new("rt".to_string());
        let scope = crate::auth::CSA_SCOPE;
        match refresh_grant(&http, &ep, "acme.io", &rt, scope)
            .await
            .unwrap()
        {
            GrantOutcome::Granted(t) => {
                assert_eq!(t.access_token.expose(), "csa");
                assert_eq!(t.refresh_token.unwrap().expose(), "rt2");
                assert_eq!(t.expires_in, 3599);
            }
            other => panic!("expected Granted, got {other:?}"),
        }
        match refresh_grant(&http, &ep, "acme.io", &rt, scope)
            .await
            .unwrap()
        {
            GrantOutcome::Refused {
                status,
                error,
                description,
                body,
            } => {
                assert_eq!(status, 400);
                assert_eq!(error.as_deref(), Some("invalid_grant"));
                assert_eq!(description.as_deref(), Some("AADSTS65001: consent"));
                assert!(body.contains("invalid_grant"), "{body}");
            }
            other => panic!("expected Refused, got {other:?}"),
        }
        assert!(matches!(
            refresh_grant(&http, &ep, "acme.io", &rt, scope).await.unwrap(),
            GrantOutcome::Refused { status: 502, error: None, description: None, ref body }
                if body == "<html>proxy</html>"
        ));
        // A 200 that is not a token response: a protocol error, never a token.
        let err = refresh_grant(&http, &ep, "acme.io", &rt, scope)
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Protocol(_)), "{err:?}");
    }
}
