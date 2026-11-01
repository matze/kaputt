//! Configuration file parsing and validation.
//!
//! Targets live in `config.toml` and are upserted into the database at
//! startup. The file is the only place a target can be added or changed.

use std::collections::HashSet;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use serde::Deserialize;

const DEFAULT_LISTEN: &str = "127.0.0.1:8080";
const DEFAULT_DATABASE: &str = "kaputt.db";
const DEFAULT_BASE_URL: &str = "https://kaputt.matze.lol";
const DEFAULT_EXPECT_STATUS: u16 = 200;
const DEFAULT_INTERVAL_S: u32 = 300;
const DEFAULT_TIMEOUT_S: u32 = 10;
const DEFAULT_THRESHOLD: u32 = 2;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default = "default_listen")]
    pub listen: String,
    #[serde(default = "default_database")]
    pub database: PathBuf,
    /// Public origin used for absolute feed links, without a trailing slash.
    #[serde(default = "default_base_url")]
    pub base_url: String,
    #[serde(default)]
    pub targets: Vec<TargetConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TargetConfig {
    pub id: String,
    pub name: String,
    pub url: String,
    #[serde(default = "default_expect_status")]
    pub expect_status: u16,
    #[serde(default = "default_interval_s")]
    pub interval_s: u32,
    #[serde(default = "default_timeout_s")]
    pub timeout_s: u32,
    #[serde(default = "default_threshold")]
    pub fail_threshold: u32,
    #[serde(default = "default_threshold")]
    pub ok_threshold: u32,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub sort: i64,
}

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let raw = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("cannot read {}: {e}", path.display()))?;
        let config: Config = toml::from_str(&raw)
            .map_err(|e| anyhow::anyhow!("cannot parse {}: {e}", path.display()))?;
        config
            .validate()
            .map_err(|e| anyhow::anyhow!("invalid {}: {e}", path.display()))?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<(), String> {
        self.listen
            .parse::<SocketAddr>()
            .map_err(|e| format!("listen {:?}: {e}", self.listen))?;

        let base = url::Url::parse(&self.base_url)
            .map_err(|e| format!("base_url {:?}: {e}", self.base_url))?;
        if !matches!(base.scheme(), "http" | "https") {
            return Err(format!(
                "base_url {:?}: scheme {:?} is not http or https",
                self.base_url,
                base.scheme()
            ));
        }
        if base.host_str().is_none() {
            return Err(format!("base_url {:?}: has no host", self.base_url));
        }
        if base.query().is_some() || base.fragment().is_some() {
            return Err(format!(
                "base_url {:?}: must not carry a query or fragment",
                self.base_url
            ));
        }

        let mut seen = HashSet::new();
        for target in &self.targets {
            target.validate()?;
            if !seen.insert(target.id.as_str()) {
                return Err(format!("duplicate target id {:?}", target.id));
            }
        }
        Ok(())
    }
}

impl TargetConfig {
    fn validate(&self) -> Result<(), String> {
        if self.id.is_empty() {
            return Err("target id must not be empty".into());
        }
        if !self
            .id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Err(format!(
                "target id {:?}: only ASCII letters, digits, '-' and '_' are allowed",
                self.id
            ));
        }
        if self.name.trim().is_empty() {
            return Err(format!("target {:?}: name must not be empty", self.id));
        }
        let url = url::Url::parse(&self.url)
            .map_err(|e| format!("target {:?}: url {:?}: {e}", self.id, self.url))?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(format!(
                "target {:?}: url scheme {:?} is not http or https",
                self.id,
                url.scheme()
            ));
        }
        if url.host_str().is_none() {
            return Err(format!("target {:?}: url has no host", self.id));
        }
        if !(100..=599).contains(&self.expect_status) {
            return Err(format!(
                "target {:?}: expect_status {} is not a HTTP status",
                self.id, self.expect_status
            ));
        }
        if self.interval_s == 0 {
            return Err(format!("target {:?}: interval_s must be > 0", self.id));
        }
        if self.timeout_s == 0 {
            return Err(format!("target {:?}: timeout_s must be > 0", self.id));
        }
        if self.fail_threshold == 0 || self.ok_threshold == 0 {
            return Err(format!("target {:?}: thresholds must be > 0", self.id));
        }
        Ok(())
    }
}

fn default_listen() -> String {
    DEFAULT_LISTEN.to_string()
}

fn default_database() -> PathBuf {
    PathBuf::from(DEFAULT_DATABASE)
}

fn default_base_url() -> String {
    DEFAULT_BASE_URL.to_string()
}

fn default_expect_status() -> u16 {
    DEFAULT_EXPECT_STATUS
}

fn default_interval_s() -> u32 {
    DEFAULT_INTERVAL_S
}

fn default_timeout_s() -> u32 {
    DEFAULT_TIMEOUT_S
}

fn default_threshold() -> u32 {
    DEFAULT_THRESHOLD
}

fn default_true() -> bool {
    true
}

#[cfg(test)]
mod tests {
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
}
