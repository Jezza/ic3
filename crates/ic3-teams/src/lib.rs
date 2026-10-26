//! `ic3-teams` — an **unofficial** Rust client for Microsoft Teams chat.
//!
//! This crate talks to the internal, undocumented "IC3"/chatsvc API that the
//! official Teams clients use. Microsoft does not support it; **it can break at
//! any time without notice.** Use at your own risk. Versioned `0.x` for that reason.
//!
//! The crate never prints and has no UI; anything a user must see (e.g. a device
//! code) is returned to the caller.
//!
//! Start with [`Client`]: build it (no network), sign in with
//! [`Client::start_device_login`] if [`Client::needs_login`], then use
//! [`Client::api`] for REST and [`Client::connect`] for push events.
#![forbid(unsafe_code)]

/// Reads a test fixture under `crates/ic3-teams/tests/fixtures/` at compile time.
///
/// Defined before the module declarations: `macro_rules!` is textually scoped,
/// so it is only visible to modules declared after it.
#[cfg(test)]
#[macro_export]
macro_rules! fixture {
    ($rel:literal) => {
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/",
            $rel
        ))
    };
}

pub mod api;
pub mod auth;
pub mod capture;
pub mod client;
pub mod csa;
pub mod emoji;
pub mod error;
pub mod events;
pub mod format;
pub(crate) mod http;
pub mod logging;
pub mod media;
pub mod redact;
pub mod secret;
pub mod store;
pub mod trouter;
pub mod types;

/// The catch-up pieces (spec §5.4), hosted in [`store`]; the function itself
/// is re-exported as [`catch_up()`].
pub mod catch_up {
    pub use crate::store::catch_up::*;
}

#[cfg(test)]
pub(crate) mod test_support;

/// The README's example, compiled as a doctest so it stays correct.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;

pub use api::{Api, ApiOptions, Page, PageCursor, PageMeta, PagingMode, TYPING_REFRESH};
pub use auth::{
    CsaAudience, DeviceCodePrompt, Endpoints, FileTokenStore, MemoryTokenStore, StoredSession,
    TokenStore,
};
pub use capture::Capture;
pub use client::{
    Client, ClientBuilder, ClientOptions, DeviceCodeLogin, EVENT_CHANNEL_CAPACITY, EventStream,
};
pub use csa::{
    Channel, ChatKind, ChatMember, ChatSummary, ConversationFolder, CsaSource, FolderId,
    FolderKind, ParseReport, Team, UserState, parse_user_state,
};
pub use error::{Error, Result};
pub use events::{
    CatchUpReason, DisconnectReason, Event, EventMessage, RawEvent, RawEventHook, map_event_message,
};
pub use format::{Block, Inline, ReplyQuote, RichText};
pub use media::{DEFAULT_MAX_DOWNLOAD_BYTES, FileSharing, Media, MediaOptions};
pub use secret::Secret;
pub use store::catch_up::{CatchUpPlan, CatchUpResult, catch_up};
pub use store::{
    Change, MessageKey, SendState, Store, StoredConversation, StoredMessage, TYPING_EXPIRY, Unread,
};
pub use types::*;
