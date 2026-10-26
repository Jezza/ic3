//! Links for Teams images and files, and uploads waiting to be sent.
//!
//! Teams media sits behind the user's sign-in, so a browser cannot load it.
//! Every Teams URL shown to a client is replaced by a link on the gateway's
//! own web endpoint: `{base}/media/{token}/{file name}`. The token is random
//! and is the only thing protecting the file; the file name keeps its
//! extension so clients show images inline. Files a client uploads
//! (`soju.im/FILEHOST`) are kept here under `{base}/upload/{token}/{name}`
//! until a message containing that link is sent, and for a while after.

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use rand::Rng;

/// How long an upload is kept.
const UPLOAD_TTL: Duration = Duration::from_secs(24 * 3600);
/// Most remote links remembered; the oldest go first.
const MAX_LINKS: usize = 100_000;

/// A Teams URL a media link stands for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Remote {
    pub url: String,
    pub name: String,
}

/// A file a client uploaded.
#[derive(Clone, Debug)]
pub struct Upload {
    pub name: String,
    pub content_type: String,
    pub bytes: Vec<u8>,
    created: Instant,
}

#[derive(Debug, Default)]
struct Inner {
    remote: HashMap<String, Remote>,
    by_url: HashMap<String, String>,
    order: std::collections::VecDeque<String>,
    uploads: HashMap<String, Upload>,
}

/// The shared table; the gateway adds links, the web endpoint reads them.
#[derive(Debug)]
pub struct MediaTable {
    /// The public base URL, without a trailing slash; `None` serves nothing
    /// and shows Teams URLs as they are.
    base: Option<String>,
    inner: Mutex<Inner>,
}

impl MediaTable {
    pub fn new(base: Option<String>) -> MediaTable {
        MediaTable {
            base: base.map(|b| b.trim_end_matches('/').to_string()),
            inner: Mutex::new(Inner::default()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Whether links are served at all.
    pub fn enabled(&self) -> bool {
        self.base.is_some()
    }

    /// The upload URL to advertise as `soju.im/FILEHOST`.
    pub fn filehost_url(&self) -> Option<String> {
        self.base.as_ref().map(|b| format!("{b}/upload"))
    }

    /// The link to show for a Teams URL; the same URL always gets the same
    /// link. `name` is the file name when known.
    pub fn link_for(&self, url: &str, name: Option<&str>) -> String {
        let Some(base) = &self.base else {
            return url.to_string();
        };
        let mut inner = self.lock();
        if let Some(token) = inner.by_url.get(url) {
            let name = &inner.remote[token].name;
            return format!("{base}/media/{token}/{}", encode_name(name));
        }
        let name = file_name(url, name);
        let token = new_token();
        inner.by_url.insert(url.to_string(), token.clone());
        inner.remote.insert(
            token.clone(),
            Remote {
                url: url.to_string(),
                name: name.clone(),
            },
        );
        inner.order.push_back(token.clone());
        while inner.order.len() > MAX_LINKS {
            if let Some(old) = inner.order.pop_front()
                && let Some(r) = inner.remote.remove(&old)
            {
                inner.by_url.remove(&r.url);
            }
        }
        format!("{base}/media/{token}/{}", encode_name(&name))
    }

    /// The Teams URL behind a media token.
    pub fn remote(&self, token: &str) -> Option<Remote> {
        self.lock().remote.get(token).cloned()
    }

    /// Keeps an upload and returns its link.
    pub fn add_upload(&self, name: &str, content_type: &str, bytes: Vec<u8>) -> Option<String> {
        let base = self.base.as_ref()?;
        let token = new_token();
        let name = sanitize_name(name);
        let mut inner = self.lock();
        inner
            .uploads
            .retain(|_, u| u.created.elapsed() < UPLOAD_TTL);
        inner.uploads.insert(
            token.clone(),
            Upload {
                name: name.clone(),
                content_type: content_type.to_string(),
                bytes,
                created: Instant::now(),
            },
        );
        Some(format!("{base}/upload/{token}/{}", encode_name(&name)))
    }

    /// An upload by token.
    pub fn upload(&self, token: &str) -> Option<Upload> {
        self.lock().uploads.get(token).cloned()
    }

    /// The upload a link in a message points to, if it is one of ours.
    pub fn upload_for_link(&self, link: &str) -> Option<(String, Upload)> {
        let base = self.base.as_ref()?;
        let rest = link.strip_prefix(base.as_str())?.strip_prefix("/upload/")?;
        let token = rest.split('/').next()?;
        self.upload(token).map(|u| (token.to_string(), u))
    }
}

fn new_token() -> String {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut rng = rand::rng();
    (0..26)
        .map(|_| ALPHABET[rng.random_range(0..ALPHABET.len())] as char)
        .collect()
}

/// The file name for a link: the given one, else the URL's last path
/// segment; Teams image objects (`…/views/imgo`) become `image.png` so
/// clients treat them as images.
fn file_name(url: &str, name: Option<&str>) -> String {
    if let Some(n) = name.map(sanitize_name).filter(|n| !n.is_empty()) {
        return n;
    }
    let path = url.split(['?', '#']).next().unwrap_or(url);
    if path.contains("/views/") {
        return "image.png".into();
    }
    let last = path.rsplit('/').next().unwrap_or("");
    let n = sanitize_name(&percent_decode(last));
    if n.is_empty() { "file".into() } else { n }
}

/// A file name safe in a URL path segment and a `Content-Disposition`.
pub fn sanitize_name(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_control() || "/\\\"".contains(c) {
                '_'
            } else {
                c
            }
        })
        .collect::<String>()
        .trim()
        .chars()
        .take(120)
        .collect()
}

/// Percent-encodes a name for a path segment.
fn encode_name(name: &str) -> String {
    let mut out = String::new();
    for b in name.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(hex) = s.get(i + 1..i + 3)
            && let Ok(v) = u8::from_str_radix(hex, 16)
        {
            out.push(v);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_are_stable_and_resolvable() {
        let t = MediaTable::new(Some("https://gw.example/".into()));
        let url = "https://eu-api.asm.skype.com/v1/objects/0-abc/views/imgo";
        let a = t.link_for(url, None);
        assert!(a.starts_with("https://gw.example/media/"), "{a}");
        assert!(a.ends_with("/image.png"), "{a}");
        assert_eq!(t.link_for(url, None), a);
        let token = a.split('/').nth(4).unwrap();
        assert_eq!(t.remote(token).unwrap().url, url);
    }

    #[test]
    fn file_names_come_from_the_url_or_the_caller() {
        let t = MediaTable::new(Some("https://gw".into()));
        let l = t.link_for(
            "https://x.sharepoint.com/a/Quarterly%20Report.pdf?web=1",
            None,
        );
        assert!(l.ends_with("/Quarterly%20Report.pdf"), "{l}");
        let l = t.link_for("https://x/y", Some("a/b \"c\".txt"));
        assert!(l.ends_with("/a_b%20_c_.txt"), "{l}");
    }

    #[test]
    fn disabled_table_passes_urls_through() {
        let t = MediaTable::new(None);
        assert_eq!(t.link_for("https://x/y.png", None), "https://x/y.png");
        assert_eq!(t.add_upload("a", "text/plain", vec![]), None);
        assert!(t.filehost_url().is_none());
    }

    #[test]
    fn uploads_are_found_by_their_link() {
        let t = MediaTable::new(Some("https://gw".into()));
        let link = t
            .add_upload("cat.jpg", "image/jpeg", vec![1, 2, 3])
            .unwrap();
        let (_, u) = t.upload_for_link(&link).unwrap();
        assert_eq!((u.name.as_str(), u.bytes.len()), ("cat.jpg", 3));
        assert!(t.upload_for_link("https://gw/media/x/y").is_none());
        assert!(t.upload_for_link("https://other/upload/x/y").is_none());
    }
}
