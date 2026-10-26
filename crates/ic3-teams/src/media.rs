//! Chat media: images in AMS (the "async media service") and files shared
//! from OneDrive / SharePoint.
//!
//! This module holds the public types and the pure URL rules; the requests
//! are [`Api::fetch_media`](crate::Api::fetch_media),
//! [`Api::upload_image`](crate::Api::upload_image) and
//! [`Api::upload_file`](crate::Api::upload_file).
//!
//! - An image is an AMS object, `{ams}/v1/objects/{id}/views/{view}`, shown
//!   in a message body as an `<img itemtype="…/AMSImage">`; its URL needs the
//!   Skype token.
//! - A file lives in the sender's OneDrive and is listed in the message's
//!   `properties.files`; [`Attachment::url`](crate::Attachment::url) is its
//!   `objectUrl` (a SharePoint URL), which `fetch_media` downloads through
//!   Microsoft Graph's `/shares` with a Graph token.
//!
//! No token is ever sent to a host outside these rules (`media_source`).

use std::fmt;

/// Default for [`MediaOptions::max_download_bytes`]: 100 MiB.
pub const DEFAULT_MAX_DOWNLOAD_BYTES: u64 = 100 * 1024 * 1024;

/// A downloaded image or file. `Debug` shows the size, not the bytes.
#[derive(Clone, PartialEq, Eq)]
pub struct Media {
    /// The `Content-Type`, when the server named one.
    pub content_type: Option<String>,
    /// The body.
    pub bytes: Vec<u8>,
    /// The file's name, when known (shared files; AMS only via
    /// `Content-Disposition`).
    pub file_name: Option<String>,
}

impl fmt::Debug for Media {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Media")
            .field("content_type", &self.content_type)
            .field("len", &self.bytes.len())
            .field("file_name", &self.file_name)
            .finish()
    }
}

/// How [`Api::upload_file`](crate::Api::upload_file) shares a file with the
/// chat (§9.31): the run-time switch `file_sharing`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FileSharing {
    /// Grant each other member of the chat read access (Graph `invite`,
    /// sign-in required, no e-mail sent). Default.
    #[default]
    Members,
    /// Create an organization-wide view link (Graph `createLink`).
    OrganizationLink,
}

impl FileSharing {
    /// `members` or `organization`; anything else is `None`.
    pub fn parse(s: &str) -> Option<FileSharing> {
        match s {
            "members" => Some(FileSharing::Members),
            "organization" => Some(FileSharing::OrganizationLink),
            _ => None,
        }
    }

    /// The name [`parse`](Self::parse) accepts.
    pub fn as_str(self) -> &'static str {
        match self {
            FileSharing::Members => "members",
            FileSharing::OrganizationLink => "organization",
        }
    }
}

/// Run-time choices for media (part of [`ApiOptions`](crate::ApiOptions)).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MediaOptions {
    /// The largest body [`Api::fetch_media`](crate::Api::fetch_media) reads;
    /// a larger one is `Error::Invalid`. Default 100 MiB.
    pub max_download_bytes: u64,
    /// The AMS base URL images are uploaded to (and one more origin whose
    /// object URLs `fetch_media` accepts); `None` uses `authz`'s
    /// `regionGtms.ams`, else `https://api.asm.skype.com` (§9.27).
    pub ams_base: Option<String>,
    /// Microsoft Graph's base URL; default `https://graph.microsoft.com`.
    pub graph_base: String,
    /// How uploaded files are shared with the chat (§9.31); default
    /// [`FileSharing::Members`].
    pub file_sharing: FileSharing,
}

impl Default for MediaOptions {
    fn default() -> Self {
        MediaOptions {
            max_download_bytes: DEFAULT_MAX_DOWNLOAD_BYTES,
            ams_base: None,
            graph_base: "https://graph.microsoft.com".into(),
            file_sharing: FileSharing::default(),
        }
    }
}

/// The AMS view that is the full-size image (§9.29).
pub(crate) const FULL_SIZE_VIEW: &str = "imgo";

/// An AMS object URL taken apart: `{origin}/v1/objects/{id}[/views/{view}]`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AmsObject {
    /// `scheme://host[:port]`.
    pub origin: String,
    pub id: String,
    pub view: Option<String>,
}

impl AmsObject {
    /// `{origin}/v1/objects/{id}/views/{view}`.
    pub fn view_url(&self, view: &str) -> String {
        object_view_url(&self.origin, &self.id, view)
    }
}

/// `{base}/v1/objects/{id}/views/{view}`.
pub(crate) fn object_view_url(base: &str, id: &str, view: &str) -> String {
    format!(
        "{}/v1/objects/{id}/views/{view}",
        base.trim_end_matches('/')
    )
}

/// Parses an AMS object URL by its path alone (any host: this decides
/// markup, never where a token goes). The query is dropped.
/// The AMS object ids of the full-size images in a body, in order, for a
/// message's `amsreferences` (Teams lists them there; observed 2026-10-23).
pub(crate) fn ams_references(body: &crate::RichText) -> Vec<String> {
    let mut out = Vec::new();
    crate::format::visit_images(body, &mut |url| {
        if let Some(o) = parse_ams_url(url).filter(|o| o.view.as_deref() == Some(FULL_SIZE_VIEW))
            && !out.contains(&o.id)
        {
            out.push(o.id);
        }
    });
    out
}

pub(crate) fn parse_ams_url(url: &str) -> Option<AmsObject> {
    let u = url::Url::parse(url).ok()?;
    if !matches!(u.scheme(), "https" | "http") || u.host_str().is_none() {
        return None;
    }
    let segs: Vec<&str> = u.path_segments()?.collect();
    let (id, view) = match segs.as_slice() {
        ["v1", "objects", id] => (*id, None),
        ["v1", "objects", id, "views", view] if !view.is_empty() => (*id, Some(*view)),
        _ => return None,
    };
    if id.is_empty() {
        return None;
    }
    Some(AmsObject {
        origin: u.origin().ascii_serialization(),
        id: id.to_string(),
        view: view.map(str::to_string),
    })
}

/// The full-size view of an AMS image URL (a thumbnail view becomes
/// [`FULL_SIZE_VIEW`]); `None` when `url` is not an AMS object URL.
pub(crate) fn full_size_image_url(url: &str) -> Option<String> {
    parse_ams_url(url).map(|o| o.view_url(FULL_SIZE_VIEW))
}

/// The Teams markup for an image already in AMS, when `url` is a full-size
/// AMS object URL (what `upload_image` returns and `html_in` keeps); `None`
/// for any other image, which stays a plain `<img src>`.
// UNVERIFIED(§9.29): Teams web's markup (observed 2026-10-23: `src` = `href` = `target-src` = the `imgo` view, `itemtype="http://schema.skype.com/AMSImage"`, `id` = `itemid` = the object id, `alt="image"`) minus what we do not know here: Teams puts the file type in `itemscope` (`"png"`) and sends `width` / `height`; that Teams shows the image without them is assumed.
pub(crate) fn ams_image_html(url: &str) -> Option<String> {
    let o = parse_ams_url(url)?;
    if o.view.as_deref() != Some(FULL_SIZE_VIEW) {
        return None;
    }
    let src = attr(url);
    let id = attr(&o.id);
    Some(format!(
        "<img src=\"{src}\" itemscope=\"\" itemtype=\"http://schema.skype.com/AMSImage\" alt=\"image\" id=\"{id}\" itemid=\"{id}\" href=\"{src}\" target-src=\"{src}\">"
    ))
}

fn attr(s: &str) -> std::borrow::Cow<'_, str> {
    html_escape::encode_double_quoted_attribute(s)
}

/// Where [`Api::fetch_media`](crate::Api::fetch_media) may send a token for
/// a URL.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MediaSource {
    /// An AMS object: the Skype token.
    Ams,
    /// A SharePoint / OneDrive for Business item: a Graph token, sent to
    /// Graph only (the item's URL is never fetched itself).
    SharePoint,
}

/// Classifies a media URL, or `None` to refuse it. AMS: an
/// `/v1/objects/{id}…` path on `ams_origins` (the configured and `authz`
/// AMS bases), or over https on `*.asm.skype.com` or
/// `*.asyncgw.teams.microsoft.com`. SharePoint: https on `*.sharepoint.com`.
/// Never a URL with credentials or, on the suffix rules, an explicit port.
// UNVERIFIED(§9.27): chat images live only on `*.asm.skype.com` and `*.asyncgw.teams.microsoft.com` (plus the AMS base `authz` names), and shared chat files only on `*.sharepoint.com` (commercial cloud; no sovereign clouds, no consumer OneDrive).
pub(crate) fn media_source(url: &url::Url, ams_origins: &[String]) -> Option<MediaSource> {
    if !url.username().is_empty() || url.password().is_some() {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    let https_default_port = url.scheme() == "https" && url.port().is_none();
    let ams_path = parse_ams_url(url.as_str()).is_some();
    let origin = url.origin().ascii_serialization();
    if ams_path
        && (ams_origins
            .iter()
            .any(|o| o.trim_end_matches('/') == origin)
            || (https_default_port
                && (host.ends_with(".asm.skype.com")
                    || host.ends_with(".asyncgw.teams.microsoft.com"))))
    {
        return Some(MediaSource::Ams);
    }
    if https_default_port && host.ends_with(".sharepoint.com") {
        return Some(MediaSource::SharePoint);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const AMS: &str = "https://eu-api.asm.skype.com/v1/objects/0-weu-d1-abc/views/imgo";

    #[test]
    fn ams_urls_parse_by_path() {
        let o = parse_ams_url(AMS).unwrap();
        assert_eq!(o.origin, "https://eu-api.asm.skype.com");
        assert_eq!(o.id, "0-weu-d1-abc");
        assert_eq!(o.view.as_deref(), Some("imgo"));
        let o = parse_ams_url("http://127.0.0.1:9/v1/objects/x").unwrap();
        assert_eq!((o.origin.as_str(), o.view), ("http://127.0.0.1:9", None));
        for bad in [
            "https://h/v1/objects/",
            "https://h/v1/objects/x/views/",
            "https://h/v1/objects/x/content/imgpsh",
            "https://h/v2/objects/x",
            "ftp://h/v1/objects/x",
            "not a url",
        ] {
            assert_eq!(parse_ams_url(bad), None, "{bad}");
        }
    }

    #[test]
    fn thumbnails_become_the_full_size_view() {
        assert_eq!(
            full_size_image_url(
                "https://eu-api.asm.skype.com/v1/objects/0-weu-d1-abc/views/imgpsh_mobile_save_anon?x=1"
            )
            .as_deref(),
            Some(AMS)
        );
        assert_eq!(full_size_image_url(AMS).as_deref(), Some(AMS));
        assert_eq!(full_size_image_url("https://i.mg/1.png"), None);
    }

    #[test]
    fn ams_markup_only_for_full_size_object_urls() {
        assert_eq!(
            ams_image_html(AMS).unwrap(),
            format!(
                "<img src=\"{AMS}\" itemscope=\"\" itemtype=\"http://schema.skype.com/AMSImage\" alt=\"image\" id=\"0-weu-d1-abc\" itemid=\"0-weu-d1-abc\" href=\"{AMS}\" target-src=\"{AMS}\">"
            )
        );
        assert_eq!(
            ams_image_html(
                "https://eu-api.asm.skype.com/v1/objects/a/views/imgpsh_mobile_save_anon"
            ),
            None
        );
        assert_eq!(ams_image_html("https://i.mg/1.png"), None);
    }

    /// `html_in` keeps the full-size view of an AMSImage, and only of one.
    #[test]
    fn ams_markup_parses_to_the_full_size_url() {
        use crate::format::{Block, Inline, from_teams_html};
        let html = "<p><img src=\"https://eu-api.asm.skype.com/v1/objects/0-weu-d1-abc/views/imgpsh_mobile_save_anon\" itemscope=\"\" itemtype=\"http://schema.skype.com/AMSImage\" width=\"250\" height=\"200\" id=\"x\" itemid=\"0-weu-d1-abc\"></p>\
                    <p><img src=\"https://eu-api.asm.skype.com/v1/objects/y/views/imgt1\"></p>\
                    <p><img itemtype=\"http://schema.skype.com/AMSImage\" src=\"https://i.mg/1.png\"></p>";
        let image = |url: &str| Block::Paragraph(vec![Inline::Image { url: url.into() }]);
        assert_eq!(
            from_teams_html(html).0,
            vec![
                image(AMS),
                image("https://eu-api.asm.skype.com/v1/objects/y/views/imgt1"),
                image("https://i.mg/1.png"),
            ]
        );
    }

    #[test]
    fn media_hosts_are_an_allow_list() {
        let src = |u: &str, extra: &[&str]| {
            let extra: Vec<String> = extra.iter().map(|s| s.to_string()).collect();
            media_source(&url::Url::parse(u).unwrap(), &extra)
        };
        assert_eq!(src(AMS, &[]), Some(MediaSource::Ams));
        assert_eq!(
            src(
                "https://emea.asyncgw.teams.microsoft.com/v1/objects/x/views/imgo",
                &[]
            ),
            Some(MediaSource::Ams)
        );
        assert_eq!(
            src(
                "http://127.0.0.1:9/v1/objects/x/views/imgo",
                &["http://127.0.0.1:9/"]
            ),
            Some(MediaSource::Ams)
        );
        assert_eq!(
            src(
                "https://contoso-my.sharepoint.com/personal/a_contoso_com/Documents/Microsoft Teams Chat Files/a.pdf",
                &[]
            ),
            Some(MediaSource::SharePoint)
        );
        for refused in [
            "https://evil.example/v1/objects/x/views/imgo",
            "https://asm.skype.com.evil.example/v1/objects/x/views/imgo",
            "http://eu-api.asm.skype.com/v1/objects/x/views/imgo",
            "https://eu-api.asm.skype.com:8443/v1/objects/x/views/imgo",
            "https://eu-api.asm.skype.com/v1/users/ME/conversations",
            "https://u:p@eu-api.asm.skype.com/v1/objects/x/views/imgo",
            "http://contoso.sharepoint.com/a.pdf",
            "https://sharepoint.com.evil.example/a.pdf",
            "https://graph.microsoft.com/v1.0/me",
            "http://127.0.0.1:9/v1/objects/x/views/imgo",
        ] {
            assert_eq!(src(refused, &[]), None, "{refused}");
        }
    }

    #[test]
    fn media_debug_hides_the_bytes_and_sharing_names_round_trip() {
        let m = Media {
            content_type: Some("image/png".into()),
            bytes: b"secret-ish".to_vec(),
            file_name: None,
        };
        let d = format!("{m:?}");
        assert!(d.contains("len: 10") && !d.contains("115"), "{d}");
        for s in [FileSharing::Members, FileSharing::OrganizationLink] {
            assert_eq!(FileSharing::parse(s.as_str()), Some(s));
        }
        assert_eq!(FileSharing::parse("anyone"), None);
        assert_eq!(MediaOptions::default().max_download_bytes, 100 << 20);
    }
}
