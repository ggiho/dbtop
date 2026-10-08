use std::time::Instant;

/// One poll's raw result.
#[derive(Debug, Clone)]
pub struct Sample {
    /// Cumulative, like `SHOW GLOBAL STATUS`: the graph shows its rate of change.
    pub counter: u64,
    /// What `counter` counts: "qps" for statements, "tps" where only transactions are available.
    pub rate_label: &'static str,
    pub threads_running: u32,
    pub threads_connected: u32,
    pub max_connections: u32,
    pub running: Vec<RunningQuery>,
    /// Why `running` may be incomplete: the account can't see other users' sessions.
    pub running_note: Option<String>,
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
    Text(String),
    /// Planned again from the statement's text, so it may differ from what is running; `why`
    /// says why the real plan wasn't available.
    Estimated { text: String, why: String },
    /// The backend cannot produce a trustworthy plan; the string says why.
    Unavailable(String),
}

/// Turns a cumulative counter into a per-second rate.
#[derive(Default)]
pub struct Rate {
    prev: Option<(Instant, u64)>,
}

impl Rate {
    pub fn update(&mut self, now: Instant, value: u64) -> Option<f64> {
        let (then, before) = self.prev.replace((now, value))?;
        let dt = now.duration_since(then).as_secs_f64();
        // A smaller value means the server restarted and the counter began again.
        if value < before || dt <= 0.0 {
            return None;
        }
        Some((value - before) as f64 / dt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn rate_is_delta_over_elapsed_seconds() {
        let t0 = Instant::now();
        let mut rate = Rate::default();
        assert_eq!(rate.update(t0, 1000), None, "first sample has no baseline");
        assert_eq!(rate.update(t0 + Duration::from_secs(2), 1600), Some(300.0));
    }

    #[test]
    fn rate_skips_a_counter_reset_then_recovers() {
        let t0 = Instant::now();
        let mut rate = Rate::default();
        rate.update(t0, 5000);
        assert_eq!(rate.update(t0 + Duration::from_secs(1), 10), None);
        assert_eq!(rate.update(t0 + Duration::from_secs(2), 110), Some(100.0));
    }

    #[test]
    fn rate_ignores_zero_elapsed_time() {
        let t0 = Instant::now();
        let mut rate = Rate::default();
        rate.update(t0, 10);
        assert_eq!(rate.update(t0, 20), None);
    }
}
