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

use crate::config::{Config, TargetConfig};
use crate::db::Store;
use crate::probe::Prober;

const DEFAULT_CONFIG_PATH: &str = "config.toml";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Command {
    Serve,
    ProbeOnce,
    Help,
}

fn usage() -> &'static str {
    "usage:
  kaputt [--config PATH] [serve]
  kaputt [--config PATH] probe --once

  serve        run the web UI (default)
  probe --once run one round of checks, print and store the results
  --config     configuration file, default config.toml
  -h, --help   print this text"
}

fn parse_args(args: &[String]) -> anyhow::Result<(PathBuf, Command)> {
    let mut config_path = PathBuf::from(DEFAULT_CONFIG_PATH);
    let mut command: Option<Command> = None;
    let mut once = false;

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
            "--once" => once = true,
            "-h" | "--help" => return Ok((config_path, Command::Help)),
            "serve" => set_command(&mut command, Command::Serve)?,
            "probe" => set_command(&mut command, Command::ProbeOnce)?,
            other => bail!("unknown argument {other:?}\n{}", usage()),
        }
        index += 1;
    }

    let command = command.unwrap_or(Command::Serve);
    match (command, once) {
        (Command::ProbeOnce, false) => bail!("probe requires --once\n{}", usage()),
        (Command::Serve, true) => bail!("--once is only valid with probe\n{}", usage()),
        _ => {}
    }
    Ok((config_path, command))
}

fn set_command(command: &mut Option<Command>, next: Command) -> anyhow::Result<()> {
    if command.is_some() {
        bail!("only one command can be given\n{}", usage());
    }
    *command = Some(next);
    Ok(())
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
        Command::ProbeOnce => probe_once(&config, &store).await,
        Command::Help => unreachable!("help is handled above"),
    }
}

async fn probe_once(config: &Config, store: &Store) -> anyhow::Result<()> {
    let prober = Prober::new()?;
    let mut targets: Vec<&TargetConfig> = config.targets.iter().filter(|t| t.enabled).collect();
    targets.sort_by(|a, b| (a.sort, &a.id).cmp(&(b.sort, &b.id)));
    if targets.is_empty() {
        bail!("no enabled targets in the configuration");
    }

    let mut failed = 0;
    for target in &targets {
        let outcome = scheduler::check_once(store, &prober, target).await?;
        println!("{}", report_line(target, &outcome));
        if !outcome.ok {
            failed += 1;
        }
    }

    if failed > 0 {
        bail!("{failed} von {} Checks fehlgeschlagen", targets.len());
    }
    Ok(())
}

fn report_line(target: &TargetConfig, outcome: &probe::Outcome) -> String {
    let state = if outcome.ok { "OK" } else { "KAPUTT" };
    let status = match outcome.status_code {
        Some(code) => code.to_string(),
        None => "-".to_string(),
    };
    let latency = match outcome.latency_ms {
        Some(ms) => format!("{ms} ms"),
        None => "-".to_string(),
    };

    let mut line = format!(
        "{state:6}  {:<14}  {status:>3}  {latency:>9}  {}",
        target.id, target.url
    );
    if let Some(kind) = outcome.error_kind {
        line.push_str(&format!("  [{}]", kind.as_str()));
        if let Some(detail) = &outcome.error_detail {
            line.push(' ');
            line.push_str(detail);
        }
    }
    line
}

#[cfg(test)]
#[path = "main/tests.rs"]
mod tests;
