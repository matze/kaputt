use super::*;

const MINIMAL: &str = r#"
        [[targets]]
        id = "karlsruhe"
        name = "Stadt Karlsruhe"
        url = "https://www.karlsruhe.de/"
    "#;

fn parse(raw: &str) -> Result<Config, String> {
    let config: Config = toml::from_str(raw).map_err(|e| e.to_string())?;
    config.validate()?;
    Ok(config)
}

#[test]
fn defaults_are_applied() {
    let config = parse(MINIMAL).expect("valid config");
    assert_eq!(config.listen, "127.0.0.1:8080");
    assert_eq!(config.database, PathBuf::from("kaputt.db"));
    assert_eq!(config.base_url, "https://kaputt.matze.lol");
    let target = &config.targets[0];
    assert_eq!(target.expect_status, 200);
    assert_eq!(target.interval_s, 300);
    assert_eq!(target.timeout_s, 10);
    assert_eq!(target.fail_threshold, 2);
    assert_eq!(target.ok_threshold, 2);
    assert!(target.enabled);
    assert_eq!(target.sort, 0);
}

#[test]
fn explicit_values_win_over_defaults() {
    let config = parse(
        r#"
            listen = "0.0.0.0:9000"
            database = "/var/lib/kaputt/kaputt.db"
            base_url = "https://status.example.org"

            [[targets]]
            id = "opac"
            name = "OPAC"
            url = "https://opac.karlsruhe.de/"
            expect_status = 404
            interval_s = 60
            timeout_s = 3
            fail_threshold = 5
            ok_threshold = 1
            enabled = false
            sort = 7
            "#,
    )
    .expect("valid config");
    assert_eq!(config.listen, "0.0.0.0:9000");
    assert_eq!(config.database, PathBuf::from("/var/lib/kaputt/kaputt.db"));
    assert_eq!(config.base_url, "https://status.example.org");
    let target = &config.targets[0];
    assert_eq!(target.expect_status, 404);
    assert_eq!(target.interval_s, 60);
    assert_eq!(target.timeout_s, 3);
    assert_eq!(target.fail_threshold, 5);
    assert_eq!(target.ok_threshold, 1);
    assert!(!target.enabled);
    assert_eq!(target.sort, 7);
}

#[test]
fn missing_target_fields_are_rejected() {
    let err = parse(
        r#"
            [[targets]]
            id = "opac"
            name = "OPAC"
            "#,
    )
    .expect_err("url is required");
    assert!(err.contains("url"), "{err}");
}

#[test]
fn unknown_keys_are_rejected() {
    let err = parse(&format!("{MINIMAL}\n[[targets]]\nid = \"x\"\nname = \"X\"\nurl = \"https://x.de/\"\nexpectstatus = 200\n")).expect_err("typo must fail");
    assert!(err.contains("expectstatus"), "{err}");
}

#[test]
fn duplicate_target_ids_are_rejected() {
    let err = parse(&format!("{MINIMAL}\n[[targets]]\nid = \"karlsruhe\"\nname = \"Doppelt\"\nurl = \"https://example.org/\"\n"))
            .expect_err("duplicate id must fail");
    assert!(err.contains("duplicate target id"), "{err}");
}

#[test]
fn bad_urls_are_rejected() {
    let err = parse(
        r#"
            [[targets]]
            id = "x"
            name = "X"
            url = "ftp://example.org/"
        "#,
    )
    .expect_err("ftp must fail");
    assert!(err.contains("not http or https"), "{err}");

    let err = parse(
        r#"
            [[targets]]
            id = "x"
            name = "X"
            url = "not a url"
        "#,
    )
    .expect_err("garbage must fail");
    assert!(err.contains("url"), "{err}");
}

#[test]
fn bad_listen_address_and_id_are_rejected() {
    let err = parse("listen = \"localhost\"\n").expect_err("listen must be a socket address");
    assert!(err.contains("listen"), "{err}");

    let err = parse(
        r#"
            [[targets]]
            id = "hat space"
            name = "X"
            url = "https://example.org/"
        "#,
    )
    .expect_err("id charset must be enforced");
    assert!(err.contains("target id"), "{err}");
}

#[test]
fn zero_interval_and_timeout_are_rejected() {
    for body in [
        "interval_s = 0",
        "timeout_s = 0",
        "fail_threshold = 0",
        "ok_threshold = 0",
    ] {
        let err = parse(&format!(
            r#"
                [[targets]]
                id = "x"
                name = "X"
                url = "https://example.org/"
                {body}
                "#
        ))
        .expect_err(body);
        assert!(!err.is_empty(), "{body}");
    }
}

#[test]
fn bad_base_urls_are_rejected() {
    for raw in [
        "base_url = \"ftp://example.org\"",
        "base_url = \"not a url\"",
        "base_url = \"https://example.org/?q=1\"",
        "base_url = \"https://example.org/#x\"",
    ] {
        let err = parse(&format!("{raw}\n{MINIMAL}\n")).expect_err(raw);
        assert!(err.contains("base_url"), "{raw}: {err}");
    }
}

#[test]
fn shipped_config_is_valid() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("config.toml");
    let config = Config::load(&path).expect("config.toml must load");
    assert_eq!(config.targets.len(), 5);
}
