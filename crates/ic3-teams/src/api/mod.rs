//! The chatsvc REST client (spec §4.4.3): the read and write endpoints, paging
//! and wire parsing.
//!
//! Every request goes through the shared `Http` layer, which applies the
//! per-host headers (§4.4.1) and the retry / 401 rules (§4.4.2). Ids go into
//! URLs as path segments (`seg`): verbatim for the usual `19:…@thread.v2`
//! shape, as the spike did, with anything that would leave the segment
//! percent-encoded.

mod files;
mod media;
pub mod paging;
pub(crate) mod wire;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde_json::Value;

pub use paging::{Page, PageCursor, PageMeta, PagingMode};

use crate::csa::{self, CsaSource, ParseReport, UserState};
use crate::format::{Block, ReplyQuote, RichText, to_teams_html, visit_mentions};
use crate::http::{Http, HttpRequest};
use crate::types::{
    Conversation, ConversationId, Me, Message, MessageId, MessageRef, Mri, Outgoing, Reaction,
    ReadHorizon, ReplyTarget, SendReceipt, ThreadInfo, Timestamp, now_ms, parse_iso_ms,
};
use crate::{Capture, Error, Result};
use paging::{CursorInner, Endpoint, next_cursor, parse_page_meta};
use wire::{
    ConversationsResponse, MessagesResponse, parse_conversation, parse_me, parse_message,
    parse_thread,
};

/// Most MRIs per `fetchShortProfile` call (spec §4.4.3).
const MAX_PROFILES_PER_CALL: usize = 900;
/// The error a `backwardLink` to another origin is refused with.
const OFF_ORIGIN_LINK: &str = "backwardLink points outside the chat service; not followed";

/// Whether `e`, from fetching one conversation's messages, will not go away
/// by retrying this session: a 4xx other than 408 / 429 (e.g. removed from
/// the chat) or a refused off-origin `backwardLink`.
pub(crate) fn is_final_for_conversation(e: &Error) -> bool {
    match e {
        Error::Http { status, .. } => refused_page(*status),
        Error::Protocol(m) => m == OFF_ORIGIN_LINK,
        _ => false,
    }
}

/// Whether `status`, answering a followed conversations page, ends paging
/// (a 4xx other than 408 / 429) instead of failing the listing.
fn refused_page(status: u16) -> bool {
    (400..500).contains(&status) && !matches!(status, 408 | 429)
}

/// Most pages `messages_since` fetches before giving up on the rest.
const MAX_SINCE_PAGES: usize = 50;
/// The `fetchShortProfile` query string (spec §4.4.3).
const PROFILE_QUERY: &str = "isMailAddress=false&canBeSmtpAddress=false&enableGuest=true&includeIBBarredUsers=true&skypeTeamsInfo=true&includeBots=true";

/// How often Teams re-sends a typing signal while the user keeps typing
/// (about every 20.5 s, observed); it sends nothing when they stop. Pacing
/// [`Api::send_typing`] is the caller's.
pub const TYPING_REFRESH: Duration = Duration::from_secs(20);

/// The geos `user_state` falls back to, in order (spec A.3).
const CSA_GEOS: [&str; 3] = ["emea", "amer", "apac"];

/// The CSA user-state URL at `geo` (spec A.3).
// UNVERIFIED(§9.13): host (`Endpoints::teams`), path and query are the official client's; whether CSA serves them to our client is not known.
fn user_state_url(teams: &str, geo: &str) -> String {
    format!(
        "{teams}/api/csa/{geo}/api/v3/teams/users/me?isPrefetch=false&enableMembershipSummary=true&supportsAdditionalSystemGeneratedFolders=true&supportsSliceItems=true&enableEngageCommunities=false"
    )
}

/// The CSA geo for an authz region: `amer` for us, ca, br, amer; `apac`
/// for apac, au, jp, in, kr, sg; `emea` otherwise (`de` → `emea` observed).
fn geo_for_region(region: Option<&str>) -> &'static str {
    match region.map(str::to_ascii_lowercase).as_deref() {
        Some("us" | "ca" | "br" | "amer") => "amer",
        Some("apac" | "au" | "jp" | "in" | "kr" | "sg") => "apac",
        _ => "emea",
    }
}

/// Run-time choices for the endpoints whose shape is unverified (spec §9
/// runtime switches) and page sizes.
#[derive(Clone, Debug)]
pub struct ApiOptions {
    /// How further pages are requested (§9.1, §9.2).
    pub paging: PagingMode,
    /// The `/api/mt/{segment}` tried first by `user_names` (§9.9); the authz
    /// region is tried after it.
    pub profile_segment: String,
    /// `pageSize` for `conversations`.
    pub conversations_page_size: usize,
    /// `pageSize` for each page `messages_since` fetches.
    pub messages_page_size: usize,
    /// The CSA `{geo}` tried first by `user_state` (§9.13); `None` derives
    /// it from the authz region.
    pub csa_geo: Option<String>,
    /// Send `properties.qtdMsgs` with replies (§9.15); default `true`.
    pub send_quoted_ids: bool,
    /// `mark_read` also clears the mark-as-unread bookmark (§9.7b); default
    /// `true`.
    pub clear_unread_bookmark: bool,
    /// Image and file transfers (`fetch_media`, `upload_image`,
    /// `upload_file`).
    pub media: crate::media::MediaOptions,
}

impl Default for ApiOptions {
    fn default() -> Self {
        ApiOptions {
            paging: PagingMode::Auto,
            profile_segment: "apac".into(),
            conversations_page_size: 100,
            messages_page_size: 50,
            csa_geo: None,
            send_quoted_ids: true,
            clear_unread_bookmark: true,
            media: crate::media::MediaOptions::default(),
        }
    }
}

/// The chatsvc and `/api/mt` endpoints.
pub struct Api {
    http: Arc<Http>,
    options: ApiOptions,
    /// `me()`'s cache; the lock also makes the first fetch single-flight.
    me: tokio::sync::Mutex<Option<Me>>,
}

impl std::fmt::Debug for Api {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Api")
            .field("options", &self.options)
            .finish_non_exhaustive()
    }
}

impl Api {
    pub(crate) fn new(http: Arc<Http>, options: ApiOptions) -> Api {
        Api {
            http,
            options,
            me: tokio::sync::Mutex::new(None),
        }
    }

    pub fn options(&self) -> &ApiOptions {
        &self.options
    }

    /// The capture sink, when capture mode is on.
    pub fn capture(&self) -> Option<&Arc<Capture>> {
        self.http.capture()
    }

    /// The chat-service base URL, without a trailing slash.
    async fn base(&self) -> Result<String> {
        Ok(self
            .http
            .chat_service()
            .await?
            .trim_end_matches('/')
            .to_string())
    }

    /// A `backwardLink` cursor's URL, resolved against the chat service
    /// (it may be relative). A link to any other origin is refused: the
    /// request would carry the Skype token there.
    async fn link_url(&self, link: &str) -> Result<String> {
        let base = self.base().await?;
        let base = url::Url::parse(&format!("{base}/"))
            .map_err(|e| Error::Protocol(format!("chat service URL: {e}")))?;
        let url = base
            .join(link)
            .map_err(|e| Error::Protocol(format!("backwardLink: {e}")))?;
        if url.origin() != base.origin() {
            return Err(Error::Protocol(OFF_ORIGIN_LINK.into()));
        }
        Ok(url.into())
    }

    /// One page of conversations, most recently active first: those active
    /// since `since` (ms; `None` = all), or the page `cursor` names.
    pub async fn conversations(
        &self,
        since: Option<Timestamp>,
        cursor: Option<PageCursor>,
    ) -> Result<Page<Conversation>> {
        let n = self.options.conversations_page_size;
        let url = match &cursor {
            Some(PageCursor(CursorInner::Link { url, .. })) => self.link_url(url).await?,
            _ => format!(
                "{}/v1/users/ME/conversations?startTime={}&pageSize={n}&view=msnp24Equivalent&targetType=Passport|Skype|Lync|Thread|PSTN|Agent",
                self.base().await?,
                since.unwrap_or(0),
            ),
        };
        let followed = matches!(&cursor, Some(PageCursor(CursorInner::Link { .. })));
        let sent = self
            .http
            .send(HttpRequest::get(url).label("conversations"))
            .await;
        let resp = match sent.and_then(crate::http::HttpResponse::ok) {
            Ok(resp) => resp,
            // Part of the §9.1 paging rule (see `paging::next_cursor`): a
            // followed page the server refuses (live 2026-10-14: a 401 on
            // the `syncState` link) ends paging; the pages before it stand.
            Err(Error::Http { status, body }) if followed && refused_page(status) => {
                tracing::warn!(
                    status,
                    "conversations backwardLink page refused; paging stopped"
                );
                return Ok(Page {
                    items: Vec::new(),
                    next: None,
                    meta: PageMeta::default(),
                    note: Some(format!(
                        "conversations backwardLink page refused (http {status}: {}); stopped",
                        crate::redact::redact_text(&body.chars().take(120).collect::<String>())
                    )),
                });
            }
            Err(e) => return Err(e),
        };
        let resp: ConversationsResponse = resp.json()?;
        let items = resp
            .conversations
            .iter()
            .filter_map(parse_conversation)
            .collect();
        let meta = parse_page_meta(resp.metadata.as_ref());
        let ids = raw_ids(&resp.conversations);
        let (next, note) = next_cursor(
            self.options.paging,
            Endpoint::Conversations,
            &meta,
            cursor.as_ref(),
            &ids,
            n,
        );
        Ok(Page {
            items,
            next,
            meta,
            note,
        })
    }

    /// Up to `n` messages of `conv`, newest first: the latest page without a
    /// cursor, else the older page `cursor` names. For a team channel the
    /// page holds posts and replies together, each reply with its
    /// [`Message::thread_root`].
    // UNVERIFIED(§9.22): a channel's `…/conversations/{channel}/messages` returns its posts and their replies interleaved by arrival, paged like a chat's.
    pub async fn messages(
        &self,
        conv: &ConversationId,
        cursor: Option<PageCursor>,
        n: usize,
    ) -> Result<Page<Message>> {
        // The latest page is exactly the spike's verified query.
        let latest = format!(
            "{}/v1/users/ME/conversations/{}/messages?pageSize={n}&view=msnp24Equivalent",
            self.base().await?,
            seg(&conv.0)
        );
        let url = match &cursor {
            None => latest,
            Some(PageCursor(CursorInner::EndTime(t))) => {
                format!("{latest}&startTime=1&endTime={t}")
            }
            Some(PageCursor(CursorInner::Link { url, .. })) => self.link_url(url).await?,
        };
        self.messages_page(conv, url, cursor.as_ref(), n).await
    }

    /// Every message of `conv` that arrived since `since` (ms), newest first,
    /// paged with the same cursor rule as [`Api::messages`] until exhausted,
    /// a page on which no item arrived at or after `since` (by its raw
    /// arrival id, counting items that do not parse), or 50
    /// pages. Nothing the server returned is dropped (the store's version
    /// upsert makes extra items harmless). Used by catch-up.
    pub async fn messages_since(
        &self,
        conv: &ConversationId,
        since: Timestamp,
    ) -> Result<Vec<Message>> {
        Ok(self.messages_since_noted(conv, since).await?.0)
    }

    /// [`Api::messages_since`], plus why paging ended early (a loop guard
    /// or the page cap), if it did.
    pub(crate) async fn messages_since_noted(
        &self,
        conv: &ConversationId,
        since: Timestamp,
    ) -> Result<(Vec<Message>, Option<String>)> {
        let n = self.options.messages_page_size;
        let first = format!(
            "{}/v1/users/ME/conversations/{}/messages?startTime={since}&pageSize={n}&view=msnp24Equivalent",
            self.base().await?,
            seg(&conv.0)
        );
        let mut all = Vec::new();
        let mut cursor: Option<PageCursor> = None;
        for _ in 0..MAX_SINCE_PAGES {
            let url = match &cursor {
                None => first.clone(),
                // The window keeps `startTime={since}`.
                Some(PageCursor(CursorInner::EndTime(t))) => format!("{first}&endTime={t}"),
                Some(PageCursor(CursorInner::Link { url, .. })) => self.link_url(url).await?,
            };
            let (page, raw) = self
                .messages_page_raw(conv, url, cursor.as_ref(), n)
                .await?;
            all.extend(page.items);
            // A `backwardLink` need not keep `startTime`: a page with nothing
            // that arrived since then means everything newer has been seen.
            let any_new = raw
                .iter()
                .any(|id| id.parse::<i64>().map_or(true, |id| id >= since));
            if !any_new {
                return Ok((all, None));
            }
            if let Some(note) = page.note {
                tracing::warn!(conversation = %conv.0, %note, "messages_since paging stopped");
                return Ok((all, Some(note)));
            }
            match page.next {
                Some(next) => cursor = Some(next),
                None => return Ok((all, None)),
            }
        }
        tracing::warn!(
            conversation = %conv.0,
            pages = MAX_SINCE_PAGES,
            "messages_since page cap reached; older messages not fetched"
        );
        Ok((
            all,
            Some(format!(
                "messages_since {MAX_SINCE_PAGES}-page cap reached; older messages not fetched"
            )),
        ))
    }

    /// Fetches one messages page from `url`; `prev` is the cursor that named it.
    async fn messages_page(
        &self,
        conv: &ConversationId,
        url: String,
        prev: Option<&PageCursor>,
        n: usize,
    ) -> Result<Page<Message>> {
        Ok(self.messages_page_raw(conv, url, prev, n).await?.0)
    }

    /// [`Api::messages_page`], plus the raw arrival id of every item the
    /// server returned.
    async fn messages_page_raw(
        &self,
        conv: &ConversationId,
        url: String,
        prev: Option<&PageCursor>,
        n: usize,
    ) -> Result<(Page<Message>, Vec<String>)> {
        let resp: MessagesResponse = self
            .http
            .send(HttpRequest::get(url).label("messages"))
            .await?
            .ok()?
            .json()?;
        let items = resp
            .messages
            .iter()
            .filter_map(|m| parse_message(m, Some(conv)))
            .collect();
        let meta = parse_page_meta(resp.metadata.as_ref());
        let ids = raw_ids(&resp.messages);
        let (next, note) = next_cursor(
            self.options.paging,
            Endpoint::Messages,
            &meta,
            prev,
            &ids,
            n,
        );
        let page = Page {
            items,
            next,
            meta,
            note,
        };
        Ok((page, ids))
    }

    /// The signed-in user; fetched once, then cached until a new session
    /// is signed in (or the client logs out). When `/properties` carries no
    /// real name (seen live: no `userDetails`), the name comes from
    /// [`user_names`](Self::user_names) for the user's own MRI, as Teams
    /// does. A network or sign-in failure of that lookup is returned (nothing
    /// is cached, so the next call tries again); any other failure keeps the
    /// fallback name, cached. The first call holds the cache lock across
    /// both requests, so concurrent callers wait for it.
    pub async fn me(&self) -> Result<Me> {
        let mut cached = self.me.lock().await;
        if let Some(me) = &*cached {
            return Ok(me.clone());
        }
        let url = format!("{}/v1/users/ME/properties", self.base().await?);
        let v: Value = self
            .http
            .send(HttpRequest::get(url).label("me"))
            .await?
            .ok()?
            .json()?;
        let (mut me, named) = parse_me(&v)
            .ok_or_else(|| Error::Protocol("/v1/users/ME/properties has no skypeName".into()))?;
        // UNVERIFIED(§9.9): `fetchShortProfile` answers for the user's own MRI with their display name (Teams web looks itself up there at startup, observed; its sent `imdisplayname` equals that name).
        if !named {
            match self.user_names(std::slice::from_ref(&me.mri)).await {
                Ok(mut names) => match names.remove(&me.mri).filter(|n| !me.is_placeholder(n)) {
                    Some(name) => me.display_name = name,
                    None => tracing::debug!("no profile name for the signed-in user"),
                },
                Err(e) if lookup_failure_is_transient(&e) => return Err(e),
                Err(e) => {
                    tracing::warn!(error = %e, "profile lookup of the signed-in user failed; keeping the fallback name");
                }
            }
        }
        *cached = Some(me.clone());
        Ok(me)
    }

    /// Drops the cached [`me`](Self::me) (a new session may be another
    /// user); the next call fetches it again.
    pub(crate) async fn forget_me(&self) {
        *self.me.lock().await = None;
    }

    /// A thread's topic and roster.
    pub async fn thread(&self, conv: &ConversationId) -> Result<ThreadInfo> {
        let url = format!(
            "{}/v1/threads/{}?view=msnp24Equivalent",
            self.base().await?,
            seg(&conv.0)
        );
        let v: Value = self
            .http
            .send(HttpRequest::get(url).label("thread"))
            .await?
            .ok()?
            .json()?;
        parse_thread(&v).ok_or_else(|| Error::Protocol("thread response has no id".into()))
    }

    /// Display names for `mris` from `/api/mt` (a fallback for names that
    /// `thread()` and `imdisplayname` leave unresolved). MRIs the service does
    /// not know are absent from the map. At most 900 MRIs go in one call; each
    /// call tries the `profile_segment` option, then the authz region, until one
    /// answers 2xx, and fails with the last error if none does.
    pub async fn user_names(&self, mris: &[Mri]) -> Result<HashMap<Mri, String>> {
        let mut names = HashMap::new();
        if mris.is_empty() {
            return Ok(names);
        }
        let region = self.http.tokens().handle().await.ready().await?.region;
        let segments = self.profile_segments(region.as_deref());
        let teams = self.http.endpoints().teams.trim_end_matches('/');
        for chunk in mris.chunks(MAX_PROFILES_PER_CALL) {
            let body: Vec<&str> = std::iter::once("")
                .chain(chunk.iter().map(|m| m.0.as_str()))
                .collect();
            let body = serde_json::to_value(body)?;
            let mut last = None;
            for seg in &segments {
                let url =
                    format!("{teams}/api/mt/{seg}/beta/users/fetchShortProfile?{PROFILE_QUERY}");
                let resp = self
                    .http
                    .send(HttpRequest::post_json(url, body.clone()).label("fetchShortProfile"))
                    .await?;
                match resp.ok() {
                    Ok(resp) => {
                        names.extend(parse_short_profiles(&resp.json()?));
                        last = None;
                        break;
                    }
                    Err(e) => {
                        tracing::debug!(segment = %seg, error = %e, "fetchShortProfile failed; trying the next segment");
                        last = Some(e);
                    }
                }
            }
            if let Some(e) = last {
                return Err(e);
            }
        }
        Ok(names)
    }

    /// The `/api/mt/{segment}` values `user_names` tries, in order: the
    /// `profile_segment` option, then the authz region, without repeats.
    ///
    /// UNVERIFIED(§9.9): `apac` (what purple-teams hard-codes) is assumed to
    /// work for every tenant, with the authz region as the fallback.
    fn profile_segments(&self, region: Option<&str>) -> Vec<String> {
        let mut segs = vec![self.options.profile_segment.clone()];
        if let Some(r) = region.filter(|r| !r.is_empty() && !segs.iter().any(|s| s == r)) {
            segs.push(r.to_string());
        }
        segs
    }

    /// The user's conversation folders, chats (with members) and teams
    /// (with channels) from CSA, with a report of how clean the parse was
    /// (spec A.4, A.5). One call, ~1 MB.
    pub async fn user_state(&self) -> Result<(UserState, ParseReport)> {
        let (state, report, _) = self.user_state_from().await?;
        Ok((state, report))
    }

    /// [`Api::user_state`], plus the host and geo that answered. Geos are
    /// tried in `csa_geos` order: on any non-2xx (a 401
    /// is renewed once by the HTTP layer first) the next one; a transport
    /// error or a second 401 ends the search. Fails with the last error when
    /// none answers.
    pub async fn user_state_from(&self) -> Result<(UserState, ParseReport, CsaSource)> {
        // The region only picks the first geo; read it without waiting on
        // the main session (spec A.3): unknown yet means `emea` first.
        let region = self.http.tokens().handle().await.current().region;
        let teams = self
            .http
            .endpoints()
            .teams
            .trim_end_matches('/')
            .to_string();
        let mut last = None;
        for geo in self.csa_geos(region.as_deref()) {
            let resp = self
                .http
                .send(HttpRequest::get(user_state_url(&teams, &geo)).label("csa-user-state"))
                .await?;
            match resp.ok() {
                Ok(resp) => {
                    let v: Value = resp.json()?;
                    let (state, report) = csa::parse_user_state(&v);
                    return Ok((state, report, CsaSource { host: teams, geo }));
                }
                Err(e) => {
                    tracing::debug!(geo = %geo, error = %e, "CSA user state failed; trying the next geo");
                    last = Some(e);
                }
            }
        }
        Err(last.unwrap_or_else(|| Error::Protocol("no CSA geo to try".into())))
    }

    /// The CSA geos to try, in order: the `csa_geo` option (lowercased), else the one
    /// derived from the authz region; then the rest of `emea`, `amer`,
    /// `apac`.
    // UNVERIFIED(§9.13): the region → geo mapping (`geo_for_region`) and the fallback order.
    fn csa_geos(&self, region: Option<&str>) -> Vec<String> {
        let first = self
            .options
            .csa_geo
            .as_deref()
            .filter(|g| !g.is_empty())
            .map(str::to_ascii_lowercase)
            .unwrap_or_else(|| geo_for_region(region).to_owned());
        let mut geos = vec![first];
        for g in CSA_GEOS {
            if !geos.iter().any(|x| x == g) {
                geos.push(g.to_owned());
            }
        }
        geos
    }

    /// Sends `body` to `conv` with a fresh client message id
    /// ([`new_client_message_id`]); see [`Api::send_with_id`].
    pub async fn send(
        &self,
        conv: &ConversationId,
        body: &RichText,
        reply_to: Option<&ReplyTarget>,
    ) -> Result<SendReceipt> {
        self.send_with_id(conv, body, reply_to, new_client_message_id())
            .await
    }

    /// Sends `body` to `conv` as Teams HTML under the caller's
    /// `client_message_id`, with a reply block for `reply_to` prepended (and,
    /// unless [`ApiOptions::send_quoted_ids`] is off, `properties.qtdMsgs`
    /// naming the quoted message). The receipt carries `OriginalArrivalTime`
    /// when the server returns one; it is never used to derive a
    /// [`MessageId`] (the message's id arrives with its event or the next
    /// fetch).
    pub async fn send_with_id(
        &self,
        conv: &ConversationId,
        body: &RichText,
        reply_to: Option<&ReplyTarget>,
        client_message_id: String,
    ) -> Result<SendReceipt> {
        let out = Outgoing {
            body: body.clone(),
            reply_to: reply_to.cloned(),
            client_message_id,
            ..Outgoing::default()
        };
        self.send_message(conv, &out).await
    }

    /// Sends `out` to `conv`: the general form of [`Api::send_with_id`],
    /// which also carries uploaded files (`properties.files`), replies in a
    /// channel thread ([`Outgoing::thread_root`], sent to the thread's
    /// address) and mentions (each [`Inline::Mention`](crate::Inline::Mention)
    /// with a real MRI, numbered into `properties.mentions`).
    pub async fn send_message(&self, conv: &ConversationId, out: &Outgoing) -> Result<SendReceipt> {
        let me = self.me().await?;
        let reply_to = out.reply_to.as_ref();
        let client_message_id = if out.client_message_id.is_empty() {
            new_client_message_id()
        } else {
            out.client_message_id.clone()
        };
        let mut rt = out.body.clone();
        if let Some(t) = reply_to {
            rt.0.insert(
                0,
                Block::Reply(ReplyQuote {
                    message_id: t.message.id.clone(),
                    author: Some(t.author.clone()),
                    author_name: Some(t.author_name.clone()),
                    excerpt: RichText::text(&t.excerpt),
                }),
            );
        }
        let mentions = number_mentions(&mut rt);
        let mut payload = serde_json::json!({
            "clientmessageid": client_message_id,
            "content": to_teams_html(&rt),
            "messagetype": "RichText/Html",
            "contenttype": "text",
        });
        if let Some(name) = me.sendable_name() {
            payload["imdisplayname"] = name.into();
        }
        let mut props = serde_json::Map::new();
        if let Some(t) = reply_to.filter(|_| self.options.send_quoted_ids) {
            props.insert("qtdMsgs".into(), quoted_messages_property(t)?.into());
        }
        if let Some(m) = mentions {
            props.insert("mentions".into(), m.into());
        }
        if !out.files.is_empty() {
            // UNVERIFIED(§9.19): `properties.files` is a JSON string holding the file entries, as Teams sends it.
            let entries: Vec<&Value> = out.files.iter().map(|f| &f.property).collect();
            props.insert("files".into(), serde_json::to_string(&entries)?.into());
        }
        if !props.is_empty() {
            payload["properties"] = Value::Object(props);
        }
        // Teams lists the message's AMS images (observed 2026-10-23).
        let images = crate::media::ams_references(&rt);
        if !images.is_empty() {
            payload["amsreferences"] = images.into();
        }
        // A thread reply names its root, as Teams' own replies do (observed).
        if let Some(root) = &out.thread_root {
            payload["replyChainId"] = root.0.clone().into();
        }
        let url = format!(
            "{}/v1/users/ME/conversations/{}/messages",
            self.base().await?,
            thread_seg(conv, out.thread_root.as_ref())
        );
        let resp = self
            .http
            .send(HttpRequest::post_json(url, payload).label("send"))
            .await?
            .ok()?;
        // A 201 may have no body, or one without the field: no arrival time.
        let arrival_time = resp.json::<Value>().ok().and_then(|v| {
            v.get("OriginalArrivalTime")
                .and_then(Value::as_str)
                .and_then(parse_iso_ms)
        });
        Ok(SendReceipt {
            client_message_id,
            arrival_time,
        })
    }

    /// Advances `conv`'s read horizon to `msg` and then, unless
    /// [`ApiOptions::clear_unread_bookmark`] is off, clears its
    /// mark-as-unread bookmark as Teams does when reading (§9.7b); both
    /// carry the same timestamp. A refused bookmark PUT is the error even
    /// though the horizon moved: the horizon PUT is idempotent, so a retry
    /// is harmless.
    pub async fn mark_read(&self, conv: &ConversationId, msg: &MessageRef) -> Result<()> {
        let props = format!(
            "{}/v1/users/ME/conversations/{}/properties",
            self.base().await?,
            seg(&conv.0)
        );
        let now = now_ms();
        self.http
            .send(
                HttpRequest::put_json(
                    format!("{props}?name=consumptionhorizon"),
                    mark_read_body(msg, now),
                )
                .label("mark_read"),
            )
            .await?
            .ok()?;
        if self.options.clear_unread_bookmark {
            self.http
                .send(
                    HttpRequest::put_json(
                        format!("{props}?name=consumptionHorizonBookmark"),
                        bookmark_clear_body(now),
                    )
                    .label("mark_read_bookmark"),
                )
                .await?
                .ok()?;
        }
        Ok(())
    }

    /// Adds the caller's `reaction` to message `msg` of `conv`.
    pub async fn react(
        &self,
        conv: &ConversationId,
        msg: &MessageId,
        reaction: &Reaction,
    ) -> Result<()> {
        let req = reaction_request(&self.base().await?, conv, msg, reaction, true, now_ms());
        self.http.send(req).await?.ok()?;
        Ok(())
    }

    /// Removes the caller's `reaction` from message `msg` of `conv`.
    pub async fn unreact(
        &self,
        conv: &ConversationId,
        msg: &MessageId,
        reaction: &Reaction,
    ) -> Result<()> {
        let req = reaction_request(&self.base().await?, conv, msg, reaction, false, now_ms());
        self.http.send(req).await?.ok()?;
        Ok(())
    }

    /// The presence of `mris` (Available, Away, Busy, Offline, …), in calls
    /// of at most 200. People Teams has no presence for are left out.
    pub async fn presence(&self, mris: &[Mri]) -> Result<Vec<crate::types::Presence>> {
        let mut out = Vec::new();
        let base = self.ups_base().await;
        for chunk in mris.chunks(PRESENCE_CHUNK) {
            let body: Vec<Value> = chunk
                .iter()
                .map(|m| serde_json::json!({ "mri": m.0 }))
                .collect();
            let v: Value = self
                .http
                .send(
                    ups_headers(HttpRequest::post_json(
                        format!("{base}/v1/presence/getpresence/"),
                        Value::Array(body),
                    ))
                    .label("getpresence"),
                )
                .await?
                .ok()?
                .json()?;
            out.extend(crate::types::parse_presence(&v));
        }
        Ok(out)
    }

    /// Subscribes to presence changes of `add` (and drops `remove`), pushed
    /// to the push connection at `push_address` (see
    /// [`Client::push_address`](crate::Client::push_address)) as
    /// [`Event::Presence`](crate::Event::Presence). `purge` first drops every
    /// earlier subscription of this endpoint (the first call after a
    /// connect).
    pub async fn subscribe_presence(
        &self,
        push_address: &str,
        endpoint_id: &str,
        add: &[Mri],
        remove: &[Mri],
        purge: bool,
    ) -> Result<()> {
        let base = self.ups_base().await;
        let (url, body) = subscribe_request(&base, push_address, endpoint_id, add, remove, purge);
        self.http
            .send(
                ups_headers(HttpRequest::post_json(url, body))
                    .header("x-ms-endpoint-id", endpoint_id)
                    .label("presence-subscribe"),
            )
            .await?
            .ok()?;
        Ok(())
    }

    /// `{teams}/ups/{geo}`, the geo as for CSA.
    async fn ups_base(&self) -> String {
        let region = self.http.tokens().handle().await.current().region;
        let geo = self
            .csa_geos(region.as_deref())
            .into_iter()
            .next()
            .unwrap_or_else(|| "emea".into());
        format!(
            "{}/ups/{geo}",
            self.http.endpoints().teams.trim_end_matches('/')
        )
    }

    /// Hides `conv` from the user's chat list as Teams' "Hide" does: it
    /// comes back when a new message arrives (Teams has no "unhide"). The
    /// listing's [`Conversation::hidden`](crate::Conversation::hidden) and
    /// the next `ConversationUpdate` reflect it.
    pub async fn hide(&self, conv: &ConversationId) -> Result<()> {
        let base = self.base().await?;
        for (url, body) in hide_requests(&base, conv, now_ms()) {
            self.http
                .send(HttpRequest::put_json(url, body).label("hide"))
                .await?
                .ok()?;
        }
        Ok(())
    }

    /// Hides a team channel from the team's list, as Teams' "Hide" does;
    /// the request depends on the kind of channel (see
    /// `hide_channel_requests`).
    pub async fn hide_channel(&self, channel: &crate::csa::Channel) -> Result<()> {
        let me = self.me().await?;
        let base = self.base().await?;
        for (url, body) in hide_channel_requests(&base, channel, &me.mri, now_ms()) {
            self.http
                .send(HttpRequest::put_json(url, body).label("hide_channel"))
                .await?
                .ok()?;
        }
        Ok(())
    }

    /// Shows a hidden team channel in the team's list again, as Teams'
    /// "Show" does (see `unhide_channel_requests`).
    pub async fn unhide_channel(&self, channel: &crate::csa::Channel) -> Result<()> {
        let me = self.me().await?;
        let base = self.base().await?;
        for (url, body) in unhide_channel_requests(&base, channel, &me.mri) {
            self.http
                .send(HttpRequest::put_json(url, body).label("unhide_channel"))
                .await?
                .ok()?;
        }
        Ok(())
    }

    /// Replaces the text of one of the user's own messages with `body`
    /// (spec C.1): the message is PUT back whole, as Teams does (see
    /// `edit_body`), the original's reply quote re-inserted. Refused
    /// locally with [`Error::Invalid`] — before any request — for a pending,
    /// deleted or non-text message, one with attachments (re-sending it
    /// could drop them), or one that is not the user's. After it succeeds,
    /// [`Store::apply_local_edit`](crate::Store::apply_local_edit) shows it
    /// until the server's edit event arrives.
    pub async fn edit(&self, original: &Message, body: &RichText) -> Result<()> {
        check_editable(original)?;
        if !matches!(
            original.message_type.as_str(),
            "RichText/Html" | "RichText" | "Text"
        ) {
            return Err(Error::Invalid("only text messages can be edited"));
        }
        if !original.attachments.is_empty() {
            return Err(Error::Invalid(
                "a message with attachments cannot be edited",
            ));
        }
        let me = self.me().await?;
        check_own(original, &me)?;
        let url = message_url(&self.base().await?, original);
        let body = edit_body(original, body, &me, now_ms());
        self.http
            .send(HttpRequest::put_json(url, body).label("edit"))
            .await?
            .ok()?;
        Ok(())
    }

    /// Deletes one of the user's own messages (spec C.2, a soft delete).
    /// Refused locally with [`Error::Invalid`] for a pending or deleted
    /// message, or one that is not the user's.
    pub async fn delete(&self, original: &Message) -> Result<()> {
        check_editable(original)?;
        let me = self.me().await?;
        check_own(original, &me)?;
        let url = delete_url(&self.base().await?, original);
        self.http
            .send(HttpRequest::delete(url).label("delete"))
            .await?
            .ok()?;
        Ok(())
    }

    /// Tells `conv` that the user is typing (spec D.2): one signal. The
    /// caller repeats it every [`TYPING_REFRESH`] while the user keeps
    /// typing; there is no stop signal (Teams sends none).
    pub async fn send_typing(&self, conv: &ConversationId) -> Result<()> {
        let url = format!(
            "{}/v1/users/ME/conversations/{}/messages",
            self.base().await?,
            seg(&conv.0)
        );
        self.http
            .send(HttpRequest::post_json(url, typing_body()).label("typing"))
            .await?
            .ok()?;
        Ok(())
    }
}

/// A fresh `clientmessageid` (spec §5.3): the current epoch milliseconds
/// followed by 4 random digits, numeric only.
///
/// Ids are also strictly increasing within the process: one that would not
/// exceed the previous id is bumped to the previous id + 1, so two sends in
/// the same millisecond can never share an id.
pub fn new_client_message_id() -> String {
    static LAST: AtomicU64 = AtomicU64::new(0);
    let ms = u64::try_from(now_ms()).unwrap_or(0);
    let candidate = ms * 10_000 + u64::from(rand::random_range(0..10_000u32));
    let next = |last: u64| candidate.max(last + 1);
    // The closure always returns `Some`, so this is always `Ok(previous)`.
    let prev = LAST
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |last| {
            Some(next(last))
        })
        .unwrap_or_else(|last| last);
    next(prev).to_string()
}

/// The `consumptionhorizon` PUT body marking `msg` read at `now`.
///
/// UNVERIFIED(§9.7): the value is assumed to be
/// `"{message_id};{now_ms};{client_message_id}"` (the form the
/// conversations response carries). A message without a client message id
/// (a `ThreadActivity/*` system message) repeats its message id there, as
/// Teams does: an empty one is refused ("consumptionhorizon: Invalid
/// format").
fn mark_read_body(msg: &MessageRef, now: Timestamp) -> Value {
    let horizon = ReadHorizon {
        message_id: msg.id.clone(),
        at: now,
        client_message_id: Some(
            msg.client_message_id
                .clone()
                .unwrap_or_else(|| msg.id.0.clone()),
        ),
    };
    serde_json::json!({ "consumptionhorizon": horizon.to_wire() })
}

/// The `consumptionHorizonBookmark` PUT body clearing a manual "Mark as
/// unread" at `now`: `"0;{now_ms};0"`.
// UNVERIFIED(§9.7b): Teams' own body when it reads a chat (observed, shape only); that it clears a mark-as-unread bookmark set by Teams is not known.
fn bookmark_clear_body(now: Timestamp) -> Value {
    serde_json::json!({ "consumptionHorizonBookmark": format!("0;{now};0") })
}

/// `properties.qtdMsgs` of a reply to `t`: a JSON *string* holding
/// `[{"messageId", "sender", "time"}]` (spec B).
// UNVERIFIED(§9.15): Teams web sends this with replies (observed); that it makes Teams link the quote to the original for our client is not.
fn quoted_messages_property(t: &ReplyTarget) -> Result<String> {
    Ok(serde_json::to_string(&[serde_json::json!({
        "messageId": t.message.id.0,
        "sender": t.author.0,
        "time": t.sent_at,
    })])?)
}

/// The typing message (spec D.2).
// UNVERIFIED(§9.18): Teams web's shape (observed); whether the service accepts it from our client, and Teams shows it, is not known.
fn typing_body() -> Value {
    serde_json::json!({
        "content": "",
        "messagetype": "Control/Typing",
        "contenttype": "Application/Message",
    })
}

/// The address a message is sent to: the conversation itself, or for a
/// reply in a team-channel thread `{channel id};messageid={root id}` as a
/// URL path segment, with the `;` and `=` percent-encoded as Teams does
/// (the ids keep `seg`'s form). Only sends use it: Teams edits, deletes and
/// reacts to a reply at the channel's own address (all observed
/// 2026-10-22).
// UNVERIFIED(§9.21): Teams web's shape (observed); whether the service accepts it from our client is not known.
fn thread_seg(conv: &ConversationId, root: Option<&MessageId>) -> String {
    match root {
        Some(r) => format!("{}%3Bmessageid%3D{}", seg(&conv.0), seg(&r.0)),
        None => seg(&conv.0),
    }
}

/// Numbers the mentions of `rt` for sending: each
/// [`Inline::Mention`](crate::Inline::Mention)'s `mri` becomes its index
/// (0, 1, 2… in document order, reply quotes skipped), and the result is the
/// `properties.mentions` value naming them, a JSON string; `None` when there
/// are no mentions.
// UNVERIFIED(§9.23): the body's mention `itemid`s index `properties.mentions`, a JSON string holding `[{"@type": "http://schema.skype.com/Mention", "itemid": "<n>", "mri": <MRI>, "mentionType": "person", "displayName": <name>}]` (shape observed 2026-10-22 for a team mention, `itemid` a string), and the span needs no `itemscope` for Teams to show the mention.
fn number_mentions(rt: &mut RichText) -> Option<String> {
    let mut entries = Vec::new();
    visit_mentions(rt, &mut |mri, name| {
        let n = entries.len();
        entries.push(serde_json::json!({
            "@type": "http://schema.skype.com/Mention",
            "itemid": n.to_string(),
            "mri": mri.0,
            "mentionType": "person",
            "displayName": name,
        }));
        *mri = Mri(n.to_string());
    });
    (!entries.is_empty()).then(|| Value::Array(entries).to_string())
}

/// Most people per presence call.
const PRESENCE_CHUNK: usize = 200;
/// The headers Teams web sends the presence service, which refuses a
/// subscription missing them (400 "Missing mandatory header '…'", one at a
/// time: user agent, correlation id, client version).
fn ups_headers(req: HttpRequest) -> HttpRequest {
    let id = || uuid::Uuid::new_v4().to_string();
    req.header("x-ms-client-user-agent", "Teams-V2-Web")
        .header("x-ms-client-type", "cdlworker")
        .header("x-ms-client-version", "1415/26100212018")
        .header("x-ms-correlation-id", &id())
        .header("x-ms-request-id", &id())
        .header("x-ms-session-id", &id())
}

/// The presence subscription request, as Teams web sends it (observed
/// 2026-10-22): `POST {ups}/v1/pubsub/subscriptions/{endpoint id}` with the
/// push address + `unifiedPresenceService`, and `source` `"ups"` on every
/// entry.
// UNVERIFIED(§9.34): that the presence service takes our main access token, and that the pushed updates follow; observed from Teams web only.
fn subscribe_request(
    base: &str,
    push_address: &str,
    endpoint_id: &str,
    add: &[Mri],
    remove: &[Mri],
    purge: bool,
) -> (String, Value) {
    let entries = |ms: &[Mri]| -> Vec<Value> {
        ms.iter()
            .map(|m| serde_json::json!({ "mri": m.0, "source": "ups" }))
            .collect()
    };
    let address = format!("{}/", push_address.trim_end_matches('/'));
    (
        format!("{base}/v1/pubsub/subscriptions/{}", seg(endpoint_id)),
        serde_json::json!({
            "trouterUri": format!("{address}unifiedPresenceService"),
            "shouldPurgePreviousSubscriptions": purge,
            "subscriptionsToAdd": entries(add),
            "subscriptionsToRemove": entries(remove),
        }),
    )
}

/// The requests hiding `conv` at `now`, as Teams sends them (observed
/// 2026-10-22): `unpinnedTime` (a number), then `historyHiddenTime` (a
/// string), both `PUT …/conversations/{conv}/properties?name=…`.
// UNVERIFIED(§9.24): Teams web's shape (observed); whether the service accepts it from our client is not known.
fn hide_requests(base: &str, conv: &ConversationId, now: Timestamp) -> [(String, Value); 2] {
    let url = |name: &str| {
        format!(
            "{base}/v1/users/ME/conversations/{}/properties?name={name}",
            seg(&conv.0)
        )
    };
    [
        (
            url("unpinnedTime"),
            serde_json::json!({ "unpinnedTime": now }),
        ),
        (
            url("historyHiddenTime"),
            serde_json::json!({ "historyHiddenTime": now.to_string() }),
        ),
    ]
}

/// How far ahead Teams sets a hidden standard channel's `expiration`
/// (observed: 14 days).
const HIDE_CHANNEL_EXPIRY_SECS: i64 = 14 * 24 * 3600;

/// The requests hiding a team channel, as Teams sends them (observed
/// 2026-10-23): every kind ends with `PUT …/conversations/{channel}/
/// properties?name=favorite` `{"favorite":"false"}` (what the team's list
/// follows). Before it, a standard channel sets the member's `interest` to
/// `NotInterested` (`PUT …/v1/threads/{channel}/members/{me}`, with an
/// `expiration` 14 days ahead in seconds, meaning unknown); a General
/// channel sets it without an expiration and also clears
/// `isGeneralChannelFavorite`; a private channel (`channelType` 1) sends
/// only `favorite`.
// UNVERIFIED(§9.33): Teams web's requests (observed); from our client the members request alone answered 200 but did not hide the channel, so `favorite` is taken to be the one that does.
fn hide_channel_requests(
    base: &str,
    channel: &crate::csa::Channel,
    me: &Mri,
    now: Timestamp,
) -> Vec<(String, Value)> {
    let props = |name: &str| {
        format!(
            "{base}/v1/users/ME/conversations/{}/properties?name={name}",
            seg(&channel.id.0)
        )
    };
    let favorite = (
        props("favorite"),
        serde_json::json!({ "favorite": "false" }),
    );
    if channel.channel_type.as_deref() == Some("1") {
        return vec![favorite];
    }
    let member = format!(
        "{base}/v1/threads/{}/members/{}",
        seg(&channel.id.0),
        seg(&me.0)
    );
    if channel.general {
        vec![
            (member, serde_json::json!({ "interest": "NotInterested" })),
            (
                props("isGeneralChannelFavorite"),
                serde_json::json!({ "isGeneralChannelFavorite": "false" }),
            ),
            favorite,
        ]
    } else {
        vec![
            (
                member,
                serde_json::json!({
                    "expiration": now / 1000 + HIDE_CHANNEL_EXPIRY_SECS,
                    "interest": "NotInterested",
                }),
            ),
            favorite,
        ]
    }
}

/// The requests showing a hidden team channel again, as Teams sends them
/// (observed 2026-10-23): a private channel only sets `favorite` back to
/// `"true"` (with `?requirePermanentJoinedState=true`); a standard channel
/// first sets the member's `interest` back to `Interested` (`expiration` 0)
/// and then `favorite`; a General channel sets `interest` (no expiration),
/// `isGeneralChannelFavorite` and then `favorite` (without the query).
// UNVERIFIED(§9.33): Teams web's requests (observed); our client's not yet tried.
fn unhide_channel_requests(
    base: &str,
    channel: &crate::csa::Channel,
    me: &Mri,
) -> Vec<(String, Value)> {
    let props = |name: &str| {
        format!(
            "{base}/v1/users/ME/conversations/{}/properties?name={name}",
            seg(&channel.id.0)
        )
    };
    let favorite = |permanent: bool| {
        let url = if permanent {
            format!("{}&requirePermanentJoinedState=true", props("favorite"))
        } else {
            props("favorite")
        };
        (url, serde_json::json!({ "favorite": "true" }))
    };
    if channel.channel_type.as_deref() == Some("1") {
        return vec![favorite(true)];
    }
    let member = format!(
        "{base}/v1/threads/{}/members/{}",
        seg(&channel.id.0),
        seg(&me.0)
    );
    if channel.general {
        vec![
            (member, serde_json::json!({ "interest": "Interested" })),
            (
                props("isGeneralChannelFavorite"),
                serde_json::json!({ "isGeneralChannelFavorite": "true" }),
            ),
            favorite(false),
        ]
    } else {
        vec![
            (
                member,
                serde_json::json!({ "expiration": 0, "interest": "Interested" }),
            ),
            favorite(true),
        ]
    }
}

/// The request adding (`add`) or removing a reaction at time `now`. A
/// channel reply's reactions go to the channel's own address, not the
/// thread's (observed 2026-10-22: Teams PUTs
/// `…/conversations/{channel}/messages/{reply}/properties?name=emotions`).
///
/// UNVERIFIED(§9.6): assumed to be a `PUT` (add) or `DELETE` (remove) of
/// `…/conversations/{conv}/messages/{msg}/properties?name=emotions` with
/// `{"emotions":{"key":<key>,"value":<now_ms>}}`. purple-teams only reads
/// reactions; a received reaction's `users[].value` being a ms timestamp is
/// the only hint.
fn reaction_request(
    base: &str,
    conv: &ConversationId,
    msg: &MessageId,
    reaction: &Reaction,
    add: bool,
    now: Timestamp,
) -> HttpRequest {
    let url = format!(
        "{base}/v1/users/ME/conversations/{}/messages/{}/properties?name=emotions",
        seg(&conv.0),
        seg(&msg.0)
    );
    let body = serde_json::json!({ "emotions": { "key": reaction.key(), "value": now } });
    if add {
        HttpRequest::put_json(url, body).label("react")
    } else {
        HttpRequest::delete_json(url, body).label("unreact")
    }
}

/// Refusals shared by edit and delete: a pending send or a deleted message.
fn check_editable(m: &Message) -> Result<()> {
    if m.id.0.starts_with("pending:") {
        return Err(Error::Invalid("the message has not been sent yet"));
    }
    if m.deleted {
        return Err(Error::Invalid("the message is deleted"));
    }
    Ok(())
}

/// Only the signed-in user's own messages can be edited or deleted.
fn check_own(m: &Message, me: &Me) -> Result<()> {
    if m.from == me.mri {
        Ok(())
    } else {
        Err(Error::Invalid("not your message"))
    }
}

/// `…/conversations/{conv}/messages/{id}` of `m`; a channel reply's too
/// (the channel's address, not the thread's).
fn message_url(base: &str, m: &Message) -> String {
    format!(
        "{base}/v1/users/ME/conversations/{}/messages/{}",
        seg(&m.conversation.0),
        seg(&m.id.0)
    )
}

/// The URL of a soft delete of `m`.
// UNVERIFIED(§9.17): `DELETE …/messages/{id}?behavior=softDelete` is what Teams web sends (observed); whether the service accepts it from our client is not known.
fn delete_url(base: &str, m: &Message) -> String {
    format!("{}?behavior=softDelete", message_url(base, m))
}

/// The PUT body editing `original` to `body` at `now` (spec C.1): the
/// message as received, with the new content (the reply quote re-inserted),
/// `contenttype: "Text"`, `version` = now as ISO-8601 and `properties` =
/// the original's (files, `qtdMsgs`, … kept) plus `edittime`. When `body`
/// has mentions they are numbered as for a send and replace
/// `properties.mentions`; otherwise the original's list is kept as is.
// UNVERIFIED(§9.16): which of these fields the service requires; this is the shape Teams web PUTs (observed).
fn edit_body(original: &Message, body: &RichText, me: &Me, now: Timestamp) -> Value {
    let mut rt = body.clone();
    if let Some(q) = &original.reply_to {
        rt.0.insert(0, Block::Reply(q.clone()));
    }
    let mentions = number_mentions(&mut rt);
    let arrival = iso_ms(original.arrived_at);
    let composetime = original
        .extra
        .get("composetime")
        .and_then(Value::as_str)
        .map_or_else(|| arrival.clone(), str::to_owned);
    let mut properties = match original.extra.get("properties") {
        Some(Value::Object(p)) => p.clone(),
        _ => serde_json::Map::new(),
    };
    properties.insert("edittime".into(), serde_json::json!(now));
    if let Some(m) = mentions {
        properties.insert("mentions".into(), m.into());
    }
    let mut v = serde_json::json!({
        "id": original.id.0,
        "type": "Message",
        "conversationid": original.conversation.0,
        "content": to_teams_html(&rt),
        "messagetype": "RichText/Html",
        "contenttype": "Text",
        "originalarrivaltime": arrival,
        "composetime": composetime,
        "version": iso_ms(now),
        "properties": properties,
    });
    // The name as received, unless that was the placeholder an earlier
    // send left; then the user's resolved name; else none.
    let name = original
        .from_name
        .as_deref()
        .filter(|n| !me.is_placeholder(n))
        .or_else(|| me.sendable_name());
    if let Some(name) = name {
        v["imdisplayname"] = name.into();
    }
    if let Some(c) = &original.client_message_id {
        v["clientmessageid"] = serde_json::json!(c);
    }
    // A channel reply names its thread's root, as Teams' edits do (observed).
    if let Some(root) = &original.thread_root {
        v["replyChainId"] = root.0.clone().into();
    }
    v
}

/// ISO-8601 UTC with milliseconds (`2025-10-13T09:00:00.000Z`).
fn iso_ms(ms: Timestamp) -> String {
    chrono::DateTime::from_timestamp_millis(ms).map_or_else(
        || ms.to_string(),
        |t| t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
    )
}

/// `id` as one URL path segment: RFC 3986 `pchar`s stay as they are (so
/// `19:…@thread.v2` ids are sent exactly as the spike sent them); every other
/// byte, and a whole `.` / `..` segment, is percent-encoded so an id can
/// never end the segment, start a query or climb the path.
pub(crate) fn seg(id: &str) -> String {
    if id == "." || id == ".." {
        return "%2E".repeat(id.len());
    }
    let mut out = String::with_capacity(id.len());
    for b in id.bytes() {
        let keep = b.is_ascii_alphanumeric() || b"-._~!$&'()*+,;=:@".contains(&b);
        if keep {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The arrival `id` of each raw item, for the paging guards: they compare
/// what the server returned (its count, and the arrival ids `endTime`
/// filters on), not what parsed (an unparseable item still counts).
fn raw_ids(items: &[Value]) -> Vec<String> {
    items
        .iter()
        .filter_map(|v| match v.get("id")? {
            Value::String(s) if !s.is_empty() => Some(s.clone()),
            Value::Number(n) => Some(n.to_string()),
            _ => None,
        })
        .collect()
}

/// Whether a failed own-name lookup should fail [`Api::me`] (so a later
/// call retries) rather than settle on the fallback name: the network or
/// the session failed, as for `/properties` itself.
fn lookup_failure_is_transient(e: &Error) -> bool {
    matches!(e, Error::Network(_) | Error::ReauthRequired)
}

/// `fetchShortProfile`'s answer: a `value` array, or a top-level array, of
/// `{mri, displayName}`. Entries without both are skipped.
fn parse_short_profiles(v: &Value) -> Vec<(Mri, String)> {
    let list = v.get("value").unwrap_or(v);
    list.as_array()
        .into_iter()
        .flatten()
        .filter_map(|p| {
            let mri = p.get("mri")?.as_str().filter(|s| !s.is_empty())?;
            let name = p.get("displayName")?.as_str().filter(|s| !s.is_empty())?;
            Some((Mri(mri.to_string()), name.to_string()))
        })
        .collect()
}

#[cfg(test)]
mod tests;
