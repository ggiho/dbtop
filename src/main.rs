mod app;
mod backend;
mod clipboard;
mod collector;
mod config;
mod model;
mod registry;
mod ui;

use std::env;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use ratatui::{
    DefaultTerminal,
    crossterm::event::{self, Event, KeyEventKind},
};

use app::App;
use backend::{Backend, fake::Fake};
use collector::{Msg, Req};
use config::{Config, Target};
use registry::Registry;

const USAGE: &str = "\
dbtop: a terminal monitor for MySQL and Postgres

usage:
  dbtop <name>              monitor a connection from the config file, or else a server from
                            the list dbq uses (~/.config/mysql/hosts, passwords in the Keychain);
                            a name with spaces needs no quotes: dbtop example dev
  dbtop --demo              run on generated data, no database needed

options:
  --config <file>           use only this config file (default: ~/.config/dbtop/config.toml)
  --interval <seconds>      time between polls, 0.5 to 60 (default 1). Every poll costs the server
                            a few statements: slow it down on a server that is busy
  -h, --help                show this help
";

/// The time between polls unless `--interval` says otherwise.
const DEFAULT_INTERVAL: Duration = Duration::from_secs(1);

enum Cli {
    Help,
    Demo {
        interval: Duration,
    },
    Connect {
        name: String,
        config: Option<PathBuf>,
        interval: Duration,
    },
}

/// Every poll is a handful of statements on the server, so not faster than twice a second; and
/// not so slow that the screen looks dead.
fn parse_interval(text: &str) -> Result<Duration, String> {
    let secs: f64 = text
        .parse()
        .map_err(|_| format!("--interval wants a number of seconds, not {text:?}"))?;
    if !(0.5..=60.0).contains(&secs) {
        return Err(format!(
            "--interval must be between 0.5 and 60 seconds, not {text}"
        ));
    }
    Ok(Duration::from_secs_f64(secs))
}

fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Cli, String> {
    let (mut demo, mut config, mut interval) = (false, None, DEFAULT_INTERVAL);
    let mut words: Vec<String> = Vec::new();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => return Ok(Cli::Help),
            "--demo" => demo = true,
            "--config" => config = Some(PathBuf::from(args.next().ok_or("--config needs a file")?)),
            "--interval" => {
                interval =
                    parse_interval(&args.next().ok_or("--interval needs a number of seconds")?)?;
            }
            flag if flag.starts_with('-') => return Err(format!("unknown option {flag}")),
            // `dbtop Example DEV` is the name "Example DEV".
            word => words.push(word.to_string()),
        }
    }
    match (demo, words.is_empty()) {
        (true, true) => Ok(Cli::Demo { interval }),
        (true, false) => Err("--demo takes no connection name".to_string()),
        (false, false) => Ok(Cli::Connect {
            name: words.join(" "),
            config,
            interval,
        }),
        (false, true) => Err("missing the connection name".to_string()),
    }
}

fn main() -> Result<()> {
    let (backend, interval): (Box<dyn Backend>, Duration) = match parse_args(env::args().skip(1)) {
        Ok(Cli::Help) => {
            print!("{USAGE}");
            list_connections();
            return Ok(());
        }
        Ok(Cli::Demo { interval }) => (Box::new(Fake::new()), interval),
        Ok(Cli::Connect {
            name,
            config,
            interval,
        }) => {
            let target = find_target(&name, config)?;
            let backend = backend::open(&target)
                .with_context(|| format!("connecting to {}", target.label()))?;
            (backend, interval)
        }
        Err(message) => {
            eprintln!("dbtop: {message}\n\n{USAGE}");
            list_connections();
            std::process::exit(2);
        }
    };

    let mut app = App::new(backend.label());
    let (msgs, reqs) = collector::spawn(backend, interval);

    let mut terminal =
        ratatui::try_init().context("cannot take over the terminal: is this a terminal?")?;
    let result = run(&mut terminal, &mut app, &msgs, &reqs);
    ratatui::restore();
    result
}

/// dbtop's own connections first; if there is none by that name, the servers dbq knows.
fn find_target(name: &str, explicit: Option<PathBuf>) -> Result<Target> {
    // --config means exactly that file.
    if let Some(path) = explicit {
        return Config::load(&path)?.target(name);
    }

    let mut searched = Vec::new();
    if let Some(path) = config::default_path().filter(|p| p.exists()) {
        let config = Config::load(&path)?;
        if config.has(name) {
            return config.target(name);
        }
        searched.push(format!(
            "in {}: {}",
            path.display(),
            config.names().collect::<Vec<_>>().join(", ")
        ));
    }
    if let Some(path) = registry::default_path().filter(|p| p.exists()) {
        let registry = Registry::load(&path)?;
        if let Some(target) = registry.target(name, registry::keychain_password)? {
            return Ok(target);
        }
        searched.push(format!(
            "in the dbq list {}: {}",
            path.display(),
            unique(registry.names()).join(", ")
        ));
    }
    if searched.is_empty() {
        bail!(
            "no connection or server named {name:?}: there is no dbtop config and no dbq server list to look in"
        );
    }
    bail!(
        "no connection or server named {name:?}\n  {}",
        searched.join("\n  ")
    )
}

fn unique<'a>(names: impl Iterator<Item = &'a str>) -> Vec<&'a str> {
    let mut seen = Vec::new();
    for name in names {
        if !seen.contains(&name) {
            seen.push(name);
        }
    }
    seen
}

/// Best effort: help shouldn't fail because a file is missing or unreadable.
fn list_connections() {
    if let Some(path) = config::default_path()
        && let Ok(config) = Config::load(&path)
    {
        println!(
            "\nconnections in {}: {}",
            path.display(),
            config.names().collect::<Vec<_>>().join(", ")
        );
    }
    if let Some(path) = registry::default_path()
        && let Ok(registry) = Registry::load(&path)
    {
        println!(
            "\nservers in the dbq list {}:\n  {}",
            path.display(),
            unique(registry.names()).join("\n  ")
        );
    }
}

fn run(
    terminal: &mut DefaultTerminal,
    app: &mut App,
    msgs: &Receiver<Msg>,
    reqs: &Sender<Req>,
) -> Result<()> {
    while !app.quit {
        app.drain(msgs);
        terminal.draw(|f| ui::draw(f, app))?;

        if event::poll(Duration::from_millis(100))?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            app.on_key(key, reqs);
            if let Some(copy) = app.to_copy.take() {
                let outcome = clipboard::copy(&copy.sql);
                app.copied(&copy, outcome);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duplicate_names_are_listed_once_in_order() {
        assert_eq!(
            unique(["a", "b", "a", "c", "b"].into_iter()),
            ["a", "b", "c"]
        );
    }

    fn parse(args: &[&str]) -> Result<Cli, String> {
        parse_args(args.iter().map(|s| s.to_string()))
    }

    #[test]
    fn a_connection_name_is_all_that_is_needed() {
        assert!(
            matches!(parse(&["example-dev"]), Ok(Cli::Connect { name, config: None, .. }) if name == "example-dev")
        );
        assert!(matches!(
            parse(&["--config", "/tmp/c.toml", "x"]),
            Ok(Cli::Connect { config: Some(p), .. }) if p.as_path() == std::path::Path::new("/tmp/c.toml")
        ));
        assert!(matches!(
            parse(&["x", "--config", "/tmp/c.toml"]),
            Ok(Cli::Connect { .. })
        ));
    }

    #[test]
    fn a_name_with_spaces_needs_no_quotes() {
        let name = |args: &[&str]| match parse(args) {
            Ok(Cli::Connect { name, .. }) => name,
            _ => panic!("{args:?} is not a connection"),
        };
        assert_eq!(name(&["Example", "DEV"]), "Example DEV");
        assert_eq!(
            name(&["example", "--config", "c.toml", "dev"]),
            "example dev"
        );
        assert_eq!(name(&["Example DEV"]), "Example DEV", "quoting still works");
    }

    #[test]
    fn the_interval_is_a_number_of_seconds_the_server_can_afford() {
        let interval = |args: &[&str]| match parse(args) {
            Ok(Cli::Connect { interval, .. } | Cli::Demo { interval }) => interval,
            _ => panic!("{args:?} is not a run"),
        };
        assert_eq!(
            interval(&["x"]),
            Duration::from_secs(1),
            "one second unless told"
        );
        assert_eq!(interval(&["x", "--interval", "5"]), Duration::from_secs(5));
        assert_eq!(
            interval(&["--demo", "--interval", "0.5"]),
            Duration::from_millis(500)
        );
        assert_eq!(
            interval(&["--interval", "60", "x"]),
            Duration::from_secs(60)
        );
        for bad in ["0.1", "0", "-3", "61", "soon", "NaN", "inf", ""] {
            assert!(parse(&["x", "--interval", bad]).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn demo_and_help() {
        assert!(matches!(parse(&["--demo"]), Ok(Cli::Demo { .. })));
        assert!(matches!(parse(&["-h"]), Ok(Cli::Help)));
        assert!(matches!(parse(&["x", "--help"]), Ok(Cli::Help)));
    }

    #[test]
    fn misuse_is_an_error_not_a_guess() {
        for args in [
            &[][..],
            &["--demo", "x"],
            &["--bogus"],
            &["--config"],
            &["--interval"],
        ] {
            assert!(parse(args).is_err(), "{args:?}");
        }
    }
}
