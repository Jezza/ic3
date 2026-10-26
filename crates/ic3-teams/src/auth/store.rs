//! Where the long-lived session (refresh token + tenant + endpoint id) lives
//! between runs.
//!
//! Review Focus #5: whatever state a session file is in — missing, truncated,
//! the wrong shape, unreadable, or readable by other users — `load` answers
//! "no session" (with a warning) rather than failing, so the caller simply
//! asks the user to sign in again. Only I/O failures that are not about the
//! file's own state (a directory in its place, a disk error) are errors.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use serde::{Deserialize, Serialize};

use crate::{Result, Secret};

/// Enough to come back without signing in again (spec §4.3).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredSession {
    /// The OAuth refresh token; rotated on every refresh.
    pub refresh_token: Secret<String>,
    /// Tenant domain or GUID the token was issued by (or `organizations`).
    pub tenant: String,
    /// Random UUID created once per installation; trouter's `epid` and
    /// registration id.
    pub endpoint_id: String,
}

/// Persistence for the session. Loads and saves are rare and tiny, so an
/// implementation may block briefly (the file store uses `std::fs`).
#[async_trait::async_trait]
pub trait TokenStore: Send + Sync + 'static {
    /// The stored session, or `None` when the user must sign in.
    async fn load(&self) -> Result<Option<StoredSession>>;
    /// Replace the stored session.
    async fn save(&self, s: &StoredSession) -> Result<()>;
    /// Forget the session; a no-op when there is none.
    async fn clear(&self) -> Result<()>;
}

/// A session kept as JSON in one file, written with mode `0600` and replaced
/// atomically (temp file + rename) so a crash never leaves a half-written
/// session behind.
#[derive(Clone, Debug)]
pub struct FileTokenStore {
    path: PathBuf,
}

/// Mode bits that would let another user read the session.
const OTHERS_MASK: u32 = 0o077;

impl FileTokenStore {
    /// A store at `path`; parent directories are created on the first save.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        FileTokenStore { path: path.into() }
    }

    /// The file this store reads and writes.
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn temp_path(&self) -> PathBuf {
        self.path.with_extension("json.tmp")
    }

    /// Write `json` to the temp file (always `0600`, even when a stale temp
    /// file with other permissions is lying around) and rename it over
    /// `path`. On failure the temp file is removed, best effort.
    fn write_atomic(&self, json: &[u8]) -> io::Result<()> {
        let tmp = self.temp_path();
        let result = (|| {
            let mut file = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp)?;
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
            file.write_all(json)?;
            file.sync_all()?;
            drop(file);
            fs::rename(&tmp, &self.path)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        result
    }
}

#[async_trait::async_trait]
impl TokenStore for FileTokenStore {
    async fn load(&self) -> Result<Option<StoredSession>> {
        let path = self.path.display();
        match fs::metadata(&self.path) {
            Ok(meta) if meta.is_file() => {
                let mode = meta.permissions().mode() & 0o777;
                if mode & OTHERS_MASK != 0 {
                    tracing::warn!(
                        %path,
                        mode = format_args!("{mode:04o}"),
                        "session file is readable by other users; ignoring it (signing in again rewrites it with mode 0600)"
                    );
                    return Ok(None);
                }
            }
            Ok(_) => {} // not a regular file: let the read report it
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
                tracing::warn!(%path, error = %e, "cannot stat session file; treating as not signed in");
                return Ok(None);
            }
            Err(e) => return Err(e.into()),
        }
        let text = match fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
                tracing::warn!(%path, error = %e, "cannot read session file; treating as not signed in");
                return Ok(None);
            }
            Err(e) => return Err(e.into()),
        };
        match serde_json::from_str::<StoredSession>(&text) {
            Ok(session) => Ok(Some(session)),
            Err(e) => {
                tracing::warn!(%path, error = %e, "session file is corrupt; treating as not signed in");
                Ok(None)
            }
        }
    }

    async fn save(&self, s: &StoredSession) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_vec_pretty(s)?;
        self.write_atomic(&json)?;
        Ok(())
    }

    async fn clear(&self) -> Result<()> {
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

/// A session that lives only as long as the process (tests, and callers that
/// do not want anything on disk).
#[derive(Debug, Default)]
pub struct MemoryTokenStore(Mutex<Option<StoredSession>>);

#[async_trait::async_trait]
impl TokenStore for MemoryTokenStore {
    async fn load(&self) -> Result<Option<StoredSession>> {
        Ok(self
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone())
    }

    async fn save(&self, s: &StoredSession) -> Result<()> {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = Some(s.clone());
        Ok(())
    }

    async fn clear(&self) -> Result<()> {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = None;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Error;

    fn session(rt: &str) -> StoredSession {
        StoredSession {
            refresh_token: Secret::new(rt.into()),
            tenant: "org".into(),
            endpoint_id: "ep".into(),
        }
    }

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[tokio::test]
    async fn file_store_round_trip_is_0600_and_atomic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/session.json");
        let store = FileTokenStore::new(&path);
        assert_eq!(store.path(), path.as_path());
        assert_eq!(store.load().await.unwrap(), None);
        let s = session("rt");
        store.save(&s).await.unwrap();
        assert_eq!(mode(&path), 0o600);
        assert!(
            fs::read_dir(path.parent().unwrap()).unwrap().count() == 1,
            "no temp file left behind"
        );
        assert_eq!(store.load().await.unwrap(), Some(s));
        store.clear().await.unwrap();
        assert_eq!(store.load().await.unwrap(), None);
        store.clear().await.unwrap(); // idempotent
    }

    #[tokio::test]
    async fn save_replaces_an_existing_file_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");
        let store = FileTokenStore::new(&path);
        store.save(&session("one")).await.unwrap();
        store.save(&session("two")).await.unwrap();
        assert_eq!(store.load().await.unwrap(), Some(session("two")));
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn stored_file_is_plain_json_with_the_token_in_it() {
        // Serde on `Secret` is transparent: the file must hold the real token.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");
        FileTokenStore::new(&path)
            .save(&session("rt"))
            .await
            .unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v["refresh_token"], "rt");
        assert_eq!(v["tenant"], "org");
        assert_eq!(v["endpoint_id"], "ep");
    }

    #[tokio::test]
    async fn corrupt_session_file_means_needs_login() {
        // Review Focus #5
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");
        fs::write(&path, "{\"refresh_token\": \"rt\", ").unwrap();
        assert_eq!(FileTokenStore::new(&path).load().await.unwrap(), None);
    }

    #[tokio::test]
    async fn wrong_shape_session_file_means_needs_login() {
        // Review Focus #5: valid JSON, missing fields.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");
        fs::write(&path, "{\"tenant\": \"org\"}").unwrap();
        assert_eq!(FileTokenStore::new(&path).load().await.unwrap(), None);
    }

    #[tokio::test]
    async fn unreadable_session_file_means_needs_login() {
        // Review Focus #5: wrong permissions (cannot read it).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");
        let store = FileTokenStore::new(&path);
        store.save(&session("rt")).await.unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::read(&path).is_ok() {
            return; // root ignores mode bits; nothing to test here
        }
        assert_eq!(store.load().await.unwrap(), None);
        // A fresh login must be able to replace it.
        store.save(&session("new")).await.unwrap();
        assert_eq!(store.load().await.unwrap(), Some(session("new")));
    }

    #[tokio::test]
    async fn too_open_session_file_means_needs_login() {
        // Review Focus #5: wrong permissions (readable by others) — refuse to
        // use it, like ssh does with an over-permissive key.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");
        let store = FileTokenStore::new(&path);
        store.save(&session("rt")).await.unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(store.load().await.unwrap(), None);
        // Saving again restores 0600.
        store.save(&session("new")).await.unwrap();
        assert_eq!(mode(&path), 0o600);
        assert_eq!(store.load().await.unwrap(), Some(session("new")));
    }

    #[tokio::test]
    async fn stale_temp_file_is_replaced_and_ends_up_0600() {
        // A crash between write and rename leaves `session.json.tmp` behind,
        // possibly with wider permissions; the next save must still end 0600.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");
        let tmp = dir.path().join("session.json.tmp");
        fs::write(&tmp, "garbage").unwrap();
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o666)).unwrap();
        let store = FileTokenStore::new(&path);
        store.save(&session("rt")).await.unwrap();
        assert!(!tmp.exists());
        assert_eq!(mode(&path), 0o600);
        assert_eq!(store.load().await.unwrap(), Some(session("rt")));
    }

    #[tokio::test]
    async fn directory_in_place_of_file_is_an_io_error_not_a_panic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");
        fs::create_dir(&path).unwrap();
        assert!(matches!(
            FileTokenStore::new(&path).load().await,
            Err(Error::Io(_))
        ));
    }

    #[tokio::test]
    async fn memory_store() {
        let m = MemoryTokenStore::default();
        assert!(m.load().await.unwrap().is_none());
        m.save(&StoredSession {
            refresh_token: Secret::new("x".into()),
            tenant: "t".into(),
            endpoint_id: "e".into(),
        })
        .await
        .unwrap();
        assert!(m.load().await.unwrap().is_some());
        m.clear().await.unwrap();
        assert!(m.load().await.unwrap().is_none());
    }

    #[test]
    fn debug_output_never_shows_the_refresh_token() {
        let s = session("hunter2");
        let m = MemoryTokenStore::default();
        *m.0.lock().unwrap() = Some(s.clone());
        for text in [format!("{s:?}"), format!("{m:?}")] {
            assert!(!text.contains("hunter2"), "{text}");
            assert!(text.contains("Secret(<redacted>)"), "{text}");
        }
    }
}
