//! The gateway's view of `ic3-teams`: exactly the calls it makes, behind a trait so
//! the gateway can be tested against a fake.

use std::collections::HashMap;

use futures::StreamExt;
use ic3_teams::{
    Client, Conversation, ConversationId, DeviceCodeLogin, DeviceCodePrompt, Event, Inline, Me,
    Media, Message, MessageId, MessageRef, Mri, Outgoing, Page, PageCursor, Reaction, RichText,
    SendReceipt, ThreadInfo, Timestamp, UploadedFile, UserState,
};

/// The push events, boxed so a fake can hand out a plain channel.
pub type EventStreamBox = futures::stream::BoxStream<'static, Event>;

/// Everything the gateway needs from a Teams client.
#[async_trait::async_trait]
pub trait Backend: Send + Sync + 'static {
    /// `true` when there is no stored session.
    async fn needs_login(&self) -> ic3_teams::Result<bool>;
    /// Starts a device-code sign-in and returns what to show the user.
    async fn start_login(&self) -> ic3_teams::Result<DeviceCodePrompt>;
    /// Waits for the sign-in started by [`start_login`](Self::start_login).
    async fn wait_login(&self) -> ic3_teams::Result<()>;
    async fn logout(&self) -> ic3_teams::Result<()>;
    async fn me(&self) -> ic3_teams::Result<Me>;
    /// CSA: chats (with `hidden`), teams and their channels.
    async fn user_state(&self) -> ic3_teams::Result<UserState>;
    async fn conversations(
        &self,
        since: Option<Timestamp>,
        cursor: Option<PageCursor>,
    ) -> ic3_teams::Result<Page<Conversation>>;
    async fn messages(
        &self,
        conv: &ConversationId,
        cursor: Option<PageCursor>,
        n: usize,
    ) -> ic3_teams::Result<Page<Message>>;
    async fn send(&self, conv: &ConversationId, out: &Outgoing) -> ic3_teams::Result<SendReceipt>;
    async fn mark_read(&self, conv: &ConversationId, msg: &MessageRef) -> ic3_teams::Result<()>;
    /// Adds (`add`) or removes the user's `reaction` on `msg` (a channel
    /// reply's too: reactions go to the channel's address).
    async fn react(
        &self,
        conv: &ConversationId,
        msg: &MessageId,
        reaction: &Reaction,
        add: bool,
    ) -> ic3_teams::Result<()>;
    async fn edit(&self, original: &Message, body: &RichText) -> ic3_teams::Result<()>;
    async fn delete(&self, original: &Message) -> ic3_teams::Result<()>;
    async fn send_typing(&self, conv: &ConversationId) -> ic3_teams::Result<()>;
    /// The presence of `mris`.
    async fn presence(&self, mris: &[Mri]) -> ic3_teams::Result<Vec<ic3_teams::Presence>>;
    /// Subscribes the push connection to presence changes of `add` (and
    /// drops `remove`); `purge` drops every earlier subscription first.
    async fn subscribe_presence(
        &self,
        add: &[Mri],
        remove: &[Mri],
        purge: bool,
    ) -> ic3_teams::Result<()>;
    /// Hides a chat in Teams until a new message arrives.
    async fn hide(&self, conv: &ConversationId) -> ic3_teams::Result<()>;
    /// Hides a team channel from its team's list in Teams.
    async fn hide_channel(&self, channel: &ic3_teams::Channel) -> ic3_teams::Result<()>;
    /// Shows a hidden team channel in its team's list again.
    async fn unhide_channel(&self, channel: &ic3_teams::Channel) -> ic3_teams::Result<()>;
    async fn thread(&self, conv: &ConversationId) -> ic3_teams::Result<ThreadInfo>;
    /// Downloads a Teams image or file with the user's sign-in.
    async fn fetch_media(&self, url: &str) -> ic3_teams::Result<Media>;
    /// Uploads an image for `conv`; the result goes in a message body.
    async fn upload_image(
        &self,
        conv: &ConversationId,
        file_name: &str,
        content_type: &str,
        bytes: Vec<u8>,
    ) -> ic3_teams::Result<Inline>;
    /// Uploads a file shared with `conv`'s members, for `Outgoing::files`.
    async fn upload_file(
        &self,
        conv: &ConversationId,
        file_name: &str,
        bytes: Vec<u8>,
    ) -> ic3_teams::Result<UploadedFile>;
    async fn user_names(&self, mris: &[Mri]) -> ic3_teams::Result<HashMap<Mri, String>>;
    /// Every message of `conv` since `since` (ms), newest first.
    async fn messages_since(
        &self,
        conv: &ConversationId,
        since: Timestamp,
    ) -> ic3_teams::Result<Vec<Message>>;
    /// Starts the push connection and returns its events.
    async fn connect(&self) -> ic3_teams::Result<EventStreamBox>;
    async fn shutdown(&self);
}

/// The real [`Backend`]: a thin adapter over [`ic3_teams::Client`].
#[derive(Debug)]
pub struct Ic3Backend {
    client: Client,
    /// The sign-in between `start_login` and `wait_login`.
    login: tokio::sync::Mutex<Option<DeviceCodeLogin>>,
}

impl Ic3Backend {
    pub fn new(client: Client) -> Self {
        Ic3Backend {
            client,
            login: tokio::sync::Mutex::new(None),
        }
    }
}

#[async_trait::async_trait]
impl Backend for Ic3Backend {
    async fn needs_login(&self) -> ic3_teams::Result<bool> {
        self.client.needs_login().await
    }

    async fn start_login(&self) -> ic3_teams::Result<DeviceCodePrompt> {
        let login = self.client.start_device_login().await?;
        let prompt = login.prompt().clone();
        *self.login.lock().await = Some(login);
        Ok(prompt)
    }

    async fn wait_login(&self) -> ic3_teams::Result<()> {
        // Taken out first: the lock is not held while waiting.
        let login = self.login.lock().await.take();
        match login {
            Some(login) => login.wait().await,
            None => Err(ic3_teams::Error::Protocol("no sign-in in progress".into())),
        }
    }

    async fn logout(&self) -> ic3_teams::Result<()> {
        self.login.lock().await.take();
        self.client.logout().await
    }

    async fn me(&self) -> ic3_teams::Result<Me> {
        self.client.me().await
    }

    async fn user_state(&self) -> ic3_teams::Result<UserState> {
        let (state, report) = self.client.user_state().await?;
        tracing::debug!(?report, "CSA user state parsed");
        Ok(state)
    }

    async fn conversations(
        &self,
        since: Option<Timestamp>,
        cursor: Option<PageCursor>,
    ) -> ic3_teams::Result<Page<Conversation>> {
        self.client.api().conversations(since, cursor).await
    }

    async fn messages(
        &self,
        conv: &ConversationId,
        cursor: Option<PageCursor>,
        n: usize,
    ) -> ic3_teams::Result<Page<Message>> {
        self.client.api().messages(conv, cursor, n).await
    }

    async fn send(&self, conv: &ConversationId, out: &Outgoing) -> ic3_teams::Result<SendReceipt> {
        self.client.api().send_message(conv, out).await
    }

    async fn mark_read(&self, conv: &ConversationId, msg: &MessageRef) -> ic3_teams::Result<()> {
        self.client.api().mark_read(conv, msg).await
    }

    async fn react(
        &self,
        conv: &ConversationId,
        msg: &MessageId,
        reaction: &Reaction,
        add: bool,
    ) -> ic3_teams::Result<()> {
        if add {
            self.client.api().react(conv, msg, reaction).await
        } else {
            self.client.api().unreact(conv, msg, reaction).await
        }
    }

    async fn edit(&self, original: &Message, body: &RichText) -> ic3_teams::Result<()> {
        self.client.api().edit(original, body).await
    }

    async fn delete(&self, original: &Message) -> ic3_teams::Result<()> {
        self.client.api().delete(original).await
    }

    async fn send_typing(&self, conv: &ConversationId) -> ic3_teams::Result<()> {
        self.client.api().send_typing(conv).await
    }

    async fn hide(&self, conv: &ConversationId) -> ic3_teams::Result<()> {
        self.client.api().hide(conv).await
    }

    async fn presence(&self, mris: &[Mri]) -> ic3_teams::Result<Vec<ic3_teams::Presence>> {
        self.client.api().presence(mris).await
    }

    async fn subscribe_presence(
        &self,
        add: &[Mri],
        remove: &[Mri],
        purge: bool,
    ) -> ic3_teams::Result<()> {
        let Some(address) = self.client.push_address() else {
            return Err(ic3_teams::Error::Protocol(
                "the push connection is not registered yet".into(),
            ));
        };
        let endpoint = self.client.endpoint_id().await?;
        self.client
            .api()
            .subscribe_presence(&address, &endpoint, add, remove, purge)
            .await
    }

    async fn hide_channel(&self, channel: &ic3_teams::Channel) -> ic3_teams::Result<()> {
        self.client.api().hide_channel(channel).await
    }

    async fn unhide_channel(&self, channel: &ic3_teams::Channel) -> ic3_teams::Result<()> {
        self.client.api().unhide_channel(channel).await
    }

    async fn thread(&self, conv: &ConversationId) -> ic3_teams::Result<ThreadInfo> {
        self.client.api().thread(conv).await
    }

    async fn user_names(&self, mris: &[Mri]) -> ic3_teams::Result<HashMap<Mri, String>> {
        self.client.api().user_names(mris).await
    }

    async fn fetch_media(&self, url: &str) -> ic3_teams::Result<Media> {
        self.client.api().fetch_media(url).await
    }

    async fn upload_image(
        &self,
        conv: &ConversationId,
        file_name: &str,
        content_type: &str,
        bytes: Vec<u8>,
    ) -> ic3_teams::Result<Inline> {
        self.client
            .api()
            .upload_image(conv, file_name, content_type, bytes)
            .await
    }

    async fn upload_file(
        &self,
        conv: &ConversationId,
        file_name: &str,
        bytes: Vec<u8>,
    ) -> ic3_teams::Result<UploadedFile> {
        self.client.api().upload_file(conv, file_name, bytes).await
    }

    async fn messages_since(
        &self,
        conv: &ConversationId,
        since: Timestamp,
    ) -> ic3_teams::Result<Vec<Message>> {
        self.client.api().messages_since(conv, since).await
    }

    async fn connect(&self) -> ic3_teams::Result<EventStreamBox> {
        Ok(self.client.connect().await?.boxed())
    }

    async fn shutdown(&self) {
        self.client.shutdown().await;
    }
}
