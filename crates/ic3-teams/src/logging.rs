//! Redacted `tracing` output (spec §4.1, §4.11).
//!
//! The crate never installs a subscriber; a binary wires these in:
//!
//! ```no_run
//! use ic3_teams::logging::{RedactingMakeWriter, filter_directives};
//! use tracing_subscriber::EnvFilter;
//!
//! let user = std::env::var("RUST_LOG").unwrap_or_else(|_| "info".into());
//! tracing_subscriber::fmt()
//!     .with_env_filter(EnvFilter::new(filter_directives(&user)))
//!     .with_writer(RedactingMakeWriter::new(std::io::stderr))
//!     // Colour codes between a field name and its value would hide a
//!     // secret from the redaction patterns.
//!     .with_ansi(false)
//!     .init();
//! ```
//!
//! [`filter_directives`] caps the HTTP/websocket stacks at `info` (their
//! debug output carries URLs and headers, and the websocket upgrade request
//! carries `X-Skypetoken`) unless the user names them; [`RedactingMakeWriter`]
//! runs every output line through [`redact_text`] before it reaches the
//! underlying writer.

use std::fmt;
use std::io::{self, Write};

use tracing_subscriber::fmt::MakeWriter;

use crate::redact::redact_text;

/// Targets whose `debug`/`trace` output can carry secrets (URLs with
/// trouter credentials, auth headers, the websocket upgrade request).
const CAPPED_TARGETS: [&str; 7] = [
    "reqwest",
    "hyper",
    "hyper_util",
    "h2",
    "rustls",
    "tungstenite",
    "tokio_tungstenite",
];

/// `base` (an `EnvFilter` directive string, e.g. from `RUST_LOG`) with
/// `{target}=info` appended for each capped HTTP/websocket target the user
/// did not name themselves (spec §4.11).
///
/// A directive names a target when its target part (before any `[span]` or
/// `=level`) equals it exactly, so `debug,reqwest=trace` keeps reqwest at
/// trace. Directives for sub-targets (`hyper::proto=debug`) do not lift the
/// cap on the rest of the crate.
pub fn filter_directives(base: &str) -> String {
    let mut parts: Vec<&str> = base
        .split(',')
        .map(str::trim)
        .filter(|d| !d.is_empty())
        .collect();
    let named: Vec<&str> = parts.iter().map(|d| directive_target(d)).collect();
    let caps: Vec<String> = CAPPED_TARGETS
        .iter()
        .filter(|t| !named.contains(t))
        .map(|t| format!("{t}=info"))
        .collect();
    parts.extend(caps.iter().map(String::as_str));
    parts.join(",")
}

/// The target part of one directive (`target[span]=level`); for a bare
/// level (`debug`) this is the level itself, which never equals a target.
fn directive_target(d: &str) -> &str {
    let end = d.find(['[', '=']).unwrap_or(d.len());
    d[..end].trim()
}

/// A writer that redacts each complete line with [`redact_text`] before
/// passing it on.
///
/// Bytes are buffered until `\n`, so a secret split across writes is still
/// seen whole. A trailing partial line is written (redacted) on
/// [`flush`](Write::flush) and on drop.
pub struct RedactingWriter<W: Write> {
    inner: W,
    pending: Vec<u8>,
}

impl<W: Write> RedactingWriter<W> {
    /// Wraps `inner`.
    pub fn new(inner: W) -> Self {
        RedactingWriter {
            inner,
            pending: Vec::new(),
        }
    }

    fn write_line(&mut self, line: &[u8], newline: bool) -> io::Result<()> {
        let mut out = redact_text(&String::from_utf8_lossy(line));
        if newline {
            out.push('\n');
        }
        self.inner.write_all(out.as_bytes())
    }

    fn write_pending(&mut self) -> io::Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let line = std::mem::take(&mut self.pending);
        self.write_line(&line, false)
    }
}

impl<W: Write> Write for RedactingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.pending.extend_from_slice(buf);
        if let Some(last) = self.pending.iter().rposition(|&b| b == b'\n') {
            let rest = self.pending.split_off(last + 1);
            let complete = std::mem::replace(&mut self.pending, rest);
            for line in complete[..last].split(|&b| b == b'\n') {
                self.write_line(line, true)?;
            }
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.write_pending()?;
        self.inner.flush()
    }
}

impl<W: Write> Drop for RedactingWriter<W> {
    fn drop(&mut self) {
        let _ = self.flush();
    }
}

impl<W: Write> fmt::Debug for RedactingWriter<W> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RedactingWriter")
            .field("pending_bytes", &self.pending.len())
            .finish_non_exhaustive()
    }
}

/// A [`MakeWriter`] wrapping every writer of `M` in a [`RedactingWriter`],
/// for `tracing_subscriber::fmt().with_writer(..)`.
#[derive(Clone)]
pub struct RedactingMakeWriter<M> {
    inner: M,
}

impl<M> RedactingMakeWriter<M> {
    /// Wraps `inner`.
    pub fn new(inner: M) -> Self {
        RedactingMakeWriter { inner }
    }
}

impl<'a, M: MakeWriter<'a>> MakeWriter<'a> for RedactingMakeWriter<M> {
    type Writer = RedactingWriter<M::Writer>;

    fn make_writer(&'a self) -> Self::Writer {
        RedactingWriter::new(self.inner.make_writer())
    }

    fn make_writer_for(&'a self, meta: &tracing::Metadata<'_>) -> Self::Writer {
        RedactingWriter::new(self.inner.make_writer_for(meta))
    }
}

impl<M> fmt::Debug for RedactingMakeWriter<M> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RedactingMakeWriter")
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use std::io::{self, Write};
    use std::sync::{Arc, Mutex};

    use super::*;

    /// A writer appending to a shared buffer.
    struct Shared(Arc<Mutex<Vec<u8>>>);

    impl Write for Shared {
        fn write(&mut self, b: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// A `MakeWriter` handing out [`Shared`] writers over one buffer.
    #[derive(Clone)]
    struct Mw(Arc<Mutex<Vec<u8>>>);

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Mw {
        type Writer = Shared;
        fn make_writer(&'a self) -> Shared {
            Shared(self.0.clone())
        }
    }

    fn text(buf: &Arc<Mutex<Vec<u8>>>) -> String {
        String::from_utf8(buf.lock().unwrap().clone()).unwrap()
    }

    #[test]
    fn writer_redacts_per_line_and_handles_split_writes() {
        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let mut w = RedactingWriter::new(Shared(buf.clone()));
        w.write_all(b"GET https://x/socket.io/1/?sr=1&si").unwrap();
        w.write_all(b"g=2 Authorization: Bearer abc\nsecond X-Skypetoken: zz\n")
            .unwrap();
        w.flush().unwrap();
        let out = text(&buf);
        assert!(
            !out.contains("sr=1") && !out.contains("abc") && !out.contains("zz"),
            "{out}"
        );
        assert_eq!(out.lines().count(), 2);
    }

    #[test]
    fn a_partial_line_is_written_redacted_on_flush_and_on_drop() {
        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let mut w = RedactingWriter::new(Shared(buf.clone()));
        w.write_all(b"X-Skypetoken: one").unwrap();
        assert_eq!(text(&buf), "", "nothing written before the line ends");
        w.flush().unwrap();
        let out = text(&buf);
        assert!(
            out.contains("X-Skypetoken") && !out.contains("one"),
            "{out}"
        );

        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        {
            let mut w = RedactingWriter::new(Shared(buf.clone()));
            w.write_all(b"Authorization: Bearer two").unwrap();
        }
        let out = text(&buf);
        assert!(
            out.contains("Authorization") && !out.contains("two"),
            "{out}"
        );
    }

    #[test]
    fn filter_directives_cap_noisy_crates_unless_named() {
        let d = filter_directives("debug");
        for c in [
            "reqwest=info",
            "hyper=info",
            "hyper_util=info",
            "h2=info",
            "rustls=info",
            "tungstenite=info",
            "tokio_tungstenite=info",
        ] {
            assert!(d.contains(c), "{d}");
        }
        assert!(d.starts_with("debug,"));
        let d2 = filter_directives("debug,reqwest=trace");
        assert!(d2.contains("reqwest=trace") && !d2.contains("reqwest=info"));
    }

    #[test]
    fn filter_directives_tolerate_spaces_empty_base_and_bare_targets() {
        let d = filter_directives("");
        assert!(!d.starts_with(','), "{d}");
        assert!(d.contains("reqwest=info"), "{d}");
        // A bare target directive enables it explicitly (at trace).
        let d = filter_directives("info, tungstenite");
        assert!(!d.contains("tungstenite=info,"), "{d}");
        assert!(d.contains("tokio_tungstenite=info"), "{d}");
        // A span-filtered directive still names the target.
        let d = filter_directives("hyper[conn]=debug");
        assert!(!d.contains(",hyper=info"), "{d}");
        assert!(tracing_subscriber::EnvFilter::try_new(&d).is_ok(), "{d}");
    }

    #[test]
    fn tracing_output_through_make_writer_is_redacted() {
        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let sub = tracing_subscriber::fmt()
            .with_writer(RedactingMakeWriter::new(Mw(buf.clone())))
            .with_ansi(false)
            .finish();
        tracing::subscriber::with_default(sub, || {
            tracing::info!(url = "https://x/socket.io/1/?sig=secret", "x");
        });
        let out = text(&buf);
        assert!(
            out.contains("socket.io/1/?<redacted>") && !out.contains("secret"),
            "{out}"
        );
    }

    /// Ruling (Task 17): the websocket upgrade request carries
    /// `X-Skypetoken`, and reqwest/hyper debug lines carry URLs and headers.
    /// At `debug` those targets are capped at `info`; what does get through
    /// is redacted.
    #[test]
    fn noisy_targets_are_capped_at_info_and_what_passes_is_redacted() {
        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let sub = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::new(filter_directives(
                "debug",
            )))
            .with_writer(RedactingMakeWriter::new(Mw(buf.clone())))
            .with_ansi(false)
            .finish();
        tracing::subscriber::with_default(sub, || {
            tracing::debug!(target: "tungstenite::handshake", "upgrade X-Skypetoken: tk-1");
            tracing::debug!(target: "tokio_tungstenite", "upgrade X-Skypetoken: tk-2");
            tracing::debug!(target: "reqwest::connect", "GET https://x/?sig=tk-3");
            tracing::debug!(target: "hyper::proto", "Authorization: Bearer tk-4");
            tracing::debug!(target: "hyper_util::client", "Authorization: Bearer tk-5");
            tracing::debug!(target: "ic3_teams::kept", "own debug line");
            tracing::info!(target: "tungstenite::handshake", "upgrade X-Skypetoken: tk-6");
        });
        let out = text(&buf);
        for n in 1..=5 {
            assert!(!out.contains(&format!("tk-{n}")), "{out}");
        }
        assert!(
            out.contains("own debug line"),
            "the base level applies: {out}"
        );
        assert!(
            out.contains("X-Skypetoken") && !out.contains("tk-6"),
            "an info line still gets through, redacted: {out}"
        );
        assert_eq!(out.lines().count(), 2, "{out}");
    }
}
