//! Configuration: project groups, connections, UI knobs.
//!
//! Lives at `$DBX_CONFIG_DIR`, else `$XDG_CONFIG_HOME/dbx`, else `~/.config/dbx`.

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Engine {
    Postgres,
    Mssql,
}

impl Engine {
    pub fn default_port(self) -> u16 {
        match self {
            Engine::Postgres => 5432,
            Engine::Mssql => 1433,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Engine::Postgres => "pg",
            Engine::Mssql => "mssql",
        }
    }
}

/// A single database connection. Passwords are never stored here in cleartext
/// unless the user explicitly opts in with `password = "..."`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct Connection {
    pub name: String,
    pub engine: Engine,
    pub host: String,
    #[serde(default)]
    pub port: Option<u16>,
    pub database: String,
    pub user: String,

    /// Keyring account key. Service is always `dbx`.
    #[serde(default)]
    pub keyring: Option<String>,
    /// Shell command whose first line of stdout is the password.
    #[serde(default)]
    pub password_command: Option<String>,
    /// Cleartext password. Discouraged; only honoured if nothing else is set.
    #[serde(default)]
    pub password: Option<String>,

    /// Postgres: `disable` | `prefer` | `require`. Default `prefer`.
    #[serde(default)]
    pub sslmode: Option<String>,
    /// Accept self-signed / unverifiable server certificates.
    #[serde(default)]
    pub trust_cert: bool,
    /// Refuse to run anything that is not a read-only statement.
    #[serde(default)]
    pub read_only: bool,
}

impl Connection {
    pub fn port_or_default(&self) -> u16 {
        self.port.unwrap_or_else(|| self.engine.default_port())
    }

    /// Resolves the password, in order: keyring, password_command, cleartext.
    pub fn resolve_password(&self) -> Result<String> {
        if let Some(account) = &self.keyring {
            let entry = keyring::Entry::new("dbx", account)
                .with_context(|| format!("keyring entry dbx/{account}"))?;
            return entry
                .get_password()
                .with_context(|| format!("reading keyring dbx/{account}"));
        }
        if let Some(cmd) = &self.password_command {
            let out = std::process::Command::new("sh")
                .arg("-c")
                .arg(cmd)
                .output()
                .context("running password_command")?;
            if !out.status.success() {
                return Err(anyhow!("password_command exited with {}", out.status));
            }
            let s = String::from_utf8_lossy(&out.stdout);
            return Ok(s.lines().next().unwrap_or("").to_string());
        }
        if let Some(p) = &self.password {
            return Ok(p.clone());
        }
        Err(anyhow!(
            "no password source for `{}`: set keyring, password_command, or password",
            self.name
        ))
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Project {
    pub name: String,
    #[serde(default, rename = "connection")]
    pub connections: Vec<Connection>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct UiConfig {
    /// Rows fetched per page of table data.
    #[serde(default = "d_page")]
    pub page_size: usize,
    /// Hard cap on rendered column width.
    #[serde(default = "d_colw")]
    pub max_column_width: usize,
    #[serde(default)]
    pub mouse: bool,
    /// Confirm before running a statement that writes.
    #[serde(default = "d_true")]
    pub confirm_writes: bool,
}

fn d_page() -> usize {
    500
}
fn d_colw() -> usize {
    48
}
fn d_true() -> bool {
    true
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            page_size: d_page(),
            max_column_width: d_colw(),
            mouse: false,
            confirm_writes: d_true(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Config {
    #[serde(default, rename = "project")]
    pub projects: Vec<Project>,
    #[serde(default)]
    pub ui: UiConfig,
}

pub fn config_dir() -> Result<PathBuf> {
    if let Ok(d) = std::env::var("DBX_CONFIG_DIR") {
        return Ok(PathBuf::from(d));
    }
    if let Ok(d) = std::env::var("XDG_CONFIG_HOME") {
        return Ok(PathBuf::from(d).join("dbx"));
    }
    let base = directories::BaseDirs::new().ok_or_else(|| anyhow!("cannot locate home dir"))?;
    Ok(base.home_dir().join(".config").join("dbx"))
}

pub fn config_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("config.toml"))
}

pub const SAMPLE_CONFIG: &str = r#"# dbx configuration
# Passwords: prefer `keyring` (service "dbx") or `password_command`.

[ui]
page_size = 500
max_column_width = 48
confirm_writes = true

[[project]]
name = "Acme"

  [[project.connection]]
  name = "acme-local"
  engine = "postgres"
  host = "127.0.0.1"
  port = 5432
  database = "acme"
  user = "postgres"
  password_command = "echo postgres"
  sslmode = "disable"

  [[project.connection]]
  name = "acme-reporting"
  engine = "mssql"
  host = "sql.internal"
  port = 1433
  database = "Reporting"
  user = "svc_report"
  keyring = "acme-reporting"
  trust_cert = true
  read_only = true
"#;

impl Config {
    pub fn load() -> Result<Self> {
        let path = config_path()?;
        if !path.exists() {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            std::fs::write(&path, SAMPLE_CONFIG)
                .with_context(|| format!("writing sample config to {}", path.display()))?;
        }
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        Self::parse(&raw).with_context(|| format!("parsing {}", path.display()))
    }

    pub fn parse(raw: &str) -> Result<Self> {
        let cfg: Config = toml::from_str(raw)?;
        let mut seen = Vec::new();
        for p in &cfg.projects {
            for c in &p.connections {
                let key = format!("{}/{}", p.name, c.name);
                if seen.contains(&key) {
                    return Err(anyhow!("duplicate connection name `{key}`"));
                }
                seen.push(key);
            }
        }
        Ok(cfg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sample() {
        let cfg = Config::parse(SAMPLE_CONFIG).unwrap();
        assert_eq!(cfg.projects.len(), 1);
        assert_eq!(cfg.projects[0].connections.len(), 2);
        assert_eq!(cfg.projects[0].connections[0].engine, Engine::Postgres);
        assert_eq!(cfg.projects[0].connections[1].engine, Engine::Mssql);
        assert!(cfg.projects[0].connections[1].read_only);
        assert_eq!(cfg.ui.page_size, 500);
    }

    #[test]
    fn defaults_apply() {
        let cfg = Config::parse(
            r#"
[[project]]
name = "P"
  [[project.connection]]
  name = "c"
  engine = "postgres"
  host = "h"
  database = "d"
  user = "u"
"#,
        )
        .unwrap();
        let c = &cfg.projects[0].connections[0];
        assert_eq!(c.port_or_default(), 5432);
        assert!(!c.read_only);
        assert_eq!(cfg.ui.max_column_width, 48);
    }

    #[test]
    fn rejects_duplicate_connection_names() {
        let err = Config::parse(
            r#"
[[project]]
name = "P"
  [[project.connection]]
  name = "c"
  engine = "postgres"
  host = "h"
  database = "d"
  user = "u"
  [[project.connection]]
  name = "c"
  engine = "mssql"
  host = "h"
  database = "d"
  user = "u"
"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("duplicate"));
    }

    #[test]
    fn password_command_first_line_only() {
        let c = Connection {
            name: "x".into(),
            engine: Engine::Postgres,
            host: "h".into(),
            port: None,
            database: "d".into(),
            user: "u".into(),
            keyring: None,
            password_command: Some("printf 'secret\\nnoise\\n'".into()),
            password: None,
            sslmode: None,
            trust_cert: false,
            read_only: false,
        };
        assert_eq!(c.resolve_password().unwrap(), "secret");
    }
}
