//! Generated data so the UI can be developed and demoed without a database.

use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Result;

use super::Backend;
use crate::model::{ExplainTarget, Plan, RunningQuery, Sample, SlowQuery};

struct Template {
    user: &'static str,
    host: &'static str,
    db: &'static str,
    state: &'static str,
    sql: &'static str,
    /// Typical run time in seconds.
    secs: f64,
    /// Rows examined per second of run time.
    scan_rate: u64,
    no_index: bool,
    /// Waiting on a row lock held by another thread.
    blocked: bool,
    plan: &'static str,
}

/// The one template whose text is generated and cut like a server with a small
/// `track_activity_query_size` would.
const BIG_IN_LIST: usize = 4;

const PLAN_ORDERS: &str = concat!(
    "-> Limit: 50 row(s)  (cost=18432 rows=50)\n",
    "    -> Nested loop inner join  (cost=18432 rows=81230)\n",
    "        -> Sort: o.created_at DESC  (cost=8123 rows=81230)\n",
    "            -> Filter: (o.status = 'PENDING')  (cost=8123 rows=81230)\n",
    "                -> Table scan on o  (cost=8123 rows=812300)\n",
    "        -> Single-row index lookup on c using PRIMARY (id=o.customer_id)  (cost=0.25 rows=1)",
);

const PLAN_INVENTORY: &str = concat!(
    "-> Update inventory (immediate)\n",
    "    -> Filter: (inventory.qty > 0)  (cost=0.35 rows=1)\n",
    "        -> Index lookup on inventory using idx_sku (sku='A-1029')  (cost=0.35 rows=1)",
);

const PLAN_PAYMENTS: &str = concat!(
    "-> Sort: d\n",
    "    -> Table scan on <temporary>\n",
    "        -> Aggregate using temporary table\n",
    "            -> Filter: (payments.created_at >= TIMESTAMP'2026-01-01 00:00:00')  (cost=412390 rows=1648000)\n",
    "                -> Table scan on payments  (cost=412390 rows=3296000)",
);

const PLAN_SETTLEMENT: &str = concat!(
    "-> Filter: (sum(si.amount) > 1000000)\n",
    "    -> Table scan on <temporary>\n",
    "        -> Aggregate using temporary table\n",
    "            -> Nested loop inner join  (cost=96211 rows=211800)\n",
    "                -> Index range scan on si using idx_settled_at over ('2026-09-01' <= settled_at <= '2026-09-30 23:59:59'), with index condition: (si.status in ('DONE','PARTIAL'))  (cost=22144 rows=211800)\n",
    "                -> Single-row index lookup on m using PRIMARY (merchant_id=si.merchant_id)  (cost=0.25 rows=1)",
);

const PLAN_ACCOUNT_UPDATE: &str = concat!(
    "-> Update accounts\n",
    "    -> Rows fetched before update\n",
    "        -> Single-row index lookup on accounts using PRIMARY (id=42)  (cost=0.35 rows=1)",
);

const PLAN_AUDIT_COUNT: &str = concat!(
    "-> Aggregate: count(0)\n",
    "    -> Filter: (audit_log.user_id = 77)  (cost=540211 rows=538000)\n",
    "        -> Table scan on audit_log  (cost=540211 rows=5380000)",
);

const PLAN_SESSION: &str = concat!(
    "-> Limit: 1 row(s)\n",
    "    -> Single-row index lookup on sessions using uk_token (token='f3a9c1d27b')  (cost=0.35 rows=1)",
);

const PLAN_NOTIFICATIONS: &str = concat!(
    "-> Sort: notifications.created_at DESC  (cost=1620 rows=1530)\n",
    "    -> Filter: (notifications.read_at is null)  (cost=1620 rows=1530)\n",
    "        -> Index lookup on notifications using idx_user (user_id=1042)  (cost=1620 rows=15300)",
);
const PLAN_EVENTS_COUNT: &str = concat!(
    "-> Aggregate: count(0)\n",
    "    -> Filter: ((events.type = 'login') and (events.created_at > TIMESTAMP'2026-10-01 00:00:00'))  (cost=244120 rows=121300)\n",
    "        -> Table scan on events  (cost=244120 rows=2420000)",
);
const PLAN_AUDIT_INSERT: &str =
    "-> Insert into audit_log\n    -> Table scan on <value row constructor>";

const TEMPLATES: &[Template] = &[
    Template {
        user: "app_web",
        host: "10.20.1.14",
        db: "orders",
        state: "Sending data",
        sql: "SELECT o.id, o.status, c.name FROM orders o JOIN customers c ON c.id = o.customer_id WHERE o.status = 'PENDING' ORDER BY o.created_at DESC LIMIT 50",
        secs: 0.6,
        scan_rate: 120_000,
        no_index: true,
        blocked: false,
        plan: PLAN_ORDERS,
    },
    Template {
        user: "app_batch",
        host: "10.20.2.7",
        db: "inventory",
        state: "updating",
        sql: "UPDATE inventory SET qty = qty - 1 WHERE sku = 'A-1029' AND qty > 0",
        secs: 0.3,
        scan_rate: 50,
        no_index: false,
        blocked: false,
        plan: PLAN_INVENTORY,
    },
    Template {
        user: "report_ro",
        host: "10.20.9.31",
        db: "payments",
        state: "Sending data",
        sql: "SELECT DATE(created_at) AS d, status, COUNT(*) AS cnt, SUM(amount) AS total FROM payments WHERE created_at >= '2026-01-01' GROUP BY DATE(created_at), status ORDER BY d",
        secs: 95.0,
        scan_rate: 400_000,
        no_index: true,
        blocked: false,
        plan: PLAN_PAYMENTS,
    },
    Template {
        user: "batch_settle",
        host: "10.20.3.5",
        db: "settlement",
        state: "Creating sort index",
        sql: concat!(
            "/* 월말 정산 배치: 가맹점별 수수료 집계 */\n",
            "SELECT m.merchant_id, m.merchant_name, SUM(si.amount) AS gross, SUM(si.fee) AS fee\n",
            "FROM settlement_items si\n",
            "JOIN merchants m ON m.merchant_id = si.merchant_id\n",
            "WHERE si.settled_at BETWEEN '2026-09-01' AND '2026-09-30 23:59:59'\n",
            "  AND si.status IN ('DONE', 'PARTIAL')\n",
            "GROUP BY m.merchant_id, m.merchant_name\n",
            "HAVING SUM(si.amount) > 1000000",
        ),
        secs: 30.0,
        scan_rate: 70_000,
        no_index: false,
        blocked: false,
        plan: PLAN_SETTLEMENT,
    },
    Template {
        user: "app_api",
        host: "10.20.1.22",
        db: "events",
        state: "Sending data",
        sql: "", // generated, see `big_in_list`
        secs: 6.0,
        scan_rate: 9_000,
        no_index: false,
        blocked: false,
        plan: "",
    },
    Template {
        user: "app_web",
        host: "10.20.1.15",
        db: "accounts",
        state: "updating",
        sql: "UPDATE accounts SET balance = balance + 100 WHERE id = 42",
        secs: 14.0,
        scan_rate: 1,
        no_index: false,
        blocked: true,
        plan: PLAN_ACCOUNT_UPDATE,
    },
    Template {
        user: "app_admin",
        host: "10.20.4.9",
        db: "audit",
        state: "executing",
        sql: "SELECT COUNT(*) FROM audit_log WHERE user_id = 77",
        secs: 8.0,
        scan_rate: 650_000,
        no_index: true,
        blocked: false,
        plan: PLAN_AUDIT_COUNT,
    },
    Template {
        user: "app_web",
        host: "10.20.1.14",
        db: "sessions",
        state: "executing",
        sql: "SELECT * FROM sessions WHERE token = 'f3a9c1d27b' LIMIT 1",
        secs: 0.08,
        scan_rate: 12,
        no_index: false,
        blocked: false,
        plan: PLAN_SESSION,
    },
];

struct SlowTemplate {
    schema: &'static str,
    sql: &'static str,
    calls: u64,
    avg_ms: f64,
    max_ms: f64,
    rows_examined_avg: f64,
    no_index: bool,
    plan: &'static str,
}

const SLOW: &[SlowTemplate] = &[
    SlowTemplate {
        schema: "payments",
        sql: "SELECT DATE(created_at) AS d, status, COUNT(*) AS cnt, SUM(amount) AS total FROM payments WHERE created_at >= '2026-01-01' GROUP BY DATE(created_at), status ORDER BY d",
        calls: 48,
        avg_ms: 91_000.0,
        max_ms: 140_000.0,
        rows_examined_avg: 3_296_000.0,
        no_index: true,
        plan: PLAN_PAYMENTS,
    },
    SlowTemplate {
        schema: "orders",
        sql: "SELECT o.id, o.status, c.name FROM orders o JOIN customers c ON c.id = o.customer_id WHERE o.status = 'PENDING' ORDER BY o.created_at DESC LIMIT 50",
        calls: 120_340,
        avg_ms: 640.0,
        max_ms: 6_200.0,
        rows_examined_avg: 812_300.0,
        no_index: true,
        plan: PLAN_ORDERS,
    },
    SlowTemplate {
        schema: "audit",
        sql: "SELECT COUNT(*) FROM audit_log WHERE user_id = 77",
        calls: 5_210,
        avg_ms: 7_800.0,
        max_ms: 31_000.0,
        rows_examined_avg: 5_380_000.0,
        no_index: true,
        plan: PLAN_AUDIT_COUNT,
    },
    SlowTemplate {
        schema: "settlement",
        sql: "/* 월말 정산 배치 */ SELECT m.merchant_id, SUM(si.amount) FROM settlement_items si JOIN merchants m ON m.merchant_id = si.merchant_id WHERE si.settled_at BETWEEN '2026-09-01' AND '2026-09-30' GROUP BY m.merchant_id",
        calls: 31,
        avg_ms: 28_000.0,
        max_ms: 45_000.0,
        rows_examined_avg: 2_100_000.0,
        no_index: false,
        plan: PLAN_SETTLEMENT,
    },
    SlowTemplate {
        schema: "app",
        sql: "SELECT * FROM notifications WHERE user_id = 1042 AND read_at IS NULL ORDER BY created_at DESC",
        calls: 2_400_000,
        avg_ms: 120.0,
        max_ms: 4_300.0,
        rows_examined_avg: 38_000.0,
        no_index: false,
        plan: PLAN_NOTIFICATIONS,
    },
    SlowTemplate {
        schema: "accounts",
        sql: "UPDATE accounts SET balance = balance + 100 WHERE id = 42",
        calls: 880_000,
        avg_ms: 35.0,
        max_ms: 14_000.0,
        rows_examined_avg: 1.0,
        no_index: false,
        plan: PLAN_ACCOUNT_UPDATE,
    },
    SlowTemplate {
        schema: "events",
        sql: "SELECT COUNT(*) FROM events WHERE type = 'login' AND created_at > '2026-10-01'",
        calls: 90_000,
        avg_ms: 420.0,
        max_ms: 9_000.0,
        rows_examined_avg: 1_200_000.0,
        no_index: true,
        plan: PLAN_EVENTS_COUNT,
    },
    SlowTemplate {
        schema: "audit",
        sql: "INSERT INTO audit_log (user_id, action, created_at) VALUES (77, 'LOGIN', NOW())",
        calls: 4_100_000,
        avg_ms: 8.0,
        max_ms: 3_100.0,
        rows_examined_avg: 0.0,
        no_index: false,
        plan: PLAN_AUDIT_INSERT,
    },
];

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    /// Uniform in `[0, 1)`.
    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
}

struct Active {
    id: u64,
    tpl: usize,
    started: Instant,
    lasts: Duration,
    sql: String,
}

pub struct Fake {
    rng: Rng,
    start: Instant,
    last_poll: Instant,
    questions: u64,
    next_id: u64,
    active: Vec<Active>,
}

impl Fake {
    pub fn new() -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(1, |d| d.as_nanos() as u64);
        Self::seeded(nanos)
    }

    pub fn seeded(seed: u64) -> Self {
        let now = Instant::now();
        let mut fake = Fake {
            rng: Rng(seed | 1),
            start: now,
            last_poll: now,
            questions: 0,
            next_id: 8_800,
            active: Vec::new(),
        };
        // Start with a few queries already in flight so the first screen isn't empty.
        for (tpl, age) in [(2, 20.0), (3, 12.0), (5, 4.0), (6, 3.0), (0, 0.1)] {
            fake.start_query(tpl, age);
        }
        fake
    }

    fn start_query(&mut self, tpl: usize, age_secs: f64) {
        let template = &TEMPLATES[tpl];
        let sql = if tpl == BIG_IN_LIST {
            big_in_list()
        } else {
            template.sql.to_string()
        };
        let lasts = Duration::from_secs_f64(template.secs * (0.7 + 0.6 * self.rng.unit()));
        let now = Instant::now();
        let started = now
            .checked_sub(Duration::from_secs_f64(age_secs))
            .unwrap_or(now);
        self.next_id += 1 + self.rng.next() % 9;
        self.active.push(Active {
            id: self.next_id,
            tpl,
            started,
            lasts,
            sql,
        });
    }
}

fn digest(i: usize) -> String {
    format!(
        "{:032x}",
        (i as u128 + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15_F39C_C060_5CED_C835)
    )
}

/// An `IN (...)` list longer than the 1024 characters a default server keeps.
fn big_in_list() -> String {
    let ids: Vec<String> = (100_000..100_400).map(|n| n.to_string()).collect();
    let full = format!(
        "SELECT e.id, e.type, e.payload FROM events e WHERE e.id IN ({}) ORDER BY e.created_at",
        ids.join(", ")
    );
    full.chars().take(1024).collect()
}

impl Backend for Fake {
    fn label(&self) -> String {
        "demo (generated data)".into()
    }

    fn poll(&mut self) -> Result<Sample> {
        let now = Instant::now();
        let dt = now.duration_since(self.last_poll).as_secs_f64();
        self.last_poll = now;
        let t = now.duration_since(self.start).as_secs_f64();

        // Two waves plus noise, with a small spike every 40s so the graph has something to show.
        // It stays under 2k so the y axis keeps the tighter 2k bucket instead of jumping to 5k.
        let mut qps =
            900.0 + 350.0 * (t / 11.0).sin() + 120.0 * (t / 3.0).sin() + 60.0 * self.rng.unit();
        if (t as u64) % 40 >= 36 {
            qps *= 1.35;
        }
        self.questions += (qps * dt) as u64;

        self.active.retain(|a| now < a.started + a.lasts);
        if self.active.len() < 14 && self.rng.unit() < 0.45 {
            let tpl = self.rng.next() as usize % TEMPLATES.len();
            // Don't pile up copies of the long jobs.
            if TEMPLATES[tpl].secs < 20.0 || !self.active.iter().any(|a| a.tpl == tpl) {
                self.start_query(tpl, 0.0);
            }
        }

        let running: Vec<RunningQuery> = self
            .active
            .iter()
            .map(|a| {
                let tpl = &TEMPLATES[a.tpl];
                let secs = now.duration_since(a.started).as_secs_f64();
                let examined = (secs * tpl.scan_rate as f64) as u64;
                let mut details = vec![
                    ("rows_examined".to_string(), examined.to_string()),
                    ("rows_sent".to_string(), (examined / 400).to_string()),
                    (
                        "lock_time".to_string(),
                        if tpl.blocked {
                            format!("{secs:.1}s")
                        } else {
                            "0.1ms".to_string()
                        },
                    ),
                    (
                        "tmp_disk_tables".to_string(),
                        if tpl.plan.contains("temporary") {
                            "1"
                        } else {
                            "0"
                        }
                        .to_string(),
                    ),
                    (
                        "no_index_used".to_string(),
                        if tpl.no_index { "yes" } else { "no" }.to_string(),
                    ),
                ];
                if tpl.blocked {
                    details.push(("blocked_by".to_string(), "thread 8841".to_string()));
                }
                RunningQuery {
                    id: a.id,
                    user: tpl.user.to_string(),
                    host: tpl.host.to_string(),
                    db: tpl.db.to_string(),
                    state: tpl.state.to_string(),
                    secs,
                    sql: a.sql.clone(),
                    truncated: a.tpl == BIG_IN_LIST,
                    details,
                }
            })
            .collect();

        let slow = SLOW
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let calls = s.calls + (t * s.calls as f64 / 3600.0) as u64;
                SlowQuery {
                    digest: digest(i),
                    schema: s.schema.to_string(),
                    sample_sql: s.sql.to_string(),
                    truncated: false,
                    calls,
                    total_ms: calls as f64 * s.avg_ms,
                    avg_ms: s.avg_ms,
                    max_ms: s.max_ms,
                    rows_examined_avg: Some(s.rows_examined_avg),
                    no_index_used: Some(s.no_index),
                }
            })
            .collect();

        Ok(Sample {
            counter: self.questions,
            rate_label: "qps",
            // The extra one is this tool's own connection.
            threads_running: running.len() as u32 + 1,
            threads_connected: (120.0 + 40.0 * (t / 17.0).sin()) as u32 + running.len() as u32,
            max_connections: 500,
            running,
            running_note: None,
            encrypted: None,
            slow,
            slow_note: None,
        })
    }

    fn explain(&mut self, target: &ExplainTarget) -> Result<Plan> {
        thread::sleep(Duration::from_millis(250)); // a real EXPLAIN is a round trip
        match target {
            ExplainTarget::Running(query) => {
                if query.truncated {
                    return Ok(Plan::Unavailable(format!(
                        "the server cut the text at {} characters, so EXPLAIN would not describe the real statement",
                        query.sql.chars().count()
                    )));
                }
                Ok(match self.active.iter().find(|a| a.id == query.id) {
                    Some(a) => Plan::Text(TEMPLATES[a.tpl].plan.to_string()),
                    None => Plan::Unavailable(
                        "the statement finished before its plan could be read".into(),
                    ),
                })
            }
            ExplainTarget::Digest(slow) => {
                Ok(match (0..SLOW.len()).find(|&i| digest(i) == slow.digest) {
                    Some(i) => Plan::Text(SLOW[i].plan.to_string()),
                    None => Plan::Unavailable("unknown digest".into()),
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_with_queries_in_flight_and_a_slow_list() {
        let sample = Fake::seeded(7).poll().unwrap();
        assert!(sample.running.len() >= 5);
        assert_eq!(sample.slow.len(), SLOW.len());
    }

    #[test]
    fn truncated_text_gets_no_plan() {
        let mut fake = Fake::seeded(7);
        fake.start_query(BIG_IN_LIST, 0.0);
        let sample = fake.poll().unwrap();
        let big = sample.running.iter().find(|q| q.truncated).unwrap();
        assert_eq!(big.sql.chars().count(), 1024);
        assert!(matches!(
            fake.explain(&ExplainTarget::Running(big.clone())).unwrap(),
            Plan::Unavailable(_)
        ));
    }

    #[test]
    fn every_slow_digest_has_a_distinct_plan_source() {
        let mut fake = Fake::seeded(7);
        let sample = fake.poll().unwrap();
        let digests: std::collections::HashSet<_> =
            sample.slow.iter().map(|s| s.digest.clone()).collect();
        assert_eq!(digests.len(), SLOW.len());
        for slow in &sample.slow {
            match fake.explain(&ExplainTarget::Digest(slow.clone())).unwrap() {
                Plan::Text(text) => assert!(text.starts_with("->"), "{text}"),
                other => panic!("no plan for {}: {other:?}", slow.sample_sql),
            }
        }
    }

    #[test]
    fn plan_for_a_finished_statement_says_so() {
        let mut fake = Fake::seeded(7);
        let mut gone = fake.poll().unwrap().running.remove(0);
        gone.id = 1;
        match fake.explain(&ExplainTarget::Running(gone)).unwrap() {
            Plan::Unavailable(why) => assert!(why.contains("finished")),
            other => panic!("expected unavailable, got {other:?}"),
        }
    }
}
