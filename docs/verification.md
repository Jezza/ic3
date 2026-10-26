# Verification checklist

ic3-teams was built without access to a live Teams account, so every assumption about the
undocumented protocol is isolated in one function and listed in [`docs/unverified.md`](unverified.md)
(spec §9). This checklist is the user-run part: run it top to bottom in your own terminal, signed in
to your own account. No step needs any further development; a step that fails tells you which flag
to retry with or which function to change.

Time: about 3 hours of wall clock, most of it unattended (two 75-minute listening runs). Attended
parts: roughly 15 minutes for steps 1–2 and the first 10 minutes of step 3.

Run everything from the repository root. Writes go only to your Notes chat
(`48:notes`) or the one chat you name with `--conv`.

## 0. Before you start

**Build once:**

```sh
cargo build --workspace --examples
```

Nothing else is needed: no app registration, no config file. Network and VPN as you normally use
Teams. Have the official Teams client open (desktop or web), and ideally Teams on your phone too.

**Pick a test chat.** A 1:1 or group chat whose members won't mind a few test posts (a formatted
message, an edit of it, a reply saying `smoke reply`, a 👍 that is removed again, the deletion of
the formatted message and, with `--typing`, a typing signal). Best if it has more than 10
messages (paging is checked 5 at a time) and someone who can send you a message on request (steps 2
and 6 need a message from someone else). A group chat you can rename is useful for §9.4 (rename it
back afterwards).

**Where things go:**

| What | Where |
|---|---|
| Session (shared by smoke and ic3-irc) | `$XDG_DATA_HOME/ic3-irc/session.json` (usually `~/.local/share/ic3-irc/session.json`), mode `0600` |
| smoke captures | `./captures/smoke-<YYYYMMDD-HHMMSS>/` (override with `--capture <dir>`) |

Captures and logs are redacted (no tokens, cookies or trouter URLs) but **contain real chat content**.
`captures/` is git-ignored: never commit it, and share single capture files only when asked.

**Run one at a time.** smoke and ic3-irc share the session file, including the endpoint id trouter
registers under. Stop ic3-irc (`ic3-irc stop`) before `smoke events`, and don't run two smoke runs at
the same time: the two
push connections would compete for one registration, and both would rotate the same refresh token.

**Reading smoke's output.** Every §9 item ends in exactly one line:

```text
PASS        §9.2   message paging (older pages)  [000012-http-messages.json]  page 2: 5 messages, all older than …
```

| Verdict | Meaning | Exit code |
|---|---|---|
| `PASS` | The assumption held. The bracketed file is the evidence. | 0 |
| `FAIL` | The assumption is wrong (or the request was rejected). The note says how; non-2xx answers include the redacted response. Look the item up in [section 6](#6-when-something-fails) and `docs/unverified.md`. | 1 |
| `UNVERIFIED` | The run could not tell: the chat was too short, you answered "n" to a precondition, nothing happened while it listened, the run was too short, or the stage did not run at all (`not run`). Not a failure; the note says what to do differently. | 0 |
| `INFO` | Informational only (§9.10, and the formatting check in `write`). Never fails the run. | 0 |

Every run ends with `=== Summary: paste everything from this line to the end back ===`, then one line
per §9 item and the same as a Markdown table. A single-stage run lists the other stages' items as
`UNVERIFIED … not run`: that is expected.

Questions shaped `>> … [y/n]` want `y` or `n` and Enter; `>> … Press Enter to continue…` waits for
you to do something in Teams first. Never use `--yes` for the real run: it answers every question with
"y" and verifies nothing.

All flags (`--conv`, `--capture`, `--register-all-apps`, `--no-reconnect-on-renewal`,
`--profile-segment`, `--paging`, `--tenant`, `--session`, `--csa-audience chatsvcagg|spaces`,
`--csa-geo <geo>`, `--no-qtd-msgs`, `--no-clear-bookmark`) work before or after the stage name. `--typing` and
`--only <items>` are different: they are `write` flags and go after the stage name
(`smoke -- --conv '<id>' write --typing`, `smoke -- write --only 9.7,9.7b`).
`cargo run -p ic3-teams --example smoke -- --help` lists them.

## 1. Sign in, then `smoke rest` (read-only; §9.1, §9.2, §9.9, §9.10, §9.11)

```sh
cargo run -p ic3-teams --example smoke -- rest
```

**First time only:** it prints `Sign-in needed.` and Microsoft's device-code message ("To sign in, use
a web browser to open the page https://microsoft.com/devicelogin and enter the code …"). Open the
URL, enter the code, sign in with your work account. It prints `Signed in; the session is saved for
next time.` and continues. ic3-irc reuses the same session. (If sign-in is rejected for your
organisation, add `--tenant contoso.com`; the default `organizations` worked in the spike.)

What it prints, in order:

1. `Signed in as <name> (<mri>)`.
2. `-- §9.1 / §9.10: conversations --`: one line per page with `totalCount`, `backwardLink`,
   `syncState`, then the redacted `_metadata`. Then the §9.1 and §9.10 verdicts.
3. `-- §9.9: names --`: how many chats are named from the listing, how many needed `thread()`, whether
   `/api/mt` was asked; then **your chat list** (most recent first, up to 40): unread marker, name,
   `via` (how the name was found), and the `--conv '<id>'` to copy. Then the §9.9 verdict.
4. `-- §9.2 / §9.11: message paging in 48:notes --`: two pages of 5 messages with ids and arrival
   times. Then the §9.2 and §9.11 verdicts.

Your Notes chat is usually too short to page, so §9.2/§9.11 will often say `UNVERIFIED … only N
messages in 48:notes: … pick a longer chat with --conv`. **Copy your test chat's `--conv '<id>'` from
the list** and run the paging part again in it:

```sh
cargo run -p ic3-teams --example smoke -- --conv '<id>' rest
```

| Item | PASS means | FAIL / UNVERIFIED means |
|---|---|---|
| §9.1 conversation paging | Every conversation was retrieved (`N conversations in K page(s)`), with the `_metadata` shape recorded. With fewer than 100 chats: `page 1 held all N conversations (totalCount); its backwardLink (…) was not followed; … still unverified for accounts with more than 100 chats` (this run cannot show paging past 100). | FAIL `only N of totalCount M` / `paging stopped by a guard` / `still more pages`: more than one page of chats exists and the assumed `backwardLink` mechanism did not get them all. UNVERIFIED `exactly 100 … cannot tell`: one full page, no `totalCount`; paste the `_metadata` line. |
| §9.2 message paging | Page 2 holds only messages older than page 1's oldest. | FAIL `page 2 repeats or overlaps page 1`, `second page is empty`, `time window did not move`, or `no cursor for older messages`. UNVERIFIED: chat too short. |
| §9.9 names | Every visible chat got a name; the note says how many needed `thread()` and `/api/mt` (`/api/mt IS needed` or `not needed`: both are fine). smoke fetches `thread()` for 10 chats only (group chats first); untitled chats past that are left out of the verdict and counted as `not checked` (ic3-irc fetches every one). | FAIL `unresolved: <ids>`: some chats have no name; the note lists `thread()`/`user_names` errors. |
| §9.10 members (INFO) | Says whether the conversations response carries `members`. Either answer is fine. | — (never fails) |
| §9.11 `endTime` filter | Every page-2 message has arrival id ≤ `endTime`. The note says whether a message straddled the boundary (only then can id- and composetime-filtering be told apart; otherwise `this run cannot tell them apart`). | FAIL `page 2 has messages with arrival id > endTime`: the server filters on something else. UNVERIFIED: no second page. INFO `not applicable: server provides backwardLink`: paging follows the server's link, so the window is unused (`--paging window` probes it). |

Also check by eye: does the chat list match the official client's (names, order, unread `●` / counts)?
Note anything that differs.

## 1b. `smoke csa` (read-only; §9.12, §9.13, §9.14)

```sh
cargo run -p ic3-teams --example smoke -- csa
```

Nothing to do in Teams. smoke asks Azure AD for a second token, for the chat service aggregator
(CSA), then reads your folders, chats and teams from it once (about 1.4 MB) and prints **counts
only**:

1. `CSA audience: chatsvcagg`, then `CSA token: granted, expires HH:MM:SS (in N min)`, or
   `CSA token: not granted: http 400: <error>: <description>` (the OAuth error; never the token).
2. Five count lines: folders by kind, user-created folders with their item counts, chats by kind,
   chats with zero members and members in all, teams and channels. Then five `populated:` lines,
   counts of how many entries carry each field: user-created folders named x/y; teams named x/y
   (and how many are favorite, collapsed, archived); channels named x/y, general, with
   channel_type x/y, pinned, muted; chats with a title x/y, with last_message_at x/y, hidden,
   muted; members with a role x/y, with friendly_name x/y. A field name that is guessed wrong
   parses cleanly but shows as 0 here (e.g. `teams named 0/5`). Then `parse report: clean` or
   what was missing or skipped.

| Item | PASS means | FAIL / UNVERIFIED means |
|---|---|---|
| §9.12 CSA token | `token granted (audience chatsvcagg); CSA answered 2xx` | FAIL `CSA token (chatsvcagg) not granted: …` (retry with `--csa-audience spaces`), or `… CSA refused it: http 401 …`. UNVERIFIED when no geo answered (then §9.13 is the FAIL). |
| §9.13 CSA host / geo / request | `answered at https://teams.microsoft.com geo emea` (the host and geo that answered) | FAIL `no geo answered 2xx: http 404 …` (path, query or host wrong; `--csa-geo <geo>` helps only with a geo other than `emea`, `amer`, `apac`, which were all tried). UNVERIFIED when there was no token. |
| §9.14 field names | `every section present, nothing skipped; check the populated-field counts above`, and counts that look like your Teams | FAIL `CSA answered 2xx but the body is not JSON` (then §9.13 is UNVERIFIED). UNVERIFIED `guessed field names look wrong: …` naming the check that failed (`no team has a name`, `no channel has a name`, `no channel is General` (every team has one), `no user-created folder has a name`), or `partial parse: missing sections …; skipped …`. Even on PASS, read the `populated:` lines and tell us if a count is clearly wrong (no user folders although you have some, 0 members everywhere, `with a role 0/N`, every flag 0). |

A CSA failure never signs you out, not even an `invalid_grant` (decision #49). The CSA answer is
captured as `*-http-csa-user-state.json`: it holds your whole chat list with member MRIs, so keep it
private. The token grant is never captured.

## 2. `smoke write` (§9.6–§9.8, §9.15–§9.18): Notes first, then your test chat

**Replies carry `properties.qtdMsgs`** by default (decision #50). If §9.15 reports `the reply
carrying properties.qtdMsgs was refused`, replies will fail until `send_quoted_ids` defaults to
`false` in code: report it.

**Re-checking some items: `--only`.** `write --only <items>` runs only the steps those items need
(comma-separated, `§` optional; the write items are §9.6, §9.7, §9.7b, §9.8, §9.15, §9.16, §9.17,
§9.18). `cargo run -p ic3-teams --example smoke -- --conv '<id>' write --only 9.7,9.7b` only marks the chat
read (no message is sent); `… write --only 9.18` only sends the typing signal (`--only 9.18` implies
`--typing`). An item that needs the smoke message (§9.6, §9.8, §9.15, §9.16, §9.17) sends it, and a
sent smoke message is always deleted at the end, which decides §9.17 too. The `Post these …?` list
shows only the steps that will run; the items left out are `UNVERIFIED … not run (--only)`.

### 2a. In your Notes chat (only you can see it)

```sh
cargo run -p ic3-teams --example smoke -- write
```

It does not ask before writing to Notes. Follow the prompts; each waits for you:

1. `-- send (formatting) --` posts
   ``ic3 smoke: *bold* _italic_ ~strike~ `code` https://example.org``, a list item and a quote.
   `>> Open your Notes (self) chat in Teams: do the formatting and the list/quote look right? [y/n]`
   (bold, italic, strikethrough, inline code, a link, a bullet, a quote). Recorded as an `INFO
   formatting` line, not a §9 item.
2. `-- §9.16: edit --` edits the `ic3 smoke: …` message, appending ` (edited by smoke)` to its
   last line, then re-reads it from the server (up to three times). Nothing to answer; in Teams it now reads
   `… (edited by smoke)` with an "Edited" mark.
3. `-- §9.8: reply --` replies `smoke reply` to the newest message that isn't the smoke message
   just sent; smoke prints which one (`replying to <id> by <name>: "…"`).
   `>> In … in Teams, does "smoke reply" show as a reply quoting <name>'s message? [y/n]`
   If it does: `>> In … in Teams, click the quote at the top of "smoke reply": does it jump to <name>'s original message? [y/n]` (§9.15).
   Then: `>> Now, in Teams, reply to any message in … (hover a message > Reply) and send it …` Do that
   (any text), then press Enter. smoke re-reads the chat; if your reply does not parse as one it asks
   `>> Was "<text>" … the reply you just sent from Teams? [y/n]`. Answer truthfully.
4. `-- §9.6: react / unreact --` puts a 👍 on the `ic3 smoke: …` message.
   `>> … does a 👍 now show on your "ic3 smoke: …" message? [y/n]`, then it removes it:
   `>> … has the 👍 on that message disappeared again? [y/n]`
5. `-- §9.7: mark read --`: smoke prints the chat's server-side read state
   (`server, before mark-as-unread: consumptionhorizon …, consumptionHorizonBookmark …`), then
   `>> In Teams, first switch to a DIFFERENT chat. Then mark your Notes (self) chat unread (hover it in
   the chat list > … > Mark as unread). Don't open it again until asked.` Press Enter.
   `>> Does your Notes (self) chat now show as unread (bold / badge) …? [y/n]`. If Teams won't mark
   Notes unread, answer `n` (§9.7 and §9.7b become UNVERIFIED, and 2b is then needed for them). Otherwise smoke
   prints the read state again (`once unread`), marks it read (the `consumptionhorizon` PUT, then
   the `consumptionHorizonBookmark` PUT `0;<now>;0` that clears a Mark as unread, §9.7b), prints it a
   third time (`after our PUT`) and asks `>> Without opening it, has the unread marker … cleared in the Teams chat list? [y/n]`
   The same answer decides §9.7 and §9.7b. §9.7b is decided only when Mark as unread set the bookmark
   (the `once unread` line shows a `consumptionHorizonBookmark` whose first `;` field is not `0`);
   otherwise Teams used the horizon, §9.7 decides, and §9.7b is UNVERIFIED. With
   `--no-clear-bookmark` only the horizon PUT is sent and §9.7b is UNVERIFIED.

   **Notes "Mark as unread" is not a decisive §9.7 test.** On the first live run (2026-10-14) the PUT
   was accepted but the Notes badge set by Mark as unread stayed: Mark as unread may set a separate
   bookmark (`consumptionHorizonBookmark`) that a read horizon does not clear. Treat a Notes §9.7 FAIL
   reading `server accepted; badge not cleared (possibly mark-as-unread bookmark …)` as inconclusive
   and do 2b. Mark read now also clears that bookmark (§9.7b, decision #57): a Notes run where Mark
   as unread set it is what decides §9.7b.
6. `-- §9.17: delete --` deletes the `ic3 smoke: …` message (last, so the steps before it could
   use it) and re-reads it. Nothing to answer; in Teams it shows as deleted.

### 2b. In your test chat (visible to its members)

**The decisive §9.7 test.** Needed for §9.7 whatever 2a said; it also checks replies and reactions in
a real chat with other people, which is closer to how ic3-irc is used.

```sh
cargo run -p ic3-teams --example smoke -- --conv '<id>' write
```

If someone else can watch the test chat, run it as `cargo run -p ic3-teams --example smoke -- --conv '<id>' write --typing`: after the delete step it asks you to have them open the chat, sends one typing signal, and asks `>> Did they see you as typing in … ("… is typing") just now? [y/n]` (§9.18). Without `--typing` (and in `all`) §9.18 stays UNVERIFIED "not run". In Notes nobody else would see it, so nothing is sent.

It first lists what it will post (the formatted message, an edit of it, the reply `smoke reply`, a 👍
added and removed, a read marker, at the end the deletion of the formatted message, and, as item 7 only
with `--typing`, one typing signal) and asks
`>> Post these to the chat "<name>" (<id>)? [y/n]`. Then the same steps and prompts as 2a. For §9.7, **don't use "Mark as unread"**: at the §9.7 prompt, look at a different
chat and have another person send you a message in the test chat, so the badge comes from a real
unread message; then press Enter.

| Item | PASS means | FAIL means |
|---|---|---|
| §9.6 reaction shape | `react and unreact 2xx; the 👍 appeared and disappeared in Teams`. | `react: http 4xx …` / `unreact: …` (request shape rejected), or `2xx, but in Teams the 👍 never showed` / `did not go away`. |
| §9.7 mark read | `consumptionhorizon PUT 2xx …; server horizon now <what we sent> (= sent); the badge cleared`: both the server-side horizon (read back from the conversations listing) and the badge. The note ends with the read state before, once unread, and after the PUT. | `mark_read: http …` (rejected); `server accepted; badge not cleared (possibly mark-as-unread bookmark: consumptionHorizonBookmark …)` (inconclusive after Mark as unread: redo with 2b); `server horizon did not move`; `the badge cleared, but the server horizon is …, not ours`. UNVERIFIED if the chat could not be made unread first, or the badge cleared but the server state could not be read. |
| §9.7b bookmark | `consumptionHorizonBookmark once unread <its value>, after our PUT 0;<ms>;0; the badge cleared`: Mark as unread had set the bookmark, our PUT cleared it, and the badge went away. | `consumptionHorizonBookmark still set after our PUT …` (Teams ignores our PUT, or its shape is wrong), `the badge did not clear while the bookmark was set …`, or `consumptionHorizonBookmark PUT failed: http …` (retry with `--no-clear-bookmark` to rule it out). UNVERIFIED when the bookmark was never set once unread (§9.7 decides that case), the chat could not be made unread, or with `--no-clear-bookmark`. |
| §9.8 reply markup | `reply shows as a reply in Teams; official reply … parses as a reply` (or `no official reply found`: smoke events captures one too). | `does not show as a reply in Teams` (our markup is wrong) or `official reply … does NOT parse as a reply` (our parser doesn't know Teams' markup). |
| §9.15 quote link | `reply sent with properties.qtdMsgs; clicking its quote jumps to the original` | `… does not jump to the original` (Teams ignores our `qtdMsgs`), or `the reply carrying properties.qtdMsgs was refused: …` (retry with `--no-qtd-msgs`). UNVERIFIED when the reply did not show as one, or with `--no-qtd-msgs`. |
| §9.16 edit | `PUT 2xx; re-read: the new text and properties.edittime …` | `edit: http 4xx …` (the body is refused: the capture shows which field), `… still has the old text`, or `… no properties.edittime`. UNVERIFIED when the smoke message was not sent or not found. |
| §9.17 delete | `DELETE 2xx; re-read: deleted, properties.deletetime …` | `delete: http 4xx …`, or `… is not deleted`. UNVERIFIED when the re-read no longer lists it (check that Teams shows it deleted). |
| §9.18 typing | `POST 2xx; Teams showed the typing indicator` | `typing: http 4xx …` (shape refused), or `… Teams showed no typing indicator`. UNVERIFIED without `--typing`, or in Notes. |

No runtime switch exists for §9.6, §9.7, §9.8, §9.16, §9.17 or §9.18; §9.7b has `--no-clear-bookmark`, §9.15 has `--no-qtd-msgs`. Otherwise a FAIL means a code
change in the function named in section 6. The capture files hold the exact request and response.

## 3. `smoke events` (§9.3, §9.4, §9.5)

### 3a. Default options, 75 minutes

```sh
cargo run -p ic3-teams --example smoke -- --conv '<id>' events --minutes 75
```

It prints `Skype token expires at HH:MM:SS (in N min); renewal expected around HH:MM:SS (in N min)`
(the token lives about 64 minutes and is renewed at 80 % of that, so roughly 51 minutes in), then
`trouter: register_all_apps=false, reconnect_on_token_renewal=true; connecting…`, then the list of
actions below. **Wait for a line ending in `Connected`**, then, in the official client, in the test
chat:

1. send a message
2. edit it
3. react to it
4. remove the reaction
5. delete it
6. after someone else posts in the chat, read it on your phone or in another Teams client (the read
   marker must move)
7. rename a group chat (any group chat; rename it back afterwards). A 1:1 rename too, if Teams lets you.
8. mark a chat unread in Teams (hover it > … > Mark as unread), then open it again. Every
   `ConversationUpdate` line starts with `consumptionhorizon=… consumptionHorizonBookmark=…`, so this
   captures what Mark as unread actually changes (§9.7).

Each action should print within about a second, timestamped:

```text
14:02:11.481  NewMessage         → [NewMessage]  messagetype="RichText/Html" content="…"
14:02:11.482  NewMessage from Ada Example: hello  [19:…@thread.v2]
14:02:30.017  MessageUpdate      → [MessageEdited]  messagetype="RichText/Html" skypeeditedid="…" …
```

The first line is the raw `resourceType → [what ic3-teams mapped it to]`; the second is the decoded event.
`(dropped)` means ic3-teams ignored it. Events from your other chats appear too; that is fine. Two extra
lines can appear: `§9.4: … has blank content and no properties.deletetime` (a delete ic3-teams doesn't
recognise) and `ThreadUpdate <id>: kind keys under properties [...] …` (after a rename).

**Watch the connection lines.** ic3-irc catches up on missed messages whenever ic3-teams reports `Connected`,
which must happen after **every** (re)connect. So: the first `Connected` should come within a few
seconds, and every `Disconnected { reason: …, retry in Ns }` must be followed by `Connected` once
the network is fine. A `Disconnected` with no later `Connected` while the network works would leave
ic3-irc without catching up: report it.

You can leave it running after the eight actions. Around the renewal time it prints
`Skype token RENEWED (next expiry HH:MM:SS). trouter reconnects (TokenRenewal); once Connected is
printed again, send a message in …`. Expect `Disconnected { reason: TokenRenewal, retry in 0s }` then
`Connected`. Send one message (e.g. from your phone) to see delivery continue. Ctrl-C ends early; the
verdicts are still printed.

| Item | PASS means | FAIL / UNVERIFIED means |
|---|---|---|
| §9.3 trouter connect + register | `Connected; N EventMessage(s) (…)`. | FAIL `never Connected (last disconnect: …); retry with --register-all-apps`. UNVERIFIED `Connected, but no /messaging EventMessage arrived`: if you did act in Teams, **retry with `--register-all-apps`** (3c). |
| §9.4 payloads | Every one of NewMessage, MessageEdited, MessageDeleted, ReactionsChanged, ReadStateChanged was mapped at least once, no blank-content delete, no ThreadUpdate kind mismatch. The note lists counts per kind. | FAIL `never mapped: <kinds>; …` with the resourceTypes seen (an edit came as NewMessage, a delete as an edit, no ReadStateChanged, …), or a blank-content delete, or `ThreadUpdate kind differs`. Before reading it as a FAIL, check you did all of actions 1–6 (a missing kind may just be a missing action). |
| §9.5 default | `the Skype-token renewal at HH:MM:SS reconnected trouter (TokenRenewal, then Connected): the default works`. | UNVERIFIED `no Skype-token renewal during the run: run with --minutes 75`, or `renewed … but no TokenRenewal reconnect followed`. |

Typing in any chat (without sending) prints lines like
`Typing in <chat id> by <name> (from_name present)`; they show the incoming typing shape and are
not a §9 item. `from_name absent` means Teams sent no display name with it.

### 3b. Without the renewal reconnect, 75 minutes (§9.5 itself; mostly unattended)

This is what actually answers §9.5: does a socket keep delivering after the token it was opened with
has expired?

```sh
cargo run -p ic3-teams --example smoke -- --conv '<id>' --no-reconnect-on-renewal events --minutes 75
```

You don't need to repeat actions 1–7. At the renewal (about 51 minutes in) it prints:

```text
Skype token RENEWED (next expiry …). §9.5: the socket still uses the old token, which expires at
HH:MM:SS (in N min): send a message in … after HH:MM:SS.
```

**After that time** (about 64 minutes in), send one message to the test chat (or have someone do it).
It must print as an event. Then let the run finish or press Ctrl-C.

**Only the §9.5 line counts in this run.** You don't repeat actions 1–7 here, so §9.4 will print
FAIL (`never mapped: …`), §9.3 may say UNVERIFIED or PASS, and smoke exits with code 1: that is
expected. §9.3 and §9.4 come from run 3a.

| §9.5 result | Meaning | What to do |
|---|---|---|
| PASS `an event arrived … after its token expired … reconnecting is not needed` | Keeping the socket across renewals works. | Nothing required. The default (`reconnect_on_token_renewal = true`) stays safe; switching it off would save one reconnect per hour. |
| FAIL `no event arrived after the old token expired … although the socket stayed up` | The socket goes silently deaf once its token expires (pings still answered). Only valid if you did send a message after the printed time. | Keep the default `true`. No code change. |
| UNVERIFIED `trouter reconnected (…) after the renewal … run again` | Something else (network, server) reconnected the socket in between, so the old socket wasn't tested. | Run 3b again. |
| UNVERIFIED `the run ended … before the old token expired` / `no Skype-token renewal` | The run was too short. | `--minutes 75` (or more). |

### 3c. Only if §9.3 failed: the three-app registration

```sh
cargo run -p ic3-teams --example smoke -- --conv '<id>' --register-all-apps events --minutes 5
```

Repeat action 1 (send a message) once `Connected` prints. **Only the §9.3 line counts in this run:**
with one action and 5 minutes, §9.4 prints FAIL, §9.5 UNVERIFIED and smoke exits with code 1, all
expected. If §9.3 now passes, set
`ClientOptions::register_all_apps` in code and repeat 3a with `--register-all-apps` added. If
it still fails (`it failed with --register-all-apps too`), stop: the trouter handshake needs revisiting
(section 6, §9.3 rows) rather than a workaround.

## 4. Optional: `smoke all` (one table for everything)

```sh
cargo run -p ic3-teams --example smoke -- --conv '<id>' all --minutes 75
```

Runs `rest`, `csa`, `write` and `events` in order in one process and prints one summary covering
every §9 item. It runs `write` without `--typing`, so §9.18 stays UNVERIFIED there. A stage that stops early never hides the later stages. Use it if you'd rather paste a single
table, or to re-check everything after a flag change or a fix. It posts to the test chat again
(after asking). Note: in `all` the token was fetched at program start, so the renewal comes earlier
in the events stage than the up-front estimate says; the run length still covers it.

## 5. Re-running with a switch

Where an unknown has a known alternative, it is a flag, not a code change: a smoke flag here, and a
`ClientOptions` / `ApiOptions` field for code using the library.

| smoke flag | Retry it when |
|---|---|
| `--register-all-apps` | §9.3 FAIL, or UNVERIFIED with no events although you acted in Teams |
| `--no-reconnect-on-renewal` | Only as the §9.5 experiment (3b); keep the default unless 3b PASSes |
| `--profile-segment <s>` | §9.9 FAIL with 404/403 from `user_names` (`apac` and then your region are already tried; try e.g. `emea` or `amer`) |
| `--paging window` / `--paging backward_link` | §9.2 FAIL: if the page's `_metadata` shows a `backwardLink`, try `window`; if it has none, `window` is already what ran, so it's a code change |
| `--csa-audience spaces` | §9.12 FAIL: the chatsvcagg grant is refused; CSA calls then carry the main access token |
| `--csa-geo <geo>` | §9.13 FAIL with 404 on every geo. It does not pin a geo: `<geo>` is tried first, then the rest of `emea`, `amer`, `apac`, so after a 404 on every geo only a geo outside those three can change the result |
| `--no-qtd-msgs` | §9.15 or §9.8 FAIL because the reply carrying `qtdMsgs` was refused: sends the §9.8 payload |
| `--no-clear-bookmark` | §9.7b FAIL because the `consumptionHorizonBookmark` PUT was refused: marks read with the horizon PUT alone |

## 6. When something fails

Each FAIL points at exactly one function (the rows of [`docs/unverified.md`](unverified.md), which
also name the capture file pattern to look at). Fill that file's **Result** column as you go.

| Item | Function(s) to change (`crates/ic3-teams/src/…`) | Flag to try first |
|---|---|---|
| §9.1 | `api/paging.rs::next_cursor` (conversations branch) | none; paste the `_metadata` line |
| §9.2 | `api/paging.rs::next_cursor` (messages branch) | `--paging window` if a `backwardLink` was present |
| §9.3 | `trouter/connect.rs::registration_apps`, then `negotiate`, `session_query`, `TC`, `con_num`, `registration_body`; `trouter/frames.rs::user_authenticate`, `user_activity` (pick by where it stopped: `/v4/a`, session GET, websocket, authenticate, registrar) | `--register-all-apps` |
| §9.4 | `events/mod.rs::map_event_message` (discriminator; ThreadUpdate kind keys); `api/wire.rs::parse_message` (delete detection, when the blank-content line printed) | none |
| §9.5 | `trouter/mod.rs::TrouterConfig.reconnect_on_token_renewal` / `client.rs::ClientOptions::default` | none: the result decides the default |
| §9.6 | `api/mod.rs::reaction_request` | none |
| §9.7 | `api/mod.rs::mark_read_body` (only if the server horizon did not become what we sent) | none |
| §9.7b | `api/mod.rs::bookmark_clear_body` | `--no-clear-bookmark` (only if the bookmark PUT itself is refused) |
| §9.8 | `format/html_out.rs::reply_block_html` (ours doesn't show as a reply); `format/html_in.rs::parse_reply` (Teams' reply doesn't parse) | none |
| §9.9 | `api/mod.rs::profile_segments` (and the `user_names` call shape) | `--profile-segment <s>` |
| §9.10 | `api/wire.rs::members_from_conversation` (only if members exist under another shape) | — (INFO) |
| §9.11 | `api/paging.rs::next_cursor` (window `endTime`) | none |
| §9.12 | `auth/mod.rs::CSA_SCOPE` (the grant: `auth/manager.rs::refresh_csa`) | `--csa-audience spaces` |
| §9.13 | `api/mod.rs::user_state_url`, `api/mod.rs::csa_geos`, `http.rs::host_headers` (`Host::Csa`) | `--csa-geo <geo>` |
| §9.14 | `csa.rs::parse_user_state` (the wire name behind whichever `populated:` count is 0 or the check the UNVERIFIED note names) | none |
| §9.15 | `api/mod.rs::quoted_messages_property` | `--no-qtd-msgs` (only if the reply send itself fails) |
| §9.16 | `api/mod.rs::edit_body` | none |
| §9.17 | `api/mod.rs::delete_url` | none |
| §9.18 | `api/mod.rs::typing_body` | none |

If an unknown behaves in a way none of this anticipates (for example the registrar rejects every
shape), stop and bring it back for a design revisit rather than working around it (spec §9).

## 7. Reporting back

Paste back, in one message:

1. **Each smoke run's summary**, from `=== Summary: paste everything from this line to the end back
   ===` to the end (it contains no tokens). Say which command each came from (the flags matter).
2. **`docs/unverified.md` with the Result column filled** (`PASS` / `FAIL: <short note>` /
   `UNVERIFIED`), one row per function. Take each item from the run that tests it: §9.3 and §9.4
   from run 3a (or 3c's §9.3 if you needed `--register-all-apps`), §9.5 from run 3b. §9.12–§9.14
   from 1b. The FAIL / UNVERIFIED lines that 3b and 3c print for items they don't exercise are
   expected; don't record them.
3. For each FAIL, the **capture file name** from its line. Don't paste captures wholesale (they hold
   chat content); a single file, or the part of it the FAIL note quotes, when asked.

Never commit `captures/`, session files or logs.
