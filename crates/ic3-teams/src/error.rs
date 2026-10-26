//! The single error type for the crate.

/// Every failure `ic3-teams` can report.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The session is gone for good; the caller must sign in again.
    #[error("re-authentication required")]
    ReauthRequired,
    /// A non-success HTTP status with the response body.
    #[error("http {status}: {body}")]
    Http { status: u16, body: String },
    /// The server replied with something this crate could not understand.
    #[error("protocol error: {0}")]
    Protocol(String),
    /// The request was refused locally, before any network: it cannot work
    /// (e.g. editing someone else's message).
    #[error("invalid request: {0}")]
    Invalid(&'static str),
    /// Transport-level failure (DNS, TLS, connection, socket).
    #[error("network error: {0}")]
    Network(String),
    /// Local I/O failure.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

impl From<reqwest::Error> for Error {
    fn from(e: reqwest::Error) -> Self {
        // `without_url` first: URLs can carry tokens in their query strings.
        // reqwest's own message ("error sending request") hides the cause, so
        // the source chain is appended; it carries no URL (DNS, TLS, connect,
        // timeout and socket errors only).
        let e = e.without_url();
        let mut msg = e.to_string();
        let mut source = std::error::Error::source(&e);
        while let Some(cause) = source {
            let cause_msg = cause.to_string();
            if !msg.ends_with(&cause_msg) {
                msg.push_str(": ");
                msg.push_str(&cause_msg);
            }
            source = cause.source();
        }
        Error::Network(msg)
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::Protocol(e.to_string())
    }
}

/// Crate-wide result alias.
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::Error;
    #[test]
    fn display_shapes() {
        assert_eq!(
            Error::ReauthRequired.to_string(),
            "re-authentication required"
        );
        assert_eq!(
            Error::Http {
                status: 404,
                body: "nope".into()
            }
            .to_string(),
            "http 404: nope"
        );
        let e: Error = serde_json::from_str::<serde_json::Value>("{")
            .unwrap_err()
            .into();
        assert!(matches!(e, Error::Protocol(_)));
        assert_eq!(
            Error::Invalid("not your message").to_string(),
            "invalid request: not your message"
        );
    }

    /// The cause chain survives (`error sending request` alone says nothing)
    /// while the URL, which may carry secrets in its query, does not.
    #[tokio::test]
    async fn network_errors_keep_their_cause_but_not_the_url() {
        let e: Error = reqwest::Client::new()
            .get("http://127.0.0.1:1/x?sig=secret-q")
            .send()
            .await
            .unwrap_err()
            .into();
        let Error::Network(m) = e else {
            panic!("{e:?}")
        };
        assert!(m.to_ascii_lowercase().contains("connection refused"), "{m}");
        assert!(
            !m.contains("secret-q") && !m.contains("127.0.0.1:1/x"),
            "{m}"
        );
    }
}
