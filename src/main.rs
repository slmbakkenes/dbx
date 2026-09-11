//! dbx — a terminal database client that never traps you in a pane.

mod app;
mod clipboard;
mod complete;
mod config;
mod db;
mod diagnose;
mod editor;
mod form;
mod grid;
mod keys;
mod store;
mod tree;
mod ui;

use anyhow::Result;
use app::App;
use config::Config;
use crossterm::event::{Event, EventStream};
use futures_util::StreamExt;
use std::time::Duration;

fn print_help() {
    println!(
        "dbx {} — terminal database client (postgres, mssql)

usage:
  dbx                 open the TUI
  dbx --config        print the config file path
  dbx --check [name]  diagnose a connection: password source, login, `select 1`
  dbx --keys          print the default keys.toml
  dbx -h, --help      this message

config lives in $DBX_CONFIG_DIR, else $XDG_CONFIG_HOME/dbx, else ~/.config/dbx",
        env!("CARGO_PKG_VERSION")
    );
}

fn print_default_keys() {
    use keys::{Context, default_keymap};
    let km = default_keymap();
    for (name, ctx) in [
        ("global", Context::Global),
        ("tree", Context::Tree),
        ("grid", Context::Grid),
        ("editor", Context::Editor),
    ] {
        println!("[{name}]");
        for (k, a) in km.entries(ctx) {
            println!("{:<14} = \"{}\"", format!("\"{k}\""), a.name());
        }
        println!();
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("-h") | Some("--help") => {
            print_help();
            return Ok(());
        }
        Some("--config") => {
            println!("{}", config::config_path()?.display());
            return Ok(());
        }
        Some("--keys") => {
            print_default_keys();
            return Ok(());
        }
        Some("--check") => {
            let cfg = Config::load()?;
            return diagnose::run(&cfg, args.get(1).map(String::as_str)).await;
        }
        Some(other) => {
            eprintln!("unknown argument `{other}`");
            print_help();
            std::process::exit(2);
        }
        None => {}
    }

    let cfg = Config::load()?;
    let mut app = App::new(cfg);
    let mut terminal = ratatui::init();
    let res = run(&mut terminal, &mut app).await;
    ratatui::restore();
    app.shutdown();
    res
}

async fn run(terminal: &mut ratatui::DefaultTerminal, app: &mut App) -> Result<()> {
    let mut events = EventStream::new();
    let mut ticker = tokio::time::interval(Duration::from_millis(100));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        terminal.draw(|f| ui::draw(f, app))?;
        if app.quit {
            return Ok(());
        }
        tokio::select! {
            maybe = events.next() => match maybe {
                Some(Ok(Event::Key(key))) => app.on_key(key),
                Some(Ok(Event::Resize(_, _))) => {}
                Some(Ok(_)) => {}
                Some(Err(e)) => return Err(e.into()),
                None => return Ok(()),
            },
            Some(ev) = app.events_rx.recv() => app.on_db_event(ev),
            Some(outcome) = app.test_rx.recv() => app.on_test_outcome(outcome),
            _ = ticker.tick() => app.tick(),
        }
    }
}
