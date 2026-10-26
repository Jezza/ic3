//! Authentication: the verified constants (spec §2), the endpoint set, the
//! session store, the raw OAuth / authz wire calls and the token manager
//! that keeps the access and Skype tokens fresh.
//!
//! Nothing here prints or prompts: device-code details are returned to the
//! caller for display.

pub mod manager;
pub mod oauth;
pub mod store;

pub use manager::{RefreshKind, TokenHandle, TokenSnapshot, TokenState};
pub use oauth::DeviceCodePrompt;
pub use store::{FileTokenStore, MemoryTokenStore, StoredSession, TokenStore};

/// The Teams first-party public client id (spec §2, verified).
pub const CLIENT_ID: &str = "1fec8e78-bce4-4aaf-ab1b-5451cc387264";
/// The v1 `resource` the access token is issued for (spec §2, verified).
pub const RESOURCE: &str = "https://api.spaces.skype.com";
/// The v2 `scope` used when refreshing (spec §2, verified).
pub const SCOPE: &str = "https://api.spaces.skype.com/.default openid profile offline_access";

/// The v2 `scope` of the CSA token (spec A.2), granted from the same refresh
/// token as [`SCOPE`].
// UNVERIFIED(§9.12): that this scope is grantable from our refresh token (client id `CLIENT_ID`) and that CSA accepts the token.
pub const CSA_SCOPE: &str =
    "https://chatsvcagg.teams.microsoft.com/.default openid profile offline_access";

/// The v2 `scope` of the Microsoft Graph token (shared files: download,
/// OneDrive upload and sharing), granted lazily from the same refresh token
/// as [`SCOPE`], like [`CSA_SCOPE`].
// UNVERIFIED(§9.25): that this scope is grantable from our refresh token (client id `CLIENT_ID`) and that Graph accepts the token for `/shares`, `/me/drive` upload, `invite` and `createLink`.
pub const GRAPH_SCOPE: &str = "https://graph.microsoft.com/.default openid profile offline_access";

/// Which token CSA requests (`/api/csa/…`) carry: the run-time switch
/// `csa_audience` (spec A.2).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CsaAudience {
    /// A separate, lazily granted token for [`CSA_SCOPE`] (default).
    #[default]
    Chatsvcagg,
    /// No extra grant: CSA requests carry the main access token.
    Spaces,
}

impl CsaAudience {
    /// `chatsvcagg` or `spaces`; anything else is `None`.
    pub fn parse(s: &str) -> Option<CsaAudience> {
        match s {
            "chatsvcagg" => Some(CsaAudience::Chatsvcagg),
            "spaces" => Some(CsaAudience::Spaces),
            _ => None,
        }
    }

    /// The name [`parse`](Self::parse) accepts.
    pub fn as_str(self) -> &'static str {
        match self {
            CsaAudience::Chatsvcagg => "chatsvcagg",
            CsaAudience::Spaces => "spaces",
        }
    }
}
/// The tenant used when the caller does not name one (spec §4.3).
pub const DEFAULT_TENANT: &str = "organizations";
/// Sent on every request so the services treat us like the desktop client.
pub const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36 Edg/126.0.0.0 Teams/24165.1410.2974.6689/49";

/// Base URLs of the three services, without trailing slashes.
///
/// Injectable so tests can point every service at one mock server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoints {
    /// Azure AD: device code, token and refresh.
    pub login: String,
    /// Teams web: `authz` (Skype token) and the registrar.
    pub teams: String,
    /// Trouter: the push-notification session service.
    pub trouter: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Endpoints {
            login: "https://login.microsoftonline.com".into(),
            teams: "https://teams.microsoft.com".into(),
            trouter: "https://go.trouter.teams.microsoft.com".into(),
        }
    }
}

impl Endpoints {
    /// Every service at `base` (for wiremock).
    pub fn all(base: &str) -> Endpoints {
        let base = base.trim_end_matches('/').to_string();
        Endpoints {
            login: base.clone(),
            teams: base.clone(),
            trouter: base,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_the_real_hosts() {
        let ep = Endpoints::default();
        assert_eq!(ep.login, "https://login.microsoftonline.com");
        assert_eq!(ep.teams, "https://teams.microsoft.com");
        assert_eq!(ep.trouter, "https://go.trouter.teams.microsoft.com");
    }

    #[test]
    fn all_points_everything_at_base_without_trailing_slash() {
        let ep = Endpoints::all("http://127.0.0.1:1234/");
        assert_eq!(ep.login, "http://127.0.0.1:1234");
        assert_eq!(ep.teams, "http://127.0.0.1:1234");
        assert_eq!(ep.trouter, "http://127.0.0.1:1234");
    }

    #[test]
    fn csa_audience_names_round_trip() {
        assert_eq!(CsaAudience::default(), CsaAudience::Chatsvcagg);
        for a in [CsaAudience::Chatsvcagg, CsaAudience::Spaces] {
            assert_eq!(CsaAudience::parse(a.as_str()), Some(a));
        }
        assert_eq!(CsaAudience::parse("Spaces"), None);
    }
}
