use std::collections::HashMap;
use std::time::Instant;

/// One number to chart, as a backend reads it.
#[derive(Debug, Clone, Copy)]
pub struct Reading {
    /// Names what it is ("qps", "hit"); the screen decides how to title and draw it.
    pub key: &'static str,
    pub raw: Raw,
}

#[derive(Debug, Clone, Copy)]
pub enum Raw {
    /// Cumulative since the server started (`Questions`): charted as a rate per second.
    Counter(u64),
    /// As it is right now (sessions waiting on a lock). NaN: there is nothing to report, such as
    /// the age of the oldest transaction when none is open.
    Gauge(f64),
    /// `part` out of `total`, both cumulative (buffer pool hits out of reads): charted as the
    /// percentage over the last interval, not since the server started.
    Share { part: u64, total: u64 },
}

/// One poll's raw result.
#[derive(Debug, Clone)]
pub struct Sample {
    /// What to chart, most useful first: the screen shows as many as fit.
    pub metrics: Vec<Reading>,
    /// Statements in flight, not counting this tool's own.
    pub threads_running: u32,
    pub threads_connected: u32,
    pub max_connections: u32,
    pub running: Vec<RunningQuery>,
    /// Why `running` may be incomplete: the account can't see other users' sessions.
    pub running_note: Option<String>,
    /// Transactions open on the server, sessions that are idle included: the ones `running`
    /// leaves out, and where a transaction that holds locks for nothing usually is.
    pub transactions: Vec<OpenTransaction>,
    /// Why `transactions` may be incomplete, when the cause is not a quiet server.
    pub transactions_note: Option<String>,
    /// Whether the connection really is encrypted, as the server reports it: with `prefer` that
    /// is only known after the handshake. `None` where there is no connection.
    pub encrypted: Option<bool>,
    pub slow: Vec<SlowQuery>,
    /// Why `slow` is empty when the cause is configuration, not a quiet server.
    pub slow_note: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RunningQuery {
    /// MySQL connection id / Postgres pid.
    pub id: u64,
    pub user: String,
    pub host: String,
    pub db: String,
    pub state: String,
    pub secs: f64,
    pub sql: String,
    /// The server cut the text (e.g. `track_activity_query_size`), so EXPLAIN would be wrong.
    pub truncated: bool,
    pub details: Vec<(String, String)>,
    /// The sessions it waits for.
    pub blocked_by: Vec<u64>,
}

/// A transaction that is open on the server, whether or not its session runs anything now.
#[derive(Debug, Clone)]
pub struct OpenTransaction {
    /// MySQL connection id / Postgres pid.
    pub id: u64,
    pub user: String,
    pub host: String,
    pub db: String,
    /// How long it has been open.
    pub secs: f64,
    /// The session is between statements: it holds what it took and does nothing with it.
    pub idle: bool,
    /// The server's own word for it (`RUNNING`, `LOCK WAIT`, `idle in transaction`).
    pub state: String,
    /// Rows locked and modified so far, where the server counts them.
    pub rows_locked: Option<u64>,
    pub rows_modified: Option<u64>,
    /// The statement it runs now, or the last one it ran. Empty where that is not known.
    pub sql: String,
    pub truncated: bool,
    /// The sessions it waits for.
    pub blocked_by: Vec<u64>,
}

/// Aggregated statistics of one normalized statement (a digest).
#[derive(Debug, Clone)]
pub struct SlowQuery {
    pub digest: String,
    pub schema: String,
    pub sample_sql: String,
    /// The server cut the sample text, so EXPLAIN would not describe the real statement.
    pub truncated: bool,
    pub calls: u64,
    pub total_ms: f64,
    pub avg_ms: f64,
    pub max_ms: f64,
    /// Not every server tracks these.
    pub rows_examined_avg: Option<f64>,
    pub no_index_used: Option<bool>,
}

/// What the user asked to explain.
#[derive(Debug, Clone)]
pub enum ExplainTarget {
    Running(RunningQuery),
    Digest(SlowQuery),
}

/// Names a plan request, so a late answer can't land on another query's details.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanKey {
    Running(u64),
    Digest(String),
    /// A transaction is not explained; the key only says whose details these are.
    Transaction(u64),
}

impl ExplainTarget {
    pub fn key(&self) -> PlanKey {
        match self {
            ExplainTarget::Running(q) => PlanKey::Running(q.id),
            ExplainTarget::Digest(q) => PlanKey::Digest(q.digest.clone()),
        }
    }
}

#[derive(Debug, Clone)]
pub enum Plan {
    /// The plan the server is actually using.
    Text(PlanViews),
    /// Planned again from the statement's text, so it may differ from what is running; `why`
    /// says why the real plan wasn't available.
    Estimated { text: PlanViews, why: String },
    /// The backend cannot produce a trustworthy plan; the string says why.
    Unavailable(String),
}

/// One plan, in each form the server can draw it. There is at least one.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlanViews {
    /// The indented tree of steps: MySQL's `FORMAT=TREE`, Postgres's `EXPLAIN`.
    pub tree: Option<String>,
    /// MySQL's classic table: a row per table read, with its access type, key and rows.
    pub table: Option<PlanTable>,
}

/// Cells of a table, kept apart from how they are laid out: that depends on the room.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanTable {
    pub header: Vec<String>,
    pub rows: Vec<Vec<String>>,
}

/// A plan that is only a tree.
impl From<String> for PlanViews {
    fn from(tree: String) -> Self {
        PlanViews {
            tree: Some(tree),
            table: None,
        }
    }
}

impl From<&str> for PlanViews {
    fn from(tree: &str) -> Self {
        tree.to_string().into()
    }
}

#[cfg(test)]
impl PlanViews {
    /// The tree, or nothing, for tests that look for a step in it.
    pub fn tree_text(&self) -> &str {
        self.tree.as_deref().unwrap_or("")
    }
}

/// Turns raw readings into the numbers that are charted. It remembers the previous poll.
#[derive(Default)]
pub struct Meters {
    prev: HashMap<&'static str, (Instant, Raw)>,
    /// The last share worked out, kept while nothing is happening to take a share of.
    shares: HashMap<&'static str, f64>,
}

impl Meters {
    /// `None` where there is nothing to show yet: the first poll, or a counter that went backwards
    /// (the server restarted). A share holds its last value while there is no activity.
    pub fn read(&mut self, now: Instant, readings: &[Reading]) -> Vec<(&'static str, Option<f64>)> {
        readings
            .iter()
            .map(|r| (r.key, self.value(now, *r)))
            .collect()
    }

    fn value(&mut self, now: Instant, reading: Reading) -> Option<f64> {
        if let Raw::Gauge(value) = reading.raw {
            return value.is_finite().then_some(value);
        }
        let (then, before) = self.prev.insert(reading.key, (now, reading.raw))?;
        let secs = now.duration_since(then).as_secs_f64();
        match (before, reading.raw) {
            (Raw::Counter(a), Raw::Counter(b)) if b >= a && secs > 0.0 => {
                Some((b - a) as f64 / secs)
            }
            (
                Raw::Share {
                    part: p0,
                    total: t0,
                },
                Raw::Share { part, total },
            ) if total > t0 && part >= p0 => {
                let share = (100.0 * (part - p0) as f64 / (total - t0) as f64).min(100.0);
                self.shares.insert(reading.key, share);
                Some(share)
            }
            // Nothing was asked in this interval: the rate is what it was.
            (Raw::Share { total: t0, .. }, Raw::Share { total, .. }) if total == t0 => {
                self.shares.get(reading.key).copied()
            }
            _ => {
                self.shares.remove(reading.key);
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn at(t0: Instant, secs: u64) -> Instant {
        t0 + Duration::from_secs(secs)
    }

    fn counter(value: u64) -> Vec<Reading> {
        vec![Reading {
            key: "c",
            raw: Raw::Counter(value),
        }]
    }

    fn share(part: u64, total: u64) -> Vec<Reading> {
        vec![Reading {
            key: "s",
            raw: Raw::Share { part, total },
        }]
    }

    #[test]
    fn a_counter_is_charted_as_its_change_per_second() {
        let t0 = Instant::now();
        let mut meters = Meters::default();
        assert_eq!(
            meters.read(t0, &counter(1000)),
            [("c", None)],
            "no baseline yet"
        );
        assert_eq!(meters.read(at(t0, 2), &counter(1600)), [("c", Some(300.0))]);
    }

    #[test]
    fn a_counter_that_goes_backwards_is_skipped_then_recovers() {
        let t0 = Instant::now();
        let mut meters = Meters::default();
        meters.read(t0, &counter(5000));
        assert_eq!(
            meters.read(at(t0, 1), &counter(10)),
            [("c", None)],
            "the server restarted"
        );
        assert_eq!(meters.read(at(t0, 2), &counter(110)), [("c", Some(100.0))]);
    }

    #[test]
    fn no_time_passing_gives_no_rate() {
        let t0 = Instant::now();
        let mut meters = Meters::default();
        meters.read(t0, &counter(10));
        assert_eq!(meters.read(t0, &counter(20)), [("c", None)]);
    }

    #[test]
    fn a_share_is_the_percentage_of_the_last_interval_not_of_all_time() {
        let t0 = Instant::now();
        let mut meters = Meters::default();
        // 99% of everything since the server started...
        meters.read(t0, &share(990_000, 1_000_000));
        // ...but in the last interval only 50 of 100 requests were hits.
        assert_eq!(
            meters.read(at(t0, 1), &share(990_050, 1_000_100)),
            [("s", Some(50.0))]
        );
    }

    #[test]
    fn a_share_holds_its_last_value_while_nothing_happens_and_not_past_a_restart() {
        let t0 = Instant::now();
        let mut meters = Meters::default();
        meters.read(t0, &share(10, 20));
        assert_eq!(
            meters.read(at(t0, 1), &share(10, 20)),
            [("s", None)],
            "no share was ever worked out"
        );
        assert_eq!(meters.read(at(t0, 2), &share(15, 30)), [("s", Some(50.0))]);
        assert_eq!(
            meters.read(at(t0, 3), &share(15, 30)),
            [("s", Some(50.0))],
            "idle: as it was"
        );
        assert_eq!(
            meters.read(at(t0, 4), &share(1, 2)),
            [("s", None)],
            "the counters restarted"
        );
        assert_eq!(
            meters.read(at(t0, 5), &share(1, 2)),
            [("s", None)],
            "and the old value is gone"
        );
    }

    #[test]
    fn a_gauge_is_charted_as_it_is_from_the_first_poll() {
        let mut meters = Meters::default();
        let gauge = vec![Reading {
            key: "g",
            raw: Raw::Gauge(7.0),
        }];
        assert_eq!(meters.read(Instant::now(), &gauge), [("g", Some(7.0))]);
    }

    #[test]
    fn a_gauge_with_nothing_to_report_is_no_number() {
        let nothing = vec![Reading {
            key: "g",
            raw: Raw::Gauge(f64::NAN),
        }];
        assert_eq!(
            Meters::default().read(Instant::now(), &nothing),
            [("g", None)]
        );
    }

    #[test]
    fn metrics_do_not_disturb_each_other() {
        let t0 = Instant::now();
        let mut meters = Meters::default();
        let both = |c, g| {
            vec![
                Reading {
                    key: "c",
                    raw: Raw::Counter(c),
                },
                Reading {
                    key: "g",
                    raw: Raw::Gauge(g),
                },
            ]
        };
        meters.read(t0, &both(100, 1.0));
        assert_eq!(
            meters.read(at(t0, 1), &both(150, 9.0)),
            [("c", Some(50.0)), ("g", Some(9.0))]
        );
    }
}
