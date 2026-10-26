//! Files shared in chats, through Microsoft Graph with a Graph token:
//! [`Api::upload_file`] (into the user's OneDrive, then shared with the chat)
//! and the shared-file half of [`Api::fetch_media`].
//!
//! Teams keeps a chat's files in the sender's OneDrive, folder "Microsoft
//! Teams Chat Files", shares them with the chat's members and lists them in
//! the message's `properties.files`. Every Graph call here is a guess at
//! that flow (§9.30–§9.32).

use base64::Engine;
use serde::Deserialize;
use serde_json::{Value, json};

use super::media::{MEDIA_TIMEOUT, header_str};
use super::{Api, seg};
use crate::http::HttpRequest;
use crate::media::{FileSharing, Media};
use crate::types::{ConversationId, UploadedFile, is_notes};
use crate::{Error, Result, Secret};

/// The OneDrive folder Teams keeps chat files in.
const CHAT_FILES_FOLDER: &str = "Microsoft Teams Chat Files";
/// The largest file sent in one `PUT` (Graph's simple-upload limit); larger
/// ones go through an upload session.
const SIMPLE_UPLOAD_MAX: usize = 4 * 1024 * 1024;
/// An upload-session chunk: a multiple of 320 KiB (Graph's rule), 10 MiB.
const UPLOAD_CHUNK: usize = 32 * 320 * 1024;
/// Characters OneDrive refuses in a name; each becomes `_`.
const FORBIDDEN_IN_NAMES: &[char] = &['"', '*', ':', '<', '>', '?', '/', '\\', '|'];

/// The parts of a Graph `driveItem` used here.
#[derive(Debug, Default, Deserialize)]
struct DriveItem {
    #[serde(default)]
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default, rename = "webUrl")]
    web_url: String,
    /// `sharepointIds` (only when selected): its `listItemUniqueId` is the
    /// id Teams gives the file in a message, its `siteUrl` the OneDrive's.
    #[serde(default, rename = "sharepointIds")]
    sharepoint_ids: Option<Value>,
    #[serde(default)]
    size: Option<u64>,
    #[serde(default)]
    file: Option<FileFacet>,
    /// Pre-authenticated (`tempauth`): a secret.
    #[serde(default, rename = "@microsoft.graph.downloadUrl")]
    download_url: Option<Secret<String>>,
}

#[derive(Debug, Default, Deserialize)]
struct FileFacet {
    #[serde(default, rename = "mimeType")]
    mime_type: Option<String>,
}

impl Api {
    /// Uploads `bytes` as `file_name` into the user's OneDrive ("Microsoft
    /// Teams Chat Files"; a name already taken gets a new one), shares it
    /// with `conv` as [`MediaOptions::file_sharing`](crate::MediaOptions::file_sharing)
    /// says (by default each other member of the chat, from
    /// [`Api::thread`], gets read access), and returns the entry to send in
    /// [`Outgoing::files`](crate::Outgoing::files).
    pub async fn upload_file(
        &self,
        conv: &ConversationId,
        file_name: &str,
        bytes: Vec<u8>,
    ) -> Result<UploadedFile> {
        let name = drive_file_name(file_name)?;
        let mut item = self
            .upload_to_chat_files(&name, bytes, SIMPLE_UPLOAD_MAX, UPLOAD_CHUNK)
            .await?;
        if item.id.is_empty() || item.web_url.is_empty() {
            return Err(Error::Protocol(
                "the uploaded driveItem has no id or webUrl".into(),
            ));
        }
        if item.sharepoint_ids.is_none() {
            item = self.drive_item_with_ids(&item.id).await?;
        }
        let sharing = self.options.media.file_sharing;
        let shared = self.share_with_chat(conv, &item.id, sharing).await?;
        Ok(UploadedFile {
            name: item.name.clone(),
            property: file_property(&item, shared.as_ref())?,
        })
    }

    /// A shared file's bytes: its `driveItem` through Graph's `/shares`
    /// (size checked against the cap first), then the item's
    /// pre-authenticated download URL, fetched without any token.
    pub(super) async fn fetch_shared_file(&self, url: &str) -> Result<Media> {
        let graph = self.graph_base();
        let item: DriveItem = self
            .http
            .send(HttpRequest::get(shared_item_url(&graph, url)).label("graph-shared-item"))
            .await?
            .ok()?
            .json()?;
        let max = self.options.media.max_download_bytes;
        if item.size.is_some_and(|n| n > max) {
            return Err(Error::Invalid("the media is larger than the size limit"));
        }
        let download = item.download_url.as_ref().ok_or_else(|| {
            Error::Protocol("the shared item has no download URL (a folder, or no access)".into())
        })?;
        let resp = self
            .http
            .send(
                HttpRequest::get(download.expose())
                    .without_tokens()
                    .binary(max)
                    .timeout(MEDIA_TIMEOUT)
                    .label("graph-download"),
            )
            .await?
            .ok()?;
        Ok(Media {
            content_type: item
                .file
                .and_then(|f| f.mime_type)
                .filter(|m| !m.is_empty())
                .or_else(|| header_str(&resp.headers, reqwest::header::CONTENT_TYPE)),
            file_name: Some(item.name).filter(|n| !n.is_empty()),
            bytes: resp.bytes,
        })
    }

    fn graph_base(&self) -> String {
        self.options
            .media
            .graph_base
            .trim_end_matches('/')
            .to_string()
    }

    /// One simple `PUT` up to `simple_max` bytes, else an upload session in
    /// `chunk`-byte pieces; the created `driveItem`.
    async fn upload_to_chat_files(
        &self,
        name: &str,
        bytes: Vec<u8>,
        simple_max: usize,
        chunk: usize,
    ) -> Result<DriveItem> {
        let graph = self.graph_base();
        if bytes.len() <= simple_max {
            return self
                .http
                .send(simple_upload_request(&graph, name, bytes))
                .await?
                .ok()?
                .json();
        }
        let session: Value = self
            .http
            .send(upload_session_request(&graph, name))
            .await?
            .ok()?
            .json()?;
        let upload_url = session
            .get("uploadUrl")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Protocol("createUploadSession returned no uploadUrl".into()))?;
        let total = bytes.len();
        let mut last = None;
        for (i, piece) in bytes.chunks(chunk.max(1)).enumerate() {
            let start = i * chunk.max(1);
            let end = start + piece.len() - 1;
            // The upload URL is pre-authenticated: Graph's rule is to send it
            // no Authorization header.
            let req =
                HttpRequest::put_bytes(upload_url, piece.to_vec(), "application/octet-stream")
                    .header("Content-Range", &format!("bytes {start}-{end}/{total}"))
                    .without_tokens()
                    .timeout(MEDIA_TIMEOUT)
                    .label("graph-upload-chunk");
            last = Some(self.http.send(req).await?.ok()?);
        }
        last.ok_or_else(|| Error::Protocol("no upload chunk was sent".into()))?
            .json()
    }

    /// An uploaded item again, with its `sharepointIds`.
    async fn drive_item_with_ids(&self, item_id: &str) -> Result<DriveItem> {
        let url = format!(
            "{}/v1.0/me/drive/items/{}?$select={ITEM_FIELDS}",
            self.graph_base(),
            seg(item_id)
        );
        self.http
            .send(HttpRequest::get(url).label("graph-item"))
            .await?
            .ok()?
            .json()
    }

    /// Shares the item with `conv`: how, and the sharing link Graph answered
    /// with, if any; `None` when nothing was shared. With
    /// [`FileSharing::Members`], every other `8:orgid:` member of the chat
    /// is invited; a chat with no one else (Notes, or alone) shares nothing.
    async fn share_with_chat(
        &self,
        conv: &ConversationId,
        item_id: &str,
        sharing: FileSharing,
    ) -> Result<Option<Shared>> {
        let graph = self.graph_base();
        let req = match sharing {
            FileSharing::OrganizationLink => share_request(&graph, item_id, sharing, &[]),
            FileSharing::Members => {
                if is_notes(conv) {
                    return Ok(None);
                }
                let me = self.me().await?;
                let thread = self.thread(conv).await?;
                let mut guids: Vec<String> = thread
                    .members
                    .iter()
                    .filter(|m| m.mri != me.mri)
                    .filter_map(|m| m.mri.orgid_guid().map(str::to_string))
                    .collect();
                guids.sort();
                guids.dedup();
                if guids.is_empty() {
                    return Ok(None);
                }
                share_request(&graph, item_id, sharing, &guids)
            }
        };
        let v: Value = self.http.send(req).await?.ok()?.json()?;
        Ok(Some(Shared {
            how: sharing,
            link: sharing_link(&v),
        }))
    }
}

/// The `driveItem` fields an upload asks for: `sharepointIds` is only
/// returned when selected, as Teams does (`$select=*,sharepointIds,…`).
const ITEM_FIELDS: &str = "id,name,webUrl,size,file,sharepointIds";

/// How an uploaded file was shared with the chat.
#[derive(Debug)]
struct Shared {
    how: FileSharing,
    link: Option<String>,
}

/// A file name OneDrive accepts: trimmed, forbidden and control characters
/// as `_`, no leading or trailing dots; `Error::Invalid` when nothing is
/// left.
fn drive_file_name(name: &str) -> Result<String> {
    let cleaned: String = name
        .trim()
        .chars()
        .map(|c| {
            if c.is_control() || FORBIDDEN_IN_NAMES.contains(&c) {
                '_'
            } else {
                c
            }
        })
        .collect();
    let cleaned = cleaned.trim_matches(|c: char| c == '.' || c.is_whitespace());
    if cleaned.is_empty() {
        return Err(Error::Invalid("an empty file name"));
    }
    Ok(cleaned.to_string())
}

/// `{graph}/v1.0/me/drive/root:/Microsoft Teams Chat Files/{name}:/{action}`,
/// each path segment percent-encoded.
fn chat_files_url(graph: &str, name: &str, action: &str) -> String {
    format!(
        "{graph}/v1.0/me/drive/root:/{}/{}:/{action}",
        seg(CHAT_FILES_FOLDER),
        seg(name)
    )
}

/// Uploading a file of up to 4 MiB in one request.
// UNVERIFIED(§9.31): the chat's files go to the sender's OneDrive folder "Microsoft Teams Chat Files", as Teams does, by `PUT {graph}/v1.0/me/drive/root:/Microsoft Teams Chat Files/{name}:/content?@microsoft.graph.conflictBehavior=rename` (raw bytes), answered with the new `driveItem`.
fn simple_upload_request(graph: &str, name: &str, bytes: Vec<u8>) -> HttpRequest {
    HttpRequest::put_bytes(
        format!(
            "{}?@microsoft.graph.conflictBehavior=rename&$select={ITEM_FIELDS}",
            chat_files_url(graph, name, "content")
        ),
        bytes,
        "application/octet-stream",
    )
    .timeout(MEDIA_TIMEOUT)
    .label("graph-upload")
}

/// Starting an upload session for a larger file.
// UNVERIFIED(§9.31): `POST …/root:/Microsoft Teams Chat Files/{name}:/createUploadSession` with `{"item":{"@microsoft.graph.conflictBehavior":"rename"}}` answers `{"uploadUrl"}`; chunks are `PUT` there in order with `Content-Range: bytes a-b/total` and no token, and the last one answers the `driveItem`.
fn upload_session_request(graph: &str, name: &str) -> HttpRequest {
    HttpRequest::post_json(
        chat_files_url(graph, name, "createUploadSession"),
        json!({"item": {"@microsoft.graph.conflictBehavior": "rename"}}),
    )
    .label("graph-upload-session")
}

/// Sharing the uploaded item with the chat.
// UNVERIFIED(§9.31): members (default): `POST {graph}/v1.0/me/drive/items/{id}/invite` with `{"recipients":[{"objectId":"<AAD guid>"}…],"roles":["read"],"requireSignIn":true,"sendInvitation":false}` gives each member read access without an e-mail; `organization`: `POST …/items/{id}/createLink` with `{"type":"view","scope":"organization"}`. Either is enough for the members to open the file from Teams.
fn share_request(
    graph: &str,
    item_id: &str,
    sharing: FileSharing,
    member_guids: &[String],
) -> HttpRequest {
    let item = format!("{graph}/v1.0/me/drive/items/{}", seg(item_id));
    match sharing {
        FileSharing::Members => {
            let recipients: Vec<Value> = member_guids
                .iter()
                .map(|g| json!({"objectId": g}))
                .collect();
            HttpRequest::post_json(
                format!("{item}/invite"),
                json!({
                    "recipients": recipients,
                    "roles": ["read"],
                    "requireSignIn": true,
                    "sendInvitation": false,
                }),
            )
            .label("graph-invite")
        }
        FileSharing::OrganizationLink => HttpRequest::post_json(
            format!("{item}/createLink"),
            json!({"type": "view", "scope": "organization"}),
        )
        .label("graph-create-link"),
    }
}

/// The sharing link in an `invite` answer (`value[].link.webUrl`, the first
/// one) or a `createLink` answer (`link.webUrl`).
fn sharing_link(v: &Value) -> Option<String> {
    let link = |p: &Value| {
        p.pointer("/link/webUrl")
            .and_then(Value::as_str)
            .filter(|u| !u.is_empty())
            .map(str::to_string)
    };
    link(v).or_else(|| v.get("value")?.as_array()?.iter().find_map(link))
}

/// A shared file's `driveItem` through Graph's `/shares`.
// UNVERIFIED(§9.30): Graph resolves a chat file's `objectUrl` (a plain SharePoint item URL, not a sharing link) as `GET {graph}/v1.0/shares/u!{base64url(url), unpadded}/driveItem` for a member it was shared with, and answers with `@microsoft.graph.downloadUrl` (pre-authenticated, fetched without a token) and `size`.
fn shared_item_url(graph: &str, url: &str) -> String {
    let id = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(url.as_bytes());
    format!("{graph}/v1.0/shares/u!{id}/driveItem")
}

/// The `properties.files` entry for an uploaded item, as Teams builds it
/// (observed 2026-10-22, a file sent to Notes): `id` and `itemid` the
/// item's `sharepointIds.listItemUniqueId`, `fileInfo.itemId` its drive
/// item id, `objectUrl` / `fileInfo.fileUrl` its `webUrl` as returned
/// (percent-encoded), `baseUrl` / `fileInfo.siteUrl` its
/// `sharepointIds.siteUrl` plus `/`, and `sharepointIds` itself. Nothing
/// shared leaves `shareUrl`, `shareId` and `permissionScope` `null`.
// UNVERIFIED(§9.32): Teams accepts this entry from our client; for a file shared with other members, `fileInfo.shareUrl` = the sharing link (else the `objectUrl`) and `permissionScope` `users` (invite) or `organization` (link) are guesses.
fn file_property(item: &DriveItem, shared: Option<&Shared>) -> Result<Value> {
    let ids = item.sharepoint_ids.clone().unwrap_or(Value::Null);
    let unique = ids
        .get("listItemUniqueId")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Protocol("the uploaded driveItem has no sharepointIds".into()))?
        .to_owned();
    let base_url = ids
        .get("siteUrl")
        .and_then(Value::as_str)
        .map(|s| format!("{}/", s.trim_end_matches('/')))
        .unwrap_or_else(|| personal_site_url(&item.web_url));
    let ext = item
        .name
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    let (share_url, scope) = match shared {
        Some(s) => (
            Value::from(s.link.clone().unwrap_or_else(|| item.web_url.clone())),
            Value::from(match s.how {
                FileSharing::Members => "users",
                FileSharing::OrganizationLink => "organization",
            }),
        ),
        None => (Value::Null, Value::Null),
    };
    Ok(json!({
        "itemid": unique,
        "fileName": item.name,
        "fileType": ext,
        "fileInfo": {
            "itemId": item.id,
            "fileUrl": item.web_url,
            "siteUrl": base_url,
            "serverRelativeUrl": "",
            "shareUrl": share_url,
            "shareId": null,
        },
        "fileChicletState": {"serviceName": "p2p", "state": "active"},
        "@type": "http://schema.skype.com/File",
        "version": 2,
        "id": unique,
        "baseUrl": base_url,
        "objectUrl": item.web_url,
        "openUrl": null,
        "type": ext,
        "title": item.name,
        "state": "active",
        "chicletBreadcrumbs": null,
        "providerData": "",
        "botFileProperties": {},
        "isUploadError": null,
        "progressComplete": null,
        "permissionScope": scope,
        "filePreview": {"previewUrl": "", "previewHeight": 0, "previewWidth": 0},
        "sharepointIds": ids,
        "publication": null,
        "site": null,
    }))
}

/// `https://{tenant}-my.sharepoint.com/personal/{user}/` from an item's
/// `webUrl`; the origin plus `/` when it has no `/personal/{user}`.
fn personal_site_url(web_url: &str) -> String {
    let Ok(u) = url::Url::parse(web_url) else {
        return String::new();
    };
    let origin = u.origin().ascii_serialization();
    let segs: Vec<&str> = u.path_segments().map(Iterator::collect).unwrap_or_default();
    match segs.iter().position(|s| *s == "personal") {
        Some(i) if segs.get(i + 1).is_some_and(|s| !s.is_empty()) => {
            format!("{origin}/personal/{}/", segs[i + 1])
        }
        _ => format!("{origin}/"),
    }
}

#[cfg(test)]
mod tests {
    use wiremock::matchers::{
        body_bytes, body_json, body_string_contains, header, method, path, query_param,
    };
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    use super::*;
    use crate::api::ApiOptions;
    use crate::media::MediaOptions;
    use crate::test_support::{mount_auth, mount_me, test_http};

    const CONV: &str = "19:groupgroupgroup@thread.v2";
    const WEB_URL: &str = "https://contoso-my.sharepoint.com/personal/ada_contoso_com/Documents/Microsoft%20Teams%20Chat%20Files/r%C3%A9sum%C3%A9%201.pdf";
    const OBJECT_URL: &str = "https://contoso-my.sharepoint.com/personal/ada_contoso_com/Documents/Microsoft Teams Chat Files/résumé 1.pdf";
    const UNIQUE_ID: &str = "aaaaaaaa-0000-0000-0000-000000000001";
    const UPLOAD_PATH: &str =
        "/v1.0/me/drive/root:/Microsoft%20Teams%20Chat%20Files/r%C3%A9sum%C3%A9%201.pdf:/content";

    struct NoHeader(&'static str);
    impl wiremock::Match for NoHeader {
        fn matches(&self, r: &Request) -> bool {
            !r.headers.contains_key(self.0)
        }
    }

    /// Graph grants: `graph-1`, then `graph-2`, … one per grant. Mount
    /// before `mount_auth` (the first matching mock answers).
    async fn mount_graph_grants(s: &MockServer, n: u64) {
        for i in 1..=n {
            Mock::given(method("POST"))
                .and(path("/organizations/oauth2/v2.0/token"))
                .and(body_string_contains("graph.microsoft.com"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "access_token": format!("graph-{i}"),
                    "refresh_token": format!("rt-graph-{i}"),
                    "expires_in": 3600,
                })))
                .up_to_n_times(1)
                .mount(s)
                .await;
        }
    }

    async fn api(s: &MockServer, sharing: FileSharing) -> Api {
        Api::new(
            test_http(s, None).await,
            ApiOptions {
                media: MediaOptions {
                    graph_base: format!("{}/", s.uri()),
                    file_sharing: sharing,
                    ..MediaOptions::default()
                },
                ..ApiOptions::default()
            },
        )
    }

    fn item() -> Value {
        json!({"id": "01ITEM", "name": "résumé 1.pdf", "webUrl": WEB_URL, "size": 3,
            "parentReference": {"driveId": "b!x"},
            "sharepointIds": {"listItemUniqueId": UNIQUE_ID,
                "siteUrl": "https://contoso-my.sharepoint.com/personal/ada_contoso_com"}})
    }

    async fn mount_thread(s: &MockServer) {
        Mock::given(method("GET"))
            .and(path(format!("/v1/threads/{CONV}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::from_str::<Value>(fixture!("9.09-names/thread.json")).unwrap(),
            ))
            .mount(s)
            .await;
    }

    #[tokio::test]
    async fn upload_file_puts_into_chat_files_and_invites_the_other_members() {
        let s = MockServer::start().await;
        mount_graph_grants(&s, 1).await;
        mount_auth(&s).await;
        mount_me(&s).await;
        mount_thread(&s).await;
        Mock::given(method("PUT"))
            .and(path(UPLOAD_PATH))
            .and(query_param("@microsoft.graph.conflictBehavior", "rename"))
            .and(query_param(
                "$select",
                "id,name,webUrl,size,file,sharepointIds",
            ))
            .and(header("Authorization", "Bearer graph-1"))
            .and(NoHeader("X-Skypetoken"))
            .and(header("Content-Type", "application/octet-stream"))
            .and(body_bytes(b"pdf".to_vec()))
            .respond_with(ResponseTemplate::new(201).set_body_json(item()))
            .expect(1)
            .mount(&s)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1.0/me/drive/items/01ITEM/invite"))
            .and(header("Authorization", "Bearer graph-1"))
            .and(body_json(json!({
                "recipients": [
                    {"objectId": "bbbbbbbb-2222-2222-2222-222222222222"},
                    {"objectId": "cccccccc-3333-3333-3333-333333333333"},
                ],
                "roles": ["read"],
                "requireSignIn": true,
                "sendInvitation": false,
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"value": [
                {"id": "p1", "roles": ["read"]},
                {"id": "p2", "roles": ["read"], "link": {"webUrl": "https://contoso-my.sharepoint.com/:b:/g/personal/ada/E1"}},
            ]})))
            .expect(1)
            .mount(&s)
            .await;
        let api = api(&s, FileSharing::Members).await;
        let f = api
            .upload_file(
                &ConversationId(CONV.into()),
                " résumé 1.pdf ",
                b"pdf".to_vec(),
            )
            .await
            .unwrap();
        assert_eq!(f.name, "résumé 1.pdf");
        let p = &f.property;
        assert_eq!(p["@type"], "http://schema.skype.com/File");
        // As Teams sends them: the webUrl as returned, ids from sharepointIds.
        assert_eq!(
            (&p["objectUrl"], &p["fileInfo"]["fileUrl"]),
            (&json!(WEB_URL), &json!(WEB_URL))
        );
        assert_eq!(
            p["baseUrl"],
            "https://contoso-my.sharepoint.com/personal/ada_contoso_com/"
        );
        assert_eq!(
            (&p["itemid"], &p["id"]),
            (&json!(UNIQUE_ID), &json!(UNIQUE_ID))
        );
        assert_eq!(p["fileInfo"]["itemId"], "01ITEM");
        assert_eq!(p["sharepointIds"]["listItemUniqueId"], UNIQUE_ID);
        assert_eq!((&p["type"], &p["fileType"]), (&json!("pdf"), &json!("pdf")));
        assert_eq!(
            p["fileInfo"]["shareUrl"],
            "https://contoso-my.sharepoint.com/:b:/g/personal/ada/E1"
        );
        assert_eq!(p["permissionScope"], "users");
        // What the message carries parses back to an attachment the
        // gateway can hand to `fetch_media`.
        assert!(
            crate::media::media_source(
                &url::Url::parse(p["objectUrl"].as_str().unwrap()).unwrap(),
                &[]
            ) == Some(crate::media::MediaSource::SharePoint)
        );
    }

    #[tokio::test]
    async fn the_organization_switch_creates_a_view_link_and_skips_the_roster() {
        let s = MockServer::start().await;
        mount_graph_grants(&s, 1).await;
        mount_auth(&s).await;
        Mock::given(method("PUT"))
            .and(path(UPLOAD_PATH))
            .respond_with(ResponseTemplate::new(201).set_body_json(item()))
            .mount(&s)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1.0/me/drive/items/01ITEM/createLink"))
            .and(body_json(json!({"type": "view", "scope": "organization"})))
            .respond_with(
                ResponseTemplate::new(201)
                    .set_body_json(json!({"link": {"webUrl": "https://l/view"}})),
            )
            .expect(1)
            .mount(&s)
            .await;
        let api = api(&s, FileSharing::OrganizationLink).await;
        let f = api
            .upload_file(
                &ConversationId(CONV.into()),
                "résumé 1.pdf",
                b"pdf".to_vec(),
            )
            .await
            .unwrap();
        assert_eq!(f.property["fileInfo"]["shareUrl"], "https://l/view");
        assert_eq!(f.property["permissionScope"], "organization");
        let reqs = s.received_requests().await.unwrap();
        assert!(
            reqs.iter()
                .all(|r| !r.url.path().starts_with("/v1/threads")),
            "no roster needed"
        );
    }

    #[tokio::test]
    async fn notes_files_are_not_shared() {
        let s = MockServer::start().await;
        mount_graph_grants(&s, 1).await;
        mount_auth(&s).await;
        Mock::given(method("PUT"))
            .and(path(UPLOAD_PATH))
            .respond_with(ResponseTemplate::new(201).set_body_json(item()))
            .mount(&s)
            .await;
        let api = api(&s, FileSharing::Members).await;
        let f = api
            .upload_file(&ConversationId("48:notes".into()), "résumé 1.pdf", vec![1])
            .await
            .unwrap();
        // Nothing shared: as Teams' own Notes upload (observed).
        assert!(f.property["fileInfo"]["shareUrl"].is_null());
        assert!(f.property["permissionScope"].is_null());
        let reqs = s.received_requests().await.unwrap();
        assert!(reqs.iter().all(|r| !r.url.path().contains("/invite")));
    }

    #[tokio::test]
    async fn large_files_go_through_an_upload_session_without_a_token() {
        let s = MockServer::start().await;
        mount_graph_grants(&s, 1).await;
        mount_auth(&s).await;
        Mock::given(method("POST"))
            .and(path(
                "/v1.0/me/drive/root:/Microsoft%20Teams%20Chat%20Files/big.bin:/createUploadSession",
            ))
            .and(header("Authorization", "Bearer graph-1"))
            .and(body_json(
                json!({"item": {"@microsoft.graph.conflictBehavior": "rename"}}),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                json!({"uploadUrl": format!("{}/v1.0/up/session?tempauth=secret", s.uri())}),
            ))
            .expect(1)
            .mount(&s)
            .await;
        for (range, body) in [("bytes 0-3/10", "0123"), ("bytes 4-7/10", "4567")] {
            Mock::given(method("PUT"))
                .and(path("/v1.0/up/session"))
                .and(header("Content-Range", range))
                .and(NoHeader("Authorization"))
                .and(body_bytes(body.as_bytes().to_vec()))
                .respond_with(ResponseTemplate::new(202).set_body_json(json!({})))
                .expect(1)
                .mount(&s)
                .await;
        }
        Mock::given(method("PUT"))
            .and(path("/v1.0/up/session"))
            .and(header("Content-Range", "bytes 8-9/10"))
            .and(NoHeader("Authorization"))
            .respond_with(
                ResponseTemplate::new(201)
                    .set_body_json(json!({"id": "BIG", "name": "big 1.bin", "webUrl": WEB_URL})),
            )
            .expect(1)
            .mount(&s)
            .await;
        let api = api(&s, FileSharing::Members).await;
        let item = api
            .upload_to_chat_files("big.bin", b"0123456789".to_vec(), 4, 4)
            .await
            .unwrap();
        assert_eq!((item.id.as_str(), item.name.as_str()), ("BIG", "big 1.bin"));
    }

    #[tokio::test]
    async fn fetch_media_downloads_a_shared_file_through_graph() {
        let s = MockServer::start().await;
        mount_graph_grants(&s, 2).await;
        mount_auth(&s).await;
        let share = format!(
            "/v1.0/shares/u!{}/driveItem",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(OBJECT_URL)
        );
        // The first Graph token is refused once: renewed, then accepted.
        Mock::given(method("GET"))
            .and(path(share.clone()))
            .and(header("Authorization", "Bearer graph-1"))
            .respond_with(ResponseTemplate::new(401))
            .expect(1)
            .mount(&s)
            .await;
        Mock::given(method("GET"))
            .and(path(share))
            .and(header("Authorization", "Bearer graph-2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "01ITEM", "name": "résumé 1.pdf", "size": 3,
                "file": {"mimeType": "application/pdf"},
                "@microsoft.graph.downloadUrl": format!("{}/download/x?tempauth=secret", s.uri()),
            })))
            .expect(1)
            .mount(&s)
            .await;
        Mock::given(method("GET"))
            .and(path("/download/x"))
            .and(NoHeader("Authorization"))
            .and(NoHeader("X-Skypetoken"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"pdf".to_vec()))
            .expect(1)
            .mount(&s)
            .await;
        let api = api(&s, FileSharing::Members).await;
        let m = api.fetch_media(OBJECT_URL).await.unwrap();
        assert_eq!(
            m,
            Media {
                content_type: Some("application/pdf".into()),
                bytes: b"pdf".to_vec(),
                file_name: Some("résumé 1.pdf".into()),
            }
        );
    }

    #[tokio::test]
    async fn a_shared_file_over_the_cap_is_refused_before_downloading() {
        let s = MockServer::start().await;
        mount_graph_grants(&s, 1).await;
        mount_auth(&s).await;
        Mock::given(method("GET"))
            .and(wiremock::matchers::path_regex(
                "^/v1.0/shares/u!.*/driveItem$",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "name": "huge.iso", "size": 101u64 << 20,
                "@microsoft.graph.downloadUrl": format!("{}/download/huge", s.uri()),
            })))
            .mount(&s)
            .await;
        let api = api(&s, FileSharing::Members).await;
        let r = api.fetch_media(OBJECT_URL).await;
        assert!(matches!(r, Err(Error::Invalid(_))), "{r:?}");
        let reqs = s.received_requests().await.unwrap();
        assert!(reqs.iter().all(|r| r.url.path() != "/download/huge"));
    }

    #[test]
    fn file_names_and_site_urls() {
        assert_eq!(drive_file_name(" a:b?.txt ").unwrap(), "a_b_.txt");
        assert_eq!(drive_file_name("../x").unwrap(), "_x");
        assert!(matches!(drive_file_name(" .. "), Err(Error::Invalid(_))));
        assert_eq!(
            personal_site_url(WEB_URL),
            "https://contoso-my.sharepoint.com/personal/ada_contoso_com/"
        );
        assert_eq!(
            personal_site_url("https://contoso.sharepoint.com/sites/x/a.pdf"),
            "https://contoso.sharepoint.com/"
        );
        assert_eq!(
            chat_files_url("https://g", "a#b%.txt", "content"),
            "https://g/v1.0/me/drive/root:/Microsoft%20Teams%20Chat%20Files/a%23b%25.txt:/content"
        );
        assert_eq!(
            shared_item_url("https://g", "https://a/b?c"),
            "https://g/v1.0/shares/u!aHR0cHM6Ly9hL2I_Yw/driveItem"
        );
        assert_eq!(sharing_link(&json!({"value": []})), None);
    }

    #[test]
    fn download_urls_stay_out_of_debug_output() {
        let item: DriveItem = serde_json::from_value(json!({
            "@microsoft.graph.downloadUrl": "https://d/x?tempauth=SECRET"
        }))
        .unwrap();
        assert!(!format!("{item:?}").contains("SECRET"));
    }
}
