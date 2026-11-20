use std::collections::HashMap;
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::widgets::TableState;

use crate::clipboard::Copied;
use crate::collector::{Msg, Req};
use crate::model::{
    ExplainTarget, OpenTransaction, Plan, PlanKey, RunningQuery, Sample, SlowQuery,
};

/// Samples kept for the graphs: ten minutes at one poll a second.
const HISTORY: usize = 600;
const PAGE: isize = 10;
/// How long a notice stays on the screen.
const NOTICE_FOR: Duration = Duration::from_secs(3);

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Running,
    Slow,
    Transactions,
}

/// What the slow list is ranked by, the largest first. "Slow" means different things: the
/// statement that costs the most in all, the one that is slowest each time, one that is run
/// too often, or one that reads far too many rows.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SlowSort {
    Total,
    Average,
    Max,
    Calls,
    Rows,
}

impl SlowSort {
    fn next(self) -> SlowSort {
        match self {
            SlowSort::Total => SlowSort::Average,
            SlowSort::Average => SlowSort::Max,
            SlowSort::Max => SlowSort::Calls,
            SlowSort::Calls => SlowSort::Rows,
            SlowSort::Rows => SlowSort::Total,
        }
    }

    /// The name of its column.
    pub fn column(self) -> &'static str {
        match self {
            SlowSort::Total => "TOTAL",
            SlowSort::Average => "AVG",
            SlowSort::Max => "MAX",
            SlowSort::Calls => "CALLS",
            SlowSort::Rows => "ROWS EXAM",
        }
    }
}

fn sort_slow(list: &mut [SlowQuery], by: SlowSort) {
    list.sort_by(|a, b| {
        let rows = |q: &SlowQuery| q.rows_examined_avg.unwrap_or(-1.0);
        match by {
            SlowSort::Total => b.total_ms.total_cmp(&a.total_ms),
            SlowSort::Average => b.avg_ms.total_cmp(&a.avg_ms),
            SlowSort::Max => b.max_ms.total_cmp(&a.max_ms),
            SlowSort::Calls => b.calls.cmp(&a.calls),
            SlowSort::Rows => rows(b).total_cmp(&rows(a)),
        }
        // Whatever ties go by what they cost in all.
        .then(b.total_ms.total_cmp(&a.total_ms))
    });
}

impl Tab {
    fn next(self) -> Tab {
        match self {
            Tab::Running => Tab::Slow,
            Tab::Slow => Tab::Transactions,
            Tab::Transactions => Tab::Running,
        }
    }

    fn previous(self) -> Tab {
        match self {
            Tab::Running => Tab::Transactions,
            Tab::Slow => Tab::Running,
            Tab::Transactions => Tab::Slow,
        }
    }
}

/// Narrows every list to the rows that hold what was typed after `/`.
#[derive(Default)]
pub struct Filter {
    pub text: String,
    /// Keys go into `text` instead of acting.
    pub typing: bool,
}

impl Filter {
    pub fn is_on(&self) -> bool {
        !self.text.trim().is_empty()
    }

    /// The rows that hold every word of the filter, whatever the case.
    fn apply<'a, T>(&self, rows: &'a [T], text: impl Fn(&T) -> String) -> Vec<&'a T> {
        let words: Vec<String> = self
            .text
            .split_whitespace()
            .map(str::to_lowercase)
            .collect();
        rows.iter()
            .filter(|row| {
                words.is_empty() || {
                    let text = text(row).to_lowercase();
                    words.iter().all(|word| text.contains(word.as_str()))
                }
            })
            .collect()
    }

    pub fn running<'a>(&self, sample: &'a Sample) -> Vec<&'a RunningQuery> {
        self.apply(&sample.running, |q| {
            format!(
                "{} {} {} {} {} {}",
                q.id, q.user, q.host, q.db, q.state, q.sql
            )
        })
    }

    pub fn slow<'a>(&self, sample: &'a Sample) -> Vec<&'a SlowQuery> {
        self.apply(&sample.slow, |q| {
            format!("{} {} {}", q.digest, q.schema, q.sample_sql)
        })
    }

    pub fn transactions<'a>(&self, sample: &'a Sample) -> Vec<&'a OpenTransaction> {
        self.apply(&sample.transactions, |t| {
            format!(
                "{} {} {} {} {} {}",
                t.id, t.user, t.host, t.db, t.state, t.sql
            )
        })
    }
}

/// A table's cursor. Rows reorder on every poll, so it follows the selected row by key
/// instead of by position.
#[derive(Default)]
pub struct Selection {
    pub state: TableState,
    key: Option<String>,
}

impl Selection {
    fn sync(&mut self, keys: &[String]) {
        if keys.is_empty() {
            self.state.select(None);
            self.key = None;
            return;
        }
        let at = self
            .key
            .as_ref()
            .and_then(|k| keys.iter().position(|x| x == k))
            // The row is gone: stay at the same position rather than jump to the top.
            .or(self.state.selected().map(|i| i.min(keys.len() - 1)))
            .unwrap_or(0);
        self.state.select(Some(at));
        self.key = Some(keys[at].clone());
    }

    fn move_by(&mut self, delta: isize, keys: &[String]) {
        if keys.is_empty() {
            return;
        }
        let from = self.state.selected().unwrap_or(0) as isize;
        let at = (from + delta).clamp(0, keys.len() as isize - 1) as usize;
        self.state.select(Some(at));
        self.key = Some(keys[at].clone());
    }
}

pub enum Subject {
    /// `finished` is set once the statement is no longer what the connection is running.
    Running {
        query: RunningQuery,
        finished: bool,
    },
    Slow(SlowQuery),
    /// `blocking` are the sessions waiting for it. `finished` is set once the transaction has
    /// ended, or the session has begun another.
    Transaction {
        trx: OpenTransaction,
        blocking: Vec<u64>,
        finished: bool,
    },
}

impl Subject {
    /// The statement, and whether the server cut it short.
    fn sql_of(&self) -> (String, bool) {
        match self {
            Subject::Running { query, .. } => (query.sql.clone(), query.truncated),
            Subject::Slow(slow) => (slow.sample_sql.clone(), slow.truncated),
            Subject::Transaction { trx, .. } => (trx.sql.clone(), trx.truncated),
        }
    }
}

pub enum PlanState {
    Loading,
    Ready(Plan),
    Failed(String),
    /// Nothing to explain: a transaction is not a statement.
    NotAsked,
}

pub struct Detail {
    pub subject: Subject,
    pub plan: PlanState,
    pub scroll: usize,
}

impl Detail {
    fn key(&self) -> PlanKey {
        match &self.subject {
            Subject::Running { query, .. } => PlanKey::Running(query.id),
            Subject::Slow(slow) => PlanKey::Digest(slow.digest.clone()),
            Subject::Transaction { trx, .. } => PlanKey::Transaction(trx.id),
        }
    }

    /// Follows the live query while it runs, then freezes it.
    fn refresh(&mut self, sample: &Sample) {
        match &mut self.subject {
            Subject::Running { query, finished } => {
                if *finished {
                    return;
                }
                // A connection id outlives its statements, so the same text has to still be
                // running.
                match sample
                    .running
                    .iter()
                    .find(|q| q.id == query.id && q.sql == query.sql)
                {
                    Some(live) => *query = live.clone(),
                    None => *finished = true,
                }
            }
            Subject::Transaction {
                trx,
                blocking,
                finished,
            } => {
                if *finished {
                    return;
                }
                // A session outlives its transactions. The same one only grows older, so a
                // younger transaction under that id is another one.
                match sample
                    .transactions
                    .iter()
                    .find(|t| t.id == trx.id && t.secs + 1.5 >= trx.secs)
                {
                    Some(live) => {
                        *trx = live.clone();
                        *blocking = waiting_for(sample, live.id);
                    }
                    None => *finished = true,
                }
            }
            Subject::Slow(_) => {}
        }
    }
}

/// SQL that is to go to the clipboard.
pub struct ToCopy {
    pub sql: String,
    /// The server cut the text short, so this is not all of the statement.
    pub cut: bool,
}

/// A line to show for a moment.
pub struct Notice {
    pub text: String,
    pub failed: bool,
    at: Instant,
}

/// What one poll puts on the charts.
struct Charted {
    values: Vec<(&'static str, Option<f64>)>,
    threads_running: u32,
}

/// What arrived while the screen was held still.
#[derive(Default)]
struct Held {
    /// Each poll's charts, oldest first, so that the charts go on without a gap.
    charted: Vec<Charted>,
    /// The newest poll: the lists jump to now, not through each poll in turn.
    sample: Option<Sample>,
}

pub struct App {
    pub label: String,
    pub tab: Tab,
    pub sample: Option<Sample>,
    /// What each chart shows, oldest first. A poll with no number for a metric still adds a
    /// `NaN`, so that all the charts move in step.
    pub history: HashMap<&'static str, Vec<f64>>,
    pub threads: Vec<f64>,
    pub running_sel: Selection,
    pub slow_sel: Selection,
    pub slow_sort: SlowSort,
    pub trx_sel: Selection,
    pub filter: Filter,
    /// The list of keys is on the screen.
    pub help: bool,
    /// When the screen was held still, if it is. The polls go on and are kept for later.
    pub paused_since: Option<Instant>,
    held: Held,
    pub detail: Option<Detail>,
    /// Asked for by a key; the main loop copies it and says how that went.
    pub to_copy: Option<ToCopy>,
    notice: Option<Notice>,
    pub error: Option<String>,
    pub updated: Option<Instant>,
    pub quit: bool,
}

fn running_keys(filter: &Filter, sample: &Sample) -> Vec<String> {
    let rows = filter.running(sample);
    rows.iter().map(|q| q.id.to_string()).collect()
}

fn slow_keys(filter: &Filter, sample: &Sample) -> Vec<String> {
    let rows = filter.slow(sample);
    rows.iter().map(|q| q.digest.clone()).collect()
}

fn transaction_keys(filter: &Filter, sample: &Sample) -> Vec<String> {
    let rows = filter.transactions(sample);
    rows.iter().map(|t| t.id.to_string()).collect()
}

/// How many sessions wait for each session, each waiter counted once, whichever list it is in.
pub fn waiter_counts(sample: &Sample) -> HashMap<u64, usize> {
    let mut pairs: Vec<(u64, u64)> = sample
        .transactions
        .iter()
        .map(|t| (t.id, &t.blocked_by))
        .chain(sample.running.iter().map(|q| (q.id, &q.blocked_by)))
        .flat_map(|(waiter, blockers)| blockers.iter().map(move |&blocker| (blocker, waiter)))
        .collect();
    pairs.sort_unstable();
    pairs.dedup();
    let mut counts = HashMap::new();
    for (blocker, _) in pairs {
        *counts.entry(blocker).or_default() += 1;
    }
    counts
}

/// The sessions that wait for `id`, each once, whichever list it is found in.
pub fn waiting_for(sample: &Sample, id: u64) -> Vec<u64> {
    let mut waiting: Vec<u64> = sample
        .transactions
        .iter()
        .map(|t| (t.id, &t.blocked_by))
        .chain(sample.running.iter().map(|q| (q.id, &q.blocked_by)))
        .filter(|(_, blocked_by)| blocked_by.contains(&id))
        .map(|(waiter, _)| waiter)
        .collect();
    waiting.sort_unstable();
    waiting.dedup();
    waiting
}

fn push(history: &mut Vec<f64>, value: f64) {
    history.push(value);
    if history.len() > HISTORY {
        history.remove(0);
    }
}

impl App {
    pub fn new(label: String) -> Self {
        App {
            label,
            tab: Tab::Running,
            sample: None,
            history: HashMap::new(),
            threads: Vec::new(),
            running_sel: Selection::default(),
            slow_sel: Selection::default(),
            slow_sort: SlowSort::Total,
            trx_sel: Selection::default(),
            filter: Filter::default(),
            help: false,
            paused_since: None,
            held: Held::default(),
            detail: None,
            to_copy: None,
            notice: None,
            error: None,
            updated: None,
            quit: false,
        }
    }

    /// Applies everything the collector has sent. If it is gone, say so: the screen would
    /// otherwise just stop updating.
    pub fn drain(&mut self, msgs: &Receiver<Msg>) {
        loop {
            match msgs.try_recv() {
                Ok(msg) => self.on_msg(msg),
                Err(TryRecvError::Empty) => return,
                Err(TryRecvError::Disconnected) => {
                    self.error = Some("the monitoring thread stopped; restart dbtop".to_string());
                    return;
                }
            }
        }
    }

    /// Adds one poll to the charts.
    fn chart(&mut self, values: Vec<(&'static str, Option<f64>)>, threads_running: u32) {
        // A metric that is no longer reported (qps once pg_stat_statements is gone) must not
        // leave a frozen chart behind.
        self.history
            .retain(|key, _| values.iter().any(|(k, _)| k == key));
        for (key, value) in values {
            push(
                self.history.entry(key).or_default(),
                value.unwrap_or(f64::NAN),
            );
        }
        push(&mut self.threads, f64::from(threads_running));
    }

    fn show(&mut self, mut sample: Sample, values: Vec<(&'static str, Option<f64>)>) {
        sample.running.sort_by(|a, b| b.secs.total_cmp(&a.secs));
        sort_slow(&mut sample.slow, self.slow_sort);
        sample
            .transactions
            .sort_by(|a, b| b.secs.total_cmp(&a.secs));
        self.chart(values, sample.threads_running);
        if let Some(detail) = &mut self.detail {
            detail.refresh(&sample);
        }
        self.sample = Some(sample);
        self.sync_selections();
        self.error = None;
        self.updated = Some(Instant::now());
    }

    /// Puts each cursor back on its row after the lists changed: a new poll, a new ranking, or
    /// a filter that lets other rows through.
    fn sync_selections(&mut self) {
        let Some(sample) = &self.sample else { return };
        self.running_sel.sync(&running_keys(&self.filter, sample));
        self.slow_sel.sync(&slow_keys(&self.filter, sample));
        self.trx_sel.sync(&transaction_keys(&self.filter, sample));
    }

    pub fn paused(&self) -> bool {
        self.paused_since.is_some()
    }

    /// Holds the screen still, or lets it go on: the charts catch up with what was missed and
    /// the lists show how things are now.
    fn toggle_pause(&mut self) {
        if !self.paused() {
            self.paused_since = Some(Instant::now());
            return;
        }
        self.paused_since = None;
        let Held {
            mut charted,
            sample,
        } = std::mem::take(&mut self.held);
        let Some(sample) = sample else { return };
        let last = charted.pop().map(|c| c.values).unwrap_or_default();
        for poll in charted {
            self.chart(poll.values, poll.threads_running);
        }
        self.show(sample, last);
    }

    pub fn on_msg(&mut self, msg: Msg) {
        match msg {
            Msg::Poll { sample, values } if self.paused() => {
                self.held.charted.push(Charted {
                    values,
                    threads_running: sample.threads_running,
                });
                if self.held.charted.len() > HISTORY {
                    self.held.charted.remove(0);
                }
                self.held.sample = Some(sample);
            }
            Msg::Poll { sample, values } => self.show(sample, values),
            Msg::PollFailed(e) => self.error = Some(e),
            Msg::Plan { key, result } => {
                if let Some(detail) = self.detail.as_mut().filter(|d| d.key() == key) {
                    detail.plan = match result {
                        Ok(plan) => PlanState::Ready(plan),
                        Err(e) => PlanState::Failed(e),
                    };
                }
            }
        }
    }

    pub fn on_key(&mut self, key: KeyEvent, reqs: &Sender<Req>) {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.quit = true;
            return;
        }

        // Whatever key is pressed puts the help away, and does nothing else.
        if self.help {
            self.help = false;
            return;
        }

        if let Some(detail) = &mut self.detail {
            let page = PAGE as usize;
            match key.code {
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter | KeyCode::Backspace => {
                    self.detail = None;
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    detail.scroll = detail.scroll.saturating_add(1)
                }
                KeyCode::Up | KeyCode::Char('k') => detail.scroll = detail.scroll.saturating_sub(1),
                KeyCode::PageDown | KeyCode::Char(' ') => {
                    detail.scroll = detail.scroll.saturating_add(page);
                }
                KeyCode::PageUp => detail.scroll = detail.scroll.saturating_sub(page),
                KeyCode::Char('c') => self.copy_sql(),
                KeyCode::Home | KeyCode::Char('g') => detail.scroll = 0,
                // Clamped to the real end when drawn.
                KeyCode::End | KeyCode::Char('G') => detail.scroll = usize::MAX,
                _ => {}
            }
            return;
        }

        if self.filter.typing {
            self.type_filter(key);
            return;
        }

        match key.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('/') => self.filter.typing = true,
            KeyCode::Char('?') => self.help = true,
            KeyCode::Esc if self.filter.is_on() => {
                self.filter.text.clear();
                self.sync_selections();
            }
            KeyCode::Tab => self.tab = self.tab.next(),
            KeyCode::BackTab => self.tab = self.tab.previous(),
            KeyCode::Char('1') => self.tab = Tab::Running,
            KeyCode::Char('2') => self.tab = Tab::Slow,
            KeyCode::Char('3') => self.tab = Tab::Transactions,
            KeyCode::Char(' ' | 'p') => self.toggle_pause(),
            KeyCode::Char('c') => self.copy_sql(),
            KeyCode::Char('s') if self.tab == Tab::Slow => {
                self.slow_sort = self.slow_sort.next();
                if let Some(sample) = &mut self.sample {
                    sort_slow(&mut sample.slow, self.slow_sort);
                }
                self.sync_selections();
            }
            KeyCode::Down | KeyCode::Char('j') => self.move_by(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_by(-1),
            KeyCode::PageDown => self.move_by(PAGE),
            KeyCode::PageUp => self.move_by(-PAGE),
            KeyCode::Home | KeyCode::Char('g') => self.move_by(isize::MIN / 2),
            KeyCode::End | KeyCode::Char('G') => self.move_by(isize::MAX / 2),
            KeyCode::Enter => self.open_detail(reqs),
            _ => {}
        }
    }

    /// A key typed into the filter. The lists narrow as it goes.
    fn type_filter(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Enter => self.filter.typing = false,
            KeyCode::Esc => {
                self.filter.typing = false;
                self.filter.text.clear();
            }
            KeyCode::Backspace => {
                self.filter.text.pop();
            }
            KeyCode::Char(c)
                if !c.is_control() && !key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                self.filter.text.push(c);
            }
            // The cursor can be moved to the row that is wanted while the filter is typed.
            KeyCode::Down => return self.move_by(1),
            KeyCode::Up => return self.move_by(-1),
            _ => return,
        }
        self.sync_selections();
    }

    fn move_by(&mut self, delta: isize) {
        let Some(sample) = &self.sample else { return };
        let filter = &self.filter;
        match self.tab {
            Tab::Running => self
                .running_sel
                .move_by(delta, &running_keys(filter, sample)),
            Tab::Slow => self.slow_sel.move_by(delta, &slow_keys(filter, sample)),
            Tab::Transactions => self
                .trx_sel
                .move_by(delta, &transaction_keys(filter, sample)),
        }
    }

    /// What the cursor is on, and what to explain for it, if anything.
    fn selected_subject(&self) -> Option<(Subject, Option<ExplainTarget>)> {
        let sample = self.sample.as_ref()?;
        Some(match self.tab {
            Tab::Running => {
                let query = self
                    .running_sel
                    .state
                    .selected()
                    .and_then(|i| self.filter.running(sample).into_iter().nth(i))?;
                (
                    Subject::Running {
                        query: query.clone(),
                        finished: false,
                    },
                    Some(ExplainTarget::Running(query.clone())),
                )
            }
            Tab::Slow => {
                let slow = self
                    .slow_sel
                    .state
                    .selected()
                    .and_then(|i| self.filter.slow(sample).into_iter().nth(i))?;
                (
                    Subject::Slow(slow.clone()),
                    Some(ExplainTarget::Digest(slow.clone())),
                )
            }
            Tab::Transactions => {
                let trx = self
                    .trx_sel
                    .state
                    .selected()
                    .and_then(|i| self.filter.transactions(sample).into_iter().nth(i))?;
                let subject = Subject::Transaction {
                    trx: trx.clone(),
                    blocking: waiting_for(sample, trx.id),
                    finished: false,
                };
                (subject, None)
            }
        })
    }

    fn open_detail(&mut self, reqs: &Sender<Req>) {
        let Some((subject, target)) = self.selected_subject() else {
            return;
        };
        // The plan is read only because the user asked to open this query.
        let plan = match target {
            None => PlanState::NotAsked,
            Some(target) => match reqs.send(Req::Explain(target)) {
                Ok(()) => PlanState::Loading,
                Err(_) => PlanState::Failed("the collector thread has stopped".into()),
            },
        };
        self.detail = Some(Detail {
            subject,
            plan,
            scroll: 0,
        });
    }

    /// Asks for the SQL on screen (open in the details, else under the cursor) to be copied;
    /// the main loop does it, since this has no terminal to do it with.
    fn copy_sql(&mut self) {
        let subject = match &self.detail {
            Some(detail) => Some(detail.subject.sql_of()),
            None => self.selected_subject().map(|(subject, _)| subject.sql_of()),
        };
        match subject {
            Some((sql, cut)) if !sql.trim().is_empty() => {
                self.to_copy = Some(ToCopy { sql, cut });
            }
            Some(_) => self.tell("there is no SQL to copy", true),
            None => {}
        }
    }

    fn tell(&mut self, text: impl Into<String>, failed: bool) {
        self.notice = Some(Notice {
            text: text.into(),
            failed,
            at: Instant::now(),
        });
    }

    /// The line to show for a moment, such as what became of a copy.
    pub fn notice(&self) -> Option<&Notice> {
        self.notice.as_ref().filter(|n| n.at.elapsed() < NOTICE_FOR)
    }

    /// Says what became of the SQL the main loop was asked to copy.
    pub fn copied(&mut self, copy: &ToCopy, outcome: Result<Copied, String>) {
        let size = format!(
            "{} characters{}",
            copy.sql.chars().count(),
            if copy.cut {
                "; the server had cut it short"
            } else {
                ""
            }
        );
        match outcome {
            Ok(Copied::Clipboard) => self.tell(format!("copied the SQL ({size})"), false),
            Ok(Copied::Terminal) => {
                self.tell(
                    format!("asked the terminal to copy the SQL ({size})"),
                    false,
                );
            }
            Err(why) => self.tell(format!("could not copy: {why}"), true),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::{self, Receiver};

    fn query(id: u64, secs: f64, sql: &str) -> RunningQuery {
        RunningQuery {
            id,
            user: "u".into(),
            host: "h".into(),
            db: "d".into(),
            state: "s".into(),
            secs,
            sql: sql.into(),
            truncated: false,
            details: vec![],
            blocked_by: vec![],
        }
    }

    fn slow(digest: &str, total_ms: f64) -> SlowQuery {
        SlowQuery {
            digest: digest.into(),
            schema: "d".into(),
            sample_sql: "SELECT 1".into(),
            truncated: false,
            calls: 1,
            total_ms,
            avg_ms: total_ms,
            max_ms: total_ms,
            rows_examined_avg: Some(0.0),
            no_index_used: Some(false),
        }
    }

    fn sample_with(running: Vec<RunningQuery>) -> Sample {
        Sample {
            metrics: vec![],
            threads_running: 1,
            threads_connected: 1,
            max_connections: 10,
            running,
            running_note: None,
            transactions: vec![],
            transactions_note: None,
            encrypted: None,
            slow: vec![slow("aaa", 5.0), slow("bbb", 9.0)],
            slow_note: None,
        }
    }

    fn poll(app: &mut App, running: Vec<RunningQuery>) {
        app.on_msg(Msg::Poll {
            sample: sample_with(running),
            values: vec![],
        });
    }

    fn selected_id(app: &App) -> u64 {
        let at = app.running_sel.state.selected().unwrap();
        app.filter.running(app.sample.as_ref().unwrap())[at].id
    }

    fn press(app: &mut App, code: KeyCode) -> Receiver<Req> {
        let (tx, rx) = mpsc::channel();
        app.on_key(KeyEvent::from(code), &tx);
        rx
    }

    fn trx(id: u64, secs: f64) -> OpenTransaction {
        OpenTransaction {
            id,
            user: "u".into(),
            host: "h".into(),
            db: "d".into(),
            secs,
            idle: true,
            state: "RUNNING".into(),
            rows_locked: Some(1),
            rows_modified: Some(1),
            sql: "UPDATE t SET a = 1".into(),
            truncated: false,
            blocked_by: vec![],
        }
    }

    fn poll_transactions(app: &mut App, transactions: Vec<OpenTransaction>) {
        let mut sample = sample_with(vec![]);
        sample.transactions = transactions;
        app.on_msg(Msg::Poll {
            sample,
            values: vec![],
        });
    }

    fn selected_transaction(app: &App) -> u64 {
        let at = app.trx_sel.state.selected().unwrap();
        app.filter.transactions(app.sample.as_ref().unwrap())[at].id
    }

    fn slow_stats(
        digest: &str,
        total: f64,
        avg: f64,
        max: f64,
        calls: u64,
        rows: Option<f64>,
    ) -> SlowQuery {
        SlowQuery {
            total_ms: total,
            avg_ms: avg,
            max_ms: max,
            calls,
            rows_examined_avg: rows,
            ..slow(digest, total)
        }
    }

    fn poll_slow(app: &mut App) {
        let mut sample = sample_with(vec![]);
        sample.slow = vec![
            slow_stats("a", 900.0, 3.0, 400.0, 300, Some(10.0)),
            slow_stats("b", 500.0, 50.0, 90.0, 10, None),
            slow_stats("c", 100.0, 1.0, 1000.0, 100, Some(5000.0)),
            slow_stats("d", 700.0, 2.0, 10.0, 350, Some(100.0)),
        ];
        app.on_msg(Msg::Poll {
            sample,
            values: vec![],
        });
    }

    fn slow_order(app: &App) -> String {
        let slow = &app.sample.as_ref().unwrap().slow;
        slow.iter().map(|q| q.digest.as_str()).collect()
    }

    fn selected_digest(app: &App) -> String {
        let at = app.slow_sel.state.selected().unwrap();
        app.filter.slow(app.sample.as_ref().unwrap())[at]
            .digest
            .clone()
    }

    #[test]
    fn s_ranks_the_slow_list_by_each_column_in_turn_and_the_cursor_stays_with_its_query() {
        let mut app = App::new(String::new());
        press(&mut app, KeyCode::Char('2'));
        poll_slow(&mut app);
        assert_eq!(slow_order(&app), "adbc", "by what it costs in all");

        press(&mut app, KeyCode::Down);
        assert_eq!(selected_digest(&app), "d");
        for (ranked_by, order) in [
            (SlowSort::Average, "badc"),
            (SlowSort::Max, "cabd"),
            (SlowSort::Calls, "dacb"),
            (SlowSort::Rows, "cdab"),
            (SlowSort::Total, "adbc"),
        ] {
            press(&mut app, KeyCode::Char('s'));
            assert_eq!(app.slow_sort, ranked_by);
            assert_eq!(slow_order(&app), order, "{ranked_by:?}");
            assert_eq!(
                selected_digest(&app),
                "d",
                "the cursor moves with its query"
            );
        }
    }

    #[test]
    fn a_new_poll_keeps_the_ranking_that_was_chosen_and_s_means_nothing_elsewhere() {
        let mut app = App::new(String::new());
        press(&mut app, KeyCode::Char('s'));
        assert_eq!(app.slow_sort, SlowSort::Total, "not on the slow tab");

        press(&mut app, KeyCode::Char('2'));
        press(&mut app, KeyCode::Char('s'));
        poll_slow(&mut app);
        assert_eq!(app.slow_sort, SlowSort::Average);
        assert_eq!(slow_order(&app), "badc");
        poll_slow(&mut app);
        assert_eq!(slow_order(&app), "badc");
    }

    #[test]
    fn tab_goes_round_three_tabs_and_shift_tab_goes_back() {
        let mut app = App::new(String::new());
        for expected in [Tab::Slow, Tab::Transactions, Tab::Running] {
            press(&mut app, KeyCode::Tab);
            assert!(app.tab == expected);
        }
        press(&mut app, KeyCode::BackTab);
        assert!(
            app.tab == Tab::Transactions,
            "back from the first is the last"
        );
        press(&mut app, KeyCode::Char('1'));
        assert!(app.tab == Tab::Running);
        press(&mut app, KeyCode::Char('3'));
        assert!(app.tab == Tab::Transactions);
    }

    #[test]
    fn transactions_are_listed_oldest_first_and_the_cursor_follows_its_session() {
        let mut app = App::new(String::new());
        press(&mut app, KeyCode::Char('3'));
        poll_transactions(&mut app, vec![trx(1, 5.0), trx(2, 90.0), trx(3, 40.0)]);
        assert_eq!(selected_transaction(&app), 2, "the oldest is on top");

        press(&mut app, KeyCode::Down);
        assert_eq!(selected_transaction(&app), 3);
        // The order changes under the cursor: it stays with its session.
        poll_transactions(&mut app, vec![trx(1, 99.0), trx(2, 91.0), trx(3, 41.0)]);
        assert_eq!(selected_transaction(&app), 3);
    }

    #[test]
    fn opening_a_transaction_asks_the_server_for_no_plan() {
        let mut app = App::new(String::new());
        press(&mut app, KeyCode::Char('3'));
        poll_transactions(&mut app, vec![trx(7, 30.0)]);

        let asked = press(&mut app, KeyCode::Enter);
        assert!(
            asked.try_recv().is_err(),
            "a transaction is not a statement"
        );
        let detail = app.detail.as_ref().expect("the details are open");
        assert!(matches!(detail.subject, Subject::Transaction { .. }));
        assert!(matches!(detail.plan, PlanState::NotAsked));
    }

    #[test]
    fn a_transactions_details_follow_it_and_end_when_it_does() {
        let mut app = App::new(String::new());
        press(&mut app, KeyCode::Char('3'));
        poll_transactions(&mut app, vec![trx(7, 30.0)]);
        press(&mut app, KeyCode::Enter);

        let age = |app: &App| match &app.detail.as_ref().unwrap().subject {
            Subject::Transaction { trx, finished, .. } => (trx.secs, *finished),
            _ => panic!("not a transaction"),
        };
        poll_transactions(&mut app, vec![trx(7, 31.0)]);
        assert_eq!(age(&app), (31.0, false), "it follows the live one");

        // The same session with a younger transaction is another one.
        poll_transactions(&mut app, vec![trx(7, 2.0)]);
        assert_eq!(age(&app), (31.0, true));

        // And one that is gone has ended; the screen keeps what it last knew.
        press(&mut app, KeyCode::Esc);
        poll_transactions(&mut app, vec![trx(8, 5.0)]);
        press(&mut app, KeyCode::Enter);
        poll_transactions(&mut app, vec![]);
        assert!(age(&app).1);
    }

    #[test]
    fn who_waits_for_whom_is_counted_once_per_waiter_whichever_list_it_is_in() {
        let mut waiter = trx(2, 10.0);
        waiter.blocked_by = vec![1];
        let mut running = query(2, 10.0, "UPDATE t SET a = 1");
        running.blocked_by = vec![1];
        let mut other = query(3, 4.0, "DELETE FROM t");
        other.blocked_by = vec![1, 9];

        let mut sample = sample_with(vec![running, other]);
        sample.transactions = vec![trx(1, 99.0), waiter];

        let counts = waiter_counts(&sample);
        assert_eq!(counts.get(&1), Some(&2), "sessions 2 and 3, session 2 once");
        assert_eq!(counts.get(&9), Some(&1));
        assert_eq!(counts.get(&2), None);
        assert_eq!(waiting_for(&sample, 1), [2, 3]);
    }

    #[test]
    fn a_vanished_collector_is_reported_not_silently_ignored() {
        let (tx, rx) = mpsc::channel();
        let mut app = App::new(String::new());
        tx.send(Msg::PollFailed("one".into())).unwrap();
        app.drain(&rx);
        assert_eq!(app.error.as_deref(), Some("one"), "messages still arrive");

        drop(tx);
        app.drain(&rx);
        assert!(
            app.error
                .as_deref()
                .unwrap()
                .contains("monitoring thread stopped")
        );
    }

    fn poll_with(app: &mut App, values: Vec<(&'static str, Option<f64>)>) {
        app.on_msg(Msg::Poll {
            sample: sample_with(vec![]),
            values,
        });
    }

    #[test]
    fn every_chart_gets_a_point_per_poll_so_they_stay_in_step() {
        let mut app = App::new(String::new());
        poll_with(&mut app, vec![("a", Some(1.0)), ("b", None)]);
        poll_with(&mut app, vec![("a", Some(2.0)), ("b", Some(5.0))]);

        let b = &app.history["b"];
        assert_eq!(
            b.len(),
            2,
            "one point per poll, even when there was no number"
        );
        assert!(b[0].is_nan() && b[1] == 5.0, "{b:?}");
        assert_eq!(app.history["a"], [1.0, 2.0]);
    }

    #[test]
    fn a_metric_that_stops_being_reported_loses_its_chart() {
        let mut app = App::new(String::new());
        poll_with(&mut app, vec![("qps", Some(1.0))]);
        poll_with(&mut app, vec![("tps", Some(2.0))]);
        assert!(!app.history.contains_key("qps") && app.history.contains_key("tps"));
    }

    fn poll_both(app: &mut App, running: Vec<RunningQuery>, value: f64) {
        app.on_msg(Msg::Poll {
            sample: sample_with(running),
            values: vec![("a", Some(value))],
        });
    }

    #[test]
    fn a_paused_screen_stands_still_and_goes_on_without_a_gap_in_the_charts() {
        let mut app = App::new(String::new());
        poll_both(&mut app, vec![query(1, 1.0, "a")], 1.0);

        press(&mut app, KeyCode::Char(' '));
        assert!(app.paused());
        poll_both(&mut app, vec![query(2, 2.0, "b")], 2.0);
        poll_both(&mut app, vec![query(3, 3.0, "c")], 3.0);
        assert_eq!(selected_id(&app), 1, "the lists show what they showed");
        assert_eq!(app.history["a"], [1.0]);

        press(&mut app, KeyCode::Char(' '));
        assert!(!app.paused());
        assert_eq!(selected_id(&app), 3, "and now show how things are");
        assert_eq!(
            app.history["a"],
            [1.0, 2.0, 3.0],
            "every poll is charted, in order"
        );
        assert_eq!(app.threads.len(), 3);
    }

    #[test]
    fn going_on_with_nothing_missed_changes_nothing() {
        let mut app = App::new(String::new());
        press(&mut app, KeyCode::Char('p'));
        press(&mut app, KeyCode::Char('p'));
        assert!(!app.paused() && app.sample.is_none(), "before any poll");

        poll_both(&mut app, vec![query(1, 1.0, "a")], 1.0);
        press(&mut app, KeyCode::Char('p'));
        press(&mut app, KeyCode::Char('p'));
        assert_eq!(app.history["a"], [1.0]);
        assert_eq!(selected_id(&app), 1);
    }

    #[test]
    fn what_is_held_back_is_bounded_like_the_charts() {
        let mut app = App::new(String::new());
        poll_both(&mut app, vec![], 0.0);
        press(&mut app, KeyCode::Char(' '));
        for i in 0..HISTORY + 50 {
            poll_both(&mut app, vec![], i as f64);
        }
        assert_eq!(app.held.charted.len(), HISTORY);

        press(&mut app, KeyCode::Char(' '));
        let chart = &app.history["a"];
        assert_eq!(chart.len(), HISTORY);
        assert_eq!(chart.last(), Some(&((HISTORY + 49) as f64)));
    }

    #[test]
    fn a_plan_and_a_failure_still_arrive_on_a_paused_screen() {
        let mut app = App::new(String::new());
        poll(&mut app, vec![query(7, 3.0, "SELECT 7")]);
        press(&mut app, KeyCode::Char(' '));
        press(&mut app, KeyCode::Enter);
        app.on_msg(Msg::Plan {
            key: PlanKey::Running(7),
            result: Ok(Plan::Text("mine".into())),
        });
        assert!(matches!(
            app.detail.as_ref().unwrap().plan,
            PlanState::Ready(_)
        ));

        app.on_msg(Msg::PollFailed("gone".into()));
        assert_eq!(app.error.as_deref(), Some("gone"));
    }

    #[test]
    fn space_scrolls_details_instead_of_pausing() {
        let mut app = App::new(String::new());
        poll(&mut app, vec![query(7, 3.0, "SELECT 7")]);
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Char(' '));
        assert!(!app.paused());
        assert!(app.detail.as_ref().unwrap().scroll > 0);
    }

    fn type_text(app: &mut App, text: &str) {
        for c in text.chars() {
            press(app, KeyCode::Char(c));
        }
    }

    fn shown_ids(app: &App) -> Vec<u64> {
        let sample = app.sample.as_ref().unwrap();
        app.filter.running(sample).iter().map(|q| q.id).collect()
    }

    fn three_queries() -> Vec<RunningQuery> {
        vec![
            query(1, 1.0, "SELECT * FROM orders WHERE id = 1"),
            query(2, 2.0, "UPDATE Orders SET paid = 1"),
            query(3, 3.0, "SELECT * FROM users"),
        ]
    }

    #[test]
    fn a_filter_keeps_the_rows_that_hold_every_word_whatever_the_case() {
        let mut app = App::new(String::new());
        poll(&mut app, three_queries());
        assert_eq!(shown_ids(&app), [3, 2, 1]);

        press(&mut app, KeyCode::Char('/'));
        type_text(&mut app, "ORDERS");
        assert_eq!(shown_ids(&app), [2, 1]);
        type_text(&mut app, " select");
        assert_eq!(
            shown_ids(&app),
            [1],
            "each word must be there, in any order"
        );
        type_text(&mut app, "x");
        assert!(shown_ids(&app).is_empty());
    }

    #[test]
    fn the_filter_looks_at_who_and_where_and_in_what_state_not_only_at_the_text() {
        let mut app = App::new(String::new());
        let mut other = query(2, 2.0, "SELECT 2");
        other.user = "alice".into();
        other.host = "10.9.8.7".into();
        other.db = "billing".into();
        other.state = "Sending data".into();
        poll(&mut app, vec![query(1, 1.0, "SELECT 1"), other]);

        press(&mut app, KeyCode::Char('/'));
        for (typed, found) in [
            ("alice", 2),
            ("10.9.8", 2),
            ("billing", 2),
            ("sending", 2),
            ("2", 2),
        ] {
            app.filter.text.clear();
            type_text(&mut app, typed);
            assert_eq!(shown_ids(&app), [found], "{typed}");
        }
    }

    #[test]
    fn backspace_widens_the_filter_again_and_blanks_let_everything_through() {
        let mut app = App::new(String::new());
        poll(&mut app, three_queries());
        press(&mut app, KeyCode::Char('/'));
        type_text(&mut app, "users");
        assert_eq!(shown_ids(&app), [3]);
        for _ in 0..5 {
            press(&mut app, KeyCode::Backspace);
        }
        assert_eq!(shown_ids(&app), [3, 2, 1]);
        press(&mut app, KeyCode::Backspace);
        type_text(&mut app, "   ");
        assert_eq!(shown_ids(&app), [3, 2, 1]);
        assert!(!app.filter.is_on());
    }

    #[test]
    fn enter_keeps_the_filter_and_esc_clears_it() {
        let mut app = App::new(String::new());
        poll(&mut app, three_queries());
        press(&mut app, KeyCode::Char('/'));
        type_text(&mut app, "users");
        press(&mut app, KeyCode::Enter);
        assert!(!app.filter.typing, "typing is over");
        assert_eq!(shown_ids(&app), [3], "but the list stays narrowed");
        assert!(app.detail.is_none(), "and Enter has not also opened a row");

        press(&mut app, KeyCode::Esc);
        assert_eq!(shown_ids(&app), [3, 2, 1]);

        press(&mut app, KeyCode::Char('/'));
        type_text(&mut app, "users");
        press(&mut app, KeyCode::Esc);
        assert!(!app.filter.typing && app.filter.text.is_empty());
        assert_eq!(shown_ids(&app), [3, 2, 1]);
    }

    #[test]
    fn what_is_typed_into_the_filter_is_text_not_commands() {
        let mut app = App::new(String::new());
        poll(&mut app, three_queries());
        press(&mut app, KeyCode::Char('/'));
        type_text(&mut app, "q p s 2 j c ?");
        assert!(!app.quit && !app.paused() && app.tab == Tab::Running);
        assert_eq!(app.filter.text, "q p s 2 j c ?");
        assert!(app.to_copy.is_none() && !app.help);

        let (tx, _rx) = mpsc::channel();
        app.on_key(
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            &tx,
        );
        assert!(app.quit, "Ctrl+C still gets out");
    }

    #[test]
    fn details_open_for_the_row_the_filter_left_not_the_one_at_that_position() {
        let mut app = App::new(String::new());
        poll(&mut app, three_queries());
        press(&mut app, KeyCode::Char('/'));
        type_text(&mut app, "select orders");
        press(&mut app, KeyCode::Enter);

        let reqs = press(&mut app, KeyCode::Enter);
        let Some(Req::Explain(asked)) = reqs.try_recv().ok() else {
            panic!("no plan was requested");
        };
        assert_eq!(asked.key(), PlanKey::Running(1));
    }

    #[test]
    fn the_cursor_stays_on_its_row_while_the_filter_narrows_and_the_arrows_work_in_it() {
        let mut app = App::new(String::new());
        poll(&mut app, three_queries());
        press(&mut app, KeyCode::Down);
        assert_eq!(selected_id(&app), 2);

        press(&mut app, KeyCode::Char('/'));
        type_text(&mut app, "orders");
        assert_eq!(selected_id(&app), 2, "still on the same query");

        press(&mut app, KeyCode::Down);
        assert_eq!(selected_id(&app), 1, "an arrow moves it while typing");
        assert_eq!(app.filter.text, "orders");
    }

    #[test]
    fn the_filter_holds_through_new_polls_and_across_the_tabs() {
        let mut app = App::new(String::new());
        let mut sample = sample_with(three_queries());
        sample.slow = vec![
            SlowQuery {
                sample_sql: "SELECT * FROM orders".into(),
                ..slow("o", 5.0)
            },
            SlowQuery {
                sample_sql: "SELECT * FROM users".into(),
                ..slow("u", 9.0)
            },
        ];
        sample.transactions = vec![
            trx(11, 9.0),
            OpenTransaction {
                sql: "UPDATE orders SET a = 2".into(),
                ..trx(12, 5.0)
            },
        ];
        app.on_msg(Msg::Poll {
            sample: sample.clone(),
            values: vec![],
        });
        press(&mut app, KeyCode::Char('/'));
        type_text(&mut app, "orders");
        press(&mut app, KeyCode::Enter);

        app.on_msg(Msg::Poll {
            sample,
            values: vec![],
        });
        assert_eq!(shown_ids(&app), [2, 1]);

        press(&mut app, KeyCode::Char('2'));
        assert_eq!(selected_digest(&app), "o");
        press(&mut app, KeyCode::Char('3'));
        assert_eq!(selected_transaction(&app), 12);
    }

    #[test]
    fn a_question_mark_shows_the_help_and_any_key_puts_it_away_without_acting() {
        let mut app = App::new(String::new());
        poll(&mut app, three_queries());
        press(&mut app, KeyCode::Char('?'));
        assert!(app.help);

        press(&mut app, KeyCode::Char('q'));
        assert!(!app.help && !app.quit, "that q only closed the help");
        press(&mut app, KeyCode::Char('?'));
        press(&mut app, KeyCode::Enter);
        assert!(
            !app.help && app.detail.is_none(),
            "and that Enter opened nothing"
        );

        press(&mut app, KeyCode::Char('?'));
        let (tx, _rx) = mpsc::channel();
        app.on_key(
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            &tx,
        );
        assert!(app.quit, "Ctrl+C still gets out");
    }

    fn copied_sql(app: &mut App) -> Option<(String, bool)> {
        app.to_copy.take().map(|c| (c.sql, c.cut))
    }

    #[test]
    fn c_asks_for_the_sql_under_the_cursor_as_the_server_gave_it() {
        let mut app = App::new(String::new());
        let sql = "SELECT *\n\tFROM orders\n WHERE id = 1";
        poll(&mut app, vec![query(1, 1.0, sql)]);
        press(&mut app, KeyCode::Char('c'));
        assert_eq!(copied_sql(&mut app), Some((sql.to_string(), false)));
        assert!(app.detail.is_none(), "the list stays as it was");
    }

    #[test]
    fn c_inside_the_details_copies_what_they_show_and_leaves_them_open() {
        let mut app = App::new(String::new());
        poll(
            &mut app,
            vec![query(1, 1.0, "SELECT 1"), query(2, 9.0, "SELECT 2")],
        );
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Char('c'));
        assert_eq!(copied_sql(&mut app), Some(("SELECT 2".into(), false)));
        assert!(app.detail.is_some());
    }

    #[test]
    fn c_copies_a_digests_sample_and_a_transactions_last_statement() {
        let mut app = App::new(String::new());
        let mut sample = sample_with(vec![]);
        sample.slow = vec![SlowQuery {
            sample_sql: "SELECT slow".into(),
            truncated: true,
            ..slow("a", 1.0)
        }];
        sample.transactions = vec![OpenTransaction {
            sql: "UPDATE held".into(),
            ..trx(5, 1.0)
        }];
        app.on_msg(Msg::Poll {
            sample,
            values: vec![],
        });

        press(&mut app, KeyCode::Char('2'));
        press(&mut app, KeyCode::Char('c'));
        assert_eq!(
            copied_sql(&mut app),
            Some(("SELECT slow".into(), true)),
            "and that the server cut it"
        );
        press(&mut app, KeyCode::Char('3'));
        press(&mut app, KeyCode::Char('c'));
        assert_eq!(copied_sql(&mut app), Some(("UPDATE held".into(), false)));
    }

    #[test]
    fn c_with_no_sql_or_no_row_copies_nothing_and_says_which() {
        let mut app = App::new(String::new());
        press(&mut app, KeyCode::Char('c'));
        assert!(app.to_copy.is_none() && app.notice().is_none(), "no row");

        let mut sample = sample_with(vec![]);
        sample.transactions = vec![OpenTransaction {
            sql: "  ".into(),
            ..trx(5, 1.0)
        }];
        app.on_msg(Msg::Poll {
            sample,
            values: vec![],
        });
        press(&mut app, KeyCode::Char('3'));
        press(&mut app, KeyCode::Char('c'));
        assert!(app.to_copy.is_none());
        let notice = app.notice().unwrap();
        assert!(notice.text.contains("no SQL") && notice.failed);
    }

    #[test]
    fn what_became_of_a_copy_is_told_for_a_moment() {
        let mut app = App::new(String::new());
        let copy = ToCopy {
            sql: "SELECT 한글".into(),
            cut: false,
        };
        app.copied(&copy, Ok(Copied::Clipboard));
        let notice = app.notice().unwrap();
        assert_eq!(notice.text, "copied the SQL (9 characters)", "not bytes");
        assert!(!notice.failed);

        app.copied(&copy, Ok(Copied::Terminal));
        assert!(app.notice().unwrap().text.starts_with("asked the terminal"));

        let cut = ToCopy {
            sql: "SELECT".into(),
            cut: true,
        };
        app.copied(&cut, Ok(Copied::Clipboard));
        assert!(app.notice().unwrap().text.contains("cut it short"));

        app.copied(&copy, Err("no room".into()));
        let notice = app.notice().unwrap();
        assert_eq!(notice.text, "could not copy: no room");
        assert!(notice.failed);

        app.notice.as_mut().unwrap().at -= NOTICE_FOR;
        assert!(app.notice().is_none(), "and then it goes");
    }

    #[test]
    fn rows_are_sorted_longest_running_first() {
        let mut app = App::new(String::new());
        poll(&mut app, vec![query(1, 1.0, "a"), query(2, 9.0, "b")]);
        assert_eq!(selected_id(&app), 2);
    }

    #[test]
    fn selection_follows_its_row_when_the_order_changes() {
        let mut app = App::new(String::new());
        poll(
            &mut app,
            vec![query(1, 10.0, "a"), query(2, 5.0, "b"), query(3, 1.0, "c")],
        );
        press(&mut app, KeyCode::Down);
        assert_eq!(selected_id(&app), 2);

        poll(
            &mut app,
            vec![query(1, 11.0, "a"), query(2, 6.0, "b"), query(3, 20.0, "c")],
        );
        assert_eq!(
            selected_id(&app),
            2,
            "query 3 jumped to the top; the cursor must stay on 2"
        );
    }

    #[test]
    fn selection_keeps_its_position_when_its_row_disappears() {
        let mut app = App::new(String::new());
        poll(
            &mut app,
            vec![query(1, 10.0, "a"), query(2, 5.0, "b"), query(3, 1.0, "c")],
        );
        press(&mut app, KeyCode::Down);
        poll(&mut app, vec![query(1, 11.0, "a"), query(3, 2.0, "c")]);
        assert_eq!(selected_id(&app), 3);

        press(&mut app, KeyCode::Down);
        poll(&mut app, vec![query(1, 12.0, "a")]);
        assert_eq!(selected_id(&app), 1, "clamped to the last row");

        poll(&mut app, vec![]);
        assert_eq!(app.running_sel.state.selected(), None);
    }

    #[test]
    fn home_and_end_jump_to_the_ends() {
        let mut app = App::new(String::new());
        poll(
            &mut app,
            vec![query(1, 3.0, "a"), query(2, 2.0, "b"), query(3, 1.0, "c")],
        );
        press(&mut app, KeyCode::Char('G'));
        assert_eq!(selected_id(&app), 3);
        press(&mut app, KeyCode::Char('g'));
        assert_eq!(selected_id(&app), 1);
    }

    #[test]
    fn enter_opens_details_and_asks_for_the_plan() {
        let mut app = App::new(String::new());
        poll(&mut app, vec![query(7, 3.0, "SELECT 7")]);
        let reqs = press(&mut app, KeyCode::Enter);

        let Some(Req::Explain(asked)) = reqs.try_recv().ok() else {
            panic!("no plan was requested");
        };
        assert_eq!(asked.key(), PlanKey::Running(7));
        assert!(matches!(
            app.detail.as_ref().unwrap().plan,
            PlanState::Loading
        ));
    }

    #[test]
    fn no_plan_is_requested_without_an_explicit_enter() {
        let mut app = App::new(String::new());
        poll(&mut app, vec![query(7, 3.0, "SELECT 7")]);
        let reqs = press(&mut app, KeyCode::Down);
        assert!(reqs.try_recv().is_err());
        assert!(app.detail.is_none());
    }

    #[test]
    fn a_plan_only_lands_on_the_query_it_was_for() {
        let mut app = App::new(String::new());
        poll(&mut app, vec![query(7, 3.0, "SELECT 7")]);
        press(&mut app, KeyCode::Enter);

        app.on_msg(Msg::Plan {
            key: PlanKey::Running(99),
            result: Ok(Plan::Text("other".into())),
        });
        app.on_msg(Msg::Plan {
            key: PlanKey::Digest("7".into()),
            result: Ok(Plan::Text("other".into())),
        });
        assert!(matches!(
            app.detail.as_ref().unwrap().plan,
            PlanState::Loading
        ));

        app.on_msg(Msg::Plan {
            key: PlanKey::Running(7),
            result: Ok(Plan::Text("mine".into())),
        });
        assert!(matches!(
            app.detail.as_ref().unwrap().plan,
            PlanState::Ready(_)
        ));

        app.on_msg(Msg::Plan {
            key: PlanKey::Running(7),
            result: Err("boom".into()),
        });
        assert!(matches!(
            app.detail.as_ref().unwrap().plan,
            PlanState::Failed(_)
        ));
    }

    #[test]
    fn details_follow_the_live_query() {
        let mut app = App::new(String::new());
        poll(&mut app, vec![query(7, 3.0, "SELECT 7")]);
        press(&mut app, KeyCode::Enter);
        poll(&mut app, vec![query(7, 4.0, "SELECT 7")]);

        let Subject::Running { query, finished } = &app.detail.as_ref().unwrap().subject else {
            panic!("expected a running subject");
        };
        assert_eq!(query.secs, 4.0);
        assert!(!finished);
    }

    #[test]
    fn details_freeze_once_the_connection_moves_on() {
        let mut app = App::new(String::new());
        poll(&mut app, vec![query(7, 3.0, "SELECT 7")]);
        press(&mut app, KeyCode::Enter);

        // Same connection id, different statement: not the query being looked at.
        poll(&mut app, vec![query(7, 0.1, "SELECT 8")]);
        // Even if the original text comes back, the earlier run is over.
        poll(&mut app, vec![query(7, 0.2, "SELECT 7")]);

        let Subject::Running { query, finished } = &app.detail.as_ref().unwrap().subject else {
            panic!("expected a running subject");
        };
        assert!(finished);
        assert_eq!(
            query.secs, 3.0,
            "frozen at the last time it was seen running"
        );
    }

    #[test]
    fn q_closes_details_first_then_quits() {
        let mut app = App::new(String::new());
        poll(&mut app, vec![query(7, 3.0, "SELECT 7")]);
        press(&mut app, KeyCode::Enter);

        press(&mut app, KeyCode::Char('q'));
        assert!(app.detail.is_none() && !app.quit);
        press(&mut app, KeyCode::Char('q'));
        assert!(app.quit);
    }

    #[test]
    fn scrolling_never_overflows() {
        let mut app = App::new(String::new());
        poll(&mut app, vec![query(7, 3.0, "SELECT 7")]);
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::End);
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::PageDown);
        press(&mut app, KeyCode::Home);
        press(&mut app, KeyCode::PageUp);
        assert_eq!(app.detail.as_ref().unwrap().scroll, 0);
    }

    #[test]
    fn slow_tab_asks_for_the_digests_plan() {
        let mut app = App::new(String::new());
        poll(&mut app, vec![query(7, 3.0, "SELECT 7")]);
        press(&mut app, KeyCode::Char('2'));
        let reqs = press(&mut app, KeyCode::Enter);

        let Some(Req::Explain(asked)) = reqs.try_recv().ok() else {
            panic!("no plan was requested");
        };
        assert_eq!(
            asked.key(),
            PlanKey::Digest("bbb".into()),
            "highest total time first"
        );
        let detail = app.detail.as_ref().unwrap();
        assert!(matches!(&detail.subject, Subject::Slow(s) if s.digest == "bbb"));
        assert!(matches!(detail.plan, PlanState::Loading));

        app.on_msg(Msg::Plan {
            key: PlanKey::Digest("bbb".into()),
            result: Ok(Plan::Text("p".into())),
        });
        assert!(matches!(
            app.detail.as_ref().unwrap().plan,
            PlanState::Ready(_)
        ));
    }

    #[test]
    fn enter_on_an_empty_list_does_nothing() {
        let mut app = App::new(String::new());
        poll(&mut app, vec![]);
        press(&mut app, KeyCode::Enter);
        assert!(app.detail.is_none());
    }

    #[test]
    fn ctrl_c_quits_even_inside_details() {
        let mut app = App::new(String::new());
        poll(&mut app, vec![query(7, 3.0, "SELECT 7")]);
        press(&mut app, KeyCode::Enter);
        let (tx, _rx) = mpsc::channel();
        app.on_key(
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            &tx,
        );
        assert!(app.quit);
    }
}
