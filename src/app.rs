use std::collections::HashMap;
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::time::Instant;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::widgets::TableState;

use crate::collector::{Msg, Req};
use crate::model::{ExplainTarget, Plan, PlanKey, RunningQuery, Sample, SlowQuery};

/// Samples kept for the graphs: ten minutes at one poll a second.
const HISTORY: usize = 600;
const PAGE: isize = 10;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Running,
    Slow,
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
}

pub enum PlanState {
    Loading,
    Ready(Plan),
    Failed(String),
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
        }
    }

    /// Follows the live query while it runs, then freezes it.
    fn refresh(&mut self, sample: &Sample) {
        let Subject::Running { query, finished } = &mut self.subject else {
            return;
        };
        if *finished {
            return;
        }
        // A connection id outlives its statements, so the same text has to still be running.
        match sample
            .running
            .iter()
            .find(|q| q.id == query.id && q.sql == query.sql)
        {
            Some(live) => *query = live.clone(),
            None => *finished = true,
        }
    }
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
    pub detail: Option<Detail>,
    pub error: Option<String>,
    pub updated: Option<Instant>,
    pub quit: bool,
}

fn running_keys(sample: &Sample) -> Vec<String> {
    sample.running.iter().map(|q| q.id.to_string()).collect()
}

fn slow_keys(sample: &Sample) -> Vec<String> {
    sample.slow.iter().map(|q| q.digest.clone()).collect()
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
            detail: None,
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

    pub fn on_msg(&mut self, msg: Msg) {
        match msg {
            Msg::Poll { mut sample, values } => {
                sample.running.sort_by(|a, b| b.secs.total_cmp(&a.secs));
                sample
                    .slow
                    .sort_by(|a, b| b.total_ms.total_cmp(&a.total_ms));
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
                push(&mut self.threads, f64::from(sample.threads_running));
                self.running_sel.sync(&running_keys(&sample));
                self.slow_sel.sync(&slow_keys(&sample));
                if let Some(detail) = &mut self.detail {
                    detail.refresh(&sample);
                }
                self.sample = Some(sample);
                self.error = None;
                self.updated = Some(Instant::now());
            }
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
                KeyCode::Home | KeyCode::Char('g') => detail.scroll = 0,
                // Clamped to the real end when drawn.
                KeyCode::End | KeyCode::Char('G') => detail.scroll = usize::MAX,
                _ => {}
            }
            return;
        }

        match key.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Tab | KeyCode::BackTab => {
                self.tab = match self.tab {
                    Tab::Running => Tab::Slow,
                    Tab::Slow => Tab::Running,
                };
            }
            KeyCode::Char('1') => self.tab = Tab::Running,
            KeyCode::Char('2') => self.tab = Tab::Slow,
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

    fn move_by(&mut self, delta: isize) {
        let Some(sample) = &self.sample else { return };
        match self.tab {
            Tab::Running => self.running_sel.move_by(delta, &running_keys(sample)),
            Tab::Slow => self.slow_sel.move_by(delta, &slow_keys(sample)),
        }
    }

    fn open_detail(&mut self, reqs: &Sender<Req>) {
        let Some(sample) = &self.sample else { return };
        let (subject, target) = match self.tab {
            Tab::Running => {
                let Some(query) = self
                    .running_sel
                    .state
                    .selected()
                    .and_then(|i| sample.running.get(i))
                else {
                    return;
                };
                (
                    Subject::Running {
                        query: query.clone(),
                        finished: false,
                    },
                    ExplainTarget::Running(query.clone()),
                )
            }
            Tab::Slow => {
                let Some(slow) = self
                    .slow_sel
                    .state
                    .selected()
                    .and_then(|i| sample.slow.get(i))
                else {
                    return;
                };
                (
                    Subject::Slow(slow.clone()),
                    ExplainTarget::Digest(slow.clone()),
                )
            }
        };
        // The plan is read only because the user asked to open this query.
        let plan = match reqs.send(Req::Explain(target)) {
            Ok(()) => PlanState::Loading,
            Err(_) => PlanState::Failed("the collector thread has stopped".into()),
        };
        self.detail = Some(Detail {
            subject,
            plan,
            scroll: 0,
        });
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
        app.sample.as_ref().unwrap().running[at].id
    }

    fn press(app: &mut App, code: KeyCode) -> Receiver<Req> {
        let (tx, rx) = mpsc::channel();
        app.on_key(KeyEvent::from(code), &tx);
        rx
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
