//! Tests for the chatsvc endpoints in [`super`].

use wiremock::matchers::{
    body_json, body_partial_json, body_string_contains, header, method, path, path_regex,
    query_param, query_param_is_missing,
};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::paging::CursorInner;
use super::*;
use crate::Error;
use crate::auth::manager::tests::grants;
use crate::csa::CsaSource;
use crate::format::parse_slack;
use crate::format::{Block, ReplyQuote, RichText, to_teams_html};
use crate::test_support::{
    fixture_with_base, mount_auth, mount_csa, mount_me, test_api, test_http,
};
use crate::types::{
    Attachment, AttachmentKind, ConversationId, Me, Message, MessageId, MessageRef, Mri, Reaction,
    ReplyTarget, SendReceipt,
};

const CONV: &str = "19:groupgroupgroup@thread.v2";
const MESSAGES: &str = "/v1/users/ME/conversations/19:groupgroupgroup@thread.v2/messages";

fn conv() -> ConversationId {
    ConversationId(CONV.into())
}

fn json(text: &str) -> serde_json::Value {
    serde_json::from_str(text).unwrap()
}

#[tokio::test]
async fn conversations_query_and_single_page_by_default() {
    let s = MockServer::start().await;
    mount_auth(&s).await;
    Mock::given(method("GET"))
        .and(path("/v1/users/ME/conversations"))
        .and(query_param("startTime", "0"))
        .and(query_param("pageSize", "100"))
        .and(query_param("view", "msnp24Equivalent"))
        .and(query_param(
            "targetType",
            "Passport|Skype|Lync|Thread|PSTN|Agent",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture_with_base(
            fixture!("9.01-conversations-paging/conversations-nometa.json"),
            &s,
        )))
        .expect(1)
        .mount(&s)
        .await;
    let api = test_api(&s).await;
    let page = api.conversations(None, None).await.unwrap();
    assert_eq!(page.items.len(), 3);
    assert_eq!(page.next, None);
    assert_eq!(page.note, None);
}

#[tokio::test]
async fn conversations_follow_backward_link_until_absent() {
    let s = MockServer::start().await;
    mount_auth(&s).await;
    Mock::given(method("GET"))
        .and(path("/v1/users/ME/conversations"))
        .and(query_param_is_missing("backwardLink"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture_with_base(
            fixture!("9.01-conversations-paging/conversations-page1.json"),
            &s,
        )))
        .expect(1)
        .mount(&s)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/users/ME/conversations"))
        .and(query_param("backwardLink", "1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture_with_base(
            fixture!("9.01-conversations-paging/conversations-page2.json"),
            &s,
        )))
        .expect(1)
        .mount(&s)
        .await;
    // Page 1's 3 items fill a 3-item page, short of totalCount 4.
    let api = Api::new(
        test_http(&s, None).await,
        ApiOptions {
            conversations_page_size: 3,
            ..ApiOptions::default()
        },
    );
    let p1 = api.conversations(Some(0), None).await.unwrap();
    assert!(p1.next.is_some());
    assert_eq!(p1.meta.total_count, Some(4));
    let p2 = api.conversations(Some(0), p1.next).await.unwrap();
    assert_eq!(p2.items.len(), 1);
    assert_eq!(p2.next, None);
}

#[tokio::test]
async fn conversations_since_uses_start_time() {
    let s = MockServer::start().await;
    mount_auth(&s).await;
    Mock::given(method("GET"))
        .and(path("/v1/users/ME/conversations"))
        .and(query_param("startTime", "1760000000000"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"conversations": []})),
        )
        .expect(1)
        .mount(&s)
        .await;
    let page = test_api(&s)
        .await
        .conversations(Some(1_760_000_000_000), None)
        .await
        .unwrap();
    assert!(page.items.is_empty());
}

#[tokio::test]
async fn messages_latest_page_is_exactly_the_spike_query() {
    let s = MockServer::start().await;
    mount_auth(&s).await;
    Mock::given(method("GET"))
        .and(path(MESSAGES))
        .and(query_param("pageSize", "5"))
        .and(query_param("view", "msnp24Equivalent"))
        .and(query_param_is_missing("startTime"))
        .and(query_param_is_missing("endTime"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture_with_base(
            fixture!("9.02-message-paging/messages-page1.json"),
            &s,
        )))
        .expect(1)
        .mount(&s)
        .await;
    let api = test_api(&s).await;
    let p = api.messages(&conv(), None, 5).await.unwrap();
    assert_eq!(p.items.len(), 5);
    assert_eq!(p.items[0].id.0, "1760346000000");
    assert_eq!(
        p.next,
        Some(PageCursor(CursorInner::EndTime(1_760_345_760_000 - 1)))
    );
    // exactly pageSize and view, nothing else
    let reqs = s.received_requests().await.unwrap();
    let req = reqs.iter().find(|r| r.url.path() == MESSAGES).unwrap();
    assert_eq!(req.url.query(), Some("pageSize=5&view=msnp24Equivalent"));
}

#[tokio::test]
async fn messages_older_page_uses_window_and_stops_on_short_page() {
    let s = MockServer::start().await;
    mount_auth(&s).await;
    Mock::given(method("GET"))
        .and(path(MESSAGES))
        .and(query_param("startTime", "1"))
        .and(query_param("endTime", "1760345759999"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture_with_base(
            fixture!("9.02-message-paging/messages-page2.json"),
            &s,
        )))
        .expect(1)
        .mount(&s)
        .await;
    Mock::given(method("GET"))
        .and(path(MESSAGES))
        .and(query_param("endTime", "1760345459999"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture_with_base(
            fixture!("9.02-message-paging/messages-short.json"),
            &s,
        )))
        .expect(1)
        .mount(&s)
        .await;
    let api = test_api(&s).await;
    let p2 = api
        .messages(
            &conv(),
            Some(PageCursor(CursorInner::EndTime(1_760_345_759_999))),
            5,
        )
        .await
        .unwrap();
    assert_eq!(p2.items.len(), 5);
    let c3 = p2.next.clone().unwrap();
    let p3 = api.messages(&conv(), Some(c3), 5).await.unwrap();
    assert_eq!(p3.items.len(), 2);
    assert_eq!(p3.next, None);
}

/// The paging guards see what the server sent: an item that does not
/// parse still counts towards a full page, and an edit's window position
/// is its arrival `id` whatever its `skypeeditedid`.
#[tokio::test]
async fn messages_paging_uses_raw_arrival_ids() {
    let s = MockServer::start().await;
    mount_auth(&s).await;
    let mut page = fixture_with_base(fixture!("9.02-message-paging/messages-page1.json"), &s);
    let msgs = page["messages"].as_array_mut().unwrap();
    msgs[1].as_object_mut().unwrap().remove("from");
    msgs[4]["skypeeditedid"] = "1760000000000".into();
    Mock::given(method("GET"))
        .and(path(MESSAGES))
        .respond_with(ResponseTemplate::new(200).set_body_json(page))
        .mount(&s)
        .await;
    let p = test_api(&s).await.messages(&conv(), None, 5).await.unwrap();
    assert_eq!(p.items.len(), 4);
    assert_eq!(
        p.next,
        Some(PageCursor(CursorInner::EndTime(1_760_345_760_000 - 1)))
    );
}

#[tokio::test]
async fn messages_window_not_moving_stops_with_note() {
    let s = MockServer::start().await;
    mount_auth(&s).await;
    Mock::given(method("GET"))
        .and(path(MESSAGES))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture_with_base(
            fixture!("9.02-message-paging/messages-page1.json"),
            &s,
        )))
        .mount(&s)
        .await;
    let api = test_api(&s).await;
    let p = api
        .messages(
            &conv(),
            Some(PageCursor(CursorInner::EndTime(1_760_345_759_999))),
            5,
        )
        .await
        .unwrap();
    assert_eq!(p.next, None);
    assert!(p.note.unwrap().contains("window"));
}

#[tokio::test]
async fn messages_backward_link_mode_and_repeat_guard() {
    let s = MockServer::start().await;
    mount_auth(&s).await;
    Mock::given(method("GET"))
        .and(path(MESSAGES))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture_with_base(
            fixture!("9.02-message-paging/messages-backwardlink.json"),
            &s,
        )))
        .mount(&s)
        .await;
    let api = test_api(&s).await;
    let p1 = api.messages(&conv(), None, 5).await.unwrap();
    assert!(matches!(
        p1.next,
        Some(PageCursor(CursorInner::Link { pages: 1, .. }))
    ));
    let p2 = api.messages(&conv(), p1.next, 5).await.unwrap();
    assert_eq!(p2.next, None);
    assert!(p2.note.unwrap().contains("repeated"));
    // the link was requested verbatim
    let reqs = s.received_requests().await.unwrap();
    assert!(
        reqs.iter()
            .any(|r| r.url.query() == Some("pageSize=5&view=msnp24Equivalent&backwardLink=1"))
    );
}

#[tokio::test]
async fn messages_since_pages_until_exhausted() {
    // messages_since pages with ApiOptions::messages_page_size; the
    // fixtures hold 5 per page, so use 5 here.
    let s = MockServer::start().await;
    mount_auth(&s).await;
    Mock::given(method("GET"))
        .and(path(MESSAGES))
        .and(query_param("startTime", "1760345000000"))
        .and(query_param("pageSize", "5"))
        .and(query_param_is_missing("endTime"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture_with_base(
            fixture!("9.02-message-paging/messages-page1.json"),
            &s,
        )))
        .expect(1)
        .mount(&s)
        .await;
    Mock::given(method("GET"))
        .and(path(MESSAGES))
        .and(query_param("startTime", "1760345000000"))
        .and(query_param("endTime", "1760345759999"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture_with_base(
            fixture!("9.02-message-paging/messages-page2.json"),
            &s,
        )))
        .expect(1)
        .mount(&s)
        .await;
    Mock::given(method("GET"))
        .and(path(MESSAGES))
        .and(query_param("startTime", "1760345000000"))
        .and(query_param("endTime", "1760345459999"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture_with_base(
            fixture!("9.02-message-paging/messages-short.json"),
            &s,
        )))
        .expect(1)
        .mount(&s)
        .await;
    let api = Api::new(
        test_http(&s, None).await,
        ApiOptions {
            messages_page_size: 5,
            ..ApiOptions::default()
        },
    );
    let ms = api
        .messages_since(&conv(), 1_760_345_000_000)
        .await
        .unwrap();
    assert_eq!(ms.len(), 12);
    assert_eq!(ms[0].id.0, "1760346000000");
    assert_eq!(ms[11].id.0, "1760345340000");
}

#[tokio::test]
async fn me_is_cached_and_thread_parses() {
    let s = MockServer::start().await;
    mount_auth(&s).await;
    mount_me(&s).await;
    Mock::given(method("GET"))
        .and(path("/v1/threads/19:groupgroupgroup@thread.v2"))
        .and(query_param("view", "msnp24Equivalent"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json(fixture!("9.09-names/thread.json"))),
        )
        .expect(1)
        .mount(&s)
        .await;
    let api = test_api(&s).await;
    assert_eq!(api.me().await.unwrap().display_name, "Ada Example");
    assert_eq!(api.me().await.unwrap().display_name, "Ada Example");
    let me_calls = s
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path() == "/v1/users/ME/properties")
        .count();
    assert_eq!(me_calls, 1, "me() is cached after the first success");
    assert_eq!(
        profile_calls(&s).await,
        0,
        "a real name in /properties needs no profile lookup"
    );
    assert_eq!(api.thread(&conv()).await.unwrap().members.len(), 3);
}

/// `/v1/users/ME/properties` with a skypeName and nothing else (seen live).
async fn mount_bare_me(s: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/v1/users/ME/properties"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"skypeName": ADA.strip_prefix("8:").unwrap()})),
        )
        .mount(s)
        .await;
}

async fn profile_calls(s: &MockServer) -> usize {
    s.received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path().ends_with("/fetchShortProfile"))
        .count()
}

#[tokio::test]
async fn me_without_a_real_name_looks_up_its_own_profile_once() {
    let s = MockServer::start().await;
    mount_auth(&s).await;
    mount_bare_me(&s).await;
    Mock::given(method("POST"))
        .and(path("/api/mt/apac/beta/users/fetchShortProfile"))
        .and(body_partial_json(serde_json::json!(["", ADA])))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            serde_json::json!({"value": [{"mri": ADA, "displayName": "Ada Lookup"}]}),
        ))
        .mount(&s)
        .await;
    let api = test_api(&s).await;
    assert_eq!(api.me().await.unwrap().display_name, "Ada Lookup");
    assert_eq!(api.me().await.unwrap().display_name, "Ada Lookup");
    assert_eq!(profile_calls(&s).await, 1, "the looked-up name is cached");
}

/// Review Focus: a failed (or empty) lookup keeps the fallback, and the
/// placeholder is never sent as `imdisplayname`.
#[tokio::test]
async fn me_keeps_the_fallback_when_the_lookup_fails_and_send_omits_it() {
    let s = MockServer::start().await;
    mount_auth(&s).await;
    mount_bare_me(&s).await;
    // No fetchShortProfile mock: every segment answers 404.
    Mock::given(method("POST"))
        .and(path("/v1/users/ME/conversations/48:notes/messages"))
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({})))
        .mount(&s)
        .await;
    let api = test_api(&s).await;
    let me = api.me().await.unwrap();
    assert_eq!(me.display_name, ADA.strip_prefix("8:").unwrap());
    let tried = profile_calls(&s).await;
    assert!(tried >= 1, "the lookup was tried");
    api.me().await.unwrap();
    assert_eq!(profile_calls(&s).await, tried, "the fallback is cached");
    api.send_with_id(
        &ConversationId("48:notes".into()),
        &parse_slack("hi"),
        None,
        "17603460000001234".into(),
    )
    .await
    .unwrap();
    let reqs = s.received_requests().await.unwrap();
    let sent: serde_json::Value = reqs
        .iter()
        .find(|r| r.url.path().ends_with("/messages"))
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .unwrap();
    assert!(sent.get("imdisplayname").is_none(), "{sent}");
}

#[test]
fn only_network_and_session_failures_of_the_lookup_fail_me() {
    assert!(lookup_failure_is_transient(&Error::Network("x".into())));
    assert!(lookup_failure_is_transient(&Error::ReauthRequired));
    assert!(!lookup_failure_is_transient(&Error::Http {
        status: 404,
        body: String::new(),
    }));
    assert!(!lookup_failure_is_transient(&Error::Protocol("x".into())));
}

/// An email (`upn`) is a fallback too: the profile name replaces it, and
/// it is still sent (it is not the placeholder) when the lookup has none.
#[tokio::test]
async fn an_email_fallback_is_replaced_by_the_profile_name() {
    let s = MockServer::start().await;
    mount_auth(&s).await;
    Mock::given(method("GET"))
        .and(path("/v1/users/ME/properties"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "skypeName": ADA.strip_prefix("8:").unwrap(),
            "userDetails": "{\"upn\":\"ada@acme.io\"}",
        })))
        .mount(&s)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/mt/apac/beta/users/fetchShortProfile"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!([{"mri": ADA, "displayName": "Ada Lookup"}])),
        )
        .mount(&s)
        .await;
    let me = test_api(&s).await.me().await.unwrap();
    assert_eq!(me.display_name, "Ada Lookup");
    assert_eq!(me.upn.as_deref(), Some("ada@acme.io"));
    let email = Me {
        display_name: "ada@acme.io".into(),
        ..me
    };
    assert_eq!(email.sendable_name(), Some("ada@acme.io"));
}

#[tokio::test]
async fn me_keeps_the_fallback_when_the_lookup_has_no_name() {
    let s = MockServer::start().await;
    mount_auth(&s).await;
    mount_bare_me(&s).await;
    Mock::given(method("POST"))
        .and(path("/api/mt/apac/beta/users/fetchShortProfile"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"value": []})))
        .mount(&s)
        .await;
    let api = test_api(&s).await;
    assert_eq!(
        api.me().await.unwrap().display_name,
        ADA.strip_prefix("8:").unwrap()
    );
}

#[tokio::test]
async fn user_names_tries_apac_then_region_and_chunks() {
    let s = MockServer::start().await;
    mount_auth(&s).await;
    let cy = Mri("8:orgid:cccccccc-3333-3333-3333-333333333333".into());
    Mock::given(method("POST"))
        .and(path("/api/mt/apac/beta/users/fetchShortProfile"))
        .and(query_param("isMailAddress", "false"))
        .and(query_param("includeBots", "true"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&s)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/mt/de/beta/users/fetchShortProfile"))
        .and(body_partial_json(serde_json::json!(["", cy.0])))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json(fixture!("9.09-names/fetch-short-profile.json"))),
        )
        .mount(&s)
        .await;
    // The chunk bodies below do not match the one above.
    Mock::given(method("POST"))
        .and(path("/api/mt/de/beta/users/fetchShortProfile"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .mount(&s)
        .await;
    let api = test_api(&s).await;
    let names = api.user_names(std::slice::from_ref(&cy)).await.unwrap();
    assert_eq!(names.get(&cy).map(String::as_str), Some("Cy Example"));

    // chunking: 901 MRIs → two POSTs to the region segment
    let many: Vec<Mri> = (0..901)
        .map(|i| Mri(format!("8:orgid:{i:08}-0000-0000-0000-000000000000")))
        .collect();
    let before = s.received_requests().await.unwrap().len();
    api.user_names(&many).await.unwrap();
    let reqs = s.received_requests().await.unwrap();
    let bodies: Vec<serde_json::Value> = reqs[before..]
        .iter()
        .filter(|r| r.url.path() == "/api/mt/de/beta/users/fetchShortProfile")
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect();
    let sizes: Vec<usize> = bodies.iter().map(|b| b.as_array().unwrap().len()).collect();
    assert_eq!(
        sizes,
        vec![901, 2],
        "900 MRIs + the leading \"\", then 1 + \"\""
    );
    assert!(bodies.iter().all(|b| b[0] == ""));
}

#[tokio::test]
async fn user_names_every_segment_failing_is_http_error() {
    let s = MockServer::start().await;
    mount_auth(&s).await;
    Mock::given(method("POST"))
        .and(path("/api/mt/apac/beta/users/fetchShortProfile"))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&s)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/mt/de/beta/users/fetchShortProfile"))
        .respond_with(ResponseTemplate::new(403).set_body_string("nope"))
        .expect(1)
        .mount(&s)
        .await;
    let r = test_api(&s)
        .await
        .user_names(&[Mri("8:orgid:x".into())])
        .await;
    assert!(matches!(r, Err(Error::Http { status: 403, .. })), "{r:?}");
}

#[tokio::test]
async fn user_names_of_nothing_sends_nothing() {
    let s = MockServer::start().await;
    mount_auth(&s).await;
    assert!(test_api(&s).await.user_names(&[]).await.unwrap().is_empty());
    assert!(s.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn profile_segments_are_the_option_then_the_region_deduplicated() {
    let s = MockServer::start().await;
    let api = test_api(&s).await;
    assert_eq!(api.profile_segments(Some("de")), vec!["apac", "de"]);
    assert_eq!(api.profile_segments(Some("apac")), vec!["apac"]);
    assert_eq!(api.profile_segments(None), vec!["apac"]);
    let api = Api::new(
        test_http(&s, None).await,
        ApiOptions {
            profile_segment: "emea".into(),
            ..ApiOptions::default()
        },
    );
    assert_eq!(api.profile_segments(Some("de")), vec!["emea", "de"]);
}

#[tokio::test]
async fn non_2xx_is_http_error() {
    let s = MockServer::start().await;
    mount_auth(&s).await;
    Mock::given(method("GET"))
        .and(path("/v1/users/ME/properties"))
        .respond_with(ResponseTemplate::new(404).set_body_string("{\"errorCode\":404}"))
        .mount(&s)
        .await;
    assert!(matches!(
        test_api(&s).await.me().await,
        Err(Error::Http { status: 404, .. })
    ));
}

#[test]
fn default_options() {
    let o = ApiOptions::default();
    assert_eq!(o.paging, PagingMode::Auto);
    assert_eq!(o.profile_segment, "apac");
    assert_eq!(o.conversations_page_size, 100);
    assert_eq!(o.messages_page_size, 50);
    assert_eq!(o.csa_geo, None);
    assert!(o.send_quoted_ids);
    assert!(o.clear_unread_bookmark);
}

// --- writes (Task 13) ---

#[test]
fn client_message_id_shape() {
    let id = new_client_message_id();
    assert!(
        id.len() >= 17 && id.chars().all(|c| c.is_ascii_digit()),
        "{id}"
    );
    assert_ne!(new_client_message_id(), id);
}

#[test]
fn client_message_ids_increase_strictly_in_a_burst() {
    let ids: Vec<u64> = (0..2_000)
        .map(|_| new_client_message_id().parse().unwrap())
        .collect();
    assert!(ids.windows(2).all(|w| w[0] < w[1]));
}

#[test]
fn client_message_id_is_now_ms_then_four_digits() {
    let before = crate::now_ms();
    let id = new_client_message_id();
    let after = crate::now_ms();
    let (ms, rand) = id.split_at(id.len() - 4);
    let ms: i64 = ms.parse().unwrap();
    // `after + 1`: the uniqueness bump can carry into the millisecond part.
    assert!((before..=after + 1).contains(&ms), "{id}");
    assert_eq!(rand.len(), 4);
}

#[tokio::test]
async fn send_posts_html_with_display_name_and_returns_receipt() {
    let s = MockServer::start().await;
    mount_auth(&s).await;
    mount_me(&s).await;
    Mock::given(method("POST"))
        .and(path("/v1/users/ME/conversations/48:notes/messages"))
        .and(header("X-Skypetoken", "sk-1"))
        .and(body_partial_json(serde_json::json!({
            "clientmessageid": "17603460000001234",
            "content": "<p>hi <b>there</b></p>",
            "messagetype": "RichText/Html",
            "contenttype": "text",
            "imdisplayname": "Ada Example"
        })))
        .respond_with(
            ResponseTemplate::new(201).set_body_json(
                serde_json::json!({"OriginalArrivalTime": "2025-10-13T09:00:00.000Z"}),
            ),
        )
        .expect(1)
        .mount(&s)
        .await;
    let api = test_api(&s).await;
    let r = api
        .send_with_id(
            &ConversationId("48:notes".into()),
            &parse_slack("hi *there*"),
            None,
            "17603460000001234".into(),
        )
        .await
        .unwrap();
    assert_eq!(
        r,
        SendReceipt {
            client_message_id: "17603460000001234".into(),
            arrival_time: Some(1_760_346_000_000),
        }
    );
}

#[tokio::test]
async fn send_with_reply_prepends_reply_block() {
    let s = MockServer::start().await;
    mount_auth(&s).await;
    mount_me(&s).await;
    Mock::given(method("POST"))
        .and(path("/v1/users/ME/conversations/48:notes/messages"))
        .and(body_string_contains(
            "itemtype=\\\"http://schema.skype.com/Reply\\\" itemid=\\\"1760345000000\\\"",
        ))
        .and(body_string_contains(
            "<strong itemprop=\\\"mri\\\" itemid=\\\"8:orgid:bbbbbbbb-2222-2222-2222-222222222222\\\">Bea Example</strong>",
        ))
        .and(body_string_contains(
            "<p itemprop=\\\"preview\\\">earlier text</p></blockquote><p>yes</p>",
        ))
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({})))
        .expect(1)
        .mount(&s)
        .await;
    let api = test_api(&s).await;
    let target = bea_reply_target();
    let r = api
        .send(
            &ConversationId("48:notes".into()),
            &parse_slack("yes"),
            Some(&target),
        )
        .await
        .unwrap();
    assert_eq!(r.arrival_time, None);
    // the generated id went out as the clientmessageid
    let reqs = s.received_requests().await.unwrap();
    let post = reqs
        .iter()
        .find(|r| r.url.path() == "/v1/users/ME/conversations/48:notes/messages")
        .unwrap();
    let body: serde_json::Value = serde_json::from_slice(&post.body).unwrap();
    assert_eq!(body["clientmessageid"], r.client_message_id.as_str());
}

#[tokio::test]
async fn send_non_2xx_is_error() {
    let s = MockServer::start().await;
    mount_auth(&s).await;
    mount_me(&s).await;
    Mock::given(method("POST"))
        .and(path("/v1/users/ME/conversations/48:notes/messages"))
        .respond_with(
            ResponseTemplate::new(403).set_body_string("{\"errorCode\":403,\"message\":\"nope\"}"),
        )
        .mount(&s)
        .await;
    let r = test_api(&s)
        .await
        .send(&ConversationId("48:notes".into()), &parse_slack("x"), None)
        .await;
    assert!(matches!(r, Err(Error::Http { status: 403, .. })), "{r:?}");
}

#[tokio::test]
async fn mark_read_puts_consumption_horizon_only_when_bookmark_clearing_is_off() {
    let s = MockServer::start().await;
    mount_auth(&s).await;
    Mock::given(method("PUT"))
        .and(path(
            "/v1/users/ME/conversations/19:groupgroupgroup@thread.v2/properties",
        ))
        .and(query_param("name", "consumptionhorizon"))
        .and(body_string_contains(
            "\"consumptionhorizon\":\"1760346000000;",
        ))
        .and(body_string_contains(";17603459999991234\""))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&s)
        .await;
    let off = Api::new(
        test_http(&s, None).await,
        ApiOptions {
            clear_unread_bookmark: false,
            ..ApiOptions::default()
        },
    );
    off.mark_read(
        &conv(),
        &MessageRef {
            id: MessageId("1760346000000".into()),
            client_message_id: Some("17603459999991234".into()),
        },
    )
    .await
    .unwrap();
    let puts: Vec<_> = s
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.method.as_str() == "PUT")
        .collect();
    assert_eq!(puts.len(), 1, "clear_unread_bookmark = false: one request");
}

/// The horizon PUT (§9.7, unchanged), then the bookmark PUT (§9.7b), both
/// at the same `now`, each under its own capture label.
#[tokio::test]
async fn mark_read_puts_the_horizon_then_clears_the_bookmark() {
    let s = MockServer::start().await;
    mount_auth(&s).await;
    let props = "/v1/users/ME/conversations/19:groupgroupgroup@thread.v2/properties";
    Mock::given(method("PUT"))
        .and(path(props))
        .and(query_param("name", "consumptionhorizon"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&s)
        .await;
    Mock::given(method("PUT"))
        .and(path(props))
        .and(query_param("name", "consumptionHorizonBookmark"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&s)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let capture = Arc::new(crate::capture::Capture::new(dir.path()).unwrap());
    let api = Api::new(test_http(&s, Some(capture)).await, ApiOptions::default());
    let msg = MessageRef {
        id: MessageId("1760346000000".into()),
        client_message_id: Some("17603459999991234".into()),
    };
    api.mark_read(&conv(), &msg).await.unwrap();

    let puts: Vec<_> = s
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.method.as_str() == "PUT")
        .collect();
    assert_eq!(puts.len(), 2);
    assert_eq!(
        puts[0].url.query(),
        Some("name=consumptionhorizon"),
        "the horizon first"
    );
    assert_eq!(puts[1].url.query(), Some("name=consumptionHorizonBookmark"));
    assert_eq!(puts[0].url.path(), props);
    assert_eq!(puts[1].url.path(), props);
    let horizon: serde_json::Value = serde_json::from_slice(&puts[0].body).unwrap();
    let now: Timestamp = horizon["consumptionhorizon"]
        .as_str()
        .unwrap()
        .split(';')
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(horizon, mark_read_body(&msg, now), "§9.7 body unchanged");
    let bookmark: serde_json::Value = serde_json::from_slice(&puts[1].body).unwrap();
    assert_eq!(
        bookmark,
        serde_json::json!({"consumptionHorizonBookmark": format!("0;{now};0")}),
        "the same now"
    );

    let mut files: Vec<String> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains("-http-mark-read"))
        .collect();
    files.sort();
    let labels: Vec<String> = files
        .iter()
        .map(|f| {
            let doc: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(dir.path().join(f)).unwrap())
                    .unwrap();
            doc["label"].as_str().unwrap().to_owned()
        })
        .collect();
    assert_eq!(labels, ["mark_read", "mark_read_bookmark"], "{files:?}");
}

#[test]
fn bookmark_clear_body_is_zero_now_zero() {
    assert_eq!(
        bookmark_clear_body(1_760_346_200_000),
        serde_json::json!({"consumptionHorizonBookmark": "0;1760346200000;0"})
    );
}

#[test]
fn mark_read_body_is_id_now_cmid() {
    let mut msg = MessageRef {
        id: MessageId("1760346000000".into()),
        client_message_id: Some("17603459999991234".into()),
    };
    assert_eq!(
        mark_read_body(&msg, 1_760_346_200_000),
        serde_json::json!({"consumptionhorizon": "1760346000000;1760346200000;17603459999991234"})
    );
    // A system message (`ThreadActivity/*`) has no client message id: Teams
    // repeats the message id; an empty one is refused (400).
    msg.client_message_id = None;
    assert_eq!(
        mark_read_body(&msg, 1_760_346_200_000),
        serde_json::json!({"consumptionhorizon": "1760346000000;1760346200000;1760346000000"})
    );
}

/// A refused horizon PUT is the error, and the bookmark is not touched.
#[tokio::test]
async fn mark_read_non_2xx_is_error() {
    let s = MockServer::start().await;
    mount_auth(&s).await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(400))
        .mount(&s)
        .await;
    let msg = MessageRef {
        id: MessageId("1".into()),
        client_message_id: None,
    };
    let r = test_api(&s).await.mark_read(&conv(), &msg).await;
    assert!(matches!(r, Err(Error::Http { status: 400, .. })), "{r:?}");
    let puts = s
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.method.as_str() == "PUT")
        .count();
    assert_eq!(puts, 1, "no bookmark PUT after a refused horizon");
}

/// The horizon was accepted but the bookmark PUT refused: `mark_read`
/// fails with that error (a retry re-sends the idempotent horizon).
#[tokio::test]
async fn mark_read_bookmark_non_2xx_is_error() {
    let s = MockServer::start().await;
    mount_auth(&s).await;
    Mock::given(method("PUT"))
        .and(query_param("name", "consumptionhorizon"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&s)
        .await;
    Mock::given(method("PUT"))
        .and(query_param("name", "consumptionHorizonBookmark"))
        .respond_with(ResponseTemplate::new(403))
        .expect(1)
        .mount(&s)
        .await;
    let msg = MessageRef {
        id: MessageId("1".into()),
        client_message_id: None,
    };
    let r = test_api(&s).await.mark_read(&conv(), &msg).await;
    assert!(matches!(r, Err(Error::Http { status: 403, .. })), "{r:?}");
}

#[tokio::test]
async fn react_and_unreact_shapes() {
    let s = MockServer::start().await;
    mount_auth(&s).await;
    let p =
        "/v1/users/ME/conversations/19:groupgroupgroup@thread.v2/messages/1760346000000/properties";
    Mock::given(method("PUT"))
        .and(path(p))
        .and(query_param("name", "emotions"))
        .and(body_partial_json(
            serde_json::json!({"emotions": {"key": "like"}}),
        ))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&s)
        .await;
    Mock::given(method("DELETE"))
        .and(path(p))
        .and(query_param("name", "emotions"))
        .and(body_partial_json(
            serde_json::json!({"emotions": {"key": "like"}}),
        ))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&s)
        .await;
    let api = test_api(&s).await;
    let id = MessageId("1760346000000".into());
    api.react(&conv(), &id, &Reaction::Like).await.unwrap();
    api.unreact(&conv(), &id, &Reaction::Like).await.unwrap();
}

#[tokio::test]
async fn react_non_2xx_is_error() {
    let s = MockServer::start().await;
    mount_auth(&s).await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&s)
        .await;
    let r = test_api(&s)
        .await
        .react(
            &conv(),
            &MessageId("1".into()),
            &Reaction::Other("yes".into()),
        )
        .await;
    assert!(matches!(r, Err(Error::Http { status: 404, .. })), "{r:?}");
}

/// The documentation fixtures show exactly what the builders send (the
/// timestamp aside, which is the time of the request).
#[test]
fn request_fixtures_match_the_builders() {
    let doc = json(fixture!("9.06-reactions/react-request.json"));
    let msg = MessageId("1760346000000".into());
    for (key, add) in [("add", true), ("remove", false)] {
        let req = reaction_request(
            "{{BASE}}",
            &ConversationId(CONV.into()),
            &msg,
            &Reaction::Like,
            add,
            1_760_346_100_000,
        );
        assert_eq!(req.method.as_str(), doc[key]["method"], "{key}");
        assert_eq!(req.url, doc[key]["url"], "{key}");
        let crate::http::Body::Json(body) = &req.body else {
            panic!("{key}: not a JSON body");
        };
        assert_eq!(body, &doc[key]["body"], "{key}");
    }

    let doc = json(fixture!("9.07-consumptionhorizon/mark-read-request.json"));
    let msg = MessageRef {
        id: MessageId("1760346000000".into()),
        client_message_id: Some("17603459999991234".into()),
    };
    assert_eq!(doc["method"], "PUT");
    assert_eq!(
        doc["url"],
        "{{BASE}}/v1/users/ME/conversations/19:groupgroupgroup@thread.v2/properties?name=consumptionhorizon"
    );
    assert_eq!(mark_read_body(&msg, 1_760_346_200_000), doc["body"]);
}

/// The event fixtures carry what the writes should cause: a peer's `like`
/// on the message, and the 1:1 chat's read horizon.
#[test]
fn write_event_fixtures_parse() {
    let ev = json(fixture!("9.06-reactions/reaction-event.json"));
    assert_eq!(ev["resourceType"], "MessageUpdate");
    let m = wire::parse_message(&ev["resource"], None).unwrap();
    assert_eq!(m.id.0, "1760346000000");
    assert_eq!(m.reactions.len(), 1);
    assert_eq!(m.reactions[0].reaction, Reaction::Like);
    assert_eq!(
        m.reactions[0].users,
        vec![Mri("8:orgid:bbbbbbbb-2222-2222-2222-222222222222".into())]
    );

    let ev = json(fixture!(
        "9.07-consumptionhorizon/conversation-update-horizon.json"
    ));
    assert_eq!(ev["resourceType"], "ConversationUpdate");
    let c = wire::parse_conversation(&ev["resource"]).unwrap();
    assert_eq!(c.kind, crate::types::ConversationKind::OneToOne);
    let h = c.read_horizon.unwrap();
    assert_eq!(h.message_id.0, "1760346000000");
    assert_eq!(h.at, 1_760_346_200_000);
    assert_eq!(h.client_message_id, None);
}

/// A fixture page as JSON, with `_metadata.backwardLink` set to `link`
/// (resolved against the mock server) or removed.
fn page_with_link(text: &str, s: &MockServer, link: Option<&str>) -> serde_json::Value {
    let mut v = fixture_with_base(text, s);
    v["_metadata"] = match link {
        Some(l) => serde_json::json!({ "backwardLink": format!("{}{l}", s.uri()) }),
        None => serde_json::json!({}),
    };
    v
}

#[tokio::test]
async fn messages_since_stops_at_since_when_the_backward_link_lacks_start_time() {
    // The first page's backwardLink carries no startTime, so the server
    // would keep paging into history: messages_since stops after the first
    // page on which nothing arrived at or after `since` (keeping what it
    // returned: the store dedups).
    let s = MockServer::start().await;
    mount_auth(&s).await;
    Mock::given(method("GET"))
        .and(path(MESSAGES))
        .and(query_param("startTime", "1760345600000"))
        .respond_with(ResponseTemplate::new(200).set_body_json(page_with_link(
            fixture!("9.02-message-paging/messages-page1.json"),
            &s,
            Some(&format!("{MESSAGES}?pageSize=5&backwardLink=1")),
        )))
        .expect(1)
        .mount(&s)
        .await;
    Mock::given(method("GET"))
        .and(path(MESSAGES))
        .and(query_param("backwardLink", "1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(page_with_link(
            fixture!("9.02-message-paging/messages-page2.json"),
            &s,
            Some(&format!("{MESSAGES}?pageSize=5&backwardLink=2")),
        )))
        .expect(1)
        .mount(&s)
        .await;
    // Page 2 straddles `since`, so page 3 is fetched; it is all older.
    Mock::given(method("GET"))
        .and(path(MESSAGES))
        .and(query_param("backwardLink", "2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(page_with_link(
            fixture!("9.02-message-paging/messages-short.json"),
            &s,
            Some(&format!("{MESSAGES}?pageSize=5&backwardLink=3")),
        )))
        .expect(1)
        .mount(&s)
        .await;
    Mock::given(method("GET"))
        .and(path(MESSAGES))
        .and(query_param("backwardLink", "3"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "messages": []
        })))
        .expect(0)
        .mount(&s)
        .await;
    let api = Api::new(
        test_http(&s, None).await,
        ApiOptions {
            messages_page_size: 5,
            ..ApiOptions::default()
        },
    );
    let ms = api
        .messages_since(&conv(), 1_760_345_600_000)
        .await
        .unwrap();
    assert_eq!(ms.len(), 12, "everything returned is kept");
}

/// Answers every messages request with a full page of `n` numeric ids just
/// below the request's `endTime` (or a fixed top), so time-window paging
/// never runs out.
struct EndlessWindow {
    n: i64,
}

impl wiremock::Respond for EndlessWindow {
    fn respond(&self, req: &wiremock::Request) -> ResponseTemplate {
        let top = req
            .url
            .query_pairs()
            .find(|(k, _)| k == "endTime")
            .and_then(|(_, v)| v.parse::<i64>().ok())
            .unwrap_or(1_760_346_000_000);
        let messages: Vec<serde_json::Value> = (0..self.n)
            .map(|i| {
                let id = (top - i).to_string();
                serde_json::json!({
                    "id": id, "type": "Message", "version": id,
                    "from": "https://x/v1/users/ME/contacts/8:orgid:b",
                    "messagetype": "Text", "content": "x",
                    "originalarrivaltime": "2025-10-13T09:00:00.000Z",
                })
            })
            .collect();
        ResponseTemplate::new(200).set_body_json(serde_json::json!({ "messages": messages }))
    }
}

#[tokio::test]
async fn messages_since_gives_up_after_max_since_pages() {
    let s = MockServer::start().await;
    mount_auth(&s).await;
    Mock::given(method("GET"))
        .and(path(MESSAGES))
        .respond_with(EndlessWindow { n: 2 })
        .mount(&s)
        .await;
    let api = Api::new(
        test_http(&s, None).await,
        ApiOptions {
            paging: PagingMode::Window,
            messages_page_size: 2,
            ..ApiOptions::default()
        },
    );
    let ms = api.messages_since(&conv(), 1).await.unwrap();
    assert_eq!(ms.len(), 2 * MAX_SINCE_PAGES);
    let pages = s
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path() == MESSAGES)
        .count();
    assert_eq!(pages, MAX_SINCE_PAGES);
}

#[tokio::test]
async fn link_cursors_only_go_to_the_chat_service() {
    let s = MockServer::start().await;
    mount_auth(&s).await;
    let other = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "messages": [], "conversations": []
        })))
        .mount(&other)
        .await;
    let api = test_api(&s).await;
    // An absolute link to another origin is refused before any request.
    let foreign = PageCursor::link(&format!("{}{MESSAGES}?backwardLink=1", other.uri()));
    let r = api.messages(&conv(), Some(foreign.clone()), 5).await;
    assert!(matches!(r, Err(Error::Protocol(_))), "{r:?}");
    let r = api.conversations(None, Some(foreign)).await;
    assert!(matches!(r, Err(Error::Protocol(_))), "{r:?}");
    assert!(
        other.received_requests().await.unwrap().is_empty(),
        "no token ever reached another host"
    );
    // A relative link resolves against the chat service.
    Mock::given(method("GET"))
        .and(path(MESSAGES))
        .and(query_param("backwardLink", "1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "messages": []
        })))
        .expect(1)
        .mount(&s)
        .await;
    let rel = PageCursor::link(&format!("{MESSAGES}?backwardLink=1"));
    let p = api.messages(&conv(), Some(rel), 5).await.unwrap();
    assert!(p.items.is_empty());
}

#[tokio::test]
async fn ids_are_percent_encoded_as_path_segments() {
    let s = MockServer::start().await;
    mount_auth(&s).await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "messages": []
        })))
        .mount(&s)
        .await;
    let api = test_api(&s).await;
    let odd = ConversationId("19:a/b?c#d e%@thread.v2".into());
    api.messages(&odd, None, 5).await.unwrap();
    let reqs = s.received_requests().await.unwrap();
    let last = reqs.last().unwrap();
    assert_eq!(
        last.url.path(),
        "/v1/users/ME/conversations/19:a%2Fb%3Fc%23d%20e%25@thread.v2/messages",
        "':' and '@' stay verbatim (the spike's ids), the rest is encoded"
    );
    assert_eq!(
        last.url.query(),
        Some("pageSize=5&view=msnp24Equivalent"),
        "the id cannot add to the query"
    );
    let r = reaction_request(
        "https://h",
        &ConversationId("..".into()),
        &MessageId("1/2".into()),
        &Reaction::Like,
        true,
        0,
    );
    assert!(
        r.url
            .starts_with("https://h/v1/users/ME/conversations/%2E%2E/messages/1%2F2/properties"),
        "{}",
        r.url
    );
}

/// A minimal message payload with raw arrival id `id`; `edits` makes it an
/// edit of that (older) message.
fn raw_message(id: i64, edits: Option<i64>) -> serde_json::Value {
    let mut m = serde_json::json!({
        "id": id.to_string(), "type": "Message", "version": id.to_string(),
        "from": "https://x/v1/users/ME/contacts/8:orgid:b",
        "messagetype": "Text", "content": format!("m{id}"),
        "originalarrivaltime": "2025-10-13T09:00:00.000Z",
    });
    if let Some(orig) = edits {
        m["skypeeditedid"] = serde_json::json!(orig.to_string());
        m["properties"] = serde_json::json!({"edittime": id.to_string()});
    }
    m
}

#[tokio::test]
async fn messages_since_keeps_paging_past_an_edit_of_an_old_message() {
    // Page 1 holds an item with edit markers (`skypeeditedid` naming an
    // old message) that arrived after `since`: paging goes on by its arrival
    // id and it is kept, under that id (decision: an item's id is always
    // `id`; live `skypeeditedid` is a client message id).
    let s = MockServer::start().await;
    mount_auth(&s).await;
    let since = 1_760_345_600_000_i64;
    Mock::given(method("GET"))
        .and(path(MESSAGES))
        .and(query_param("startTime", since.to_string()))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "messages": [
                raw_message(since + 300_000, Some(1_760_000_000_000)),
                raw_message(since + 200_000, None),
                raw_message(since + 100_000, None),
            ],
            "_metadata": {"backwardLink": format!("{}{MESSAGES}?pageSize=3&backwardLink=1", s.uri())},
        })))
        .expect(1)
        .mount(&s)
        .await;
    Mock::given(method("GET"))
        .and(path(MESSAGES))
        .and(query_param("backwardLink", "1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "messages": [raw_message(since + 50_000, None), raw_message(since + 20_000, None)],
        })))
        .expect(1)
        .mount(&s)
        .await;
    let api = Api::new(
        test_http(&s, None).await,
        ApiOptions {
            messages_page_size: 3,
            ..ApiOptions::default()
        },
    );
    let ms = api.messages_since(&conv(), since).await.unwrap();
    let ids: Vec<&str> = ms.iter().map(|m| m.id.0.as_str()).collect();
    assert_eq!(
        ids,
        [
            "1760345900000",
            "1760345800000",
            "1760345700000",
            "1760345650000",
            "1760345620000"
        ],
        "the edit and both pages"
    );
    assert!(ms[0].edited);
}

/// The followed-link mock path of `conversations-live-shape.json`: another
/// user segment than `ME` (the live server's form).
const LIVE_LINK_PATH: &str = "/v1/users/8:orgid:00000000-0000-4000-8000-000000000001/conversations";

/// The live 2026-10-14 answer to following that link.
fn auth_failed_911() -> ResponseTemplate {
    ResponseTemplate::new(401)
        .set_body_json(serde_json::json!({"errorCode": 911, "message": "Authentication failed."}))
}

/// Live run 2026-10-14 (§9.1): 96 of `totalCount` 96 on a `pageSize=100`
/// page that still carried a `backwardLink` (== `syncState`), which 401s.
/// The short page is the whole listing; the link is not requested.
#[tokio::test]
async fn conversations_short_page_with_backward_link_is_complete() {
    let s = MockServer::start().await;
    mount_auth(&s).await;
    Mock::given(method("GET"))
        .and(path("/v1/users/ME/conversations"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture_with_base(
            fixture!("9.01-conversations-paging/conversations-live-shape.json"),
            &s,
        )))
        .expect(1)
        .mount(&s)
        .await;
    Mock::given(method("GET"))
        .and(path(LIVE_LINK_PATH))
        .respond_with(auth_failed_911())
        .expect(0)
        .mount(&s)
        .await;
    let page = test_api(&s).await.conversations(None, None).await.unwrap();
    assert_eq!(page.items.len(), 3);
    assert_eq!(page.meta.total_count, Some(3));
    assert!(page.meta.backward_link.is_some());
    assert_eq!((page.next, page.note), (None, None));
}

/// A followed conversations page that fails with a 4xx ends paging with a
/// note instead of failing the listing (the pages before it stand).
#[tokio::test]
async fn conversations_followed_page_4xx_ends_paging_with_a_note() {
    let s = MockServer::start().await;
    mount_auth(&s).await;
    let mut first = fixture_with_base(
        fixture!("9.01-conversations-paging/conversations-live-shape.json"),
        &s,
    );
    first["_metadata"]["totalCount"] = 250.into();
    Mock::given(method("GET"))
        .and(path("/v1/users/ME/conversations"))
        .respond_with(ResponseTemplate::new(200).set_body_json(first))
        .expect(1)
        .mount(&s)
        .await;
    Mock::given(method("GET"))
        .and(path(LIVE_LINK_PATH))
        .and(header("x-skypetoken", "sk-1"))
        .respond_with(auth_failed_911())
        .expect(1)
        .mount(&s)
        .await;
    Mock::given(method("GET"))
        .and(path(LIVE_LINK_PATH))
        .respond_with(auth_failed_911())
        .mount(&s)
        .await;
    let api = Api::new(
        test_http(&s, None).await,
        ApiOptions {
            conversations_page_size: 3,
            ..ApiOptions::default()
        },
    );
    let p1 = api.conversations(None, None).await.unwrap();
    assert_eq!(p1.items.len(), 3);
    assert!(
        p1.next.is_some(),
        "a full page short of totalCount follows the link"
    );
    let p2 = api.conversations(None, p1.next).await.unwrap();
    assert!(p2.items.is_empty());
    assert_eq!(p2.next, None);
    let note = p2.note.expect("a noted stop");
    assert!(note.contains("401"), "{note}");
    // A failing first page is still an error.
    let s2 = MockServer::start().await;
    mount_auth(&s2).await;
    Mock::given(method("GET"))
        .and(path("/v1/users/ME/conversations"))
        .respond_with(ResponseTemplate::new(403))
        .mount(&s2)
        .await;
    let r = test_api(&s2).await.conversations(None, None).await;
    assert!(matches!(r, Err(Error::Http { status: 403, .. })), "{r:?}");
}

// --- CSA user state (spec A.3, A.4) ---

fn csa_path(geo: &str) -> String {
    format!("/api/csa/{geo}/api/v3/teams/users/me")
}

fn users_me() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(
        fixture!("9.14-csa-user-state/users-me.json"),
        "application/json",
    )
}

#[tokio::test]
async fn user_state_requests_the_derived_geo_with_the_csa_bearer() {
    let s = MockServer::start().await;
    mount_csa(&s).await;
    mount_auth(&s).await; // authz region "de" → emea
    Mock::given(method("GET"))
        .and(path(csa_path("emea")))
        .and(header("Authorization", "Bearer csa-1"))
        .respond_with(users_me())
        .expect(1)
        .mount(&s)
        .await;
    let (state, report, source) = test_api(&s).await.user_state_from().await.unwrap();
    assert!(report.is_clean(), "{report:?}");
    assert_eq!(
        (state.folders.len(), state.chats.len(), state.teams.len()),
        (4, 3, 1)
    );
    assert_eq!(
        source,
        CsaSource {
            host: s.uri(),
            geo: "emea".into()
        }
    );
    let reqs = s.received_requests().await.unwrap();
    let req = reqs
        .iter()
        .find(|r| r.url.path() == csa_path("emea"))
        .unwrap();
    assert_eq!(
        req.url.query(),
        Some(
            "isPrefetch=false&enableMembershipSummary=true&supportsAdditionalSystemGeneratedFolders=true&supportsSliceItems=true&enableEngageCommunities=false"
        ),
        "the exact query, in order"
    );
    assert!(!req.headers.contains_key("x-skypetoken"));
}

#[tokio::test]
async fn user_state_tries_the_other_geos_on_any_non_2xx() {
    let s = MockServer::start().await;
    mount_csa(&s).await;
    mount_auth(&s).await;
    Mock::given(method("GET"))
        .and(path(csa_path("emea")))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&s)
        .await;
    Mock::given(method("GET"))
        .and(path(csa_path("amer")))
        .respond_with(ResponseTemplate::new(403))
        .expect(1)
        .mount(&s)
        .await;
    Mock::given(method("GET"))
        .and(path(csa_path("apac")))
        .respond_with(users_me())
        .expect(1)
        .mount(&s)
        .await;
    let (state, _, source) = test_api(&s).await.user_state_from().await.unwrap();
    assert_eq!(source.geo, "apac");
    assert_eq!(state.chats.len(), 3);
}

#[tokio::test]
async fn user_state_fails_with_the_last_error_when_no_geo_answers() {
    let s = MockServer::start().await;
    mount_csa(&s).await;
    mount_auth(&s).await;
    Mock::given(method("GET"))
        .and(path_regex("^/api/csa/"))
        .respond_with(ResponseTemplate::new(404).set_body_string("nope"))
        .expect(3)
        .mount(&s)
        .await;
    let r = test_api(&s).await.user_state().await;
    assert!(
        matches!(&r, Err(Error::Http { status: 404, body }) if body == "nope"),
        "{r:?}"
    );
}

#[tokio::test]
async fn user_state_stops_at_a_second_401() {
    let s = MockServer::start().await;
    mount_csa(&s).await;
    mount_auth(&s).await;
    Mock::given(method("GET"))
        .and(path(csa_path("emea")))
        .respond_with(ResponseTemplate::new(401))
        .expect(2)
        .mount(&s)
        .await;
    for geo in ["amer", "apac"] {
        Mock::given(method("GET"))
            .and(path(csa_path(geo)))
            .respond_with(users_me())
            .expect(0)
            .mount(&s)
            .await;
    }
    let r = test_api(&s).await.user_state().await;
    assert!(matches!(r, Err(Error::Http { status: 401, .. })), "{r:?}");
    assert_eq!(
        grants(&s).await.0,
        2,
        "one CSA renewal, then the search ends"
    );
}

#[tokio::test]
async fn user_state_rejects_a_2xx_that_is_not_json() {
    let s = MockServer::start().await;
    mount_csa(&s).await;
    mount_auth(&s).await;
    Mock::given(method("GET"))
        .and(path(csa_path("emea")))
        .respond_with(ResponseTemplate::new(200).set_body_string("<html>"))
        .mount(&s)
        .await;
    let r = test_api(&s).await.user_state().await;
    assert!(matches!(r, Err(Error::Protocol(_))), "{r:?}");
}

#[tokio::test]
async fn csa_geos_are_the_option_or_the_region_then_the_rest() {
    let s = MockServer::start().await;
    let api = test_api(&s).await;
    assert_eq!(api.csa_geos(Some("de")), ["emea", "amer", "apac"]);
    assert_eq!(api.csa_geos(None), ["emea", "amer", "apac"]);
    for r in ["us", "ca", "br", "amer", "US"] {
        assert_eq!(api.csa_geos(Some(r)), ["amer", "emea", "apac"], "{r}");
    }
    for r in ["apac", "au", "jp", "in", "kr", "sg"] {
        assert_eq!(api.csa_geos(Some(r)), ["apac", "emea", "amer"], "{r}");
    }
    assert_eq!(
        api_with_geo(&s, "apac").await.csa_geos(Some("us")),
        ["apac", "emea", "amer"]
    );
    assert_eq!(
        api_with_geo(&s, "xyz").await.csa_geos(Some("de")),
        ["xyz", "emea", "amer", "apac"]
    );
    assert_eq!(
        api_with_geo(&s, "EMEA").await.csa_geos(Some("us")),
        ["emea", "amer", "apac"]
    );
}

/// An `Api` whose `csa_geo` option is `geo`.
async fn api_with_geo(s: &MockServer, geo: &str) -> Api {
    Api::new(
        test_http(s, None).await,
        ApiOptions {
            csa_geo: Some(geo.into()),
            ..ApiOptions::default()
        },
    )
}

/// Bea's message 1760345000000, the target of the reply tests.
fn bea_reply_target() -> ReplyTarget {
    ReplyTarget {
        message: MessageRef {
            id: MessageId("1760345000000".into()),
            client_message_id: None,
        },
        author: Mri("8:orgid:bbbbbbbb-2222-2222-2222-222222222222".into()),
        author_name: "Bea Example".into(),
        excerpt: "earlier text".into(),
        sent_at: 1_760_345_000_000,
    }
}

#[tokio::test]
async fn a_reply_carries_qtd_msgs_unless_switched_off() {
    let s = MockServer::start().await;
    mount_auth(&s).await;
    mount_me(&s).await;
    Mock::given(method("POST"))
        .and(path("/v1/users/ME/conversations/48:notes/messages"))
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({})))
        .mount(&s)
        .await;
    let notes = ConversationId("48:notes".into());
    let target = bea_reply_target();
    let on = test_api(&s).await;
    on.send(&notes, &parse_slack("yes"), Some(&target))
        .await
        .unwrap();
    on.send(&notes, &parse_slack("plain"), None).await.unwrap();
    let off = Api::new(
        test_http(&s, None).await,
        ApiOptions {
            send_quoted_ids: false,
            ..ApiOptions::default()
        },
    );
    off.send(&notes, &parse_slack("yes"), Some(&target))
        .await
        .unwrap();
    let bodies: Vec<serde_json::Value> = s
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| {
            r.method.as_str() == "POST"
                && r.url.path() == "/v1/users/ME/conversations/48:notes/messages"
        })
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect();
    assert_eq!(bodies.len(), 3);
    assert_eq!(
        bodies[0]["properties"],
        serde_json::json!({"qtdMsgs": "[{\"messageId\":\"1760345000000\",\"sender\":\"8:orgid:bbbbbbbb-2222-2222-2222-222222222222\",\"time\":1760345000000}]"}),
        "a JSON string, not an array"
    );
    assert!(
        bodies[0]["content"]
            .as_str()
            .unwrap()
            .contains("schema.skype.com/Reply"),
        "the reply HTML is unchanged"
    );
    assert!(
        bodies[1].get("properties").is_none(),
        "no reply, no properties"
    );
    assert!(
        bodies[2].get("properties").is_none(),
        "send_quoted_ids = false: the §9.8-verified payload"
    );
    assert!(
        bodies[2]["content"]
            .as_str()
            .unwrap()
            .contains("schema.skype.com/Reply")
    );
}

// --- edit and delete (spec C) ---

const ADA: &str = "8:orgid:aaaaaaaa-1111-1111-1111-111111111111";
const MESSAGE_PATH: &str =
    "/v1/users/ME/conversations/19:groupgroupgroup@thread.v2/messages/1760346000000";

fn ada() -> Me {
    Me {
        mri: Mri(ADA.into()),
        display_name: "Ada Example".into(),
        upn: None,
    }
}

/// A message of Ada's (the signed-in user of `mount_me`) in [`CONV`], as
/// `parse_message` reads it, with `patch`'s keys set (null removes a key).
fn own_message(patch: serde_json::Value) -> Message {
    let mut v = serde_json::json!({
        "id": "1760346000000",
        "clientmessageid": "17603459999991234",
        "conversationLink": format!("https://de.ng.msg.teams.microsoft.com/v1/users/ME/conversations/{CONV}"),
        "from": format!("https://de.ng.msg.teams.microsoft.com/v1/users/ME/contacts/{ADA}"),
        "imdisplayname": "Ada Example",
        "messagetype": "RichText/Html",
        "contenttype": "text",
        "content": "<p>hello</p>",
        "composetime": "2025-10-13T08:59:59.9000000Z",
        "originalarrivaltime": "2025-10-13T09:00:00.000Z",
        "version": "1760346000000",
    });
    let obj = v.as_object_mut().unwrap();
    for (k, val) in patch.as_object().unwrap() {
        if val.is_null() {
            obj.remove(k);
        } else {
            obj.insert(k.clone(), val.clone());
        }
    }
    super::wire::parse_message(&v, None).unwrap()
}

#[test]
fn edit_body_rebuilds_the_message() {
    let original = own_message(serde_json::json!({
        "properties": {"formatVariant": "TEAMS", "mentions": "[]"},
    }));
    let body = parse_slack("hello again");
    let v = edit_body(&original, &body, &ada(), 1_760_346_060_000);
    assert_eq!(
        v,
        serde_json::json!({
            "id": "1760346000000",
            "type": "Message",
            "conversationid": CONV,
            "clientmessageid": "17603459999991234",
            "content": to_teams_html(&body),
            "messagetype": "RichText/Html",
            "contenttype": "Text",
            "imdisplayname": "Ada Example",
            "originalarrivaltime": "2025-10-13T09:00:00.000Z",
            "composetime": "2025-10-13T08:59:59.9000000Z",
            "version": "2025-10-13T09:01:00.000Z",
            "properties": {"formatVariant": "TEAMS", "mentions": "[]", "edittime": 1_760_346_060_000_i64},
        })
    );
}

#[test]
fn edit_body_keeps_the_reply_quote() {
    let mut original = own_message(serde_json::json!({}));
    let quote = ReplyQuote {
        message_id: MessageId("1760345000000".into()),
        author: Some(Mri("8:orgid:bbbbbbbb-2222-2222-2222-222222222222".into())),
        author_name: Some("Bea Example".into()),
        excerpt: RichText::text("earlier text"),
    };
    original.reply_to = Some(quote.clone());
    let body = parse_slack("yes, edited");
    let v = edit_body(&original, &body, &ada(), 1_760_346_060_000);
    let mut expected = body.clone();
    expected.0.insert(0, Block::Reply(quote));
    assert_eq!(
        v["content"],
        to_teams_html(&expected),
        "the quote is re-inserted"
    );
}

/// Review Focus #4: odd originals fall back instead of failing.
#[test]
fn edit_body_defaults_without_properties_composetime_or_name() {
    let mut original = own_message(serde_json::json!({
        "composetime": null, "imdisplayname": null, "clientmessageid": null,
    }));
    original
        .extra
        .insert("properties".into(), serde_json::json!("junk"));
    let v = edit_body(&original, &parse_slack("x"), &ada(), 1_760_346_060_000);
    assert_eq!(
        v["composetime"], "2025-10-13T09:00:00.000Z",
        "falls back to originalarrivaltime"
    );
    assert_eq!(
        v["imdisplayname"], "Ada Example",
        "the signed-in user's name"
    );
    assert!(v.get("clientmessageid").is_none());
    assert_eq!(
        v["properties"],
        serde_json::json!({"edittime": 1_760_346_060_000_i64}),
        "a properties value that is not an object is replaced"
    );
    let placeholder = Me {
        display_name: ADA.strip_prefix("8:").unwrap().into(),
        ..ada()
    };
    let v = edit_body(
        &original,
        &parse_slack("x"),
        &placeholder,
        1_760_346_060_000,
    );
    assert!(
        v.get("imdisplayname").is_none(),
        "the placeholder is not sent"
    );
}

/// An original sent with the placeholder name (before the lookup existed)
/// is edited under the resolved name.
#[test]
fn edit_body_replaces_a_placeholder_name_from_the_original() {
    let original = own_message(serde_json::json!({
        "imdisplayname": ADA.strip_prefix("8:").unwrap(),
    }));
    let v = edit_body(&original, &parse_slack("x"), &ada(), 1_760_346_060_000);
    assert_eq!(v["imdisplayname"], "Ada Example");
}

#[tokio::test]
async fn edit_puts_the_message_back_and_delete_soft_deletes() {
    let s = MockServer::start().await;
    mount_auth(&s).await;
    mount_me(&s).await;
    Mock::given(method("PUT"))
        .and(path(MESSAGE_PATH))
        .and(header("X-Skypetoken", "sk-1"))
        .and(body_partial_json(serde_json::json!({
            "id": "1760346000000", "contenttype": "Text", "messagetype": "RichText/Html"
        })))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&s)
        .await;
    Mock::given(method("DELETE"))
        .and(path(MESSAGE_PATH))
        .and(query_param("behavior", "softDelete"))
        .and(header("X-Skypetoken", "sk-1"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&s)
        .await;
    let api = test_api(&s).await;
    let original = own_message(serde_json::json!({}));
    let t0 = crate::now_ms();
    api.edit(&original, &parse_slack("hello again"))
        .await
        .unwrap();
    let t1 = crate::now_ms();
    api.delete(&original).await.unwrap();
    let reqs = s.received_requests().await.unwrap();
    let put = reqs.iter().find(|r| r.method.as_str() == "PUT").unwrap();
    let body: serde_json::Value = serde_json::from_slice(&put.body).unwrap();
    let edittime = body["properties"]["edittime"].as_i64().unwrap();
    assert!((t0..=t1).contains(&edittime), "{edittime}");
    let version = crate::types::parse_iso_ms(body["version"].as_str().unwrap()).unwrap();
    assert_eq!(version, edittime, "one `now` for both");
}

#[tokio::test]
async fn delete_skips_the_type_and_attachment_rules() {
    let s = MockServer::start().await;
    mount_auth(&s).await;
    mount_me(&s).await;
    Mock::given(method("DELETE"))
        .and(path(MESSAGE_PATH))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&s)
        .await;
    let mut card = own_message(serde_json::json!({}));
    card.message_type = "RichText/Media_Card".into();
    card.attachments = vec![Attachment {
        name: "a.pdf".into(),
        url: None,
        kind: AttachmentKind::File,
    }];
    test_api(&s).await.delete(&card).await.unwrap();
}

#[tokio::test]
async fn edit_and_delete_refuse_locally() {
    let s = MockServer::start().await;
    mount_auth(&s).await;
    mount_me(&s).await;
    let api = test_api(&s).await;
    let body = parse_slack("x");
    let mut not_mine = own_message(serde_json::json!({}));
    not_mine.from = Mri("8:orgid:bbbbbbbb-2222-2222-2222-222222222222".into());
    let mut deleted = own_message(serde_json::json!({}));
    deleted.deleted = true;
    let mut pending = own_message(serde_json::json!({}));
    pending.id = MessageId("pending:17603459999991234".into());
    for m in [&not_mine, &deleted, &pending] {
        assert!(
            matches!(api.edit(m, &body).await, Err(Error::Invalid(_))),
            "{m:?}"
        );
        assert!(
            matches!(api.delete(m).await, Err(Error::Invalid(_))),
            "{m:?}"
        );
    }
    let mut card = own_message(serde_json::json!({}));
    card.message_type = "RichText/Media_Card".into();
    let mut with_file = own_message(serde_json::json!({}));
    with_file.attachments = vec![Attachment {
        name: "a.pdf".into(),
        url: None,
        kind: AttachmentKind::File,
    }];
    for m in [&card, &with_file] {
        assert!(
            matches!(api.edit(m, &body).await, Err(Error::Invalid(_))),
            "{m:?}"
        );
    }
    let writes = s
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| matches!(r.method.as_str(), "PUT" | "DELETE"))
        .count();
    assert_eq!(writes, 0, "refused before any request");
}

#[tokio::test]
async fn edit_and_delete_non_2xx_are_errors() {
    let s = MockServer::start().await;
    mount_auth(&s).await;
    mount_me(&s).await;
    Mock::given(path(MESSAGE_PATH))
        .respond_with(ResponseTemplate::new(403).set_body_string("{\"errorCode\":403}"))
        .mount(&s)
        .await;
    let api = test_api(&s).await;
    let m = own_message(serde_json::json!({}));
    assert!(matches!(
        api.edit(&m, &parse_slack("x")).await,
        Err(Error::Http { status: 403, .. })
    ));
    assert!(matches!(
        api.delete(&m).await,
        Err(Error::Http { status: 403, .. })
    ));
}

// --- typing (spec D.2) ---

#[tokio::test]
async fn send_typing_posts_a_control_message() {
    let s = MockServer::start().await;
    mount_auth(&s).await;
    Mock::given(method("POST"))
        .and(path(MESSAGES))
        .and(header("X-Skypetoken", "sk-1"))
        .and(body_json(serde_json::json!({
            "content": "", "messagetype": "Control/Typing", "contenttype": "Application/Message"
        })))
        .respond_with(ResponseTemplate::new(201))
        .expect(1)
        .mount(&s)
        .await;
    test_api(&s).await.send_typing(&conv()).await.unwrap();
    assert_eq!(TYPING_REFRESH, std::time::Duration::from_secs(20));
}

#[tokio::test]
async fn send_typing_non_2xx_is_an_error() {
    let s = MockServer::start().await;
    mount_auth(&s).await;
    Mock::given(method("POST"))
        .and(path(MESSAGES))
        .respond_with(ResponseTemplate::new(403).set_body_string("{\"errorCode\":403}"))
        .mount(&s)
        .await;
    let r = test_api(&s).await.send_typing(&conv()).await;
    assert!(matches!(r, Err(Error::Http { status: 403, .. })), "{r:?}");
}

// --- channel threads (§9.20–§9.22) ---

const CHANNEL: &str = "19:team-general@thread.tacv2";
/// The thread address as Teams sends it: `;` and `=` percent-encoded.
const THREAD_MESSAGES: &str =
    "/v1/users/ME/conversations/19:team-general@thread.tacv2%3Bmessageid%3D1760346000000/messages";

#[tokio::test]
async fn a_thread_reply_posts_to_the_threads_address() {
    let s = MockServer::start().await;
    mount_auth(&s).await;
    mount_me(&s).await;
    Mock::given(method("POST"))
        .and(path(THREAD_MESSAGES))
        .and(body_partial_json(serde_json::json!({
            "content": "<p>in the thread</p>",
            "replyChainId": "1760346000000",
        })))
        .respond_with(ResponseTemplate::new(201))
        .expect(1)
        .mount(&s)
        .await;
    Mock::given(method("POST"))
        .and(path(format!(
            "/v1/users/ME/conversations/{CHANNEL}/messages"
        )))
        .respond_with(ResponseTemplate::new(201))
        .expect(1)
        .mount(&s)
        .await;
    let api = test_api(&s).await;
    let chan = ConversationId(CHANNEL.into());
    let reply = Outgoing {
        body: parse_slack("in the thread"),
        thread_root: Some(MessageId("1760346000000".into())),
        ..Outgoing::default()
    };
    api.send_message(&chan, &reply).await.unwrap();
    // a new post goes to the bare channel
    let post = Outgoing {
        body: parse_slack("a new post"),
        ..Outgoing::default()
    };
    api.send_message(&chan, &post).await.unwrap();
    let reqs = s.received_requests().await.unwrap();
    assert!(
        reqs.iter().any(|r| r
            .url
            .as_str()
            .contains("@thread.tacv2%3Bmessageid%3D1760346000000/messages")),
        "`;` and `=` go out percent-encoded, as Teams sends them"
    );
}

#[tokio::test]
async fn channel_messages_parse_posts_and_replies_into_the_channel() {
    let s = MockServer::start().await;
    mount_auth(&s).await;
    Mock::given(method("GET"))
        .and(path(format!(
            "/v1/users/ME/conversations/{CHANNEL}/messages"
        )))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json(fixture!("9.20-channel-threads/channel-messages.json"))),
        )
        .mount(&s)
        .await;
    let page = test_api(&s)
        .await
        .messages(&ConversationId(CHANNEL.into()), None, 50)
        .await
        .unwrap();
    assert_eq!(page.items.len(), 4);
    assert!(page.items.iter().all(|m| m.conversation.0 == CHANNEL));
    let replies = page
        .items
        .iter()
        .filter(|m| m.thread_root == Some(MessageId("1760346000000".into())))
        .count();
    assert_eq!(replies, 2);
}

#[tokio::test]
async fn a_replys_reactions_edits_and_deletes_go_to_the_channel() {
    let s = MockServer::start().await;
    mount_auth(&s).await;
    mount_me(&s).await;
    let in_channel = format!("/v1/users/ME/conversations/{CHANNEL}/messages/1760346000001");
    Mock::given(method("PUT"))
        .and(path(format!("{in_channel}/properties")))
        .and(query_param("name", "emotions"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&s)
        .await;
    Mock::given(method("DELETE"))
        .and(path(format!("{in_channel}/properties")))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&s)
        .await;
    Mock::given(method("PUT"))
        .and(path(in_channel.as_str()))
        .and(body_partial_json(serde_json::json!({
            "conversationid": CHANNEL,
            "replyChainId": "1760346000000",
        })))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&s)
        .await;
    Mock::given(method("DELETE"))
        .and(path(in_channel.as_str()))
        .and(query_param("behavior", "softDelete"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&s)
        .await;
    let api = test_api(&s).await;
    let chan = ConversationId(CHANNEL.into());
    let root = MessageId("1760346000000".into());
    let id = MessageId("1760346000001".into());
    api.react(&chan, &id, &Reaction::Like).await.unwrap();
    api.unreact(&chan, &id, &Reaction::Like).await.unwrap();
    let reply = own_message(serde_json::json!({
        "id": "1760346000001",
        "conversationLink": format!("https://de.ng.msg.teams.microsoft.com/v1/users/ME/conversations/{CHANNEL};messageid=1760346000000"),
    }));
    assert_eq!(
        (reply.conversation.0.as_str(), reply.thread_root.as_ref()),
        (CHANNEL, Some(&root))
    );
    api.edit(&reply, &parse_slack("edited")).await.unwrap();
    api.delete(&reply).await.unwrap();
}

// --- mentions (§9.23) ---

fn mention(mri: &str, name: &str) -> crate::format::Inline {
    crate::format::Inline::Mention {
        mri: Mri(mri.into()),
        name: name.into(),
    }
}

/// "hi @Ada and @Cy", built with real MRIs as a sender would.
fn body_mentioning_ada_and_cy() -> RichText {
    use crate::format::Inline;
    RichText(vec![Block::Paragraph(vec![
        Inline::Text("hi ".into()),
        mention(ADA, "Ada Example"),
        Inline::Text(" and ".into()),
        mention("8:orgid:cccccccc-3333-3333-3333-333333333333", "Cy Example"),
    ])])
}

#[tokio::test]
async fn mentions_are_numbered_into_properties_mentions() {
    let s = MockServer::start().await;
    mount_auth(&s).await;
    mount_me(&s).await;
    let doc = json(fixture!("9.23-mentions/send-request.json"));
    Mock::given(method("POST"))
        .and(path(MESSAGES))
        .respond_with(ResponseTemplate::new(201))
        .expect(1)
        .mount(&s)
        .await;
    let out = Outgoing {
        body: body_mentioning_ada_and_cy(),
        reply_to: Some(bea_reply_target()),
        ..Outgoing::default()
    };
    test_api(&s)
        .await
        .send_message(&conv(), &out)
        .await
        .unwrap();
    let reqs = s.received_requests().await.unwrap();
    let post = reqs.iter().find(|r| r.url.path() == MESSAGES).unwrap();
    let body: serde_json::Value = serde_json::from_slice(&post.body).unwrap();
    let content = body["content"].as_str().unwrap();
    assert!(
        content.ends_with(doc["content"].as_str().unwrap()),
        "the reply quote first, then the numbered mentions: {content}"
    );
    let sent: serde_json::Value =
        serde_json::from_str(body["properties"]["mentions"].as_str().unwrap()).unwrap();
    let want: serde_json::Value =
        serde_json::from_str(doc["properties"]["mentions"].as_str().unwrap()).unwrap();
    assert_eq!(sent, want);
    assert!(
        body["properties"]["qtdMsgs"].is_string(),
        "other properties stay"
    );
}

#[test]
fn a_message_without_mentions_sends_no_mentions_property() {
    let mut rt = parse_slack("no one here");
    assert_eq!(number_mentions(&mut rt), None);
    assert_eq!(rt, parse_slack("no one here"));
}

#[test]
fn an_edit_with_mentions_renumbers_them_and_one_without_keeps_the_list() {
    let original = own_message(serde_json::json!({
        "properties": {"mentions": "[\"stale\"]"},
    }));
    let v = edit_body(
        &original,
        &body_mentioning_ada_and_cy(),
        &ada(),
        1_760_346_060_000,
    );
    let content = v["content"].as_str().unwrap();
    assert!(
        content.contains("itemid=\"0\">Ada Example</span>"),
        "{content}"
    );
    assert!(
        content.contains("itemid=\"1\">Cy Example</span>"),
        "{content}"
    );
    let list: serde_json::Value =
        serde_json::from_str(v["properties"]["mentions"].as_str().unwrap()).unwrap();
    assert_eq!(
        list[1]["mri"],
        "8:orgid:cccccccc-3333-3333-3333-333333333333"
    );
    assert_eq!(list[1]["itemid"], "1");
    let v = edit_body(&original, &parse_slack("x"), &ada(), 1_760_346_060_000);
    assert_eq!(v["properties"]["mentions"], "[\"stale\"]");
}

/// A received message's mentions (resolved to MRIs) survive an edit that
/// re-sends its body: they are renumbered against the same people.
#[test]
fn received_mentions_round_trip_through_an_edit() {
    let mut v = json(fixture!("9.23-mentions/message-with-mentions.json"));
    v["from"] = format!("https://x/v1/users/ME/contacts/{ADA}").into();
    let m = super::wire::parse_message(&v, None).unwrap();
    let edited = edit_body(&m, &m.body, &ada(), 1_760_346_060_000);
    let list: serde_json::Value =
        serde_json::from_str(edited["properties"]["mentions"].as_str().unwrap()).unwrap();
    let mris: Vec<&str> = list
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["mri"].as_str().unwrap())
        .collect();
    assert_eq!(
        mris,
        [ADA, "8:orgid:cccccccc-3333-3333-3333-333333333333", "7"]
    );
}

// --- hiding (observed 2026-10-22) ---

#[tokio::test]
async fn hide_puts_unpinned_time_then_history_hidden_time() {
    let s = MockServer::start().await;
    mount_auth(&s).await;
    for name in ["unpinnedTime", "historyHiddenTime"] {
        Mock::given(method("PUT"))
            .and(path(format!(
                "/v1/users/ME/conversations/{CONV}/properties"
            )))
            .and(query_param("name", name))
            .and(header("X-Skypetoken", "sk-1"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&s)
            .await;
    }
    test_api(&s).await.hide(&conv()).await.unwrap();
    let reqs = s.received_requests().await.unwrap();
    let bodies: Vec<serde_json::Value> = reqs
        .iter()
        .filter(|r| r.method.as_str() == "PUT")
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect();
    assert!(bodies[0]["unpinnedTime"].is_i64(), "{bodies:?}");
    assert!(bodies[1]["historyHiddenTime"].is_string(), "{bodies:?}");
    assert_eq!(
        bodies[0]["unpinnedTime"].as_i64().unwrap().to_string(),
        bodies[1]["historyHiddenTime"].as_str().unwrap()
    );
}

#[tokio::test]
async fn hide_non_2xx_is_an_error() {
    let s = MockServer::start().await;
    mount_auth(&s).await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&s)
        .await;
    let r = test_api(&s).await.hide(&conv()).await;
    assert!(matches!(r, Err(Error::Http { status: 404, .. })), "{r:?}");
}

// --- hiding channels (observed 2026-10-23; §9.33 for plain standard channels) ---

fn channel(kind: &str, general: bool) -> crate::csa::Channel {
    crate::csa::Channel {
        id: ConversationId("19:c@thread.tacv2".into()),
        name: "c".into(),
        channel_type: Some(kind.into()),
        general,
        favorite: true,
        pinned: false,
        muted: false,
        archived: false,
        interest: None,
        member_role: None,
    }
}

#[test]
fn hiding_a_private_channel_unfavorites_it() {
    let reqs = hide_channel_requests(
        "https://b",
        &channel("1", false),
        &Mri("8:orgid:me".into()),
        0,
    );
    assert_eq!(reqs.len(), 1);
    assert_eq!(
        reqs[0].0,
        "https://b/v1/users/ME/conversations/19:c@thread.tacv2/properties?name=favorite"
    );
    assert_eq!(reqs[0].1, serde_json::json!({"favorite": "false"}));
}

#[test]
fn hiding_a_general_channel_drops_interest_and_the_general_favorite() {
    let reqs = hide_channel_requests(
        "https://b",
        &channel("0", true),
        &Mri("8:orgid:me".into()),
        0,
    );
    assert_eq!(
        reqs[0].0,
        "https://b/v1/threads/19:c@thread.tacv2/members/8:orgid:me"
    );
    assert_eq!(reqs[0].1, serde_json::json!({"interest": "NotInterested"}));
    assert_eq!(
        reqs[1].1,
        serde_json::json!({"isGeneralChannelFavorite": "false"})
    );
    assert_eq!(reqs[2].1, serde_json::json!({"favorite": "false"}));
    let plain = hide_channel_requests(
        "https://b",
        &channel("0", false),
        &Mri("8:orgid:me".into()),
        1_000_000,
    );
    assert_eq!(plain.len(), 2);
    assert_eq!(plain[1].1, serde_json::json!({"favorite": "false"}));
    assert_eq!(
        plain[0].1,
        serde_json::json!({"expiration": 1_000 + 14 * 24 * 3600, "interest": "NotInterested"})
    );
}

#[test]
fn unhiding_channels_mirrors_teams() {
    let me = Mri("8:orgid:me".into());
    let private = unhide_channel_requests("https://b", &channel("1", false), &me);
    assert_eq!(private.len(), 1);
    assert!(
        private[0]
            .0
            .ends_with("properties?name=favorite&requirePermanentJoinedState=true")
    );
    assert_eq!(private[0].1, serde_json::json!({"favorite": "true"}));
    let general = unhide_channel_requests("https://b", &channel("0", true), &me);
    assert_eq!(general[0].1, serde_json::json!({"interest": "Interested"}));
    assert_eq!(
        general[1].1,
        serde_json::json!({"isGeneralChannelFavorite": "true"})
    );
    assert!(general[2].0.ends_with("properties?name=favorite"));
    let standard = unhide_channel_requests("https://b", &channel("0", false), &me);
    assert_eq!(
        standard[0].1,
        serde_json::json!({"expiration": 0, "interest": "Interested"})
    );
    assert!(standard[1].0.ends_with("requirePermanentJoinedState=true"));
}

// --- presence (§9.34) ---

#[test]
fn the_presence_subscription_names_the_push_address() {
    let (url, body) = subscribe_request(
        "https://t/ups/emea",
        "https://x.trouter.teams.microsoft.com:3443/v4/f/abc/",
        "ep-1",
        &[Mri("8:orgid:a".into())],
        &[],
        true,
    );
    assert_eq!(url, "https://t/ups/emea/v1/pubsub/subscriptions/ep-1");
    assert_eq!(
        body["trouterUri"],
        "https://x.trouter.teams.microsoft.com:3443/v4/f/abc/unifiedPresenceService"
    );
    assert_eq!(
        body["subscriptionsToAdd"][0],
        serde_json::json!({"mri": "8:orgid:a", "source": "ups"})
    );
    assert_eq!(body["shouldPurgePreviousSubscriptions"], true);
}

#[tokio::test]
async fn presence_is_fetched_with_the_access_token() {
    let s = MockServer::start().await;
    mount_auth(&s).await;
    Mock::given(method("POST"))
        .and(path_regex(r"^/ups/[a-z]+/v1/presence/getpresence/$"))
        .and(header("Authorization", "Bearer at-1"))
        .and(body_json(serde_json::json!([{"mri": "8:orgid:a"}])))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            {"mri": "8:orgid:a", "presence": {"availability": "Away", "activity": "Away"}, "status": 20000}
        ])))
        .expect(1)
        .mount(&s)
        .await;
    let p = test_api(&s)
        .await
        .presence(&[Mri("8:orgid:a".into())])
        .await
        .unwrap();
    assert_eq!(p[0].availability, "Away");
}
