//! kaputt.matze.lol — status page for Karlsruhe's city websites.
//!
//! One binary, two shapes: `serve` runs the web UI and (later) the scheduler,
//! `probe --once` runs a single round of checks, prints them, and stores them.

mod config;
mod day;
mod db;
mod probe;
mod rollup;
mod scheduler;
mod server;
mod views;

use std::path::PathBuf;

use anyhow::bail;

use crate::config::Config;
use crate::db::Store;

const DEFAULT_CONFIG_PATH: &str = "config.toml";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Command {
    Serve,
    Help,
}

fn usage() -> &'static str {
    "usage:
  kaputt [--config PATH] [serve]

  serve        run the web UI (default)
  --config     configuration file, default config.toml
  -h, --help   print this text"
}

fn parse_args(args: &[String]) -> anyhow::Result<(PathBuf, Command)> {
    let mut config_path = PathBuf::from(DEFAULT_CONFIG_PATH);

    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--config" => {
                index += 1;
                let Some(value) = args.get(index) else {
                    bail!("--config needs a path\n{}", usage());
                };
                config_path = PathBuf::from(value);
            }
            "-h" | "--help" => return Ok((config_path, Command::Help)),
            other => bail!("unknown argument {other:?}\n{}", usage()),
        }
        index += 1;
    }

    Ok((config_path, Command::Serve))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (config_path, command) = parse_args(&args)?;
    if command == Command::Help {
        println!("{}", usage());
        return Ok(());
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let config = Config::load(&config_path)?;
    tracing::info!(
        config = %config_path.display(),
        targets = config.targets.len(),
        "configuration loaded"
    );

    let store = Store::open(&config.database).await?;
    store.upsert_targets(&config.targets).await?;
    // Catch the rollups up before the first page is served: the bars read
    // them, so an empty table would mean 90 empty bars after every upgrade.
    rollup::pass(&store, db::unix_now()).await?;

    match command {
        Command::Serve => server::run(config, store).await,
        Command::Help => unreachable!("help is handled above"),
    }
}

#[cfg(test)]
#[path = "main/tests.rs"]
mod tests;
