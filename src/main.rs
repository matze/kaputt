//! kaputt.matze.lol — status page for Karlsruhe's city websites.
//!
//! One binary, two shapes: `serve` runs the web UI and (later) the scheduler,
//! `probe --once` runs a single round of checks, prints them, and stores them.

mod config;
mod db;
mod probe;
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
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn no_arguments_means_serve() {
        let (path, command) = parse_args(&[]).expect("default");
        assert_eq!(path, PathBuf::from("config.toml"));
        assert_eq!(command, Command::Serve);
    }

    #[test]
    fn config_flag_is_honoured() {
        let (path, command) =
            parse_args(&args(&["--config", "/etc/kaputt.toml", "serve"])).expect("valid args");
        assert_eq!(path, PathBuf::from("/etc/kaputt.toml"));
        assert_eq!(command, Command::Serve);
    }

    #[test]
    fn probe_requires_once_flag() {
        let err = parse_args(&args(&["probe"])).expect_err("--once is required");
        assert!(err.to_string().contains("--once"), "{err}");
    }

    #[test]
    fn probe_once_is_parsed() {
        let (_, command) = parse_args(&args(&["probe", "--once"])).expect("valid args");
        assert_eq!(command, Command::ProbeOnce);

        let (_, command) = parse_args(&args(&["--once", "probe"])).expect("valid args");
        assert_eq!(command, Command::ProbeOnce);
    }

    #[test]
    fn once_without_probe_is_rejected() {
        assert!(parse_args(&args(&["serve", "--once"])).is_err());
        assert!(parse_args(&args(&["--once"])).is_err());
    }

    #[test]
    fn unknown_and_missing_arguments_are_rejected() {
        assert!(parse_args(&args(&["kaputt"])).is_err());
        assert!(parse_args(&args(&["--config"])).is_err());
        assert!(parse_args(&args(&["probe", "serve", "--once"])).is_err());
    }

    #[test]
    fn help_wins() {
        let (_, command) = parse_args(&args(&["--help"])).expect("help");
        assert_eq!(command, Command::Help);
    }

    #[test]
    fn report_line_shows_state_status_and_latency() {
        let target = TargetConfig {
            id: "opac".into(),
            name: "OPAC".into(),
            url: "https://opac.karlsruhe.de/".into(),
            expect_status: 200,
            interval_s: 300,
            timeout_s: 10,
            fail_threshold: 2,
            ok_threshold: 2,
            enabled: true,
            sort: 0,
        };
        let ok = probe::Outcome {
            ok: true,
            status_code: Some(200),
            latency_ms: Some(42),
            error_kind: None,
            error_detail: None,
        };
        let line = report_line(&target, &ok);
        assert!(line.starts_with("OK"), "{line}");
        assert!(line.contains("opac"), "{line}");
        assert!(line.contains("200"), "{line}");
        assert!(line.contains("42 ms"), "{line}");

        let broken = probe::Outcome {
            ok: false,
            status_code: None,
            latency_ms: Some(10_000),
            error_kind: Some(probe::ErrorKind::Timeout),
            error_detail: Some("operation timed out".into()),
        };
        let line = report_line(&target, &broken);
        assert!(line.starts_with("KAPUTT"), "{line}");
        assert!(line.contains("[timeout]"), "{line}");
        assert!(line.contains("operation timed out"), "{line}");
    }
}
