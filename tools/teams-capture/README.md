# teams-capture

Records what a real Teams client sends and receives, so ic3-teams can be built against observed
protocol shapes instead of guesses. Nothing here talks to Microsoft: `record.mjs` attaches to a
Teams client you run yourself and only watches its traffic.

Needs Node ≥ 22 (built-in `WebSocket`) and Python 3 (standard library only).

## 1. Run Teams with remote debugging

teams-for-linux (an Electron app) passes Chromium switches through:

```sh
/opt/teams/teams-for-linux-<version>.AppImage --remote-debugging-port=9222
```

or, persistently, in `~/.config/teams-for-linux/config.json`:

```json
"performance": { "electronCLIFlags": [["remote-debugging-port", "9222"]] }
```

Quit any running instance first (single-instance lock). Chrome or Edge with
`--remote-debugging-port=9222 --user-data-dir=<separate profile>` on teams.microsoft.com works
the same way. While the port is open, any local process can drive the signed-in session: only
open it for capture sessions.

Check: `curl -s localhost:9222/json/version`.

## 2. Record

```sh
node tools/teams-capture/record.mjs            # → captures/live-<YYYYMMDD-HHMMSS>/events.jsonl
node tools/teams-capture/record.mjs --out captures/live-sections --port 9222
```

It waits for the port, so it can be started before Teams. New pages and workers are attached
paused and resumed once recording is on, so a recorder started first sees the first requests.
It exits when Teams closes.

**Capturing a full sync.** Teams caches its state in IndexedDB and later starts only fetch
deltas (`…/v3/teams/users/me/updates` with a sync token). To see the full
`GET /api/csa/{geo}/api/v3/teams/users/me` (sections, chats with rosters, teams and channels):
quit Teams, start the recorder, add `"clearStorageData": { "storages": ["indexdb"] }` to the
teams-for-linux config, launch Teams, wait for the chat list, then **remove that line again**
(it clears on every launch). Cookies and localStorage stay, so you stay signed in.

## 3. Inspect

```sh
python3 -I tools/teams-capture/inspect.py mark "before edit"     # then do the action in Teams
python3 -I tools/teams-capture/inspect.py endpoints --since-mark  # what it called
python3 -I tools/teams-capture/inspect.py shape '/messages/\d+' --req --depth 4
python3 -I tools/teams-capture/inspect.py shape 'v3/teams/users/me\?'
```

`endpoints` and `shape` print URL templates, field names, value types and string lengths — not
message text, names or ids — so their output can go into `docs/unverified.md` and specs. Read it
before sharing anyway.

## Privacy

`events.jsonl` holds real chat content. Authorization and token headers are redacted and sign-in
bodies omitted, but message bodies, names and ids are recorded. Keep captures under the
git-ignored `captures/`, never commit them, and quote shapes, not contents.

Findings so far: `docs/specs/2026-10-14-ui-v2-brainstorm.md` §14.1.
