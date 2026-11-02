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
    let (path, command) = parse_args(&args(&["--config", "/etc/kaputt.toml"])).expect("valid args");
    assert_eq!(path, PathBuf::from("/etc/kaputt.toml"));
    assert_eq!(command, Command::Serve);
}

#[test]
fn unknown_and_missing_arguments_are_rejected() {
    assert!(parse_args(&args(&["kaputt"])).is_err());
    assert!(parse_args(&args(&["--config"])).is_err());
    assert!(parse_args(&args(&["probe", "serve", "--once"])).is_err());
}

#[test]
fn serve_is_accepted_as_documented() {
    let (path, command) = parse_args(&args(&["serve"])).expect("serve");
    assert_eq!(path, PathBuf::from("config.toml"));
    assert_eq!(command, Command::Serve);

    let (path, command) =
        parse_args(&args(&["--config", "/etc/kaputt.toml", "serve"])).expect("serve with config");
    assert_eq!(path, PathBuf::from("/etc/kaputt.toml"));
    assert_eq!(command, Command::Serve);
}

#[test]
fn help_wins() {
    let (_, command) = parse_args(&args(&["--help"])).expect("help");
    assert_eq!(command, Command::Help);
}
