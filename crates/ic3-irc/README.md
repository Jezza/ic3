# ic3-irc

An IRCv3 server for Microsoft Teams **chat**. Run it, point any IRCv3 client at it, and your Teams
chats and channels show up as IRC: send and receive messages, scroll back through history, reply,
react, edit, delete, see who is typing and who is away, and share images and files.

> **Unofficial.** It talks to Teams' internal, undocumented chat service ("IC3" / chatsvc), the one
> the official Teams clients use, through the [`ic3-teams`](https://crates.io/crates/ic3-teams) library. Microsoft does
> not support or document it, and **it can change or break at any time without notice.** It acts as
> you, with your own account. Use at your own risk.

**No app registration.** There is no Entra/Azure app to register. It signs in with a one-time device
code, using the Teams client's own first-party public client ID, and stays signed in across
restarts.

## Getting started

Needs stable Rust 1.98 or newer.

```sh
cargo install ic3-irc
ic3-irc                 # IRC on 127.0.0.1:6667, web endpoint on 127.0.0.1:6680
```

Connect your client to `127.0.0.1:6667`. The first time, the `Server` user sends you a code and a
URL: sign in in a browser and your chats are joined. If the session ever expires, `Server` sends a
new code by itself. `ic3-irc logout` forgets the saved session.

If you set a `password`, give it as the SASL (PLAIN) password. Whatever shows images and files (a
web client's browser, say) must be able to reach `public_url`, where they and uploads are served.

### As a service

`ic3-irc` can install itself as a systemd user service, which starts at login and restarts if it
fails:

```sh
ic3-irc install         # write ~/.config/systemd/user/ic3-irc.service, enable and start it
ic3-irc logs -f         # its log (the journal); -n N for more history
ic3-irc status | start | stop | restart | enable | disable
ic3-irc uninstall       # stop, disable and remove it
```

`install` runs the binary it was started as (install it with `cargo install` first, so the path
stays put) and takes `--config <path>` to run with a specific config file. The log level is the
config file's `log`. A user service stops when you log out unless you run `loginctl enable-linger`.

## How Teams maps to IRC

| Teams | IRC |
|---|---|
| Group and meeting chats | `#channels` named after the chat (`#Launch-plans`), joined for you |
| Team channels | `#Team/Channel` (`#Engineering/General`); a post's replies carry `+reply` pointing at the post |
| 1:1 chats | private queries with the other person's nick (listed by `CHATHISTORY TARGETS`) |
| Notes | a query with `Notes` |
| People | nicks made from display names (`JaneSmith`); the display name is the real name and `display-name` metadata |
| History, read state | `CHATHISTORY` and `MARKREAD`, served from Teams (no local database) |
| Replies, reactions | `+reply` (quotes in chats, thread replies in channels), `+draft/react` / `+draft/unreact` (👍 ❤️ 😂 😮 😢 😡) |
| Edits, deletes | `draft/message-edit` (`EDIT`), `draft/message-redaction` (`REDACT`) |
| Typing | `+typing` both ways |
| Presence | anyone not available is away with Teams' reason ("In a meeting", "Offline"): `away-notify`, `G` in `WHO`, `WHOIS` 301; `MONITOR` counts everyone but the Teams-offline as online |
| Mentions | your nick in the text (so clients highlight); `@Nick` or a leading `Nick:` becomes a Teams mention; a team, channel or everyone mention reads `@Name` |
| Images and files | links to the server's web endpoint, which fetches them with your sign-in; uploads through `soju.im/FILEHOST` are posted to Teams (images inline, files via OneDrive) |
| Leaving | `/part` hides the chat or channel in Teams too; a chat comes back with its next message, a channel with `/join` (which shows it in Teams again) |

Formatting: Teams emphasis becomes IRC formatting codes, and lists, quotes and code blocks become
Markdown (which Markdown-aware clients render). What you type may use IRC codes or Markdown
(`**bold**`, `*italic*`, `~~strike~~`, `` `code` ``, fences, `> `, `- `, `1. `).

### The `Server` user

Message `Server` (or use the raw command where one is given):

| Command | |
|---|---|
| `login`, `logout`, `status` | sign in, sign out, show the connection |
| `join <name>` | join a chat or channel by any part of its name; one match is joined, several are listed |
| `hide <name>` | hide a chat or channel in Teams, as `/part` does; also takes a person, for your 1:1 with them |
| `cutoff [age]` (also `/cutoff`) | how long a chat may be idle and still be joined: `60d`, `2w`, `12h`, `off`; without an age, shows it. Older chats join when a message arrives. Kept across restarts |

Capabilities offered: `away-notify`, `batch`, `cap-notify`, `echo-message`, `labeled-response`,
`message-tags`, `sasl` (PLAIN), `server-time`, `standard-replies`, `userhost-in-names`,
`draft/chathistory`, `draft/event-playback`, `draft/message-edit`, `draft/message-redaction`,
`draft/metadata-2` (read-only `display-name`), `draft/multiline`, `draft/read-marker`; and
`MONITOR`, `soju.im/FILEHOST`.

**Not supported yet:** starting new chats (a 1:1 with someone you have never chatted with can't be
opened from IRC), setting your own presence, calls, renaming chats, avatars.

## Configuration

In increasing precedence: the defaults, the config file, the environment, then the command line.

The config file is the platform config dir's `ic3-irc/config.toml` (`~/.config/ic3-irc/config.toml`
on Linux), or `--config <path>`. Unknown keys are errors. Every key is optional:

```toml
listen = ["127.0.0.1:6667"]          # IRC; several addresses allowed (e.g. a container bridge)
http_listen = "127.0.0.1:6680"       # web endpoint for media and uploads; "off" disables it
public_url = "https://irc.example"   # how browsers reach the web endpoint (default http://{http_listen})
password = "secret"                  # PASS / SASL PLAIN password, also the upload password
tenant = "organizations"             # or your tenant's domain / GUID
max_chat_age_days = 60               # the starting cutoff (see `Server`'s cutoff); 0 = every chat
# server_name = "ic3-irc"
# session_path = "/path/session.json"
# capture_dir = "/path"              # record redacted traffic (holds chat content)
# log = "info,ic3_irc=debug"
```

Environment: `IC3_IRC_PASSWORD`, `RUST_LOG`. Command line: `--config`, `--listen` (repeatable),
`--http-listen`, `--public-url`, `--tenant`, `--capture-dir`; `ic3-irc --help` lists them.

### Files

| File | |
|---|---|
| `~/.config/ic3-irc/config.toml` | the optional config |
| `~/.local/share/ic3-irc/session.json` | the refresh token and endpoint id, mode `0600` (a file readable by others is ignored, like an ssh key) |
| `~/.local/share/ic3-irc/settings.json` | what you change at run time: the cutoff, and the chats you left |

## Privacy

Tokens never leave memory except the refresh token in the `0600` session file. Logs (stderr, or the
journal as a service) and captures are redacted: no access, Skype or refresh tokens, cookies, push
URLs or connection parameters, and sign-in traffic is never captured. They do contain real chat
content (names, message text), so keep them private.

Media links are random, and that is the only thing protecting a file: anyone with a link can open it
while the server runs. Put the web endpoint behind something you trust if it is reachable by others.

## Licence

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT licence](LICENSE-MIT)
at your option. Unless you explicitly state otherwise, any contribution you intentionally submit for
inclusion in this work, as defined in the Apache-2.0 licence, is dual-licensed as above, without any
additional terms or conditions.
