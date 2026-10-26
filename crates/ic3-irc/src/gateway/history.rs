//! `CHATHISTORY` from Teams: an in-memory window per conversation that
//! answers what is already known, and an adaptive limit on how many fetches
//! run at once.
//!
//! **The window.** After a fetch of a conversation's latest page, the store
//! holds every message from the page's oldest to now, and live events keep
//! it that way; a page fetched just before the window's start extends it.
//! History inside the window is answered from the store. A dropped push
//! connection may have lost events, so it forgets every window.
//!
//! **The limit.** Like TCP's congestion control: each fetch's time is
//! measured against the fastest of the recent ones (the connection when it
//! is not busy). A fast fetch raises the limit a little, a slow one lowers
//! it, a timeout or network error halves it. A new fetch also waits while a
//! running one is far slower than that baseline, so a struggling connection
//! is not loaded further.

use std::collections::{HashMap, VecDeque};
use std::time::Duration;

use ic3_teams::{ConversationId, Message as TeamsMessage, MessageKey, PageCursor, Timestamp};
use tokio::time::Instant;

use super::{ClientId, Gateway, OpFn};

/// Which messages a `CHATHISTORY` request wants.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Query {
    Latest(Option<Timestamp>),
    Before(Timestamp),
    After(Timestamp),
    Around(Timestamp),
    Between(Timestamp, Timestamp),
}

/// What a fetch returned.
#[derive(Debug)]
pub(crate) struct Fetched {
    /// Oldest first, at most the request's limit.
    pub(crate) messages: Vec<TeamsMessage>,
    /// The oldest message the fetched page held (before trimming to the
    /// limit), when it held any.
    pub(crate) page_oldest: Option<Timestamp>,
    /// The page was short: there is nothing older.
    pub(crate) reached_start: bool,
}

/// A `CHATHISTORY` request waiting for (or in) a fetch.
#[derive(Debug)]
pub(crate) struct HistoryJob {
    pub(crate) client: ClientId,
    pub(crate) label: Option<String>,
    pub(crate) target: String,
    pub(crate) sub: String,
    pub(crate) conv: ConversationId,
    pub(crate) query: Query,
    pub(crate) limit: usize,
}

const START_LIMIT: f64 = 4.0;
const MIN_LIMIT: f64 = 1.0;
const MAX_LIMIT: f64 = 16.0;
/// Recent fetch times kept for the baseline.
const SAMPLES: usize = 32;
/// A running fetch slower than this many baselines holds new ones back.
const STALL_FACTOR: u32 = 3;
/// …but never below this, so normal jitter does not.
const STALL_FLOOR: Duration = Duration::from_secs(2);

/// How many history fetches may run at once.
#[derive(Debug)]
pub(crate) struct Limiter {
    limit: f64,
    in_flight: HashMap<u64, Instant>,
    next: u64,
    samples: VecDeque<Duration>,
}

impl Default for Limiter {
    fn default() -> Self {
        Limiter {
            limit: START_LIMIT,
            in_flight: HashMap::new(),
            next: 0,
            samples: VecDeque::new(),
        }
    }
}

/// How a fetch ended, for the limiter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    Done,
    /// A timeout, a network error or a 429: the connection is overloaded.
    Congested,
    /// Any other failure: says nothing about the network.
    Failed,
}

impl Limiter {
    /// The fastest recent fetch: the connection when it is not busy.
    fn baseline(&self) -> Option<Duration> {
        self.samples.iter().min().copied()
    }

    /// Whether a running fetch is far slower than the baseline.
    fn stalled(&self, now: Instant) -> bool {
        let Some(base) = self.baseline() else {
            return false;
        };
        let slow = (base * STALL_FACTOR).max(STALL_FLOOR);
        self.in_flight
            .values()
            .any(|t| now.duration_since(*t) > slow)
    }

    /// Whether another fetch may start now.
    pub(crate) fn can_start(&self, now: Instant) -> bool {
        (self.in_flight.len() as f64) < self.limit.floor() && !self.stalled(now)
    }

    pub(crate) fn start(&mut self, now: Instant) -> u64 {
        self.next += 1;
        self.in_flight.insert(self.next, now);
        self.next
    }

    /// A fetch ended: adjusts the limit by how it went.
    pub(crate) fn finish(&mut self, ticket: u64, now: Instant, outcome: Outcome) {
        let Some(started) = self.in_flight.remove(&ticket) else {
            return;
        };
        let took = now.duration_since(started);
        match outcome {
            Outcome::Congested => self.limit = (self.limit / 2.0).max(MIN_LIMIT),
            Outcome::Failed => {}
            Outcome::Done => {
                let base = self.baseline().unwrap_or(took);
                self.samples.push_back(took);
                if self.samples.len() > SAMPLES {
                    self.samples.pop_front();
                }
                if took <= base * 2 {
                    // About one more per round of fetches.
                    self.limit = (self.limit + 1.0 / self.limit).min(MAX_LIMIT);
                } else if took > base * 4 {
                    self.limit = (self.limit * 0.75).max(MIN_LIMIT);
                }
            }
        }
    }

    pub(crate) fn running(&self) -> usize {
        self.in_flight.len()
    }

    pub(crate) fn limit(&self) -> usize {
        self.limit.floor() as usize
    }
}

impl Gateway {
    /// Answers a `CHATHISTORY` request: from the window when it covers it,
    /// else through a fetch, queued behind the limit.
    pub(crate) fn history(&mut self, job: HistoryJob) {
        if let Some(messages) = self.answer_from_window(&job.conv, job.query, job.limit) {
            tracing::debug!(conversation = %job.conv, n = messages.len(), "history from memory");
            return self.on_history(job.client, job.label, job.target, job.sub, Ok(messages));
        }
        self.history_queue.push_back(job);
        self.pump_history();
    }

    /// Starts queued fetches while the limit allows.
    pub(crate) fn pump_history(&mut self) {
        let now = Instant::now();
        while self.limiter.can_start(now) {
            let Some(job) = self.history_queue.pop_front() else {
                return;
            };
            if !self.clients.contains_key(&job.client) {
                continue; // the client left meanwhile
            }
            let ticket = self.limiter.start(now);
            let page = self.cfg.page_size.max(job.limit);
            let (conv, query, limit) = (job.conv.clone(), job.query, job.limit);
            self.spawn(|b| async move {
                let r = fetch_history(b.as_ref(), &conv, query, limit, page).await;
                Box::new(move |gw: &mut Gateway| gw.on_fetched(ticket, job, r)) as OpFn
            });
        }
        if !self.history_queue.is_empty() {
            tracing::debug!(
                waiting = self.history_queue.len(),
                running = self.limiter.running(),
                limit = self.limiter.limit(),
                "history fetches held back"
            );
        }
    }

    fn on_fetched(&mut self, ticket: u64, job: HistoryJob, r: ic3_teams::Result<Fetched>) {
        let outcome = match &r {
            Ok(_) => Outcome::Done,
            Err(ic3_teams::Error::Network(_)) | Err(ic3_teams::Error::Http { status: 429, .. }) => {
                Outcome::Congested
            }
            Err(_) => Outcome::Failed,
        };
        self.limiter.finish(ticket, Instant::now(), outcome);
        let r = r.map(|f| {
            self.extend_window(&job.conv, job.query, &f);
            f.messages
        });
        self.on_history(job.client, job.label, job.target, job.sub, r);
        self.pump_history();
    }

    // ----- The window -----

    /// Grows `conv`'s window by what a fetch showed.
    fn extend_window(&mut self, conv: &ConversationId, query: Query, f: &Fetched) {
        let start = if f.reached_start {
            Some(0)
        } else {
            f.page_oldest
        };
        let Some(start) = start else {
            return;
        };
        match query {
            // The latest page reaches to now: the window is its span.
            Query::Latest(None) => {
                let w = self.history_windows.entry(conv.clone()).or_insert(start);
                *w = (*w).min(start);
            }
            // A page just before the window joins it.
            Query::Before(t) => {
                if let Some(w) = self.history_windows.get_mut(conv)
                    && t >= *w
                {
                    *w = (*w).min(start);
                }
            }
            _ => {}
        }
    }

    /// Forgets every window: events may have been lost.
    pub(crate) fn forget_windows(&mut self) {
        if !self.history_windows.is_empty() {
            tracing::debug!(n = self.history_windows.len(), "history windows forgotten");
        }
        self.history_windows.clear();
    }

    /// The answer to `query` from the store, when the window covers it.
    fn answer_from_window(
        &self,
        conv: &ConversationId,
        query: Query,
        limit: usize,
    ) -> Option<Vec<TeamsMessage>> {
        let start = *self.history_windows.get(conv)?;
        // Stored messages from the window's start, oldest first.
        let known: Vec<&TeamsMessage> = self
            .store
            .messages(conv)
            .iter()
            .filter(|sm| matches!(sm.key, MessageKey::Server(_)))
            .map(|sm| &sm.message)
            .filter(|m| m.arrived_at >= start)
            .collect();
        let last = |ms: Vec<&TeamsMessage>, n: usize| -> Vec<TeamsMessage> {
            let skip = ms.len().saturating_sub(n);
            ms.into_iter().skip(skip).cloned().collect()
        };
        let first = |ms: Vec<&TeamsMessage>, n: usize| -> Vec<TeamsMessage> {
            ms.into_iter().take(n).cloned().collect()
        };
        // Enough before `t`, or the window reaches the very first message.
        let enough_before = |t: Timestamp, n: usize| {
            start == 0 || known.iter().filter(|m| m.arrived_at < t).count() >= n
        };
        match query {
            Query::Latest(None) if enough_before(Timestamp::MAX, limit) => Some(last(known, limit)),
            Query::Latest(Some(t)) if t >= start => Some(last(
                known.into_iter().filter(|m| m.arrived_at > t).collect(),
                limit,
            )),
            Query::Before(t) if enough_before(t, limit) => Some(last(
                known.into_iter().filter(|m| m.arrived_at < t).collect(),
                limit,
            )),
            Query::After(t) if t >= start => Some(first(
                known.into_iter().filter(|m| m.arrived_at > t).collect(),
                limit,
            )),
            Query::Between(a, b) if a.min(b) >= start => {
                let (lo, hi) = (a.min(b), a.max(b));
                let inside: Vec<&TeamsMessage> = known
                    .into_iter()
                    .filter(|m| m.arrived_at > lo && m.arrived_at < hi)
                    .collect();
                Some(if a <= b {
                    first(inside, limit)
                } else {
                    last(inside, limit)
                })
            }
            Query::Around(t) if enough_before(t, limit / 2) => {
                let before = last(
                    known.iter().copied().filter(|m| m.arrived_at < t).collect(),
                    limit / 2,
                );
                let rest = limit - before.len();
                let mut out = before;
                out.extend(first(
                    known.into_iter().filter(|m| m.arrived_at >= t).collect(),
                    rest,
                ));
                Some(out)
            }
            _ => None,
        }
    }
}

/// Fetches the messages `query` asks for, oldest first, at most `limit`.
pub(crate) async fn fetch_history(
    b: &dyn crate::backend::Backend,
    conv: &ConversationId,
    query: Query,
    limit: usize,
    page: usize,
) -> ic3_teams::Result<Fetched> {
    let page_n = page.max(limit);
    // One page ending before `t` (or the latest), oldest first, with what
    // the page showed about older messages.
    let page_before = |t: Option<Timestamp>| async move {
        let cursor = t.map(|t| PageCursor::end_time(t - 1));
        let p = b.messages(conv, cursor, page_n).await?;
        let reached_start = p.items.len() < page_n && p.next.is_none();
        let mut ms: Vec<TeamsMessage> = p
            .items
            .into_iter()
            .filter(|m| t.is_none_or(|t| m.arrived_at < t))
            .collect();
        ms.sort_by_key(|m| m.arrived_at);
        let page_oldest = ms.first().map(|m| m.arrived_at);
        Ok::<_, ic3_teams::Error>((ms, page_oldest, reached_start))
    };
    let after = |t: Timestamp| async move {
        let mut ms: Vec<TeamsMessage> = b
            .messages_since(conv, t + 1)
            .await?
            .into_iter()
            .filter(|m| m.arrived_at > t)
            .collect();
        ms.sort_by_key(|m| m.arrived_at);
        Ok::<_, ic3_teams::Error>(ms)
    };
    let keep_last = |ms: Vec<TeamsMessage>, n: usize| -> Vec<TeamsMessage> {
        let skip = ms.len().saturating_sub(n);
        ms.into_iter().skip(skip).collect()
    };
    let mut out = Fetched {
        messages: Vec::new(),
        page_oldest: None,
        reached_start: false,
    };
    match query {
        Query::Latest(since) => {
            let (ms, oldest, start) = page_before(None).await?;
            out.page_oldest = oldest;
            out.reached_start = start;
            let ms = ms
                .into_iter()
                .filter(|m| since.is_none_or(|t| m.arrived_at > t))
                .collect();
            out.messages = keep_last(ms, limit);
        }
        Query::Before(t) => {
            let (ms, oldest, start) = page_before(Some(t)).await?;
            out.page_oldest = oldest;
            out.reached_start = start;
            out.messages = keep_last(ms, limit);
        }
        Query::After(t) => {
            let mut ms = after(t).await?;
            ms.truncate(limit);
            out.messages = ms;
        }
        Query::Around(t) => {
            let (ms, _, _) = page_before(Some(t)).await?;
            let mut ms = keep_last(ms, limit / 2);
            let rest = limit - ms.len();
            let mut later = after(t - 1).await?;
            later.truncate(rest);
            ms.extend(later);
            out.messages = ms;
        }
        Query::Between(a, b2) if a <= b2 => {
            let mut ms = after(a).await?;
            ms.retain(|m| m.arrived_at < b2);
            ms.truncate(limit);
            out.messages = ms;
        }
        Query::Between(a, b2) => {
            let (ms, _, _) = page_before(Some(a)).await?;
            let ms = ms.into_iter().filter(|m| m.arrived_at > b2).collect();
            out.messages = keep_last(ms, limit);
        }
    }
    out.messages.dedup_by(|x, y| x.id == y.id);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[tokio::test(start_paused = true)]
    async fn fast_fetches_raise_the_limit_and_slow_ones_lower_it() {
        let mut l = Limiter::default();
        let t0 = Instant::now();
        assert_eq!(l.limit(), 4);
        // Steady fast fetches: the limit climbs to the maximum.
        for i in 0..200 {
            let s = l.start(t0 + ms(i * 300));
            l.finish(s, t0 + ms(i * 300 + 200), Outcome::Done);
        }
        assert_eq!(l.limit(), 16);
        // Much slower than the baseline: it backs off.
        let s = l.start(t0 + ms(100_000));
        l.finish(s, t0 + ms(103_000), Outcome::Done);
        assert_eq!(l.limit(), 12);
        // A timeout halves it; it never drops below one.
        for _ in 0..10 {
            let s = l.start(t0 + ms(200_000));
            l.finish(s, t0 + ms(230_000), Outcome::Congested);
        }
        assert_eq!(l.limit(), 1);
        // Other failures say nothing about the network.
        let s = l.start(t0);
        l.finish(s, t0 + ms(1), Outcome::Failed);
        assert_eq!(l.limit(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_stalled_fetch_holds_new_ones_back() {
        let mut l = Limiter::default();
        let t0 = Instant::now();
        // A baseline of 200 ms.
        let s = l.start(t0);
        l.finish(s, t0 + ms(200), Outcome::Done);
        let _running = l.start(t0 + ms(1_000));
        assert!(l.can_start(t0 + ms(2_500)), "1.5 s: under the 2 s floor");
        assert!(!l.can_start(t0 + ms(3_500)), "2.5 s: stalled");
        // At the limit, nothing more starts either.
        let mut l = Limiter::default();
        for _ in 0..4 {
            l.start(t0);
        }
        assert!(!l.can_start(t0));
    }
}
