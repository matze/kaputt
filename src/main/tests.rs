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
