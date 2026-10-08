//! Statement text read from a server (its process list, its statement digests) is later run
//! again under EXPLAIN. Before that, make sure it is exactly one data statement.
//!
//! This is defence in depth: the sessions are read-only and the monitoring accounts hold no
//! write privileges. It errs towards refusing: a `;` inside a string literal blocks the plan
//! too, which costs one plan and never a safety margin.

const ALLOWED: [&str; 9] = [
    "SELECT", "WITH", "INSERT", "UPDATE", "DELETE", "REPLACE", "MERGE", "VALUES", "TABLE",
];

/// `Ok` if `sql` is a single statement that EXPLAIN may wrap; `Err` says why not.
pub fn explainable(sql: &str) -> Result<(), String> {
    // MySQL runs the contents of /*! ... */ as code, so a comment can hide a statement.
    if sql.contains("/*!") || sql.contains("/*M!") {
        return Err("it contains an executable comment (/*! ... */)".into());
    }
    let body = sql.trim_end_matches(|c: char| c.is_whitespace() || c == ';');
    if body.contains(';') {
        return Err("it contains ';', so it may be more than one statement".into());
    }
    match first_word(body) {
        Some(word) if ALLOWED.contains(&word.as_str()) => Ok(()),
        Some(word) => Err(format!(
            "it starts with {word}, not a SELECT/INSERT/UPDATE/DELETE"
        )),
        None => Err("it does not start with a statement keyword".into()),
    }
}

/// Whether the statement changes data. Explaining those needs write privileges on MySQL.
pub fn writes_data(sql: &str) -> bool {
    let body = sql.trim_end_matches(|c: char| c.is_whitespace() || c == ';');
    matches!(
        first_word(body).as_deref(),
        Some("INSERT" | "UPDATE" | "DELETE" | "REPLACE" | "MERGE")
    )
}

/// The first keyword, skipping whitespace, comments and opening parentheses.
fn first_word(mut s: &str) -> Option<String> {
    loop {
        s = s.trim_start();
        if let Some(rest) = s.strip_prefix("/*") {
            s = &rest[rest.find("*/")? + 2..];
        } else if s.starts_with("--") || s.starts_with('#') {
            s = s.split_once('\n')?.1;
        } else if let Some(rest) = s.strip_prefix('(') {
            s = rest;
        } else {
            break;
        }
    }
    let end = s
        .find(|c: char| !c.is_ascii_alphabetic())
        .unwrap_or(s.len());
    (end > 0).then(|| s[..end].to_ascii_uppercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_data_statements_pass() {
        for sql in [
            "SELECT 1",
            "select * from t where a = 'x'",
            "  /* hint */ SELECT 1;",
            "-- note\nUPDATE t SET a = 1",
            "# note\nDELETE FROM t WHERE id = 1",
            "WITH x AS (SELECT 1) SELECT * FROM x",
            "(SELECT 1) UNION (SELECT 2)",
            "INSERT INTO t VALUES (1)",
            "SELECT 1 ;; ",
            "SELECT /*+ INDEX(t i) */ * FROM t",
            "/* 월말 정산 */ SELECT 1",
        ] {
            assert_eq!(explainable(sql), Ok(()), "{sql:?}");
        }
    }

    #[test]
    fn tells_writes_from_reads() {
        for sql in [
            "INSERT INTO t VALUES (1)",
            " /* c */ update t set a = 1",
            "DELETE FROM t",
            "REPLACE INTO t VALUES (1)",
        ] {
            assert!(writes_data(sql), "{sql:?}");
        }
        for sql in [
            "SELECT 1",
            "WITH x AS (SELECT 1) SELECT * FROM x",
            "(SELECT 1)",
            "TABLE t",
            "",
        ] {
            assert!(!writes_data(sql), "{sql:?}");
        }
    }

    #[test]
    fn a_second_statement_is_refused() {
        for sql in [
            "SELECT 1; DROP TABLE t",
            "SELECT 1;DROP TABLE t",
            "SELECT 1; -- x\nDELETE FROM t",
            "UPDATE t SET a = 1; UPDATE t SET b = 2",
        ] {
            let why = explainable(sql).unwrap_err();
            assert!(why.contains("';'"), "{sql:?} -> {why}");
        }
    }

    #[test]
    fn a_semicolon_inside_a_string_is_refused_too_by_design() {
        assert!(explainable("SELECT 'a;b'").is_err());
    }

    #[test]
    fn executable_comments_are_refused() {
        assert!(explainable("/*!50000 DROP TABLE t */ SELECT 1").is_err());
        assert!(explainable("SELECT 1 /*!50000 , 2 */").is_err());
        assert!(explainable("SELECT 1 /*M!100000 , 2 */").is_err());
    }

    #[test]
    fn anything_but_a_data_statement_is_refused() {
        for sql in [
            "DROP TABLE t",
            "EXPLAIN SELECT 1",
            "CALL p()",
            "SET x = 1",
            "KILL 1",
            "SHOW TABLES",
            "ANALYZE TABLE t",
            "TRUNCATE t",
            "",
            "   ",
            ";",
            "-- only a comment",
            "/* only a comment */",
            "/* unterminated SELECT 1",
            "123",
        ] {
            assert!(explainable(sql).is_err(), "{sql:?} should be refused");
        }
    }
}
