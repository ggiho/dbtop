//! Connections live in `~/.config/dbtop/config.toml`:
//!
//! ```toml
//! [connections.example-dev]
//! kind = "mysql"                 # or "postgres"
//! host = "example-dev.example.test"
//! port = 3306                    # optional; 3306 / 5432
//! user = "dbtop"
//! password_cmd = "security find-generic-password -s dbtop-example-dev -w"
//! database = "example"           # optional
//! tls = "prefer"                 # off | prefer (default) | require | verify
//! tls_ca = "~/rds-bundle.pem"    # optional; extra trusted roots, only with tls = "verify"
//! ```
//!
//! `password_cmd` keeps the secret out of the file; a plaintext `password` is accepted for local
//! throwaway databases, but only if the file is private to its owner.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

/// A password that never prints.
#[derive(Clone, Deserialize)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    pub fn new(password: &str) -> Secret {
        Secret(password.to_string())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(***)")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Mysql,
    Postgres,
}

/// How to use TLS. The default, `prefer`, is what the `mysql` client (so `dbq`) and `psql` do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tls {
    /// No encryption.
    Off,
    /// Encrypt when the server can, otherwise carry on unencrypted. The certificate is not checked.
    #[default]
    Prefer,
    /// Encrypt or fail. The certificate is not checked.
    Require,
    /// Encrypt or fail, and check that the certificate chains to a trusted CA and names the host.
    Verify,
}

impl Kind {
    fn default_port(self) -> u16 {
        match self {
            Kind::Mysql => 3306,
            Kind::Postgres => 5432,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    kind: Kind,
    host: String,
    port: Option<u16>,
    user: String,
    password: Option<Secret>,
    password_cmd: Option<String>,
    database: Option<String>,
    tls: Option<Tls>,
    tls_ca: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    connections: BTreeMap<String, Entry>,
}

/// One connection, ready to use.
#[derive(Debug, Clone)]
pub struct Target {
    /// What the user typed to pick it, e.g. "Example DEV".
    pub name: String,
    pub kind: Kind,
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: Option<Secret>,
    pub database: Option<String>,
    pub tls: Tls,
    /// Extra trusted root certificates (PEM), for `Tls::Verify`.
    pub tls_ca: Option<PathBuf>,
}

impl Target {
    /// A target on 127.0.0.1, for the throwaway databases in dev/compose.yaml.
    #[cfg(test)]
    pub fn dev(kind: Kind, port: u16, user: &str, password: &str) -> Target {
        Target {
            name: "dev".into(),
            kind,
            host: "127.0.0.1".into(),
            port,
            user: user.into(),
            password: Some(Secret(password.into())),
            database: None,
            tls: Tls::default(),
            tls_ca: None,
        }
    }

    /// For the status line, so it is plain which server this is: never contains the password.
    pub fn label(&self) -> String {
        let kind = match self.kind {
            Kind::Mysql => "mysql",
            Kind::Postgres => "postgres",
        };
        format!(
            "{} · {kind} {}@{}:{}",
            self.name,
            self.user,
            short_host(&self.host),
            self.port
        )
    }
}

/// `example-dev.cluster-abc.us-east-1.rds.amazonaws.com` is `example-dev` on a status line;
/// an IP address stays whole.
fn short_host(host: &str) -> &str {
    if host.parse::<std::net::IpAddr>().is_ok() {
        return host;
    }
    host.split('.').next().unwrap_or(host)
}

impl Config {
    pub fn load(path: &Path) -> Result<Config> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read {}", path.display()))?;
        let config: Config = toml::from_str(&text)
            .with_context(|| format!("{} is not a valid config", path.display()))?;
        if config.connections.values().any(|e| e.password.is_some()) {
            require_private(path)?;
        }
        if config
            .connections
            .values()
            .any(|e| e.password_cmd.is_some())
        {
            require_not_writable_by_others(path)?;
        }
        Ok(config)
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.connections.keys().map(String::as_str)
    }

    pub fn has(&self, name: &str) -> bool {
        self.connections.contains_key(name)
    }

    pub fn target(&self, name: &str) -> Result<Target> {
        let Some(entry) = self.connections.get(name) else {
            let have: Vec<_> = self.names().collect();
            bail!(
                "no connection named {name:?} (configured: {})",
                have.join(", ")
            );
        };
        let password = match (&entry.password, &entry.password_cmd) {
            (Some(_), Some(_)) => bail!("{name}: set either password or password_cmd, not both"),
            (Some(password), None) => Some(password.clone()),
            (None, Some(cmd)) => Some(
                run_password_cmd(cmd).with_context(|| format!("{name}: password_cmd failed"))?,
            ),
            (None, None) => None,
        };
        let tls = entry.tls.unwrap_or_default();
        let tls_ca = match (&entry.tls_ca, tls) {
            (None, _) => None,
            (Some(_), tls) if tls != Tls::Verify => {
                bail!("{name}: tls_ca only applies to tls = \"verify\"")
            }
            (Some(path), _) => {
                let path = expand_home(path, std::env::var("HOME").ok().as_deref());
                std::fs::metadata(&path)
                    .with_context(|| format!("{name}: cannot read tls_ca {}", path.display()))?;
                Some(path)
            }
        };
        Ok(Target {
            name: name.to_string(),
            kind: entry.kind,
            host: entry.host.clone(),
            port: entry.port.unwrap_or_else(|| entry.kind.default_port()),
            user: entry.user.clone(),
            password,
            database: entry.database.clone(),
            tls,
            tls_ca,
        })
    }
}

/// `~/x` is `$HOME/x`; the shell does that for a command line, not for a file's contents.
fn expand_home(path: &str, home: Option<&str>) -> PathBuf {
    match (path.strip_prefix("~/"), home) {
        (Some(rest), Some(home)) if !home.is_empty() => Path::new(home).join(rest),
        _ => PathBuf::from(path),
    }
}

/// `$XDG_CONFIG_HOME/dbtop/config.toml`, else `$HOME/.config/dbtop/config.toml`.
pub fn default_path() -> Option<PathBuf> {
    config_path(
        std::env::var("XDG_CONFIG_HOME").ok().as_deref(),
        std::env::var("HOME").ok().as_deref(),
    )
}

fn config_path(xdg: Option<&str>, home: Option<&str>) -> Option<PathBuf> {
    let base = match (
        xdg.filter(|s| !s.is_empty()),
        home.filter(|s| !s.is_empty()),
    ) {
        (Some(xdg), _) => PathBuf::from(xdg),
        (None, Some(home)) => Path::new(home).join(".config"),
        (None, None) => return None,
    };
    Some(base.join("dbtop").join("config.toml"))
}

/// Like ssh and libpq: a file holding a password must not be readable by anyone else.
#[cfg(unix)]
fn require_private(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(path)?.permissions().mode();
    if mode & 0o077 != 0 {
        bail!(
            "{} holds a plaintext password but is readable by other users (mode {:o}); run: chmod 600 {}",
            path.display(),
            mode & 0o777,
            path.display()
        );
    }
    Ok(())
}

#[cfg(not(unix))]
fn require_private(_: &Path) -> Result<()> {
    Ok(())
}

/// `password_cmd` runs whatever this file says, so whoever can edit it can run commands as you.
#[cfg(unix)]
fn require_not_writable_by_others(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(path)?.permissions().mode();
    if mode & 0o022 != 0 {
        bail!(
            "{} runs a password_cmd but other users can write to it (mode {:o}); run: chmod go-w {}",
            path.display(),
            mode & 0o777,
            path.display()
        );
    }
    Ok(())
}

#[cfg(not(unix))]
fn require_not_writable_by_others(_: &Path) -> Result<()> {
    Ok(())
}

fn run_password_cmd(cmd: &str) -> Result<Secret> {
    let out = Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .stdin(Stdio::null())
        .stderr(Stdio::inherit())
        .output()
        .context("cannot run it")?;
    if !out.status.success() {
        bail!("it exited with {}", out.status);
    }
    let text = String::from_utf8(out.stdout).context("its output is not UTF-8")?;
    let password = text.trim_end_matches(['\n', '\r']);
    if password.is_empty() {
        bail!("it printed nothing");
    }
    Ok(Secret(password.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
        [connections.local-mysql]
        kind = "mysql"
        host = "127.0.0.1"
        port = 13306
        user = "dbtop"
        password_cmd = "echo s3cret"
        database = "shop"

        [connections.local-pg]
        kind = "postgres"
        host = "127.0.0.1"
        user = "dbtop"
    "#;

    fn parse(text: &str) -> Config {
        toml::from_str(text).unwrap()
    }

    #[test]
    fn resolves_a_connection_with_defaults() {
        let config = parse(SAMPLE);
        assert_eq!(
            config.names().collect::<Vec<_>>(),
            ["local-mysql", "local-pg"]
        );

        let pg = config.target("local-pg").unwrap();
        assert_eq!(pg.port, 5432, "default port by kind");
        assert!(pg.password.is_none() && pg.database.is_none());

        let my = config.target("local-mysql").unwrap();
        assert_eq!((my.port, my.database.as_deref()), (13306, Some("shop")));
        assert_eq!(my.label(), "local-mysql · mysql dbtop@127.0.0.1:13306");
    }

    #[test]
    fn the_label_names_the_server_briefly() {
        let mut target = parse(SAMPLE).target("local-pg").unwrap();
        assert_eq!(
            target.label(),
            "local-pg · postgres dbtop@127.0.0.1:5432",
            "an IP address stays whole"
        );

        target.name = "Example DEV".into();
        target.host = "example-dev.cluster-abc123.us-east-1.rds.amazonaws.com".into();
        assert_eq!(
            target.label(),
            "Example DEV · postgres dbtop@example-dev:5432"
        );

        target.host = "localhost".into();
        assert!(target.label().ends_with("dbtop@localhost:5432"));
    }

    fn with_tls(lines: &str) -> Config {
        parse(&format!(
            "[connections.c]\nkind = \"mysql\"\nhost = \"h\"\nuser = \"u\"\n{lines}\n"
        ))
    }

    #[test]
    fn tls_defaults_to_prefer_like_the_mysql_client() {
        let target = parse(SAMPLE).target("local-pg").unwrap();
        assert_eq!(target.tls, Tls::Prefer);
        assert!(target.tls_ca.is_none());
    }

    #[test]
    fn every_tls_mode_can_be_chosen_and_nothing_else() {
        for (word, mode) in [
            ("off", Tls::Off),
            ("prefer", Tls::Prefer),
            ("require", Tls::Require),
            ("verify", Tls::Verify),
        ] {
            let target = with_tls(&format!("tls = \"{word}\"")).target("c").unwrap();
            assert_eq!(target.tls, mode, "{word}");
        }
        for typo in ["tls = \"verfy\"", "tls = true", "tls = \"on\""] {
            let text =
                format!("[connections.c]\nkind = \"mysql\"\nhost = \"h\"\nuser = \"u\"\n{typo}\n");
            assert!(toml::from_str::<Config>(&text).is_err(), "{typo}");
        }
    }

    #[test]
    fn a_ca_file_only_makes_sense_when_verifying() {
        let err = with_tls("tls_ca = \"/tmp/ca.pem\"")
            .target("c")
            .unwrap_err();
        assert!(
            err.to_string().contains("only applies to tls = \"verify\""),
            "{err}"
        );
        let err = with_tls("tls = \"require\"\ntls_ca = \"/tmp/ca.pem\"")
            .target("c")
            .unwrap_err();
        assert!(err.to_string().contains("only applies"), "{err}");
    }

    #[test]
    fn a_missing_ca_file_is_reported_up_front() {
        let config = with_tls("tls = \"verify\"\ntls_ca = \"/nonexistent/ca.pem\"");
        let err = format!("{:#}", config.target("c").unwrap_err());
        assert!(
            err.contains("cannot read tls_ca") && err.contains("/nonexistent/ca.pem"),
            "{err}"
        );

        let present = with_tls(&format!("tls = \"verify\"\ntls_ca = \"{}\"", file!()));
        // file!() is relative to the crate root, which is where cargo test runs
        assert_eq!(
            present.target("c").unwrap().tls_ca,
            Some(PathBuf::from(file!()))
        );
    }

    #[test]
    fn a_leading_tilde_means_home() {
        assert_eq!(
            expand_home("~/certs/ca.pem", Some("/Users/me")),
            PathBuf::from("/Users/me/certs/ca.pem")
        );
        assert_eq!(
            expand_home("/abs/ca.pem", Some("/Users/me")),
            PathBuf::from("/abs/ca.pem")
        );
        assert_eq!(
            expand_home("~other/ca.pem", Some("/Users/me")),
            PathBuf::from("~other/ca.pem")
        );
        assert_eq!(expand_home("~/ca.pem", None), PathBuf::from("~/ca.pem"));
    }

    #[test]
    fn password_cmd_supplies_the_secret_without_the_newline() {
        let target = parse(SAMPLE).target("local-mysql").unwrap();
        assert_eq!(target.password.unwrap().expose(), "s3cret");
    }

    #[test]
    fn secrets_never_print() {
        let target = parse(SAMPLE).target("local-mysql").unwrap();
        let shown = format!("{target:?} {:?}", target.password);
        assert!(!shown.contains("s3cret"), "{shown}");
        assert!(shown.contains("***"));
    }

    #[test]
    fn a_failing_or_silent_password_cmd_is_an_error() {
        for (cmd, expect) in [
            ("exit 3", "exited"),
            ("true", "printed nothing"),
            ("printf ''", "printed nothing"),
        ] {
            let text = format!(
                "[connections.c]\nkind = \"mysql\"\nhost = \"h\"\nuser = \"u\"\npassword_cmd = \"{cmd}\"\n"
            );
            let err = format!("{:#}", parse(&text).target("c").unwrap_err());
            assert!(err.contains(expect), "{cmd}: {err}");
        }
    }

    #[test]
    fn password_and_password_cmd_together_are_ambiguous() {
        let text = "[connections.c]\nkind = \"mysql\"\nhost = \"h\"\nuser = \"u\"\npassword = \"a\"\npassword_cmd = \"echo b\"\n";
        assert!(
            parse(text)
                .target("c")
                .unwrap_err()
                .to_string()
                .contains("not both")
        );
    }

    #[test]
    fn typos_and_unknown_connections_are_errors() {
        let typo =
            "[connections.c]\nkind = \"mysql\"\nhost = \"h\"\nuser = \"u\"\npasword = \"x\"\n";
        assert!(
            toml::from_str::<Config>(typo).is_err(),
            "misspelt key must not be ignored"
        );
        assert!(
            toml::from_str::<Config>(
                "[connections.c]\nkind = \"oracle\"\nhost = \"h\"\nuser = \"u\"\n"
            )
            .is_err()
        );

        let err = parse(SAMPLE).target("nope").unwrap_err().to_string();
        assert!(
            err.contains("local-mysql") && err.contains("local-pg"),
            "lists what exists: {err}"
        );
    }

    #[test]
    fn finds_the_config_under_xdg_then_home() {
        assert_eq!(
            config_path(Some("/x"), Some("/h")),
            Some(PathBuf::from("/x/dbtop/config.toml"))
        );
        assert_eq!(
            config_path(None, Some("/h")),
            Some(PathBuf::from("/h/.config/dbtop/config.toml"))
        );
        assert_eq!(
            config_path(Some(""), Some("/h")),
            Some(PathBuf::from("/h/.config/dbtop/config.toml"))
        );
        assert_eq!(config_path(None, None), None);
    }

    #[cfg(unix)]
    #[test]
    fn a_plaintext_password_needs_a_private_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("dbtop-config-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let write = |name: &str, body: &str, mode: u32| {
            let path = dir.join(name);
            std::fs::write(&path, body).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
            path
        };
        let with_password =
            "[connections.c]\nkind = \"mysql\"\nhost = \"h\"\nuser = \"u\"\npassword = \"pw\"\n";
        let without = "[connections.c]\nkind = \"mysql\"\nhost = \"h\"\nuser = \"u\"\n";

        let open = write("open.toml", with_password, 0o644);
        let err = Config::load(&open).unwrap_err().to_string();
        assert!(err.contains("chmod 600"), "{err}");
        assert!(
            !err.contains("pw\""),
            "the error must not echo the password"
        );

        assert!(Config::load(&write("private.toml", with_password, 0o600)).is_ok());
        assert!(
            Config::load(&write("nopw.toml", without, 0o644)).is_ok(),
            "no password, nothing to protect"
        );

        // A password_cmd is a command to run: the file must not be editable by anyone else.
        let with_cmd = "[connections.c]\nkind = \"mysql\"\nhost = \"h\"\nuser = \"u\"\npassword_cmd = \"echo x\"\n";
        let err = Config::load(&write("cmd-group.toml", with_cmd, 0o664))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("password_cmd") && err.contains("chmod go-w"),
            "{err}"
        );
        assert!(Config::load(&write("cmd-other.toml", with_cmd, 0o646)).is_err());
        assert!(
            Config::load(&write("cmd-readable.toml", with_cmd, 0o644)).is_ok(),
            "readable is fine: no secret in it"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
