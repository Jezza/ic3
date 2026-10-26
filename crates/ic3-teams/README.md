# ic3-teams

An **unofficial** Rust client library for Microsoft Teams **chat**: sign-in, chats and team
channels, messages, history, read state, reactions, edits, deletes, typing, mentions, images and
files, presence, and live events over Teams' own push connection.

> **Unofficial.** It talks to Teams' internal, undocumented chat service ("IC3" / chatsvc), the one
> the official Teams clients use. Microsoft does not support or document it, and **it can change or
> break at any time without notice.** It acts as you, with your own account. Use at your own risk;
> the crate stays `0.x` for that reason.

**No app registration.** There is no Entra/Azure app to register: it signs in with a one-time
device code using the Teams client's own first-party public client ID, and keeps the session in a
token store you choose (a file, memory, or your own).

## Example

```rust,no_run
use ic3_teams::{Client, Event, FileTokenStore};

#[tokio::main]
async fn main() -> ic3_teams::Result<()> {
    let client = Client::builder()
        .token_store(FileTokenStore::new("session.json"))
        .build()?; // no network yet

    if client.needs_login().await? {
        let login = client.start_device_login().await?;
        println!("{}", login.prompt().message);
        login.wait().await?;
    }

    // REST: the chat list and the latest messages of the first chat.
    let chats = client.api().conversations(None, None).await?;
    if let Some(c) = chats.items.first() {
        let page = client.api().messages(&c.id, None, 20).await?;
        for m in &page.items {
            println!("{}: {}", m.from.0, m.body.plain_text());
        }
    }

    // Push: events until the stream ends.
    let mut events = client.connect().await?;
    while let Some(ev) = events.recv().await {
        if let Event::NewMessage(m) = ev {
            println!("new in {}: {}", m.conversation.0, m.body.plain_text());
        }
    }
    client.shutdown().await;
    Ok(())
}
```

## What is in it

- `Client`: sign-in (device code), token refresh, and the entry points below. Building one does no
  network; the crate never prints, so anything a person must see (the device code) is handed to you.
- `Api`: the REST calls (conversations, messages, send, edit, delete, react, mark read, hide,
  typing, names, team channels, presence, images and files).
- `Client::connect`: the push connection ("trouter"), as a stream of `Event`s, reconnecting and
  re-registering by itself.
- `Store` (optional): an in-memory model with unread state, pending sends and catch-up after a
  reconnect.
- `format`: conversion between Slack-style text, a rich-text tree and Teams HTML.
- `capture` and `logging`: redacted request captures and a redacting log writer, for working out
  what changed when Teams does.

Usable REST-only, without the push connection.

Much of the protocol is observed rather than documented. Every assumption that has not been
confirmed against a live account is marked `UNVERIFIED(§n)` in the code and listed, with what to
try if it fails, in the repository's `docs/unverified.md`.

[`ic3-irc`](https://crates.io/crates/ic3-irc), an IRCv3 server for Teams chat, is built on it.

## Licence

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT licence](LICENSE-MIT)
at your option. Unless you explicitly state otherwise, any contribution you intentionally submit for
inclusion in this work, as defined in the Apache-2.0 licence, is dual-licensed as above, without any
additional terms or conditions.

The protocol knowledge comes from reading [purple-teams](https://github.com/EionRobb/purple-teams)
(GPL-3.0) and observing the official clients; **no code was copied** from purple-teams or
teams-for-linux.
