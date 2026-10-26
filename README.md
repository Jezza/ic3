# ic3

An IRCv3 server for Microsoft Teams **chat** (`ic3-irc`), built on `ic3-teams`, a reusable Rust library
that holds all of the Teams protocol machinery. Point any IRCv3 client at it and your Teams chats
and channels show up as IRC.

> **Unofficial.** Both talk to Teams' internal, undocumented chat service ("IC3" / chatsvc), the one
> the official Teams clients use. Microsoft does not support or document it, and **it can change or
> break at any time without notice.** It acts as you, with your own account. Use at your own risk.

**No app registration.** There is no Entra/Azure app to register. The server signs in with a one-time
device code, using the Teams client's own first-party public client ID
(`1fec8e78-bce4-4aaf-ab1b-5451cc387264`), and stays signed in across restarts.

## How Teams maps to IRC

| Teams | IRC |
|---|---|
| Group and meeting chats | `#channels` named after the chat (`#Launch-plans`), joined for you |
| Team channels | `#Team/Channel` (`#Engineering/General`); a post's replies carry `+reply` pointing at the post |
| 1:1 chats | private queries with the other person's nick (listed by `CHATHISTORY TARGETS`) |
| Notes | a query with `Notes` |
| People | nicks made from display names (`JaneSmith`); the display name is the real name and `display-name` metadata |
| Sign-in, commands | the `Server` pseudo-user: `login`, `logout`, `status`, `join <name>`, `hide <name>` (any part of a name; one match acts, several are listed), `cutoff [age]` (how old a chat may be and still be joined, e.g. `60d`, `2w`, `off`; also `/cutoff`; kept across restarts) |
| History, read state | `CHATHISTORY` and `MARKREAD`, served straight from Teams (no local database) |
| Replies, reactions | `+reply` (quotes in chats, thread replies in channels), `+draft/react` / `+draft/unreact` (👍 ❤️ 😂 😮 😢 😡) |
| Edits, deletes | `draft/message-edit` (`EDIT`), `draft/message-redaction` (`REDACT`) |
| Typing | `+typing` both ways |
| Hide a chat or channel | `/part` (or `Server`'s `hide`, which also takes a person for a 1:1) hides it in Teams; a chat comes back with its next message, a channel with `/join` (which shows it in Teams again) |
| Images and files | links to the server's web endpoint, which fetches them with your sign-in; uploads through `soju.im/FILEHOST` are posted to Teams (images inline, files via OneDrive) |
| Mentions | your nick in the text (so clients highlight); `@Nick` or a leading `Nick:` becomes a Teams mention; a team, channel or everyone mention reads `@Name` |
| Presence | anyone not available is away with Teams' reason ("In a meeting", "Offline"): `away-notify`, `G` in `WHO`, `WHOIS` 301; `MONITOR` counts everyone but the Teams-offline as online |

Formatting: Teams emphasis becomes IRC formatting codes, and lists, quotes and code blocks become
Markdown (which Markdown-aware clients render). What you type may use IRC codes or Markdown (`**bold**`, `*italic*`,
`~~strike~~`, `` `code` ``, fences, `> `, `- `, `1. `).

Capabilities offered: `away-notify`, `message-tags`, `server-time`, `batch`, `labeled-response`, `echo-message`,
`cap-notify`, `standard-replies`, `userhost-in-names`, `sasl` (PLAIN), `draft/chathistory`,
`draft/event-playback`, `draft/multiline`, `draft/read-marker`, `draft/message-edit`,
`draft/message-redaction`, `draft/metadata-2` (read-only `display-name`).

## Status

Much of the protocol is observed rather than documented. Every assumption is isolated in one
function, marked `UNVERIFIED(§9.N)` in the code and listed in [`docs/unverified.md`](docs/unverified.md)
with what to try if it fails; that file also records which have since been confirmed against a live
account (most of them) and which have not yet.

Tests: wiremock for auth, HTTP, API and trouter; a fake transport for trouter; a fake backend for the
gateway, driven through IRC lines. No test talks to Microsoft.

**Not supported yet:** starting new chats (a 1:1 with someone you have never chatted with can't be
opened from IRC), setting your own presence, calls, renaming chats, avatars.

## Layout

```
crates/ic3-teams/       library: auth, HTTP, REST API, trouter push, events, store, formatting, media.
crates/ic3-teams/examples/smoke.rs   the interactive verification tool (docs/verification.md)
crates/ic3-irc/   the IRC server: connections, the gateway loop on ic3-teams, text rendering, web endpoint
docs/             decisions log, unverified assumptions, verification checklist
tools/            observing a real Teams client (teams-capture), the emoji table generator
```

## Build and run

Needs stable Rust 1.98 or newer (`rust-toolchain.toml` picks stable).

```sh
cargo build --workspace
cargo run -p ic3-irc                       # IRC on 127.0.0.1:6667, web endpoint on 127.0.0.1:6680
cargo run -p ic3-irc -- logout             # forget the saved session
```

### As a service

`ic3-irc` can install itself as a systemd user service, which starts at login and restarts if it
fails:

```sh
cargo install --path crates/ic3-irc   # a stable path for the service to run
ic3-irc install                        # write ~/.config/systemd/user/ic3-irc.service, enable, start
ic3-irc logs -f                        # its log (the journal); -n N for more history
ic3-irc status | start | stop | restart | enable | disable
ic3-irc uninstall                      # stop, disable and remove it
```

`install` takes `--config <path>` to run with a specific config file. The log level is the config
file's `log`. A user service stops when you log out unless you run `loginctl enable-linger`.

Connect your client to the IRC port. The first time, `Server` sends you a code and a URL; sign in in
a browser and your chats are joined. If the session expires, `Server` sends a new code by itself.

If you set a password, give it as the SASL (PLAIN) password. Whatever shows images and files (a
web client's browser, say) must be able to reach `public_url`, where they and uploads are served.

## Configuration

In increasing precedence: the default, the config file, the environment, then the command line.

- **Config file:** the platform config dir's `ic3-irc/config.toml` (`~/.config/ic3-irc/config.toml`
  on Linux), or `--config <path>`. Unknown keys are errors.

  ```toml
  listen = ["127.0.0.1:6667"]          # IRC; several addresses allowed (e.g. a container bridge)
  http_listen = "127.0.0.1:6680"       # web endpoint for media and uploads; "off" disables it
  public_url = "https://irc.example"   # how browsers reach the web endpoint (default http://{http_listen})
  password = "secret"                  # optional: PASS / SASL PLAIN password, also the upload password
  tenant = "organizations"             # or your tenant's domain / GUID
  max_chat_age_days = 60               # starting cutoff: chats idle longer are joined only when a message arrives; 0 = all
  # server_name = "ic3-irc"
  # session_path = "/path/session.json"
  # capture_dir = "/path"              # record redacted traffic (holds chat content)
  # log = "info,ic3_irc=debug"
  ```

- **Environment:** `IC3_IRC_PASSWORD`, `RUST_LOG`.
- **Command line:** `ic3-irc [run]`, `ic3-irc logout`, `--listen`, `--http-listen`, `--public-url`,
  `--tenant`, `--capture-dir`, `--config`.

Media links are random and are the only thing protecting a file: anyone with a link can open it
while the server runs.

### Files

| File | |
|---|---|
| `~/.config/ic3-irc/config.toml` | optional config |
| `~/.local/share/ic3-irc/session.json` | the refresh token and endpoint id; mode `0600` (a file readable by others is ignored, like an ssh key) |

Logs go to stderr, redacted.

## Privacy

Tokens never leave memory except the refresh token in the `0600` session file. The log and capture
files are redacted: no access, Skype or refresh tokens, cookies, trouter URLs or connection
parameters. Sign-in and token traffic (OAuth, authz) is never captured at all. Captures and logs
do contain real chat content (names, message text), so keep them private: `captures/` is
git-ignored and must never be committed.

## Observing a real Teams client

`tools/teams-capture/` records the traffic of a Teams client you run with remote debugging
(teams-for-linux, Chrome, Edge) and summarises it as shapes, for working out protocol details
ic3-teams doesn't know yet. It only observes; see its README.

## Using `ic3-teams` on its own

`ic3-teams` is usable on its own, including REST-only without the push connection. The crate never
prints; anything a user must see (such as the device code) is handed to the caller.

```rust
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

`ic3_teams::Store` (optional) keeps an in-memory model with unread state, pending sends and catch-up;
`ic3_teams::format` converts between Slack-style text, a rich-text tree and Teams HTML.

## Development

```sh
just check     # cargo fmt --check, clippy -D warnings, all tests, rustdoc -D warnings
```

Version control is jj (colocated git). The live checks are not automated: they are the smoke tool
(`cargo run -p ic3-teams --example smoke -- --help`), run by hand per [`docs/verification.md`](docs/verification.md).

Documents:

- [`docs/decisions.md`](docs/decisions.md): decisions made along the way, and why
- [`docs/unverified.md`](docs/unverified.md): every unverified protocol assumption, mapped to its code
- [`docs/verification.md`](docs/verification.md): the user-run verification checklist

## Licence

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT licence](LICENSE-MIT)
at your option. Unless you explicitly state otherwise, any contribution you intentionally submit for
inclusion in this work, as defined in the Apache-2.0 licence, is dual-licensed as above, without any
additional terms or conditions.

The protocol knowledge comes from reading [purple-teams](https://github.com/EionRobb/purple-teams)
(GPL-3.0), used as protocol reference only: **no code was copied** from it or from teams-for-linux.
