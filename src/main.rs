mod app;
mod backend;
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
                            the list dbq uses (~/.config/mysql/hosts, passwords in the Keychain)
  dbtop --demo              run on generated data, no database needed

options:
  --config <file>           use only this config file (default: ~/.config/dbtop/config.toml)
  -h, --help                show this help
";

enum Cli {
    Help,
    Demo,
    Connect {
        name: String,
        config: Option<PathBuf>,
    },
}

fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Cli, String> {
    let (mut demo, mut config, mut name) = (false, None, None);
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => return Ok(Cli::Help),
            "--demo" => demo = true,
            "--config" => config = Some(PathBuf::from(args.next().ok_or("--config needs a file")?)),
            flag if flag.starts_with('-') => return Err(format!("unknown option {flag}")),
            other if name.is_none() => name = Some(other.to_string()),
            _ => return Err("only one connection name can be given".to_string()),
        }
    }
    match (demo, name) {
        (true, None) => Ok(Cli::Demo),
        (true, Some(_)) => Err("--demo takes no connection name".to_string()),
        (false, Some(name)) => Ok(Cli::Connect { name, config }),
        (false, None) => Err("missing the connection name".to_string()),
    }
}

fn main() -> Result<()> {
    let backend: Box<dyn Backend> = match parse_args(env::args().skip(1)) {
        Ok(Cli::Help) => {
            print!("{USAGE}");
            list_connections();
            return Ok(());
        }
        Ok(Cli::Demo) => Box::new(Fake::new()),
        Ok(Cli::Connect { name, config }) => {
            let target = find_target(&name, config)?;
            backend::open(&target).with_context(|| format!("connecting to {}", target.label()))?
        }
        Err(message) => {
            eprintln!("dbtop: {message}\n\n{USAGE}");
            list_connections();
            std::process::exit(2);
        }
    };

    let mut app = App::new(backend.label());
    let (msgs, reqs) = collector::spawn(backend, Duration::from_secs(1));

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
            matches!(parse(&["example-dev"]), Ok(Cli::Connect { name, config: None }) if name == "example-dev")
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
    fn demo_and_help() {
        assert!(matches!(parse(&["--demo"]), Ok(Cli::Demo)));
        assert!(matches!(parse(&["-h"]), Ok(Cli::Help)));
        assert!(matches!(parse(&["x", "--help"]), Ok(Cli::Help)));
    }

    #[test]
    fn misuse_is_an_error_not_a_guess() {
        for args in [
            &[][..],
            &["--demo", "x"],
            &["a", "b"],
            &["--bogus"],
            &["--config"],
        ] {
            assert!(parse(args).is_err(), "{args:?}");
        }
    }
}
