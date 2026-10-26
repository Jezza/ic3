//! Chat images in AMS, and the media download entry point: [`Api::fetch_media`]
//! (AMS objects, and shared files via [`super::files`]) and
//! [`Api::upload_image`].

use std::time::Duration;

use reqwest::header;
use serde_json::{Value, json};

use super::Api;
use crate::format::Inline;
use crate::http::HttpRequest;
use crate::media::{FULL_SIZE_VIEW, Media, MediaSource, media_source, object_view_url};
use crate::types::ConversationId;
use crate::{Error, Result};

/// The timeout of one media transfer (the API's 30 s is too short for
/// 100 MiB on a slow link).
pub(super) const MEDIA_TIMEOUT: Duration = Duration::from_secs(300);
/// The AMS base when neither the `ams_base` option nor `authz` names one.
const DEFAULT_AMS: &str = "https://api.asm.skype.com";
/// What a URL outside the media allow-list is refused with.
const NOT_MEDIA: &str = "not an AMS or SharePoint URL; no token is sent there";

impl Api {
    /// Downloads an image or file a message refers to: an AMS object URL
    /// (an [`Inline::Image`] in a body) with the Skype token, or a
    /// SharePoint / OneDrive URL (an [`Attachment::url`](crate::Attachment::url),
    /// the file's `objectUrl`) through Microsoft Graph with a Graph token.
    /// Any other host is refused (`Error::Invalid`) before any network: no
    /// token ever goes to a host outside that list. A body larger than
    /// [`MediaOptions::max_download_bytes`](crate::MediaOptions::max_download_bytes)
    /// is `Error::Invalid` too, refused before or while reading.
    pub async fn fetch_media(&self, url: &str) -> Result<Media> {
        let parsed = url::Url::parse(url).map_err(|_| Error::Invalid(NOT_MEDIA))?;
        // Only an AMS-shaped path needs the AMS origins (and so the tokens).
        let ams_origins = if crate::media::parse_ams_url(url).is_some() {
            self.ams_origins().await?
        } else {
            Vec::new()
        };
        match media_source(&parsed, &ams_origins) {
            Some(MediaSource::Ams) => self.fetch_ams(parsed.as_str()).await,
            // Verbatim, as the message carries it: Graph decodes the share id.
            Some(MediaSource::SharePoint) => self.fetch_shared_file(url).await,
            None => Err(Error::Invalid(NOT_MEDIA)),
        }
    }

    /// Uploads an image to AMS, readable by `conv`'s members, and returns
    /// the [`Inline::Image`] to put in a message body: its URL is the
    /// object's full-size view, which `to_teams_html` renders as Teams'
    /// AMSImage markup. `content_type` must be `image/…`.
    pub async fn upload_image(
        &self,
        conv: &ConversationId,
        file_name: &str,
        content_type: &str,
        bytes: Vec<u8>,
    ) -> Result<Inline> {
        if !content_type
            .get(..6)
            .is_some_and(|p| p.eq_ignore_ascii_case("image/"))
        {
            return Err(Error::Invalid("upload_image needs an image/… content type"));
        }
        if bytes.is_empty() {
            return Err(Error::Invalid("an empty image"));
        }
        let base = self.ams_base().await?;
        let created: Value = self
            .http
            .send(ams_create_request(&base, conv, file_name))
            .await?
            .ok()?
            .json()?;
        let id = created
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty() && !id.contains(['/', '?', '#']))
            .ok_or_else(|| Error::Protocol("AMS object creation returned no usable id".into()))?
            .to_string();
        self.http
            .send(ams_content_request(&base, &id, bytes))
            .await?
            .ok()?;
        Ok(Inline::Image {
            url: object_view_url(&base, &id, FULL_SIZE_VIEW),
        })
    }

    /// The AMS base images are uploaded to: the `ams_base` option, else
    /// `authz`'s `regionGtms.ams`, else `https://api.asm.skype.com`.
    // UNVERIFIED(§9.27): `authz`'s `regionGtms.ams` names the user's AMS host (e.g. `https://eu-api.asm.skype.com`); the global `https://api.asm.skype.com` (what the Skype web clients use) serves Teams tenants when it is absent.
    async fn ams_base(&self) -> Result<String> {
        if let Some(base) = &self.options.media.ams_base {
            return Ok(base.trim_end_matches('/').to_string());
        }
        let snap = self.http.tokens().handle().await.ready().await?;
        Ok(snap.ams_service.unwrap_or_else(|| DEFAULT_AMS.to_string()))
    }

    /// The AMS origins `fetch_media` accepts besides the built-in host
    /// suffixes: the `ams_base` option and `authz`'s AMS host.
    async fn ams_origins(&self) -> Result<Vec<String>> {
        let mut origins = Vec::new();
        if let Some(base) = &self.options.media.ams_base {
            origins.push(base.clone());
        }
        let snap = self.http.tokens().handle().await.ready().await?;
        origins.extend(snap.ams_service);
        Ok(origins
            .iter()
            .filter_map(|b| url::Url::parse(b).ok())
            .map(|u| u.origin().ascii_serialization())
            .collect())
    }

    /// `GET` an AMS object view with the Skype token, size-capped.
    async fn fetch_ams(&self, url: &str) -> Result<Media> {
        let resp = self
            .http
            .send(
                HttpRequest::get(url)
                    .label("ams-get")
                    .binary(self.options.media.max_download_bytes)
                    .timeout(MEDIA_TIMEOUT),
            )
            .await?
            .ok()?;
        Ok(Media {
            content_type: header_str(&resp.headers, header::CONTENT_TYPE),
            file_name: header_str(&resp.headers, header::CONTENT_DISPOSITION)
                .as_deref()
                .and_then(disposition_file_name),
            bytes: resp.bytes,
        })
    }
}

/// Creating the AMS object: `POST {ams}/v1/objects` (`Content-Type: application/json`).
// UNVERIFIED(§9.28): Teams web's body (observed 2026-10-23: `POST {ams}/v1/objects/` with `{"type":"pish/image","permissions":{"<conversation id>":["read"]},"sharingMode":"Inline","filename":"<name>"}`, answered 201 `{"id"}`); that it works from our client is not known.
fn ams_create_request(base: &str, conv: &ConversationId, file_name: &str) -> HttpRequest {
    HttpRequest::post_json(
        format!("{base}/v1/objects"),
        json!({
            "type": "pish/image",
            "permissions": { conv.0.as_str(): ["read"] },
            "sharingMode": "Inline",
            "filename": file_name,
        }),
    )
    .label("ams-create")
}

/// Uploading the AMS object's content.
// UNVERIFIED(§9.28): the image bytes go in `PUT {ams}/v1/objects/{id}/content/imgpsh` as `application/octet-stream`, after which the `imgo` view serves the full-size image.
fn ams_content_request(base: &str, id: &str, bytes: Vec<u8>) -> HttpRequest {
    HttpRequest::put_bytes(
        format!("{base}/v1/objects/{id}/content/imgpsh"),
        bytes,
        "application/octet-stream",
    )
    .label("ams-content")
    .timeout(MEDIA_TIMEOUT)
}

pub(super) fn header_str(
    h: &reqwest::header::HeaderMap,
    name: header::HeaderName,
) -> Option<String> {
    h.get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
}

/// The `filename` of a `Content-Disposition` (`filename="a b.png"` or
/// `filename=a.png`); `filename*` is not decoded.
fn disposition_file_name(v: &str) -> Option<String> {
    v.split(';')
        .filter_map(|p| p.trim().split_once('='))
        .find(|(k, _)| k.trim().eq_ignore_ascii_case("filename"))
        .map(|(_, v)| v.trim().trim_matches('"').to_string())
        .filter(|n| !n.is_empty())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use wiremock::matchers::{body_bytes, body_json, body_partial_json, header, method, path};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    use super::*;
    use crate::api::ApiOptions;
    use crate::format::{Block, RichText, to_teams_html};
    use crate::media::MediaOptions;
    use crate::test_support::{mount_auth, test_http};
    use crate::{Capture, Error};

    const CONV: &str = "19:groupgroupgroup@thread.v2";

    struct NoHeader(&'static str);
    impl wiremock::Match for NoHeader {
        fn matches(&self, r: &Request) -> bool {
            !r.headers.contains_key(self.0)
        }
    }

    async fn api_with(s: &MockServer, media: MediaOptions, capture: Option<Arc<Capture>>) -> Api {
        Api::new(
            test_http(s, capture).await,
            ApiOptions {
                media,
                ..ApiOptions::default()
            },
        )
    }

    /// AMS at the mock server.
    fn ams_here(s: &MockServer) -> MediaOptions {
        MediaOptions {
            ams_base: Some(s.uri()),
            ..MediaOptions::default()
        }
    }

    #[tokio::test]
    async fn upload_image_creates_the_object_then_puts_the_bytes() {
        let s = MockServer::start().await;
        mount_auth(&s).await;
        Mock::given(method("POST"))
            .and(path("/v1/objects"))
            .and(header("Authorization", "skype_token sk-1"))
            .and(NoHeader("X-Skypetoken"))
            .and(body_json(json!({
                "type": "pish/image",
                "permissions": {CONV: ["read"]},
                "sharingMode": "Inline",
                "filename": "cat.png",
            })))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": "0-weu-d1-abc"})))
            .expect(1)
            .mount(&s)
            .await;
        Mock::given(method("PUT"))
            .and(path("/v1/objects/0-weu-d1-abc/content/imgpsh"))
            .and(header("Authorization", "skype_token sk-1"))
            .and(header("Content-Type", "application/octet-stream"))
            .and(body_bytes(b"\x89PNG-bytes".to_vec()))
            .respond_with(ResponseTemplate::new(201))
            .expect(1)
            .mount(&s)
            .await;
        let api = api_with(&s, ams_here(&s), None).await;
        let img = api
            .upload_image(
                &ConversationId(CONV.into()),
                "cat.png",
                "image/png",
                b"\x89PNG-bytes".to_vec(),
            )
            .await
            .unwrap();
        let url = format!("{}/v1/objects/0-weu-d1-abc/views/imgo", s.uri());
        assert_eq!(img, Inline::Image { url: url.clone() });
        // The body markup is Teams' AMSImage, and parses back to the same URL.
        let body = RichText(vec![Block::Paragraph(vec![img])]);
        let html = to_teams_html(&body);
        assert!(
            html.contains("itemtype=\"http://schema.skype.com/AMSImage\"")
                && html.contains("itemid=\"0-weu-d1-abc\""),
            "{html}"
        );
        assert_eq!(crate::format::from_teams_html(&html), body);
    }

    #[tokio::test]
    async fn upload_image_refuses_non_images_and_bad_ids() {
        let s = MockServer::start().await;
        mount_auth(&s).await;
        let api = api_with(&s, ams_here(&s), None).await;
        let conv = ConversationId(CONV.into());
        let r = api
            .upload_image(&conv, "a.pdf", "application/pdf", vec![1])
            .await;
        assert!(matches!(r, Err(Error::Invalid(_))), "{r:?}");
        let r = api.upload_image(&conv, "a.png", "image/png", vec![]).await;
        assert!(matches!(r, Err(Error::Invalid(_))), "{r:?}");
        assert!(s.received_requests().await.unwrap().is_empty());
        Mock::given(method("POST"))
            .and(path("/v1/objects"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": "../x"})))
            .mount(&s)
            .await;
        let r = api.upload_image(&conv, "a.png", "IMAGE/PNG", vec![1]).await;
        assert!(matches!(r, Err(Error::Protocol(_))), "{r:?}");
    }

    #[tokio::test]
    async fn the_ams_base_comes_from_authz_when_not_configured() {
        let s = MockServer::start().await;
        // Mounted first: wins over `mount_auth`'s authz.
        Mock::given(method("POST"))
            .and(path("/api/authsvc/v1.0/authz"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "tokens": {"skypeToken": "sk-a", "expiresIn": 3800},
                "region": "de",
                "regionGtms": {"chatService": s.uri(), "ams": format!("{}/", s.uri())},
            })))
            .mount(&s)
            .await;
        mount_auth(&s).await;
        Mock::given(method("GET"))
            .and(path("/v1/objects/o1/views/imgo"))
            .and(header("Authorization", "skype_token sk-a"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("Content-Type", "image/jpeg")
                    .insert_header("Content-Disposition", "inline; filename=\"a b.jpg\"")
                    .set_body_bytes(b"jpeg!".to_vec()),
            )
            .expect(1)
            .mount(&s)
            .await;
        let api = api_with(&s, MediaOptions::default(), None).await;
        assert_eq!(api.ams_base().await.unwrap(), s.uri());
        let m = api
            .fetch_media(&format!("{}/v1/objects/o1/views/imgo", s.uri()))
            .await
            .unwrap();
        assert_eq!(
            m,
            Media {
                content_type: Some("image/jpeg".into()),
                bytes: b"jpeg!".to_vec(),
                file_name: Some("a b.jpg".into()),
            }
        );
    }

    #[tokio::test]
    async fn without_any_ams_host_the_global_one_is_used() {
        let s = MockServer::start().await;
        mount_auth(&s).await;
        let api = api_with(&s, MediaOptions::default(), None).await;
        assert_eq!(api.ams_base().await.unwrap(), DEFAULT_AMS);
    }

    #[tokio::test]
    async fn fetch_media_refuses_other_hosts_before_any_network() {
        let s = MockServer::start().await;
        let api = api_with(&s, MediaOptions::default(), None).await;
        for url in [
            "https://evil.example/a.png",
            "https://evil.example/v1.0/shares/x",
            "http://contoso.sharepoint.com/a.pdf",
            "not a url",
        ] {
            let r = api.fetch_media(url).await;
            assert!(matches!(r, Err(Error::Invalid(_))), "{url}: {r:?}");
        }
        assert!(
            s.received_requests().await.unwrap().is_empty(),
            "no token was even fetched"
        );
        // An AMS-shaped path on an unknown host is refused too (it needs the
        // tokens only to learn the authz AMS host).
        mount_auth(&s).await;
        let r = api
            .fetch_media("https://evil.example/v1/objects/x/views/imgo")
            .await;
        assert!(matches!(r, Err(Error::Invalid(_))), "{r:?}");
        let reqs = s.received_requests().await.unwrap();
        assert!(
            reqs.iter()
                .all(|r| !r.url.path().starts_with("/v1/objects"))
        );
    }

    #[tokio::test]
    async fn fetch_media_caps_the_size_and_keeps_bytes_out_of_captures() {
        let s = MockServer::start().await;
        mount_auth(&s).await;
        Mock::given(method("GET"))
            .and(path("/v1/objects/big/views/imgo"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![7u8; 64]))
            .mount(&s)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/objects/ok/views/imgo"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"BINARYBODY".to_vec()))
            .mount(&s)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let capture = Arc::new(Capture::new(dir.path()).unwrap());
        let api = api_with(
            &s,
            MediaOptions {
                max_download_bytes: 32,
                ..ams_here(&s)
            },
            Some(capture.clone()),
        )
        .await;
        let r = api
            .fetch_media(&format!("{}/v1/objects/big/views/imgo", s.uri()))
            .await;
        assert!(matches!(r, Err(Error::Invalid(_))), "{r:?}");
        let m = api
            .fetch_media(&format!("{}/v1/objects/ok/views/imgo", s.uri()))
            .await
            .unwrap();
        assert_eq!(m.bytes, b"BINARYBODY");
        let text = std::fs::read_to_string(capture.last_path().unwrap()).unwrap();
        assert!(!text.contains("BINARYBODY"), "{text}");
        assert!(text.contains("<10 bytes, not captured>"), "{text}");
        assert!(!text.contains("sk-1"), "{text}");
    }

    #[tokio::test]
    async fn an_ams_401_renews_the_skype_token_once() {
        let s = MockServer::start().await;
        mount_auth(&s).await;
        Mock::given(method("GET"))
            .and(path("/v1/objects/o/views/imgo"))
            .and(header("Authorization", "skype_token sk-1"))
            .respond_with(ResponseTemplate::new(401))
            .expect(1)
            .mount(&s)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/objects/o/views/imgo"))
            .and(header("Authorization", "skype_token sk-2"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"x".to_vec()))
            .expect(1)
            .mount(&s)
            .await;
        let api = api_with(&s, ams_here(&s), None).await;
        let m = api
            .fetch_media(&format!("{}/v1/objects/o/views/imgo", s.uri()))
            .await
            .unwrap();
        assert_eq!(m.bytes, b"x");
    }

    #[test]
    fn content_disposition_names() {
        assert_eq!(
            disposition_file_name("attachment; filename=\"a.png\"").as_deref(),
            Some("a.png")
        );
        assert_eq!(
            disposition_file_name("inline;FILENAME=b.gif").as_deref(),
            Some("b.gif")
        );
        assert_eq!(disposition_file_name("inline"), None);
        assert_eq!(disposition_file_name("inline; filename=\"\""), None);
    }

    const AMS_IMGO: &str = "https://eu-api.asm.skype.com/v1/objects/0-weu-d1-abc/views/imgo";

    #[tokio::test]
    async fn a_message_with_images_lists_them_in_amsreferences() {
        let s = MockServer::start().await;
        mount_auth(&s).await;
        crate::test_support::mount_me(&s).await;
        Mock::given(method("POST"))
            .and(path(format!("/v1/users/ME/conversations/{CONV}/messages")))
            .and(body_partial_json(
                json!({"amsreferences": ["0-weu-d1-abc"]}),
            ))
            .respond_with(ResponseTemplate::new(201))
            .expect(1)
            .mount(&s)
            .await;
        let body = crate::RichText(vec![crate::Block::Paragraph(vec![
            Inline::Image {
                url: AMS_IMGO.into(),
            },
            Inline::Image {
                url: AMS_IMGO.into(),
            },
            Inline::Image {
                url: "https://i.mg/1.png".into(),
            },
        ])]);
        let out = crate::Outgoing {
            body,
            ..crate::Outgoing::default()
        };
        api_with(&s, ams_here(&s), None)
            .await
            .send_message(&ConversationId(CONV.into()), &out)
            .await
            .unwrap();
    }
}
