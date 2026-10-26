//! Shared wiremock rig for the crate's unit tests: token mocks, an `Http` and
//! an `Api` pointed at one mock server.

use std::sync::Arc;

use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::Capture;
use crate::api::{Api, ApiOptions};
use crate::auth::manager::tests::{mount_success_mocks, session};
use crate::auth::manager::{ManagerDeps, Tokens};
use crate::auth::{CsaAudience, Endpoints, MemoryTokenStore, TokenStore, USER_AGENT};
use crate::http::Http;

/// Mounts the refresh (`at-{n}` / `rt-{n}`, 3600 s) and `authz` (`sk-{n}`,
/// 3800 s, region `de`, `chatService` = the mock server) responses.
pub(crate) async fn mount_auth(s: &MockServer) {
    mount_success_mocks(s, 3800, 3600).await;
}

/// A `reqwest::Client` with the Teams `User-Agent` but no total timeout and
/// no pool idle timer, so it also works under `start_paused` (the paused
/// runtime auto-advances to the next timer whenever it parks on wiremock
/// I/O, which would fire either of them mid-request).
pub(crate) fn test_client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .pool_idle_timeout(None)
        .build()
        .unwrap()
}

/// CSA grants (`csa-{n}`, 3600 s) for `/api/csa/…` requests. Mount before
/// [`mount_auth`]: both answer the token endpoint, the first mounted match
/// wins.
pub(crate) async fn mount_csa(s: &MockServer) {
    crate::auth::manager::tests::mount_csa_grant(s, 3600).await;
}

/// An `Http` over a store seeded with `rt-0`, every service at the mock
/// server, over [`test_client`].
pub(crate) async fn test_http(s: &MockServer, capture: Option<Arc<Capture>>) -> Arc<Http> {
    let store = Arc::new(MemoryTokenStore::default());
    store.save(&session("rt-0")).await.unwrap();
    let client = test_client();
    let tokens = Tokens::new(ManagerDeps {
        http: client.clone(),
        endpoints: Endpoints::all(&s.uri()),
        store,
        cancel: CancellationToken::new(),
        csa_audience: CsaAudience::default(),
    });
    Arc::new(Http::new(
        client,
        Arc::new(tokens),
        Endpoints::all(&s.uri()),
        capture,
    ))
}

/// An `Api` with default options over [`test_http`].
pub(crate) async fn test_api(s: &MockServer) -> Api {
    Api::new(test_http(s, None).await, ApiOptions::default())
}

/// `GET /v1/users/ME/properties` → fixture `9.09-names/me-properties.json`
/// (Ada Example).
pub(crate) async fn mount_me(s: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/v1/users/ME/properties"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(
                serde_json::from_str::<serde_json::Value>(fixture!(
                    "9.09-names/me-properties.json"
                ))
                .unwrap(),
            ),
        )
        .mount(s)
        .await;
}

/// Parses a fixture after replacing `{{BASE}}` with the mock server's URL.
pub(crate) fn fixture_with_base(text: &str, s: &MockServer) -> serde_json::Value {
    serde_json::from_str(&text.replace("{{BASE}}", &s.uri())).unwrap()
}

/// trouter's HTTP side (Task 15's fixtures): `POST /v4/a` → `v4a.json`
/// with `{{BASE}}` replaced (so `socketio` is the mock server), the
/// socket.io session `GET` → `session.txt` (`sessid-abc123`), and the
/// registrar → 200 `{}`. A test that wants another registrar answer mounts
/// it before calling this (wiremock answers with the first mounted match).
pub(crate) async fn mount_trouter_http(s: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/v4/a"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture_with_base(
            fixture!("9.03-trouter-connect/v4a.json"),
            s,
        )))
        .mount(s)
        .await;
    Mock::given(method("GET"))
        .and(path("/socket.io/1/"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(fixture!("9.03-trouter-connect/session.txt")),
        )
        .mount(s)
        .await;
    Mock::given(method("POST"))
        .and(path("/registrar/prod/V2/registrations"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .mount(s)
        .await;
}

/// Awaits `f` with the paused clock held still, giving up after `limit` of
/// real time (`None`).
///
/// Under `start_paused`, whenever the runtime parks on real I/O (every
/// wiremock round-trip) it auto-advances the clock to the next timer, and
/// keeps doing so until the I/O completes: a background HTTP call can race
/// the clock through pings, silence timeouts and token refreshes. While a
/// blocking task runs, tokio does not auto-advance, so this keeps one
/// running for the duration. Time then moves only through
/// `tokio::time::advance`; anything `f` needs that waits on a timer must be
/// driven by the test.
pub(crate) async fn hold_clock_for<F: std::future::Future>(
    limit: std::time::Duration,
    f: F,
) -> Option<F::Output> {
    use std::sync::atomic::{AtomicBool, Ordering};
    let done = Arc::new(AtomicBool::new(false));
    let flag = done.clone();
    let mut guard = tokio::task::spawn_blocking(move || {
        let start = std::time::Instant::now();
        while !flag.load(Ordering::Acquire) && start.elapsed() < limit {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    });
    let out = tokio::select! {
        biased;
        v = f => Some(v),
        _ = &mut guard => None,
    };
    if out.is_some() {
        done.store(true, Ordering::Release);
        let _ = guard.await;
    }
    out
}

/// [`hold_clock_for`] with a 5 s real-time limit; panics when it passes.
pub(crate) async fn hold_clock<F: std::future::Future>(f: F) -> F::Output {
    hold_clock_for(std::time::Duration::from_secs(5), f)
        .await
        .expect("no progress within 5 s of real time (paused clock held)")
}
