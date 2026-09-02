//! `dbx --check` — explains why a connection will not log in, without ever
//! printing the password itself.

use crate::config::{Config, Connection, Engine};
use crate::db::Driver;
use anyhow::Result;
use sha2::{Digest, Sha256};

/// Which config field supplied the password.
fn source(c: &Connection) -> &'static str {
    if c.keyring.is_some() {
        "keyring"
    } else if c.password_command.is_some() {
        "password_command"
    } else if c.password.is_some() {
        "password (cleartext)"
    } else {
        "none"
    }
}

fn fingerprint(pw: &str) -> String {
    let digest = Sha256::digest(pw.as_bytes());
    digest.iter().take(4).map(|b| format!("{b:02x}")).collect()
}

/// Problems that produce "Login failed" while looking correct in a config file.
pub fn password_warnings(pw: &str) -> Vec<String> {
    let mut out = Vec::new();
    if pw.is_empty() {
        out.push("the resolved password is empty".into());
        return out;
    }
    if pw != pw.trim() {
        let lead = pw.len() - pw.trim_start().len();
        let trail = pw.len() - pw.trim_end().len();
        out.push(format!(
            "surrounding whitespace: {lead} leading, {trail} trailing byte(s) — \
             a stored trailing newline is the usual cause"
        ));
    }
    // Only a newline *inside* the value is a separate problem; a trailing one
    // is already covered above.
    let trimmed = pw.trim();
    if trimmed.contains('\n') || trimmed.contains('\r') {
        out.push("contains a newline in the middle of the value".into());
    }
    if !pw.is_ascii() {
        out.push("contains non-ASCII characters — check the encoding".into());
    }
    out
}

pub async fn probe(c: &Connection) -> Result<String> {
    let mut driver: Box<dyn Driver> = match c.engine {
        Engine::Postgres => Box::new(crate::db::postgres::PgDriver::connect(c).await?),
        Engine::Mssql => Box::new(crate::db::mssql::MsDriver::connect(c).await?),
    };
    let grid = driver.query("select 1", 1).await?;
    Ok(format!(
        "ok — {} row(s) returned by `select 1`",
        grid.rows.len()
    ))
}

pub async fn run(cfg: &Config, filter: Option<&str>) -> Result<()> {
    let mut checked = 0usize;
    let mut failed = 0usize;

    for project in &cfg.projects {
        for c in &project.connections {
            let full = format!("{}/{}", project.name, c.name);
            if filter.is_some_and(|f| f != c.name && f != full) {
                continue;
            }
            checked += 1;
            println!("\n{full}  ({})", c.engine.label());
            println!(
                "  target        {}:{}/{}",
                c.host,
                c.port_or_default(),
                c.database
            );
            println!("  user          {}", c.user);

            let pw = match c.resolve_password() {
                Ok(pw) => {
                    println!(
                        "  password      {} → {} chars, sha256:{}",
                        source(c),
                        pw.chars().count(),
                        fingerprint(&pw)
                    );
                    for w in password_warnings(&pw) {
                        println!("  warning       {w}");
                    }
                    Some(pw)
                }
                Err(e) => {
                    println!("  password      {} → FAILED: {e:#}", source(c));
                    failed += 1;
                    None
                }
            };

            if pw.is_none() {
                continue;
            }
            if c.engine == Engine::Mssql {
                println!("  auth          SQL login (Windows/Azure AD is not supported)");
            }
            match probe(c).await {
                Ok(msg) => println!("  connect       {msg}"),
                Err(e) => {
                    failed += 1;
                    let msg = format!("{e:#}");
                    println!("  connect       FAILED: {msg}");
                    for hint in hints(c, &msg) {
                        println!("  hint          {hint}");
                    }
                }
            }
        }
    }

    if checked == 0 {
        match filter {
            Some(f) => println!("no connection named `{f}` in the config"),
            None => println!("no connections configured"),
        }
    } else {
        println!("\n{checked} checked, {failed} failed");
    }
    Ok(())
}

/// Maps the server's own wording onto the thing that is actually misconfigured.
pub fn hints(c: &Connection, err: &str) -> Vec<String> {
    let low = err.to_ascii_lowercase();
    let mut out = Vec::new();

    if low.contains("login failed for user") {
        out.push(
            "the server accepted the connection and rejected the credential, so \
             host, port and TLS are all fine"
                .into(),
        );
        out.push(format!(
            "compare the stored secret with the real one: \
             `security find-generic-password -s dbx -a {} -w | tr -d '\\n' | shasum -a 256 | cut -c1-8` \
             should match the sha256 above",
            c.keyring.as_deref().unwrap_or("<account>")
        ));
        out.push(
            "if this is a Windows/domain account it will never work here — dbx \
             only speaks SQL logins"
                .into(),
        );
        out.push(
            "check the login is enabled and the server allows SQL auth: \
             `SELECT is_disabled FROM sys.server_principals WHERE name = '<user>'` \
             and Server Properties → Security → SQL Server and Windows Authentication mode"
                .into(),
        );
    }
    if low.contains("cannot open database") || low.contains("4060") {
        out.push(format!(
            "the login is valid but has no access to `{}` — ask for db_datareader on it",
            c.database
        ));
    }
    if low.contains("password authentication failed") {
        out.push("Postgres rejected the password; check pg_hba.conf method too".into());
    }
    if low.contains("connection refused") {
        out.push("nothing is listening — wrong port, or the server/tunnel is down".into());
    }
    if low.contains("certificate") || low.contains("tls") || low.contains("handshake") {
        out.push(
            "set `trust_cert = true` (mssql) or `sslmode = \"disable\"` (postgres) to test".into(),
        );
    }
    if low.contains("no password source") {
        out.push("set one of keyring, password_command or password".into());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Engine;

    fn conn() -> Connection {
        Connection {
            name: "c".into(),
            engine: Engine::Mssql,
            host: "h".into(),
            port: None,
            database: "Reporting".into(),
            user: "globex".into(),
            keyring: Some("zv".into()),
            password_command: None,
            password: None,
            sslmode: None,
            trust_cert: false,
            read_only: false,
        }
    }

    #[test]
    fn fingerprint_is_stable_and_short() {
        assert_eq!(fingerprint("hunter2").len(), 8);
        assert_eq!(fingerprint("hunter2"), fingerprint("hunter2"));
        assert_ne!(fingerprint("hunter2"), fingerprint("hunter3"));
    }

    #[test]
    fn flags_trailing_newline() {
        let w = password_warnings("secret\n");
        assert_eq!(w.len(), 1);
        assert!(w[0].contains("trailing"));
    }

    #[test]
    fn flags_a_newline_inside_the_value_separately() {
        let w = password_warnings("se\ncret");
        assert_eq!(w.len(), 1);
        assert!(w[0].contains("middle"));
    }

    #[test]
    fn flags_empty_and_non_ascii() {
        assert!(password_warnings("")[0].contains("empty"));
        assert!(password_warnings("pässwörd")[0].contains("non-ASCII"));
    }

    #[test]
    fn clean_password_has_no_warnings() {
        assert!(password_warnings("Str0ng-P@ss").is_empty());
    }

    #[test]
    fn login_failure_hints_mention_sql_logins_and_the_keyring_account() {
        let h = hints(&conn(), "Login failed for user 'globex'.");
        assert!(h.iter().any(|x| x.contains("SQL logins")));
        assert!(h.iter().any(|x| x.contains("-a zv")));
        assert!(h.iter().any(|x| x.contains("host, port and TLS")));
    }

    #[test]
    fn database_access_failure_names_the_database() {
        let h = hints(
            &conn(),
            "Cannot open database \"Reporting\" requested by the login.",
        );
        assert!(h.iter().any(|x| x.contains("Reporting")));
    }

    #[test]
    fn source_reports_the_field_in_use() {
        let mut c = conn();
        assert_eq!(source(&c), "keyring");
        c.keyring = None;
        c.password_command = Some("x".into());
        assert_eq!(source(&c), "password_command");
        c.password_command = None;
        assert_eq!(source(&c), "none");
    }
}
