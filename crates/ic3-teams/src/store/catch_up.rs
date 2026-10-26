//! Catch-up after a reconnect or lost events (spec §5.4), split so the store
//! is never held across a network call (§4.8):
//! [`Store::catch_up_plan`](super::Store::catch_up_plan) (sync) →
//! [`catch_up`] (network only, spawnable) →
//! [`Store::apply_catch_up`](super::Store::apply_catch_up) (sync).

use std::collections::HashSet;

use crate::api::Api;
use crate::types::{Conversation, ConversationId, Message, Timestamp, now_ms};

/// Overlap subtracted from `last_event_time` to absorb clock skew; dedup makes
/// the overlap harmless (spec §5.4).
pub(crate) const CATCH_UP_OVERLAP_MS: Timestamp = 60_000;

/// Size of the latest page re-fetched per loaded conversation (spec §5.4 step 3).
pub(crate) const CATCH_UP_PAGE_SIZE: usize = 50;

/// Most `conversations` pages one catch-up follows.
const MAX_CONVERSATION_PAGES: usize = 50;

/// What to fetch, taken from the store without holding it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatchUpPlan {
    /// `last_event_time` minus the overlap (ms).
    pub since: Timestamp,
    /// Conversations with at least one loaded page, sorted by id.
    pub loaded_conversations: Vec<ConversationId>,
    pub page_size: usize,
}

impl CatchUpPlan {
    /// Widens the plan to also cover everything since `last_event_time` (ms,
    /// minus the overlap): for a consumer that keeps the oldest
    /// `last_event_time` of requests not yet served by a complete run.
    /// Never narrows the plan.
    pub fn cover_from(&mut self, last_event_time: Timestamp) {
        let since = last_event_time.saturating_sub(CATCH_UP_OVERLAP_MS).max(0);
        self.since = self.since.min(since);
    }
}

/// Everything [`catch_up`] fetched, for `Store::apply_catch_up`.
#[derive(Debug, Default)]
pub struct CatchUpResult {
    /// When the network step started: once a complete result is applied,
    /// nothing before it is missing.
    pub started_at: Timestamp,
    pub conversations: Vec<Conversation>,
    pub messages: Vec<Message>,
    /// Failed fetches (`"{what}: {error}"`); never fatal, but any makes the
    /// result incomplete (see [`CatchUpResult::complete`]).
    pub errors: Vec<String>,
    /// Paging guard stops (a loop guard or page cap ended a listing early)
    /// and per-conversation final failures; reported, but not failures.
    pub notes: Vec<String>,
    /// Conversations whose fetch failed for good this session (a 4xx such as
    /// a chat the user was removed from, or a refused off-origin
    /// `backwardLink`): not counted against completeness; a consumer should
    /// leave them out of later plans rather than retry them forever.
    pub gone: Vec<ConversationId>,
}

impl CatchUpResult {
    /// No fetch failed: applying it closes the gap, so `last_event_time`
    /// may advance to `started_at`. An incomplete result is still applied,
    /// but the consumer should catch up again (with backoff).
    pub fn complete(&self) -> bool {
        self.errors.is_empty()
    }
}

/// The network step: changed conversations since `plan.since`, then for each
/// loaded conversation its messages since then (only if it changed, or for
/// every loaded conversation when the changed list is incomplete) and its
/// latest page (to pick up edits, deletes and reactions). Touches no store and
/// never fails: every error is collected into [`CatchUpResult::errors`],
/// except a conversation's final failure, which goes to
/// [`CatchUpResult::gone`] and [`CatchUpResult::notes`].
pub async fn catch_up(api: &Api, plan: CatchUpPlan) -> CatchUpResult {
    let mut r = CatchUpResult {
        started_at: now_ms(),
        ..Default::default()
    };

    // Whether the changed-conversations list is known to be whole.
    let mut listed_all = true;
    let mut cursor = None;
    for _ in 0..MAX_CONVERSATION_PAGES {
        match api.conversations(Some(plan.since), cursor.take()).await {
            Ok(page) => {
                r.conversations.extend(page.items);
                if let Some(note) = page.note {
                    r.notes.push(format!("conversations: {note}"));
                    listed_all = false;
                }
                cursor = page.next;
            }
            Err(e) => {
                r.errors.push(format!("conversations: {e}"));
                listed_all = false;
                break;
            }
        }
        if cursor.is_none() {
            break;
        }
    }
    if cursor.is_some() {
        r.notes.push(format!(
            "conversations: {MAX_CONVERSATION_PAGES}-page cap reached; stopped"
        ));
        listed_all = false;
    }

    let returned: HashSet<ConversationId> = r.conversations.iter().map(|c| c.id.clone()).collect();
    for conv in &plan.loaded_conversations {
        if !listed_all || returned.contains(conv) {
            match api.messages_since_noted(conv, plan.since).await {
                Ok((ms, note)) => {
                    r.messages.extend(ms);
                    if let Some(note) = note {
                        r.notes.push(format!("{}: messages_since: {note}", conv.0));
                    }
                }
                Err(e) => {
                    if r.failed(conv, "messages_since", &e) {
                        continue;
                    }
                }
            }
        }
        match api.messages(conv, None, plan.page_size).await {
            Ok(page) => r.messages.extend(page.items),
            Err(e) => {
                r.failed(conv, "latest page", &e);
            }
        }
    }
    r
}

impl CatchUpResult {
    /// Records `conv`'s failed `what`: final (into `gone` and `notes`;
    /// returns true) or an error to retry.
    fn failed(&mut self, conv: &ConversationId, what: &str, e: &crate::Error) -> bool {
        let text = format!("{}: {what}: {e}", conv.0);
        if crate::api::is_final_for_conversation(e) {
            self.notes.push(text);
            self.gone.push(conv.clone());
            true
        } else {
            self.errors.push(text);
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use wiremock::matchers::{method, path, query_param, query_param_is_missing};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::test_support::{fixture_with_base, mount_auth, test_api};

    #[tokio::test]
    async fn catch_up_fetches_conversations_since_then_messages_for_loaded() {
        let s = MockServer::start().await;
        mount_auth(&s).await;
        Mock::given(method("GET"))
            .and(path("/v1/users/ME/conversations"))
            .and(query_param("startTime", "1760345000000"))
            .respond_with(ResponseTemplate::new(200).set_body_json(fixture_with_base(
                fixture!("9.01-conversations-paging/conversations-nometa.json"),
                &s,
            )))
            .expect(1)
            .mount(&s)
            .await;
        let p = || path("/v1/users/ME/conversations/19:groupgroupgroup@thread.v2/messages");
        Mock::given(method("GET"))
            .and(p())
            .and(query_param("startTime", "1760345000000"))
            .respond_with(ResponseTemplate::new(200).set_body_json(fixture_with_base(
                fixture!("9.02-message-paging/messages-short.json"),
                &s,
            )))
            .expect(1)
            .mount(&s)
            .await;
        Mock::given(method("GET"))
            .and(p())
            .and(query_param("pageSize", "50"))
            .and(query_param_is_missing("startTime"))
            .respond_with(ResponseTemplate::new(200).set_body_json(fixture_with_base(
                fixture!("9.02-message-paging/messages-page1.json"),
                &s,
            )))
            .expect(1)
            .mount(&s)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/users/ME/conversations/48:notes/messages"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&s)
            .await;
        let api = test_api(&s).await;
        let r = catch_up(
            &api,
            CatchUpPlan {
                since: 1_760_345_000_000,
                loaded_conversations: vec![
                    ConversationId("19:groupgroupgroup@thread.v2".into()),
                    ConversationId("48:notes".into()),
                ],
                page_size: 50,
            },
        )
        .await;
        assert_eq!(r.conversations.len(), 3);
        assert_eq!(r.messages.len(), 7, "2 since + 5 latest page");
        assert_eq!(
            r.errors.len(),
            1,
            "48:notes is not in the returned conversations, so only its latest-page fetch runs; \
             its failure is recorded, not fatal"
        );
        assert!(
            r.errors[0].starts_with("48:notes: latest page:"),
            "{:?}",
            r.errors
        );
        assert!(r.started_at > 0);
    }

    #[tokio::test]
    async fn a_failed_conversations_step_fetches_messages_since_for_every_loaded_conversation() {
        // Without the changed-conversations list nothing says which loaded
        // conversation is unchanged: all of them get messages_since, and the
        // run is incomplete (last_event_time must not advance).
        let s = MockServer::start().await;
        mount_auth(&s).await;
        Mock::given(method("GET"))
            .and(path("/v1/users/ME/conversations"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&s)
            .await;
        let p = || path("/v1/users/ME/conversations/19:groupgroupgroup@thread.v2/messages");
        Mock::given(method("GET"))
            .and(p())
            .and(query_param("startTime", "1760345000000"))
            .respond_with(ResponseTemplate::new(200).set_body_json(fixture_with_base(
                fixture!("9.02-message-paging/messages-short.json"),
                &s,
            )))
            .expect(1)
            .mount(&s)
            .await;
        Mock::given(method("GET"))
            .and(p())
            .and(query_param_is_missing("startTime"))
            .respond_with(ResponseTemplate::new(200).set_body_json(fixture_with_base(
                fixture!("9.02-message-paging/messages-page1.json"),
                &s,
            )))
            .expect(1)
            .mount(&s)
            .await;
        let api = test_api(&s).await;
        let r = catch_up(
            &api,
            CatchUpPlan {
                since: 1_760_345_000_000,
                loaded_conversations: vec![ConversationId("19:groupgroupgroup@thread.v2".into())],
                page_size: 50,
            },
        )
        .await;
        assert!(r.conversations.is_empty());
        assert_eq!(r.messages.len(), 7, "2 since + 5 latest page");
        assert_eq!(r.errors.len(), 1, "{:?}", r.errors);
        assert!(r.errors[0].starts_with("conversations:"), "{:?}", r.errors);
        assert!(!r.complete());
    }

    #[tokio::test]
    async fn a_conversations_guard_note_is_noted_not_an_error_and_widens_messages_since() {
        let s = MockServer::start().await;
        mount_auth(&s).await;
        // paging=backward_link with no link: a guard note, not an error.
        Mock::given(method("GET"))
            .and(path("/v1/users/ME/conversations"))
            .respond_with(ResponseTemplate::new(200).set_body_json(fixture_with_base(
                fixture!("9.01-conversations-paging/conversations-nometa.json"),
                &s,
            )))
            .mount(&s)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/users/ME/conversations/48:notes/messages"))
            .and(query_param("startTime", "1760345000000"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "messages": []
            })))
            .expect(1)
            .mount(&s)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/users/ME/conversations/48:notes/messages"))
            .and(query_param_is_missing("startTime"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "messages": []
            })))
            .expect(1)
            .mount(&s)
            .await;
        let api = Api::new(
            crate::test_support::test_http(&s, None).await,
            crate::api::ApiOptions {
                paging: crate::api::PagingMode::BackwardLink,
                ..Default::default()
            },
        );
        let r = catch_up(
            &api,
            CatchUpPlan {
                since: 1_760_345_000_000,
                loaded_conversations: vec![ConversationId("48:notes".into())],
                page_size: 50,
            },
        )
        .await;
        assert!(r.errors.is_empty(), "{:?}", r.errors);
        assert_eq!(r.notes.len(), 1, "{:?}", r.notes);
        assert!(r.notes[0].starts_with("conversations:"), "{:?}", r.notes);
        assert!(r.complete(), "a guard stop is not a failure");
    }

    /// Live run 2026-10-14 (§9.1): a short conversations page with a
    /// `backwardLink` that 401s is the whole listing (the link is never
    /// requested); with a full page short of `totalCount`, the refused link
    /// ends paging with a note. Either way catch-up is complete.
    #[tokio::test]
    async fn conversations_backward_link_401_never_makes_catch_up_incomplete() {
        let link = "/v1/users/8:orgid:00000000-0000-4000-8000-000000000001/conversations";
        for (page_size, link_hits, notes) in [(100, 0, 0), (3, 1, 1)] {
            let s = MockServer::start().await;
            mount_auth(&s).await;
            let mut first = fixture_with_base(
                fixture!("9.01-conversations-paging/conversations-live-shape.json"),
                &s,
            );
            if page_size == 3 {
                first["_metadata"]["totalCount"] = 250.into();
            }
            Mock::given(method("GET"))
                .and(path("/v1/users/ME/conversations"))
                .respond_with(ResponseTemplate::new(200).set_body_json(first))
                .expect(1)
                .mount(&s)
                .await;
            Mock::given(method("GET"))
                .and(path(link))
                .respond_with(ResponseTemplate::new(401).set_body_json(
                    serde_json::json!({"errorCode": 911, "message": "Authentication failed."}),
                ))
                .expect(link_hits..=link_hits * 2)
                .mount(&s)
                .await;
            Mock::given(method("GET"))
                .and(path("/v1/users/ME/conversations/48:notes/messages"))
                .respond_with(
                    ResponseTemplate::new(200).set_body_json(serde_json::json!({"messages": []})),
                )
                .mount(&s)
                .await;
            let api = Api::new(
                crate::test_support::test_http(&s, None).await,
                crate::api::ApiOptions {
                    conversations_page_size: page_size,
                    ..Default::default()
                },
            );
            let r = catch_up(
                &api,
                CatchUpPlan {
                    since: 1_760_345_000_000,
                    loaded_conversations: vec![ConversationId("48:notes".into())],
                    page_size: 50,
                },
            )
            .await;
            assert!(r.errors.is_empty(), "{:?}", r.errors);
            assert!(r.complete());
            assert_eq!(r.conversations.len(), 3);
            assert_eq!(r.notes.len(), notes, "{:?}", r.notes);
        }
    }

    #[test]
    fn cover_from_only_moves_since_back() {
        let mut p = CatchUpPlan {
            since: 1_000_000,
            loaded_conversations: Vec::new(),
            page_size: 50,
        };
        p.cover_from(2_000_000);
        assert_eq!(p.since, 1_000_000, "a later floor never narrows the plan");
        p.cover_from(500_000);
        assert_eq!(p.since, 500_000 - CATCH_UP_OVERLAP_MS);
        p.cover_from(10);
        assert_eq!(p.since, 0, "never negative");
    }

    #[tokio::test]
    async fn a_4xx_or_a_refused_link_on_one_conversation_is_final_not_incomplete() {
        let s = MockServer::start().await;
        mount_auth(&s).await;
        Mock::given(method("GET"))
            .and(path("/v1/users/ME/conversations"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "conversations": [
                    {"id": "19:gone@thread.v2", "version": 2, "threadProperties": {}},
                    {"id": "19:link@thread.v2", "version": 2, "threadProperties": {}},
                    {"id": "19:down@thread.v2", "version": 2, "threadProperties": {}},
                ]
            })))
            .mount(&s)
            .await;
        // Removed from this chat.
        Mock::given(method("GET"))
            .and(path(
                "/v1/users/ME/conversations/19:gone@thread.v2/messages",
            ))
            .respond_with(ResponseTemplate::new(403))
            .mount(&s)
            .await;
        // This chat's backwardLink points at another host.
        Mock::given(method("GET"))
            .and(path("/v1/users/ME/conversations/19:link@thread.v2/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "messages": [{
                    "id": "1760346000000", "type": "Message", "version": "1",
                    "from": "https://x/v1/users/ME/contacts/8:orgid:b",
                    "messagetype": "Text", "content": "x",
                }],
                "_metadata": {"backwardLink": "https://elsewhere.example/v1/users/ME/conversations/19:link@thread.v2/messages?x=1"},
            })))
            .mount(&s)
            .await;
        // A server error stays a failure.
        Mock::given(method("GET"))
            .and(path(
                "/v1/users/ME/conversations/19:down@thread.v2/messages",
            ))
            .respond_with(ResponseTemplate::new(503))
            .mount(&s)
            .await;
        let api = test_api(&s).await;
        let plan = |ids: &[&str]| CatchUpPlan {
            since: 1_760_345_000_000,
            loaded_conversations: ids.iter().map(|i| ConversationId((*i).into())).collect(),
            page_size: 50,
        };
        // All three changed, so each gets messages_since.
        let r = catch_up(&api, plan(&["19:gone@thread.v2", "19:link@thread.v2"])).await;
        assert!(r.errors.is_empty(), "{:?}", r.errors);
        assert!(r.complete());
        assert_eq!(
            r.gone,
            vec![
                ConversationId("19:gone@thread.v2".into()),
                ConversationId("19:link@thread.v2".into())
            ]
        );
        assert!(r.notes.iter().any(|n| n.contains("403")), "{:?}", r.notes);
        let r = catch_up(&api, plan(&["19:down@thread.v2"])).await;
        assert!(!r.complete(), "a 5xx is retried");
        assert!(r.gone.is_empty());
    }

    #[tokio::test]
    async fn a_messages_since_guard_stop_is_noted() {
        let s = MockServer::start().await;
        mount_auth(&s).await;
        Mock::given(method("GET"))
            .and(path("/v1/users/ME/conversations"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&s)
            .await;
        // The window never moves: a guard note on messages_since.
        Mock::given(method("GET"))
            .and(path("/v1/users/ME/conversations/48:notes/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "messages": [{
                    "id": "1760346000000", "type": "Message", "version": "1",
                    "from": "https://x/v1/users/ME/contacts/8:orgid:b",
                    "messagetype": "Text", "content": "x",
                }],
            })))
            .mount(&s)
            .await;
        let api = Api::new(
            crate::test_support::test_http(&s, None).await,
            crate::api::ApiOptions {
                paging: crate::api::PagingMode::Window,
                messages_page_size: 1,
                ..Default::default()
            },
        );
        let r = catch_up(
            &api,
            CatchUpPlan {
                since: 1_760_345_000_000,
                loaded_conversations: vec![ConversationId("48:notes".into())],
                page_size: 50,
            },
        )
        .await;
        assert!(
            r.notes
                .iter()
                .any(|n| n.starts_with("48:notes: messages_since:")),
            "{:?}",
            r.notes
        );
    }
}
