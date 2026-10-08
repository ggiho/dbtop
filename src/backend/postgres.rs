//! PostgreSQL and Aurora PostgreSQL. Developed against 16; 13 and later are supported.
//!
//! PostgreSQL cannot show the plan another session is running, so every plan here is the
//! planner's estimate for the same text, and is labelled that way.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use postgres::config::SslMode;
use postgres::error::SqlState;
use postgres::{Client, Config, Row, SimpleQueryMessage};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, ring};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme};
use tokio_postgres_rustls::MakeRustlsConnect;

use super::{Backend, guard};
use crate::config::{Target, Tls};
use crate::model::{ExplainTarget, Plan, RunningQuery, Sample, SlowQuery};

const MAX_DIGESTS: i64 = 100;

/// What pg_stat_activity shows in place of another role's query when the viewer may not see it.
const HIDDEN: &str = "<insufficient privilege>";

const HIDDEN_NOTE: &str = "this role cannot see other roles' queries: grant pg_read_all_stats (for example through pg_monitor)";

const GENERIC_PLAN_FROM: i32 = 160_000;

/// dbtop's own statements would otherwise top the list of a quiet server.
const NOT_OURS: &str = "s.query NOT ILIKE '%pg_stat_activity%' \
    AND s.query NOT ILIKE '%pg_stat_statements%' \
    AND s.query NOT ILIKE '%pg_stat_database%' \
    AND s.query NOT ILIKE 'EXPLAIN%' \
    AND s.query NOT ILIKE 'SHOW %' \
    AND s.query NOT ILIKE 'SET %'";

const RUNNING: &str = "\
    SELECT a.pid, COALESCE(a.usename, ''), COALESCE(host(a.client_addr), ''), COALESCE(a.datname, ''), \
           COALESCE(a.wait_event_type || ':' || a.wait_event, ''), \
           EXTRACT(EPOCH FROM (now() - a.query_start))::float8, \
           a.query, octet_length(a.query) >= $1 - 1, COALESCE(a.application_name, ''), \
           EXTRACT(EPOCH FROM (now() - a.xact_start))::float8, pg_blocking_pids(a.pid) \
    FROM pg_stat_activity a \
    WHERE a.backend_type = 'client backend' AND a.state = 'active' AND a.pid <> pg_backend_pid() \
    ORDER BY a.query_start";

const TOTALS: &str = "\
    SELECT (SELECT COALESCE(sum(xact_commit + xact_rollback), 0)::bigint FROM pg_stat_database), \
           (SELECT count(*) FROM pg_stat_activity \
             WHERE backend_type = 'client backend' AND state = 'active' AND pid <> pg_backend_pid()), \
           (SELECT count(*) FROM pg_stat_activity WHERE backend_type = 'client backend'), \
           current_setting('max_connections')::int";

pub struct Postgres {
    /// `None` after a failure; the next poll reconnects.
    session: Option<Session>,
    target: Target,
}

struct Session {
    client: Client,
    target: Target,
    database: String,
    /// `server_version_num`, e.g. 160014.
    version: i32,
    /// `track_activity_query_size`: pg_stat_activity cuts query text at this many bytes minus one.
    query_size: i32,
    /// Whether this role may read other roles' sessions (superuser or pg_read_all_stats).
    sees_all: bool,
    /// What the server says about this connection, not what was asked for.
    encrypted: bool,
}

impl Postgres {
    /// Connects now, so a wrong host or password fails before the screen is taken over.
    pub fn connect(target: &Target) -> Result<Postgres> {
        Ok(Postgres {
            session: Some(Session::open(target)?),
            target: target.clone(),
        })
    }

    fn session(&mut self) -> Result<&mut Session> {
        if self.session.is_none() {
            self.session = Some(Session::open(&self.target)?);
        }
        self.session.as_mut().context("no session")
    }
}

impl Backend for Postgres {
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
        // An error from the server is an answer; anything else means the connection is gone.
        if let Err(e) = &plan
            && e.downcast_ref::<postgres::Error>()
                .is_some_and(|e| e.as_db_error().is_none())
        {
            self.session = None;
        }
        plan
    }
}

/// Redshift speaks the Postgres protocol but has none of what this reads: no pg_stat_statements,
/// no pg_read_all_stats, a different pg_stat_activity.
fn refuse_redshift(version: &str) -> Result<()> {
    if version.contains("Redshift") {
        anyhow::bail!("this is Amazon Redshift, which dbtop does not support; use dbq for it");
    }
    Ok(())
}

fn connect(target: &Target, database: Option<&str>) -> Result<Client> {
    let mut config = Config::new();
    config
        .host(&target.host)
        .port(target.port)
        .user(&target.user)
        .dbname(
            database
                .or(target.database.as_deref())
                .unwrap_or("postgres"),
        )
        .application_name("dbtop")
        .connect_timeout(Duration::from_secs(5))
        .keepalives_idle(Duration::from_secs(30))
        // Whatever the role may do, this session reads only and cannot hang on a lock or a
        // slow statement.
        .options(
            "-c default_transaction_read_only=on -c statement_timeout=5000 -c lock_timeout=2000 \
             -c idle_in_transaction_session_timeout=60000",
        );
    if let Some(password) = &target.password {
        config.password(password.expose());
    }
    config.ssl_mode(match target.tls {
        Tls::Off => SslMode::Disable,
        // libpq's sslmode=prefer: try TLS, carry on without if the server declines.
        Tls::Prefer => SslMode::Prefer,
        Tls::Require | Tls::Verify => SslMode::Require,
    });
    config
        .connect(MakeRustlsConnect::new(tls_config(target)?))
        .with_context(|| match target.tls {
            Tls::Verify => {
                "cannot connect (tls = \"verify\" checks the certificate and that it names the host)"
            }
            _ => "cannot connect",
        })
}

fn tls_config(target: &Target) -> Result<ClientConfig> {
    let provider = Arc::new(ring::default_provider());
    let builder = ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .context("cannot set up TLS")?;
    Ok(if target.tls == Tls::Verify {
        // The same roots as the MySQL side (Mozilla's), plus the file the user names.
        let mut roots = RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        if let Some(path) = &target.tls_ca {
            add_ca_file(&mut roots, path)?;
        }
        builder.with_root_certificates(roots).with_no_client_auth()
    } else {
        // Encrypt without asking who answered: libpq's prefer and require.
        builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AcceptAnyCertificate(provider)))
            .with_no_client_auth()
    })
}

/// Every certificate in a PEM file: a CA bundle holds many.
fn add_ca_file(roots: &mut RootCertStore, path: &Path) -> Result<()> {
    let bytes =
        std::fs::read(path).with_context(|| format!("cannot read tls_ca {}", path.display()))?;
    let unreadable = || format!("{} holds a certificate that cannot be read", path.display());
    let certificates = CertificateDer::pem_slice_iter(&bytes)
        .collect::<Result<Vec<_>, _>>()
        .with_context(unreadable)?;
    anyhow::ensure!(
        !certificates.is_empty(),
        "{} holds no PEM certificate",
        path.display()
    );
    for certificate in certificates {
        roots.add(certificate).with_context(unreadable)?;
    }
    Ok(())
}

/// Takes any certificate, for `prefer` and `require`. The handshake signatures are still checked,
/// so the peer must hold the key of the certificate it shows; nothing says whose it is.
#[derive(Debug)]
struct AcceptAnyCertificate(Arc<CryptoProvider>);

impl ServerCertVerifier for AcceptAnyCertificate {
    fn verify_server_cert(
        &self,
        _: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

impl Session {
    fn open(target: &Target) -> Result<Session> {
        let mut client = connect(target, None)?;
        let banner: String = client.query_one("SELECT version()", &[])?.try_get(0)?;
        refuse_redshift(&banner)?;
        let facts = client.query_one(
            "SELECT current_setting('server_version_num')::int, current_database()::text, \
                    (SELECT setting::int FROM pg_settings WHERE name = 'track_activity_query_size'), \
                    pg_has_role(current_user, 'pg_read_all_stats', 'USAGE'), \
                    COALESCE((SELECT ssl FROM pg_stat_ssl WHERE pid = pg_backend_pid()), false)",
            &[],
        )?;
        Ok(Session {
            version: facts.try_get(0)?,
            database: facts.try_get(1)?,
            query_size: facts.try_get(2)?,
            sees_all: facts.try_get(3)?,
            encrypted: facts.try_get(4)?,
            client,
            target: target.clone(),
        })
    }

    fn poll(&mut self) -> Result<Sample> {
        let totals = self.client.query_one(TOTALS, &[])?;
        let transactions: i64 = totals.try_get(0)?;
        let running_threads: i64 = totals.try_get(1)?;
        let connected: i64 = totals.try_get(2)?;
        let max_connections: i32 = totals.try_get(3)?;

        let running = self.running()?;
        let (slow, slow_note, calls) = self.statements();
        // Statements per second where pg_stat_statements can count them; otherwise the closest
        // thing core PostgreSQL has, transactions.
        let (counter, rate_label) = match calls {
            Some(calls) => (calls, "qps"),
            None => (transactions as u64, "tps"),
        };

        Ok(Sample {
            counter,
            rate_label,
            threads_running: running_threads as u32,
            threads_connected: connected as u32,
            max_connections: max_connections as u32,
            running,
            encrypted: Some(self.encrypted),
            running_note: (!self.sees_all).then(|| HIDDEN_NOTE.to_string()),
            slow,
            slow_note,
        })
    }

    fn running(&mut self) -> Result<Vec<RunningQuery>> {
        let rows = self.client.query(RUNNING, &[&self.query_size])?;
        rows.iter().map(|row| Ok(running_from_row(row)?)).collect()
    }

    /// The statements that have used the most time in total, from pg_stat_statements, and the
    /// total number of calls. Optional: without the extension this says how to get it.
    fn statements(&mut self) -> (Vec<SlowQuery>, Option<String>, Option<u64>) {
        let (total, max) = if self.version >= 130_000 {
            ("total_exec_time", "max_exec_time")
        } else {
            ("total_time", "max_time")
        };
        let list = self.client.query(
            &format!(
                "SELECT s.dbid::text || ':' || s.queryid::text, d.datname::text, min(s.query), \
                        sum(s.calls)::bigint, sum(s.{total})::float8, max(s.{max})::float8 \
                 FROM pg_stat_statements s JOIN pg_database d ON d.oid = s.dbid \
                 WHERE s.calls > 0 AND {NOT_OURS} \
                 GROUP BY s.dbid, s.queryid, d.datname \
                 ORDER BY sum(s.{total}) DESC LIMIT {MAX_DIGESTS}"
            ),
            &[],
        );
        let rows = match list {
            Ok(rows) => rows,
            Err(e) => {
                return (
                    Vec::new(),
                    Some(statements_unavailable(&e, &self.database)),
                    None,
                );
            }
        };
        let digests = rows
            .iter()
            .filter_map(|row| digest_from_row(row).ok())
            .collect();
        let calls = self
            .client
            .query_one(
                "SELECT COALESCE(sum(calls), 0)::bigint FROM pg_stat_statements",
                &[],
            )
            .ok()
            .and_then(|row| row.try_get::<_, i64>(0).ok())
            .map(|n| n as u64);
        (digests, None, calls)
    }

    fn explain_running(&mut self, query: &RunningQuery) -> Result<Plan> {
        // A pid outlives its statements: make sure it still runs the one that was opened.
        let now = self.client.query_opt(
            "SELECT query FROM pg_stat_activity WHERE pid = $1 AND state = 'active'",
            &[&(query.id as i32)],
        )?;
        match now.map(|row| row.try_get::<_, String>(0)).transpose()? {
            None => {
                return Ok(Plan::Unavailable(
                    "the statement finished before its plan could be read".into(),
                ));
            }
            Some(text) if text != query.sql => {
                return Ok(Plan::Unavailable(
                    "the session has moved on to another statement".into(),
                ));
            }
            Some(_) => {}
        }
        if query.sql == HIDDEN {
            return Ok(Plan::Unavailable(format!(
                "the server hides this role's view of that query ({HIDDEN}): grant pg_read_all_stats"
            )));
        }
        if query.truncated {
            return Ok(Plan::Unavailable(format!(
                "the server cut the text at {} bytes (track_activity_query_size), so EXPLAIN would not describe it",
                self.query_size - 1
            )));
        }
        let why =
            "PostgreSQL cannot show another session's plan: this is a fresh plan for the same text";
        self.explain_text(&query.sql, &query.db, why)
    }

    fn explain_digest(&mut self, digest: &SlowQuery) -> Result<Plan> {
        let why = "no statement is running: this is the statement's normalized text planned now";
        self.explain_text(&digest.sample_sql, &digest.schema, why)
    }

    /// Plans `sql` again. The text came from the server, so it is only explained if it is one
    /// data statement; the session is read-only too, and `EXPLAIN` never executes it.
    fn explain_text(&mut self, sql: &str, database: &str, why: &str) -> Result<Plan> {
        if let Err(not) = guard::explainable(sql) {
            return Ok(Plan::Unavailable(format!(
                "the text is not explained because {not}"
            )));
        }
        // A connection belongs to one database; reach into another one for this plan only.
        let mut other;
        let client = if database.is_empty() || database == self.database {
            &mut self.client
        } else {
            other = connect(&self.target, Some(database))?;
            &mut other
        };

        // Parsing it as a prepared statement proves it is a single statement, whatever the text
        // holds, and tells how many parameters ($1 ...) it has.
        let probe = match client.prepare(&format!("EXPLAIN {sql}")) {
            Ok(statement) => statement,
            Err(e) => return explain_failed(e),
        };
        let (lines, how) = if probe.params().is_empty() {
            let rows = client.query(&probe, &[]);
            (
                rows.map(|rows| {
                    rows.iter()
                        .filter_map(|row| row.try_get::<_, String>(0).ok())
                        .collect::<Vec<_>>()
                }),
                "",
            )
        } else if self.version >= GENERIC_PLAN_FROM {
            // A generic plan must not be given values: a bound NULL gets folded into the plan and
            // yields "One-Time Filter: false". The simple protocol, like psql's, binds nothing.
            let rows = client.simple_query(&format!("EXPLAIN (GENERIC_PLAN) {sql}"));
            let lines = rows.map(|messages| {
                messages
                    .iter()
                    .filter_map(|m| match m {
                        SimpleQueryMessage::Row(row) => row.get(0).map(str::to_owned),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
            });
            (lines, " (a generic plan: the parameter values are unknown)")
        } else {
            return Ok(Plan::Unavailable(format!(
                "the text has parameters ($1 ...) and only PostgreSQL 16+ can plan that without values; this server is {}",
                self.version / 10_000
            )));
        };
        match lines {
            Ok(lines) => Ok(Plan::Estimated {
                text: lines.join("\n"),
                why: format!("{why}{how}"),
            }),
            Err(e) => explain_failed(e),
        }
    }
}

/// A server error is an answer to show; anything else is the connection.
fn explain_failed(e: postgres::Error) -> Result<Plan> {
    match e.as_db_error() {
        Some(db) => Ok(Plan::Unavailable(format!(
            "PostgreSQL refused to plan it ({}): {}",
            db.code().code(),
            db.message()
        ))),
        None => Err(e.into()),
    }
}

fn statements_unavailable(e: &postgres::Error, database: &str) -> String {
    match e.as_db_error().map(|db| db.code()) {
        Some(code) if *code == SqlState::UNDEFINED_TABLE => format!(
            "pg_stat_statements is not installed in database {database:?}: CREATE EXTENSION pg_stat_statements there, or connect to the database that has it"
        ),
        Some(code) if *code == SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE => {
            "pg_stat_statements is installed but not loaded: add it to shared_preload_libraries and restart".to_string()
        }
        Some(code) if *code == SqlState::INSUFFICIENT_PRIVILEGE => {
            "this role may not read pg_stat_statements: grant pg_read_all_stats (for example through pg_monitor)".to_string()
        }
        _ => format!("cannot read pg_stat_statements: {e}"),
    }
}

fn running_from_row(row: &Row) -> Result<RunningQuery, postgres::Error> {
    let pid: i32 = row.try_get(0)?;
    let wait: String = row.try_get(4)?;
    let application: String = row.try_get(8)?;
    let xact_secs: Option<f64> = row.try_get(9)?;
    let blockers: Vec<i32> = row.try_get(10)?;

    let mut details = Vec::new();
    if !application.is_empty() {
        details.push(("application".to_string(), application));
    }
    if !wait.is_empty() {
        details.push(("wait_event".to_string(), wait.clone()));
    }
    if let Some(secs) = xact_secs {
        details.push(("transaction".to_string(), format!("{secs:.1}s old")));
    }
    if !blockers.is_empty() {
        let list = blockers
            .iter()
            .map(|b| format!("pid {b}"))
            .collect::<Vec<_>>()
            .join(", ");
        details.push(("blocked_by".to_string(), list));
    }
    Ok(RunningQuery {
        id: pid as u64,
        user: row.try_get(1)?,
        host: row.try_get(2)?,
        db: row.try_get(3)?,
        state: if wait.is_empty() {
            "active".to_string()
        } else {
            wait
        },
        secs: row.try_get::<_, Option<f64>>(5)?.unwrap_or(0.0),
        sql: row.try_get(6)?,
        truncated: row.try_get(7)?,
        details,
    })
}

fn digest_from_row(row: &Row) -> Result<SlowQuery, postgres::Error> {
    let calls: i64 = row.try_get(3)?;
    let total_ms: f64 = row.try_get(4)?;
    Ok(SlowQuery {
        digest: row.try_get(0)?,
        schema: row.try_get(1)?,
        sample_sql: row.try_get(2)?,
        // pg_stat_statements keeps the whole text in a file, not a size-limited buffer.
        truncated: false,
        calls: calls as u64,
        total_ms,
        avg_ms: total_ms / calls.max(1) as f64,
        max_ms: row.try_get(5)?,
        // PostgreSQL does not count rows examined or index use per statement.
        rows_examined_avg: None,
        no_index_used: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two throwaway self-signed certificates, only to be read.
    const CERT_A: &str = "-----BEGIN CERTIFICATE-----
MIIBHjCBxgIJAIAS1O0blkVhMAoGCCqGSM49BAMCMBcxFTATBgNVBAMMDGRidG9w
IHRlc3QgQTAgFw0yNjEwMDgwMjAxMjNaGA8yMTI2MDkxNDAyMDEyM1owFzEVMBMG
A1UEAwwMZGJ0b3AgdGVzdCBBMFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAENpwD
KGF/rTG6Te3hUTI+lu/CnFKwH+PZwMIQHT1Nt4DRZ+aZVOEJgJNBoE02m5RnY9Ay
dwxOGYUn7gcSgfGNETAKBggqhkjOPQQDAgNHADBEAiBIJmi2d6V0cW8qKlY1NpLT
SWtOZNLpWeNt61nAIyEFaQIgOXj6UgEswtZCBHer4qMbVX3Y0yJgrEI0aPaw+yz9
rgY=
-----END CERTIFICATE-----";
    const CERT_B: &str = "-----BEGIN CERTIFICATE-----
MIIBIDCBxgIJAK4+3FUf/vTSMAoGCCqGSM49BAMCMBcxFTATBgNVBAMMDGRidG9w
IHRlc3QgQjAgFw0yNjEwMDgwMjAxMjNaGA8yMTI2MDkxNDAyMDEyM1owFzEVMBMG
A1UEAwwMZGJ0b3AgdGVzdCBCMFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAE2wyM
zEB83JY79HsjJdxHEep231BSqUL8cjp1aXRQnbqoEv4SUHub1RJZkc69x4Mt1dw1
2cfSVEkHBYnn1OrmRzAKBggqhkjOPQQDAgNJADBGAiEA3Ahul5NGsv1btc6/QC5Z
7x4c+m4wfuU1lnuLG2m2hWoCIQDkH5HsV5B56a+9cvUbDg2wgqwHtrsztUTZVQHW
4xs3iw==
-----END CERTIFICATE-----";

    /// A well-formed PEM block that holds no certificate.
    const BROKEN: &str = "-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n";

    fn temp_file(name: &str, contents: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("dbtop-{}-{name}", std::process::id()));
        std::fs::write(&path, contents).unwrap();
        path
    }

    fn with_tls(tls: Tls, ca: Option<&Path>) -> Target {
        Target {
            tls,
            tls_ca: ca.map(Path::to_path_buf),
            ..Target::dev(crate::config::Kind::Postgres, 5432, "u", "p")
        }
    }

    #[test]
    fn a_ca_bundle_adds_every_certificate_in_it() {
        let bundle = temp_file(
            "bundle.pem",
            &format!("# a bundle\n{CERT_A}\n\nnotes\n{CERT_B}\n"),
        );
        let mut roots = RootCertStore::empty();
        add_ca_file(&mut roots, &bundle).unwrap();
        assert_eq!(roots.len(), 2);
        std::fs::remove_file(bundle).unwrap();
    }

    #[test]
    fn every_tls_mode_builds_a_config() {
        let ca = temp_file("ca.pem", CERT_A);
        for tls in [Tls::Off, Tls::Prefer, Tls::Require] {
            tls_config(&with_tls(tls, None)).unwrap_or_else(|e| panic!("{tls:?}: {e:#}"));
        }
        tls_config(&with_tls(Tls::Verify, None)).expect("verify with the built-in roots");
        tls_config(&with_tls(Tls::Verify, Some(&ca))).expect("verify with an extra CA");
        std::fs::remove_file(ca).unwrap();
    }

    #[test]
    fn a_ca_file_that_is_missing_empty_or_broken_is_an_error_not_silently_ignored() {
        let missing = tls_config(&with_tls(
            Tls::Verify,
            Some(Path::new("/nonexistent/ca.pem")),
        ))
        .unwrap_err();
        assert!(
            format!("{missing:#}").contains("cannot read tls_ca"),
            "{missing:#}"
        );

        let text = temp_file("text.pem", "just some text\n");
        let none = tls_config(&with_tls(Tls::Verify, Some(&text))).unwrap_err();
        assert!(
            none.to_string().contains("holds no PEM certificate"),
            "{none:#}"
        );

        let broken = temp_file("broken.pem", BROKEN);
        let err = tls_config(&with_tls(Tls::Verify, Some(&broken))).unwrap_err();
        assert!(err.to_string().contains("cannot be read"), "{err:#}");
        for path in [text, broken] {
            std::fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn redshift_is_refused_by_name_and_postgres_is_not() {
        let redshift = "PostgreSQL 8.0.2 on i686-pc-linux-gnu, compiled by GCC gcc (GCC) 3.4.2, Redshift 1.0.123456";
        assert!(
            refuse_redshift(redshift)
                .unwrap_err()
                .to_string()
                .contains("dbq")
        );
        assert!(
            refuse_redshift(
                "PostgreSQL 16.14 (Debian 16.14-1.pgdg13+1) on aarch64-unknown-linux-gnu"
            )
            .is_ok()
        );
    }
}

/// Against the throwaway databases in dev/compose.yaml, with a workload role (`app`) running
/// statements while dbtop's monitoring role (`dbtop`) or the superuser looks on:
///
/// ```text
/// docker compose -f dev/compose.yaml up -d --wait
/// cargo test postgres::live -- --ignored
/// ```
#[cfg(test)]
mod live {
    use std::sync::mpsc;
    use std::thread::{self, JoinHandle};
    use std::time::{Duration, Instant};

    use postgres::NoTls;

    use super::*;
    use crate::config::Kind;

    const PORT: u16 = 15432;
    const PORT_13: u16 = 15433;

    fn target(user: &str, password: &str, port: u16, database: &str) -> Target {
        Target {
            database: Some(database.into()),
            ..Target::dev(Kind::Postgres, port, user, password)
        }
    }

    fn monitor(user: &str, password: &str, port: u16, database: &str) -> Postgres {
        Postgres::connect(&target(user, password, port, database))
            .expect("is the database from dev/compose.yaml up?")
    }

    /// What dbtop is meant to run as: pg_monitor, SELECT on the tables, nothing else.
    fn least() -> Postgres {
        monitor("dbtop", "dbtop", PORT, "shop")
    }

    fn admin() -> Postgres {
        monitor("postgres", "root", PORT, "shop")
    }

    /// A plain connection: unlike dbtop's own, it may write.
    fn app_client(port: u16, database: &str) -> Client {
        let mut config = Config::new();
        config
            .host("127.0.0.1")
            .port(port)
            .user("app")
            .password("app")
            .dbname(database);
        config
            .connect(NoTls)
            .expect("is the database from dev/compose.yaml up?")
    }

    /// Runs `sql` as the workload role on its own connection.
    fn work_on(port: u16, database: &'static str, sql: &str) -> JoinHandle<()> {
        let sql = sql.to_string();
        thread::spawn(move || app_client(port, database).batch_execute(&sql).unwrap())
    }

    fn work(sql: &str) -> JoinHandle<()> {
        work_on(PORT, "shop", sql)
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

    fn running(mon: &mut Postgres, marker: &str) -> RunningQuery {
        wait_for(|| {
            mon.poll()
                .unwrap()
                .running
                .into_iter()
                .find(|q| q.sql.contains(marker))
        })
    }

    fn digest(mon: &mut Postgres, needle: &str) -> SlowQuery {
        wait_for(|| {
            mon.poll()
                .unwrap()
                .slow
                .into_iter()
                .find(|d| d.sample_sql.contains(needle))
        })
    }

    #[test]
    #[ignore = "needs dev/compose.yaml"]
    fn a_running_statement_shows_up_with_details_and_a_labelled_plan() {
        let job = work("SELECT /* pg1 */ pg_sleep(4) FROM orders WHERE status = 'NEW' LIMIT 1");
        let mut mon = least();
        let q = running(&mut mon, "pg1");

        assert_eq!((q.user.as_str(), q.db.as_str()), ("app", "shop"));
        assert!(
            q.state.contains("PgSleep"),
            "the wait event is the state: {}",
            q.state
        );
        assert!(!q.truncated && q.secs < 5.0, "{q:?}");
        assert!(
            q.details
                .iter()
                .any(|(k, v)| k == "wait_event" && v.contains("PgSleep")),
            "{:?}",
            q.details
        );
        match mon.explain(&ExplainTarget::Running(q)).unwrap() {
            Plan::Estimated { text, why } => {
                assert!(text.contains("Seq Scan on orders"), "{text}");
                assert!(why.contains("cannot show another session's plan"), "{why}");
            }
            other => panic!("PostgreSQL has no way to read the running plan, got {other:?}"),
        }
        job.join().unwrap();
    }

    #[test]
    #[ignore = "needs dev/compose.yaml"]
    fn a_lock_wait_names_its_blocker() {
        let (pid_tx, pid_rx) = mpsc::channel();
        let holder = thread::spawn(move || {
            let mut client = app_client(PORT, "shop");
            client
                .batch_execute(
                    "BEGIN; UPDATE orders SET amount = amount WHERE id = 1 /* pg2-holder */",
                )
                .unwrap();
            let pid: i32 = client
                .query_one("SELECT pg_backend_pid()", &[])
                .unwrap()
                .get(0);
            pid_tx.send(pid).unwrap();
            thread::sleep(Duration::from_secs(5));
            client.batch_execute("ROLLBACK").unwrap();
        });
        let holder_pid = pid_rx.recv().unwrap();
        let waiter = work("UPDATE orders SET amount = amount + 1 WHERE id = 1 /* pg2-waiter */");

        let mut mon = least();
        let q = wait_for(|| {
            let q = mon
                .poll()
                .unwrap()
                .running
                .into_iter()
                .find(|q| q.sql.contains("pg2-waiter"))?;
            q.details
                .iter()
                .any(|(k, _)| k == "blocked_by")
                .then_some(q)
        });
        let detail = |name: &str| {
            q.details
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.clone())
        };
        assert_eq!(detail("blocked_by"), Some(format!("pid {holder_pid}")));
        assert_eq!(detail("wait_event").as_deref(), Some("Lock:transactionid"));
        assert_eq!(q.state, "Lock:transactionid");

        // Planning an UPDATE needs UPDATE on the table; the monitoring role has SELECT only.
        match mon.explain(&ExplainTarget::Running(q.clone())).unwrap() {
            Plan::Unavailable(why) => assert!(why.contains("42501"), "{why}"),
            other => panic!("{other:?}"),
        }
        match admin().explain(&ExplainTarget::Running(q)).unwrap() {
            Plan::Estimated { text, .. } => assert!(text.contains("Update on orders"), "{text}"),
            other => panic!("{other:?}"),
        }
        holder.join().unwrap();
        waiter.join().unwrap();
    }

    #[test]
    #[ignore = "needs dev/compose.yaml"]
    fn a_statement_with_parameters_gets_a_generic_plan() {
        let job = thread::spawn(|| {
            let mut client = app_client(PORT, "shop");
            client
                .query(
                    "SELECT /* pg3 */ pg_sleep(4) FROM orders WHERE customer_id = $1 LIMIT 1",
                    &[&7i32],
                )
                .unwrap();
        });
        let mut mon = least();
        let q = running(&mut mon, "pg3");
        assert!(
            q.sql.contains("$1"),
            "the server shows the placeholder, not the value: {}",
            q.sql
        );

        match mon.explain(&ExplainTarget::Running(q)).unwrap() {
            Plan::Estimated { text, why } => {
                assert!(why.contains("generic plan"), "{why}");
                assert!(text.contains("orders") && text.contains("$1"), "{text}");
            }
            other => panic!("{other:?}"),
        }
        job.join().unwrap();
    }

    #[test]
    #[ignore = "needs dev/compose.yaml"]
    fn a_slow_digest_has_stats_and_a_plan_and_leaves_dbtops_own_queries_out() {
        for _ in 0..3 {
            work("SELECT /* pg4 */ count(*) FROM orders WHERE status = 'PAID' AND amount > 2.5")
                .join()
                .unwrap();
        }
        let mut mon = least();
        let d = digest(&mut mon, "amount > $2");

        assert!(
            d.calls >= 3 && d.total_ms > 0.0 && d.max_ms >= d.avg_ms,
            "{d:?}"
        );
        assert_eq!(d.schema, "shop");
        assert_eq!(
            (d.rows_examined_avg, d.no_index_used),
            (None, None),
            "PostgreSQL doesn't count these"
        );
        match mon.explain(&ExplainTarget::Digest(d)).unwrap() {
            Plan::Estimated { text, why } => {
                assert!(text.contains("Seq Scan on orders"), "{text}");
                assert!(why.contains("normalized text"), "{why}");
            }
            other => panic!("{other:?}"),
        }

        let sample = mon.poll().unwrap();
        assert_eq!(
            sample.rate_label, "qps",
            "pg_stat_statements can count statements"
        );
        let leaked: Vec<_> = sample
            .slow
            .iter()
            .map(|d| d.sample_sql.as_str())
            .filter(|sql| {
                let sql = sql.to_lowercase();
                sql.contains("pg_stat_activity")
                    || sql.contains("pg_stat_statements")
                    || sql.starts_with("explain")
            })
            .collect();
        assert!(
            leaked.is_empty(),
            "dbtop's own statements leaked into the list: {leaked:?}"
        );
    }

    #[test]
    #[ignore = "needs dev/compose.yaml"]
    fn text_cut_by_track_activity_query_size_is_flagged_and_not_explained() {
        let ids = (1..=400)
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join(",");
        let job = work(&format!(
            "SELECT /* pg5 */ pg_sleep(4) FROM orders WHERE id IN ({ids}) LIMIT 1"
        ));
        let mut mon = least();
        let q = running(&mut mon, "pg5");

        assert!(q.truncated);
        assert_eq!(
            q.sql.len(),
            1023,
            "track_activity_query_size 1024 keeps 1023 bytes"
        );
        match mon.explain(&ExplainTarget::Running(q)).unwrap() {
            Plan::Unavailable(why) => assert!(why.contains("cut the text"), "{why}"),
            other => panic!("{other:?}"),
        }
        job.join().unwrap();
    }

    #[test]
    #[ignore = "needs dev/compose.yaml"]
    fn a_finished_or_replaced_statement_is_reported_not_explained_wrongly() {
        let mut mon = least();

        let job = work("SELECT /* pg6 */ pg_sleep(1) FROM orders LIMIT 1");
        let finished = running(&mut mon, "pg6");
        job.join().unwrap();
        match mon.explain(&ExplainTarget::Running(finished)).unwrap() {
            Plan::Unavailable(why) => assert!(why.contains("finished"), "{why}"),
            other => panic!("{other:?}"),
        }

        let job = thread::spawn(|| {
            let mut client = app_client(PORT, "shop");
            client
                .batch_execute("SELECT /* pg6-first */ pg_sleep(1.5) FROM orders LIMIT 1")
                .unwrap();
            client
                .batch_execute("SELECT /* pg6-second */ pg_sleep(4) FROM orders LIMIT 1")
                .unwrap();
        });
        let first = running(&mut mon, "pg6-first");
        wait_for(|| {
            mon.poll()
                .unwrap()
                .running
                .into_iter()
                .find(|q| q.id == first.id && q.sql.contains("pg6-second"))
        });
        match mon.explain(&ExplainTarget::Running(first)).unwrap() {
            Plan::Unavailable(why) => assert!(why.contains("another statement"), "{why}"),
            other => panic!("{other:?}"),
        }
        job.join().unwrap();
    }

    #[test]
    #[ignore = "needs dev/compose.yaml"]
    fn a_statement_in_another_database_is_explained_there() {
        let job = work_on(
            PORT,
            "postgres",
            "SELECT /* pg7 */ pg_sleep(4) FROM pg_class LIMIT 1",
        );
        let mut mon = least(); // connected to `shop`
        let q = running(&mut mon, "pg7");
        assert_eq!(q.db, "postgres");

        match mon.explain(&ExplainTarget::Running(q)).unwrap() {
            Plan::Estimated { text, .. } => assert!(text.contains("pg_class"), "{text}"),
            other => panic!("{other:?}"),
        }
        job.join().unwrap();
    }

    #[test]
    #[ignore = "needs dev/compose.yaml"]
    fn a_second_statement_hidden_in_the_text_is_never_run() {
        let before: i64 = app_client(PORT, "shop")
            .query_one("SELECT count(*) FROM orders", &[])
            .unwrap()
            .get(0);
        let hostile = SlowQuery {
            digest: "x".into(),
            schema: "shop".into(),
            sample_sql: "SELECT 1; DELETE FROM orders".into(),
            truncated: false,
            calls: 1,
            total_ms: 1.0,
            avg_ms: 1.0,
            max_ms: 1.0,
            rows_examined_avg: None,
            no_index_used: None,
        };
        // Even a superuser must not be made to run it.
        for mon in [&mut least(), &mut admin()] {
            match mon
                .explain(&ExplainTarget::Digest(hostile.clone()))
                .unwrap()
            {
                Plan::Unavailable(why) => assert!(why.contains("';'"), "{why}"),
                other => panic!("{other:?}"),
            }
        }
        let after: i64 = app_client(PORT, "shop")
            .query_one("SELECT count(*) FROM orders", &[])
            .unwrap()
            .get(0);
        assert_eq!(before, after);
    }

    #[test]
    #[ignore = "needs dev/compose.yaml"]
    fn the_session_is_read_only_whatever_the_role_may_do() {
        let mut mon = admin();
        let err = mon
            .session()
            .unwrap()
            .client
            .batch_execute("UPDATE orders SET amount = amount WHERE id = 2")
            .unwrap_err();
        let message = err.as_db_error().expect("a server error").message();
        assert!(message.contains("read-only transaction"), "{message}");
    }

    #[test]
    #[ignore = "needs dev/compose.yaml"]
    fn it_reconnects_after_its_backend_is_terminated() {
        let mut mon = least();
        mon.poll().unwrap();
        let pid: i32 = mon
            .session()
            .unwrap()
            .client
            .query_one("SELECT pg_backend_pid()", &[])
            .unwrap()
            .get(0);

        admin()
            .session()
            .unwrap()
            .client
            .query("SELECT pg_terminate_backend($1)", &[&pid])
            .unwrap();
        assert!(mon.poll().is_err(), "the dead connection is reported");
        assert!(mon.session.is_none());
        assert!(mon.poll().is_ok(), "and the next poll connects again");
    }

    /// The throwaway CA that signed the dev Postgres's certificate (written by dev/tls-setup.sh).
    const CA: &str = "dev/certs/postgres.pem";

    fn tls_target(port: u16, tls: Tls, ca: Option<&str>) -> Target {
        Target {
            tls,
            tls_ca: ca.map(std::path::PathBuf::from),
            ..target("dbtop", "dbtop", port, "shop")
        }
    }

    /// What the server says about a connection made with these settings.
    fn encrypted(target: &Target) -> Option<bool> {
        Postgres::connect(target)
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
                !text.contains("refused") && !text.contains("timed out"),
                "{text}"
            );
        };

        // The dev CA is in no trust store, and none was given.
        let err = Postgres::connect(&by_name(Tls::Verify, None))
            .err()
            .expect("untrusted");
        assert!(format!("{err:#}").contains("tls = \"verify\""), "{err:#}");
        not_a_connection_problem(&err);

        // Someone else's CA.
        let wrong = by_name(Tls::Verify, Some("dev/certs/mysql.pem"));
        let err = Postgres::connect(&wrong)
            .err()
            .expect("a CA that did not sign it must not do");
        not_a_connection_problem(&err);

        // The right CA, but the address 127.0.0.1 is not among the names the certificate holds.
        // Require checks nothing and accepts; Verify must not.
        let mut by_ip = tls_target(PORT, Tls::Verify, Some(CA));
        assert_eq!(by_ip.host, "127.0.0.1");
        let err = Postgres::connect(&by_ip)
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
            encrypted(&tls_target(PORT_13, Tls::Prefer, None)),
            Some(false)
        );
        assert_eq!(encrypted(&tls_target(PORT_13, Tls::Off, None)), Some(false));
        for tls in [Tls::Require, Tls::Verify] {
            assert!(
                Postgres::connect(&tls_target(PORT_13, tls, None)).is_err(),
                "{tls:?} must not fall back to plain text"
            );
        }
    }

    #[test]
    #[ignore = "needs dev/compose.yaml"]
    fn a_wrong_password_fails_before_anything_else() {
        let err = Postgres::connect(&target("dbtop", "nope", PORT, "shop"))
            .err()
            .expect("must fail");
        assert!(
            format!("{err:#}").contains("password authentication failed"),
            "{err:#}"
        );
    }

    #[test]
    #[ignore = "needs dev/compose.yaml"]
    fn without_the_extension_it_says_how_to_get_it_and_counts_transactions() {
        let mut mon = monitor("dbtop", "dbtop", PORT, "template1"); // no extension created there
        let sample = mon.poll().unwrap();
        assert!(sample.slow.is_empty());
        assert!(
            sample
                .slow_note
                .unwrap()
                .contains("CREATE EXTENSION pg_stat_statements")
        );
        assert_eq!(sample.rate_label, "tps");
    }

    #[test]
    #[ignore = "needs dev/compose.yaml"]
    fn a_role_that_cannot_see_other_sessions_is_told_instead_of_shown_an_empty_list() {
        let job = work("SELECT /* pg8 */ pg_sleep(3) FROM orders LIMIT 1");
        thread::sleep(Duration::from_millis(500));
        let mut mon = monitor("plain", "plain", PORT, "shop");
        let sample = mon.poll().unwrap();

        assert!(
            sample.running.is_empty(),
            "the role can't see it: {:?}",
            sample.running
        );
        assert!(sample.running_note.unwrap().contains("pg_read_all_stats"));
        assert!(
            least().poll().unwrap().running_note.is_none(),
            "pg_monitor sees everything"
        );
        job.join().unwrap();
    }

    #[test]
    #[ignore = "needs: docker compose -f dev/compose.yaml --profile extra up -d --wait"]
    fn postgres_13_has_no_generic_plan_but_still_lists_slow_statements() {
        let job = thread::spawn(|| {
            let mut client = app_client(PORT_13, "shop");
            client
                .query(
                    "SELECT /* pg13 */ pg_sleep(4) FROM orders WHERE customer_id = $1 LIMIT 1",
                    &[&7i32],
                )
                .unwrap();
        });
        let mut mon = monitor("dbtop", "dbtop", PORT_13, "shop");
        let q = running(&mut mon, "pg13");
        match mon.explain(&ExplainTarget::Running(q)).unwrap() {
            Plan::Unavailable(why) => assert!(
                why.contains("only PostgreSQL 16+") && why.contains("13"),
                "{why}"
            ),
            other => panic!("planning with NULLs would be misleading, got {other:?}"),
        }
        job.join().unwrap();

        // total_time / max_time rather than total_exec_time / max_exec_time
        work_on(
            PORT_13,
            "shop",
            "SELECT /* pg13b */ count(*) FROM orders WHERE amount > 7.5",
        )
        .join()
        .unwrap();
        let d = digest(&mut mon, "amount > $1");
        assert!(d.calls >= 1 && d.total_ms > 0.0);
        match mon.explain(&ExplainTarget::Digest(d)).unwrap() {
            Plan::Unavailable(why) => assert!(why.contains("only PostgreSQL 16+"), "{why}"),
            other => panic!("{other:?}"),
        }
    }
}
