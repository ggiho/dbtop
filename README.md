# dbtop

A terminal monitor for MySQL and Postgres: running queries, slow queries, and for any of them the
full SQL, details and execution plan. Rust, [ratatui](https://ratatui.rs), braille graphs.

```
cargo run -- --demo                 # generated data, no database needed
cargo run -- "Example DEV"          # a server from the dbq list (below)
cargo run -- <connection>           # a connection from dbtop's own config
cargo run -- --config FILE <name>   # only that config file
```

Needs a terminal of at least 80x20.

| Key | |
|---|---|
| `↑↓` `j k` `PgUp PgDn` `g G` | move |
| `Tab` `1` `2` | running / slow |
| `Enter` | details: full SQL, info, plan. `↑↓ g G` scroll, `Esc` closes |
| `q` `Ctrl-C` | quit |

## The servers dbq knows

`dbtop "Example DEV"` reads the same list as `dbq` and the interactive `db` command,
`~/.config/mysql/hosts` (`name:host:port:user[:type[:database]]`), and the password from the same
macOS Keychain item (service `mysql-db-connect`, account = the user). Matching is dbq's: case does
not matter, an exact name wins, otherwise a name containing the text, and several matches are an
error that lists them. The status line shows the name, so it is plain which server you are on.

dbtop's own config (below) is looked at first, so it can override or add to the list. A `Redshift`
entry is refused: dbtop does not support it.

## Config

`~/.config/dbtop/config.toml` (or under `$XDG_CONFIG_HOME`, or `--config`):

```toml
[connections.example-dev]
kind = "mysql"                 # or "postgres"
host = "example-dev.example.test"
port = 3306                    # optional: 3306 / 5432
user = "dbtop"
password_cmd = "security find-generic-password -s dbtop-example-dev -w"
database = "example"           # optional; for Postgres, the one with pg_stat_statements
tls = "prefer"                 # off | prefer (default) | require | verify
tls_ca = "~/rds-bundle.pem"    # optional, only with tls = "verify"
```

`password_cmd` keeps the secret out of the file. A plaintext `password` is accepted only if the
file is private to you (`chmod 600`), like ssh and libpq. Misspelt keys are errors.

## TLS

| `tls` | |
|---|---|
| `prefer` (default) | encrypt if the server can, otherwise carry on unencrypted; the certificate is not checked. This is what the `mysql` client (so `dbq`) and `psql` do, so a server from the dbq list behaves as it does in `dbq`. |
| `require` | encrypt or fail; the certificate is not checked |
| `verify` | encrypt or fail, and check that the certificate chains to a trusted CA **and names the host you typed** |
| `off` | never encrypt |

`tls_ca` adds trusted roots (a PEM file; a bundle with many certificates is fine, such as the
[RDS global bundle](https://truststore.pki.rds.amazonaws.com/global/global-bundle.pem)) to the
Mozilla roots dbtop already trusts, and only goes with `verify`. `prefer` and `require` encrypt
but do not authenticate the server, so they do not stop someone in the path from impersonating
it; `verify` does.

**`verify` needs a host name the certificate holds.** An RDS certificate names the instance and
cluster endpoints (`…rds.amazonaws.com`); a CNAME alias in front of them is not among its names,
and is refused with a message that lists the names it does hold. Put the endpoint in `host`.
This was checked against an Aurora MySQL 8.0 server: the alias was refused, the endpoint was
accepted with the RDS bundle.

dbtop carries its own TLS library (rustls) rather than the system's. RDS certificates have no
extended-key-usage extension, which RFC 5280 allows and which macOS's own TLS library rejects
("The extended key usage is not valid"), so `verify` could never have worked against RDS there.

The status line says what the server reports for the connection, `tls` or a warning
`unencrypted`, not what was asked for: with `prefer` the two can differ. The dbq list has no
column for TLS, so its servers always use `prefer`; give one a `[connections.*]` entry in
dbtop's config to verify it.

## Privileges

Both sessions are read-only and bounded (`READ ONLY`, 5 s statement timeout), and the accounts
need no write privilege. What was verified, on MySQL 8.0 and Postgres 13 and 16:

```sql
-- MySQL
CREATE USER 'dbtop'@'%' IDENTIFIED BY '...';
GRANT PROCESS ON *.* TO 'dbtop'@'%';                  -- see other users' queries
GRANT SELECT ON performance_schema.* TO 'dbtop'@'%';  -- slow list, per-query details, lock waits
GRANT SELECT ON some_schema.* TO 'dbtop'@'%';         -- for each schema whose plans you want

-- Postgres
CREATE ROLE dbtop LOGIN PASSWORD '...' IN ROLE pg_monitor;   -- pg_read_all_stats
GRANT SELECT ON ALL TABLES IN SCHEMA public TO dbtop;         -- for plans
-- slow list: pg_stat_statements in shared_preload_libraries (restart), then
-- CREATE EXTENSION pg_stat_statements in the database dbtop connects to
```

Without them dbtop says so in the screen (a banner, or why the slow list is empty) instead of
showing an empty list that looks like an idle server.

## Plans: real, or an estimate

Never `EXPLAIN ANALYZE` (it would run the statement). A plan is fetched only when you press
`Enter`, and an estimate is always labelled as one.

- **MySQL, running query**: `EXPLAIN FOR CONNECTION` gives the plan actually in use, but MySQL
  asks for the privileges of the explained connection's owner as well (bug #95850), so a
  least-privilege account is refused. dbtop then plans the statement's *text* and says why.
  INSERT/UPDATE/DELETE text is not explained that way: MySQL wants write privileges even to
  `EXPLAIN` it.
- **Postgres**: another session's plan cannot be read, so it is always the planner's estimate for
  the same text. Text with `$1` placeholders gets a generic plan on 16+; older servers say so.
- **Slow query** (both): the digest's sample text planned now.
- A statement the server cut short (`track_activity_query_size`, `performance_schema_max_sql_text_length`)
  is flagged and not explained; one that finished, or whose connection moved on to another
  statement, is reported as such rather than explained wrongly.
- Text read from a server is run again only if it is a single data statement.

## Limits

- Redshift is not supported.
- Tested on MySQL 8.0 and Postgres 13 and 16 in containers, and on Aurora MySQL 8.0.42 and
  Aurora PostgreSQL 17.7 (a DEV and a test cluster): connecting with `prefer` and `verify`, the
  running list and its details, plans (including a statement in another database), and the
  explanations shown when `performance_schema` is off or `pg_stat_statements` is not installed.
  Not tested: MySQL 5.7/8.4/9, MariaDB, Postgres 14/15, the slow list and lock waits on Aurora
  (no digests were available there), and any PROD server.
- The slow list ranks statements by total time (`performance_schema` digests,
  `pg_stat_statements`); it does not read slow-log files, which RDS/Aurora don't expose.
- With `performance_schema` off (the Aurora default) the running list still works, in whole
  seconds, without per-query details.

## Development

```
docker compose -f dev/compose.yaml up -d --wait     # MySQL 8.0 on 13306, Postgres 16 on 15432
dev/workload.sh                                      # keeps queries running to look at
cargo run -- --config dev/config.toml local-mysql    # or local-pg

cargo test                                           # offline
cargo test -- --ignored                              # against the databases above
docker compose -f dev/compose.yaml --profile extra up -d --wait   # + performance_schema OFF, Postgres 13
cargo test -- --include-ignored                      # everything

docker compose -f dev/compose.yaml --profile extra down -v
```

The databases are bound to the loopback addresses with throwaway passwords, and offer TLS with a
certificate made at start-up by `dev/tls-setup.sh` (its CA is written to `dev/certs/`), so the
`verify` tests check real certificates. The certificate names `localhost` and, on purpose, not
127.0.0.1: connecting by IP is the host-name mismatch the tests expect to be refused. Never point
the tests at anything else.
