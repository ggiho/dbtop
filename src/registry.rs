//! The server list that `dbq` and the interactive `db` command share, so that
//! `dbtop "Example DEV"` works with nothing more to set up.
//!
//! `~/.config/mysql/hosts` holds one server per line, `name:host:port:user[:type[:database]]`.
//! Passwords are in the macOS Keychain: service `mysql-db-connect`, account = the user.
//! Matching is dbq's own: case-insensitive; an exact name wins, otherwise a name containing the text.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

use crate::config::{Kind, Secret, Target, Tls};

const KEYCHAIN_SERVICE: &str = "mysql-db-connect";

pub struct Registry {
    servers: Vec<Server>,
}

struct Server {
    name: String,
    host: String,
    port: u16,
    user: String,
    kind: Kind,
    database: Option<String>,
}

enum Found<'a> {
    One(&'a Server),
    Nothing,
    Several(Vec<String>),
}

/// `$HOME/.config/mysql/hosts`.
pub fn default_path() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok().filter(|h| !h.is_empty())?;
    Some(Path::new(&home).join(".config").join("mysql").join("hosts"))
}

impl Registry {
    pub fn load(path: &Path) -> Result<Registry> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read {}", path.display()))?;
        Ok(Registry::parse(&text))
    }

    /// Lines that don't have a name, host, port and user are skipped, like blank lines and `#`
    /// comments.
    pub fn parse(text: &str) -> Registry {
        let servers = text
            .lines()
            .filter_map(|line| {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    return None;
                }
                let mut fields = line.splitn(6, ':');
                let (name, host) = (fields.next()?, fields.next()?);
                let port = fields.next()?.parse().ok()?;
                let user = fields.next()?;
                if name.is_empty() || host.is_empty() || user.is_empty() {
                    return None;
                }
                // As dbq: pg/postgres select psql, anything else is MySQL.
                let kind = match fields.next().unwrap_or("") {
                    "pg" | "postgres" | "postgresql" => Kind::Postgres,
                    _ => Kind::Mysql,
                };
                Some(Server {
                    name: name.to_string(),
                    host: host.to_string(),
                    port,
                    user: user.to_string(),
                    kind,
                    database: fields.next().filter(|d| !d.is_empty()).map(str::to_string),
                })
            })
            .collect();
        Registry { servers }
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.servers.iter().map(|s| s.name.as_str())
    }

    fn find(&self, query: &str) -> Found<'_> {
        let query = query.to_lowercase();
        if let Some(exact) = self.servers.iter().find(|s| s.name.to_lowercase() == query) {
            return Found::One(exact);
        }
        let partial: Vec<_> = self
            .servers
            .iter()
            .filter(|s| s.name.to_lowercase().contains(&query))
            .collect();
        match partial.as_slice() {
            [] => Found::Nothing,
            [one] => Found::One(one),
            many => Found::Several(many.iter().map(|s| s.name.clone()).collect()),
        }
    }

    /// `None` if no server has that name. The password is fetched only for the server that
    /// matched, never for the others.
    pub fn target(
        &self,
        query: &str,
        password: impl FnOnce(&str) -> Result<Secret>,
    ) -> Result<Option<Target>> {
        let server = match self.find(query) {
            Found::One(server) => server,
            Found::Nothing => return Ok(None),
            Found::Several(names) => {
                bail!(
                    "{query:?} matches several servers in the dbq list: {}; be more specific",
                    names.join(", ")
                )
            }
        };
        let password = password(&server.user)
            .with_context(|| format!("{}: no password for {}", server.name, server.user))?;
        Ok(Some(Target {
            name: server.name.clone(),
            kind: server.kind,
            host: server.host.clone(),
            port: server.port,
            user: server.user.clone(),
            password: Some(password),
            database: server.database.clone(),
            // What dbq's own mysql/psql do, since the list has nowhere to say otherwise.
            tls: Tls::Prefer,
            tls_ca: None,
        }))
    }
}

/// The password `dbq` would use: the Keychain item for this account.
#[cfg(target_os = "macos")]
pub fn keychain_password(user: &str) -> Result<Secret> {
    // Absolute path and no shell: the user name goes in as an argument, never into a command line.
    let out = Command::new("/usr/bin/security")
        .args([
            "find-generic-password",
            "-s",
            KEYCHAIN_SERVICE,
            "-a",
            user,
            "-w",
        ])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .context("cannot run `security`")?;
    let text = String::from_utf8(out.stdout).unwrap_or_default();
    let password = text.trim_end_matches(['\n', '\r']);
    if !out.status.success() || password.is_empty() {
        bail!(
            "the Keychain has no item for it; save one with: security add-generic-password -s {KEYCHAIN_SERVICE} -a {user} -w"
        );
    }
    Ok(Secret::new(password))
}

#[cfg(not(target_os = "macos"))]
pub fn keychain_password(_: &str) -> Result<Secret> {
    bail!(
        "the dbq server list keeps passwords in the macOS Keychain, which this system does not have"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOSTS: &str = "\
# servers
Example DEV:example-dev.example.test:3306:alice
Example UAT:example-uat.example.test:3306:alice

  Example TEST:example-test.example.test:3306:x
Sample warehouse:warehouse.example.test:5439:analyst:pg:datamart
Pinned:pinned.example.test:3306:u:mysql:app:with:colons
Sample DEV:sample-1.example.test:3306:sample
Sample DEV:sample-2.example.test:3306:sample
Sample SQA:sample-3.example.test:3306:sample
Odd type:odd.example.test:3306:u:oracle
broken line without a port
No Port:host::user
";

    fn registry() -> Registry {
        Registry::parse(HOSTS)
    }

    fn names(r: &Registry) -> Vec<&str> {
        r.names().collect()
    }

    fn secret(user: &str) -> Result<Secret> {
        Ok(Secret::new(&format!("pw-of-{user}")))
    }

    #[test]
    fn parses_the_hosts_file_like_dbq_reads_it() {
        let r = registry();
        assert_eq!(
            names(&r),
            [
                "Example DEV",
                "Example UAT",
                "Example TEST",
                "Sample warehouse",
                "Pinned",
                "Sample DEV",
                "Sample DEV",
                "Sample SQA",
                "Odd type"
            ],
            "comments, blanks and lines without a port are skipped"
        );
    }

    #[test]
    fn type_and_database_are_optional_fields() {
        let r = registry();
        let get = |q: &str| r.target(q, secret).unwrap().unwrap();

        let mysql = get("Example DEV");
        assert_eq!(
            (mysql.kind, mysql.port, mysql.database.as_deref()),
            (Kind::Mysql, 3306, None)
        );
        assert_eq!(mysql.name, "Example DEV");

        let pg = get("Sample warehouse");
        assert_eq!(
            (pg.kind, pg.port, pg.database.as_deref()),
            (Kind::Postgres, 5439, Some("datamart"))
        );

        assert_eq!(
            get("Pinned").database.as_deref(),
            Some("app:with:colons"),
            "the last field keeps its colons"
        );
        assert_eq!(
            get("Odd type").kind,
            Kind::Mysql,
            "anything but pg/postgres is MySQL, as in dbq"
        );
    }

    #[test]
    fn matching_is_dbqs_exact_wins_then_partial_ignoring_case() {
        let r = registry();
        let user_of = |q: &str| r.target(q, secret).map(|t| t.map(|t| t.host));
        assert_eq!(
            user_of("example dev").unwrap().as_deref(),
            Some("example-dev.example.test")
        );
        assert_eq!(
            user_of("EXAMPLE UAT").unwrap().as_deref(),
            Some("example-uat.example.test")
        );
        assert_eq!(
            user_of("sqa").unwrap().as_deref(),
            Some("sample-3.example.test"),
            "a unique part is enough"
        );
        assert_eq!(
            user_of("Sample DEV").unwrap().as_deref(),
            Some("sample-1.example.test"),
            "a duplicate: the first"
        );
    }

    #[test]
    fn an_ambiguous_or_unknown_name_is_not_guessed() {
        let r = registry();
        let err = r.target("Example", secret).unwrap_err().to_string();
        assert!(
            err.contains("Example DEV")
                && err.contains("Example UAT")
                && err.contains("be more specific"),
            "{err}"
        );
        assert!(
            r.target("sample", secret).is_err(),
            "two Sample DEV entries and Sample SQA"
        );
        assert!(r.target("no such server", secret).unwrap().is_none());
    }

    #[test]
    fn the_password_comes_from_the_matched_servers_user_only() {
        let r = registry();
        let mut asked = Vec::new();
        let target = r
            .target("Sample warehouse", |user| {
                asked.push(user.to_string());
                secret(user)
            })
            .unwrap()
            .unwrap();
        assert_eq!(asked, ["analyst"]);
        assert_eq!(target.password.unwrap().expose(), "pw-of-analyst");

        let mut asked = false;
        let _ = r.target("nope", |_| {
            asked = true;
            secret("x")
        });
        assert!(!asked, "no password lookup for a name that matched nothing");
    }

    #[test]
    fn a_missing_keychain_item_names_the_server_and_the_fix() {
        let r = registry();
        let err = r
            .target("Example DEV", |user| bail!("the Keychain has no item; save one with: security add-generic-password -a {user}"))
            .unwrap_err();
        let shown = format!("{err:#}");
        assert!(
            shown.contains("Example DEV")
                && shown.contains("alice")
                && shown.contains("add-generic-password"),
            "{shown}"
        );
    }

    #[test]
    fn the_password_never_prints() {
        let target = registry().target("Example DEV", secret).unwrap().unwrap();
        assert!(!format!("{target:?}").contains("pw-of"));
    }

    #[test]
    fn an_empty_file_has_no_servers() {
        let r = Registry::parse("");
        assert!(r.names().next().is_none());
        assert!(r.target("anything", secret).unwrap().is_none());
    }
}
