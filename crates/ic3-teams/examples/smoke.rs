//! `smoke` — the interactive checklist that verifies spec §9's unknowns
//! against your own Teams account (spec §9 "The verification tool").
//!
//! ```text
//! cargo run -p ic3-teams --example smoke -- rest
//! cargo run -p ic3-teams --example smoke -- csa
//! cargo run -p ic3-teams --example smoke -- --conv '<id from the rest list>' write
//! cargo run -p ic3-teams --example smoke -- --conv '<id>' write --typing
//! cargo run -p ic3-teams --example smoke -- --conv '<id>' events --minutes 75
//! cargo run -p ic3-teams --example smoke -- --conv '<id>' all
//! ```
//!
//! Every §9 item gets one `PASS` / `FAIL` / `UNVERIFIED` line (§9.10 is
//! informational and prints `INFO`, never failing the run) naming the
//! capture file that holds the evidence (and, on failure, the redacted
//! response); the run ends with a summary table to paste back. Steps that need
//! you to look at or act in the official Teams client ask and wait. Writes go
//! only to `48:notes` or the `--conv` chat.
#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::collections::{BTreeMap, HashMap, HashSet};
use std::future::Future;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use anyhow::{anyhow, bail};
use clap::{Parser, Subcommand};
use ic3_teams::api::Api;
use ic3_teams::format::parse_slack;
use ic3_teams::logging::{RedactingMakeWriter, filter_directives};
use ic3_teams::redact::{redact_json, redact_text};
use ic3_teams::{
    Block, Client, ClientBuilder, ClientOptions, Conversation, ConversationId, ConversationKind,
    CsaAudience, Event, FileTokenStore, FolderKind, Inline, Me, Message, MessageId, MessageRef,
    Mri, Page, PageMeta, PagingMode, ParseReport, RawEvent, RawEventHook, Reaction, ReadHorizon,
    ReplyTarget, Store, Timestamp, Unread, UserState, is_displayable, is_notes, now_ms,
    parse_iso_ms,
};
use serde_json::{Map, Value};

/// The only chat writes may go to without `--conv`.
const NOTES: &str = "48:notes";
/// Longest excerpt of a response or error printed.
const EXCERPT_CHARS: usize = 200;
/// Longest excerpt of an EventMessage resource printed by the events stage.
const RAW_EXCERPT_CHARS: usize = 120;
/// Longest message text excerpt in an event line.
const TEXT_EXCERPT_CHARS: usize = 80;
/// `--minutes` that spans a Skype-token renewal (§9.5).
/// The Skype token lives about 64 minutes (the spike measured `expiresIn`
/// ≈ 3831 s) and is renewed at 80 % of that; 75 minutes outlasts the old
/// token too, which `--no-reconnect-on-renewal` needs.
const RENEWAL_MINUTES: u64 = 75;
/// Default `--minutes` for the events stage.
const DEFAULT_EVENT_MINUTES: u64 = 3;
/// Most conversation pages fetched (the library's own guard is 50 too).
const MAX_CONVERSATION_PAGES: usize = 50;
/// Most `thread()` calls the names step makes.
const MAX_THREAD_FETCHES: usize = 10;
/// Page size for the message-paging check (small, so a short chat pages).
const PAGING_PAGE: usize = 5;
/// How many recent messages the write steps look through.
const RECENT: usize = 20;
/// How many chats the list prints.
const CHAT_LIST_LEN: usize = 40;
/// What the formatting step sends.
const FORMAT_SAMPLE: &str =
    "ic3 smoke: *bold* _italic_ ~strike~ `code` https://example.org\n- item\n> quote";
/// What the reply step sends.
const REPLY_TEXT: &str = "smoke reply";

/// What the edit step appends to the smoke message.
const EDIT_NOTE: &str = "(edited by smoke)";

const LONG_ABOUT: &str = "\
Verifies the protocol assumptions of spec §9 against your own Teams account.

Run it yourself, in a terminal. It reuses ic3-irc's session, or shows a device
code to sign in with the first time. Suggested order:

  1. smoke rest                       read-only; ends with a list of your recent chats
  1b. smoke csa                       read-only; your folders, chats and teams from CSA (counts only)
  2. smoke --conv '<id>' write        sends to that chat; asks you to check Teams (y/n)
     (add --typing when someone else watches that chat: §9.18)
  3. smoke --conv '<id>' events --minutes 75
  4. smoke --conv '<id>' --no-reconnect-on-renewal events --minutes 75   (§9.5 itself)

or everything at once: smoke --conv '<id>' all --minutes 75

Every §9 item prints one PASS / FAIL / UNVERIFIED line with the capture file that
holds the evidence (and the redacted response on failure); §9.10 is informational
and prints INFO, which never fails the run. The table printed at the
end is what to paste back. Writes go only to 48:notes or the --conv chat. Captures
are redacted (no tokens) but contain real chat content: keep them private.";

// ----- Command line ---------------------------------------------------------

/// Verifies the protocol assumptions of spec §9 against your Teams account.
#[derive(Parser, Clone, Debug)]
#[command(
    name = "smoke",
    long_about = LONG_ABOUT,
    subcommand_value_name = "STAGE",
    subcommand_help_heading = "Stages"
)]
struct Args {
    /// Azure AD tenant to sign in with (domain or GUID).
    #[arg(long, global = true, default_value = "organizations")]
    tenant: String,
    /// Session file [default: ic3-irc's, $XDG_DATA_HOME/ic3-irc/session.json].
    #[arg(long, global = true, value_name = "PATH")]
    session: Option<PathBuf>,
    /// Capture directory [default: ./captures/smoke-<timestamp>].
    #[arg(long, global = true, value_name = "DIR")]
    capture: Option<PathBuf>,
    /// The chat to page through and write to (pick one from `smoke rest`'s list)
    /// [default: 48:notes].
    #[arg(long, global = true, value_name = "ID")]
    conv: Option<String>,
    /// Register the three-app trouter set instead of TeamsCDLWebWorker alone (§9.3).
    #[arg(long, global = true)]
    register_all_apps: bool,
    /// Keep the trouter socket across a Skype-token renewal instead of reconnecting (§9.5).
    #[arg(long, global = true)]
    no_reconnect_on_renewal: bool,
    /// The /api/mt segment tried first for display names (§9.9) [default: apac].
    #[arg(long, global = true, value_name = "SEGMENT")]
    profile_segment: Option<String>,
    /// How further pages are requested (§9.1, §9.2): auto, window or backward_link.
    #[arg(long, global = true, value_name = "MODE", default_value = "auto", value_parser = parse_paging)]
    paging: PagingMode,
    /// Which token CSA requests carry (§9.12): chatsvcagg (a separate grant) or spaces (the main access token).
    #[arg(long, global = true, value_name = "AUDIENCE", default_value = "chatsvcagg", value_parser = parse_csa_audience)]
    csa_audience: CsaAudience,
    /// The CSA geo tried first (§9.13), e.g. emea, amer, apac [default: from your region].
    #[arg(long, global = true, value_name = "GEO")]
    csa_geo: Option<String>,
    /// Send replies without properties.qtdMsgs, as §9.8 verified (§9.15).
    #[arg(long, global = true)]
    no_qtd_msgs: bool,
    /// Mark read with the consumptionhorizon PUT alone, without clearing
    /// consumptionHorizonBookmark (§9.7b).
    #[arg(long, global = true)]
    no_clear_bookmark: bool,
    /// Answer every [y/n] question with y and don't wait for Enter. Only for
    /// unattended compile checks: nothing seen in Teams is verified.
    #[arg(long, global = true)]
    yes: bool,
    #[command(subcommand)]
    stage: Stage,
}

/// A stage of the checklist.
#[derive(Subcommand, Clone, Debug, PartialEq, Eq)]
enum Stage {
    /// §9.1, 9.2, 9.9, 9.10, 9.11 (read-only): conversation paging, members,
    /// names, message paging; lists your recent chats for --conv.
    Rest,
    /// §9.12, 9.13, 9.14 (read-only): the CSA token and one read of your
    /// folders, chats and teams; prints counts only.
    Csa,
    /// §9.6–9.8, 9.15–9.18: send, edit, reply, react, unreact, mark read
    /// and delete in 48:notes or the --conv chat; asks you to check each in
    /// Teams.
    Write {
        /// Also send one typing signal to the --conv chat; someone else
        /// watching it says whether Teams showed it (§9.18).
        #[arg(long)]
        typing: bool,
        /// Run only these items and the steps they need, e.g. `9.7,9.7b`
        /// (comma-separated; `§` optional): §9.6, §9.7, §9.7b, §9.8,
        /// §9.15, §9.16, §9.17, §9.18. A sent smoke message is always
        /// deleted at the end.
        #[arg(long, value_name = "ITEMS", value_delimiter = ',', value_parser = parse_write_item)]
        only: Vec<Item>,
    },
    /// §9.3, 9.4, 9.5: connect to trouter and print each pushed event while you
    /// act in Teams.
    Events {
        /// How long to listen; 75 spans a Skype-token renewal and the old token's expiry (§9.5).
        #[arg(long, default_value_t = DEFAULT_EVENT_MINUTES)]
        minutes: u64,
    },
    /// Every stage in order (rest, csa, write, events); a failing stage never hides
    /// the later ones.
    All {
        /// How long the events stage listens.
        #[arg(long, default_value_t = DEFAULT_EVENT_MINUTES)]
        minutes: u64,
    },
}

/// The items the write stage decides (and `--only` accepts), in report
/// order.
const WRITE_ITEMS: [Item; 8] = [
    Item::S9_6,
    Item::S9_7,
    Item::S9_7b,
    Item::S9_8,
    Item::S9_15,
    Item::S9_16,
    Item::S9_17,
    Item::S9_18,
];

impl Stage {
    /// The stages this one runs, in order.
    fn expand(&self) -> Vec<Stage> {
        match self {
            Stage::All { minutes } => vec![
                Stage::Rest,
                Stage::Csa,
                Stage::Write {
                    typing: false,
                    only: Vec::new(),
                },
                Stage::Events { minutes: *minutes },
            ],
            other => vec![other.clone()],
        }
    }

    fn name(&self) -> &'static str {
        match self {
            Stage::Rest => "rest",
            Stage::Csa => "csa",
            Stage::Write { .. } => "write",
            Stage::Events { .. } => "events",
            Stage::All { .. } => "all",
        }
    }

    /// The §9 items this stage decides.
    fn items(&self) -> Vec<Item> {
        use Item::*;
        match self {
            Stage::Rest => vec![S9_1, S9_2, S9_9, S9_10, S9_11],
            Stage::Csa => vec![S9_12, S9_13, S9_14],
            Stage::Write { .. } => WRITE_ITEMS.to_vec(),
            Stage::Events { .. } => vec![S9_3, S9_4, S9_5],
            Stage::All { .. } => Item::ALL.to_vec(),
        }
    }
}

fn parse_paging(s: &str) -> Result<PagingMode, String> {
    PagingMode::parse(s).ok_or_else(|| format!("`{s}`: expected auto, window or backward_link"))
}

/// A `--only` label: a write item, with or without its leading `§`.
fn parse_write_item(s: &str) -> Result<Item, String> {
    let label = format!("§{}", s.trim().trim_start_matches('§'));
    WRITE_ITEMS
        .into_iter()
        .find(|i| i.label() == label)
        .ok_or_else(|| {
            let valid: Vec<&str> = WRITE_ITEMS.iter().map(|i| i.label()).collect();
            format!("`{s}`: expected one of {}", valid.join(", "))
        })
}

fn parse_csa_audience(s: &str) -> Result<CsaAudience, String> {
    CsaAudience::parse(s).ok_or_else(|| format!("`{s}`: expected chatsvcagg or spaces"))
}

/// The client's run-time switches from the flags; unset ones keep the
/// library defaults.
fn client_options(args: &Args) -> ClientOptions {
    let defaults = ClientOptions::default();
    ClientOptions {
        register_all_apps: args.register_all_apps,
        reconnect_on_token_renewal: !args.no_reconnect_on_renewal,
        profile_segment: args
            .profile_segment
            .clone()
            .unwrap_or(defaults.profile_segment),
        paging: args.paging,
        csa_audience: args.csa_audience,
        csa_geo: args.csa_geo.clone(),
        send_quoted_ids: !args.no_qtd_msgs,
        clear_unread_bookmark: !args.no_clear_bookmark,
        media: defaults.media,
    }
}

/// `--session`, else ic3-irc's session file.
fn session_path(args: &Args) -> PathBuf {
    args.session.clone().unwrap_or_else(|| {
        directories::ProjectDirs::from("", "", "ic3-irc")
            .map(|d| d.data_dir().join("session.json"))
            .unwrap_or_else(|| PathBuf::from("session.json"))
    })
}

/// `--capture`, else `./captures/smoke-<timestamp>`.
fn capture_dir(args: &Args) -> PathBuf {
    args.capture.clone().unwrap_or_else(|| {
        PathBuf::from("captures").join(format!(
            "smoke-{}",
            chrono::Local::now().format("%Y%m%d-%H%M%S")
        ))
    })
}

/// Everything but the token store: tenant, capture, the switches and the
/// raw event hook that feeds `tally` (the events stage).
fn client_builder(args: &Args, capture: &Path, tally: &Tally) -> ClientBuilder {
    Client::builder()
        .tenant(&args.tenant)
        .capture_dir(capture)
        .options(client_options(args))
        .raw_event_hook(raw_event_hook(tally.clone()))
}

fn build_client(
    args: &Args,
    capture: &Path,
    session: &Path,
    tally: &Tally,
) -> ic3_teams::Result<Client> {
    client_builder(args, capture, tally)
        .token_store(FileTokenStore::new(session))
        .build()
}

/// Signs in with a device code when there is no session.
async fn ensure_login(client: &Client) -> ic3_teams::Result<()> {
    if !client.needs_login().await? {
        return Ok(());
    }
    let login = client.start_device_login().await?;
    let p = login.prompt();
    println!("\nSign-in needed.\n{}", p.message);
    println!(
        "(code {} at {}; waiting for you to finish…)",
        p.user_code, p.verification_url
    );
    login.wait().await?;
    println!("Signed in; the session is saved for next time.");
    Ok(())
}

// ----- Report ---------------------------------------------------------------

/// The §9 items (spec §9's numbered list).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Item {
    S9_1,
    S9_2,
    S9_3,
    S9_4,
    S9_5,
    S9_6,
    S9_7,
    S9_7b,
    S9_8,
    S9_9,
    S9_10,
    S9_11,
    S9_12,
    S9_13,
    S9_14,
    S9_15,
    S9_16,
    S9_17,
    S9_18,
}

impl Item {
    const ALL: [Item; 19] = [
        Item::S9_1,
        Item::S9_2,
        Item::S9_3,
        Item::S9_4,
        Item::S9_5,
        Item::S9_6,
        Item::S9_7,
        Item::S9_7b,
        Item::S9_8,
        Item::S9_9,
        Item::S9_10,
        Item::S9_11,
        Item::S9_12,
        Item::S9_13,
        Item::S9_14,
        Item::S9_15,
        Item::S9_16,
        Item::S9_17,
        Item::S9_18,
    ];

    fn label(self) -> &'static str {
        match self {
            Item::S9_1 => "§9.1",
            Item::S9_2 => "§9.2",
            Item::S9_3 => "§9.3",
            Item::S9_4 => "§9.4",
            Item::S9_5 => "§9.5",
            Item::S9_6 => "§9.6",
            Item::S9_7 => "§9.7",
            Item::S9_7b => "§9.7b",
            Item::S9_8 => "§9.8",
            Item::S9_9 => "§9.9",
            Item::S9_10 => "§9.10",
            Item::S9_11 => "§9.11",
            Item::S9_12 => "§9.12",
            Item::S9_13 => "§9.13",
            Item::S9_14 => "§9.14",
            Item::S9_15 => "§9.15",
            Item::S9_16 => "§9.16",
            Item::S9_17 => "§9.17",
            Item::S9_18 => "§9.18",
        }
    }

    /// Items that only inform (decision #24): their verdict is always
    /// `INFO`, so they never fail and never affect the exit code.
    fn informational(self) -> bool {
        self == Item::S9_10
    }

    fn title(self) -> &'static str {
        match self {
            Item::S9_1 => "conversation paging (_metadata, >100 chats)",
            Item::S9_2 => "message paging (older pages)",
            Item::S9_3 => "trouter connect, register, /messaging events arrive",
            Item::S9_4 => "EventMessage payloads (new, edit, delete, reaction, read, topic)",
            Item::S9_5 => "events continue after a Skype-token renewal",
            Item::S9_6 => "reaction add/remove request shape",
            Item::S9_7 => "consumptionhorizon PUT clears the unread badge",
            Item::S9_7b => "consumptionHorizonBookmark PUT clears a manual mark-as-unread",
            Item::S9_8 => "reply markup",
            Item::S9_9 => "names resolve without /api/mt",
            Item::S9_10 => "conversations response includes members",
            Item::S9_11 => "endTime filters on arrival id, not composetime",
            Item::S9_12 => "CSA token (chatsvcagg) granted and accepted",
            Item::S9_13 => "CSA host, geo, path, query and headers",
            Item::S9_14 => "CSA users/me field names (folders, chats, teams)",
            Item::S9_15 => "reply properties.qtdMsgs links the quote to the original",
            Item::S9_16 => "edit PUT accepted and shows as edited",
            Item::S9_17 => "DELETE ?behavior=softDelete deletes",
            Item::S9_18 => "our Control/Typing shows in Teams",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Verdict {
    Pass,
    Fail,
    Unverified,
    /// Informational items (§9.10, decision #24): never a failure.
    Info,
}

impl Verdict {
    fn as_str(self) -> &'static str {
        match self {
            Verdict::Pass => "PASS",
            Verdict::Fail => "FAIL",
            Verdict::Unverified => "UNVERIFIED",
            Verdict::Info => "INFO",
        }
    }
}

/// The verdict per item plus informational lines.
#[derive(Debug, Default)]
struct Report {
    rows: HashMap<Item, (Verdict, Option<String>, String)>,
    /// `INFO` lines (checks that are not §9 items).
    info: Vec<String>,
}

impl Report {
    fn new() -> Self {
        Report::default()
    }

    fn set(
        &mut self,
        item: Item,
        v: Verdict,
        capture_file: Option<String>,
        note: impl Into<String>,
    ) {
        let v = if item.informational() {
            Verdict::Info
        } else {
            v
        };
        self.rows.insert(item, (v, capture_file, note.into()));
    }

    fn decided(&self, item: Item) -> bool {
        self.rows.contains_key(&item)
    }

    fn any_fail(&self) -> bool {
        self.rows.values().any(|(v, _, _)| *v == Verdict::Fail)
    }

    /// The row for `item`; undecided items are `UNVERIFIED` ("not run").
    fn row(&self, item: Item) -> (Verdict, Option<&str>, &str) {
        match self.rows.get(&item) {
            Some((v, cap, note)) => (*v, cap.as_deref(), note.as_str()),
            None => (Verdict::Unverified, None, "not run"),
        }
    }

    /// `PASS        §9.1  <title>  [capture]  note`
    fn line(&self, item: Item) -> String {
        let (v, cap, note) = self.row(item);
        format!(
            "{:<10}  {:<5}  {}  [{}]  {}",
            v.as_str(),
            item.label(),
            item.title(),
            cap.unwrap_or("no capture"),
            note
        )
    }

    /// A markdown table with every item, then the `INFO` lines.
    fn render(&self) -> String {
        let mut t = String::from(
            "| Item | Verdict | What | Capture file | Note |\n|---|---|---|---|---|\n",
        );
        for item in Item::ALL {
            let (v, cap, note) = self.row(item);
            t.push_str(&format!(
                "| {} | {} | {} | {} | {} |\n",
                item.label(),
                v.as_str(),
                item.title(),
                cell(cap.unwrap_or("-")),
                cell(note)
            ));
        }
        for i in &self.info {
            t.push_str(&format!("\nINFO {}", i.replace('\n', " ")));
        }
        t
    }
}

/// Text safe inside a markdown table cell.
fn cell(s: &str) -> String {
    s.replace('|', "\\|").replace(['\n', '\r'], " ")
}

// ----- Helpers --------------------------------------------------------------

/// A redacted excerpt of a response body or error: JSON is redacted
/// field-wise, then the text; at most 200 characters, `…` when cut.
fn excerpt(s: &str) -> String {
    let redacted = match serde_json::from_str::<Value>(s) {
        Ok(mut v) if v.is_object() || v.is_array() => {
            redact_json(&mut v);
            redact_text(&v.to_string())
        }
        _ => redact_text(s),
    };
    truncate(&redacted, EXCERPT_CHARS)
}

/// At most `n` characters, `…` appended when cut.
fn truncate(s: &str, n: usize) -> String {
    match s.char_indices().nth(n) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_owned(),
    }
}

/// An `ic3-teams` error for printing: status plus redacted body excerpt.
fn ic3_err_text(e: &ic3_teams::Error) -> String {
    match e {
        ic3_teams::Error::Http { status, body } => format!("http {status}: {}", excerpt(body)),
        other => excerpt(&other.to_string()),
    }
}

fn err_text(e: &anyhow::Error) -> String {
    match e.downcast_ref::<ic3_teams::Error>() {
        Some(ie) => ic3_err_text(ie),
        None => excerpt(&format!("{e:#}")),
    }
}

/// Prints `msg` and waits for Enter.
fn prompt_enter(msg: &str) {
    println!("\n>> {msg}");
    print!("   Press Enter to continue… ");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    let _ = std::io::stdin().lock().read_line(&mut line);
}

/// Asks a y/n question; `assume_yes` answers `y` without asking. End of
/// input counts as no.
fn prompt_yes_no(msg: &str, assume_yes: bool) -> bool {
    if assume_yes {
        println!("\n>> {msg} [y/n] y  (--yes)");
        return true;
    }
    loop {
        print!("\n>> {msg} [y/n] ");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        match std::io::stdin().lock().read_line(&mut line) {
            Ok(0) | Err(_) => {
                println!("(no input: counted as no)");
                return false;
            }
            Ok(_) => {}
        }
        match line.trim().to_ascii_lowercase().as_str() {
            "y" | "yes" => return true,
            "n" | "no" => return false,
            _ => println!("   please answer y or n"),
        }
    }
}

/// The file name of the most recent capture.
fn last_capture(api: &Api) -> Option<String> {
    api.capture()
        .and_then(|c| c.last_path())
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
}

/// `a, b` of the capture names that exist.
fn join_captures(caps: &[Option<String>]) -> Option<String> {
    let names: Vec<&str> = caps.iter().flatten().map(String::as_str).collect();
    (!names.is_empty()).then(|| names.join(", "))
}

/// A capture file's JSON.
fn read_capture(dir: &Path, file: &str) -> Option<Value> {
    let text = std::fs::read_to_string(dir.join(file)).ok()?;
    serde_json::from_str(&text).ok()
}

/// A page's `_metadata`, redacted, as one line.
fn meta_line<T>(page: &Page<T>) -> String {
    match &page.meta.raw {
        Some(v) => {
            let mut v = v.clone();
            redact_json(&mut v);
            redact_text(&v.to_string())
        }
        None => "(no _metadata)".into(),
    }
}

/// UTC ISO-8601 for a millisecond timestamp.
fn iso(ms: Timestamp) -> String {
    chrono::DateTime::from_timestamp_millis(ms).map_or_else(
        || ms.to_string(),
        |t| t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
    )
}

/// Writes go only to `48:notes` or the chat chosen with `--conv`.
fn write_allowed(conv: &ConversationId, chosen: Option<&ConversationId>) -> bool {
    is_notes(conv) || chosen == Some(conv)
}

// ----- Interaction ----------------------------------------------------------

/// How the stages talk to the person running them.
trait Ui {
    /// Shows `msg` and waits until they are done.
    fn enter(&mut self, msg: &str);
    /// Asks a y/n question.
    fn yes_no(&mut self, msg: &str) -> bool;
}

/// stdin/stdout.
struct Terminal {
    assume_yes: bool,
}

impl Ui for Terminal {
    fn enter(&mut self, msg: &str) {
        if self.assume_yes {
            println!("\n>> {msg}\n   (--yes: not waiting)");
        } else {
            off_runtime(|| prompt_enter(msg));
        }
    }

    fn yes_no(&mut self, msg: &str) -> bool {
        let assume_yes = self.assume_yes;
        off_runtime(|| prompt_yes_no(msg, assume_yes))
    }
}

/// Runs a blocking stdin read without stalling the runtime's other tasks
/// (trouter's pings keep running while a prompt waits): on a multi-thread
/// runtime the worker hands its tasks off first.
fn off_runtime<R>(f: impl FnOnce() -> R) -> R {
    match tokio::runtime::Handle::try_current() {
        Ok(h) if h.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(f)
        }
        _ => f(),
    }
}

/// What every stage works with.
struct Ctx {
    client: Client,
    report: Report,
    /// What the raw event hook saw (the events stage).
    tally: Tally,
    ui: Box<dyn Ui>,
    /// `--conv`.
    conv: Option<ConversationId>,
    /// `--yes`: answers are assumed, not given.
    assume_yes: bool,
}

impl Ctx {
    fn new(client: Client, args: &Args, ui: Box<dyn Ui>, tally: Tally) -> Ctx {
        Ctx {
            client,
            report: Report::new(),
            tally,
            ui,
            conv: args.conv.clone().map(ConversationId),
            assume_yes: args.yes,
        }
    }

    fn api(&self) -> &Api {
        self.client.api()
    }

    /// The chat paged through and written to: `--conv`, else `48:notes`.
    fn target(&self) -> ConversationId {
        self.conv
            .clone()
            .unwrap_or_else(|| ConversationId(NOTES.into()))
    }

    /// The newest capture file now; pass it to [`Ctx::capture_since`] after
    /// a request to cite only the file that request wrote.
    fn capture_mark(&self) -> Option<String> {
        last_capture(self.api())
    }

    /// The newest capture file if one was written after `mark`, else `None`
    /// (never a stale file from an earlier request).
    fn capture_since(&self, mark: &Option<String>) -> Option<String> {
        last_capture(self.api()).filter(|now| Some(now) != mark.as_ref())
    }

    fn capture_dir(&self) -> Option<PathBuf> {
        self.api().capture().map(|c| c.dir().to_path_buf())
    }

    /// Records a verdict and prints its line.
    fn decide(&mut self, item: Item, v: Verdict, capture: Option<String>, note: impl Into<String>) {
        self.report.set(item, v, capture, note);
        println!("{}", self.report.line(item));
    }

    /// Records and prints an `INFO` line.
    fn info(&mut self, what: &str, capture: Option<String>, note: impl Into<String>) {
        let line = format!(
            "{what}  [{}]  {}",
            capture.as_deref().unwrap_or("no capture"),
            note.into()
        );
        println!("INFO        {line}");
        self.report.info.push(line);
    }

    /// FAIL every item of `items` not decided yet: `what` failed with `e`.
    /// `mark` is the capture mark taken before the step began.
    fn fail_undecided(
        &mut self,
        items: &[Item],
        what: &str,
        e: &anyhow::Error,
        mark: &Option<String>,
    ) {
        let note = format!("{what} failed: {}", err_text(e));
        let cap = self.capture_since(mark);
        for &item in items {
            if !self.report.decided(item) {
                self.decide(item, Verdict::Fail, cap.clone(), note.clone());
            }
        }
    }

    /// UNVERIFIED for every item of `items` not decided yet.
    fn unverified_undecided(&mut self, items: &[Item], note: &str) {
        for &item in items {
            if !self.report.decided(item) {
                self.decide(item, Verdict::Unverified, None, note);
            }
        }
    }

    fn ask(&mut self, q: &str) -> bool {
        self.ui.yes_no(q)
    }

    fn wait(&mut self, msg: &str) {
        self.ui.enter(msg);
    }

    /// Appended to notes that rest on a y/n answer.
    fn answers(&self) -> &'static str {
        if self.assume_yes {
            " (answer assumed by --yes: NOT verified)"
        } else {
            ""
        }
    }

    /// Refuses a write anywhere but `48:notes` or the `--conv` chat.
    fn guard_write(&self, conv: &ConversationId) -> anyhow::Result<()> {
        if write_allowed(conv, self.conv.as_ref()) {
            Ok(())
        } else {
            bail!(
                "refusing to write to {}: writes go only to {NOTES} or the --conv chat",
                conv.0
            )
        }
    }
}

// ----- Stages ---------------------------------------------------------------

async fn run_stage(ctx: &mut Ctx, stage: &Stage) -> anyhow::Result<()> {
    match stage {
        Stage::Rest => stage_rest(ctx).await,
        Stage::Csa => stage_csa(ctx).await,
        Stage::Write { typing, only } => stage_write(ctx, &WritePlan::new(*typing, only)).await,
        Stage::Events { minutes } => stage_events(ctx, *minutes).await,
        Stage::All { .. } => bail!("`all` runs its stages one by one"),
    }
}

/// Runs every stage of `stage` in order; a stage's error is printed, its
/// undecided items become UNVERIFIED, and the next stage still runs.
async fn run_stages(ctx: &mut Ctx, stage: &Stage) {
    for s in stage.expand() {
        println!("\n=== stage {} ===", s.name());
        if let Err(e) = run_stage(ctx, &s).await {
            let why = err_text(&e);
            println!("stage {} stopped: {why}", s.name());
            ctx.unverified_undecided(
                &s.items(),
                &format!("stage {} stopped early: {why}", s.name()),
            );
        }
    }
}

// ----- rest -----------------------------------------------------------------

async fn stage_rest(ctx: &mut Ctx) -> anyhow::Result<()> {
    let me = ctx.client.me().await?;
    println!("Signed in as {} ({})", me.display_name, me.mri.0);

    let mark = ctx.capture_mark();
    let convs = match rest_conversations(ctx).await {
        Ok(c) => c,
        Err(e) => {
            ctx.fail_undecided(&[Item::S9_1, Item::S9_10], "conversations", &e, &mark);
            Vec::new()
        }
    };
    let mark = ctx.capture_mark();
    if let Err(e) = rest_names(ctx, &me, &convs).await {
        ctx.fail_undecided(&[Item::S9_9], "names", &e, &mark);
    }
    let mark = ctx.capture_mark();
    if let Err(e) = rest_messages(ctx).await {
        ctx.fail_undecided(&[Item::S9_2, Item::S9_11], "messages", &e, &mark);
    }
    Ok(())
}

/// What one page told us, for the §9.1 verdict.
#[derive(Clone, Debug, Default, PartialEq)]
struct PageSummary {
    items: usize,
    meta_present: bool,
    total_count: Option<i64>,
    backward_link: bool,
    sync_state: bool,
    /// The backwardLink's path with the user segment generalised, and
    /// whether it equals syncState (see [`link_shape`]).
    link_shape: Option<String>,
    note: Option<String>,
}

impl PageSummary {
    fn of<T>(page: &Page<T>) -> PageSummary {
        PageSummary {
            items: page.items.len(),
            meta_present: page.meta.raw.is_some(),
            total_count: page.meta.total_count,
            backward_link: page.meta.backward_link.is_some(),
            sync_state: page.meta.sync_state.is_some(),
            link_shape: link_shape(&page.meta),
            note: page.note.clone(),
        }
    }
}

/// A `backwardLink` without host, query or user: `/v1/users/ME/…` or
/// `/v1/users/<mri>/…`, plus `, == syncState` when the two are equal.
fn link_shape(meta: &PageMeta) -> Option<String> {
    let link = meta.backward_link.as_deref()?;
    let path = url::Url::parse(link).map_or_else(
        |_| link.split('?').next().unwrap_or("").to_owned(),
        |u| u.path().to_owned(),
    );
    let path = match path.strip_prefix("/v1/users/") {
        Some(rest) => {
            let (user, tail) = rest.split_once('/').unwrap_or((rest, ""));
            let user = if user == "ME" { "ME" } else { "<mri>" };
            format!("/v1/users/{user}/{tail}")
        }
        None => path,
    };
    let same = meta.sync_state.as_deref() == Some(link);
    Some(if same {
        format!("{path}, == syncState")
    } else {
        path
    })
}

fn yes(b: bool) -> &'static str {
    if b { "yes" } else { "no" }
}

/// §9.1 from the pages fetched (all of them, unless `capped`), the number of
/// distinct conversations, and the page size asked for.
fn conversations_verdict(
    pages: &[PageSummary],
    unique: usize,
    page_size: usize,
    capped: bool,
) -> (Verdict, String) {
    let Some(first) = pages.first() else {
        return (Verdict::Unverified, "no page fetched".into());
    };
    let total = pages.iter().filter_map(|p| p.total_count).max();
    let shape = format!(
        "_metadata {}, backwardLink {}, syncState {}, totalCount {}",
        if first.meta_present {
            "present"
        } else {
            "absent"
        },
        yes(pages.iter().any(|p| p.backward_link)),
        yes(pages.iter().any(|p| p.sync_state)),
        total.map_or_else(|| "absent".to_owned(), |t| t.to_string()),
    );
    let n = pages.len();
    if capped {
        return (
            Verdict::Fail,
            format!("still more pages after {n}; stopped ({unique} conversations; {shape})"),
        );
    }
    if let Some(note) = pages.iter().find_map(|p| p.note.as_deref()) {
        return (
            Verdict::Fail,
            format!("paging stopped by a guard: {note} ({unique} conversations; {shape})"),
        );
    }
    if let Some(t) = total
        && t > unique as i64
    {
        return (
            Verdict::Fail,
            format!(
                "only {unique} of totalCount {t} conversations retrieved in {n} page(s); {shape}"
            ),
        );
    }
    if !first.meta_present && n == 1 && unique == page_size {
        return (
            Verdict::Unverified,
            format!(
                "exactly {page_size} conversations and no _metadata: cannot tell whether there are more"
            ),
        );
    }
    match total {
        Some(t) if n == 1 && unique < page_size => {
            let link = match &first.link_shape {
                Some(l) => format!("; its backwardLink ({l}) was not followed"),
                None => String::new(),
            };
            (
                Verdict::Pass,
                format!(
                    "page 1 held all {t} conversations (totalCount){link}; {shape}; paging past \
                     {page_size} chats is not exercised: still unverified for accounts with more \
                     than {page_size} chats"
                ),
            )
        }
        Some(_) => (
            Verdict::Pass,
            format!("{unique} conversations in {n} page(s); {shape}"),
        ),
        None if unique < page_size || n > 1 => (
            Verdict::Pass,
            format!("{unique} conversations in {n} page(s); {shape}"),
        ),
        None => (
            Verdict::Unverified,
            format!(
                "exactly {page_size} conversations in one page and no totalCount: cannot tell whether there are more; {shape}"
            ),
        ),
    }
}

/// §9.1 and §9.10: every conversation page, then members.
async fn rest_conversations(ctx: &mut Ctx) -> anyhow::Result<Vec<Conversation>> {
    println!("\n-- §9.1 / §9.10: conversations --");
    let page_size = ctx.api().options().conversations_page_size;
    let mut all: Vec<Conversation> = Vec::new();
    let mut seen = HashSet::new();
    let mut pages = Vec::new();
    let mut first_capture = None;
    let mut cursor = None;
    let mut capped = true;
    for n in 1..=MAX_CONVERSATION_PAGES {
        let mark = ctx.capture_mark();
        let page = ctx.api().conversations(None, cursor.take()).await?;
        let cap = ctx.capture_since(&mark);
        if first_capture.is_none() {
            first_capture.clone_from(&cap);
        }
        let s = PageSummary::of(&page);
        println!(
            "page {n}: {} conversations, totalCount {}, backwardLink {}, syncState {}, more {}, note {}  [{}]",
            s.items,
            s.total_count
                .map_or_else(|| "-".to_owned(), |t| t.to_string()),
            yes(s.backward_link),
            yes(s.sync_state),
            yes(page.next.is_some()),
            s.note.as_deref().unwrap_or("-"),
            cap.as_deref().unwrap_or("no capture"),
        );
        println!("  _metadata: {}", meta_line(&page));
        pages.push(s);
        for c in page.items {
            if seen.insert(c.id.clone()) {
                all.push(c);
            }
        }
        match page.next {
            Some(next) => cursor = Some(next),
            None => {
                capped = false;
                break;
            }
        }
    }
    let (v, note) = conversations_verdict(&pages, all.len(), page_size, capped);
    ctx.decide(Item::S9_1, v, first_capture.clone(), note);

    // Informational (decision #24): either answer is fine; it only says
    // whether thread() calls at startup could be skipped.
    let with_members = all.iter().filter(|c| c.members.is_some()).count();
    let note = if with_members > 0 {
        format!(
            "{with_members} of {} conversations carry `members`: thread() is not needed at startup",
            all.len()
        )
    } else {
        "no `members` in the conversations response: names come from thread(), as ic3-teams already does"
            .into()
    };
    ctx.decide(Item::S9_10, Verdict::Info, first_capture, note);
    Ok(all)
}

/// Visible chats (1:1, group, `48:notes`), most recently active first.
fn visible_chats(store: &Store) -> Vec<ConversationId> {
    let mut v: Vec<(Timestamp, ConversationId)> = store
        .conversations()
        .filter(|sc| !sc.stub && sc.conversation.kind.is_v1_visible(&sc.conversation.id.0))
        .map(|sc| {
            let id = sc.conversation.id.clone();
            (store.last_activity(&id), id)
        })
        .collect();
    v.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    v.into_iter().map(|(_, id)| id).collect()
}

/// How a chat named from the conversations listing alone got its name.
fn listing_method(store: &Store, id: &ConversationId, me: &Me) -> &'static str {
    if is_notes(id) {
        return "notes";
    }
    let Some(sc) = store.conversation(id) else {
        return "listing";
    };
    if sc
        .conversation
        .topic
        .as_deref()
        .is_some_and(|t| !t.trim().is_empty())
    {
        return "topic";
    }
    match sc.conversation.kind {
        ConversationKind::OneToOne => {
            let from_id = me.mri.orgid_guid().is_some_and(|g| {
                id.0.starts_with("19:") && id.0.ends_with("@unq.gbl.spaces") && id.0.contains(g)
            });
            if from_id { "id" } else { "lastMessage" }
        }
        _ => "members",
    }
}

fn unread_label(u: Unread) -> String {
    match u {
        Unread::None => String::new(),
        Unread::Some => "●".into(),
        Unread::Count(n) => n.to_string(),
        Unread::AtLeast(n) => format!("{n}+"),
    }
}

/// §9.9: name every visible chat from the listing, then `thread()`, then
/// `/api/mt`; prints the chat list to pick `--conv` from.
async fn rest_names(ctx: &mut Ctx, me: &Me, convs: &[Conversation]) -> anyhow::Result<()> {
    println!("\n-- §9.9: names --");
    if convs.is_empty() {
        ctx.decide(
            Item::S9_9,
            Verdict::Unverified,
            None,
            "no conversations listed",
        );
        return Ok(());
    }
    let mut store = Store::new();
    store.set_me(me);
    store.apply_conversations(convs.to_vec());
    let visible = visible_chats(&store);
    let mut how: HashMap<ConversationId, &'static str> = HashMap::new();
    for id in &visible {
        if store.conversation_name(id).is_some() {
            how.insert(id.clone(), listing_method(&store, id, me));
        }
    }
    let mut need: Vec<ConversationId> = visible
        .iter()
        .filter(|id| store.needs_thread_fetch(id))
        .cloned()
        .collect();
    // Group chats first: only thread() can name an untitled group, while
    // /api/mt can still name a 1:1 whose peer is known.
    need.sort_by_key(|id| {
        store
            .conversation(id)
            .is_none_or(|sc| sc.conversation.kind != ConversationKind::Group)
    });
    let skipped: Vec<ConversationId> = need.iter().skip(MAX_THREAD_FETCHES).cloned().collect();
    println!(
        "{} of {} visible chats are named from the listing; {} need thread()",
        how.len(),
        visible.len(),
        need.len()
    );
    let mut errors = Vec::new();
    let mut thread_capture = None;
    for id in need.iter().take(MAX_THREAD_FETCHES) {
        let mark = ctx.capture_mark();
        match ctx.api().thread(id).await {
            Ok(info) => store.apply_thread_info(info),
            Err(e) => errors.push(format!("thread({}): {}", id.0, ic3_err_text(&e))),
        }
        thread_capture = ctx.capture_since(&mark).or(thread_capture);
    }
    if !skipped.is_empty() {
        println!(
            "(smoke fetches thread() for {MAX_THREAD_FETCHES} chats only, group chats first; the other {} \
             are left out of the §9.9 verdict unless /api/mt names them — ic3-irc itself fetches every one)",
            skipped.len()
        );
    }
    for id in &visible {
        if store.conversation_name(id).is_some() {
            how.entry(id.clone()).or_insert("thread");
        }
    }
    let mris = store.unresolved_mris();
    let mut profile_capture = None;
    if !mris.is_empty() {
        println!(
            "{} member names still unknown: asking /api/mt fetchShortProfile (profile segment `{}`)",
            mris.len(),
            ctx.api().options().profile_segment
        );
        let mark = ctx.capture_mark();
        match ctx.api().user_names(&mris).await {
            Ok(names) => {
                println!("  /api/mt answered {} of {}", names.len(), mris.len());
                for (mri, name) in &names {
                    store.learn_name(mri, name);
                }
            }
            Err(e) => errors.push(format!("user_names: {}", ic3_err_text(&e))),
        }
        profile_capture = ctx.capture_since(&mark);
        for id in &visible {
            if store.conversation_name(id).is_some() {
                how.entry(id.clone()).or_insert("profile");
            }
        }
    }

    println!(
        "\nYour chats (most recent first; pass one to --conv). `via`: id = peer from the chat id, lastMessage = \
         the last message's sender, members = listing members, thread = thread(), profile = /api/mt"
    );
    for (i, id) in visible.iter().take(CHAT_LIST_LEN).enumerate() {
        println!(
            "{:>3}. {:<4} {:<36} via {:<11} --conv '{}'",
            i + 1,
            unread_label(store.unread(id)),
            store
                .conversation_name(id)
                .unwrap_or_else(|| "(unnamed)".into()),
            how.get(id).copied().unwrap_or("UNRESOLVED"),
            id.0
        );
    }
    if visible.len() > CHAT_LIST_LEN {
        println!(
            "     (+{} older chats not shown)",
            visible.len() - CHAT_LIST_LEN
        );
    }

    // Chats past the thread() cap that stayed unnamed were never checked.
    let unchecked = visible
        .iter()
        .filter(|id| skipped.contains(id) && store.conversation_name(id).is_none())
        .count();
    let unresolved: Vec<&str> = visible
        .iter()
        .filter(|id| store.conversation_name(id).is_none() && !skipped.contains(id))
        .map(|id| id.0.as_str())
        .collect();
    let checked = visible.len() - unchecked;
    let count = |m: &str| how.values().filter(|v| **v == m).count();
    let by_profile = count("profile");
    let mut note = format!(
        "{} of {} chats named: listing {}, thread() {}, /api/mt {} ({})",
        checked - unresolved.len(),
        checked,
        how.len() - count("thread") - by_profile,
        count("thread"),
        by_profile,
        if by_profile > 0 {
            "/api/mt IS needed"
        } else {
            "/api/mt not needed"
        }
    );
    if unchecked > 0 {
        note.push_str(&format!(
            "; {unchecked} untitled chats past the thread() cap not checked"
        ));
    }
    if !errors.is_empty() {
        note.push_str(&format!("; errors: {}", errors.join("; ")));
    }
    let cap = profile_capture.or(thread_capture);
    if unresolved.is_empty() {
        ctx.decide(Item::S9_9, Verdict::Pass, cap, note);
    } else {
        let shown: Vec<&str> = unresolved.iter().take(5).copied().collect();
        ctx.decide(
            Item::S9_9,
            Verdict::Fail,
            cap,
            format!(
                "unresolved: {}{}; {note}",
                shown.join(", "),
                if unresolved.len() > 5 { ", …" } else { "" }
            ),
        );
    }
    Ok(())
}

/// A message's arrival id as a number (its id, else `arrived_at`).
fn arrival_id(m: &Message) -> i64 {
    m.id.as_i64().unwrap_or(m.arrived_at)
}

/// §9.2 from the arrival ids of the first two pages and page 2's note.
fn message_paging_verdict(page1: &[i64], page2: &[i64], note: Option<&str>) -> (Verdict, String) {
    if let Some(note) = note {
        return (Verdict::Fail, format!("page 2 stopped with a note: {note}"));
    }
    if page2.is_empty() {
        return (
            Verdict::Fail,
            "the second page is empty although the first was full".into(),
        );
    }
    let Some(oldest) = page1.iter().min().copied() else {
        return (Verdict::Unverified, "first page empty".into());
    };
    let overlap: Vec<String> = page2
        .iter()
        .filter(|id| **id >= oldest)
        .map(i64::to_string)
        .collect();
    if overlap.is_empty() {
        (
            Verdict::Pass,
            format!(
                "page 2: {} messages, all older than page 1's oldest ({oldest})",
                page2.len()
            ),
        )
    } else {
        (
            Verdict::Fail,
            format!(
                "page 2 repeats or overlaps page 1: ids {} not older than {oldest}",
                overlap.join(", ")
            ),
        )
    }
}

/// §9.11: with a window cursor, page 2 must hold only messages whose
/// arrival id is at most `endTime`.
fn end_time_verdict(end: i64, page2: &[i64]) -> (Verdict, String) {
    if page2.is_empty() {
        return (
            Verdict::Unverified,
            "page 2 is empty: nothing to compare".into(),
        );
    }
    let late: Vec<String> = page2
        .iter()
        .filter(|id| **id > end)
        .map(i64::to_string)
        .collect();
    if late.is_empty() {
        (
            Verdict::Pass,
            format!("every page-2 message has arrival id <= endTime {end}"),
        )
    } else {
        (
            Verdict::Fail,
            format!(
                "page 2 has messages with arrival id > endTime {end} ({}): the server filters on something else (composetime?)",
                late.join(", ")
            ),
        )
    }
}

/// `endTime` of the request a capture file recorded.
fn end_time_of(capture: &Value) -> Option<i64> {
    let url = url::Url::parse(capture.pointer("/request/url")?.as_str()?).ok()?;
    url.query_pairs()
        .find(|(k, _)| k == "endTime")
        .and_then(|(_, v)| v.parse().ok())
}

/// `(id, composetime ms)` of each message in a captured messages response.
fn captured_composetimes(capture: &Value) -> Vec<(i64, i64)> {
    capture
        .pointer("/response/body/messages")
        .and_then(Value::as_array)
        .map(|ms| {
            ms.iter()
                .filter_map(|m| {
                    let id = m.get("id")?.as_str()?.parse().ok()?;
                    let ct = parse_iso_ms(m.get("composetime")?.as_str()?)?;
                    Some((id, ct))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Messages whose `composetime` lies on the other side of `end` than their
/// arrival id: the ones that tell id-filtering from composetime-filtering.
fn boundary_hints(end: i64, page1: &[(i64, i64)], page2: &[(i64, i64)]) -> Vec<String> {
    let mut hints = Vec::new();
    for (id, ct) in page1 {
        if *id > end && *ct <= end {
            hints.push(format!(
                "page 1 message {id} was composed at {ct} (<= endTime {end}) but arrived after it"
            ));
        }
    }
    for (id, ct) in page2 {
        if *ct > end {
            hints.push(format!(
                "page 2 message {id} was composed at {ct} (> endTime {end})"
            ));
        }
    }
    hints
}

fn print_message_page(label: &str, page: &Page<Message>, cap: Option<&str>) {
    println!(
        "{label}: {} messages, more {}, note {}  [{}]",
        page.items.len(),
        yes(page.next.is_some()),
        page.note.as_deref().unwrap_or("-"),
        cap.unwrap_or("no capture")
    );
    println!("  _metadata: {}", meta_line(page));
    for m in &page.items {
        println!(
            "  id {}  arrived {}  from {}  {}",
            m.id.0,
            iso(m.arrived_at),
            m.from_name.as_deref().unwrap_or(&m.from.0),
            truncate(&m.body.plain_text().replace('\n', " "), 40)
        );
    }
}

/// §9.2 and §9.11: two small pages of the target chat.
async fn rest_messages(ctx: &mut Ctx) -> anyhow::Result<()> {
    let conv = ctx.target();
    println!("\n-- §9.2 / §9.11: message paging in {} --", conv.0);
    let mark = ctx.capture_mark();
    let p1 = ctx.api().messages(&conv, None, PAGING_PAGE).await?;
    let cap1 = ctx.capture_since(&mark);
    print_message_page("page 1", &p1, cap1.as_deref());
    if p1.items.len() < PAGING_PAGE {
        let note = format!(
            "only {} messages in {}: paging needs more than {PAGING_PAGE}; pick a longer chat with --conv",
            p1.items.len(),
            conv.0
        );
        ctx.decide(Item::S9_2, Verdict::Unverified, cap1.clone(), note.clone());
        ctx.decide(Item::S9_11, Verdict::Unverified, cap1, note);
        return Ok(());
    }
    let Some(next) = p1.next.clone() else {
        let note = p1
            .note
            .as_deref()
            .map_or_else(String::new, |n| format!(" ({n})"));
        ctx.decide(
            Item::S9_2,
            Verdict::Fail,
            cap1.clone(),
            format!("a full first page but no cursor for older messages{note}"),
        );
        ctx.decide(
            Item::S9_11,
            Verdict::Unverified,
            cap1,
            "no second page to compare",
        );
        return Ok(());
    };
    let mark = ctx.capture_mark();
    let p2 = ctx.api().messages(&conv, Some(next), PAGING_PAGE).await?;
    let cap2 = ctx.capture_since(&mark);
    print_message_page("page 2", &p2, cap2.as_deref());
    let ids1: Vec<i64> = p1.items.iter().map(arrival_id).collect();
    let ids2: Vec<i64> = p2.items.iter().map(arrival_id).collect();
    let (v, note) = message_paging_verdict(&ids1, &ids2, p2.note.as_deref());
    ctx.decide(Item::S9_2, v, cap2.clone(), note);

    let dir = ctx.capture_dir();
    let doc = |cap: &Option<String>| {
        dir.as_deref()
            .zip(cap.as_deref())
            .and_then(|(d, f)| read_capture(d, f))
    };
    // paging=auto and backward_link follow the server's link: the endTime
    // window (§9.11's subject) is never used.
    if ctx.api().options().paging != PagingMode::Window && p1.meta.backward_link.is_some() {
        ctx.decide(
            Item::S9_11,
            Verdict::Info,
            cap2,
            "not applicable: server provides backwardLink (paging follows it; the endTime window is \
             unused). Run with --paging window to probe endTime",
        );
        return Ok(());
    }
    let (doc1, doc2) = (doc(&cap1), doc(&cap2));
    let Some(end) = doc2.as_ref().and_then(end_time_of) else {
        ctx.decide(
            Item::S9_11,
            Verdict::Unverified,
            cap2,
            "page 2 was not requested with endTime (paging via backwardLink?) or its capture is missing",
        );
        return Ok(());
    };
    let (v, mut note) = end_time_verdict(end, &ids2);
    let hints = boundary_hints(
        end,
        &doc1.as_ref().map(captured_composetimes).unwrap_or_default(),
        &doc2.as_ref().map(captured_composetimes).unwrap_or_default(),
    );
    for h in &hints {
        println!("  hint: {h}");
    }
    if v == Verdict::Pass {
        note.push_str(&if hints.is_empty() {
            "; no message straddles the boundary (composetime vs arrival), so this run cannot tell them apart".to_owned()
        } else {
            format!(
                "; {} message(s) straddle the boundary and paged by arrival id",
                hints.len()
            )
        });
    }
    ctx.decide(Item::S9_11, v, join_captures(&[cap1, cap2]), note);
    Ok(())
}

// ----- csa ------------------------------------------------------------------

/// §9.12–§9.14: the CSA token, then one user-state read; prints counts only
/// (spec A.6).
async fn stage_csa(ctx: &mut Ctx) -> anyhow::Result<()> {
    let audience = ctx.client.options().csa_audience;
    let tokens = ctx.client.tokens().await.clone();
    println!("CSA audience: {}", audience.as_str());
    if let Err(e) = tokens.csa_token().await {
        let why = oauth_error_text(&e);
        println!("CSA token: not granted: {why}");
        ctx.decide(
            Item::S9_12,
            Verdict::Fail,
            None,
            format!("CSA token ({}) not granted: {why}", audience.as_str()),
        );
        ctx.unverified_undecided(
            &[Item::S9_13, Item::S9_14],
            "no CSA token, so CSA was not called",
        );
        return Ok(());
    }
    let snap = tokens.current();
    let expires = match audience {
        CsaAudience::Chatsvcagg => snap.csa_expires_at,
        CsaAudience::Spaces => snap.access_expires_at,
    };
    println!(
        "CSA token: granted, expires {} (in {} min)",
        local_time(expires),
        (expires - now_ms()).max(0) / 60_000
    );
    let mark = ctx.capture_mark();
    let fetched = ctx.api().user_state_from().await;
    let cap = ctx.capture_since(&mark);
    match fetched {
        Ok((state, report, source)) => {
            for line in csa_summary(&state) {
                println!("  {line}");
            }
            println!("  parse report: {}", report_line(&report));
            ctx.decide(
                Item::S9_12,
                Verdict::Pass,
                cap.clone(),
                format!(
                    "token granted (audience {}); CSA answered 2xx",
                    audience.as_str()
                ),
            );
            ctx.decide(
                Item::S9_13,
                Verdict::Pass,
                cap.clone(),
                format!("answered at {} geo {}", source.host, source.geo),
            );
            let (v, note) = user_state_verdict(&state, &report);
            ctx.decide(Item::S9_14, v, cap, note);
        }
        // A 401 survived a renewal: the token itself is refused (§9.12).
        Err(e @ ic3_teams::Error::Http { status: 401, .. }) => {
            ctx.decide(
                Item::S9_12,
                Verdict::Fail,
                cap,
                format!(
                    "token granted (audience {}), but CSA refused it: {}",
                    audience.as_str(),
                    ic3_err_text(&e)
                ),
            );
            ctx.unverified_undecided(&[Item::S9_13, Item::S9_14], "CSA refused the token");
        }
        // A geo answered 2xx, but its body is not JSON: a §9.14 problem.
        Err(e @ ic3_teams::Error::Protocol(_)) => {
            ctx.decide(
                Item::S9_12,
                Verdict::Pass,
                cap.clone(),
                format!(
                    "token granted (audience {}); CSA answered 2xx",
                    audience.as_str()
                ),
            );
            ctx.decide(
                Item::S9_14,
                Verdict::Fail,
                cap,
                format!(
                    "CSA answered 2xx but the body is not JSON: {}",
                    ic3_err_text(&e)
                ),
            );
            ctx.unverified_undecided(
                &[Item::S9_13],
                "a geo answered 2xx, but its body could not be read (see §9.14 and the capture)",
            );
        }
        Err(e) => {
            ctx.decide(
                Item::S9_13,
                Verdict::Fail,
                cap,
                format!("no geo answered 2xx: {}", ic3_err_text(&e)),
            );
            ctx.unverified_undecided(
                &[Item::S9_12, Item::S9_14],
                "token granted, but CSA never answered 2xx (see §9.13)",
            );
        }
    }
    Ok(())
}

/// An OAuth error as `http <status>: <error>: <description>` (never the
/// form or a token); anything else as [`ic3_err_text`].
fn oauth_error_text(e: &ic3_teams::Error) -> String {
    if let ic3_teams::Error::Http { status, body } = e
        && let Ok(v) = serde_json::from_str::<Value>(body)
        && let Some(code) = v.get("error").and_then(Value::as_str)
    {
        let desc = v
            .get("error_description")
            .and_then(Value::as_str)
            .unwrap_or("");
        return format!(
            "http {status}: {code}: {}",
            truncate(&redact_text(desc), EXCERPT_CHARS)
        );
    }
    ic3_err_text(e)
}

/// The user state as counts (spec A.6): never names, ids or members.
fn csa_summary(s: &UserState) -> Vec<String> {
    let mut kinds: BTreeMap<String, usize> = BTreeMap::new();
    for f in &s.folders {
        *kinds.entry(format!("{:?}", f.kind)).or_default() += 1;
    }
    let user_items: Vec<String> = s
        .folders
        .iter()
        .filter(|f| f.kind == FolderKind::UserCreated)
        .map(|f| f.items.len().to_string())
        .collect();
    let mut chat_kinds: BTreeMap<String, usize> = BTreeMap::new();
    for c in &s.chats {
        *chat_kinds.entry(format!("{:?}", c.kind)).or_default() += 1;
    }
    let empty = s.chats.iter().filter(|c| c.members.is_empty()).count();
    let members: usize = s.chats.iter().map(|c| c.members.len()).sum();
    let channels: usize = s.teams.iter().map(|t| t.channels.len()).sum();
    let items = if user_items.is_empty() {
        "-".to_owned()
    } else {
        user_items.join(", ")
    };
    vec![
        format!("folders: {} ({})", s.folders.len(), counts(&kinds)),
        format!(
            "user-created folders: {} (items: {items})",
            user_items.len()
        ),
        format!("chats: {} ({})", s.chats.len(), counts(&chat_kinds)),
        format!("chats with zero members: {empty}; members in all: {members}"),
        format!("teams: {}; channels: {channels}", s.teams.len()),
    ]
    .into_iter()
    .chain(populated_fields(s))
    .collect()
}

/// How many entries carry each field the parser reads (counts only), so
/// a guessed wire name that never matches shows as e.g. `teams named 0/5`.
fn populated_fields(s: &UserState) -> Vec<String> {
    let user_folders: Vec<_> = s
        .folders
        .iter()
        .filter(|f| f.kind == FolderKind::UserCreated)
        .collect();
    let folders_named = user_folders.iter().filter(|f| f.name.is_some()).count();
    let channels: Vec<_> = s.teams.iter().flat_map(|t| &t.channels).collect();
    let members: Vec<_> = s.chats.iter().flat_map(|c| &c.members).collect();
    let teams = |p: fn(&ic3_teams::Team) -> bool| s.teams.iter().filter(|t| p(t)).count();
    let chans = |p: fn(&ic3_teams::Channel) -> bool| channels.iter().filter(|c| p(c)).count();
    let chats = |p: fn(&ic3_teams::ChatSummary) -> bool| s.chats.iter().filter(|c| p(c)).count();
    let membs = |p: fn(&ic3_teams::ChatMember) -> bool| members.iter().filter(|m| p(m)).count();
    vec![
        format!(
            "populated: user-created folders named {folders_named}/{}",
            user_folders.len()
        ),
        format!(
            "populated: teams named {}/{}; favorite {}, collapsed {}, archived {}",
            teams(|t| !t.name.is_empty()),
            s.teams.len(),
            teams(|t| t.favorite),
            teams(|t| t.collapsed),
            teams(|t| t.archived),
        ),
        format!(
            "populated: channels named {}/{}; general {}; with channel_type {}/{}; pinned {}, muted {}",
            chans(|c| !c.name.is_empty()),
            channels.len(),
            chans(|c| c.general),
            chans(|c| c.channel_type.is_some()),
            channels.len(),
            chans(|c| c.pinned),
            chans(|c| c.muted),
        ),
        format!(
            "populated: chats with a title {}/{}; with last_message_at {}/{}; hidden {}, muted {}",
            chats(|c| c.title.is_some()),
            s.chats.len(),
            chats(|c| c.last_message_at.is_some()),
            s.chats.len(),
            chats(|c| c.hidden),
            chats(|c| c.muted),
        ),
        format!(
            "populated: members with a role {}/{}; with friendly_name {}/{}",
            membs(|m| m.role.is_some()),
            members.len(),
            membs(|m| m.friendly_name.is_some()),
            members.len(),
        ),
    ]
}

/// `clean`, or what was missing and skipped.
fn report_line(r: &ParseReport) -> String {
    if r.is_clean() {
        return "clean".into();
    }
    let missing = if r.missing_sections.is_empty() {
        "none".to_owned()
    } else {
        r.missing_sections.join(", ")
    };
    let skipped = if r.skipped.is_empty() {
        "none".to_owned()
    } else {
        r.skipped
            .iter()
            .map(|(s, n)| format!("{s}×{n}"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    format!("missing sections: {missing}; skipped: {skipped}")
}

/// §9.14: UNVERIFIED when the fields every account has came back empty
/// (a guessed wire name that never matches parses cleanly as empty);
/// otherwise [`parse_report_verdict`].
fn user_state_verdict(s: &UserState, r: &ParseReport) -> (Verdict, String) {
    let channels: Vec<_> = s.teams.iter().flat_map(|t| &t.channels).collect();
    let user_folders: Vec<_> = s
        .folders
        .iter()
        .filter(|f| f.kind == FolderKind::UserCreated)
        .collect();
    let mut gaps = Vec::new();
    if !s.teams.is_empty() && s.teams.iter().all(|t| t.name.is_empty()) {
        gaps.push("no team has a name");
    }
    if !channels.is_empty() && channels.iter().all(|c| c.name.is_empty()) {
        gaps.push("no channel has a name");
    }
    // Every team has a General channel.
    if !channels.is_empty() && !channels.iter().any(|c| c.general) {
        gaps.push("no channel is General");
    }
    if !user_folders.is_empty() && user_folders.iter().all(|f| f.name.is_none()) {
        gaps.push("no user-created folder has a name");
    }
    if gaps.is_empty() {
        parse_report_verdict(r)
    } else {
        (
            Verdict::Unverified,
            format!("guessed field names look wrong: {}", gaps.join("; ")),
        )
    }
}

/// §9.14's parse-report rule: PASS for a clean parse, UNVERIFIED naming
/// what was not.
fn parse_report_verdict(r: &ParseReport) -> (Verdict, String) {
    if r.is_clean() {
        (
            Verdict::Pass,
            "every section present, nothing skipped; check the populated-field counts above".into(),
        )
    } else {
        (
            Verdict::Unverified,
            format!("partial parse: {}", report_line(r)),
        )
    }
}

// ----- write ----------------------------------------------------------------

/// How prompts name the target chat: "your Notes (self) chat", or the chat's
/// topic / members' names plus its id.
async fn chat_label(ctx: &Ctx, conv: &ConversationId) -> String {
    if is_notes(conv) {
        return "your Notes (self) chat".into();
    }
    let me = ctx.client.me().await.ok().map(|m| m.mri);
    let name = match ctx.api().thread(conv).await {
        Ok(t) => t.topic.filter(|t| !t.trim().is_empty()).or_else(|| {
            let names: Vec<String> = t
                .members
                .iter()
                .filter(|m| Some(&m.mri) != me.as_ref())
                .filter_map(|m| m.friendly_name.clone())
                .take(3)
                .collect();
            (!names.is_empty()).then(|| names.join(", "))
        }),
        Err(e) => {
            println!("(could not look up the chat's name: {})", ic3_err_text(&e));
            None
        }
    };
    match name {
        Some(n) => format!("the chat \"{n}\" ({})", conv.0),
        None => format!("the chat {}", conv.0),
    }
}

/// A step of the write stage; declared in the order the steps run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum WriteStep {
    Send,
    Edit,
    Reply,
    React,
    MarkRead,
    Delete,
    Typing,
}

impl WriteStep {
    /// The steps `item` needs (`--only`).
    fn needed_by(item: Item) -> &'static [WriteStep] {
        use WriteStep::*;
        match item {
            Item::S9_16 => &[Send, Edit],
            Item::S9_6 => &[Send, React],
            Item::S9_8 | Item::S9_15 => &[Send, Reply],
            Item::S9_7 | Item::S9_7b => &[MarkRead],
            Item::S9_17 => &[Send, Delete],
            Item::S9_18 => &[Typing],
            _ => &[],
        }
    }

    /// The step's line in the consent prompt.
    fn consent(self) -> String {
        match self {
            WriteStep::Send => format!("the message \"{}\"", FORMAT_SAMPLE.replace('\n', "\\n")),
            WriteStep::Edit => format!("an edit of that message, adding \"{EDIT_NOTE}\""),
            WriteStep::Reply => format!("the reply \"{REPLY_TEXT}\" (quoting the newest message)"),
            WriteStep::React => "a 👍 on message 1, removed again".into(),
            WriteStep::MarkRead => {
                "a read marker (consumption horizon) at the newest message".into()
            }
            WriteStep::Delete => "the deletion of message 1 (last)".into(),
            WriteStep::Typing => {
                "one typing signal (\"… is typing\"), while someone else watches".into()
            }
        }
    }
}

/// Note of the write items `--only` left out that no step decided.
const NOT_RUN_ONLY: &str = "not run (--only)";

/// What `smoke write` runs: every step (typing only with `--typing`), or
/// with `--only` the steps its items need, plus the delete that removes
/// the smoke message once sent.
#[derive(Clone, Debug, PartialEq, Eq)]
struct WritePlan {
    /// In the order they run.
    steps: Vec<WriteStep>,
    /// `--only`'s items; `None` without it.
    only: Option<Vec<Item>>,
}

impl WritePlan {
    fn new(typing: bool, only: &[Item]) -> WritePlan {
        use WriteStep::*;
        if only.is_empty() {
            let mut steps = vec![Send, Edit, Reply, React, MarkRead, Delete];
            if typing {
                steps.push(Typing);
            }
            return WritePlan { steps, only: None };
        }
        let mut steps: Vec<WriteStep> = only
            .iter()
            .flat_map(|&i| WriteStep::needed_by(i))
            .copied()
            .collect();
        if typing {
            steps.push(Typing);
        }
        // Cleanup: no smoke message is left in a real chat.
        if steps.contains(&Send) {
            steps.push(Delete);
        }
        steps.sort();
        steps.dedup();
        WritePlan {
            steps,
            only: Some(only.to_vec()),
        }
    }

    fn runs(&self, step: WriteStep) -> bool {
        self.steps.contains(&step)
    }

    /// Whether `item` was asked for: every item without `--only`.
    fn selects(&self, item: Item) -> bool {
        self.only.as_ref().is_none_or(|o| o.contains(&item))
    }

    /// The consent prompt's list: the steps that will run, numbered.
    fn consent_list(&self) -> String {
        self.steps
            .iter()
            .enumerate()
            .map(|(n, s)| format!("  {}. {}", n + 1, s.consent()))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// `UNVERIFIED … not run (--only)` for the items `--only` left out
    /// that no step decided.
    fn skip_unselected(&self, ctx: &mut Ctx) {
        let left_out: Vec<Item> = WRITE_ITEMS
            .into_iter()
            .filter(|&i| !self.selects(i))
            .collect();
        ctx.unverified_undecided(&left_out, NOT_RUN_ONLY);
    }
}

/// §9.6–§9.8, §9.15–§9.18 in the target chat, or with `--only` the steps
/// its items need; each step reports on its own, so a failing step never
/// hides the later ones. §9.18 runs only with `--typing` (or `--only 9.18`).
async fn stage_write(ctx: &mut Ctx, plan: &WritePlan) -> anyhow::Result<()> {
    use WriteStep::*;
    let conv = ctx.target();
    ctx.guard_write(&conv)?;
    let label = chat_label(ctx, &conv).await;
    println!("Writing to {label}  (writes go only to {NOTES} or the --conv chat)");
    if let Some(only) = &plan.only {
        let items: Vec<&str> = only.iter().map(|i| i.label()).collect();
        println!(
            "--only {}: the other write items are not run",
            items.join(",")
        );
    }
    if is_notes(&conv) {
        println!(
            "(no --conv: using {NOTES}; pick a real chat from `smoke rest` for the full check)"
        );
    } else {
        println!(
            "\nThis stage posts to {label}, visible to all its members:\n{}",
            plan.consent_list()
        );
        if !ctx.ask(&format!("Post these to {label}?")) {
            plan.skip_unselected(ctx);
            ctx.unverified_undecided(&WRITE_ITEMS, &format!("you declined writing to {}", conv.0));
            return Ok(());
        }
    }
    let own = if plan.runs(Send) {
        match write_send(ctx, &conv, &label).await {
            Ok(cmid) => Some(cmid),
            Err(e) => {
                println!("send failed: {}", err_text(&e));
                None
            }
        }
    } else {
        None
    };
    if plan.runs(Edit) {
        let mark = ctx.capture_mark();
        if let Err(e) = write_edit(ctx, &conv, own.as_deref()).await {
            ctx.fail_undecided(&[Item::S9_16], "edit", &e, &mark);
        }
    }
    if plan.runs(Reply) {
        let mark = ctx.capture_mark();
        if let Err(e) = write_reply(ctx, &conv, &label, own.as_deref()).await {
            ctx.fail_undecided(&[Item::S9_8, Item::S9_15], "reply", &e, &mark);
        }
    }
    if plan.runs(React) {
        let mark = ctx.capture_mark();
        if let Err(e) = write_react(ctx, &conv, &label, own.as_deref()).await {
            ctx.fail_undecided(&[Item::S9_6], "react", &e, &mark);
        }
    }
    if plan.runs(MarkRead) {
        let mark = ctx.capture_mark();
        if let Err(e) = write_mark_read(ctx, &conv, &label).await {
            ctx.fail_undecided(&[Item::S9_7, Item::S9_7b], "mark read", &e, &mark);
        }
    }
    // Last, so every step before it still has its target; as cleanup only
    // once the smoke message was sent.
    if plan.runs(Delete) && (own.is_some() || plan.selects(Item::S9_17)) {
        let mark = ctx.capture_mark();
        if let Err(e) = write_delete(ctx, &conv, own.as_deref()).await {
            ctx.fail_undecided(&[Item::S9_17], "delete", &e, &mark);
        }
    }
    if plan.runs(Typing) {
        let mark = ctx.capture_mark();
        if let Err(e) = write_typing(ctx, &conv, &label).await {
            ctx.fail_undecided(&[Item::S9_18], "typing", &e, &mark);
        }
    } else if plan.only.is_none() {
        ctx.decide(
            Item::S9_18,
            Verdict::Unverified,
            None,
            "not run: add --typing (`smoke --conv '<id>' write --typing`, with someone else watching the chat)",
        );
    }
    plan.skip_unselected(ctx);
    Ok(())
}

/// Sends the formatting sample; returns its client message id.
async fn write_send(ctx: &mut Ctx, conv: &ConversationId, label: &str) -> anyhow::Result<String> {
    println!("\n-- send (formatting) --");
    ctx.guard_write(conv)?;
    let mark = ctx.capture_mark();
    let sent = ctx
        .api()
        .send(conv, &parse_slack(FORMAT_SAMPLE), None)
        .await;
    let cap = ctx.capture_since(&mark);
    let receipt = match sent {
        Ok(r) => r,
        Err(e) => {
            ctx.info(
                "formatting",
                cap,
                format!("FAIL send: {}", ic3_err_text(&e)),
            );
            return Err(e.into());
        }
    };
    println!("sent (clientmessageid {})", receipt.client_message_id);
    let ok = ctx.ask(&format!(
        "Open {label} in Teams: do the formatting and the list/quote look right?"
    ));
    let note = format!(
        "{}{}",
        if ok { "looks right" } else { "looks WRONG" },
        ctx.answers()
    );
    ctx.info("formatting", cap, note);
    Ok(receipt.client_message_id)
}

/// `properties.<key>` of a message as text, when set.
fn property(m: &Message, key: &str) -> Option<String> {
    m.extra
        .get("properties")
        .and_then(|p| p.get(key))
        .filter(|v| !v.is_null() && v.as_str() != Some(""))
        .map(|v| v.as_str().map_or_else(|| v.to_string(), str::to_owned))
}

/// The last paragraph of `blocks`, looking into a trailing quote or list.
fn last_paragraph(blocks: &mut [Block]) -> Option<&mut Vec<Inline>> {
    match blocks.last_mut()? {
        Block::Paragraph(p) => Some(p),
        Block::Quote(inner) => last_paragraph(inner),
        Block::List { items, .. } => last_paragraph(items.last_mut()?),
        _ => None,
    }
}

/// `body` as `"<original> (edited by smoke)"` (spec C.3): the note is
/// appended to the last paragraph, or becomes one when there is none.
fn with_edit_note(body: &ic3_teams::RichText) -> ic3_teams::RichText {
    let mut b = body.clone();
    match last_paragraph(&mut b.0) {
        Some(p) => p.push(Inline::Text(format!(" {EDIT_NOTE}"))),
        None => {
            b.0.push(Block::Paragraph(vec![Inline::Text(EDIT_NOTE.into())]))
        }
    }
    b.normalize()
}

/// §9.16: edits the smoke message (appending [`EDIT_NOTE`]) and re-reads it.
async fn write_edit(ctx: &mut Ctx, conv: &ConversationId, own: Option<&str>) -> anyhow::Result<()> {
    println!("\n-- §9.16: edit --");
    let Some(cmid) = own else {
        ctx.decide(
            Item::S9_16,
            Verdict::Unverified,
            None,
            "the formatting message was not sent, so there is nothing of ours to edit",
        );
        return Ok(());
    };
    let Some(msg) = find_own(ctx.api(), conv, cmid).await? else {
        ctx.decide(
            Item::S9_16,
            Verdict::Unverified,
            None,
            format!(
                "lookup miss: the smoke message (clientmessageid {cmid}) is not among the latest {RECENT} messages of {}",
                conv.0
            ),
        );
        return Ok(());
    };
    let body = with_edit_note(&msg.body);
    ctx.guard_write(conv)?;
    let mark = ctx.capture_mark();
    let edited = ctx.api().edit(&msg, &body).await;
    let cap = ctx.capture_since(&mark);
    if let Err(e) = edited {
        ctx.decide(
            Item::S9_16,
            Verdict::Fail,
            cap,
            format!("edit: {}", ic3_err_text(&e)),
        );
        return Ok(());
    }
    // The edit may take a moment to show: up to 3 reads, 1 s apart.
    let mark = ctx.capture_mark();
    let mut verdict = edit_verdict(None);
    for attempt in 0..3 {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        let again = find_own(ctx.api(), conv, cmid).await?;
        verdict = edit_verdict(again.as_ref());
        if verdict.0 != Verdict::Fail {
            break;
        }
    }
    let check_cap = ctx.capture_since(&mark);
    let (v, note) = verdict;
    ctx.decide(Item::S9_16, v, join_captures(&[cap, check_cap]), note);
    Ok(())
}

/// §9.16 from the re-read message: PASS when it carries the new text and
/// `properties.edittime`.
fn edit_verdict(reread: Option<&Message>) -> (Verdict, String) {
    let Some(m) = reread else {
        return (
            Verdict::Unverified,
            "PUT 2xx, but the message was not found when re-read".into(),
        );
    };
    let text = m.body.plain_text().contains(EDIT_NOTE);
    match (text, property(m, "edittime")) {
        (true, Some(t)) => (
            Verdict::Pass,
            format!("PUT 2xx; re-read: the new text and properties.edittime {t}"),
        ),
        (true, None) => (
            Verdict::Fail,
            "PUT 2xx; the re-read shows the new text but no properties.edittime (not marked edited)".into(),
        ),
        (false, _) => (
            Verdict::Fail,
            "PUT 2xx, but the re-read message still has the old text".into(),
        ),
    }
}

/// §9.17: deletes the smoke message (the stage's last step) and re-reads it.
async fn write_delete(
    ctx: &mut Ctx,
    conv: &ConversationId,
    own: Option<&str>,
) -> anyhow::Result<()> {
    println!("\n-- §9.17: delete --");
    let Some(cmid) = own else {
        ctx.decide(
            Item::S9_17,
            Verdict::Unverified,
            None,
            "the formatting message was not sent, so there is nothing of ours to delete",
        );
        return Ok(());
    };
    let Some(msg) = find_own(ctx.api(), conv, cmid).await? else {
        ctx.decide(
            Item::S9_17,
            Verdict::Unverified,
            None,
            format!(
                "lookup miss: the smoke message (clientmessageid {cmid}) is not among the latest {RECENT} messages of {}",
                conv.0
            ),
        );
        return Ok(());
    };
    if msg.deleted {
        ctx.decide(
            Item::S9_17,
            Verdict::Unverified,
            None,
            "the smoke message is already deleted",
        );
        return Ok(());
    }
    ctx.guard_write(conv)?;
    let mark = ctx.capture_mark();
    let deleted = ctx.api().delete(&msg).await;
    let cap = ctx.capture_since(&mark);
    if let Err(e) = deleted {
        ctx.decide(
            Item::S9_17,
            Verdict::Fail,
            cap,
            format!("delete: {}", ic3_err_text(&e)),
        );
        return Ok(());
    }
    // The delete may take a moment to show: up to 3 reads, 1 s apart.
    let mark = ctx.capture_mark();
    let mut verdict = delete_verdict(None);
    for attempt in 0..3 {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        let again = find_own(ctx.api(), conv, cmid).await?;
        verdict = delete_verdict(again.as_ref());
        if verdict.0 != Verdict::Fail {
            break;
        }
    }
    let check_cap = ctx.capture_since(&mark);
    let (v, note) = verdict;
    ctx.decide(Item::S9_17, v, join_captures(&[cap, check_cap]), note);
    Ok(())
}

/// §9.17 from the re-read message: PASS when it is deleted with
/// `properties.deletetime`.
fn delete_verdict(reread: Option<&Message>) -> (Verdict, String) {
    let Some(m) = reread else {
        return (
            Verdict::Unverified,
            "DELETE 2xx, but the message was not among the latest when re-read (deleted messages may be left out): check Teams".into(),
        );
    };
    match (m.deleted, property(m, "deletetime")) {
        (true, Some(t)) => (
            Verdict::Pass,
            format!("DELETE 2xx; re-read: deleted, properties.deletetime {t}"),
        ),
        _ => (
            Verdict::Fail,
            "DELETE 2xx, but the re-read message is not deleted (no properties.deletetime)".into(),
        ),
    }
}

/// §9.18: one typing signal to the `--conv` chat; someone else watching it
/// says whether Teams showed it. Nobody else sees typing in 48:notes, so
/// nothing is sent there.
async fn write_typing(ctx: &mut Ctx, conv: &ConversationId, label: &str) -> anyhow::Result<()> {
    println!("\n-- §9.18: typing --");
    if is_notes(conv) {
        ctx.decide(
            Item::S9_18,
            Verdict::Unverified,
            None,
            "nobody else sees typing in your Notes chat: use --conv with a chat where someone else watches",
        );
        return Ok(());
    }
    ctx.wait(&format!(
        "Ask another member of {label} to open it in Teams and watch, then press Enter: smoke sends one \
         typing signal (Teams shows \"… is typing\" for a few seconds)."
    ));
    ctx.guard_write(conv)?;
    let mark = ctx.capture_mark();
    let sent = ctx.api().send_typing(conv).await;
    let cap = ctx.capture_since(&mark);
    if let Err(e) = sent {
        ctx.decide(
            Item::S9_18,
            Verdict::Fail,
            cap,
            format!("typing: {}", ic3_err_text(&e)),
        );
        return Ok(());
    }
    let shown = ctx.ask(&format!(
        "Did they see you as typing in {label} (\"… is typing\") just now?"
    ));
    let answers = ctx.answers();
    if shown {
        ctx.decide(
            Item::S9_18,
            Verdict::Pass,
            cap,
            format!("POST 2xx; Teams showed the typing indicator{answers}"),
        );
    } else {
        ctx.decide(
            Item::S9_18,
            Verdict::Fail,
            cap,
            format!("POST 2xx, but Teams showed no typing indicator{answers}"),
        );
    }
    Ok(())
}

/// The newest of the recent messages whose client message id is `cmid`,
/// trying a few times while it arrives.
async fn find_own(
    api: &Api,
    conv: &ConversationId,
    cmid: &str,
) -> ic3_teams::Result<Option<Message>> {
    for attempt in 0..3 {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        let page = api.messages(conv, None, RECENT).await?;
        if let Some(m) = page
            .items
            .into_iter()
            .find(|m| m.client_message_id.as_deref() == Some(cmid))
        {
            return Ok(Some(m));
        }
    }
    Ok(None)
}

/// What to reply to: the newest displayable message that is not ours, else
/// our own.
fn pick_reply_target(items: &[Message], own: Option<&str>) -> Option<ReplyTarget> {
    let usable = |m: &&Message| is_displayable(&m.message_type) && !m.deleted;
    let mine = |m: &&Message| own.is_some() && m.client_message_id.as_deref() == own;
    let m = items
        .iter()
        .filter(usable)
        .filter(|m| !mine(m))
        .max_by(|a, b| a.id.cmp(&b.id))
        .or_else(|| items.iter().filter(usable).find(mine))?;
    Some(ReplyTarget {
        message: MessageRef {
            id: m.id.clone(),
            client_message_id: m.client_message_id.clone(),
        },
        author: m.from.clone(),
        author_name: m.from_name.clone().unwrap_or_else(|| m.from.0.clone()),
        excerpt: truncate(&m.body.plain_text().replace('\n', " "), 100),
        sent_at: m.arrived_at,
    })
}

/// The reply the user just sent from the official client: the newest message
/// from `me` after `after` (our own reply) that smoke did not send.
fn official_reply_candidate<'a>(
    items: &'a [Message],
    me: &Mri,
    sent_by_smoke: &[&str],
    after: Option<&MessageId>,
) -> Option<&'a Message> {
    items
        .iter()
        .filter(|m| m.from == *me)
        .filter(|m| {
            m.client_message_id
                .as_deref()
                .is_none_or(|c| !sent_by_smoke.contains(&c))
        })
        .filter(|m| after.is_none_or(|a| m.id > *a))
        .max_by(|a, b| a.id.cmp(&b.id))
}

/// §9.8: reply to a message and have the user confirm it shows as a reply;
/// then check that a reply sent from Teams parses as one.
async fn write_reply(
    ctx: &mut Ctx,
    conv: &ConversationId,
    label: &str,
    own: Option<&str>,
) -> anyhow::Result<()> {
    println!("\n-- §9.8: reply --");
    let me = ctx.client.me().await?;
    let page = ctx.api().messages(conv, None, RECENT).await?;
    let target = pick_reply_target(&page.items, own)
        .ok_or_else(|| anyhow!("{} has no message to reply to", conv.0))?;
    println!(
        "replying to {} by {}: \"{}\"",
        target.message.id.0,
        target.author_name,
        truncate(&target.excerpt, 60)
    );
    ctx.guard_write(conv)?;
    let mark = ctx.capture_mark();
    let sent = ctx
        .api()
        .send(conv, &parse_slack(REPLY_TEXT), Some(&target))
        .await;
    let cap = ctx.capture_since(&mark);
    let quoted = ctx.client.options().send_quoted_ids;
    let receipt = match sent {
        Ok(r) => r,
        Err(e) => {
            let why = ic3_err_text(&e);
            if quoted {
                ctx.decide(
                    Item::S9_15,
                    Verdict::Fail,
                    cap.clone(),
                    format!(
                        "the reply carrying properties.qtdMsgs was refused: {why} (retry with --no-qtd-msgs to rule qtdMsgs out)"
                    ),
                );
            } else {
                ctx.decide(
                    Item::S9_15,
                    Verdict::Unverified,
                    None,
                    "not sent: --no-qtd-msgs",
                );
            }
            ctx.decide(
                Item::S9_8,
                Verdict::Fail,
                cap,
                format!("reply send failed: {why}"),
            );
            return Ok(());
        }
    };
    let shown = ctx.ask(&format!(
        "In {label} in Teams, does \"{REPLY_TEXT}\" show as a reply quoting {}'s message?",
        target.author_name
    ));
    decide_quote_link(ctx, label, &target.author_name, shown, quoted, cap.clone());

    // The other half of §9.8: the official client's reply markup.
    ctx.wait(&format!(
        "Now, in Teams, reply to any message in {label} (hover a message > Reply) and send it, so the \
         official reply markup can be checked. Press Enter when sent (or just Enter to skip; `smoke events` \
         captures one too)."
    ));
    let mark = ctx.capture_mark();
    let recent = ctx.api().messages(conv, None, RECENT).await?;
    let check_cap = ctx.capture_since(&mark);
    let ours = recent
        .items
        .iter()
        .find(|m| m.client_message_id.as_deref() == Some(receipt.client_message_id.as_str()))
        .map(|m| m.id.clone());
    let smoke_sent: Vec<&str> = [Some(receipt.client_message_id.as_str()), own]
        .into_iter()
        .flatten()
        .collect();
    let official = official_reply_candidate(&recent.items, &me.mri, &smoke_sent, ours.as_ref());
    let mut mismatch = false;
    let official_note = match official {
        Some(m) if m.reply_to.is_some() => format!(
            "official reply {} parses as a reply (to {})",
            m.id.0,
            m.reply_to.as_ref().map_or("?", |r| r.message_id.0.as_str())
        ),
        Some(m) => {
            let preview = truncate(&m.body.plain_text().replace('\n', " "), 60);
            if ctx.ask(&format!(
                "Was \"{preview}\" (message {} in {label}) the reply you just sent from Teams?",
                m.id.0
            )) {
                mismatch = true;
                format!(
                    "official reply {} does NOT parse as a reply: html_in::parse_reply must learn its markup (see capture)",
                    m.id.0
                )
            } else {
                "no official reply found to check".into()
            }
        }
        None => "no official reply of yours found after ours; `smoke events` captures one".into(),
    };
    let caps = join_captures(&[cap, check_cap]);
    let answers = ctx.answers();
    if !shown {
        ctx.decide(
            Item::S9_8,
            Verdict::Fail,
            caps,
            format!("sent 2xx but it does not show as a reply in Teams{answers}; {official_note}"),
        );
    } else if mismatch {
        ctx.decide(
            Item::S9_8,
            Verdict::Fail,
            caps,
            format!("our reply shows as a reply{answers}, but {official_note}"),
        );
    } else {
        ctx.decide(
            Item::S9_8,
            Verdict::Pass,
            caps,
            format!("reply shows as a reply in Teams{answers}; {official_note}"),
        );
    }
    Ok(())
}

/// §9.15: does the quote of our reply link to the original (`qtdMsgs`)?
/// Asked only when the reply shows as one and `qtdMsgs` was sent.
fn decide_quote_link(
    ctx: &mut Ctx,
    label: &str,
    author: &str,
    shown: bool,
    quoted: bool,
    cap: Option<String>,
) {
    if !quoted {
        ctx.decide(
            Item::S9_15,
            Verdict::Unverified,
            cap,
            "not sent: --no-qtd-msgs",
        );
        return;
    }
    if !shown {
        ctx.decide(
            Item::S9_15,
            Verdict::Unverified,
            cap,
            "the reply does not show as a reply, so its quote link cannot be checked",
        );
        return;
    }
    let jumps = ctx.ask(&format!(
        "In {label} in Teams, click the quote at the top of \"{REPLY_TEXT}\": does it jump to {author}'s original message?"
    ));
    let answers = ctx.answers();
    if jumps {
        ctx.decide(
            Item::S9_15,
            Verdict::Pass,
            cap,
            format!("reply sent with properties.qtdMsgs; clicking its quote jumps to the original{answers}"),
        );
    } else {
        ctx.decide(
            Item::S9_15,
            Verdict::Fail,
            cap,
            format!(
                "reply sent with properties.qtdMsgs, but clicking its quote does not jump to the original{answers}"
            ),
        );
    }
}

/// §9.6: 👍 on our own message, then remove it; the user confirms both.
async fn write_react(
    ctx: &mut Ctx,
    conv: &ConversationId,
    label: &str,
    own: Option<&str>,
) -> anyhow::Result<()> {
    println!("\n-- §9.6: react / unreact --");
    let Some(cmid) = own else {
        ctx.decide(
            Item::S9_6,
            Verdict::Unverified,
            None,
            "the formatting message was not sent, so there is no own message to react to",
        );
        return Ok(());
    };
    let Some(msg) = find_own(ctx.api(), conv, cmid).await? else {
        ctx.decide(
            Item::S9_6,
            Verdict::Unverified,
            None,
            format!(
                "lookup miss: the smoke message (clientmessageid {cmid}) is not among the latest {RECENT} \
                 messages of {}; nothing was reacted to",
                conv.0
            ),
        );
        return Ok(());
    };
    ctx.guard_write(conv)?;
    let mark = ctx.capture_mark();
    let reacted = ctx.api().react(conv, &msg.id, &Reaction::Like).await;
    let cap_react = ctx.capture_since(&mark);
    if let Err(e) = reacted {
        ctx.decide(
            Item::S9_6,
            Verdict::Fail,
            cap_react,
            format!("react: {}", ic3_err_text(&e)),
        );
        return Ok(());
    }
    let shown = ctx.ask(&format!(
        "In {label} in Teams, does a 👍 now show on your \"ic3 smoke: …\" message?"
    ));
    ctx.guard_write(conv)?;
    let mark = ctx.capture_mark();
    let unreacted = ctx.api().unreact(conv, &msg.id, &Reaction::Like).await;
    let cap_unreact = ctx.capture_since(&mark);
    let caps = join_captures(&[cap_react, cap_unreact]);
    if let Err(e) = unreacted {
        ctx.decide(
            Item::S9_6,
            Verdict::Fail,
            caps,
            format!(
                "react 2xx (👍 shown: {}); unreact: {}",
                yes(shown),
                ic3_err_text(&e)
            ),
        );
        return Ok(());
    }
    let gone = ctx.ask(&format!(
        "In {label}, has the 👍 on that message disappeared again?"
    ));
    let answers = ctx.answers();
    if shown && gone {
        ctx.decide(
            Item::S9_6,
            Verdict::Pass,
            caps,
            format!("react and unreact 2xx; the 👍 appeared and disappeared in Teams{answers}"),
        );
    } else {
        ctx.decide(
            Item::S9_6,
            Verdict::Fail,
            caps,
            format!(
                "react and unreact 2xx, but in Teams the 👍 {}{answers}",
                if shown {
                    "did not go away"
                } else {
                    "never showed"
                }
            ),
        );
    }
    Ok(())
}

/// A conversation's read state as the server holds it (§9.7).
#[derive(Clone, Debug, Default, PartialEq)]
struct HorizonProps {
    horizon: Option<String>,
    bookmark: Option<String>,
}

impl HorizonProps {
    fn of(c: &Conversation) -> HorizonProps {
        let props = c.extra.get("properties");
        let get = |k: &str| {
            props
                .and_then(|p| p.get(k))
                .and_then(Value::as_str)
                .map(str::to_owned)
        };
        HorizonProps {
            horizon: get("consumptionhorizon"),
            bookmark: get("consumptionHorizonBookmark"),
        }
    }

    /// `consumptionhorizon …, consumptionHorizonBookmark …` (`absent`, or
    /// `""` when empty).
    fn line(&self) -> String {
        format!(
            "consumptionhorizon {}, consumptionHorizonBookmark {}",
            show_prop(&self.horizon),
            show_prop(&self.bookmark)
        )
    }

    /// The bookmark marks the chat unread (§9.7b): non-empty, and its first
    /// `;` field is not `0` (Teams' own cleared form is `0;<ms>;0`).
    fn bookmark_set(&self) -> bool {
        self.bookmark
            .as_deref()
            .is_some_and(|b| !b.is_empty() && b.split(';').next() != Some("0"))
    }
}

/// A read-state property as printed: `absent`, `""` when empty, else as is.
fn show_prop(v: &Option<String>) -> String {
    match v.as_deref() {
        None => "absent".to_owned(),
        Some("") => "\"\"".to_owned(),
        Some(s) => s.to_owned(),
    }
}

/// Most conversations pages read looking for the target chat's read state.
const HORIZON_LOOKUP_PAGES: usize = 5;

/// `conv`'s read state from the conversations listing; `None` when it is
/// not among the first pages.
async fn horizon_props(
    api: &Api,
    conv: &ConversationId,
) -> ic3_teams::Result<Option<HorizonProps>> {
    let mut cursor = None;
    for _ in 0..HORIZON_LOOKUP_PAGES {
        let page = api.conversations(None, cursor.take()).await?;
        if let Some(c) = page.items.iter().find(|c| &c.id == conv) {
            return Ok(Some(HorizonProps::of(c)));
        }
        match page.next {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    Ok(None)
}

/// Reads and prints `conv`'s read state `when`; `None` if it could not be
/// read. Returns the capture file too.
async fn show_horizon(
    ctx: &Ctx,
    conv: &ConversationId,
    when: &str,
) -> (Option<HorizonProps>, Option<String>, String) {
    let mark = ctx.capture_mark();
    let r = horizon_props(ctx.api(), conv).await;
    let cap = ctx.capture_since(&mark);
    let (props, text) = match r {
        Ok(Some(p)) => {
            let line = p.line();
            (Some(p), line)
        }
        Ok(None) => (None, format!("{} not in the conversations listing", conv.0)),
        Err(e) => (None, format!("could not read: {}", ic3_err_text(&e))),
    };
    println!(
        "  server, {when}: {text}  [{}]",
        cap.as_deref().unwrap_or("no capture")
    );
    (props, cap, format!("{when}: {text}"))
}

/// §9.7 from what the server holds after our PUT and the user's answer:
/// PASS needs both the server-side horizon to be the one sent (`sent`, at a
/// time within `window`) and the badge to have cleared. When the horizon
/// read once the chat was unread (`unread`) already pointed at `sent`'s
/// message, the PUT's effect cannot be seen: UNVERIFIED.
fn mark_read_verdict(
    sent: &MessageRef,
    window: (Timestamp, Timestamp),
    unread: Option<&HorizonProps>,
    after: Option<&HorizonProps>,
    badge_cleared: bool,
) -> (Verdict, String) {
    let points_at_sent = |p: &HorizonProps| {
        p.horizon
            .as_deref()
            .and_then(ReadHorizon::parse)
            .is_some_and(|h| {
                h.message_id == sent.id && h.client_message_id == sent.client_message_id
            })
    };
    if let Some(u) = unread.filter(|u| points_at_sent(u)) {
        return (
            Verdict::Unverified,
            format!(
                "once unread, the server horizon already pointed at {} ({}), so the PUT's effect is \
                 not observable (the badge {}cleared; consumptionHorizonBookmark {}): make the chat \
                 unread with a new message from someone else",
                sent.id.0,
                u.horizon.as_deref().unwrap_or(""),
                if badge_cleared { "" } else { "did not " },
                u.bookmark.as_deref().unwrap_or("absent")
            ),
        );
    }
    let Some(after) = after else {
        return if badge_cleared {
            (
                Verdict::Unverified,
                "the badge cleared, but the server horizon could not be read".into(),
            )
        } else {
            (
                Verdict::Fail,
                "badge not cleared; the server horizon could not be read".into(),
            )
        };
    };
    let shown = after.horizon.as_deref().unwrap_or("absent");
    let parsed = after.horizon.as_deref().and_then(ReadHorizon::parse);
    let ours = points_at_sent(after);
    let exact = parsed
        .as_ref()
        .is_some_and(|h| (window.0..=window.1).contains(&h.at));
    let bookmark = after.bookmark.as_deref().unwrap_or("absent");
    match (ours, badge_cleared) {
        (true, true) => (
            Verdict::Pass,
            format!(
                "server horizon now {shown} (= sent{}); the badge cleared",
                if exact {
                    ""
                } else {
                    "; timestamp differs from ours: the server rewrote it"
                }
            ),
        ),
        (true, false) => (
            Verdict::Fail,
            format!(
                "server accepted; badge not cleared (possibly mark-as-unread bookmark: \
                 consumptionHorizonBookmark {bookmark}); server horizon {shown}"
            ),
        ),
        (false, true) => (
            Verdict::Fail,
            format!(
                "the badge cleared, but the server horizon is {shown}, not ours ({})",
                sent.id.0
            ),
        ),
        (false, false) => (
            Verdict::Fail,
            format!("server horizon did not move ({shown}); badge not cleared"),
        ),
    }
}

/// §9.7b from the bookmark once unread and after our call, and the user's
/// answer: PASS needs the bookmark set once unread, cleared after our PUT,
/// and the badge gone. A bookmark never set once unread means Teams used
/// the horizon instead (§9.7 decides that case): UNVERIFIED, as is a run
/// that did not send the PUT (`sent` false: `--no-clear-bookmark`).
fn bookmark_verdict(
    sent: bool,
    unread: Option<&HorizonProps>,
    after: Option<&HorizonProps>,
    badge_cleared: bool,
) -> (Verdict, String) {
    if !sent {
        return (Verdict::Unverified, "not sent: --no-clear-bookmark".into());
    }
    let Some(unread) = unread else {
        return (
            Verdict::Unverified,
            "the bookmark could not be read once unread".into(),
        );
    };
    let was = show_prop(&unread.bookmark);
    if !unread.bookmark_set() {
        return (
            Verdict::Unverified,
            format!(
                "the bookmark was never set once unread ({was}): Teams used the horizon instead \
                 (§9.7 decides that case)"
            ),
        );
    }
    let badge = if badge_cleared {
        "the badge cleared"
    } else {
        "the badge did not clear"
    };
    match after {
        Some(a) if a.bookmark_set() => (
            Verdict::Fail,
            format!(
                "consumptionHorizonBookmark still set after our PUT ({}; once unread {was}); {badge}",
                show_prop(&a.bookmark)
            ),
        ),
        _ if !badge_cleared => (
            Verdict::Fail,
            format!(
                "the badge did not clear while the bookmark was set (once unread {was}; after our PUT {})",
                after.map_or_else(
                    || "could not be read".to_owned(),
                    |a| show_prop(&a.bookmark)
                )
            ),
        ),
        None => (
            Verdict::Unverified,
            format!(
                "the badge cleared, but the bookmark could not be read after our PUT (once unread {was})"
            ),
        ),
        Some(a) => (
            Verdict::Pass,
            format!(
                "consumptionHorizonBookmark once unread {was}, after our PUT {}; {badge}",
                show_prop(&a.bookmark)
            ),
        ),
    }
}

/// §9.7: the user makes the chat unread while looking at another chat,
/// smoke marks it read, the user checks the chat list without opening it
/// (opening it would clear the marker by itself). The server's read state
/// is printed before, once unread, and after the PUT.
async fn write_mark_read(ctx: &mut Ctx, conv: &ConversationId, label: &str) -> anyhow::Result<()> {
    println!("\n-- §9.7: mark read --");
    let (_, _, before) = show_horizon(ctx, conv, "before mark-as-unread").await;
    // Nobody else can post to the self chat: there, Mark as unread is the
    // only way.
    let (or_message, caveat) = if is_notes(conv) {
        (
            "",
            "\n(Mark as unread may only set a bookmark that a read horizon does not clear: for a \
             decisive §9.7, use --conv with a chat where someone else sends you a message.)",
        )
    } else {
        (", or (better) have someone else send a message to it", "")
    };
    ctx.wait(&format!(
        "In Teams, first switch to a DIFFERENT chat. Then mark {label} unread (hover it in the chat list > \
         … > Mark as unread){or_message}. Don't open it again until asked. Press Enter when done.{caveat}"
    ));
    if !ctx.ask(&format!(
        "Does {label} now show as unread (bold / badge) in the Teams chat list?"
    )) {
        let note = format!(
            "{} could not be made unread first, so marking it read cannot be observed; nothing was sent{}",
            conv.0,
            ctx.answers()
        );
        ctx.decide(Item::S9_7, Verdict::Unverified, None, note.clone());
        ctx.decide(Item::S9_7b, Verdict::Unverified, None, note);
        return Ok(());
    }
    let (unread_props, _, unread) = show_horizon(ctx, conv, "once unread").await;
    let page = ctx.api().messages(conv, None, PAGING_PAGE).await?;
    let newest = page
        .items
        .iter()
        .max_by(|a, b| a.id.cmp(&b.id))
        .ok_or_else(|| anyhow!("{} has no messages to mark read", conv.0))?;
    let horizon = MessageRef {
        id: newest.id.clone(),
        client_message_id: newest.client_message_id.clone(),
    };
    ctx.guard_write(conv)?;
    let mark = ctx.capture_mark();
    let t0 = now_ms();
    let marked = ctx.api().mark_read(conv, &horizon).await;
    let t1 = now_ms();
    // mark_read sends the horizon PUT, then (unless --no-clear-bookmark)
    // the bookmark PUT, each with its own capture.
    let dir = ctx.capture_dir();
    let last_after = |needle: &str| captures_after(dir.as_deref(), mark.as_deref(), needle).pop();
    let cap = last_after("-http-mark-read.json");
    let bookmark_cap = last_after("-http-mark-read-bookmark.json");
    let clear_bookmark = ctx.client.options().clear_unread_bookmark;
    // The horizon PUT goes first: once its capture shows a 2xx, a failure
    // is the bookmark PUT's, with or without a response.
    let horizon_accepted = dir
        .as_deref()
        .zip(cap.as_deref())
        .and_then(|(d, f)| read_capture(d, f))
        .and_then(|c| c["response"]["status"].as_u64())
        .is_some_and(|s| (200..300).contains(&s));
    let bookmark_failed = match marked {
        Err(e) if horizon_accepted && clear_bookmark => Some(ic3_err_text(&e)),
        Err(e) => {
            let what = format!("mark_read: {}", ic3_err_text(&e));
            ctx.decide(Item::S9_7, Verdict::Fail, cap, what);
            let why = if clear_bookmark {
                "not sent: the consumptionhorizon PUT failed (§9.7)"
            } else {
                "not sent: --no-clear-bookmark"
            };
            ctx.decide(Item::S9_7b, Verdict::Unverified, None, why);
            return Ok(());
        }
        Ok(()) => None,
    };
    let (after, after_cap, after_line) = show_horizon(ctx, conv, "after our PUT").await;
    let cleared = ctx.ask(&format!(
        "Without opening it, has the unread marker on {label} cleared in the Teams chat list?"
    ));
    let answers = ctx.answers();
    let (v, what) = mark_read_verdict(
        &horizon,
        (t0, t1),
        unread_props.as_ref(),
        after.as_ref(),
        cleared,
    );
    ctx.decide(
        Item::S9_7,
        v,
        join_captures(&[cap, after_cap.clone()]),
        format!(
            "consumptionhorizon PUT 2xx (horizon {}); {what}{answers}; {before}; {unread}; {after_line}",
            horizon.id.0
        ),
    );
    if let Some(why) = bookmark_failed {
        ctx.decide(
            Item::S9_7b,
            Verdict::Fail,
            bookmark_cap,
            format!("consumptionHorizonBookmark PUT failed: {why}"),
        );
    } else {
        let (v, what) = bookmark_verdict(
            clear_bookmark,
            unread_props.as_ref(),
            after.as_ref(),
            cleared,
        );
        let cap = bookmark_cap.and_then(|b| join_captures(&[Some(b), after_cap]));
        ctx.decide(Item::S9_7b, v, cap, format!("{what}{answers}"));
    }
    Ok(())
}

// ----- events ---------------------------------------------------------------

/// The mapped events §9.4 needs to see at least once.
const REQUIRED_EVENTS: [&str; 5] = [
    "NewMessage",
    "MessageEdited",
    "MessageDeleted",
    "ReactionsChanged",
    "ReadStateChanged",
];

/// The thread properties `ConversationKind::classify` reads.
const KIND_KEYS: [&str; 2] = ["productThreadType", "uniquerosterthread"];

/// The events stage's tally, shared with the raw event hook.
type Tally = Arc<Mutex<EventTally>>;

fn lock(t: &Tally) -> MutexGuard<'_, EventTally> {
    t.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What the events stage saw, for the §9.3–9.5 verdicts.
#[derive(Debug, Default)]
struct EventTally {
    /// EventMessages per `resourceType`.
    by_resource_type: BTreeMap<String, usize>,
    /// Mapped events per name (`RawEvent::mapped`).
    by_mapped: BTreeMap<String, usize>,
    connected: bool,
    /// `Disconnected` reasons with when they arrived, in order.
    disconnects: Vec<(String, Timestamp)>,
    /// When the Skype token was first renewed during the run.
    renewal_at: Option<Timestamp>,
    /// The expiry of the token replaced by that renewal: the socket opened
    /// before it authenticated with that token, which stays valid until
    /// then (the renewal happens at 80 % of its life).
    old_expiry: Option<Timestamp>,
    /// The first EventMessage after `old_expiry`.
    first_event_after_old_expiry: Option<Timestamp>,
    first_event_at: Option<Timestamp>,
    last_event_at: Option<Timestamp>,
    /// When the stage stopped listening.
    ended_at: Option<Timestamp>,
    /// A `TokenRenewal` disconnect was followed by `Connected`.
    reconnected_after_renewal: bool,
    /// Ids of message events with blank content and no
    /// `properties.deletetime` (a delete that only blanks content?).
    blank_without_deletetime: Vec<String>,
    /// One line per ThreadUpdate: where its kind keys were.
    thread_kinds: Vec<String>,
    /// ThreadUpdates whose kind from `properties` (what ic3-teams reads) differs
    /// from the kind from `threadProperties`.
    thread_kind_mismatches: Vec<String>,
}

impl EventTally {
    /// One EventMessage of `resource_type`, mapped to `mapped`.
    fn record(&mut self, resource_type: &str, mapped: &[&str]) {
        *self
            .by_resource_type
            .entry(resource_type.to_owned())
            .or_default() += 1;
        for m in mapped {
            *self.by_mapped.entry((*m).to_owned()).or_default() += 1;
        }
    }

    fn note_connected(&mut self) {
        if self.disconnects.last().map(|(r, _)| r.as_str()) == Some("TokenRenewal") {
            self.reconnected_after_renewal = true;
        }
        self.connected = true;
    }

    fn note_disconnect(&mut self, reason: &str, at: Timestamp) {
        self.disconnects.push((reason.to_owned(), at));
    }

    /// The Skype token was renewed at `at`; the replaced one expires at
    /// `old_expiry`. Only the first renewal counts.
    fn note_renewal(&mut self, at: Timestamp, old_expiry: Timestamp) {
        if self.renewal_at.is_none() {
            self.renewal_at = Some(at);
            self.old_expiry = Some(old_expiry);
        }
    }

    fn note_event_at(&mut self, ts: Timestamp) {
        self.first_event_at.get_or_insert(ts);
        self.last_event_at = Some(self.last_event_at.map_or(ts, |l| l.max(ts)));
        if self.old_expiry.is_some_and(|old| ts > old) {
            self.first_event_after_old_expiry.get_or_insert(ts);
        }
    }

    fn note_end_at(&mut self, ts: Timestamp) {
        self.ended_at = Some(ts);
    }

    /// Looks for the two shapes Task 14 left open (§9.4) in a resource that
    /// was mapped to `mapped`; returns a line to print when there is
    /// something to say.
    fn observe(
        &mut self,
        resource_type: &str,
        resource: &Value,
        mapped: &[&str],
    ) -> Option<String> {
        let id = resource.get("id").and_then(Value::as_str).unwrap_or("?");
        match resource_type {
            "NewMessage" | "MessageUpdate" => {
                // Only what would show as a (blank) message: a reaction or
                // a recognised delete with blank content is fine.
                let shown = mapped
                    .iter()
                    .any(|m| *m == "NewMessage" || *m == "MessageEdited");
                let blank = resource
                    .get("content")
                    .and_then(Value::as_str)
                    .is_none_or(|c| c.trim().is_empty());
                let delete_marker = resource
                    .pointer("/properties/deletetime")
                    .is_some_and(|v| !v.is_null() && v.as_str() != Some(""));
                if !(shown && blank && !delete_marker) {
                    return None;
                }
                self.blank_without_deletetime.push(id.to_owned());
                Some(format!(
                    "§9.4: {resource_type} {id} has blank content and no properties.deletetime: if this \
                     was a delete, api/wire.rs::parse_message does not see it"
                ))
            }
            "ThreadUpdate" => {
                let props = |k: &str| {
                    resource
                        .get(k)
                        .and_then(Value::as_object)
                        .cloned()
                        .unwrap_or_default()
                };
                let (props, thread_props) = (props("properties"), props("threadProperties"));
                let keys = |m: &Map<String, Value>| {
                    KIND_KEYS
                        .iter()
                        .filter_map(|k| m.get(*k).map(|v| format!("{k}={v}")))
                        .collect::<Vec<_>>()
                        .join(" ")
                };
                let used = ConversationKind::classify(id, &props);
                let mut line = format!(
                    "ThreadUpdate {id}: kind keys under properties [{}], under threadProperties [{}]; \
                     kind {used:?}",
                    keys(&props),
                    keys(&thread_props)
                );
                if !thread_props.is_empty() {
                    let alt = ConversationKind::classify(id, &thread_props);
                    if alt != used {
                        line.push_str(&format!(" (threadProperties say {alt:?})"));
                        self.thread_kind_mismatches.push(line.clone());
                    }
                }
                self.thread_kinds.push(line.clone());
                Some(line)
            }
            _ => None,
        }
    }

    fn events_seen(&self) -> usize {
        self.by_resource_type.values().sum()
    }

    /// §9.3: `Connected` and at least one `/messaging` EventMessage.
    fn verdict_9_3(&self) -> Verdict {
        if !self.connected {
            Verdict::Fail
        } else if self.events_seen() == 0 {
            Verdict::Unverified
        } else {
            Verdict::Pass
        }
    }

    /// §9.4: every [`REQUIRED_EVENTS`] kind mapped at least once and neither
    /// open shape seen; the missing kinds.
    fn verdict_9_4(&self) -> (Verdict, Vec<String>) {
        let missing: Vec<String> = REQUIRED_EVENTS
            .iter()
            .filter(|k| !self.by_mapped.contains_key(**k))
            .map(|k| (*k).to_owned())
            .collect();
        let v = if self.events_seen() == 0 {
            Verdict::Unverified
        } else if !missing.is_empty()
            || !self.blank_without_deletetime.is_empty()
            || !self.thread_kind_mismatches.is_empty()
        {
            Verdict::Fail
        } else {
            Verdict::Pass
        };
        (v, missing)
    }

    /// The §9.4 note: counts plus the two open shapes.
    fn notes_9_4(&self) -> String {
        let mut parts = vec![
            format!("mapped: {}", counts(&self.by_mapped)),
            format!("resourceTypes: {}", counts(&self.by_resource_type)),
        ];
        if !self.blank_without_deletetime.is_empty() {
            parts.push(format!(
                "blank content without properties.deletetime on {} (a delete that only blanks content is \
                 not recognised: api/wire.rs::parse_message)",
                self.blank_without_deletetime.join(", ")
            ));
        }
        if !self.thread_kind_mismatches.is_empty() {
            parts.push(format!(
                "ThreadUpdate kind differs between properties and threadProperties (events/mod.rs::\
                 map_event_message reads properties): {}",
                self.thread_kind_mismatches.join("; ")
            ));
        } else if self.thread_kinds.is_empty() {
            parts.push("no ThreadUpdate seen (rename a group chat to check its kind keys)".into());
        } else {
            parts.push(self.thread_kinds.join("; "));
        }
        parts.join("; ")
    }

    /// §9.5: whether delivery survived a Skype-token renewal.
    #[cfg(test)]
    fn verdict_9_5(&self, reconnect_enabled: bool) -> Verdict {
        self.assess_9_5(reconnect_enabled).0
    }

    /// §9.5 verdict and note. With the default, PASS needs the TokenRenewal
    /// reconnect. Without reconnecting, the socket keeps the token it
    /// authenticated with until that one expires (`old_expiry`), so only an
    /// event after `old_expiry` on the same socket (no disconnect since the
    /// renewal) answers the question.
    fn assess_9_5(&self, reconnect_enabled: bool) -> (Verdict, String) {
        let longer = format!("run with --minutes {RENEWAL_MINUTES}");
        let (Some(renewal), Some(old)) = (self.renewal_at, self.old_expiry) else {
            if reconnect_enabled && self.reconnected_after_renewal {
                return (
                    Verdict::Pass,
                    "trouter reconnected after a Skype-token renewal (TokenRenewal, then Connected)"
                        .into(),
                );
            }
            return (
                Verdict::Unverified,
                format!("no Skype-token renewal during the run: {longer}"),
            );
        };
        let (renewal, old_at) = (local_time(renewal), local_time(old));
        if reconnect_enabled {
            return if self.reconnected_after_renewal {
                (
                    Verdict::Pass,
                    format!(
                        "the Skype-token renewal at {renewal} reconnected trouter (TokenRenewal, then \
                         Connected): the default works; --no-reconnect-on-renewal tells whether the \
                         reconnect is needed"
                    ),
                )
            } else {
                (
                    Verdict::Unverified,
                    format!(
                        "the Skype token was renewed at {renewal} but no TokenRenewal reconnect followed"
                    ),
                )
            };
        }
        let after = self.first_event_after_old_expiry;
        let reconnect = self
            .disconnects
            .iter()
            .find(|(_, at)| *at >= self.renewal_at.unwrap_or_default());
        if let Some((why, at)) = reconnect
            && after.is_none_or(|e| *at <= e)
        {
            return (
                Verdict::Unverified,
                format!(
                    "trouter reconnected ({why} at {}) after the renewal at {renewal} and before any event \
                     past the old token's expiry at {old_at}: the old socket was not tested; run again",
                    local_time(*at)
                ),
            );
        }
        if let Some(e) = after {
            return (
                Verdict::Pass,
                format!(
                    "an event arrived at {} on the socket opened before the renewal at {renewal}, after its \
                     token expired at {old_at}: reconnecting is not needed",
                    local_time(e)
                ),
            );
        }
        if self.ended_at.is_some_and(|end| end <= old) {
            return (
                Verdict::Unverified,
                format!(
                    "the run ended at {} before the old token expired at {old_at}: {longer}",
                    self.ended_at.map(local_time).unwrap_or_default()
                ),
            );
        }
        match self.last_event_at {
            Some(last) => (
                Verdict::Fail,
                format!(
                    "no event arrived after the old token expired at {old_at} (renewed at {renewal}; the last \
                     event at {}) although the socket stayed up: keep reconnect_on_token_renewal = true (if \
                     you did send a message after {old_at})",
                    local_time(last)
                ),
            ),
            None => (
                Verdict::Unverified,
                format!("the Skype token was renewed at {renewal} but no events arrived at all"),
            ),
        }
    }

    /// The three verdicts with their notes.
    fn verdicts(
        &self,
        reconnect_enabled: bool,
        register_all_apps: bool,
    ) -> [(Item, Verdict, String); 3] {
        let reasons: Vec<&str> = self.disconnects.iter().map(|(r, _)| r.as_str()).collect();
        let disconnects = if reasons.is_empty() {
            String::new()
        } else {
            format!("; disconnects: {}", reasons.join(", "))
        };
        let retry = if register_all_apps {
            "it failed with --register-all-apps too"
        } else {
            "retry with --register-all-apps"
        };
        let v3 = self.verdict_9_3();
        let n3 = match v3 {
            Verdict::Pass => format!(
                "Connected; {} EventMessage(s) ({}){disconnects}",
                self.events_seen(),
                counts(&self.by_resource_type)
            ),
            Verdict::Fail => format!(
                "never Connected (last disconnect: {}); {retry}",
                reasons.last().copied().unwrap_or("none")
            ),
            _ => format!(
                "Connected, but no /messaging EventMessage arrived (act in Teams while it listens; if you \
                 did, {retry}){disconnects}"
            ),
        };
        let (v4, missing) = self.verdict_9_4();
        let n4 = match v4 {
            Verdict::Unverified => "no EventMessage seen".to_owned(),
            _ if !missing.is_empty() => {
                format!("never mapped: {}; {}", missing.join(", "), self.notes_9_4())
            }
            _ => self.notes_9_4(),
        };
        let (v5, n5) = self.assess_9_5(reconnect_enabled);
        [
            (Item::S9_3, v3, n3),
            (Item::S9_4, v4, n4),
            (Item::S9_5, v5, n5),
        ]
    }
}

/// `a×2, b×1`, or `none`.
fn counts(m: &BTreeMap<String, usize>) -> String {
    if m.is_empty() {
        return "none".into();
    }
    m.iter()
        .map(|(k, n)| format!("{k}×{n}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Local wall-clock time of a millisecond timestamp.
fn local_time(ms: Timestamp) -> String {
    chrono::DateTime::from_timestamp_millis(ms).map_or_else(
        || ms.to_string(),
        |t| {
            t.with_timezone(&chrono::Local)
                .format("%H:%M:%S")
                .to_string()
        },
    )
}

/// Now, local, with milliseconds: the prefix of every events-stage line.
fn now_local() -> String {
    chrono::Local::now().format("%H:%M:%S%.3f").to_string()
}

/// Prints every EventMessage trouter accepts and records it in `tally`.
fn raw_event_hook(tally: Tally) -> RawEventHook {
    Arc::new(move |raw: &RawEvent| {
        println!("{}  {}", now_local(), raw_line(raw));
        let resource = raw.body.get("resource").unwrap_or(&Value::Null);
        let mapped: Vec<&str> = raw.mapped.iter().map(String::as_str).collect();
        let mut t = lock(&tally);
        t.record(&raw.resource_type, &mapped);
        t.note_event_at(now_ms());
        if let Some(note) = t.observe(&raw.resource_type, resource, &mapped) {
            println!("              {note}");
        }
    })
}

/// The resource fields that decide the mapping, shown first in an excerpt.
const EXCERPT_KEYS: [&str; 6] = [
    "messagetype",
    "skypeeditedid",
    "properties",
    "threadProperties",
    "topic",
    "content",
];

/// `resourceType → [mapped…]  <resource excerpt>`.
fn raw_line(raw: &RawEvent) -> String {
    let mapped = if raw.mapped.is_empty() {
        "(dropped)".to_owned()
    } else {
        format!("[{}]", raw.mapped.join(", "))
    };
    let resource = raw.body.get("resource").unwrap_or(&Value::Null);
    // §9.7: the read horizon and the mark-as-unread bookmark, never
    // truncated away.
    let read_state = if raw.resource_type == "ConversationUpdate" {
        let prop = |k: &str| {
            resource
                .pointer(&format!("/properties/{k}"))
                .map_or_else(|| "absent".to_owned(), Value::to_string)
        };
        format!(
            "consumptionhorizon={} consumptionHorizonBookmark={}  ",
            redact_text(&prop("consumptionhorizon")),
            redact_text(&prop("consumptionHorizonBookmark"))
        )
    } else {
        String::new()
    };
    format!(
        "{:<18} → {mapped}  {read_state}{}",
        raw.resource_type,
        resource_excerpt(resource)
    )
}

/// At most 120 characters of `resource` (already redacted by trouter; its
/// text is redacted again for good measure), the [`EXCERPT_KEYS`] first;
/// the whole payload is in the capture file.
fn resource_excerpt(resource: &Value) -> String {
    let parts: Vec<String> = EXCERPT_KEYS
        .iter()
        .filter_map(|k| resource.get(*k).map(|v| format!("{k}={v}")))
        .collect();
    let text = if parts.is_empty() {
        resource.to_string()
    } else {
        parts.join(" ")
    };
    truncate(&redact_text(&text), RAW_EXCERPT_CHARS)
}

/// A message's text for an event line.
fn text_excerpt(m: &Message) -> String {
    truncate(
        &redact_text(&m.body.plain_text().replace('\n', " ")),
        TEXT_EXCERPT_CHARS,
    )
}

/// One line per event from the stream.
fn describe_event(e: &Event) -> String {
    match e {
        Event::Presence(list) => format!(
            "Presence: {}",
            list.iter()
                .map(|p| format!("{} {}/{}", p.mri.0, p.availability, p.activity))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Event::NewMessage(m) => format!(
            "NewMessage from {}: {}  [{}]",
            m.from_name.as_deref().unwrap_or(&m.from.0),
            text_excerpt(m),
            m.conversation.0
        ),
        Event::MessageEdited(m) => format!(
            "MessageEdited {} in {}: {}",
            m.id.0,
            m.conversation.0,
            text_excerpt(m)
        ),
        Event::MessageDeleted {
            conversation,
            message,
            ..
        } => format!("MessageDeleted {} in {}", message.0, conversation.0),
        Event::ReactionsChanged {
            conversation,
            message,
            reactions,
            ..
        } => {
            let set = if reactions.is_empty() {
                "none".to_owned()
            } else {
                reactions
                    .iter()
                    .map(|r| format!("{}×{}", r.reaction.key(), r.users.len()))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            format!(
                "ReactionsChanged {} in {}: {set}",
                message.0, conversation.0
            )
        }
        Event::ReadStateChanged {
            conversation,
            horizon,
        } => format!(
            "ReadStateChanged {}: read up to {} at {}",
            conversation.0,
            horizon.message_id.0,
            local_time(horizon.at)
        ),
        Event::ConversationUpdated(c) => format!(
            "ConversationUpdated {} ({:?}{}{})",
            c.id.0,
            c.kind,
            c.topic
                .as_deref()
                .map(|t| format!(", topic \"{}\"", redact_text(t)))
                .unwrap_or_default(),
            if c.version == 0 {
                ", refetch marker"
            } else {
                ""
            }
        ),
        Event::Typing {
            conversation,
            from,
            from_name,
        } => format!(
            "Typing in {} by {} (from_name {})",
            conversation.0,
            from_name
                .as_deref()
                .map_or_else(|| from.0.clone(), redact_text),
            if from_name.is_some() {
                "present"
            } else {
                "absent"
            }
        ),
        Event::Connected => "Connected".into(),
        Event::NeedsCatchUp { reason } => format!("NeedsCatchUp {{ reason: {reason:?} }}"),
        Event::Disconnected { reason, retry_in } => format!(
            "Disconnected {{ reason: {reason:?}, retry in {}s }}",
            retry_in.as_secs()
        ),
        Event::ReauthRequired => {
            "ReauthRequired: the session is no longer usable; run smoke again to sign in".into()
        }
    }
}

/// What the user is asked to do while the events stage listens.
fn event_script(label: &str, minutes: u64, notes: bool) -> String {
    let read = if notes {
        "6) read a chat with a new message from someone else on your phone or in another Teams client (the read \
         marker can't move in your Notes chat)"
    } else {
        "6) after someone else posts in it, read this chat on your phone or in another Teams client"
    };
    let mut s = format!(
        "Listening for {minutes} minute(s); Ctrl-C ends early. Events come from all your chats.\n\
         In the official Teams client, in {label}:\n  \
         1) send a message\n  2) edit it\n  3) react to it\n  4) remove the reaction\n  5) delete it\n  \
         {read}\n  7) rename a group chat (if you have one); a 1:1 rename too, if Teams lets you\n  \
         8) mark a chat unread (… > Mark as unread), then open it again: the ConversationUpdate lines \
         show consumptionHorizonBookmark (§9.7)\n\
         Start once `Connected` is printed below. Each should print within ~1 s: \
         `resourceType → [mapped events]`, then the decoded event."
    );
    if minutes < RENEWAL_MINUTES {
        s.push_str(&format!(
            "\n(§9.5 needs the run to span a Skype-token renewal: --minutes {RENEWAL_MINUTES}.)"
        ));
    }
    s
}

/// `HH:MM:SS (in N min)`, or `HH:MM:SS (now)` once due.
fn at_in(ts: Timestamp, now: Timestamp) -> String {
    let left = ts - now;
    if left <= 0 {
        format!("{} (now)", local_time(ts))
    } else {
        format!("{} (in {} min)", local_time(ts), (left + 59_999) / 60_000)
    }
}

/// What to print once the Skype token's expiry is known: when it expires
/// and when it will be renewed (80 % of its life, counted from `now`: it was
/// fetched for this run), with a warning when §9.5 cannot be answered
/// before the run ends at `run_end`.
fn expiry_plan(
    now: Timestamp,
    expires: Timestamp,
    run_end: Timestamp,
    reconnect: bool,
) -> Vec<String> {
    let renewal = now + (expires - now).max(0) * 4 / 5;
    let mut lines = vec![format!(
        "Skype token expires at {}; renewal expected around {}",
        at_in(expires, now),
        at_in(renewal, now)
    )];
    let end = local_time(run_end);
    if renewal > run_end {
        lines.push(format!(
            "WARNING: the renewal falls after this run ends at {end}, so §9.5 will stay UNVERIFIED: use \
             --minutes {RENEWAL_MINUTES}"
        ));
    } else if !reconnect && expires > run_end {
        lines.push(format!(
            "WARNING: without reconnecting, §9.5 needs an event after the old token expires at {}, but this \
             run ends at {end}: use --minutes {RENEWAL_MINUTES}",
            local_time(expires)
        ));
    }
    lines
}

/// What to print when the Skype token was renewed at `now`; `old` is the
/// replaced token's expiry, `next` the new one's.
fn renewal_prompt(
    now: Timestamp,
    old: Timestamp,
    next: Timestamp,
    run_end: Timestamp,
    reconnect: bool,
    label: &str,
) -> String {
    let head = format!("Skype token RENEWED (next expiry {}).", local_time(next));
    if reconnect {
        return format!(
            "{head} trouter reconnects (TokenRenewal); once `Connected` is printed again, send a message in \
             {label} to see delivery continue."
        );
    }
    let mut s = format!(
        "{head} §9.5: the socket still uses the old token, which expires at {}: send a message in {label} \
         after {}.",
        at_in(old, now),
        local_time(old)
    );
    if old > run_end {
        s.push_str(&format!(
            " WARNING: that is after this run ends at {}, so §9.5 will stay UNVERIFIED.",
            local_time(run_end)
        ));
    }
    s
}

/// How the Skype token's expiry moved.
#[derive(Debug, PartialEq, Eq)]
enum ExpiryChange {
    /// Known for the first time.
    First,
    /// A new token replaced a known one.
    Renewed,
}

/// `prev` → `next` (`0` = no token): `None` when nothing new is known.
fn expiry_change(prev: Timestamp, next: Timestamp) -> Option<ExpiryChange> {
    if next == 0 || next == prev {
        None
    } else if prev == 0 {
        Some(ExpiryChange::First)
    } else {
        Some(ExpiryChange::Renewed)
    }
}

/// Capture files written after `mark` whose name contains `needle`, oldest
/// first (names start with a zero-padded sequence number).
fn captures_after(dir: Option<&Path>, mark: Option<&str>, needle: &str) -> Vec<String> {
    let Some(entries) = dir.and_then(|d| std::fs::read_dir(d).ok()) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains(needle) && mark.is_none_or(|m| n.as_str() > m))
        .collect();
    names.sort();
    names
}

/// `one`, or `first … last (n files)`.
fn file_span(files: &[String]) -> Option<String> {
    match files {
        [] => None,
        [one] => Some(one.clone()),
        [first, .., last] => Some(format!("{first} … {last} ({} files)", files.len())),
    }
}

/// §9.3, §9.4, §9.5: listen for `--minutes` (or until Ctrl-C).
async fn stage_events(ctx: &mut Ctx, minutes: u64) -> anyhow::Result<()> {
    let listen = Duration::from_secs(minutes.saturating_mul(60));
    let stop = async move {
        // A signal handler that cannot be installed just leaves the timer.
        let ctrl_c = async {
            if tokio::signal::ctrl_c().await.is_err() {
                std::future::pending::<()>().await;
            }
        };
        tokio::select! {
            () = tokio::time::sleep(listen) => format!("{minutes} minute(s) are up"),
            () = ctrl_c => "Ctrl-C".to_owned(),
        }
    };
    events_listen(ctx, minutes, stop).await
}

/// Connects, prints every EventMessage (from the hook) and every event until
/// `stop` resolves, then decides §9.3–9.5.
async fn events_listen(
    ctx: &mut Ctx,
    minutes: u64,
    stop: impl Future<Output = String>,
) -> anyhow::Result<()> {
    let me = ctx.client.me().await?;
    println!("Signed in as {} ({})", me.display_name, me.mri.0);
    let conv = ctx.target();
    let label = chat_label(ctx, &conv).await;
    let reconnect = ctx.client.options().reconnect_on_token_renewal;
    let register_all = ctx.client.options().register_all_apps;
    *lock(&ctx.tally) = EventTally::default();
    let mark = ctx.capture_mark();
    let run_end =
        now_ms().saturating_add(i64::try_from(minutes.saturating_mul(60_000)).unwrap_or(i64::MAX));

    let mut tokens = ctx.client.tokens().await.watch();
    let mut skype_expires = tokens.borrow_and_update().skype_expires_at;
    if skype_expires != 0 {
        for line in expiry_plan(now_ms(), skype_expires, run_end, reconnect) {
            println!("{line}");
        }
    }
    // The newest capture when the token was renewed: §9.5 cites what follows.
    let mut renewal_mark: Option<Option<String>> = None;
    println!(
        "trouter: register_all_apps={register_all}, reconnect_on_token_renewal={reconnect}; connecting…"
    );
    let mut stream = ctx.client.connect().await?;
    println!("\n{}\n", event_script(&label, minutes, is_notes(&conv)));

    tokio::pin!(stop);
    let mut reauth = false;
    let why = loop {
        tokio::select! {
            why = &mut stop => break why,
            event = stream.recv() => {
                let Some(event) = event else {
                    break "the event stream ended".to_owned();
                };
                println!("{}  {}", now_local(), describe_event(&event));
                let mut t = lock(&ctx.tally);
                match &event {
                    Event::Connected => t.note_connected(),
                    Event::Disconnected { reason, .. } => {
                        t.note_disconnect(&format!("{reason:?}"), now_ms());
                    }
                    Event::ReauthRequired => reauth = true,
                    _ => {}
                }
            }
            Ok(()) = tokens.changed() => {
                let next = tokens.borrow_and_update().skype_expires_at;
                match expiry_change(skype_expires, next) {
                    None => continue,
                    Some(ExpiryChange::First) => {
                        for line in expiry_plan(now_ms(), next, run_end, reconnect) {
                            println!("{line}");
                        }
                    }
                    Some(ExpiryChange::Renewed) => {
                        let now = now_ms();
                        lock(&ctx.tally).note_renewal(now, skype_expires);
                        if renewal_mark.is_none() {
                            renewal_mark = Some(ctx.capture_mark());
                        }
                        println!(
                            "\n{}  {}\n",
                            now_local(),
                            renewal_prompt(now, skype_expires, next, run_end, reconnect, &label)
                        );
                    }
                }
                skype_expires = next;
            }
        }
    };
    println!("\nstopped listening: {why}");
    lock(&ctx.tally).note_end_at(now_ms());
    drop(stream);
    if reauth {
        // Not a protocol answer: the stage stops and its items stay UNVERIFIED.
        bail!("trouter needs a new sign-in (ReauthRequired); run smoke again to sign in");
    }

    let (verdicts, renewed) = {
        let t = lock(&ctx.tally);
        (
            t.verdicts(reconnect, register_all),
            t.renewal_at.is_some() || t.reconnected_after_renewal,
        )
    };
    let dir = ctx.capture_dir();
    let requests = captures_after(dir.as_deref(), mark.as_deref(), "-ws-in-trouter-request");
    let registrar = captures_after(dir.as_deref(), mark.as_deref(), "-http-registrar");
    let request_span = file_span(&requests);
    // §9.5: what followed the renewal — the re-registration by default, the
    // deliveries on the old socket without reconnecting.
    let after_renewal = renewal_mark.unwrap_or(mark);
    let renewal_cap = file_span(&captures_after(
        dir.as_deref(),
        after_renewal.as_deref(),
        if reconnect {
            "-http-registrar"
        } else {
            "-ws-in-trouter-request"
        },
    ));
    for (item, v, note) in verdicts {
        let cap = match item {
            Item::S9_3 => join_captures(&[registrar.last().cloned(), requests.first().cloned()]),
            // Nothing to cite for §9.5 without a renewal.
            Item::S9_5 if !renewed => None,
            Item::S9_5 => renewal_cap.clone(),
            _ => request_span.clone(),
        };
        ctx.decide(item, v, cap, note);
    }
    Ok(())
}

// ----- main -----------------------------------------------------------------

fn init_logging() {
    let user = std::env::var("RUST_LOG").unwrap_or_else(|_| "warn".into());
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new(filter_directives(&user)))
        .with_writer(RedactingMakeWriter::new(std::io::stderr))
        .with_ansi(false)
        .try_init();
}

fn print_header(args: &Args, capture: &Path, session: &Path) {
    let o = client_options(args);
    println!("smoke: verifying spec §9 against your Teams account");
    println!("  session:  {}", session.display());
    println!(
        "  captures: {} (redacted, but real chat content: keep private)",
        capture.display()
    );
    println!(
        "  options:  register_all_apps={} reconnect_on_token_renewal={} profile_segment={} paging={}",
        o.register_all_apps,
        o.reconnect_on_token_renewal,
        o.profile_segment,
        o.paging.as_str()
    );
    println!(
        "  chat:     {}",
        args.conv.as_deref().unwrap_or("48:notes (no --conv)")
    );
    if args.yes {
        eprintln!(
            "WARNING: --yes answers every [y/n] question with y and does not wait for Enter. Nothing you would \
             check in Teams is verified; use it only for unattended compile checks."
        );
    }
}

fn print_summary(report: &Report, capture: &Path) {
    println!("\n=== Summary: paste everything from this line to the end back ===\n");
    for item in Item::ALL {
        println!("{}", report.line(item));
    }
    println!("\n{}", report.render());
    println!(
        "\nThe table holds no tokens. Captures are in {}: they are redacted but contain real chat content, \
         so share single files only if asked. A FAIL maps to a row of docs/unverified.md naming the code to \
         change.",
        capture.display()
    );
}

async fn run(args: &Args) -> Report {
    let capture = capture_dir(args);
    let session = session_path(args);
    print_header(args, &capture, &session);
    let tally = Tally::default();
    let client = match build_client(args, &capture, &session, &tally) {
        Ok(c) => c,
        Err(e) => {
            println!("cannot set up the client: {}", ic3_err_text(&e));
            let report = Report::new();
            print_summary(&report, &capture);
            return report;
        }
    };
    let mut ctx = Ctx::new(
        client.clone(),
        args,
        Box::new(Terminal {
            assume_yes: args.yes,
        }),
        tally,
    );
    match ensure_login(&client).await {
        Ok(()) => run_stages(&mut ctx, &args.stage).await,
        Err(e) => {
            let why = format!("sign-in failed: {}", ic3_err_text(&e));
            println!("{why}");
            ctx.unverified_undecided(&args.stage.items(), &why);
        }
    }
    print_summary(&ctx.report, &capture);
    client.shutdown().await;
    ctx.report
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = Args::parse();
    init_logging();
    if run(&args).await.any_fail() {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_table_renders_every_item_once() {
        let mut r = Report::new();
        r.set(
            Item::S9_1,
            Verdict::Pass,
            Some("000003-http-conversations.json".into()),
            "4 conversations, _metadata.backwardLink present",
        );
        r.set(
            Item::S9_6,
            Verdict::Fail,
            Some("000010-http-react.json".into()),
            "403 {\"errorCode\":403}",
        );
        let t = r.render();
        assert!(
            t.contains("| §9.1 |")
                && t.contains("PASS")
                && t.contains("000003-http-conversations.json")
        );
        assert!(t.contains("| §9.6 |") && t.contains("FAIL"));
        assert_eq!(
            t.matches("| §9.").count(),
            19,
            "all nineteen items appear, unverified ones as UNVERIFIED"
        );
        assert!(t.contains("| §9.7b | "));
        assert!(t.contains("| §9.5 | ") && t.contains("UNVERIFIED"));
    }

    #[test]
    fn excerpt_truncates_and_redacts() {
        assert_eq!(
            excerpt("a".repeat(300).as_str()),
            format!("{}…", "a".repeat(200))
        );
        assert!(!excerpt("Authorization: Bearer abc").contains("abc"));
    }

    // ----- events tally -----

    #[test]
    fn event_summary_counts_and_renewal_verdict() {
        let mut s = EventTally::default();
        s.record("NewMessage", &["NewMessage"]);
        s.record("MessageUpdate", &["MessageEdited"]);
        // The brief's sketch omitted these two, yet §9.4 PASS needs them.
        s.record("MessageUpdate", &["MessageDeleted"]);
        s.record("MessageUpdate", &["ReactionsChanged"]);
        s.record(
            "ConversationUpdate",
            &["ReadStateChanged", "ConversationUpdated"],
        );
        s.note_connected();
        // Renewed at 1 000; the old token (the socket's) expires at 1 500.
        s.note_renewal(1_000, 1_500);
        s.note_event_at(2_000);
        assert_eq!(s.verdict_9_3(), Verdict::Pass);
        assert_eq!(s.verdict_9_4().0, Verdict::Pass);
        assert_eq!(
            s.verdict_9_5(false /* reconnect disabled */),
            Verdict::Pass,
            "an event after the old token's expiry without reconnect"
        );
        assert_eq!(
            EventTally::default().verdict_9_5(false),
            Verdict::Unverified
        );
        assert_eq!(s.by_resource_type["MessageUpdate"], 3);
        assert_eq!(s.by_mapped["ConversationUpdated"], 1);
    }

    #[test]
    fn verdict_9_3_needs_connected_and_an_event() {
        let mut s = EventTally::default();
        s.note_disconnect("RegistrationFailed", 1);
        assert_eq!(s.verdict_9_3(), Verdict::Fail, "never connected");
        s.note_connected();
        assert_eq!(s.verdict_9_3(), Verdict::Unverified, "connected, no events");
        s.record("NewMessage", &["NewMessage"]);
        assert_eq!(s.verdict_9_3(), Verdict::Pass);
    }

    #[test]
    fn verdict_9_4_lists_the_missing_kinds() {
        assert_eq!(EventTally::default().verdict_9_4().0, Verdict::Unverified);
        let mut s = EventTally::default();
        s.record("NewMessage", &["NewMessage"]);
        s.record("MessageUpdate", &["NewMessage"]);
        let (v, missing) = s.verdict_9_4();
        assert_eq!(v, Verdict::Fail);
        assert_eq!(
            missing,
            [
                "MessageEdited",
                "MessageDeleted",
                "ReactionsChanged",
                "ReadStateChanged"
            ]
        );
    }

    /// Connected, one event at 500, renewed at 1 000; the old token expires
    /// at 1 500.
    fn renewed() -> EventTally {
        let mut s = EventTally::default();
        s.note_connected();
        s.note_event_at(500);
        s.note_renewal(1_000, 1_500);
        s
    }

    #[test]
    fn verdict_9_5_without_reconnect_waits_for_the_old_tokens_expiry() {
        // An event after the renewal but before the old token expired proves
        // nothing (the old token was still valid): no PASS.
        let mut s = renewed();
        s.note_event_at(1_200);
        s.note_end_at(3_000);
        let (v, note) = s.assess_9_5(false);
        assert_eq!(v, Verdict::Fail, "{note}");
        assert!(
            note.contains("no event arrived after the old token expired"),
            "{note}"
        );
        // After the old expiry, on the same socket: PASS.
        s.note_event_at(1_600);
        let (v, note) = s.assess_9_5(false);
        assert_eq!(v, Verdict::Pass, "{note}");
        // A reconnect between the renewal and that event: the old socket
        // was not tested.
        let mut s = renewed();
        s.note_disconnect("Silence", 1_300);
        s.note_connected();
        s.note_event_at(1_600);
        let (v, note) = s.assess_9_5(false);
        assert_eq!(v, Verdict::Unverified, "{note}");
        assert!(note.contains("reconnected (Silence"), "{note}");
        // A reconnect after the qualifying event does not undo it.
        let mut s = renewed();
        s.note_event_at(1_600);
        s.note_disconnect("Silence", 1_700);
        assert_eq!(s.assess_9_5(false).0, Verdict::Pass);
        // A reconnect after the renewal and no event since: not tested.
        let mut s = renewed();
        s.note_disconnect("SocketError", 1_100);
        s.note_end_at(3_000);
        assert_eq!(s.assess_9_5(false).0, Verdict::Unverified);
        // The run ended before the old token expired.
        let mut s = renewed();
        s.note_end_at(1_400);
        let (v, note) = s.assess_9_5(false);
        assert_eq!(v, Verdict::Unverified, "{note}");
        assert!(
            note.contains("before the old token expired") && note.contains("--minutes 75"),
            "{note}"
        );
        // No events at all: nothing to tell.
        let mut s = EventTally::default();
        s.note_renewal(1_000, 1_500);
        s.note_end_at(3_000);
        assert_eq!(s.assess_9_5(false).0, Verdict::Unverified);
        // No renewal.
        let (v, note) = EventTally::default().assess_9_5(false);
        assert_eq!(v, Verdict::Unverified);
        assert!(note.contains("--minutes 75"), "{note}");
    }

    #[test]
    fn verdict_9_5_by_default_needs_the_renewal_reconnect() {
        let mut s = renewed();
        assert_eq!(
            s.verdict_9_5(true),
            Verdict::Unverified,
            "no reconnect seen"
        );
        s.note_disconnect("TokenRenewal", 1_001);
        s.note_connected();
        let (v, note) = s.assess_9_5(true);
        assert_eq!(v, Verdict::Pass, "{note}");
        // A reconnect for another reason does not count.
        let mut s = renewed();
        s.note_disconnect("Silence", 1_100);
        s.note_connected();
        assert_eq!(s.verdict_9_5(true), Verdict::Unverified);
    }

    #[test]
    fn expiry_plan_warns_when_the_run_is_too_short() {
        let min = 60_000;
        // Expires in 64 min: renewal around 51 min.
        let plan = expiry_plan(0, 64 * min, 75 * min, false);
        assert_eq!(plan.len(), 1, "{plan:?}");
        assert!(
            plan[0].contains("(in 64 min)") && plan[0].contains("(in 52 min)"),
            "{plan:?}"
        );
        let plan = expiry_plan(0, 64 * min, 3 * min, true);
        assert!(
            plan[1].starts_with("WARNING: the renewal falls after"),
            "{plan:?}"
        );
        // Renewal inside the run, old expiry after it: only matters
        // without reconnecting.
        assert_eq!(expiry_plan(0, 64 * min, 60 * min, true).len(), 1);
        let plan = expiry_plan(0, 64 * min, 60 * min, false);
        assert!(plan[1].contains("old token expires"), "{plan:?}");
        assert!(at_in(5, 10).ends_with("(now)"));
    }

    #[test]
    fn renewal_prompt_names_the_old_expiry() {
        let min = 60_000;
        let p = renewal_prompt(51 * min, 64 * min, 115 * min, 75 * min, false, "the chat X");
        assert!(
            p.contains("expires at")
                && p.contains("(in 13 min)")
                && p.contains("send a message in the chat X after"),
            "{p}"
        );
        assert!(!p.contains("WARNING"), "{p}");
        let p = renewal_prompt(51 * min, 64 * min, 115 * min, 60 * min, false, "the chat X");
        assert!(p.contains("WARNING"), "{p}");
        let p = renewal_prompt(51 * min, 64 * min, 115 * min, 60 * min, true, "the chat X");
        assert!(p.contains("once `Connected` is printed again"), "{p}");
    }

    #[test]
    fn a_delete_that_only_blanks_content_is_flagged() {
        let mut s = EventTally::default();
        let blank =
            json!({"id": "17", "messagetype": "RichText/Html", "content": "", "properties": {}});
        assert!(
            s.observe("MessageUpdate", &blank, &["NewMessage"])
                .is_some()
        );
        let edited = json!({"id": "21", "messagetype": "RichText/Html", "content": " ",
                            "properties": {"edittime": "1"}});
        assert!(
            s.observe("MessageUpdate", &edited, &["MessageEdited"])
                .is_some()
        );
        let marked = json!({"id": "18", "messagetype": "RichText/Html", "content": "",
                            "properties": {"deletetime": "1760346120000"}});
        assert!(
            s.observe("MessageUpdate", &marked, &["MessageDeleted"])
                .is_none()
        );
        let text = json!({"id": "19", "messagetype": "Text", "content": "hi"});
        assert!(s.observe("NewMessage", &text, &["NewMessage"]).is_none());
        let control = json!({"id": "20", "messagetype": "Control/Typing", "content": ""});
        assert!(s.observe("NewMessage", &control, &[]).is_none());
        // A reaction update with blank content is no delete.
        let reaction = json!({"id": "22", "messagetype": "RichText/Html", "content": "",
                              "properties": {"emotions": []}});
        assert!(
            s.observe("MessageUpdate", &reaction, &["ReactionsChanged"])
                .is_none()
        );
        assert_eq!(s.blank_without_deletetime, ["17", "21"]);
        s.record("NewMessage", &["NewMessage"]);
        s.record("MessageUpdate", &["MessageEdited"]);
        s.record("MessageUpdate", &["MessageDeleted"]);
        s.record("MessageUpdate", &["ReactionsChanged"]);
        s.record("ConversationUpdate", &["ReadStateChanged"]);
        let (v, missing) = s.verdict_9_4();
        assert_eq!(v, Verdict::Fail, "every kind mapped, but a blanked delete");
        assert!(missing.is_empty());
        assert!(
            s.notes_9_4().contains("api/wire.rs::parse_message"),
            "{}",
            s.notes_9_4()
        );
    }

    #[test]
    fn thread_update_kind_keys_are_compared() {
        let mut s = EventTally::default();
        // Group rename: kind keys under properties (what ic3-teams reads).
        let group = json!({"id": "19:g@thread.v2", "properties": {"topic": "T", "productThreadType": "Chat"}});
        let line = s
            .observe("ThreadUpdate", &group, &["ConversationUpdated"])
            .unwrap();
        assert!(line.contains("productThreadType=\"Chat\""), "{line}");
        assert!(s.thread_kind_mismatches.is_empty());
        // A 1:1 whose kind keys sit under threadProperties only: misclassified.
        let one = json!({"id": "19:x@thread.v2", "properties": {"topic": "T"},
                         "threadProperties": {"uniquerosterthread": "true"}});
        s.observe("ThreadUpdate", &one, &["ConversationUpdated"])
            .unwrap();
        assert_eq!(s.thread_kind_mismatches.len(), 1);
        assert!(s.thread_kind_mismatches[0].contains("19:x@thread.v2"));
        for k in REQUIRED_EVENTS {
            s.record("X", &[k]);
        }
        assert_eq!(s.verdict_9_4().0, Verdict::Fail);
        assert!(s.notes_9_4().contains("ThreadUpdate"), "{}", s.notes_9_4());
    }

    #[test]
    fn events_are_described_in_one_line() {
        let m = msg(7, "c", "RichText/Html");
        let l = describe_event(&Event::NewMessage(m.clone()));
        assert!(l.starts_with("NewMessage from Bea: text 7"), "{l}");
        assert!(describe_event(&Event::MessageEdited(m)).starts_with("MessageEdited 7 "));
        let l = describe_event(&Event::Disconnected {
            reason: ic3_teams::DisconnectReason::TokenRenewal,
            retry_in: Duration::from_secs(0),
        });
        assert_eq!(l, "Disconnected { reason: TokenRenewal, retry in 0s }");
        assert_eq!(describe_event(&Event::Connected), "Connected");
        let l = describe_event(&Event::Typing {
            conversation: ConversationId("19:g@thread.v2".into()),
            from: ic3_teams::Mri("8:orgid:b".into()),
            from_name: None,
        });
        assert_eq!(
            l,
            "Typing in 19:g@thread.v2 by 8:orgid:b (from_name absent)"
        );
    }

    #[test]
    fn s9_10_is_informational() {
        let mut r = Report::new();
        r.set(Item::S9_10, Verdict::Fail, None, "no members");
        assert_eq!(r.row(Item::S9_10).0, Verdict::Info);
        assert!(r.line(Item::S9_10).starts_with("INFO        §9.10"));
        assert!(!r.any_fail(), "§9.10 never affects the exit code");
    }

    #[test]
    fn stage_order_for_all() {
        assert_eq!(
            Stage::All { minutes: 5 }.expand(),
            vec![
                Stage::Rest,
                Stage::Csa,
                Stage::Write {
                    typing: false,
                    only: vec![]
                },
                Stage::Events { minutes: 5 }
            ]
        );
    }

    // ----- command line -----

    use clap::CommandFactory;

    fn parse(argv: &[&str]) -> Args {
        Args::try_parse_from(argv).unwrap()
    }

    #[test]
    fn cli_definition_is_valid() {
        Args::command().debug_assert();
    }

    #[test]
    fn cli_defaults_keep_the_library_defaults() {
        let a = parse(&["smoke", "write"]);
        assert_eq!(
            a.stage,
            Stage::Write {
                typing: false,
                only: vec![]
            }
        );
        assert_eq!(a.tenant, "organizations");
        assert_eq!(a.conv, None);
        assert!(!a.yes);
        assert_eq!(client_options(&a), ClientOptions::default());
        assert!(
            session_path(&a).ends_with("ic3-irc/session.json"),
            "{}",
            session_path(&a).display()
        );
        let cap = capture_dir(&a);
        assert!(cap.starts_with("captures"));
        assert!(
            cap.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("smoke-")
        );
    }

    #[test]
    fn cli_flags_map_to_client_options() {
        let a = parse(&[
            "smoke",
            "--register-all-apps",
            "--no-reconnect-on-renewal",
            "--profile-segment",
            "emea",
            "--paging",
            "backward_link",
            "--csa-audience",
            "spaces",
            "--csa-geo",
            "amer",
            "--no-qtd-msgs",
            "--no-clear-bookmark",
            "--session",
            "/tmp/s.json",
            "--capture",
            "/tmp/cap",
            "--tenant",
            "acme.io",
            "rest",
        ]);
        assert_eq!(
            client_options(&a),
            ClientOptions {
                register_all_apps: true,
                reconnect_on_token_renewal: false,
                profile_segment: "emea".into(),
                paging: PagingMode::BackwardLink,
                csa_audience: CsaAudience::Spaces,
                csa_geo: Some("amer".into()),
                send_quoted_ids: false,
                clear_unread_bookmark: false,
                media: ClientOptions::default().media,
            }
        );
        assert_eq!(session_path(&a), PathBuf::from("/tmp/s.json"));
        assert_eq!(capture_dir(&a), PathBuf::from("/tmp/cap"));
        assert_eq!(a.tenant, "acme.io");
        assert_eq!(
            parse(&["smoke", "--paging", "window", "rest"]).paging,
            PagingMode::Window
        );
    }

    #[test]
    fn cli_accepts_options_after_the_stage_and_minutes() {
        let a = parse(&[
            "smoke",
            "events",
            "--minutes",
            "70",
            "--conv",
            "19:x@thread.v2",
            "--yes",
        ]);
        assert_eq!(a.stage, Stage::Events { minutes: 70 });
        assert_eq!(a.conv.as_deref(), Some("19:x@thread.v2"));
        assert!(a.yes);
        assert_eq!(
            parse(&["smoke", "all"]).stage,
            Stage::All {
                minutes: DEFAULT_EVENT_MINUTES
            }
        );
        assert_eq!(
            parse(&["smoke", "events"]).stage,
            Stage::Events { minutes: 3 }
        );
        assert_eq!(parse(&["smoke", "csa"]).stage, Stage::Csa);
        assert_eq!(
            parse(&["smoke", "--conv", "19:x@thread.v2", "write", "--typing"]).stage,
            Stage::Write {
                typing: true,
                only: vec![]
            }
        );
    }

    #[test]
    fn cli_rejects_bad_input() {
        assert!(Args::try_parse_from(["smoke", "--paging", "sideways", "rest"]).is_err());
        assert!(Args::try_parse_from(["smoke", "--csa-audience", "graph", "rest"]).is_err());
        assert!(
            Args::try_parse_from(["smoke"]).is_err(),
            "a stage is required"
        );
        assert!(Args::try_parse_from(["smoke", "bogus"]).is_err());
        assert!(Args::try_parse_from(["smoke", "rest", "--minutes", "3"]).is_err());
        assert!(Args::try_parse_from(["smoke", "rest", "--typing"]).is_err());
    }

    /// `write --only`'s items, parsed.
    fn only(argv: &[&str]) -> Vec<Item> {
        match parse(argv).stage {
            Stage::Write { only, .. } => only,
            other => panic!("not write: {other:?}"),
        }
    }

    #[test]
    fn write_only_takes_labels_with_or_without_the_section_sign() {
        assert_eq!(
            only(&["smoke", "write", "--only", "9.16,§9.7b, 9.7"]),
            vec![Item::S9_16, Item::S9_7b, Item::S9_7]
        );
        assert_eq!(
            only(&["smoke", "write", "--only", "§9.6", "--only", "9.18"]),
            vec![Item::S9_6, Item::S9_18]
        );
        for item in WRITE_ITEMS {
            let bare = item.label().trim_start_matches('§');
            assert_eq!(only(&["smoke", "write", "--only", bare]), vec![item]);
            assert_eq!(
                only(&["smoke", "write", "--only", item.label()]),
                vec![item]
            );
        }
        assert!(only(&["smoke", "write"]).is_empty());
    }

    #[test]
    fn write_only_rejects_other_labels_and_lists_the_valid_ones() {
        for bad in ["9.1", "9.7c", "§9.12", "bogus", ""] {
            let e = Args::try_parse_from(["smoke", "write", "--only", bad])
                .unwrap_err()
                .to_string();
            assert!(
                e.contains("expected one of §9.6, §9.7, §9.7b, §9.8, §9.15, §9.16, §9.17, §9.18"),
                "{bad}: {e}"
            );
        }
        assert!(
            Args::try_parse_from(["smoke", "--only", "9.7", "write"]).is_err(),
            "a write flag: after the stage name"
        );
        assert!(Args::try_parse_from(["smoke", "rest", "--only", "9.7"]).is_err());
    }

    /// The plan `smoke … write <flags>` runs.
    fn plan_of(argv: &[&str]) -> WritePlan {
        match parse(argv).stage {
            Stage::Write { typing, only } => WritePlan::new(typing, &only),
            other => panic!("not write: {other:?}"),
        }
    }

    #[test]
    fn write_only_runs_the_steps_its_items_need() {
        use WriteStep::*;
        let steps = |only: &str| plan_of(&["smoke", "write", "--only", only]).steps;
        assert_eq!(steps("9.7"), vec![MarkRead], "no message is sent");
        assert_eq!(steps("9.7,9.7b"), vec![MarkRead]);
        assert_eq!(steps("9.16"), vec![Send, Edit, Delete], "cleanup delete");
        assert_eq!(steps("9.6"), vec![Send, React, Delete]);
        assert_eq!(steps("9.15"), vec![Send, Reply, Delete]);
        assert_eq!(steps("9.8"), vec![Send, Reply, Delete]);
        assert_eq!(steps("9.17"), vec![Send, Delete]);
        assert_eq!(steps("9.18"), vec![Typing], "--only 9.18 implies --typing");
        assert_eq!(
            steps("9.18,9.7,9.6,9.16"),
            vec![Send, Edit, React, MarkRead, Delete, Typing],
            "in stage order"
        );
        assert_eq!(
            plan_of(&["smoke", "write", "--only", "9.7", "--typing"]).steps,
            vec![MarkRead, Typing]
        );
        let full = vec![Send, Edit, Reply, React, MarkRead, Delete];
        assert_eq!(
            plan_of(&["smoke", "write"]),
            WritePlan {
                steps: full.clone(),
                only: None
            },
            "no --only: today's sequence"
        );
        let mut typed = full;
        typed.push(Typing);
        assert_eq!(plan_of(&["smoke", "write", "--typing"]).steps, typed);
    }

    #[test]
    fn the_consent_prompt_lists_only_the_steps_that_run() {
        let sample = FORMAT_SAMPLE.replace('\n', "\\n");
        assert_eq!(
            plan_of(&["smoke", "write", "--only", "9.16"]).consent_list(),
            format!(
                "  1. the message \"{sample}\"\n  2. an edit of that message, adding \"{EDIT_NOTE}\"\n  \
                 3. the deletion of message 1 (last)"
            )
        );
        assert_eq!(
            plan_of(&["smoke", "write", "--only", "9.7b"]).consent_list(),
            "  1. a read marker (consumption horizon) at the newest message"
        );
        assert_eq!(
            plan_of(&["smoke", "write", "--typing"]).consent_list(),
            format!(
                "  1. the message \"{sample}\"\n  \
                 2. an edit of that message, adding \"{EDIT_NOTE}\"\n  \
                 3. the reply \"{REPLY_TEXT}\" (quoting the newest message)\n  \
                 4. a 👍 on message 1, removed again\n  \
                 5. a read marker (consumption horizon) at the newest message\n  \
                 6. the deletion of message 1 (last)\n  \
                 7. one typing signal (\"… is typing\"), while someone else watches"
            ),
            "no --only: today's list"
        );
    }

    #[test]
    fn stages_cover_every_item_once() {
        let mut all: Vec<Item> = Stage::All { minutes: 1 }
            .expand()
            .iter()
            .flat_map(Stage::items)
            .collect();
        all.sort_by_key(|i| Item::ALL.iter().position(|x| x == i));
        assert_eq!(all, Item::ALL.to_vec());
    }

    // ----- report and helpers -----

    #[test]
    fn line_shows_verdict_item_capture_and_note() {
        let mut r = Report::new();
        r.set(
            Item::S9_6,
            Verdict::Fail,
            Some("000002-http-react.json".into()),
            "http 403",
        );
        let l = r.line(Item::S9_6);
        assert!(
            l.starts_with("FAIL        §9.6   reaction add/remove request shape"),
            "{l}"
        );
        assert!(l.ends_with("[000002-http-react.json]  http 403"), "{l}");
        assert!(r.line(Item::S9_3).starts_with("UNVERIFIED  §9.3 "));
        assert!(r.line(Item::S9_3).contains("[no capture]  not run"));
        assert!(r.any_fail());
        assert!(!Report::new().any_fail());
    }

    #[test]
    fn render_escapes_cells_and_appends_info() {
        let mut r = Report::new();
        r.set(Item::S9_2, Verdict::Pass, None, "a | b\nc");
        r.info
            .push("formatting  [000001-http-send.json]  looks right".into());
        let t = r.render();
        assert!(
            t.contains("| §9.2 | PASS | message paging (older pages) | - | a \\| b c |"),
            "{t}"
        );
        assert!(
            t.ends_with("INFO formatting  [000001-http-send.json]  looks right"),
            "{t}"
        );
    }

    #[test]
    fn excerpt_redacts_json_bodies_and_keeps_short_text() {
        assert_eq!(excerpt("short"), "short");
        let e = excerpt(r#"{"skypeToken":"sekrit","errorCode":403}"#);
        assert!(!e.contains("sekrit") && e.contains("403"), "{e}");
        assert_eq!(truncate("héllo", 2), "hé…");
        let http = ic3_teams::Error::Http {
            status: 403,
            body: r#"{"refresh_token":"rt-x"}"#.into(),
        };
        let t = ic3_err_text(&http);
        assert!(t.starts_with("http 403: ") && !t.contains("rt-x"), "{t}");
        assert_eq!(err_text(&anyhow::Error::from(http)), t);
    }

    #[test]
    fn writes_only_to_notes_or_the_chosen_chat() {
        let notes = ConversationId(NOTES.into());
        let chosen = ConversationId("19:chosen@thread.v2".into());
        let other = ConversationId("19:other@thread.v2".into());
        assert!(write_allowed(&notes, None));
        assert!(write_allowed(&notes, Some(&chosen)));
        assert!(write_allowed(&chosen, Some(&chosen)));
        assert!(!write_allowed(&other, Some(&chosen)));
        assert!(!write_allowed(&other, None));
    }

    // ----- verdicts -----

    fn page(items: usize, meta: bool, total: Option<i64>, link: bool) -> PageSummary {
        PageSummary {
            items,
            meta_present: meta,
            total_count: total,
            backward_link: link,
            sync_state: meta,
            link_shape: None,
            note: None,
        }
    }

    #[test]
    fn conversations_verdicts() {
        let v = |pages: &[PageSummary], unique, capped| {
            conversations_verdict(pages, unique, 100, capped)
        };
        let (p, note) = v(&[page(4, true, Some(4), true)], 4, false);
        assert_eq!(p, Verdict::Pass);
        assert!(
            note.contains("backwardLink yes") && note.contains("totalCount 4"),
            "{note}"
        );
        assert_eq!(
            v(&[page(37, false, None, false)], 37, false).0,
            Verdict::Pass
        );
        let three = [
            page(100, true, Some(250), true),
            page(100, true, Some(250), true),
            page(50, true, Some(250), false),
        ];
        assert_eq!(v(&three, 250, false).0, Verdict::Pass);
        assert_eq!(
            v(&three[..1], 100, false).0,
            Verdict::Fail,
            "totalCount > retrieved"
        );
        assert_eq!(v(&three, 250, true).0, Verdict::Fail, "page cap");
        let mut noted = page(100, true, None, true);
        noted.note = Some("backwardLink repeated the previous link; stopped".into());
        let (f, note) = v(&[noted], 100, false);
        assert_eq!(f, Verdict::Fail);
        assert!(note.contains("repeated"), "{note}");
        assert_eq!(
            v(&[page(100, false, None, false)], 100, false).0,
            Verdict::Unverified
        );
        assert_eq!(
            v(&[page(100, true, None, false)], 100, false).0,
            Verdict::Unverified
        );
        assert_eq!(
            v(
                &[page(100, true, None, true), page(20, true, None, false)],
                120,
                false
            )
            .0,
            Verdict::Pass
        );
        assert_eq!(v(&[], 0, false).0, Verdict::Unverified);
    }

    /// Live 2026-10-14: 96 of totalCount 96 on page 1, with a backwardLink
    /// (== syncState) naming the user by MRI. PASS, saying what the link was
    /// and that paging past 100 chats was not exercised.
    #[test]
    fn conversations_verdict_for_a_complete_first_page_with_a_link() {
        let mut p = page(96, true, Some(96), true);
        p.link_shape = Some("/v1/users/<mri>/conversations, == syncState".into());
        let (v, note) = conversations_verdict(&[p], 96, 100, false);
        assert_eq!(v, Verdict::Pass, "{note}");
        assert!(
            note.contains("page 1 held all 96")
                && note.contains("not followed")
                && note.contains("/v1/users/<mri>/conversations, == syncState")
                && note.contains("still unverified for accounts with more than 100 chats"),
            "{note}"
        );
        assert_eq!(
            link_shape(&PageMeta {
                backward_link: Some(
                    "https://h/v1/users/8:orgid:00000000-0000-4000-8000-000000000001/conversations?view=x&syncState=abc".into()
                ),
                sync_state: Some(
                    "https://h/v1/users/8:orgid:00000000-0000-4000-8000-000000000001/conversations?view=x&syncState=abc".into()
                ),
                total_count: Some(96),
                raw: None,
            }),
            Some("/v1/users/<mri>/conversations, == syncState".into())
        );
        assert_eq!(
            link_shape(&PageMeta {
                backward_link: Some("https://h/v1/users/ME/conversations?x=1".into()),
                ..PageMeta::default()
            }),
            Some("/v1/users/ME/conversations".into())
        );
        assert_eq!(link_shape(&PageMeta::default()), None);
    }

    #[test]
    fn message_paging_verdicts() {
        let p1 = [50, 40, 30, 20, 10];
        assert_eq!(message_paging_verdict(&p1, &[9, 8], None).0, Verdict::Pass);
        assert_eq!(message_paging_verdict(&p1, &[10, 9], None).0, Verdict::Fail);
        assert_eq!(message_paging_verdict(&p1, &[], None).0, Verdict::Fail);
        assert_eq!(
            message_paging_verdict(&p1, &[9], Some("time window did not move")).0,
            Verdict::Fail
        );
    }

    #[test]
    fn end_time_verdicts_and_hints() {
        assert_eq!(end_time_verdict(100, &[100, 90]).0, Verdict::Pass);
        let (f, note) = end_time_verdict(100, &[101, 90]);
        assert_eq!(f, Verdict::Fail);
        assert!(note.contains("101"), "{note}");
        assert_eq!(end_time_verdict(100, &[]).0, Verdict::Unverified);
        let hints = boundary_hints(100, &[(150, 140), (101, 99)], &[(100, 99), (90, 120)]);
        assert_eq!(hints.len(), 2, "{hints:?}");
        assert!(hints[0].contains("page 1 message 101") && hints[1].contains("page 2 message 90"));
    }

    #[test]
    fn capture_parsing() {
        let doc = serde_json::json!({
            "request": {"url": "https://h/v1/users/ME/conversations/48:notes/messages?pageSize=5&view=msnp24Equivalent&startTime=1&endTime=1759999999999"},
            "response": {"body": {"messages": [
                {"id": "1760000000000", "composetime": "2025-10-09T07:33:19.900Z"},
                {"id": "x", "composetime": "2025-10-09T07:33:19.900Z"},
                {"id": "1760000000001"}
            ]}}
        });
        assert_eq!(end_time_of(&doc), Some(1_759_999_999_999));
        assert_eq!(
            captured_composetimes(&doc),
            vec![(1_760_000_000_000, 1_759_995_199_900)]
        );
        assert_eq!(
            end_time_of(&serde_json::json!({"request": {"url": "https://h/x?pageSize=5"}})),
            None
        );
    }

    fn msg(id: i64, cmid: &str, kind: &str) -> Message {
        Message {
            id: ic3_teams::MessageId(id.to_string()),
            client_message_id: Some(cmid.into()),
            conversation: ConversationId(NOTES.into()),
            thread_root: None,
            from: ic3_teams::Mri("8:orgid:b".into()),
            from_name: Some("Bea".into()),
            arrived_at: id,
            version: id,
            edited: false,
            deleted: false,
            body: ic3_teams::RichText::text(&format!("text {id}")),
            reply_to: None,
            reactions: Vec::new(),
            attachments: Vec::new(),
            message_type: kind.into(),
            extra: serde_json::Map::new(),
        }
    }

    #[test]
    fn reply_target_prefers_the_newest_other_message() {
        let items = [
            msg(30, "own", "RichText/Html"),
            msg(25, "c", "ThreadActivity/AddMember"),
            msg(20, "b", "Text"),
            msg(10, "a", "Text"),
        ];
        let t = pick_reply_target(&items, Some("own")).unwrap();
        assert_eq!(t.message.id.0, "20");
        assert_eq!(t.author_name, "Bea");
        assert_eq!(t.excerpt, "text 20");
        assert_eq!(t.sent_at, 20);
        let t = pick_reply_target(&items[..2], Some("own")).unwrap();
        assert_eq!(t.message.id.0, "30", "falls back to our own message");
        assert_eq!(pick_reply_target(&items, None).unwrap().message.id.0, "30");
        assert!(pick_reply_target(&items[1..2], None).is_none());
    }

    #[test]
    fn the_edit_note_ends_the_last_paragraph() {
        let edited = with_edit_note(&parse_slack(FORMAT_SAMPLE));
        assert!(
            edited.plain_text().contains(&format!("quote {EDIT_NOTE}")),
            "inside the trailing quote: {:?}",
            edited.plain_text()
        );
        assert_eq!(
            edited.0.len(),
            parse_slack(FORMAT_SAMPLE).0.len(),
            "no new block"
        );
        assert_eq!(
            with_edit_note(&ic3_teams::RichText::text("hello")).plain_text(),
            format!("hello {EDIT_NOTE}")
        );
        let code = ic3_teams::RichText(vec![Block::CodeBlock("x".into())]);
        assert_eq!(with_edit_note(&code).0.len(), 2, "no paragraph: a new one");
    }

    #[test]
    fn edit_and_delete_verdicts() {
        let mut m = msg(7, "c", "RichText/Html");
        assert_eq!(edit_verdict(None).0, Verdict::Unverified);
        assert_eq!(
            edit_verdict(Some(&m)).0,
            Verdict::Fail,
            "still the old text"
        );
        m.body = ic3_teams::RichText::text(&format!("text 7 {EDIT_NOTE}"));
        let (v, note) = edit_verdict(Some(&m));
        assert_eq!(v, Verdict::Fail, "{note}");
        assert!(note.contains("no properties.edittime"), "{note}");
        m.extra.insert(
            "properties".into(),
            json!({"edittime": 1_760_000_000_123_i64}),
        );
        let (v, note) = edit_verdict(Some(&m));
        assert_eq!(v, Verdict::Pass, "{note}");
        assert!(note.contains("1760000000123"), "{note}");

        assert_eq!(delete_verdict(None).0, Verdict::Unverified);
        assert_eq!(delete_verdict(Some(&m)).0, Verdict::Fail);
        m.deleted = true;
        m.extra
            .insert("properties".into(), json!({"deletetime": "1760000000456"}));
        let (v, note) = delete_verdict(Some(&m));
        assert_eq!(v, Verdict::Pass, "{note}");
        assert!(note.contains("1760000000456"), "{note}");
    }

    // ----- stages against a mock server -----

    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    use ic3_teams::trouter::{Transport, TransportFactory};
    use ic3_teams::{Endpoints, MemoryTokenStore, Secret, StoredSession, TokenStore};
    use serde_json::json;
    use wiremock::matchers::{
        body_json, body_string_contains, method, path, path_regex, query_param,
    };
    use wiremock::{Match, Mock, MockServer, Request, Respond, ResponseTemplate};

    const ME_GUID: &str = "aaaaaaaa-1111-1111-1111-111111111111";
    const BEA_GUID: &str = "bbbbbbbb-2222-2222-2222-222222222222";
    const CY_GUID: &str = "cccccccc-3333-3333-3333-333333333333";
    const DAN_GUID: &str = "dddddddd-4444-4444-4444-444444444444";
    const BASE_ID: i64 = 1_760_000_000_000;

    fn contact(guid: &str) -> String {
        format!("https://de.ng.msg.teams.microsoft.com/v1/users/ME/contacts/8:orgid:{guid}")
    }

    fn wire_message(id: i64, cmid: &str, from: &str, name: &str, content: &str) -> Value {
        json!({
            "id": id.to_string(),
            "clientmessageid": cmid,
            "from": contact(from),
            "imdisplayname": name,
            "messagetype": "RichText/Html",
            "contenttype": "text",
            "content": content,
            "composetime": iso(id - 100),
            "originalarrivaltime": iso(id),
            "version": id.to_string(),
        })
    }

    fn id_of(m: &Value) -> i64 {
        m["id"].as_str().unwrap().parse().unwrap()
    }

    /// A chat's messages endpoint: GET pages by `pageSize` / `endTime`
    /// (arrival id), POST appends.
    #[derive(Clone, Default)]
    struct FakeChat {
        msgs: Arc<Mutex<Vec<Value>>>,
        posted: Arc<Mutex<Vec<Value>>>,
        /// Bodies of the edits (PUTs) it received.
        edits: Arc<Mutex<Vec<Value>>>,
        /// Answer full pages with a `backwardLink` (to the next window).
        link: bool,
    }

    impl FakeChat {
        fn with(n: i64) -> FakeChat {
            let chat = FakeChat::default();
            for i in 0..n {
                chat.push(wire_message(
                    BASE_ID + i * 60_000,
                    &format!("seed{i}"),
                    BEA_GUID,
                    "Bea Example",
                    &format!("<p>msg {i}</p>"),
                ));
            }
            chat
        }

        fn push(&self, m: Value) {
            self.msgs.lock().unwrap().push(m);
        }

        fn newest_id(&self) -> i64 {
            self.msgs
                .lock()
                .unwrap()
                .iter()
                .map(id_of)
                .max()
                .unwrap_or(BASE_ID)
        }
    }

    impl Respond for FakeChat {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            // `…/messages/{id}`: an edit (PUT) or a soft delete (DELETE).
            if let Some(id) = req
                .url
                .path()
                .rsplit('/')
                .next()
                .and_then(|s| s.parse::<i64>().ok())
            {
                let mut msgs = self.msgs.lock().unwrap();
                let Some(m) = msgs.iter_mut().find(|m| id_of(m) == id) else {
                    return ResponseTemplate::new(404);
                };
                match req.method.as_str() {
                    "PUT" => {
                        let body: Value = serde_json::from_slice(&req.body).unwrap();
                        m["content"] = body["content"].clone();
                        m["properties"] = body["properties"].clone();
                        m["version"] = body["properties"]["edittime"].to_string().into();
                        self.edits.lock().unwrap().push(body);
                    }
                    "DELETE" => {
                        let at = id + 600_000;
                        m["content"] = "".into();
                        m["properties"]["deletetime"] = at.to_string().into();
                        m["version"] = at.to_string().into();
                    }
                    _ => return ResponseTemplate::new(405),
                }
                return ResponseTemplate::new(200);
            }
            if req.method.as_str() == "POST" {
                let body: Value = serde_json::from_slice(&req.body).unwrap();
                let id = self.newest_id() + 1_000;
                self.push(wire_message(
                    id,
                    body["clientmessageid"].as_str().unwrap(),
                    ME_GUID,
                    "Ada Example",
                    body["content"].as_str().unwrap(),
                ));
                self.posted.lock().unwrap().push(body);
                return ResponseTemplate::new(201)
                    .set_body_json(json!({"OriginalArrivalTime": iso(id)}));
            }
            let q: HashMap<String, String> = req.url.query_pairs().into_owned().collect();
            let size: usize = q.get("pageSize").map_or(50, |s| s.parse().unwrap());
            let end: i64 = q.get("endTime").map_or(i64::MAX, |s| s.parse().unwrap());
            let mut page: Vec<Value> = self
                .msgs
                .lock()
                .unwrap()
                .iter()
                .filter(|m| id_of(m) <= end)
                .cloned()
                .collect();
            page.sort_by_key(|m| std::cmp::Reverse(id_of(m)));
            page.truncate(size);
            let mut meta = json!({"lastCompleteSegmentStartTime": 1});
            if self.link && page.len() == size {
                let oldest = page.iter().map(id_of).min().unwrap_or(0);
                // Relative: resolved against the chat service.
                meta["backwardLink"] = format!(
                    "{}?pageSize={size}&view=msnp24Equivalent&syncState=s{oldest}&endTime={}",
                    req.url.path(),
                    oldest - 1
                )
                .into();
            }
            ResponseTemplate::new(200).set_body_json(json!({"messages": page, "_metadata": meta}))
        }
    }

    /// Any write (non-GET) outside `48:notes`, the auth endpoints and
    /// trouter's setup.
    struct WriteElsewhere;

    impl Match for WriteElsewhere {
        fn matches(&self, r: &Request) -> bool {
            let p = r.url.path();
            r.method.as_str() != "GET"
                && !p.contains("48:notes")
                && !p.contains("/oauth2/")
                && !p.contains("/authsvc/")
                && !p.contains("/fetchShortProfile")
                && p != "/v4/a"
                && !p.starts_with("/registrar/")
        }
    }

    async fn mount_basics(s: &MockServer) {
        Mock::given(method("POST"))
            .and(path("/organizations/oauth2/v2.0/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": "at-1", "refresh_token": "rt-1", "expires_in": 3600
            })))
            .mount(s)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/authsvc/v1.0/authz"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "tokens": {"skypeToken": "sk-1", "expiresIn": 3600},
                "region": "de",
                "regionGtms": {"chatService": s.uri()},
            })))
            .mount(s)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/users/ME/properties"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "skypeName": format!("orgid:{ME_GUID}"),
                "userDetails": "{\"name\":\"Ada Example\",\"upn\":\"ada@acme.io\"}",
            })))
            .mount(s)
            .await;
        Mock::given(WriteElsewhere)
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .named("writes outside 48:notes")
            .mount(s)
            .await;
    }

    async fn mount_notes(s: &MockServer, chat: &FakeChat) {
        Mock::given(path("/v1/users/ME/conversations/48:notes/messages"))
            .respond_with(chat.clone())
            .mount(s)
            .await;
        Mock::given(path_regex(
            r"^/v1/users/ME/conversations/48:notes/messages/\d+$",
        ))
        .respond_with(chat.clone())
        .mount(s)
        .await;
    }

    /// Answers and Enter actions given in order; an unexpected question panics.
    struct Scripted {
        answers: VecDeque<bool>,
        on_enter: VecDeque<Box<dyn FnOnce()>>,
        asked: Arc<Mutex<Vec<String>>>,
        waited: Arc<Mutex<Vec<String>>>,
    }

    impl Ui for Scripted {
        fn enter(&mut self, msg: &str) {
            self.waited.lock().unwrap().push(msg.to_owned());
            if let Some(f) = self.on_enter.pop_front() {
                f();
            }
        }

        fn yes_no(&mut self, msg: &str) -> bool {
            self.asked.lock().unwrap().push(msg.to_owned());
            self.answers
                .pop_front()
                .unwrap_or_else(|| panic!("unexpected question: {msg}"))
        }
    }

    async fn ctx_for(s: &MockServer, capture: &Path, argv: &[&str], ui: Scripted) -> Ctx {
        ctx_with(s, capture, argv, ui, None).await
    }

    /// [`ctx_for`] with trouter's websocket replaced by `factory`.
    async fn ctx_with(
        s: &MockServer,
        capture: &Path,
        argv: &[&str],
        ui: Scripted,
        factory: Option<Arc<dyn TransportFactory>>,
    ) -> Ctx {
        let args = parse(argv);
        let store = MemoryTokenStore::default();
        store
            .save(&StoredSession {
                refresh_token: Secret::new("rt-0".into()),
                tenant: "organizations".into(),
                endpoint_id: "ep".into(),
            })
            .await
            .unwrap();
        let tally = Tally::default();
        let mut builder = client_builder(&args, capture, &tally)
            .token_store(store)
            .endpoints(Endpoints::all(&s.uri()));
        if let Some(f) = factory {
            builder = builder.transport_factory(f);
        }
        Ctx::new(builder.build().unwrap(), &args, Box::new(ui), tally)
    }

    fn scripted(answers: &[bool], on_enter: Vec<Box<dyn FnOnce()>>) -> Scripted {
        Scripted {
            answers: answers.iter().copied().collect(),
            on_enter: on_enter.into(),
            asked: Arc::default(),
            waited: Arc::default(),
        }
    }

    fn row(ctx: &Ctx, item: Item) -> (Verdict, Option<String>, String) {
        let (v, c, n) = ctx.report.row(item);
        (v, c.map(str::to_owned), n.to_owned())
    }

    /// `thread()` for any `/v1/threads/{id}`: Ada and Bea.
    struct AnyThread;

    impl Respond for AnyThread {
        fn respond(&self, r: &Request) -> ResponseTemplate {
            let id = r.url.path().trim_start_matches("/v1/threads/").to_owned();
            ResponseTemplate::new(200).set_body_json(json!({
                "id": id,
                "members": [
                    {"id": format!("8:orgid:{ME_GUID}"), "friendlyName": "Ada Example"},
                    {"id": format!("8:orgid:{BEA_GUID}"), "friendlyName": "Bea Example"},
                ],
            }))
        }
    }

    #[tokio::test]
    async fn untitled_groups_past_the_thread_cap_are_left_out_of_the_names_verdict() {
        let s = MockServer::start().await;
        mount_basics(&s).await;
        Mock::given(method("GET"))
            .and(path_regex("^/v1/threads/"))
            .respond_with(AnyThread)
            .mount(&s)
            .await;
        let n = MAX_THREAD_FETCHES + 2;
        let convs: Vec<Conversation> = (0..n)
            .map(|i| Conversation {
                id: ConversationId(format!("19:g{i:02}@thread.v2")),
                kind: ic3_teams::ConversationKind::Group,
                topic: None,
                members: None,
                last_message: None,
                read_horizon: None,
                version: i64::try_from(n - i).unwrap(),
                hidden: false,
                extra: serde_json::Map::new(),
            })
            .collect();
        let me = Me {
            mri: Mri(format!("8:orgid:{ME_GUID}")),
            display_name: "Ada Example".into(),
            upn: None,
        };
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = ctx_for(
            &s,
            dir.path(),
            &["smoke", "rest"],
            scripted(&[], Vec::new()),
        )
        .await;
        rest_names(&mut ctx, &me, &convs).await.unwrap();
        let (v, _, note) = row(&ctx, Item::S9_9);
        assert_eq!(v, Verdict::Pass, "{note}");
        assert!(
            note.contains(&format!(
                "{MAX_THREAD_FETCHES} of {MAX_THREAD_FETCHES} chats named"
            )) && note.contains("2 untitled chats past the thread() cap not checked"),
            "{note}"
        );
    }

    #[tokio::test]
    async fn rest_stage_decides_its_five_items() {
        let s = MockServer::start().await;
        mount_basics(&s).await;
        let one_to_one = format!("19:{ME_GUID}_{BEA_GUID}@unq.gbl.spaces");
        Mock::given(method("GET"))
            .and(path("/v1/users/ME/conversations"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "conversations": [
                    {"id": "48:notes", "version": 5, "threadProperties": {}},
                    {"id": one_to_one, "version": 4, "threadProperties": {"uniquerosterthread": "true"},
                     "lastMessage": {"id": "4", "from": contact(BEA_GUID), "imdisplayname": "Bea Example", "messagetype": "Text"}},
                    {"id": "19:topic@thread.v2", "version": 3, "threadProperties": {"topic": "Platform team"}},
                    {"id": "19:untitled@thread.v2", "version": 2, "threadProperties": {},
                     "lastMessage": {"id": "2", "from": contact(ME_GUID), "imdisplayname": "Ada Example", "messagetype": "Text"}},
                    {"id": "19:uni01_dddd@thread.v2", "version": 1, "threadProperties": {},
                     "lastMessage": {"id": "1", "from": contact(ME_GUID), "imdisplayname": "Ada Example", "messagetype": "Text"}},
                ],
                "_metadata": {"totalCount": 5, "syncState": "https://x/sync?token=abc"},
            })))
            .expect(1)
            .mount(&s)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/threads/19:untitled@thread.v2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "19:untitled@thread.v2",
                "members": [
                    {"id": format!("8:orgid:{ME_GUID}"), "friendlyName": "Ada Example"},
                    {"id": format!("8:orgid:{BEA_GUID}"), "friendlyName": "Bea Example"},
                    {"linkedMri": format!("8:orgid:{CY_GUID}")},
                ],
            })))
            .expect(1)
            .mount(&s)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/threads/19:uni01_dddd@thread.v2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "19:uni01_dddd@thread.v2",
                "members": [{"id": format!("8:orgid:{ME_GUID}")}, {"id": format!("8:orgid:{DAN_GUID}")}],
            })))
            .expect(1)
            .mount(&s)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/mt/apac/beta/users/fetchShortProfile"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"value": [
                {"mri": format!("8:orgid:{DAN_GUID}"), "displayName": "Dan Example"},
                {"mri": format!("8:orgid:{CY_GUID}"), "displayName": "Cy Example"},
            ]})))
            .expect(1)
            .mount(&s)
            .await;
        let chat = FakeChat::with(8);
        mount_notes(&s, &chat).await;

        let dir = tempfile::tempdir().unwrap();
        let mut ctx = ctx_for(
            &s,
            dir.path(),
            &["smoke", "rest"],
            scripted(&[], Vec::new()),
        )
        .await;
        run_stages(&mut ctx, &Stage::Rest).await;

        let (v, cap, note) = row(&ctx, Item::S9_1);
        assert_eq!(v, Verdict::Pass, "{note}");
        assert!(cap.unwrap().ends_with("-http-conversations.json"));
        assert!(
            note.contains("5 conversations") && note.contains("syncState yes"),
            "{note}"
        );
        assert_eq!(row(&ctx, Item::S9_10).0, Verdict::Info);
        let (v, cap, note) = row(&ctx, Item::S9_9);
        assert_eq!(v, Verdict::Pass, "{note}");
        assert!(cap.unwrap().ends_with("-http-fetchshortprofile.json"));
        assert!(
            note.contains("5 of 5 chats named")
                && note.contains("thread() 1")
                && note.contains("/api/mt 1")
                && note.contains("/api/mt IS needed"),
            "{note}"
        );
        let (v, cap, note) = row(&ctx, Item::S9_2);
        assert_eq!(v, Verdict::Pass, "{note}");
        assert!(cap.unwrap().ends_with("-http-messages.json"));
        let (v, cap, note) = row(&ctx, Item::S9_11);
        assert_eq!(v, Verdict::Pass, "{note}");
        assert!(cap.unwrap().contains(", "), "both pages are cited");
        assert!(note.contains("straddle"), "{note}");
        for item in [
            Item::S9_3,
            Item::S9_4,
            Item::S9_5,
            Item::S9_6,
            Item::S9_7,
            Item::S9_8,
        ] {
            assert!(
                !ctx.report.decided(item),
                "{item:?} belongs to another stage"
            );
        }
        assert!(chat.posted.lock().unwrap().is_empty(), "rest never writes");
    }

    /// §9.11 asks how `endTime` filters; with paging=auto and a server
    /// that provides a `backwardLink`, the window is never used.
    #[tokio::test]
    async fn end_time_is_not_applicable_when_the_server_provides_a_backward_link() {
        let s = MockServer::start().await;
        mount_basics(&s).await;
        let chat = FakeChat {
            link: true,
            ..FakeChat::with(8)
        };
        mount_notes(&s, &chat).await;
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = ctx_for(
            &s,
            dir.path(),
            &["smoke", "rest"],
            scripted(&[], Vec::new()),
        )
        .await;
        rest_messages(&mut ctx).await.unwrap();
        let (v, _, note) = row(&ctx, Item::S9_2);
        assert_eq!(v, Verdict::Pass, "{note}");
        let (v, _, note) = row(&ctx, Item::S9_11);
        assert_eq!(v, Verdict::Info, "{note}");
        assert!(
            note.contains("not applicable: server provides backwardLink"),
            "{note}"
        );
        assert!(!ctx.report.any_fail());
    }

    #[tokio::test]
    async fn a_failing_rest_step_does_not_hide_the_others() {
        let s = MockServer::start().await;
        mount_basics(&s).await;
        Mock::given(method("GET"))
            .and(path("/v1/users/ME/conversations"))
            .respond_with(
                ResponseTemplate::new(403)
                    .set_body_json(json!({"errorCode": 403, "skypeToken": "leak"})),
            )
            .mount(&s)
            .await;
        mount_notes(&s, &FakeChat::with(3)).await;
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = ctx_for(
            &s,
            dir.path(),
            &["smoke", "rest"],
            scripted(&[], Vec::new()),
        )
        .await;
        run_stages(&mut ctx, &Stage::Rest).await;

        let (v, cap, note) = row(&ctx, Item::S9_1);
        assert_eq!(v, Verdict::Fail);
        assert!(cap.unwrap().ends_with("-http-conversations.json"));
        assert!(
            note.contains("http 403") && !note.contains("leak"),
            "{note}"
        );
        assert_eq!(row(&ctx, Item::S9_10).0, Verdict::Info);
        assert_eq!(row(&ctx, Item::S9_9).0, Verdict::Unverified);
        let (v, _, note) = row(&ctx, Item::S9_2);
        assert_eq!(v, Verdict::Unverified);
        assert!(note.contains("only 3 messages"), "{note}");
        assert_eq!(row(&ctx, Item::S9_11).0, Verdict::Unverified);
    }

    const OFFICIAL_REPLY: &str = "<blockquote itemscope=\"\" itemtype=\"http://schema.skype.com/Reply\" itemid=\"1760000000000\"><strong itemprop=\"mri\" itemid=\"8:orgid:bbbbbbbb-2222-2222-2222-222222222222\">Bea Example</strong><span itemprop=\"time\" itemid=\"1760000000000\"></span><p itemprop=\"preview\">msg 0</p></blockquote><p>yes</p>";

    async fn mount_writes(s: &MockServer, react_status: u16, mark_reads: u64) {
        let reaction = r"^/v1/users/ME/conversations/48:notes/messages/\d+/properties$";
        Mock::given(method("PUT"))
            .and(path_regex(reaction))
            .and(query_param("name", "emotions"))
            .respond_with(
                ResponseTemplate::new(react_status)
                    .set_body_json(json!({"errorCode": react_status})),
            )
            .expect(1)
            .mount(s)
            .await;
        Mock::given(method("DELETE"))
            .and(path_regex(reaction))
            .respond_with(ResponseTemplate::new(200))
            .expect(u64::from(react_status == 200))
            .mount(s)
            .await;
        mount_horizon(s, &FakeHorizon::accepting(), mark_reads, mark_reads).await;
    }

    /// `48:notes`' read state: the conversations listing reports it, the
    /// `consumptionhorizon` PUT stores it (when `accepts`), and the
    /// `consumptionHorizonBookmark` PUT answers `bookmark_status` and, on a
    /// 200, stores the bookmark (when `stores_bookmark`).
    #[derive(Clone)]
    struct FakeHorizon {
        props: Arc<Mutex<(String, String)>>,
        accepts: bool,
        stores_bookmark: bool,
        bookmark_status: u16,
    }

    impl FakeHorizon {
        fn accepting() -> FakeHorizon {
            FakeHorizon {
                props: Arc::new(Mutex::new((
                    "1759999000000;1759999000500;seed".into(),
                    String::new(),
                ))),
                accepts: true,
                stores_bookmark: true,
                bookmark_status: 200,
            }
        }

        /// What "Mark as unread" might do: set the bookmark.
        fn set_bookmark(&self, b: &str) {
            self.props.lock().unwrap().1 = b.into();
        }
    }

    impl Respond for FakeHorizon {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            if req.method.as_str() == "PUT" {
                let body: Value = serde_json::from_slice(&req.body).unwrap();
                if let Some(b) = body.get("consumptionHorizonBookmark") {
                    if self.bookmark_status != 200 {
                        return ResponseTemplate::new(self.bookmark_status)
                            .set_body_json(json!({"errorCode": self.bookmark_status}));
                    }
                    if self.stores_bookmark {
                        self.props.lock().unwrap().1 = b.as_str().unwrap().into();
                    }
                } else if self.accepts {
                    self.props.lock().unwrap().0 =
                        body["consumptionhorizon"].as_str().unwrap().into();
                }
                return ResponseTemplate::new(200);
            }
            let (h, b) = self.props.lock().unwrap().clone();
            ResponseTemplate::new(200).set_body_json(json!({
                "conversations": [
                    {"id": "19:other@thread.v2", "version": 9, "properties": {}, "threadProperties": {}},
                    {"id": "48:notes", "version": 5, "threadProperties": {},
                     "properties": {"consumptionhorizon": h, "consumptionHorizonBookmark": b}},
                ],
                "_metadata": {"totalCount": 2},
            }))
        }
    }

    /// `h` behind the conversations listing and both mark-read PUTs:
    /// `mark_reads` horizon PUTs and `bookmark_puts` bookmark PUTs expected.
    async fn mount_horizon(s: &MockServer, h: &FakeHorizon, mark_reads: u64, bookmark_puts: u64) {
        Mock::given(method("PUT"))
            .and(path("/v1/users/ME/conversations/48:notes/properties"))
            .and(query_param("name", "consumptionhorizon"))
            .respond_with(h.clone())
            .expect(mark_reads)
            .mount(s)
            .await;
        Mock::given(method("PUT"))
            .and(path("/v1/users/ME/conversations/48:notes/properties"))
            .and(query_param("name", "consumptionHorizonBookmark"))
            .respond_with(h.clone())
            .expect(bookmark_puts)
            .mount(s)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/users/ME/conversations"))
            .respond_with(h.clone())
            .mount(s)
            .await;
    }

    /// The `consumptionHorizonBookmark` PUT bodies the server received.
    async fn bookmark_puts(s: &MockServer) -> Vec<Value> {
        s.received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| {
                r.method.as_str() == "PUT"
                    && r.url.query() == Some("name=consumptionHorizonBookmark")
            })
            .map(|r| serde_json::from_slice(&r.body).unwrap())
            .collect()
    }

    #[tokio::test]
    async fn write_stage_sends_replies_reacts_and_marks_read_in_notes() {
        let s = MockServer::start().await;
        mount_basics(&s).await;
        mount_writes(&s, 200, 1).await;
        let chat = FakeChat::with(3);
        mount_notes(&s, &chat).await;
        let official = chat.clone();
        let ui = scripted(
            &[true, true, true, true, true, true, true],
            vec![Box::new(move || {
                let id = official.newest_id() + 1_000;
                official.push(wire_message(
                    id,
                    "official-1",
                    ME_GUID,
                    "Ada Example",
                    OFFICIAL_REPLY,
                ));
            })],
        );
        let asked = ui.asked.clone();
        let waited = ui.waited.clone();
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = ctx_for(&s, dir.path(), &["smoke", "write"], ui).await;
        run_stages(
            &mut ctx,
            &Stage::Write {
                typing: false,
                only: vec![],
            },
        )
        .await;

        let posted = chat.posted.lock().unwrap().clone();
        assert_eq!(posted.len(), 2, "the formatting sample and the reply");
        assert_eq!(
            posted[0]["content"].as_str().unwrap(),
            ic3_teams::format::to_teams_html(&parse_slack(FORMAT_SAMPLE))
        );
        let reply = posted[1]["content"].as_str().unwrap();
        assert!(
            reply.contains("schema.skype.com/Reply") && reply.contains("itemid=\"1760000120000\""),
            "replies to the newest other message: {reply}"
        );
        let (v, cap, note) = row(&ctx, Item::S9_8);
        assert_eq!(v, Verdict::Pass, "{note}");
        assert!(cap.unwrap().contains("-http-send.json"));
        assert!(note.contains("parses as a reply"), "{note}");
        let (v, cap, note) = row(&ctx, Item::S9_15);
        assert_eq!(v, Verdict::Pass, "{note}");
        assert!(cap.unwrap().ends_with("-http-send.json"));
        let qtd: Value =
            serde_json::from_str(posted[1]["properties"]["qtdMsgs"].as_str().unwrap()).unwrap();
        assert_eq!(qtd[0]["messageId"], "1760000120000");
        assert!(
            posted[0].get("properties").is_none(),
            "only the reply carries qtdMsgs"
        );
        let (v, cap, note) = row(&ctx, Item::S9_16);
        assert_eq!(v, Verdict::Pass, "{note}");
        let cap = cap.unwrap();
        assert!(
            cap.contains("-http-edit.json") && cap.contains("-http-messages.json"),
            "{cap}"
        );
        let edits = chat.edits.lock().unwrap().clone();
        assert_eq!(edits.len(), 1, "only the smoke message is edited");
        assert_eq!(edits[0]["contenttype"], "Text");
        assert!(edits[0]["content"].as_str().unwrap().contains(EDIT_NOTE));
        let (v, cap, note) = row(&ctx, Item::S9_17);
        assert_eq!(v, Verdict::Pass, "{note}");
        assert!(cap.unwrap().contains("-http-delete.json"));
        let own = chat
            .msgs
            .lock()
            .unwrap()
            .iter()
            .find(|m| m["clientmessageid"] == posted[0]["clientmessageid"])
            .cloned()
            .unwrap();
        assert!(
            own["properties"]["deletetime"].is_string(),
            "deleted last: {own}"
        );
        let (v, cap, note) = row(&ctx, Item::S9_6);
        assert_eq!(v, Verdict::Pass, "{note}");
        let cap = cap.unwrap();
        assert!(
            cap.contains("-http-react.json") && cap.contains("-http-unreact.json"),
            "{cap}"
        );
        let (v, cap, note) = row(&ctx, Item::S9_7);
        assert_eq!(v, Verdict::Pass, "{note}");
        let cap = cap.unwrap();
        assert!(
            cap.contains("-http-mark-read.json") && cap.contains("-http-conversations.json"),
            "{cap}"
        );
        assert!(
            note.contains("server horizon now")
                && note.contains(
                    "before mark-as-unread: consumptionhorizon 1759999000000;1759999000500;seed"
                ),
            "{note}"
        );
        let bookmarks = bookmark_puts(&s).await;
        assert_eq!(bookmarks.len(), 1, "mark read also clears the bookmark");
        let b = bookmarks[0]["consumptionHorizonBookmark"].as_str().unwrap();
        let fields: Vec<&str> = b.split(';').collect();
        assert!(
            fields.len() == 3
                && fields[0] == "0"
                && fields[2] == "0"
                && fields[1].parse::<Timestamp>().is_ok(),
            "{b}"
        );
        let (v, cap, note) = row(&ctx, Item::S9_7b);
        assert_eq!(
            v,
            Verdict::Unverified,
            "Mark as unread set no bookmark here"
        );
        assert!(note.contains("never set once unread"), "{note}");
        assert!(cap.unwrap().contains("-http-mark-read-bookmark.json"));
        assert_eq!(ctx.report.info.len(), 1);
        assert!(
            ctx.report.info[0].starts_with("formatting  [")
                && ctx.report.info[0].contains("looks right")
        );
        let asked = asked.lock().unwrap();
        assert!(asked[2].contains("click the quote"), "{asked:?}");
        assert_eq!(asked.len(), 7, "{asked:?}");
        assert!(
            asked.iter().all(|q| q.contains("your Notes (self) chat")),
            "every question names the chat: {asked:?}"
        );
        assert!(
            asked[5].contains("now show as unread") && asked[6].starts_with("Without opening it")
        );
        let waited = waited.lock().unwrap();
        let unread = waited
            .iter()
            .find(|w| w.contains("Mark as unread"))
            .unwrap();
        assert!(
            !unread.contains("have someone send"),
            "nobody else can post to the self chat: {unread}"
        );
        let (v, _, note) = row(&ctx, Item::S9_18);
        assert_eq!(v, Verdict::Unverified);
        assert!(note.starts_with("not run: add --typing"), "{note}");
    }

    /// The user's live result: the PUT is stored server-side, but the badge
    /// set by "Mark as unread" stays. §9.7 says so instead of guessing; a
    /// server that ignores our bookmark PUT fails §9.7b.
    #[tokio::test]
    async fn mark_read_server_accepted_but_badge_not_cleared_is_a_fail_with_the_bookmark() {
        let s = MockServer::start().await;
        mount_basics(&s).await;
        let h = FakeHorizon {
            stores_bookmark: false,
            ..FakeHorizon::accepting()
        };
        mount_horizon(&s, &h, 1, 1).await;
        let chat = FakeChat::with(3);
        mount_notes(&s, &chat).await;
        let unread = h.clone();
        let ui = scripted(
            &[true, false],
            vec![Box::new(move || {
                unread.set_bookmark("1760000060000;1760000300000;seed1");
            })],
        );
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = ctx_for(&s, dir.path(), &["smoke", "write"], ui).await;
        write_mark_read(
            &mut ctx,
            &ConversationId(NOTES.into()),
            "your Notes (self) chat",
        )
        .await
        .unwrap();
        let (v, _, note) = row(&ctx, Item::S9_7);
        assert_eq!(v, Verdict::Fail, "{note}");
        assert!(
            note.contains("server accepted; badge not cleared (possibly mark-as-unread bookmark")
                && note.contains("consumptionHorizonBookmark 1760000060000;1760000300000;seed1"),
            "{note}"
        );
        let (v, cap, note) = row(&ctx, Item::S9_7b);
        assert_eq!(v, Verdict::Fail, "{note}");
        assert!(
            note.contains("still set after our PUT")
                && note.contains("1760000060000;1760000300000;seed1"),
            "{note}"
        );
        let cap = cap.unwrap();
        assert!(
            cap.contains("-http-mark-read-bookmark.json")
                && cap.contains("-http-conversations.json"),
            "{cap}"
        );
    }

    /// §9.7b's case: Mark as unread set the bookmark, our PUT cleared it
    /// and the badge went away.
    #[tokio::test]
    async fn clearing_a_mark_as_unread_bookmark_passes_9_7b() {
        let s = MockServer::start().await;
        mount_basics(&s).await;
        let h = FakeHorizon::accepting();
        mount_horizon(&s, &h, 1, 1).await;
        let chat = FakeChat::with(3);
        mount_notes(&s, &chat).await;
        let unread = h.clone();
        let ui = scripted(
            &[true, true],
            vec![Box::new(move || {
                unread.set_bookmark("1760000060000;1760000300000;seed1");
            })],
        );
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = ctx_for(&s, dir.path(), &["smoke", "write"], ui).await;
        write_mark_read(
            &mut ctx,
            &ConversationId(NOTES.into()),
            "your Notes (self) chat",
        )
        .await
        .unwrap();
        let (v, _, note) = row(&ctx, Item::S9_7b);
        assert_eq!(v, Verdict::Pass, "{note}");
        assert!(
            note.contains("once unread 1760000060000;1760000300000;seed1")
                && note.contains("after our PUT 0;"),
            "{note}"
        );
        assert_eq!(row(&ctx, Item::S9_7).0, Verdict::Pass);
    }

    /// `--no-clear-bookmark`: today's single horizon PUT, §9.7b UNVERIFIED.
    #[tokio::test]
    async fn no_clear_bookmark_sends_only_the_horizon() {
        let s = MockServer::start().await;
        mount_basics(&s).await;
        let h = FakeHorizon::accepting();
        mount_horizon(&s, &h, 1, 0).await;
        let chat = FakeChat::with(3);
        mount_notes(&s, &chat).await;
        let unread = h.clone();
        let ui = scripted(
            &[true, true],
            vec![Box::new(move || {
                unread.set_bookmark("1760000060000;1760000300000;seed1");
            })],
        );
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = ctx_for(
            &s,
            dir.path(),
            &["smoke", "--no-clear-bookmark", "write"],
            ui,
        )
        .await;
        write_mark_read(
            &mut ctx,
            &ConversationId(NOTES.into()),
            "your Notes (self) chat",
        )
        .await
        .unwrap();
        assert!(bookmark_puts(&s).await.is_empty());
        let (v, cap, note) = row(&ctx, Item::S9_7b);
        assert_eq!(v, Verdict::Unverified, "{note}");
        assert!(note.contains("--no-clear-bookmark"), "{note}");
        assert_eq!(cap, None);
    }

    /// A refused bookmark PUT fails §9.7b; the accepted horizon PUT is
    /// still judged for §9.7.
    #[tokio::test]
    async fn a_refused_bookmark_put_fails_9_7b_and_still_decides_9_7() {
        let s = MockServer::start().await;
        mount_basics(&s).await;
        let h = FakeHorizon {
            bookmark_status: 403,
            ..FakeHorizon::accepting()
        };
        mount_horizon(&s, &h, 1, 1).await;
        let chat = FakeChat::with(3);
        mount_notes(&s, &chat).await;
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = ctx_for(
            &s,
            dir.path(),
            &["smoke", "write"],
            scripted(&[true, true], vec![]),
        )
        .await;
        write_mark_read(
            &mut ctx,
            &ConversationId(NOTES.into()),
            "your Notes (self) chat",
        )
        .await
        .unwrap();
        let (v, cap, note) = row(&ctx, Item::S9_7b);
        assert_eq!(v, Verdict::Fail, "{note}");
        assert!(
            note.starts_with("consumptionHorizonBookmark PUT failed: http 403"),
            "{note}"
        );
        assert!(cap.unwrap().ends_with("-http-mark-read-bookmark.json"));
        let (v, cap, note) = row(&ctx, Item::S9_7);
        assert_eq!(v, Verdict::Pass, "{note}");
        let cap = cap.unwrap();
        assert!(
            cap.contains("-http-mark-read.json") && !cap.contains("bookmark"),
            "{cap}"
        );
    }

    /// Closes the connection without answering (the server's connection
    /// task panics): a transport error for the client.
    struct NoResponse;

    impl Respond for NoResponse {
        fn respond(&self, _: &Request) -> ResponseTemplate {
            panic!("closing the connection without a response")
        }
    }

    /// The horizon PUT was answered 2xx, then the bookmark PUT got no
    /// response at all: §9.7b fails without a capture, §9.7 is judged.
    #[tokio::test]
    async fn a_bookmark_put_without_a_response_fails_9_7b_and_still_decides_9_7() {
        let s = MockServer::start().await;
        mount_basics(&s).await;
        Mock::given(method("PUT"))
            .and(path("/v1/users/ME/conversations/48:notes/properties"))
            .and(query_param("name", "consumptionHorizonBookmark"))
            .respond_with(NoResponse)
            .with_priority(1)
            .mount(&s)
            .await;
        mount_horizon(&s, &FakeHorizon::accepting(), 1, 0).await;
        let chat = FakeChat::with(3);
        mount_notes(&s, &chat).await;
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = ctx_for(
            &s,
            dir.path(),
            &["smoke", "write"],
            scripted(&[true, true], vec![]),
        )
        .await;
        write_mark_read(
            &mut ctx,
            &ConversationId(NOTES.into()),
            "your Notes (self) chat",
        )
        .await
        .unwrap();
        assert!(
            captures_after(Some(dir.path()), None, "-http-mark-read-bookmark.json").is_empty(),
            "no response, no capture"
        );
        let (v, cap, note) = row(&ctx, Item::S9_7b);
        assert_eq!(v, Verdict::Fail, "{note}");
        assert!(
            note.starts_with("consumptionHorizonBookmark PUT failed: "),
            "{note}"
        );
        assert_eq!(cap, None);
        let (v, cap, note) = row(&ctx, Item::S9_7);
        assert_eq!(v, Verdict::Pass, "{note}");
        assert!(cap.unwrap().contains("-http-mark-read.json"));
    }

    /// `write_mark_read` against a refused horizon PUT: §9.7 fails, §9.7b
    /// (never sent) is UNVERIFIED.
    async fn refused_horizon(argv: &[&str]) -> Ctx {
        let s = MockServer::start().await;
        mount_basics(&s).await;
        Mock::given(method("PUT"))
            .and(path("/v1/users/ME/conversations/48:notes/properties"))
            .and(query_param("name", "consumptionhorizon"))
            .respond_with(ResponseTemplate::new(403).set_body_json(json!({"errorCode": 403})))
            .with_priority(1)
            .expect(1)
            .mount(&s)
            .await;
        mount_horizon(&s, &FakeHorizon::accepting(), 0, 0).await;
        let chat = FakeChat::with(3);
        mount_notes(&s, &chat).await;
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = ctx_for(&s, dir.path(), argv, scripted(&[true], vec![])).await;
        write_mark_read(
            &mut ctx,
            &ConversationId(NOTES.into()),
            "your Notes (self) chat",
        )
        .await
        .unwrap();
        let (v, cap, note) = row(&ctx, Item::S9_7);
        assert_eq!(v, Verdict::Fail, "{note}");
        assert!(note.starts_with("mark_read: http 403"), "{note}");
        assert!(cap.unwrap().ends_with("-http-mark-read.json"));
        assert_eq!(row(&ctx, Item::S9_7b).0, Verdict::Unverified);
        ctx
    }

    #[tokio::test]
    async fn a_refused_horizon_put_leaves_9_7b_unsent() {
        let ctx = refused_horizon(&["smoke", "write"]).await;
        let (_, cap, note) = row(&ctx, Item::S9_7b);
        assert_eq!(note, "not sent: the consumptionhorizon PUT failed (§9.7)");
        assert_eq!(cap, None);
        let ctx = refused_horizon(&["smoke", "--no-clear-bookmark", "write"]).await;
        let (_, _, note) = row(&ctx, Item::S9_7b);
        assert_eq!(note, "not sent: --no-clear-bookmark");
    }

    #[test]
    fn bookmark_verdicts() {
        let props = |b: Option<&str>| HorizonProps {
            horizon: Some("1759999000000;1759999000500;seed".into()),
            bookmark: b.map(str::to_owned),
        };
        let set = props(Some("1760000060000;1760000300000;seed1"));
        let cleared = props(Some("0;1770000000100;0"));
        let v = |unread: Option<&HorizonProps>, after: Option<&HorizonProps>, badge| {
            bookmark_verdict(true, unread, after, badge)
        };
        let (p, note) = v(Some(&set), Some(&cleared), true);
        assert_eq!(p, Verdict::Pass, "{note}");
        for empty in [props(Some("")), props(None)] {
            assert_eq!(
                v(Some(&set), Some(&empty), true).0,
                Verdict::Pass,
                "empty is cleared"
            );
        }
        let (f, note) = v(Some(&set), Some(&set), true);
        assert_eq!(f, Verdict::Fail);
        assert!(note.contains("still set after our PUT"), "{note}");
        let (f, note) = v(Some(&set), Some(&set), false);
        assert_eq!(f, Verdict::Fail);
        assert!(
            note.contains("still set after our PUT") && note.contains("did not clear"),
            "{note}"
        );
        let (f, note) = v(Some(&set), Some(&cleared), false);
        assert_eq!(f, Verdict::Fail);
        assert!(
            note.contains("the badge did not clear while the bookmark was set"),
            "{note}"
        );
        assert_eq!(v(Some(&set), None, false).0, Verdict::Fail);
        assert_eq!(
            v(Some(&set), None, true).0,
            Verdict::Unverified,
            "the badge cleared but the bookmark could not be read back"
        );
        // never set once unread: Teams used the horizon (§9.7 decides)
        for unset in [
            props(Some("")),
            props(None),
            props(Some("0;1760000300000;0")),
        ] {
            for badge in [true, false] {
                let (u, note) = v(Some(&unset), Some(&cleared), badge);
                assert_eq!(u, Verdict::Unverified, "{note}");
                assert!(note.contains("never set once unread"), "{note}");
            }
        }
        assert_eq!(v(None, Some(&cleared), true).0, Verdict::Unverified);
        let (u, note) = bookmark_verdict(false, Some(&set), Some(&set), false);
        assert_eq!(u, Verdict::Unverified);
        assert!(note.contains("--no-clear-bookmark"), "{note}");
    }

    #[test]
    fn mark_read_verdicts() {
        let sent = MessageRef {
            id: MessageId("1760000120000".into()),
            client_message_id: Some("seed2".into()),
        };
        let window = (1_770_000_000_000, 1_770_000_000_500);
        let props = |h: &str| HorizonProps {
            horizon: Some(h.into()),
            bookmark: Some(String::new()),
        };
        let ours = props("1760000120000;1770000000100;seed2");
        let old = props("1759999000000;1759999000500;seed");
        let v = |after: Option<&HorizonProps>, badge| {
            mark_read_verdict(&sent, window, Some(&old), after, badge)
        };
        let (p, note) = v(Some(&ours), true);
        assert_eq!(p, Verdict::Pass, "{note}");
        assert!(
            note.contains("server horizon now 1760000120000;1770000000100;seed2"),
            "{note}"
        );
        let (f, note) = v(Some(&ours), false);
        assert_eq!(f, Verdict::Fail);
        assert!(
            note.contains("server accepted; badge not cleared"),
            "{note}"
        );
        let (f, note) = v(Some(&props("1759999000000;1759999000500;seed")), true);
        assert_eq!(f, Verdict::Fail, "badge alone is not enough");
        assert!(note.contains("not ours"), "{note}");
        let (f, note) = v(Some(&props("1759999000000;1759999000500;seed")), false);
        assert_eq!(f, Verdict::Fail);
        assert!(note.contains("did not move"), "{note}");
        // our message, but a timestamp outside the call: the server rewrote it
        let (p, note) = v(Some(&props("1760000120000;1;seed2")), true);
        assert_eq!(p, Verdict::Pass, "{note}");
        assert!(note.contains("timestamp differs"), "{note}");
        assert_eq!(v(None, true).0, Verdict::Unverified);
        assert_eq!(v(None, false).0, Verdict::Fail);
        let absent = HorizonProps::default();
        assert_eq!(v(Some(&absent), true).0, Verdict::Fail);
        assert_eq!(
            absent.line(),
            "consumptionhorizon absent, consumptionHorizonBookmark absent"
        );
        // Once unread, the horizon already pointed at the message we then
        // marked: the PUT changed nothing observable, whatever the badge did.
        let already = props("1760000120000;1769999999000;seed2");
        for badge in [true, false] {
            let (u, note) = mark_read_verdict(&sent, window, Some(&already), Some(&ours), badge);
            assert_eq!(u, Verdict::Unverified, "{note}");
            assert!(
                note.contains("the PUT's effect is not observable"),
                "{note}"
            );
        }
        // an unreadable "once unread" state does not block a verdict
        assert_eq!(
            mark_read_verdict(&sent, window, None, Some(&ours), true).0,
            Verdict::Pass
        );
        assert_eq!(
            props("a;1;").line(),
            "consumptionhorizon a;1;, consumptionHorizonBookmark \"\""
        );
    }

    /// Every write item but `run` is `UNVERIFIED … not run (--only)`.
    fn assert_only_ran(ctx: &Ctx, run: &[Item]) {
        for item in WRITE_ITEMS {
            let (v, cap, note) = row(ctx, item);
            if run.contains(&item) {
                assert_ne!(note, NOT_RUN_ONLY, "{item:?}");
            } else {
                assert_eq!(
                    (v, cap, note.as_str()),
                    (Verdict::Unverified, None, NOT_RUN_ONLY),
                    "{item:?}"
                );
            }
        }
    }

    #[tokio::test]
    async fn write_only_mark_read_sends_no_message() {
        let s = MockServer::start().await;
        mount_basics(&s).await;
        mount_horizon(&s, &FakeHorizon::accepting(), 1, 1).await;
        let chat = FakeChat::with(3);
        mount_notes(&s, &chat).await;
        let ui = scripted(&[true, true], Vec::new());
        let asked = ui.asked.clone();
        let dir = tempfile::tempdir().unwrap();
        let argv = ["smoke", "write", "--only", "9.7,9.7b"];
        let mut ctx = ctx_for(&s, dir.path(), &argv, ui).await;
        run_stages(&mut ctx, &parse(&argv).stage).await;

        assert!(chat.posted.lock().unwrap().is_empty(), "nothing sent");
        assert!(chat.edits.lock().unwrap().is_empty());
        let asked = asked.lock().unwrap();
        assert_eq!(asked.len(), 2, "the mark-read questions only: {asked:?}");
        assert!(asked[0].contains("now show as unread"), "{asked:?}");
        assert_eq!(row(&ctx, Item::S9_7).0, Verdict::Pass);
        assert_ne!(row(&ctx, Item::S9_7b).2, NOT_RUN_ONLY);
        assert_only_ran(&ctx, &[Item::S9_7, Item::S9_7b]);
        assert!(ctx.report.info.is_empty(), "no formatting check");
    }

    #[tokio::test]
    async fn write_only_edit_sends_edits_and_deletes_the_smoke_message() {
        let s = MockServer::start().await;
        mount_basics(&s).await;
        mount_horizon(&s, &FakeHorizon::accepting(), 0, 0).await;
        let chat = FakeChat::with(3);
        mount_notes(&s, &chat).await;
        let ui = scripted(&[true], Vec::new());
        let dir = tempfile::tempdir().unwrap();
        let argv = ["smoke", "write", "--only", "§9.16"];
        let mut ctx = ctx_for(&s, dir.path(), &argv, ui).await;
        run_stages(&mut ctx, &parse(&argv).stage).await;

        let posted = chat.posted.lock().unwrap().clone();
        assert_eq!(posted.len(), 1, "the smoke message only, no reply");
        assert_eq!(chat.edits.lock().unwrap().len(), 1);
        let (v, _, note) = row(&ctx, Item::S9_16);
        assert_eq!(v, Verdict::Pass, "{note}");
        let (v, cap, note) = row(&ctx, Item::S9_17);
        assert_eq!(v, Verdict::Pass, "the cleanup delete decides §9.17: {note}");
        assert!(cap.unwrap().contains("-http-delete.json"));
        let own = chat
            .msgs
            .lock()
            .unwrap()
            .iter()
            .find(|m| m["clientmessageid"] == posted[0]["clientmessageid"])
            .cloned()
            .unwrap();
        assert!(own["properties"]["deletetime"].is_string(), "{own}");
        assert_only_ran(&ctx, &[Item::S9_16, Item::S9_17]);
        let property_writes = s
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| r.method.as_str() != "GET" && r.url.path().ends_with("/properties"))
            .count();
        assert_eq!(property_writes, 0, "no react, no mark read");
    }

    /// No message was sent: nothing to clean up, so the delete step does
    /// not run for an unselected §9.17.
    #[tokio::test]
    async fn write_only_without_a_sent_message_skips_the_cleanup() {
        let s = MockServer::start().await;
        mount_basics(&s).await;
        let chat = FakeChat::with(3);
        // The send is refused; reads still work.
        Mock::given(method("POST"))
            .and(path("/v1/users/ME/conversations/48:notes/messages"))
            .respond_with(ResponseTemplate::new(403))
            .with_priority(1)
            .mount(&s)
            .await;
        mount_notes(&s, &chat).await;
        let dir = tempfile::tempdir().unwrap();
        let argv = ["smoke", "write", "--only", "9.16"];
        let mut ctx = ctx_for(&s, dir.path(), &argv, scripted(&[], Vec::new())).await;
        run_stages(&mut ctx, &parse(&argv).stage).await;
        let (v, _, note) = row(&ctx, Item::S9_16);
        assert_eq!(v, Verdict::Unverified);
        assert!(note.contains("was not sent"), "{note}");
        assert_only_ran(&ctx, &[Item::S9_16]);
    }

    #[tokio::test]
    async fn write_only_typing_runs_typing_alone_in_the_conv_chat() {
        let s = MockServer::start().await;
        mount_basics(&s).await;
        Mock::given(method("GET"))
            .and(path("/v1/threads/19:chosen@thread.v2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "19:chosen@thread.v2", "properties": {"topic": "Platform team"}, "members": []
            })))
            .mount(&s)
            .await;
        Mock::given(method("POST"))
            .and(path(
                "/v1/users/ME/conversations/19:chosen@thread.v2/messages",
            ))
            .and(body_json(json!({
                "content": "", "messagetype": "Control/Typing", "contenttype": "Application/Message"
            })))
            .respond_with(ResponseTemplate::new(201))
            .with_priority(1)
            .expect(1)
            .mount(&s)
            .await;
        // Consent, then "did they see you typing?".
        let ui = scripted(&[true, true], Vec::new());
        let asked = ui.asked.clone();
        let dir = tempfile::tempdir().unwrap();
        let argv = [
            "smoke",
            "--conv",
            "19:chosen@thread.v2",
            "write",
            "--only",
            "9.18",
        ];
        let mut ctx = ctx_for(&s, dir.path(), &argv, ui).await;
        run_stages(&mut ctx, &parse(&argv).stage).await;
        assert_eq!(row(&ctx, Item::S9_18).0, Verdict::Pass);
        assert_only_ran(&ctx, &[Item::S9_18]);
        assert!(asked.lock().unwrap()[0].starts_with("Post these to"));
    }

    #[tokio::test]
    async fn declining_an_only_run_marks_the_rest_not_run() {
        let s = MockServer::start().await;
        mount_basics(&s).await;
        let dir = tempfile::tempdir().unwrap();
        let argv = [
            "smoke",
            "--conv",
            "19:chosen@thread.v2",
            "write",
            "--only",
            "9.7",
        ];
        let mut ctx = ctx_for(&s, dir.path(), &argv, scripted(&[false], Vec::new())).await;
        run_stages(&mut ctx, &parse(&argv).stage).await;
        let (v, _, note) = row(&ctx, Item::S9_7);
        assert_eq!(v, Verdict::Unverified);
        assert!(note.contains("declined"), "{note}");
        assert_only_ran(&ctx, &[Item::S9_7]);
    }

    #[tokio::test]
    async fn write_stage_reports_a_failed_react_and_carries_on() {
        let s = MockServer::start().await;
        mount_basics(&s).await;
        mount_writes(&s, 403, 0).await;
        let chat = FakeChat::with(3);
        mount_notes(&s, &chat).await;
        let other = chat.clone();
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = ctx_for(
            &s,
            dir.path(),
            &["smoke", "write", "--yes"],
            // formatting, reply shown?, "is it unread now?" (no)
            scripted(
                &[true, false, false],
                vec![Box::new(move || {
                    // Someone else posts while the user is asked to reply:
                    // not a candidate for the official reply.
                    let id = other.newest_id() + 1_000;
                    other.push(wire_message(
                        id,
                        "bea-1",
                        BEA_GUID,
                        "Bea Example",
                        "<p>hi</p>",
                    ));
                })],
            ),
        )
        .await;
        run_stages(
            &mut ctx,
            &Stage::Write {
                typing: false,
                only: vec![],
            },
        )
        .await;

        let (v, cap, note) = row(&ctx, Item::S9_6);
        assert_eq!(v, Verdict::Fail);
        assert!(cap.unwrap().ends_with("-http-react.json"));
        assert!(note.starts_with("react: http 403: "), "{note}");
        let (v, _, note) = row(&ctx, Item::S9_8);
        assert_eq!(v, Verdict::Fail, "the user said no");
        assert!(
            note.contains("no official reply of yours found") && note.contains("--yes"),
            "{note}"
        );
        let (v, cap, note) = row(&ctx, Item::S9_7);
        assert_eq!(v, Verdict::Unverified, "not unread first: no PUT");
        assert_eq!(cap, None);
        assert!(
            note.contains("could not be made unread") && note.contains("NOT verified"),
            "--yes answers are flagged: {note}"
        );
        let (v, cap, note) = row(&ctx, Item::S9_7b);
        assert_eq!(v, Verdict::Unverified, "not unread first: no PUT");
        assert_eq!(cap, None);
        assert!(note.contains("could not be made unread"), "{note}");
        assert_eq!(
            row(&ctx, Item::S9_15).0,
            Verdict::Unverified,
            "not shown as a reply: not asked"
        );
        for item in [Item::S9_16, Item::S9_17] {
            assert_eq!(
                row(&ctx, item).0,
                Verdict::Pass,
                "{item:?}: a failed react hides nothing"
            );
        }
    }

    #[tokio::test]
    async fn no_qtd_msgs_sends_the_old_reply_payload_and_leaves_9_15_unverified() {
        let s = MockServer::start().await;
        mount_basics(&s).await;
        mount_writes(&s, 200, 1).await;
        let chat = FakeChat::with(3);
        mount_notes(&s, &chat).await;
        let ui = scripted(&[true; 6], Vec::new());
        let asked = ui.asked.clone();
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = ctx_for(&s, dir.path(), &["smoke", "--no-qtd-msgs", "write"], ui).await;
        run_stages(
            &mut ctx,
            &Stage::Write {
                typing: false,
                only: vec![],
            },
        )
        .await;
        let posted = chat.posted.lock().unwrap().clone();
        assert!(posted[1].get("properties").is_none(), "{}", posted[1]);
        let (v, _, note) = row(&ctx, Item::S9_15);
        assert_eq!(v, Verdict::Unverified);
        assert!(note.contains("--no-qtd-msgs"), "{note}");
        assert!(
            !asked
                .lock()
                .unwrap()
                .iter()
                .any(|q| q.contains("click the quote"))
        );
    }

    #[tokio::test]
    async fn write_to_a_conv_chat_asks_first_and_stops_on_no() {
        let s = MockServer::start().await;
        mount_basics(&s).await;
        Mock::given(method("GET"))
            .and(path("/v1/threads/19:chosen@thread.v2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "19:chosen@thread.v2", "properties": {"topic": "Platform team"}, "members": []
            })))
            .expect(1)
            .mount(&s)
            .await;
        let ui = scripted(&[false], Vec::new());
        let asked = ui.asked.clone();
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = ctx_for(
            &s,
            dir.path(),
            &["smoke", "--conv", "19:chosen@thread.v2", "write"],
            ui,
        )
        .await;
        run_stages(
            &mut ctx,
            &Stage::Write {
                typing: false,
                only: vec![],
            },
        )
        .await;
        let asked = asked.lock().unwrap();
        assert_eq!(asked.len(), 1);
        assert_eq!(
            asked[0],
            "Post these to the chat \"Platform team\" (19:chosen@thread.v2)?"
        );
        for item in [Item::S9_6, Item::S9_7, Item::S9_7b, Item::S9_8] {
            let (v, _, note) = row(&ctx, item);
            assert_eq!(v, Verdict::Unverified);
            assert!(note.contains("declined"), "{note}");
        }
    }

    #[tokio::test]
    async fn captures_are_cited_only_when_the_request_wrote_one() {
        let s = MockServer::start().await;
        mount_basics(&s).await;
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_for(
            &s,
            dir.path(),
            &["smoke", "rest"],
            scripted(&[], Vec::new()),
        )
        .await;
        let before = ctx.capture_mark();
        assert_eq!(before, None);
        ctx.client.me().await.unwrap();
        let me_file = ctx.capture_since(&before).unwrap();
        assert!(me_file.ends_with("-http-me.json"), "{me_file}");
        let mark = ctx.capture_mark();
        ctx.client.me().await.unwrap(); // cached: no request, no file
        assert_eq!(ctx.capture_since(&mark), None, "never the stale file");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn prompts_step_off_a_multi_thread_runtime() {
        assert_eq!(off_runtime(|| 7), 7);
    }

    #[tokio::test]
    async fn prompts_run_inline_on_a_current_thread_runtime() {
        // block_in_place would panic here.
        assert_eq!(off_runtime(|| 8), 8);
    }

    #[test]
    fn official_reply_is_the_newest_of_mine_after_ours() {
        let me = ic3_teams::Mri("8:orgid:b".into());
        let mut theirs = msg(60, "x", "Text");
        theirs.from = ic3_teams::Mri("8:orgid:z".into());
        let items = [
            theirs,
            msg(50, "smoke-reply", "Text"),
            msg(45, "mine", "Text"),
            msg(40, "smoke-1", "Text"),
            msg(30, "old", "Text"),
        ];
        let ours = ic3_teams::MessageId("40".into());
        let pick = |after| {
            official_reply_candidate(&items, &me, &["smoke-reply", "smoke-1"], after)
                .map(|m| m.id.0.clone())
        };
        assert_eq!(pick(Some(&ours)).as_deref(), Some("45"));
        let newest = ic3_teams::MessageId("45".into());
        assert_eq!(pick(Some(&newest)), None);
        assert_eq!(pick(None).as_deref(), Some("45"));
    }

    #[tokio::test]
    async fn write_refuses_a_chat_not_chosen_with_conv() {
        let s = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_for(
            &s,
            dir.path(),
            &["smoke", "--conv", "19:chosen@thread.v2", "write"],
            scripted(&[], Vec::new()),
        )
        .await;
        assert_eq!(ctx.target(), ConversationId("19:chosen@thread.v2".into()));
        assert!(ctx.guard_write(&ctx.target()).is_ok());
        assert!(ctx.guard_write(&ConversationId(NOTES.into())).is_ok());
        let e = ctx
            .guard_write(&ConversationId("19:other@thread.v2".into()))
            .unwrap_err();
        assert!(
            e.to_string()
                .contains("refusing to write to 19:other@thread.v2"),
            "{e}"
        );
    }

    #[tokio::test]
    async fn a_failed_stage_leaves_its_items_unverified_and_later_stages_run() {
        let s = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = ctx_for(
            &s,
            dir.path(),
            &["smoke", "events"],
            scripted(&[], Vec::new()),
        )
        .await;
        run_stages(&mut ctx, &Stage::Events { minutes: 1 }).await;
        for item in [Item::S9_3, Item::S9_4, Item::S9_5] {
            let (v, _, note) = row(&ctx, item);
            assert_eq!(v, Verdict::Unverified);
            assert!(note.starts_with("stage events stopped early: "), "{note}");
        }
        assert!(!ctx.report.decided(Item::S9_1));
    }

    // ----- events stage against wiremock and a fake websocket -----

    /// A websocket whose inbound frames the test sends; outbound ones are
    /// dropped.
    struct FakeWs {
        rx: tokio::sync::mpsc::Receiver<String>,
    }

    #[async_trait::async_trait]
    impl Transport for FakeWs {
        async fn send(&mut self, _text: String) -> ic3_teams::Result<()> {
            Ok(())
        }

        async fn recv(&mut self) -> ic3_teams::Result<Option<String>> {
            Ok(self.rx.recv().await)
        }

        async fn close(&mut self) {}
    }

    /// Hands the sending end of every connection to the test.
    struct FakeWsFactory {
        conns: tokio::sync::mpsc::UnboundedSender<tokio::sync::mpsc::Sender<String>>,
    }

    #[async_trait::async_trait]
    impl TransportFactory for FakeWsFactory {
        async fn connect(
            &self,
            _url: &str,
            _skype_token: &Secret<String>,
        ) -> ic3_teams::Result<Box<dyn Transport>> {
            let (tx, rx) = tokio::sync::mpsc::channel(64);
            self.conns
                .send(tx)
                .map_err(|_| ic3_teams::Error::Network("test gone".into()))?;
            Ok(Box::new(FakeWs { rx }))
        }
    }

    async fn mount_trouter(s: &MockServer) {
        Mock::given(method("POST"))
            .and(path("/v4/a"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "socketio": format!("{}/", s.uri()),
                "surl": "https://trouter.example/v4/f/AbCdEf123456/",
                "ccid": "ccid-xyz",
                "connectparams": {"sr": "sr", "issuer": "prod-2", "sp": "connect", "se": "1", "st": "1", "sig": "sig"},
            })))
            .mount(s)
            .await;
        Mock::given(method("GET"))
            .and(path("/socket.io/1/"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string("sessid-abc123:40:60:websocket,xhr-polling"),
            )
            .mount(s)
            .await;
        Mock::given(method("POST"))
            .and(path("/registrar/prod/V2/registrations"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .mount(s)
            .await;
    }

    /// The §9.4 fixtures, in the order a user would cause them.
    const EVENT_FIXTURES: [&str; 7] = [
        include_str!("../tests/fixtures/9.04-event-payloads/new-message.json"),
        include_str!("../tests/fixtures/9.04-event-payloads/message-edit.json"),
        include_str!("../tests/fixtures/9.04-event-payloads/reaction-add.json"),
        include_str!("../tests/fixtures/9.04-event-payloads/message-delete.json"),
        include_str!("../tests/fixtures/9.04-event-payloads/read-horizon.json"),
        include_str!("../tests/fixtures/9.04-event-payloads/topic-update.json"),
        include_str!("../tests/fixtures/9.04-event-payloads/thread-update.json"),
    ];

    /// A trouter `/messaging` request frame carrying `body`.
    fn messaging_frame(id: usize, body: &str) -> String {
        let req = json!({
            "id": id, "method": "POST", "url": "/v4/f/AbCdEf123456/messaging",
            "headers": {}, "body": body,
        });
        format!("3:::{req}")
    }

    /// Waits (real time, at most 10 s) until `done` holds for the tally.
    async fn until(tally: &Tally, done: impl Fn(&EventTally) -> bool) {
        for _ in 0..1_000 {
            if done(&lock(tally)) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("timed out; tally: {:?}", lock(tally));
    }

    #[tokio::test]
    async fn events_stage_maps_every_fixture_and_decides_its_items() {
        let s = MockServer::start().await;
        mount_basics(&s).await;
        mount_trouter(&s).await;
        let (conns_tx, mut conns) = tokio::sync::mpsc::unbounded_channel();
        let factory = Arc::new(FakeWsFactory { conns: conns_tx });
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = ctx_with(
            &s,
            dir.path(),
            &["smoke", "events", "--minutes", "1"],
            scripted(&[], Vec::new()),
            Some(factory),
        )
        .await;
        let tally = ctx.tally.clone();
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let user = async move {
            let ws = conns.recv().await.unwrap();
            ws.send("1::".into()).await.unwrap();
            for (i, body) in EVENT_FIXTURES.iter().enumerate() {
                ws.send(messaging_frame(7001 + i, body)).await.unwrap();
            }
            until(&tally, |t| {
                t.connected && t.events_seen() == EVENT_FIXTURES.len()
            })
            .await;
            stop_tx.send(()).unwrap();
            ws
        };
        let stop = async {
            let _ = stop_rx.await;
            "test done".to_owned()
        };
        let (listened, _ws) = tokio::join!(events_listen(&mut ctx, 1, stop), user);
        listened.unwrap();

        let (v, cap, note) = row(&ctx, Item::S9_3);
        assert_eq!(v, Verdict::Pass, "{note}");
        let cap = cap.unwrap();
        assert!(
            cap.contains("-http-registrar.json") && cap.contains("-ws-in-trouter-request.json"),
            "{cap}"
        );
        assert!(note.contains("7 EventMessage(s)"), "{note}");
        let (v, cap, note) = row(&ctx, Item::S9_4);
        assert_eq!(v, Verdict::Pass, "{note}");
        assert!(cap.unwrap().contains("(7 files)"));
        assert!(
            note.contains("MessageDeleted×1")
                && note.contains("ReactionsChanged×1")
                && note.contains(
                    "ThreadUpdate 19:groupgroupgroup@thread.v2: kind keys under properties []"
                ),
            "{note}"
        );
        let (v, cap, note) = row(&ctx, Item::S9_5);
        assert_eq!(v, Verdict::Unverified);
        assert_eq!(cap, None, "no renewal: nothing to cite");
        assert!(note.contains("--minutes 75"), "{note}");
    }

    /// §9.7 evidence: a ConversationUpdate line always shows the read
    /// horizon and the mark-as-unread bookmark, ahead of the truncated
    /// excerpt.
    #[test]
    fn conversation_update_lines_show_the_horizon_and_bookmark() {
        let raw = |props: Value| RawEvent {
            url: "/messaging".into(),
            resource_type: "ConversationUpdate".into(),
            time: None,
            id: None,
            body: json!({"resource": {"id": "19:a@thread.v2", "lastMessage": {"content": "x".repeat(300)},
                                       "properties": props}}),
            mapped: vec!["ReadStateChanged".into(), "ConversationUpdated".into()],
        };
        let line = raw_line(&raw(json!({
            "consumptionhorizon": "1760000000000;1760000001000;c1",
            "consumptionHorizonBookmark": "1760000000000;1760000002000;c1",
            "alerts": "x".repeat(200),
        })));
        assert!(
            line.contains("consumptionhorizon=\"1760000000000;1760000001000;c1\"")
                && line.contains("consumptionHorizonBookmark=\"1760000000000;1760000002000;c1\""),
            "{line}"
        );
        let line = raw_line(&raw(json!({})));
        assert!(line.contains("consumptionHorizonBookmark=absent"), "{line}");
        let mut other = raw(json!({"consumptionHorizonBookmark": "b"}));
        other.resource_type = "MessageUpdate".into();
        assert!(
            !raw_line(&other).contains("consumptionhorizon=absent"),
            "only ConversationUpdate lines get the read-state prefix"
        );
    }

    #[test]
    fn resource_excerpts_lead_with_the_discriminating_fields() {
        let r = json!({"clientmessageid": "1", "composetime": "x", "content": "<p>hi</p>",
                       "messagetype": "RichText/Html", "properties": {"deletetime": "5"}});
        assert_eq!(
            resource_excerpt(&r),
            r#"messagetype="RichText/Html" properties={"deletetime":"5"} content="<p>hi</p>""#
        );
        let long = json!({"content": "a".repeat(300)});
        assert_eq!(
            resource_excerpt(&long).chars().count(),
            RAW_EXCERPT_CHARS + 1
        );
        assert_eq!(resource_excerpt(&json!({"x": 1})), r#"{"x":1}"#);
    }

    #[test]
    fn a_new_skype_expiry_is_a_renewal() {
        assert_eq!(expiry_change(0, 0), None);
        assert_eq!(expiry_change(0, 5), Some(ExpiryChange::First));
        assert_eq!(expiry_change(5, 5), None);
        assert_eq!(expiry_change(5, 0), None, "a dropped token is no renewal");
        assert_eq!(expiry_change(5, 9), Some(ExpiryChange::Renewed));
    }

    #[tokio::test]
    async fn all_runs_every_stage_even_when_each_fails() {
        let s = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        // Nothing is mounted: every request fails. The only question left
        // is "is the chat unread now?" (no).
        let mut ctx = ctx_for(
            &s,
            dir.path(),
            &["smoke", "all"],
            scripted(&[false], Vec::new()),
        )
        .await;
        run_stages(&mut ctx, &Stage::All { minutes: 1 }).await;
        for item in Item::ALL {
            assert!(ctx.report.decided(item), "{item:?} has a verdict");
        }
        for item in [Item::S9_1, Item::S9_3, Item::S9_4, Item::S9_5] {
            let (v, _, note) = row(&ctx, item);
            assert_eq!(v, Verdict::Unverified, "{item:?}: {note}");
            assert!(note.contains("stopped early"), "{note}");
        }
        assert_eq!(
            row(&ctx, Item::S9_8).0,
            Verdict::Fail,
            "write ran after rest failed"
        );
    }

    #[tokio::test]
    async fn a_reauth_stops_the_events_stage_without_verdicts() {
        let s = MockServer::start().await;
        mount_basics(&s).await;
        // Registration meets a 401 and the token refresh it triggers finds
        // the refresh token revoked: trouter ends with ReauthRequired. (A
        // 401 after a successful refresh is only an HTTP error.)
        Mock::given(method("POST"))
            .and(path("/registrar/prod/V2/registrations"))
            .respond_with(ResponseTemplate::new(401))
            .with_priority(1)
            .mount(&s)
            .await;
        Mock::given(method("POST"))
            .and(path("/organizations/oauth2/v2.0/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "at-1", "refresh_token": "rt-1", "expires_in": 3600
            })))
            .up_to_n_times(1)
            .with_priority(1)
            .mount(&s)
            .await;
        Mock::given(method("POST"))
            .and(path("/organizations/oauth2/v2.0/token"))
            .respond_with(
                ResponseTemplate::new(400)
                    .set_body_json(serde_json::json!({"error": "invalid_grant"})),
            )
            .with_priority(2)
            .mount(&s)
            .await;
        mount_trouter(&s).await;
        let (conns_tx, mut conns) = tokio::sync::mpsc::unbounded_channel();
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = ctx_with(
            &s,
            dir.path(),
            &["smoke", "events"],
            scripted(&[], Vec::new()),
            Some(Arc::new(FakeWsFactory { conns: conns_tx })),
        )
        .await;
        let user = async move {
            let ws = conns.recv().await.unwrap();
            ws.send("1::".into()).await.unwrap();
            ws
        };
        let never = std::future::pending::<String>();
        let (listened, _ws) = tokio::join!(events_listen(&mut ctx, 1, never), user);
        let e = listened.unwrap_err();
        assert!(e.to_string().contains("ReauthRequired"), "{e}");
    }

    #[test]
    fn event_script_lists_the_actions() {
        let s = event_script("the chat \"Platform\" (19:x)", 3, false);
        for step in [
            "1) send",
            "2) edit",
            "3) react",
            "4) remove the reaction",
            "5) delete",
            "6) after someone else posts",
            "7) rename",
            "8) mark a chat unread",
        ] {
            assert!(s.contains(step), "{step}: {s}");
        }
        assert!(s.contains("--minutes 75"));
        assert!(s.contains("Start once `Connected` is printed"), "{s}");
        let notes = event_script("your Notes (self) chat", 75, true);
        assert!(!notes.contains("after someone else posts in it"), "{notes}");
        assert!(
            !notes.contains("--minutes"),
            "75 already spans a renewal: {notes}"
        );
    }

    #[test]
    fn capture_files_after_a_mark() {
        let dir = tempfile::tempdir().unwrap();
        for n in [
            "000001-http-registrar.json",
            "000002-ws-in-trouter-request.json",
            "000003-http-registrar.json",
            "000004-ws-in-trouter-request.json",
            "000005-ws-in-trouter-request.json",
        ] {
            std::fs::write(dir.path().join(n), "{}").unwrap();
        }
        let after = captures_after(
            Some(dir.path()),
            Some("000002-ws-in-trouter-request.json"),
            "-ws-in-trouter-request",
        );
        assert_eq!(
            after,
            [
                "000004-ws-in-trouter-request.json",
                "000005-ws-in-trouter-request.json"
            ]
        );
        assert_eq!(
            file_span(&after).unwrap(),
            "000004-ws-in-trouter-request.json … 000005-ws-in-trouter-request.json (2 files)"
        );
        assert_eq!(
            captures_after(Some(dir.path()), None, "-http-registrar").len(),
            2
        );
        assert!(captures_after(None, None, "x").is_empty());
        assert_eq!(file_span(&[]), None);
    }

    // ----- csa stage -----

    const CSA_STATE: &str = include_str!("../tests/fixtures/9.14-csa-user-state/users-me.json");

    fn users_me_path(geo: &str) -> String {
        format!("/api/csa/{geo}/api/v3/teams/users/me")
    }

    /// The CSA grant answer. Mount it before `mount_basics`, whose token
    /// mock answers any scope (the first mounted match wins).
    async fn mount_csa_token(s: &MockServer, status: u16, body: Value) {
        Mock::given(method("POST"))
            .and(path("/organizations/oauth2/v2.0/token"))
            .and(body_string_contains("chatsvcagg"))
            .respond_with(ResponseTemplate::new(status).set_body_json(body))
            .mount(s)
            .await;
    }

    fn csa_ok() -> Value {
        json!({"access_token": "csa-1", "expires_in": 3600})
    }

    #[tokio::test]
    async fn csa_stage_decides_its_three_items() {
        let s = MockServer::start().await;
        mount_csa_token(&s, 200, csa_ok()).await;
        mount_basics(&s).await;
        Mock::given(method("GET"))
            .and(path(users_me_path("emea")))
            .and(query_param("isPrefetch", "false"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(CSA_STATE, "application/json"))
            .expect(1)
            .mount(&s)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = ctx_for(&s, dir.path(), &["smoke", "csa"], scripted(&[], Vec::new())).await;
        run_stages(&mut ctx, &Stage::Csa).await;
        let (v, cap, note) = row(&ctx, Item::S9_12);
        assert_eq!(v, Verdict::Pass, "{note}");
        assert!(cap.unwrap().ends_with("-http-csa-user-state.json"));
        assert!(note.contains("audience chatsvcagg"), "{note}");
        let (v, _, note) = row(&ctx, Item::S9_13);
        assert_eq!(v, Verdict::Pass, "{note}");
        assert!(note.ends_with("geo emea"), "{note}");
        let (v, _, note) = row(&ctx, Item::S9_14);
        assert_eq!(v, Verdict::Pass, "{note}");
        assert!(
            note.ends_with("check the populated-field counts above"),
            "{note}"
        );
    }

    #[tokio::test]
    async fn a_refused_csa_grant_fails_9_12_with_the_oauth_error() {
        let s = MockServer::start().await;
        mount_csa_token(
            &s,
            400,
            json!({"error": "invalid_grant", "error_description": "AADSTS65001: consent required"}),
        )
        .await;
        mount_basics(&s).await;
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = ctx_for(&s, dir.path(), &["smoke", "csa"], scripted(&[], Vec::new())).await;
        run_stages(&mut ctx, &Stage::Csa).await;
        let (v, cap, note) = row(&ctx, Item::S9_12);
        assert_eq!(v, Verdict::Fail);
        assert_eq!(cap, None, "the grant is never captured");
        assert!(
            note.contains("not granted: http 400: invalid_grant: AADSTS65001: consent required"),
            "{note}"
        );
        for item in [Item::S9_13, Item::S9_14] {
            assert_eq!(row(&ctx, item).0, Verdict::Unverified, "{item:?}");
        }
    }

    #[tokio::test]
    async fn csa_names_the_geo_that_answered() {
        let s = MockServer::start().await;
        mount_csa_token(&s, 200, csa_ok()).await;
        mount_basics(&s).await;
        Mock::given(method("GET"))
            .and(path(users_me_path("emea")))
            .respond_with(ResponseTemplate::new(404))
            .mount(&s)
            .await;
        Mock::given(method("GET"))
            .and(path(users_me_path("amer")))
            .respond_with(ResponseTemplate::new(200).set_body_raw(CSA_STATE, "application/json"))
            .mount(&s)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = ctx_for(&s, dir.path(), &["smoke", "csa"], scripted(&[], Vec::new())).await;
        run_stages(&mut ctx, &Stage::Csa).await;
        let (v, _, note) = row(&ctx, Item::S9_13);
        assert_eq!(v, Verdict::Pass, "{note}");
        assert!(note.ends_with("geo amer"), "{note}");
    }

    #[tokio::test]
    async fn csa_401_fails_9_12_and_other_refusals_fail_9_13() {
        for (status, failed, other) in [
            (401, Item::S9_12, Item::S9_13),
            (404, Item::S9_13, Item::S9_12),
        ] {
            let s = MockServer::start().await;
            mount_csa_token(&s, 200, csa_ok()).await;
            mount_basics(&s).await;
            Mock::given(method("GET"))
                .and(wiremock::matchers::path_regex("^/api/csa/"))
                .respond_with(ResponseTemplate::new(status).set_body_string("{\"errorCode\":1}"))
                .mount(&s)
                .await;
            let dir = tempfile::tempdir().unwrap();
            let mut ctx =
                ctx_for(&s, dir.path(), &["smoke", "csa"], scripted(&[], Vec::new())).await;
            run_stages(&mut ctx, &Stage::Csa).await;
            let (v, cap, note) = row(&ctx, failed);
            assert_eq!(v, Verdict::Fail, "{status}: {note}");
            assert!(note.contains(&format!("http {status}")), "{note}");
            assert!(cap.unwrap().ends_with("-http-csa-user-state.json"));
            assert_eq!(row(&ctx, other).0, Verdict::Unverified, "{status}");
            assert_eq!(row(&ctx, Item::S9_14).0, Verdict::Unverified);
        }
    }

    #[tokio::test]
    async fn a_csa_body_that_is_not_json_fails_9_14_only() {
        let s = MockServer::start().await;
        mount_csa_token(&s, 200, csa_ok()).await;
        mount_basics(&s).await;
        Mock::given(method("GET"))
            .and(path(users_me_path("emea")))
            .respond_with(ResponseTemplate::new(200).set_body_string("<html>"))
            .mount(&s)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = ctx_for(&s, dir.path(), &["smoke", "csa"], scripted(&[], Vec::new())).await;
        run_stages(&mut ctx, &Stage::Csa).await;
        let (v, cap, note) = row(&ctx, Item::S9_14);
        assert_eq!(v, Verdict::Fail, "{note}");
        assert!(note.contains("not JSON"), "{note}");
        assert!(cap.unwrap().ends_with("-http-csa-user-state.json"));
        assert_eq!(row(&ctx, Item::S9_12).0, Verdict::Pass);
        assert_eq!(row(&ctx, Item::S9_13).0, Verdict::Unverified);
    }

    #[tokio::test]
    async fn csa_under_spaces_sends_the_access_token_without_a_grant() {
        let s = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/organizations/oauth2/v2.0/token"))
            .and(body_string_contains("chatsvcagg"))
            .respond_with(ResponseTemplate::new(200).set_body_json(csa_ok()))
            .expect(0)
            .mount(&s)
            .await;
        mount_basics(&s).await;
        Mock::given(method("GET"))
            .and(path(users_me_path("emea")))
            .and(wiremock::matchers::header("Authorization", "Bearer at-1"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(CSA_STATE, "application/json"))
            .expect(1)
            .mount(&s)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = ctx_for(
            &s,
            dir.path(),
            &["smoke", "--csa-audience", "spaces", "csa"],
            scripted(&[], Vec::new()),
        )
        .await;
        run_stages(&mut ctx, &Stage::Csa).await;
        let (v, _, note) = row(&ctx, Item::S9_12);
        assert_eq!(v, Verdict::Pass, "{note}");
        assert!(note.contains("audience spaces"), "{note}");
    }

    #[test]
    fn csa_summary_prints_counts_only() {
        let (state, report) =
            ic3_teams::parse_user_state(&serde_json::from_str::<Value>(CSA_STATE).unwrap());
        let lines = csa_summary(&state);
        assert_eq!(
            lines,
            [
                "folders: 4 (Favorites×1, Other(\"BrandNewKind\")×1, QuickViews×1, UserCreated×1)",
                "user-created folders: 1 (items: 2)",
                "chats: 3 (Group×1, Meeting×1, OneOnOne×1)",
                "chats with zero members: 1; members in all: 5",
                "teams: 1; channels: 2",
                "populated: user-created folders named 1/1",
                "populated: teams named 1/1; favorite 1, collapsed 0, archived 0",
                "populated: channels named 2/2; general 1; with channel_type 2/2; pinned 1, muted 1",
                "populated: chats with a title 2/3; with last_message_at 2/3; hidden 1, muted 1",
                "populated: members with a role 5/5; with friendly_name 1/5",
            ]
        );
        let all = lines.join("\n");
        for secret in [
            "Projects",
            "Bea",
            "19:",
            "8:orgid",
            "f-user",
            "Engineering",
            "General",
            "Releases",
            "Platform team",
            "Weekly sync",
            "Admin",
            "Private",
        ] {
            assert!(!all.contains(secret), "{secret}: {all}");
        }
        assert_eq!(report_line(&report), "clean");
        assert_eq!(parse_report_verdict(&report).0, Verdict::Pass);
        let partial = ic3_teams::ParseReport {
            missing_sections: vec!["chats"],
            skipped: vec![("teams", 2)],
        };
        let (v, note) = parse_report_verdict(&partial);
        assert_eq!(v, Verdict::Unverified);
        assert_eq!(
            note,
            "partial parse: missing sections: chats; skipped: teams×2"
        );
    }

    fn csa_fixture() -> (UserState, ic3_teams::ParseReport) {
        ic3_teams::parse_user_state(&serde_json::from_str::<Value>(CSA_STATE).unwrap())
    }

    #[test]
    fn a_fully_populated_user_state_passes_9_14() {
        let (state, report) = csa_fixture();
        let (v, note) = user_state_verdict(&state, &report);
        assert_eq!(v, Verdict::Pass, "{note}");
        assert!(
            note.ends_with("check the populated-field counts above"),
            "{note}"
        );
    }

    #[test]
    fn unnamed_teams_leave_9_14_unverified_despite_a_clean_parse() {
        let (mut state, report) = csa_fixture();
        assert!(report.is_clean());
        for t in &mut state.teams {
            t.name.clear();
        }
        let (v, note) = user_state_verdict(&state, &report);
        assert_eq!(v, Verdict::Unverified, "{note}");
        assert_eq!(note, "guessed field names look wrong: no team has a name");
    }

    #[test]
    fn each_field_check_names_its_failure() {
        type Break = fn(&mut UserState);
        let cases: [(Break, &str); 3] = [
            (
                |s| {
                    s.teams
                        .iter_mut()
                        .flat_map(|t| &mut t.channels)
                        .for_each(|c| c.name.clear())
                },
                "no channel has a name",
            ),
            (
                |s| {
                    s.teams
                        .iter_mut()
                        .flat_map(|t| &mut t.channels)
                        .for_each(|c| c.general = false)
                },
                "no channel is General",
            ),
            (
                |s| s.folders.iter_mut().for_each(|f| f.name = None),
                "no user-created folder has a name",
            ),
        ];
        for (break_it, why) in cases {
            let (mut state, report) = csa_fixture();
            break_it(&mut state);
            let (v, note) = user_state_verdict(&state, &report);
            assert_eq!(v, Verdict::Unverified, "{why}: {note}");
            assert!(note.ends_with(why), "{why}: {note}");
        }
        // Empty sections have nothing to check: the parse report decides.
        let (v, note) =
            user_state_verdict(&UserState::default(), &ic3_teams::ParseReport::default());
        assert_eq!(v, Verdict::Pass, "{note}");
    }

    #[tokio::test]
    async fn typing_is_sent_once_to_the_conv_chat_and_confirmed() {
        let s = MockServer::start().await;
        mount_basics(&s).await;
        Mock::given(method("POST"))
            .and(path(
                "/v1/users/ME/conversations/19:chosen@thread.v2/messages",
            ))
            .and(body_json(json!({
                "content": "", "messagetype": "Control/Typing", "contenttype": "Application/Message"
            })))
            .respond_with(ResponseTemplate::new(201))
            // Ahead of mount_basics' "no writes outside 48:notes" guard.
            .with_priority(1)
            .expect(1)
            .mount(&s)
            .await;
        let ui = scripted(&[true], Vec::new());
        let waited = ui.waited.clone();
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = ctx_for(
            &s,
            dir.path(),
            &[
                "smoke",
                "--conv",
                "19:chosen@thread.v2",
                "write",
                "--typing",
            ],
            ui,
        )
        .await;
        let conv = ConversationId("19:chosen@thread.v2".into());
        write_typing(&mut ctx, &conv, "the chat \"Platform team\"")
            .await
            .unwrap();
        let (v, cap, note) = row(&ctx, Item::S9_18);
        assert_eq!(v, Verdict::Pass, "{note}");
        assert!(cap.unwrap().ends_with("-http-typing.json"));
        assert!(waited.lock().unwrap()[0].contains("another member"));
    }

    #[tokio::test]
    async fn typing_in_notes_is_unverified_and_sends_nothing() {
        let s = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = ctx_for(
            &s,
            dir.path(),
            &["smoke", "write", "--typing"],
            scripted(&[], Vec::new()),
        )
        .await;
        write_typing(
            &mut ctx,
            &ConversationId(NOTES.into()),
            "your Notes (self) chat",
        )
        .await
        .unwrap();
        let (v, _, note) = row(&ctx, Item::S9_18);
        assert_eq!(v, Verdict::Unverified);
        assert!(note.contains("--conv"), "{note}");
        assert!(s.received_requests().await.unwrap().is_empty());
    }
}
