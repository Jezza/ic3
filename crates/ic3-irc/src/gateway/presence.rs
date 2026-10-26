//! Teams presence on IRC: people who are not available are away
//! (`away-notify`, `WHO`'s `G` flag, `WHOIS` 301) with the reason as text
//! ("In a meeting", "Offline", …), and `MONITOR` reports Teams-offline people
//! as offline.
//!
//! Followed: 1:1 partners, group-chat members and anyone a client
//! monitors. Their presence is fetched and subscribed in batches (pushed
//! changes arrive as [`Event::Presence`](ic3_teams::Event::Presence)), afresh after
//! every reconnect of the push connection.

use std::collections::{BTreeSet, HashSet};
use std::time::Duration;

use ic3_teams::{ConversationId, ConversationKind, Mri, Presence};
use tokio::time::Instant;

use super::{ClientId, Gateway, OpFn};
use crate::irc::{Message, casefold};

/// Most people asked about (and subscribed) per batch.
const BATCH: usize = 200;
/// How long to wait after a failed batch.
const RETRY: Duration = Duration::from_secs(30);
/// Most nicks one client may monitor.
pub(crate) const MONITOR_MAX: usize = 1000;

/// Presence bookkeeping.
#[derive(Debug, Default)]
pub(crate) struct PresenceState {
    /// The latest known presence of each person.
    known: std::collections::HashMap<Mri, Presence>,
    /// Subscribed on the current push connection.
    subscribed: HashSet<Mri>,
    /// Waiting to be fetched and subscribed.
    wanted: BTreeSet<Mri>,
    in_flight: bool,
    /// The next batch drops earlier subscriptions first (after a reconnect).
    purge: bool,
    retry_at: Option<Instant>,
    /// Per client, the folded nicks it monitors.
    pub(crate) monitors: std::collections::HashMap<ClientId, BTreeSet<String>>,
}

/// The away text for a presence; `None` when they are around.
pub(crate) fn away_text(p: &Presence) -> Option<String> {
    let mut text = match (p.availability.as_str(), p.activity.as_str()) {
        ("Available" | "AvailableIdle" | "PresenceUnknown" | "", _) => return None,
        (_, "InAMeeting") => "In a meeting".to_string(),
        (_, "InACall" | "InAConferenceCall") => "In a call".to_string(),
        (_, "Presenting") => "Presenting".to_string(),
        (_, "OffWork") => "Off work".to_string(),
        ("Offline", _) => "Offline".to_string(),
        ("Away", _) => "Away".to_string(),
        ("BeRightBack", _) => "Be right back".to_string(),
        ("Busy" | "BusyIdle", _) => "Busy".to_string(),
        ("DoNotDisturb", _) => "Do not disturb".to_string(),
        (other, _) => other.to_string(),
    };
    if p.out_of_office {
        text.push_str(" (out of office)");
    }
    Some(text)
}

/// Whether `MONITOR` counts them as online: anything but Teams-offline.
fn online(p: Option<&Presence>) -> bool {
    p.is_some_and(|p| !matches!(p.availability.as_str(), "Offline" | "PresenceUnknown"))
}

impl Gateway {
    /// The away text of a Teams user; `None` when around or unknown.
    pub(crate) fn away_of(&self, mri: &Mri) -> Option<String> {
        self.presence.known.get(mri).and_then(away_text)
    }

    /// The away text behind a nick.
    pub(crate) fn away_of_nick(&self, nick: &str) -> Option<String> {
        self.dir.user_by_nick(nick).and_then(|m| self.away_of(m))
    }

    /// Everyone whose presence is followed: 1:1 partners, group-chat
    /// members and monitored nicks.
    fn presence_targets(&self) -> BTreeSet<Mri> {
        let mut out: BTreeSet<Mri> = self.one_to_ones().into_iter().map(|(_, p)| p).collect();
        for sc in self.store.conversations() {
            if sc.conversation.kind != ConversationKind::Group {
                continue;
            }
            out.extend(
                sc.conversation
                    .members
                    .iter()
                    .flatten()
                    .map(|m| m.mri.clone()),
            );
        }
        for nicks in self.presence.monitors.values() {
            out.extend(
                nicks
                    .iter()
                    .filter_map(|n| self.dir.user_by_nick(n).cloned()),
            );
        }
        if let Some(me) = self.store.me() {
            out.remove(me);
        }
        out
    }

    /// The push connection (re)registered: subscribe everyone afresh.
    pub(crate) fn presence_reconnected(&mut self) {
        self.presence.subscribed.clear();
        self.presence.purge = true;
        self.presence.retry_at = None;
        self.presence.wanted = self.presence_targets();
        self.pump_presence();
    }

    /// Sync finished: follow everyone known now.
    pub(crate) fn presence_ready(&mut self) {
        let targets = self.presence_targets();
        for m in targets {
            self.follow_presence(&m);
        }
        self.pump_presence();
    }

    /// Follows `mri` too (someone new: a message, a monitor).
    pub(crate) fn follow_presence(&mut self, mri: &Mri) {
        if self.store.me() == Some(mri) || self.presence.subscribed.contains(mri) {
            return;
        }
        self.presence.wanted.insert(mri.clone());
    }

    /// Fetches and subscribes the next batch, one batch at a time.
    pub(crate) fn pump_presence(&mut self) {
        if self.presence.in_flight
            || self.presence.wanted.is_empty()
            || self.session.phase != super::Phase::Ready
            || self.presence.retry_at.is_some_and(|t| Instant::now() < t)
        {
            return;
        }
        let batch: Vec<Mri> = self.presence.wanted.iter().take(BATCH).cloned().collect();
        for m in &batch {
            self.presence.wanted.remove(m);
        }
        let purge = std::mem::take(&mut self.presence.purge);
        self.presence.in_flight = true;
        self.spawn(|b| async move {
            let fetched = b.presence(&batch).await;
            let subscribed = b.subscribe_presence(&batch, &[], purge).await;
            Box::new(move |gw: &mut Gateway| {
                gw.on_presence_batch(batch, purge, fetched, subscribed)
            }) as OpFn
        });
    }

    fn on_presence_batch(
        &mut self,
        batch: Vec<Mri>,
        purge: bool,
        fetched: ic3_teams::Result<Vec<Presence>>,
        subscribed: ic3_teams::Result<()>,
    ) {
        self.presence.in_flight = false;
        match fetched {
            Ok(list) => {
                tracing::debug!(n = list.len(), "presence fetched");
                self.apply_presence(list);
            }
            Err(e) => tracing::warn!(error = %e, n = batch.len(), "fetching presence failed"),
        }
        match subscribed {
            Ok(()) => {
                tracing::debug!(n = batch.len(), purge, "subscribed to presence");
                self.presence.subscribed.extend(batch);
            }
            Err(e) => {
                tracing::warn!(error = %e, n = batch.len(), "subscribing to presence failed; retrying later");
                self.presence.wanted.extend(batch);
                self.presence.purge |= purge;
                self.presence.retry_at = Some(Instant::now() + RETRY);
            }
        }
        self.pump_presence();
    }

    /// New presence for some people: tells clients what changed.
    pub(crate) fn apply_presence(&mut self, list: Vec<Presence>) {
        for p in list {
            let mri = p.mri.clone();
            if self.store.me() == Some(&mri) {
                continue;
            }
            let (was_away, was_online) =
                (self.away_of(&mri), online(self.presence.known.get(&mri)));
            self.presence.known.insert(mri.clone(), p);
            let (away, is_online) = (self.away_of(&mri), online(self.presence.known.get(&mri)));
            let Some(nick) = self.dir.existing_nick(&mri).map(str::to_owned) else {
                continue; // not shown anywhere yet
            };
            if away != was_away {
                let m = Message::new("AWAY", away.into_iter().collect::<Vec<_>>())
                    .with_source(Gateway::user_mask(&nick));
                self.broadcast(&m);
            }
            if is_online != was_online {
                self.monitor_notify(&nick, is_online);
            }
        }
    }

    /// `730` / `731` to every client monitoring `nick`.
    fn monitor_notify(&mut self, nick: &str, is_online: bool) {
        let folded = casefold(nick);
        let watchers: Vec<ClientId> = self
            .presence
            .monitors
            .iter()
            .filter(|(_, ns)| ns.contains(&folded))
            .map(|(id, _)| *id)
            .collect();
        for id in watchers {
            let m = self.monitor_reply(is_online, &[nick.to_string()]);
            self.send_to(id, m);
        }
    }

    fn monitor_reply(&self, is_online: bool, nicks: &[String]) -> Message {
        let list = nicks
            .iter()
            .map(|n| {
                if is_online {
                    Gateway::user_mask(n)
                } else {
                    n.clone()
                }
            })
            .collect::<Vec<_>>()
            .join(",");
        Message::new(
            if is_online { "730" } else { "731" },
            ["*".to_string(), list],
        )
        .with_source(self.cfg.server_name.clone())
    }

    /// `MONITOR +|-|C|L|S [targets]`.
    pub(crate) fn monitor(&mut self, id: ClientId, label: Option<String>, msg: &Message) {
        let targets: Vec<String> = msg
            .param(1)
            .split(',')
            .filter(|t| !t.is_empty())
            .map(str::to_owned)
            .collect();
        let mut out = Vec::new();
        match msg.param(0) {
            "+" => {
                let used = self.presence.monitors.get(&id).map_or(0, BTreeSet::len);
                let room = MONITOR_MAX.saturating_sub(used);
                if targets.len() > room {
                    out.push(self.numeric(
                        "734",
                        [
                            MONITOR_MAX.to_string(),
                            targets.join(","),
                            "Monitor list is full.".into(),
                        ],
                    ));
                }
                let added: Vec<String> = targets.into_iter().take(room).collect();
                self.presence
                    .monitors
                    .entry(id)
                    .or_default()
                    .extend(added.iter().map(|t| casefold(t)));
                let (mut on, mut off) = (Vec::new(), Vec::new());
                for nick in added {
                    match self.dir.user_by_nick(&nick).cloned() {
                        Some(mri) => {
                            self.follow_presence(&mri);
                            if online(self.presence.known.get(&mri)) {
                                on.push(nick);
                            } else {
                                off.push(nick);
                            }
                        }
                        None => off.push(nick),
                    }
                }
                if !on.is_empty() {
                    out.push(self.monitor_reply(true, &on));
                }
                if !off.is_empty() {
                    out.push(self.monitor_reply(false, &off));
                }
                self.pump_presence();
            }
            "-" => {
                if let Some(set) = self.presence.monitors.get_mut(&id) {
                    for t in &targets {
                        set.remove(&casefold(t));
                    }
                }
            }
            "C" | "c" => {
                self.presence.monitors.remove(&id);
            }
            "L" | "l" => {
                let list: Vec<String> = self
                    .presence
                    .monitors
                    .get(&id)
                    .map(|s| s.iter().cloned().collect())
                    .unwrap_or_default();
                for chunk in list.chunks(40) {
                    out.push(self.numeric("732", [chunk.join(",")]));
                }
                out.push(self.numeric("733", ["End of MONITOR list".to_string()]));
            }
            "S" | "s" => {
                let list: Vec<String> = self
                    .presence
                    .monitors
                    .get(&id)
                    .map(|s| s.iter().cloned().collect())
                    .unwrap_or_default();
                let (on, off): (Vec<String>, Vec<String>) = list.into_iter().partition(|n| {
                    self.dir
                        .user_by_nick(n)
                        .is_some_and(|m| online(self.presence.known.get(m)))
                });
                if !on.is_empty() {
                    out.push(self.monitor_reply(true, &on));
                }
                if !off.is_empty() {
                    out.push(self.monitor_reply(false, &off));
                }
            }
            _ => {}
        }
        self.reply(id, label.as_deref(), out);
    }

    /// `AWAY` lines for the away members of a channel, for a client that
    /// just joined it.
    pub(crate) fn away_burst(&mut self, id: ClientId, conv: &ConversationId) {
        if !self.caps(id).is_some_and(|c| c.has("away-notify")) || self.presence.known.is_empty() {
            return;
        }
        for nick in self.member_nicks(conv) {
            if let Some(text) = self.away_of_nick(&nick) {
                let m = Message::new("AWAY", [text]).with_source(Gateway::user_mask(&nick));
                self.send_to(id, m);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(availability: &str, activity: &str, ooo: bool) -> Presence {
        Presence {
            mri: Mri("8:orgid:a".into()),
            availability: availability.into(),
            activity: activity.into(),
            last_active: None,
            out_of_office: ooo,
        }
    }

    #[test]
    fn away_texts() {
        assert_eq!(away_text(&p("Available", "Available", false)), None);
        assert_eq!(away_text(&p("AvailableIdle", "Inactive", false)), None);
        assert_eq!(
            away_text(&p("Busy", "InAMeeting", false)).as_deref(),
            Some("In a meeting")
        );
        assert_eq!(
            away_text(&p("Busy", "Busy", false)).as_deref(),
            Some("Busy")
        );
        assert_eq!(
            away_text(&p("Away", "Away", true)).as_deref(),
            Some("Away (out of office)")
        );
        assert_eq!(
            away_text(&p("Offline", "OffWork", false)).as_deref(),
            Some("Off work")
        );
        assert_eq!(
            away_text(&p("Offline", "Offline", false)).as_deref(),
            Some("Offline")
        );
        assert!(online(Some(&p("Away", "Away", false))));
        assert!(!online(Some(&p("Offline", "Offline", false))));
        assert!(!online(None));
    }
}
