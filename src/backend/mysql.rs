//! MySQL and Aurora MySQL. Developed against 8.0; falls back where older servers differ.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{Context, Result};
use mysql::prelude::Queryable;
use mysql::{Conn, DriverError, OptsBuilder, Row, SslOpts, Value};

use super::{Backend, guard};
use crate::config::{Target, Tls};
use crate::model::{
    ExplainTarget, Plan, PlanTable, PlanViews, Raw, Reading, RunningQuery, Sample, SlowQuery,
};

const MAX_DIGESTS: usize = 100;

/// dbtop's own statements would otherwise top the digest list of a quiet server. These match the
/// server's normalised DIGEST_TEXT, in which names are back-quoted: SELECT `CONNECTION_ID` ( ).
const NOT_OURS: &str = "DIGEST_TEXT NOT LIKE 'SHOW %' \
    AND DIGEST_TEXT NOT LIKE '%performance_schema%' \
    AND DIGEST_TEXT NOT LIKE '%INNODB_TRX%' \
    AND DIGEST_TEXT NOT LIKE '%INNODB_METRICS%' \
    AND DIGEST_TEXT NOT LIKE 'SELECT @@%' \
    AND DIGEST_TEXT NOT LIKE 'SELECT `CONNECTION_ID`%' \
    AND DIGEST_TEXT NOT LIKE 'SET SESSION %' \
    AND DIGEST_TEXT NOT LIKE 'EXPLAIN %' \
    AND DIGEST_TEXT NOT LIKE 'USE %'";

/// The server counters and gauges read in one `SHOW GLOBAL STATUS`.
const STATUS: [&str; 10] = [
    "Questions",
    "Threads_running",
    "Threads_connected",
    "Slow_queries",
    "Innodb_buffer_pool_read_requests",
    "Innodb_buffer_pool_reads",
    "Innodb_row_lock_current_waits",
    "Bytes_received",
    "Bytes_sent",
    "Created_tmp_disk_tables",
];

const PS_OFF: &str = "performance_schema is OFF on this server, so statement digests are not \
    collected. Turn it on (performance_schema=ON, needs a restart) to see slow queries here.";

pub struct MySql {
    target: Target,
    /// `None` after a failure; the next poll reconnects.
    session: Option<Session>,
}

struct Session {
    conn: Conn,
    /// Our own connection id, to leave ourselves out of the list.
    id: u64,
    perf_schema: bool,
    /// How many bytes of a statement performance_schema keeps.
    max_sql_text: usize,
    /// What the server says about this connection, not what was asked for.
    encrypted: bool,
}

impl MySql {
    /// Connects now, so a wrong host or password fails before the screen is taken over.
    pub fn connect(target: &Target) -> Result<MySql> {
        Ok(MySql {
            target: target.clone(),
            session: Some(Session::open(target)?),
        })
    }

    fn session(&mut self) -> Result<&mut Session> {
        if self.session.is_none() {
            self.session = Some(Session::open(&self.target)?);
        }
        self.session.as_mut().context("no session")
    }
}

impl Backend for MySql {
    fn label(&self) -> String {
        self.target.label()
    }

    fn poll(&mut self) -> Result<Sample> {
        let sample = self.session()?.poll();
        if sample.is_err() {
            self.session = None;
        }
        sample
    }

    fn explain(&mut self, target: &ExplainTarget) -> Result<Plan> {
        let session = self.session()?;
        let plan = match target {
            ExplainTarget::Running(query) => session.explain_running(query),
            ExplainTarget::Digest(digest) => session.explain_digest(digest),
        };
        // A refusal from the server is an answer; anything else means the connection is gone.
        if let Err(e) = &plan
            && e.downcast_ref::<mysql::Error>()
                .is_some_and(|e| !matches!(e, mysql::Error::MySqlError(_)))
        {
            self.session = None;
        }
        plan
    }
}

impl Session {
    fn open(target: &Target) -> Result<Session> {
        let mut conn = match connect(target, ssl_opts(target)) {
            // `prefer` is the mysql client's PREFERRED: a server that offers no TLS gets an
            // unencrypted connection. Any other failure, a bad certificate included, is final.
            Err(mysql::Error::DriverError(DriverError::TlsNotSupported)) if target.tls == Tls::Prefer => {
                connect(target, None)
            }
            other => other,
        }
        .with_context(|| match target.tls {
            Tls::Verify => "cannot connect (tls = \"verify\" checks the certificate and that it names the host)",
            _ => "cannot connect",
        })?;
        // Best effort (MySQL 5.7.8+): a hung SELECT must not hold the collector forever.
        let _ = conn.query_drop("SET SESSION MAX_EXECUTION_TIME = 5000");

        let id = conn
            .query_first::<u64, _>("SELECT CONNECTION_ID()")?
            .context("the server returned no connection id")?;
        let perf_schema = conn
            .query_first::<u8, _>("SELECT @@performance_schema")?
            .unwrap_or(0)
            == 1;
        let max_sql_text = conn
            .query_first::<usize, _>("SELECT @@performance_schema_max_sql_text_length")
            .ok()
            .flatten()
            .unwrap_or(1024);
        // Ask the server: `prefer` may have fallen back, and a proxy could have stripped the TLS.
        let encrypted = conn
            .query_first::<(String, String), _>("SHOW SESSION STATUS LIKE 'Ssl_cipher'")?
            .is_some_and(|(_, cipher)| !cipher.is_empty());
        Ok(Session {
            conn,
            id,
            perf_schema,
            max_sql_text,
            encrypted,
        })
    }

    fn poll(&mut self) -> Result<Sample> {
        let names = STATUS.map(|name| format!("'{name}'")).join(", ");
        let status: Vec<(String, String)> = self.conn.query(format!(
            "SHOW GLOBAL STATUS WHERE Variable_name IN ({names})"
        ))?;
        let value = |name: &str| -> Option<u64> {
            status
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .and_then(|(_, v)| v.parse().ok())
        };
        let required = |name: &str| -> Result<u64> {
            value(name).with_context(|| format!("SHOW GLOBAL STATUS has no {name}"))
        };
        // The statement asking is itself running: leave it out, as on the Postgres side.
        let status_running = required("Threads_running")?.saturating_sub(1) as u32;
        let threads_connected = required("Threads_connected")? as u32;
        required("Questions")?;
        let metrics = readings(value, &self.innodb());
        let max_connections = self
            .conn
            .query_first::<u32, _>("SELECT @@max_connections")?
            .unwrap_or(0);

        let (mut running, visible) = self.processlist()?;
        if !running.is_empty() {
            self.enrich(&mut running);
        }
        // Without PROCESS the list holds only our own connection: the other users' queries are
        // missing without an error, which reads as an idle server.
        let running_note = (visible <= 1 && threads_connected >= 3).then(|| {
            "this account sees only its own connection, so other users' queries are missing: grant PROCESS"
                .to_string()
        });
        // The Running tab's own count, so the two agree. The server's counter also holds background
        // threads such as the event scheduler, so on a quiet server it never reads 0; it is what
        // is left where the account cannot see the other sessions.
        let threads_running = if running_note.is_none() {
            running.len() as u32
        } else {
            status_running
        };
        let (slow, slow_note) = self.digests();

        Ok(Sample {
            metrics,
            threads_running,
            threads_connected,
            max_connections,
            running,
            running_note,
            encrypted: Some(self.encrypted),
            slow,
            slow_note,
        })
    }

    /// What InnoDB says beyond the status counters. A part is `None` where the account may not
    /// look or the server does not keep it, and its graph is then left out.
    fn innodb(&mut self) -> Innodb {
        let (deadlocks, history_list) = self.innodb_metrics();
        Innodb {
            oldest_transaction: self.oldest_transaction(),
            deadlocks,
            history_list,
        }
    }

    /// `lock_deadlocks` and `trx_rseg_history_len`. MySQL has no status variable for either, and
    /// keeps both in INNODB_METRICS, switched on unless someone switched them off.
    fn innodb_metrics(&mut self) -> (Option<u64>, Option<u64>) {
        let rows: Vec<(String, i64)> = self
            .conn
            .query(
                "SELECT NAME, `COUNT` FROM information_schema.INNODB_METRICS \
                 WHERE NAME IN ('lock_deadlocks', 'trx_rseg_history_len') AND STATUS = 'enabled'",
            )
            .unwrap_or_default();
        let get = |name: &str| {
            rows.iter()
                .find(|(n, _)| n == name)
                .map(|(_, count)| (*count).max(0) as u64)
        };
        (get("lock_deadlocks"), get("trx_rseg_history_len"))
    }

    /// How long the longest open transaction has been open, in seconds: the one that holds back
    /// purging and keeps locks. NaN when none is open; `None` where the account may not look at
    /// InnoDB's transactions.
    fn oldest_transaction(&mut self) -> Option<f64> {
        self.conn
            .query_first::<Option<i64>, _>(
                "SELECT MAX(TIMESTAMPDIFF(SECOND, trx_started, NOW())) FROM information_schema.INNODB_TRX",
            )
            .ok()
            .flatten()
            .map(|age| age.map_or(f64::NAN, |secs| secs.max(0) as f64))
    }

    /// Statements in flight. `SHOW FULL PROCESSLIST` works on every server, with or without
    /// performance_schema.
    fn processlist(&mut self) -> Result<(Vec<RunningQuery>, usize)> {
        let rows: Vec<Row> = self.conn.query("SHOW FULL PROCESSLIST")?;
        let visible = rows.len();
        Ok((
            rows.into_iter()
                .filter_map(|row| process_from_row(row, self.id))
                .collect(),
            visible,
        ))
    }

    /// Adds what performance_schema and InnoDB know about each statement. Each source is optional:
    /// an older server, a missing grant or a disabled schema just leaves the details shorter.
    fn enrich(&mut self, running: &mut [RunningQuery]) {
        let ids = running
            .iter()
            .map(|q| q.id.to_string())
            .collect::<Vec<_>>()
            .join(",");
        let mut details: HashMap<u64, Vec<(String, String)>> = HashMap::new();
        let mut elapsed: HashMap<u64, f64> = HashMap::new();

        if self.perf_schema {
            let rows = self.conn.query::<(u64, Option<u64>, u64, u64, u64, u64, u64, u64), _>(format!(
                "SELECT t.PROCESSLIST_ID, s.TIMER_WAIT, s.ROWS_EXAMINED, s.ROWS_SENT, s.LOCK_TIME, \
                        s.CREATED_TMP_TABLES, s.CREATED_TMP_DISK_TABLES, s.NO_INDEX_USED + s.NO_GOOD_INDEX_USED \
                 FROM performance_schema.events_statements_current s \
                 JOIN performance_schema.threads t ON t.THREAD_ID = s.THREAD_ID \
                 WHERE t.PROCESSLIST_ID IN ({ids}) AND s.END_EVENT_ID IS NULL"
            ));
            for (id, timer, examined, sent, lock, tmp, tmp_disk, no_index) in
                rows.unwrap_or_default()
            {
                // For a statement still running, TIMER_WAIT is the time elapsed so far,
                // in picoseconds: finer than the process list's whole seconds.
                if let Some(timer) = timer {
                    elapsed.insert(id, timer as f64 / 1e12);
                }
                details.entry(id).or_default().extend([
                    ("rows_examined".to_string(), examined.to_string()),
                    ("rows_sent".to_string(), sent.to_string()),
                    (
                        "lock_time".to_string(),
                        format!("{:.1}ms", lock as f64 / 1e9),
                    ),
                    (
                        "tmp_tables".to_string(),
                        format!("{tmp} ({tmp_disk} on disk)"),
                    ),
                    (
                        "no_index_used".to_string(),
                        if no_index > 0 { "yes" } else { "no" }.to_string(),
                    ),
                ]);
            }
        }

        let trx = self.conn.query::<(u64, String, i64, u64, u64), _>(format!(
            "SELECT trx_mysql_thread_id, trx_state, TIMESTAMPDIFF(SECOND, trx_started, NOW()), \
                    trx_rows_locked, trx_rows_modified \
             FROM information_schema.INNODB_TRX WHERE trx_mysql_thread_id IN ({ids})"
        ));
        for (id, state, age, locked, modified) in trx.unwrap_or_default() {
            details.entry(id).or_default().extend([
                ("transaction".to_string(), format!("{state}, {age}s old")),
                (
                    "trx_rows".to_string(),
                    format!("{locked} locked, {modified} modified"),
                ),
            ]);
        }

        if self.perf_schema {
            // MySQL 8.0 only; elsewhere the table is missing and this is skipped.
            let waits = self.conn.query::<(u64, u64), _>(
                "SELECT waiting.PROCESSLIST_ID, blocking.PROCESSLIST_ID \
                 FROM performance_schema.data_lock_waits w \
                 JOIN performance_schema.threads waiting ON waiting.THREAD_ID = w.REQUESTING_THREAD_ID \
                 JOIN performance_schema.threads blocking ON blocking.THREAD_ID = w.BLOCKING_THREAD_ID",
            );
            let mut blockers: HashMap<u64, Vec<u64>> = HashMap::new();
            for (waiting, blocking) in waits.unwrap_or_default() {
                let list = blockers.entry(waiting).or_default();
                if !list.contains(&blocking) {
                    list.push(blocking);
                }
            }
            for (id, list) in blockers {
                let list = list
                    .iter()
                    .map(|b| format!("thread {b}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                details
                    .entry(id)
                    .or_default()
                    .push(("blocked_by".to_string(), list));
            }
        }

        for query in running {
            if let Some(secs) = elapsed.get(&query.id) {
                query.secs = *secs;
            }
            if let Some(found) = details.remove(&query.id) {
                query.details = found;
            }
        }
    }

    /// The statements that have used the most time in total, from performance_schema.
    fn digests(&mut self) -> (Vec<SlowQuery>, Option<String>) {
        if !self.perf_schema {
            return (Vec::new(), Some(PS_OFF.to_string()));
        }
        let rows = self.conn.query::<Row, _>(format!(
            "SELECT SCHEMA_NAME, DIGEST, QUERY_SAMPLE_TEXT, COUNT_STAR, SUM_TIMER_WAIT, MAX_TIMER_WAIT, \
                    SUM_ROWS_EXAMINED, SUM_NO_INDEX_USED + SUM_NO_GOOD_INDEX_USED \
             FROM performance_schema.events_statements_summary_by_digest \
             WHERE DIGEST IS NOT NULL AND COUNT_STAR > 0 AND {NOT_OURS} \
             ORDER BY SUM_TIMER_WAIT DESC LIMIT {MAX_DIGESTS}"
        ));
        match rows {
            Ok(rows) => {
                let max_text = self.max_sql_text;
                (
                    rows.into_iter()
                        .filter_map(|row| digest_from_row(row, max_text))
                        .collect(),
                    None,
                )
            }
            Err(e) => (
                Vec::new(),
                Some(format!("cannot read the statement digests: {e}")),
            ),
        }
    }

    /// Whether `query` is still what its connection runs: a connection id outlives its statements.
    fn standing(&mut self, query: &RunningQuery) -> Result<Standing> {
        let now = self.processlist()?.0.into_iter().find(|q| q.id == query.id);
        Ok(Standing::of(now.as_ref(), query))
    }

    fn explain_running(&mut self, query: &RunningQuery) -> Result<Plan> {
        match self.standing(query)? {
            Standing::Finished => {
                return Ok(Plan::Unavailable(
                    "the statement finished before its plan could be read".into(),
                ));
            }
            Standing::Other => {
                return Ok(Plan::Unavailable(
                    "the connection has moved on to another statement".into(),
                ));
            }
            Standing::Same => {}
        }

        // The plan the server is really using. Only an integer goes into this statement.
        let refused = match self.run_explain(&format!("FOR CONNECTION {}", query.id), false) {
            Ok(Some(views)) => {
                // The tree and the table are two statements, one after the other: had the
                // connection taken up another statement in between, one of them would describe it.
                if self.standing(query)? == Standing::Other {
                    return Ok(Plan::Unavailable(
                        "the connection moved on to another statement while its plan was being read"
                            .into(),
                    ));
                }
                return Ok(Plan::Text(views));
            }
            Ok(None) => {
                return Ok(Plan::Unavailable(
                    "the connection is not executing a statement".into(),
                ));
            }
            Err(mysql::Error::MySqlError(refused)) => refused,
            Err(e) => return Err(e.into()),
        };
        // MySQL checks the privileges of the explained statement's owner too (bug #95850), so an
        // account with little more than PROCESS is refused. Plan the statement's text instead,
        // and say so.
        let why = format!(
            "EXPLAIN FOR CONNECTION was refused (ERROR {}: {}), so this is the statement's text planned again",
            refused.code, refused.message
        );
        if query.truncated {
            return Ok(Plan::Unavailable(format!(
                "{why}, and the statement's text was cut"
            )));
        }
        self.explain_text(&query.sql, &query.db, why)
    }

    fn explain_digest(&mut self, digest: &SlowQuery) -> Result<Plan> {
        if digest.truncated {
            return Ok(Plan::Unavailable(format!(
                "performance_schema kept only the first {} bytes of this statement, so EXPLAIN would not describe it",
                self.max_sql_text
            )));
        }
        let why =
            "no statement is running; this is the digest's sample text planned now".to_string();
        self.explain_text(&digest.sample_sql, &digest.schema, why)
    }

    /// Plans `sql` again. The text came from the server, so it is only explained if it is one
    /// read statement; the session is read-only and the account holds no write privileges either.
    fn explain_text(&mut self, sql: &str, schema: &str, why: String) -> Result<Plan> {
        if let Err(not) = guard::explainable(sql) {
            return Ok(Plan::Unavailable(format!(
                "{why}; the text is not explained because {not}"
            )));
        }
        if guard::writes_data(sql) {
            return Ok(Plan::Unavailable(format!(
                "{why}; INSERT/UPDATE/DELETE text is not explained, because MySQL asks for write privileges even to EXPLAIN it"
            )));
        }
        if !schema.is_empty() {
            self.conn
                .query_drop(format!("USE {}", quote_ident(schema)))?;
        }
        match self.run_explain(sql, true) {
            Ok(Some(views)) => Ok(Plan::Estimated { text: views, why }),
            Ok(None) => Ok(Plan::Unavailable(format!(
                "{why}; the server returned no plan"
            ))),
            Err(mysql::Error::MySqlError(e)) => Ok(Plan::Unavailable(format!(
                "{why}; explaining the text failed (ERROR {}: {})",
                e.code, e.message
            ))),
            Err(e) => Err(e.into()),
        }
    }

    /// `target` is `FOR CONNECTION <id>` or a statement's text.
    ///
    /// Text goes through the prepared-statement protocol (`prepared`), which carries exactly one
    /// statement whatever the text contains. `FOR CONNECTION` is not allowed there (ERROR 1295),
    /// but it is built from an integer, so the plain protocol is safe for it.
    ///
    /// The plan is asked for in both of MySQL's forms, the tree and the classic table. Either may
    /// be missing: MySQL 8.0 cannot draw an UPDATE or DELETE as a tree, and says so in the result
    /// instead of failing, and the tree needs 8.0.16.
    fn run_explain(&mut self, target: &str, prepared: bool) -> mysql::Result<Option<PlanViews>> {
        let mut rows = |sql: String| -> mysql::Result<Vec<Row>> {
            if prepared {
                self.conn.exec(sql, ())
            } else {
                self.conn.query(sql)
            }
        };
        let tree = match rows(format!("EXPLAIN FORMAT=TREE {target}")) {
            Ok(found) => tree_text(found)
                .filter(|text| !text.contains("<not executable by iterator executor>")),
            Err(mysql::Error::MySqlError(e)) if e.code == 1064 => None,
            Err(e) => return Err(e),
        };
        // Named, so that a server whose `explain_format` was changed still answers with the table.
        let table = plan_table(rows(format!("EXPLAIN FORMAT=TRADITIONAL {target}"))?);
        Ok((tree.is_some() || table.is_some()).then_some(PlanViews { tree, table }))
    }
}

/// What InnoDB reports outside `SHOW GLOBAL STATUS`.
#[derive(Default)]
struct Innodb {
    /// Seconds the longest open transaction has been open; NaN when none is.
    oldest_transaction: Option<f64>,
    /// Deadlocks since the server began counting them.
    deadlocks: Option<u64>,
    /// History list length: undo records waiting to be purged.
    history_list: Option<u64>,
}

/// What to chart, most useful first. A metric whose counters this server does not have (MariaDB,
/// a managed variant) or its account may not read is left out rather than shown as zero.
fn readings(value: impl Fn(&str) -> Option<u64>, innodb: &Innodb) -> Vec<Reading> {
    let counter = |key, name: &str| {
        value(name).map(|v| Reading {
            key,
            raw: Raw::Counter(v),
        })
    };
    let sum = |key, names: &[&str]| {
        let parts: Option<Vec<u64>> = names.iter().map(|name| value(name)).collect();
        parts.map(|parts| Reading {
            key,
            raw: Raw::Counter(parts.iter().sum()),
        })
    };
    // Reads that the buffer pool answered, out of all reads.
    let hit = value("Innodb_buffer_pool_read_requests")
        .zip(value("Innodb_buffer_pool_reads"))
        .map(|(requests, from_disk)| Reading {
            key: "hit",
            raw: Raw::Share {
                part: requests.saturating_sub(from_disk),
                total: requests,
            },
        });
    let gauge = |key, name: &str| {
        value(name).map(|v| Reading {
            key,
            raw: Raw::Gauge(v as f64),
        })
    };
    [
        counter("qps", "Questions"),
        counter("slow", "Slow_queries"),
        hit,
        gauge("lock_waits", "Innodb_row_lock_current_waits"),
        innodb.deadlocks.map(|count| Reading {
            key: "deadlocks",
            raw: Raw::Counter(count),
        }),
        innodb.oldest_transaction.map(|secs| Reading {
            key: "oldest_xact",
            raw: Raw::Gauge(secs),
        }),
        innodb.history_list.map(|length| Reading {
            key: "hll",
            raw: Raw::Gauge(length as f64),
        }),
        sum("net", &["Bytes_received", "Bytes_sent"]),
        counter("tmp_disk", "Created_tmp_disk_tables"),
    ]
    .into_iter()
    .flatten()
    .collect()
}

fn connect(target: &Target, ssl: Option<SslOpts>) -> mysql::Result<Conn> {
    let opts = OptsBuilder::new()
        .ip_or_hostname(Some(target.host.clone()))
        .tcp_port(target.port)
        .user(Some(target.user.clone()))
        .pass(target.password.as_ref().map(|p| p.expose().to_string()))
        .db_name(target.database.clone())
        .prefer_socket(false)
        .ssl_opts(ssl)
        .tcp_connect_timeout(Some(Duration::from_secs(5)))
        .read_timeout(Some(Duration::from_secs(10)))
        .write_timeout(Some(Duration::from_secs(10)))
        .tcp_keepalive_time_ms(Some(30_000))
        .connect_attrs(Some(HashMap::from([("program_name", "dbtop")])))
        // Nothing dbtop sends should ever write.
        .init(vec!["SET SESSION TRANSACTION READ ONLY"]);
    Conn::new(opts)
}

/// `None` is a plain connection.
fn ssl_opts(target: &Target) -> Option<SslOpts> {
    match target.tls {
        Tls::Off => None,
        // Encrypt without asking who answered: the mysql client's PREFERRED, and so dbq's.
        Tls::Prefer | Tls::Require => Some(
            SslOpts::default()
                .with_danger_accept_invalid_certs(true)
                .with_danger_skip_domain_validation(true),
        ),
        Tls::Verify => {
            let opts = SslOpts::default();
            Some(match &target.tls_ca {
                Some(ca) => opts.with_root_cert_path(Some(ca.clone())),
                None => opts,
            })
        }
    }
}

fn process_from_row(mut row: Row, own_id: u64) -> Option<RunningQuery> {
    let id: u64 = row.take_opt("Id")?.ok()?;
    let user = string(&mut row, "User").unwrap_or_default();
    let command = string(&mut row, "Command").unwrap_or_default();
    let sql = string(&mut row, "Info")?;
    if id == own_id || command == "Sleep" || command == "Daemon" || user == "system user" {
        return None;
    }
    let host = string(&mut row, "Host").unwrap_or_default();
    Some(RunningQuery {
        id,
        user,
        host: host
            .rsplit_once(':')
            .map_or(host.clone(), |(host, _port)| host.to_string()),
        db: string(&mut row, "db").unwrap_or_default(),
        state: string(&mut row, "State")
            .filter(|s| !s.is_empty())
            .unwrap_or(command),
        secs: row
            .take_opt::<Option<i64>, _>("Time")
            .and_then(Result::ok)
            .flatten()
            .unwrap_or(0) as f64,
        sql,
        truncated: false,
        details: Vec::new(),
    })
}

fn digest_from_row(mut row: Row, max_sql_text: usize) -> Option<SlowQuery> {
    let count: u64 = row.take_opt("COUNT_STAR")?.ok()?;
    let total: u64 = row.take_opt("SUM_TIMER_WAIT")?.ok()?;
    let max: u64 = row.take_opt("MAX_TIMER_WAIT")?.ok()?;
    let examined: u64 = row.take_opt("SUM_ROWS_EXAMINED")?.ok()?;
    let no_index: u64 = row.take_opt(7)?.ok()?;
    let sample = string(&mut row, "QUERY_SAMPLE_TEXT").unwrap_or_default();
    let total_ms = total as f64 / 1e9; // picoseconds
    Some(SlowQuery {
        digest: string(&mut row, "DIGEST")?,
        schema: string(&mut row, "SCHEMA_NAME").unwrap_or_default(),
        truncated: sample.len() >= max_sql_text,
        sample_sql: sample,
        calls: count,
        total_ms,
        avg_ms: total_ms / count.max(1) as f64,
        max_ms: max as f64 / 1e9,
        rows_examined_avg: Some(examined as f64 / count.max(1) as f64),
        no_index_used: Some(no_index > 0),
    })
}

/// A text column. Statements can hold bytes that aren't valid UTF-8, so decode leniently.
fn string(row: &mut Row, column: &str) -> Option<String> {
    row.take_opt::<Option<Vec<u8>>, _>(column)?
        .ok()?
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
}

fn quote_ident(name: &str) -> String {
    format!("`{}`", name.replace('`', "``"))
}

/// `EXPLAIN FORMAT=TREE` answers with the whole tree as text in one column.
fn tree_text(rows: Vec<Row>) -> Option<String> {
    let lines: Vec<String> = rows
        .into_iter()
        .filter_map(|row| row.unwrap().into_iter().next().map(|v| cell(&v)))
        .collect();
    (!lines.is_empty()).then(|| lines.join("\n").trim_end().to_string())
}

/// The classic tabular `EXPLAIN`: the screen lays it out, to fit the room it has.
fn plan_table(rows: Vec<Row>) -> Option<PlanTable> {
    let header: Vec<String> = rows
        .first()?
        .columns_ref()
        .iter()
        .map(|c| c.name_str().into_owned())
        .collect();
    let rows = rows
        .into_iter()
        .map(|row| row.unwrap().iter().map(cell).collect())
        .collect();
    Some(PlanTable { header, rows })
}

/// What a connection is doing now, compared with the statement a user opened.
#[derive(Debug, PartialEq)]
enum Standing {
    Same,
    /// It runs nothing any more.
    Finished,
    /// It runs another statement.
    Other,
}

impl Standing {
    fn of(now: Option<&RunningQuery>, opened: &RunningQuery) -> Standing {
        match now {
            None => Standing::Finished,
            Some(now) if now.sql != opened.sql => Standing::Other,
            Some(_) => Standing::Same,
        }
    }
}

fn cell(value: &Value) -> String {
    match value {
        Value::NULL => "NULL".to_string(),
        Value::Bytes(bytes) => String::from_utf8_lossy(bytes).into_owned(),
        Value::Int(n) => n.to_string(),
        Value::UInt(n) => n.to_string(),
        Value::Float(n) => n.to_string(),
        Value::Double(n) => n.to_string(),
        other => format!("{other:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_tls(tls: Tls, ca: Option<&str>) -> Target {
        Target {
            tls,
            tls_ca: ca.map(std::path::PathBuf::from),
            ..Target::dev(crate::config::Kind::Mysql, 3306, "u", "p")
        }
    }

    fn status(pairs: &[(&str, u64)]) -> impl Fn(&str) -> Option<u64> {
        let map: HashMap<String, u64> = pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect();
        move |name| map.get(name).copied()
    }

    fn keys(readings: &[Reading]) -> Vec<&'static str> {
        readings.iter().map(|r| r.key).collect()
    }

    #[test]
    fn every_status_variable_the_readings_read_is_asked_for() {
        let read = std::cell::RefCell::new(Vec::<String>::new());
        readings(
            |name| {
                read.borrow_mut().push(name.to_string());
                Some(1)
            },
            &Innodb::default(),
        );
        let read = read.into_inner();
        assert!(read.len() >= 8, "{read:?}");
        for name in &read {
            assert!(
                STATUS.contains(&name.as_str()),
                "{name} is read but never requested"
            );
        }
    }

    const ALL: [(&str, u64); 8] = [
        ("Questions", 1000),
        ("Slow_queries", 3),
        ("Innodb_buffer_pool_read_requests", 10_000),
        ("Innodb_buffer_pool_reads", 100),
        ("Innodb_row_lock_current_waits", 2),
        ("Bytes_received", 700),
        ("Bytes_sent", 300),
        ("Created_tmp_disk_tables", 1),
    ];

    /// Everything the account may read from InnoDB.
    fn everything() -> Innodb {
        Innodb {
            oldest_transaction: Some(12.0),
            deadlocks: Some(3),
            history_list: Some(900),
        }
    }

    #[test]
    fn the_metrics_come_most_useful_first() {
        assert_eq!(
            keys(&readings(status(&ALL), &everything())),
            [
                "qps",
                "slow",
                "hit",
                "lock_waits",
                "deadlocks",
                "oldest_xact",
                "hll",
                "net",
                "tmp_disk"
            ]
        );
    }

    #[test]
    fn deadlocks_are_counted_and_the_history_list_is_a_level() {
        let readings = readings(status(&ALL), &everything());
        let raw = |key: &str| readings.iter().find(|r| r.key == key).unwrap().raw;
        assert!(
            matches!(raw("deadlocks"), Raw::Counter(3)),
            "charted as a rate"
        );
        assert!(
            matches!(raw("hll"), Raw::Gauge(g) if g == 900.0),
            "charted as it is"
        );
    }

    #[test]
    fn related_counters_are_added_up_and_the_hit_rate_is_a_share() {
        let readings = readings(status(&ALL), &everything());
        let raw = |key: &str| readings.iter().find(|r| r.key == key).unwrap().raw;
        assert!(matches!(raw("net"), Raw::Counter(1000)));
        assert!(matches!(
            raw("hit"),
            Raw::Share {
                part: 9900,
                total: 10_000
            }
        ));
        assert!(matches!(raw("lock_waits"), Raw::Gauge(g) if g == 2.0));
    }

    #[test]
    fn a_variable_the_server_lacks_drops_its_metric_instead_of_showing_zero() {
        let without = |gone: &str| {
            let rest: Vec<_> = ALL.iter().filter(|(k, _)| *k != gone).copied().collect();
            keys(&readings(status(&rest), &everything()))
        };
        assert!(!without("Innodb_buffer_pool_reads").contains(&"hit"));
        assert!(
            !without("Bytes_sent").contains(&"net"),
            "half a sum would be a wrong number"
        );
        assert!(
            without("Slow_queries").contains(&"qps"),
            "and the others are unaffected"
        );
        let blind = keys(&readings(status(&ALL), &Innodb::default()));
        for key in ["oldest_xact", "deadlocks", "hll"] {
            assert!(!blind.contains(&key), "{key}: no InnoDB access");
        }
    }

    #[test]
    fn more_disk_reads_than_requests_cannot_make_a_negative_share() {
        let odd = [
            ("Innodb_buffer_pool_read_requests", 5),
            ("Innodb_buffer_pool_reads", 9),
        ];
        let readings = readings(status(&odd), &Innodb::default());
        assert!(matches!(readings[0].raw, Raw::Share { part: 0, total: 5 }));
    }

    #[test]
    fn every_key_has_a_place_on_the_screen() {
        for key in keys(&readings(status(&ALL), &everything())) {
            assert!(
                crate::ui::metrics::spec(key).is_some(),
                "{key} has no title and unit"
            );
        }
    }

    fn opened(sql: &str) -> RunningQuery {
        RunningQuery {
            id: 7,
            user: String::new(),
            host: String::new(),
            db: String::new(),
            state: String::new(),
            secs: 0.0,
            sql: sql.to_string(),
            truncated: false,
            details: Vec::new(),
        }
    }

    #[test]
    fn a_connection_is_judged_by_the_statement_it_runs_now() {
        let query = opened("SELECT 1");
        assert_eq!(
            Standing::of(Some(&opened("SELECT 1")), &query),
            Standing::Same
        );
        assert_eq!(
            Standing::of(Some(&opened("SELECT 2")), &query),
            Standing::Other
        );
        assert_eq!(Standing::of(None, &query), Standing::Finished);
    }

    #[test]
    fn off_means_no_tls_at_all() {
        assert!(ssl_opts(&with_tls(Tls::Off, None)).is_none());
    }

    #[test]
    fn prefer_and_require_encrypt_without_checking_the_certificate() {
        for tls in [Tls::Prefer, Tls::Require] {
            let opts = ssl_opts(&with_tls(tls, None)).expect("TLS is on");
            assert!(opts.accept_invalid_certs(), "{tls:?}");
            assert!(opts.skip_domain_validation(), "{tls:?}");
        }
    }

    #[test]
    fn verify_checks_the_certificate_and_the_host_and_trusts_the_given_ca() {
        let plain = ssl_opts(&with_tls(Tls::Verify, None)).expect("TLS is on");
        assert!(!plain.accept_invalid_certs() && !plain.skip_domain_validation());
        assert!(plain.root_cert_path().is_none());

        let with_ca = ssl_opts(&with_tls(Tls::Verify, Some("/certs/ca.pem"))).unwrap();
        assert!(!with_ca.accept_invalid_certs() && !with_ca.skip_domain_validation());
        assert_eq!(
            with_ca.root_cert_path(),
            Some(std::path::Path::new("/certs/ca.pem"))
        );
    }

    #[test]
    fn identifiers_are_quoted_against_backticks() {
        assert_eq!(quote_ident("shop"), "`shop`");
        assert_eq!(quote_ident("we`ird"), "`we``ird`");
    }
}

/// Against the throwaway databases in dev/compose.yaml, with a workload account (`app`) running
/// statements while dbtop's monitoring account (`dbtop`) or `root` looks on:
///
/// ```text
/// docker compose -f dev/compose.yaml up -d --wait
/// cargo test mysql::live -- --ignored
/// ```
#[cfg(test)]
mod live {
    use std::sync::mpsc;
    use std::thread::{self, JoinHandle};
    use std::time::{Duration, Instant};

    use super::*;
    use crate::config::Kind;

    const PORT: u16 = 13306;
    const PORT_NO_PS: u16 = 13307;

    fn monitor(user: &str, password: &str, port: u16) -> MySql {
        MySql::connect(&Target::dev(Kind::Mysql, port, user, password))
            .expect("is the database from dev/compose.yaml up?")
    }

    /// What dbtop is meant to run as: it can see everything and change nothing.
    fn least() -> MySql {
        monitor("dbtop", "dbtop", PORT)
    }

    /// An account that MySQL lets run EXPLAIN FOR CONNECTION on anyone's thread.
    fn admin() -> MySql {
        monitor("root", "root", PORT)
    }

    fn app_conn(port: u16) -> Conn {
        let opts = OptsBuilder::new()
            .ip_or_hostname(Some("127.0.0.1"))
            .tcp_port(port)
            .user(Some("app"))
            .pass(Some("app"))
            .prefer_socket(false);
        Conn::new(opts).expect("is the database from dev/compose.yaml up?")
    }

    /// Runs `sql` as the workload account on its own connection.
    fn work_on(port: u16, sql: &str) -> JoinHandle<()> {
        let sql = sql.to_string();
        thread::spawn(move || app_conn(port).query_drop(&sql).unwrap())
    }

    fn work(sql: &str) -> JoinHandle<()> {
        work_on(PORT, sql)
    }

    #[track_caller]
    fn wait_for<T>(mut f: impl FnMut() -> Option<T>) -> T {
        let give_up = Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(found) = f() {
                return found;
            }
            assert!(Instant::now() < give_up, "timed out waiting");
            thread::sleep(Duration::from_millis(100));
        }
    }

    fn running(mon: &mut MySql, marker: &str) -> RunningQuery {
        wait_for(|| {
            mon.poll()
                .unwrap()
                .running
                .into_iter()
                .find(|q| q.sql.contains(marker))
        })
    }

    /// One cell of the classic table, by column name.
    fn cell_of<'a>(table: &'a PlanTable, row: usize, column: &str) -> &'a str {
        let at = table
            .header
            .iter()
            .position(|h| h == column)
            .unwrap_or_else(|| panic!("no column {column} in {:?}", table.header));
        &table.rows[row][at]
    }

    fn digest(mon: &mut MySql, needle: &str) -> SlowQuery {
        wait_for(|| {
            mon.poll()
                .unwrap()
                .slow
                .into_iter()
                .find(|d| d.sample_sql.contains(needle))
        })
    }

    fn row_count() -> u64 {
        app_conn(PORT)
            .query_first("SELECT COUNT(*) FROM shop.orders")
            .unwrap()
            .unwrap()
    }

    #[test]
    #[ignore = "needs dev/compose.yaml"]
    fn the_running_count_is_the_running_list_not_the_servers_thread_counter() {
        // `Threads_running` also holds the asking statement and the event scheduler.
        let job = work("SELECT /* live-running-count */ SLEEP(3) FROM shop.orders LIMIT 1");
        let mut mon = least();
        let sample = wait_for(|| {
            let sample = mon.poll().unwrap();
            sample
                .running
                .iter()
                .any(|q| q.sql.contains("live-running-count"))
                .then_some(sample)
        });
        assert!(sample.running_note.is_none());
        assert_eq!(sample.threads_running as usize, sample.running.len());
        job.join().unwrap();
    }

    #[test]
    #[ignore = "needs dev/compose.yaml"]
    fn a_running_statement_shows_up_with_details_and_its_real_plan() {
        let job = work("SELECT /* live1 */ SLEEP(4) FROM shop.orders WHERE status = 'NEW' LIMIT 1");
        let mut mon = admin();
        let q = running(&mut mon, "live1");

        assert_eq!(q.user, "app");
        assert!(
            !q.host.contains(':'),
            "the client port is dropped: {}",
            q.host
        );
        assert!(
            q.secs < 5.0 && q.secs.fract() != 0.0,
            "sub-second time from performance_schema: {}",
            q.secs
        );
        assert!(
            q.details
                .iter()
                .any(|(k, v)| k == "no_index_used" && v == "yes"),
            "{:?}",
            q.details
        );
        assert!(
            q.details.iter().any(|(k, _)| k == "rows_examined"),
            "{:?}",
            q.details
        );
        match mon.explain(&ExplainTarget::Running(q)).unwrap() {
            Plan::Text(views) => {
                assert!(
                    views.tree_text().contains("Table scan on orders"),
                    "{views:?}"
                );
                let table = views.table.expect("the classic table comes with the tree");
                assert_eq!(cell_of(&table, 0, "table"), "orders", "{table:?}");
                assert_eq!(cell_of(&table, 0, "type"), "ALL", "{table:?}");
                assert_eq!(cell_of(&table, 0, "select_type"), "SIMPLE", "{table:?}");
            }
            other => panic!("an admin account should get the real plan, got {other:?}"),
        }
        job.join().unwrap();
    }

    #[test]
    #[ignore = "needs dev/compose.yaml"]
    fn a_least_privilege_account_gets_an_estimate_and_is_told_why() {
        let job =
            work("SELECT /* live2 */ SLEEP(4) FROM shop.orders WHERE status = 'PAID' LIMIT 1");
        let mut mon = least();
        let q = running(&mut mon, "live2");

        match mon.explain(&ExplainTarget::Running(q)).unwrap() {
            Plan::Estimated { text, why } => {
                assert!(
                    why.contains("FOR CONNECTION was refused") && why.contains("1045"),
                    "{why}"
                );
                assert!(
                    text.tree_text().contains("Table scan on orders"),
                    "{text:?}"
                );
                assert!(
                    text.table.is_some(),
                    "the estimate has both forms: {text:?}"
                );
            }
            other => panic!("expected an estimate, got {other:?}"),
        }
        job.join().unwrap();
    }

    #[test]
    #[ignore = "needs dev/compose.yaml"]
    fn a_lock_wait_names_its_blocker_and_dml_text_is_not_explained_read_only() {
        let (id_tx, id_rx) = mpsc::channel();
        let holder = thread::spawn(move || {
            let mut conn = app_conn(PORT);
            conn.query_drop("BEGIN").unwrap();
            conn.query_drop(
                "UPDATE shop.orders SET amount = amount WHERE id = 1 /* live3-holder */",
            )
            .unwrap();
            id_tx.send(u64::from(conn.connection_id())).unwrap();
            thread::sleep(Duration::from_secs(6));
            conn.query_drop("ROLLBACK").unwrap();
        });
        let holder_id = id_rx.recv().unwrap();
        let waiter =
            work("UPDATE shop.orders SET amount = amount + 1 WHERE id = 1 /* live3-waiter */");

        let mut mon = admin();
        let q = wait_for(|| {
            let q = mon
                .poll()
                .unwrap()
                .running
                .into_iter()
                .find(|q| q.sql.contains("live3-waiter"))?;
            let seen = |name: &str| q.details.iter().any(|(k, _)| k == name);
            // INNODB_TRX is a snapshot cached for ~100ms, so it can lag data_lock_waits.
            (seen("blocked_by") && seen("transaction")).then_some(q)
        });
        let detail = |name: &str| {
            q.details
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.clone())
        };
        assert_eq!(detail("blocked_by"), Some(format!("thread {holder_id}")));
        assert!(
            detail("transaction").unwrap().starts_with("LOCK WAIT"),
            "{:?}",
            q.details
        );

        // An admin account gets the real plan of a running UPDATE...
        match mon.explain(&ExplainTarget::Running(q.clone())).unwrap() {
            Plan::Text(views) => {
                // Some 8.0 servers cannot draw an UPDATE as a tree: then only the table is there.
                assert!(
                    !views.tree_text().contains("not executable"),
                    "that is not a plan: {views:?}"
                );
                let table = views.table.expect("the classic table can draw an UPDATE");
                assert_eq!(cell_of(&table, 0, "select_type"), "UPDATE", "{table:?}");
                assert_eq!(cell_of(&table, 0, "table"), "orders", "{table:?}");
            }
            other => panic!("{other:?}"),
        }
        // ...but the read-only account, refused FOR CONNECTION, will not explain UPDATE text.
        match least().explain(&ExplainTarget::Running(q)).unwrap() {
            Plan::Unavailable(why) => assert!(why.contains("INSERT/UPDATE/DELETE"), "{why}"),
            other => panic!("{other:?}"),
        }
        holder.join().unwrap();
        waiter.join().unwrap();
    }

    #[test]
    #[ignore = "needs dev/compose.yaml"]
    fn a_slow_digest_has_stats_a_sample_and_a_plan_and_leaves_dbtops_own_queries_out() {
        for _ in 0..3 {
            work("SELECT /* live4 */ COUNT(*) FROM shop.orders WHERE status = 'PAID' AND amount > 1.25")
                .join()
                .unwrap();
        }
        let mut mon = least();
        let d = digest(&mut mon, "amount > 1.25");

        assert!(
            d.calls >= 3 && d.total_ms > 0.0 && d.max_ms >= d.avg_ms,
            "{d:?}"
        );
        assert_eq!(d.no_index_used, Some(true));
        assert!(d.rows_examined_avg.unwrap() >= 100_000.0, "{d:?}");
        assert!(!d.truncated);

        match mon.explain(&ExplainTarget::Digest(d)).unwrap() {
            Plan::Estimated { text, why } => {
                assert!(
                    text.tree_text().contains("Table scan on orders"),
                    "{text:?}"
                );
                assert!(text.table.is_some(), "{text:?}");
                assert!(why.contains("sample text"), "{why}");
            }
            other => panic!("{other:?}"),
        }

        // Everything dbtop itself sends, including the EXPLAIN just run above.
        const OURS: [&str; 13] = [
            "SHOW ",
            "SET SESSION",
            "SELECT @@",
            "SELECT CONNECTION_ID",
            "SELECT t.PROCESSLIST_ID",
            "SELECT trx_mysql_thread_id",
            "SELECT waiting.PROCESSLIST_ID",
            "SELECT SCHEMA_NAME, DIGEST",
            "SELECT NAME, ",
            "EXPLAIN ",
            "USE ",
            "SELECT t.",
            "SELECT s.",
        ];
        let leaked: Vec<_> = mon
            .poll()
            .unwrap()
            .slow
            .into_iter()
            .map(|d| d.sample_sql)
            .filter(|sql| OURS.iter().any(|ours| sql.starts_with(ours)))
            .collect();
        assert!(
            leaked.is_empty(),
            "dbtop's own statements leaked into the list: {leaked:?}"
        );
    }

    #[test]
    #[ignore = "needs dev/compose.yaml"]
    fn dml_digests_are_not_explained() {
        work("UPDATE /* live5 */ shop.orders SET amount = amount WHERE status = 'ZZZ'")
            .join()
            .unwrap();
        let mut mon = least();
        let d = digest(&mut mon, "status = 'ZZZ'");
        match mon.explain(&ExplainTarget::Digest(d)).unwrap() {
            Plan::Unavailable(why) => assert!(why.contains("INSERT/UPDATE/DELETE"), "{why}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    #[ignore = "needs dev/compose.yaml"]
    fn a_digest_sample_cut_by_performance_schema_is_flagged_and_not_explained() {
        let ids = (1..=400)
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join(",");
        work(&format!(
            "SELECT /* live6 */ COUNT(*) FROM shop.orders WHERE id IN ({ids})"
        ))
        .join()
        .unwrap();
        let mut mon = least();
        let d = digest(&mut mon, "FROM shop.orders WHERE id IN (1,2,3");

        assert!(d.truncated, "{} bytes kept", d.sample_sql.len());
        match mon.explain(&ExplainTarget::Digest(d)).unwrap() {
            Plan::Unavailable(why) => assert!(why.contains("kept only the first"), "{why}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    #[ignore = "needs dev/compose.yaml"]
    fn a_finished_or_replaced_statement_is_reported_not_explained_wrongly() {
        let mut mon = admin();

        let job = work("SELECT /* live7 */ SLEEP(1) FROM shop.orders WHERE status = 'NEW' LIMIT 1");
        let finished = running(&mut mon, "live7");
        job.join().unwrap();
        match mon.explain(&ExplainTarget::Running(finished)).unwrap() {
            Plan::Unavailable(why) => assert!(why.contains("finished"), "{why}"),
            other => panic!("{other:?}"),
        }

        // One connection, two statements in a row: the id stays, the statement changes.
        let job = thread::spawn(|| {
            let mut conn = app_conn(PORT);
            conn.query_drop(
                "SELECT /* live8-first */ SLEEP(1.5) FROM shop.orders WHERE status = 'NEW' LIMIT 1",
            )
            .unwrap();
            conn.query_drop(
                "SELECT /* live8-second */ SLEEP(4) FROM shop.orders WHERE status = 'NEW' LIMIT 1",
            )
            .unwrap();
        });
        let first = running(&mut mon, "live8-first");
        wait_for(|| {
            mon.poll()
                .unwrap()
                .running
                .into_iter()
                .find(|q| q.id == first.id && q.sql.contains("live8-second"))
        });
        match mon.explain(&ExplainTarget::Running(first)).unwrap() {
            Plan::Unavailable(why) => assert!(why.contains("another statement"), "{why}"),
            other => panic!("{other:?}"),
        }
        job.join().unwrap();
    }

    #[test]
    #[ignore = "needs dev/compose.yaml"]
    fn a_second_statement_hidden_in_a_digest_sample_is_never_run() {
        let before = row_count();
        let hostile = SlowQuery {
            digest: "x".into(),
            schema: "shop".into(),
            sample_sql: "SELECT 1; DELETE FROM shop.orders".into(),
            truncated: false,
            calls: 1,
            total_ms: 1.0,
            avg_ms: 1.0,
            max_ms: 1.0,
            rows_examined_avg: None,
            no_index_used: None,
        };
        // Even an account that could delete rows must not be made to.
        for mon in [&mut least(), &mut admin()] {
            match mon
                .explain(&ExplainTarget::Digest(hostile.clone()))
                .unwrap()
            {
                Plan::Unavailable(why) => assert!(why.contains("';'"), "{why}"),
                other => panic!("{other:?}"),
            }
        }
        assert_eq!(row_count(), before);
    }

    #[test]
    #[ignore = "needs dev/compose.yaml"]
    fn the_session_is_read_only_whatever_the_account_may_do() {
        let mut mon = admin();
        let session = mon.session().unwrap();
        let err = session
            .conn
            .query_drop("UPDATE shop.orders SET amount = amount WHERE id = 2")
            .unwrap_err();
        assert!(err.to_string().contains("READ ONLY"), "{err}");
    }

    #[test]
    #[ignore = "needs dev/compose.yaml"]
    fn it_reconnects_after_its_connection_is_killed() {
        let mut mon = least();
        mon.poll().unwrap();
        let id = mon.session.as_ref().unwrap().id;

        admin()
            .session()
            .unwrap()
            .conn
            .query_drop(format!("KILL {id}"))
            .unwrap();
        assert!(mon.poll().is_err(), "the dead connection is reported");
        assert!(mon.session.is_none());
        assert!(mon.poll().is_ok(), "and the next poll connects again");
        assert_ne!(mon.session.as_ref().unwrap().id, id);
    }

    #[test]
    #[ignore = "needs dev/compose.yaml"]
    fn an_account_without_process_is_told_it_cannot_see_the_others() {
        // Other users' connections: run as `dbtop`, so they are not the blind account's own.
        let jobs: Vec<_> = (0..3)
            .map(|n| {
                thread::spawn(move || {
                    let opts = OptsBuilder::new()
                        .ip_or_hostname(Some("127.0.0.1"))
                        .tcp_port(PORT)
                        .user(Some("dbtop"))
                        .pass(Some("dbtop"))
                        .prefer_socket(false);
                    let mut conn = Conn::new(opts).unwrap();
                    conn.query_drop(format!(
                        "SELECT /* live10-{n} */ SLEEP(3) FROM shop.orders LIMIT 1"
                    ))
                    .unwrap();
                })
            })
            .collect();

        // Nobody else uses `blind`, and it holds no PROCESS privilege (other tests share `app`).
        let mut blind = monitor("blind", "blind", PORT);
        let note = wait_for(|| blind.poll().unwrap().running_note);
        assert!(note.contains("grant PROCESS"), "{note}");
        assert!(
            blind.poll().unwrap().running.is_empty(),
            "the other accounts' queries are invisible to it"
        );

        let mut sees = least();
        let sample = wait_for(|| {
            Some(sees.poll().unwrap())
                .filter(|s| s.running.iter().any(|q| q.sql.contains("live10")))
        });
        assert!(
            sample.running_note.is_none(),
            "dbtop's own account has PROCESS"
        );
        for job in jobs {
            job.join().unwrap();
        }
    }

    /// The throwaway CA that signed the dev MySQL's certificate (written by dev/tls-setup.sh).
    const CA: &str = "dev/certs/mysql.pem";

    fn tls_target(port: u16, tls: Tls, ca: Option<&str>) -> Target {
        Target {
            tls,
            tls_ca: ca.map(std::path::PathBuf::from),
            ..Target::dev(Kind::Mysql, port, "dbtop", "dbtop")
        }
    }

    /// What the server says about a connection made with these settings.
    fn encrypted(target: &Target) -> Option<bool> {
        MySql::connect(target)
            .unwrap_or_else(|e| panic!("{e:#}"))
            .poll()
            .unwrap()
            .encrypted
    }

    #[test]
    #[ignore = "needs dev/compose.yaml"]
    fn prefer_and_require_encrypt_and_off_does_not() {
        assert_eq!(encrypted(&tls_target(PORT, Tls::Prefer, None)), Some(true));
        assert_eq!(encrypted(&tls_target(PORT, Tls::Require, None)), Some(true));
        assert_eq!(encrypted(&tls_target(PORT, Tls::Off, None)), Some(false));
    }

    #[test]
    #[ignore = "needs dev/compose.yaml"]
    fn verify_accepts_a_certificate_whose_ca_is_trusted_and_that_names_the_host() {
        // The dev certificate names `localhost` (see dev/compose.yaml), not 127.0.0.1.
        let mut target = tls_target(PORT, Tls::Verify, Some(CA));
        target.host = "localhost".into();
        assert_eq!(encrypted(&target), Some(true));
    }

    #[test]
    #[ignore = "needs dev/compose.yaml"]
    fn verify_refuses_what_it_cannot_trust_and_a_name_the_certificate_does_not_hold() {
        // Use the name the certificate does hold, so that only the CA is in question.
        let by_name = |tls, ca| Target {
            host: "localhost".into(),
            ..tls_target(PORT, tls, ca)
        };
        // A refusal only means something if the server was reachable by that name at all.
        assert_eq!(
            encrypted(&by_name(Tls::Require, None)),
            Some(true),
            "control: localhost connects"
        );
        let not_a_connection_problem = |err: &anyhow::Error| {
            let text = format!("{err:#}");
            assert!(
                !text.contains("Could not connect") && !text.contains("refused"),
                "{text}"
            );
        };

        // The dev CA is in no trust store, and none was given.
        let err = MySql::connect(&by_name(Tls::Verify, None))
            .err()
            .expect("untrusted");
        assert!(format!("{err:#}").contains("tls = \"verify\""), "{err:#}");
        not_a_connection_problem(&err);

        // Someone else's CA.
        let wrong = by_name(Tls::Verify, Some("dev/certs/postgres.pem"));
        let err = MySql::connect(&wrong)
            .err()
            .expect("a CA that did not sign it must not do");
        not_a_connection_problem(&err);

        // The right CA, but the address 127.0.0.1 is not among the names the certificate holds.
        // Require checks nothing and accepts; Verify must not.
        let mut by_ip = tls_target(PORT, Tls::Verify, Some(CA));
        assert_eq!(by_ip.host, "127.0.0.1");
        let err = MySql::connect(&by_ip)
            .err()
            .expect("a host name mismatch must fail");
        not_a_connection_problem(&err);
        by_ip.tls = Tls::Require;
        by_ip.tls_ca = None;
        assert_eq!(encrypted(&by_ip), Some(true));
    }

    #[test]
    #[ignore = "needs: docker compose -f dev/compose.yaml --profile extra up -d --wait"]
    fn a_server_without_tls_gets_a_plain_connection_from_prefer_and_an_error_from_the_rest() {
        assert_eq!(
            encrypted(&tls_target(PORT_NO_PS, Tls::Prefer, None)),
            Some(false)
        );
        assert_eq!(
            encrypted(&tls_target(PORT_NO_PS, Tls::Off, None)),
            Some(false)
        );
        for tls in [Tls::Require, Tls::Verify] {
            let err = MySql::connect(&tls_target(PORT_NO_PS, tls, None))
                .err()
                .expect("no TLS to be had");
            assert!(
                format!("{err:#}").contains("does not have this capability"),
                "{tls:?}: {err:#}"
            );
        }
    }

    /// Polls twice with `activity` in between and returns what would be charted.
    fn charted(
        mon: &mut MySql,
        activity: impl FnOnce(),
    ) -> std::collections::HashMap<&'static str, f64> {
        let mut meters = crate::model::Meters::default();
        let start = Instant::now();
        meters.read(start, &mon.poll().unwrap().metrics);
        activity();
        thread::sleep(Duration::from_millis(1100));
        let readings = mon.poll().unwrap().metrics;
        meters
            .read(Instant::now(), &readings)
            .into_iter()
            .filter_map(|(key, value)| value.map(|v| (key, v)))
            .collect()
    }

    #[test]
    #[ignore = "needs dev/compose.yaml"]
    fn the_dashboard_metrics_are_real_numbers_that_react_to_activity() {
        let mut mon = least();
        let shown = charted(&mut mon, || {
            // A scan reads through the buffer pool, so the hit rate has something to be a share of.
            // Not the shape of live4's statement: both would be one digest, whose sample text is
            // whichever execution was slowest.
            work("SELECT /* live-metrics */ COUNT(*) FROM shop.orders WHERE amount > 1.5 AND status = 'PAID'")
                .join()
                .unwrap();
        });

        for key in ["qps", "hit", "net", "lock_waits", "deadlocks", "hll"] {
            assert!(shown.contains_key(key), "{key} is missing from {shown:?}");
            assert!(crate::ui::metrics::spec(key).is_some());
        }
        assert!(shown["qps"] > 0.0, "{shown:?}");
        assert!((0.0..=100.0).contains(&shown["hit"]), "{}", shown["hit"]);
        assert!(shown["net"] > 0.0);
        assert!(shown["deadlocks"] >= 0.0 && shown["hll"] >= 0.0);
        // Server-wide gauges: tests that run alongside may be holding locks or a transaction open
        // (without one there is no age at all), so only their shape.
        assert!(shown["lock_waits"] >= 0.0);
        assert!(shown.get("oldest_xact").is_none_or(|age| *age >= 0.0));
    }

    /// Two transactions that each want the row the other holds: InnoDB rolls one of them back.
    fn make_a_deadlock() {
        let side = |first: u32, second: u32| {
            thread::spawn(move || {
                let mut conn = app_conn(PORT);
                conn.query_drop("BEGIN").unwrap();
                conn.query_drop(format!(
                    "UPDATE shop.orders SET amount = amount WHERE id = {first} /* live-deadlock */"
                ))
                .unwrap();
                thread::sleep(Duration::from_millis(600)); // both hold their first row by now
                // One of the two is refused here, with error 1213; the other one goes on.
                let _ = conn.query_drop(format!(
                    "UPDATE shop.orders SET amount = amount WHERE id = {second} /* live-deadlock */"
                ));
                let _ = conn.query_drop("ROLLBACK");
            })
        };
        let (a, b) = (side(70, 71), side(71, 70));
        a.join().unwrap();
        b.join().unwrap();
    }

    #[test]
    #[ignore = "needs dev/compose.yaml"]
    fn a_real_deadlock_is_counted_and_charted_as_a_rate() {
        let mut mon = least();
        let shown = charted(&mut mon, make_a_deadlock);
        let rate = shown.get("deadlocks").copied();
        assert!(rate.is_some_and(|rate| rate > 0.0), "{shown:?}");
    }

    #[test]
    #[ignore = "needs dev/compose.yaml"]
    fn the_history_list_grows_while_an_old_snapshot_holds_back_the_purge() {
        let history = |mon: &mut MySql| {
            mon.poll()
                .unwrap()
                .metrics
                .iter()
                .find_map(|m| match (m.key, m.raw) {
                    ("hll", Raw::Gauge(length)) => Some(length),
                    _ => None,
                })
        };
        let mut mon = least();
        // Nothing changed after this snapshot can be purged while it is open.
        let mut reader = app_conn(PORT);
        reader
            .query_drop("START TRANSACTION WITH CONSISTENT SNAPSHOT")
            .unwrap();
        let before = history(&mut mon).expect("the server keeps the history list length");

        let mut writer = app_conn(PORT);
        for i in 0..300 {
            let sign = if i % 2 == 0 { "+" } else { "-" }; // ends where it began
            writer
                .query_drop(format!(
                    "UPDATE shop.orders SET amount = amount {sign} 1 WHERE id = 72"
                ))
                .unwrap();
        }
        let after = wait_for(|| history(&mut mon).filter(|length| *length >= before + 250.0));
        reader.query_drop("ROLLBACK").unwrap();
        assert!(after >= before + 250.0, "{before} -> {after}");
    }

    #[test]
    #[ignore = "needs dev/compose.yaml"]
    fn a_lock_wait_and_an_old_transaction_show_up_in_the_gauges() {
        let (id_tx, id_rx) = mpsc::channel();
        let holder = thread::spawn(move || {
            let mut conn = app_conn(PORT);
            conn.query_drop("BEGIN").unwrap();
            conn.query_drop(
                "UPDATE shop.orders SET amount = amount WHERE id = 3 /* live-gauge-holder */",
            )
            .unwrap();
            id_tx.send(()).unwrap();
            thread::sleep(Duration::from_secs(5));
            conn.query_drop("ROLLBACK").unwrap();
        });
        id_rx.recv().unwrap();
        let waiter =
            work("UPDATE shop.orders SET amount = amount + 1 WHERE id = 3 /* live-gauge-waiter */");

        let mut mon = least();
        let seen = wait_for(|| {
            let metrics = mon.poll().unwrap().metrics;
            let gauge = |key: &str| {
                metrics
                    .iter()
                    .find(|m| m.key == key)
                    .and_then(|m| match m.raw {
                        Raw::Gauge(v) => Some(v),
                        _ => None,
                    })
            };
            (gauge("lock_waits")? >= 1.0 && gauge("oldest_xact")? >= 1.0)
                .then(|| (gauge("lock_waits"), gauge("oldest_xact")))
        });
        assert!(seen.0.unwrap() >= 1.0 && seen.1.unwrap() >= 1.0, "{seen:?}");
        holder.join().unwrap();
        waiter.join().unwrap();
    }

    #[test]
    #[ignore = "needs dev/compose.yaml"]
    fn a_wrong_password_fails_before_anything_else() {
        let err = MySql::connect(&Target::dev(Kind::Mysql, PORT, "dbtop", "nope"))
            .err()
            .expect("must fail");
        assert!(format!("{err:#}").contains("1045"), "{err:#}");
    }

    #[test]
    #[ignore = "needs: docker compose -f dev/compose.yaml --profile extra up -d --wait"]
    fn without_performance_schema_the_running_list_still_works() {
        let job = work_on(
            PORT_NO_PS,
            "SELECT /* live9 */ SLEEP(4) FROM shop.orders WHERE status = 'NEW' LIMIT 1",
        );
        let mut mon = monitor("root", "root", PORT_NO_PS);

        let sample = mon.poll().unwrap();
        assert!(sample.slow.is_empty());
        assert!(
            sample
                .slow_note
                .unwrap()
                .contains("performance_schema is OFF")
        );

        let q = running(&mut mon, "live9");
        assert_eq!(
            q.secs.fract(),
            0.0,
            "only whole seconds without performance_schema"
        );
        assert!(
            q.details.iter().all(|(k, _)| k != "rows_examined"),
            "{:?}",
            q.details
        );
        match mon.explain(&ExplainTarget::Running(q)).unwrap() {
            Plan::Text(views) => {
                assert!(
                    views.tree_text().contains("Table scan on orders"),
                    "{views:?}"
                )
            }
            other => panic!("{other:?}"),
        }
        job.join().unwrap();
    }
}
