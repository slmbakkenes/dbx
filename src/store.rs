//! Writing connections back to `config.toml` without destroying the file.
//!
//! Everything goes through `toml_edit`, so comments, ordering and whitespace
//! that the user put there survive a save. Secrets never enter the document —
//! they go to the OS keyring, keyed `dbx` / `<project>/<name>`.

use crate::config::{Connection, Engine};
use anyhow::{Context, Result, anyhow};
use std::path::Path;
use toml_edit::{ArrayOfTables, DocumentMut, Item, Table, value};

/// Keyring account for a connection. Includes the project so two projects can
/// each have a `local` without colliding.
pub fn keyring_account(project: &str, name: &str) -> String {
    format!("{project}/{name}")
}

pub fn set_password(account: &str, password: &str) -> Result<()> {
    keyring::Entry::new("dbx", account)
        .with_context(|| format!("keyring entry dbx/{account}"))?
        .set_password(password)
        .with_context(|| format!("storing keyring dbx/{account}"))
}

/// Best-effort removal; a missing entry is not an error.
pub fn delete_password(account: &str) -> Result<()> {
    let entry = keyring::Entry::new("dbx", account)
        .with_context(|| format!("keyring entry dbx/{account}"))?;
    match entry.delete_credential() {
        Ok(()) => Ok(()),
        Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(e).with_context(|| format!("deleting keyring dbx/{account}")),
    }
}

fn read(path: &Path) -> Result<DocumentMut> {
    let raw = if path.exists() {
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?
    } else {
        String::new()
    };
    raw.parse::<DocumentMut>()
        .with_context(|| format!("parsing {}", path.display()))
}

fn write(path: &Path, doc: &DocumentMut) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    // Write via a sibling temp file so an interrupted save cannot truncate the
    // real config.
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, doc.to_string()).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

fn projects_mut(doc: &mut DocumentMut) -> &mut ArrayOfTables {
    if !doc.contains_key("project") {
        doc.insert("project", Item::ArrayOfTables(ArrayOfTables::new()));
    }
    doc["project"]
        .as_array_of_tables_mut()
        .expect("`project` must be an array of tables")
}

fn find_project<'a>(aot: &'a mut ArrayOfTables, name: &str) -> Option<&'a mut Table> {
    aot.iter_mut()
        .find(|t| t.get("name").and_then(|v| v.as_str()) == Some(name))
}

fn connections_mut(project: &mut Table) -> &mut ArrayOfTables {
    if !project.contains_key("connection") {
        project.insert("connection", Item::ArrayOfTables(ArrayOfTables::new()));
    }
    project["connection"]
        .as_array_of_tables_mut()
        .expect("`connection` must be an array of tables")
}

fn conn_index(aot: &ArrayOfTables, name: &str) -> Option<usize> {
    aot.iter()
        .position(|t| t.get("name").and_then(|v| v.as_str()) == Some(name))
}

/// Indents a freshly created table so it lines up with hand-written entries.
fn indent(table: &mut Table, keys: &[&str], depth: usize) {
    let pad = "  ".repeat(depth);
    table.decor_mut().set_prefix(format!("\n{pad}"));
    for k in keys {
        if let Some(mut km) = table.key_mut(k) {
            km.leaf_decor_mut().set_prefix(pad.clone());
        }
    }
}

fn set_str(t: &mut Table, key: &str, v: Option<&str>) {
    match v {
        Some(s) => t[key] = value(s),
        None => {
            t.remove(key);
        }
    }
}

/// Writes `v` only when it differs from the implicit default, so generated
/// entries stay as short as hand-written ones.
fn set_bool(t: &mut Table, key: &str, v: bool, default: bool) {
    if v == default {
        t.remove(key);
    } else {
        t[key] = value(v);
    }
}

const CONN_KEYS: &[&str] = &[
    "name",
    "engine",
    "host",
    "port",
    "database",
    "user",
    "keyring",
    "password_command",
    "password",
    "sslmode",
    "trust_cert",
    "read_only",
];

fn apply(t: &mut Table, c: &Connection) {
    t["name"] = value(&c.name);
    t["engine"] = value(match c.engine {
        Engine::Postgres => "postgres",
        Engine::Mssql => "mssql",
    });
    t["host"] = value(&c.host);
    t["port"] = value(c.port_or_default() as i64);
    t["database"] = value(&c.database);
    t["user"] = value(&c.user);
    set_str(t, "keyring", c.keyring.as_deref());
    set_str(t, "password_command", c.password_command.as_deref());
    set_str(t, "password", c.password.as_deref());
    set_str(
        t,
        "sslmode",
        if c.engine == Engine::Postgres {
            c.sslmode.as_deref()
        } else {
            None
        },
    );
    set_bool(t, "trust_cert", c.trust_cert, false);
    set_bool(t, "read_only", c.read_only, false);
}

pub fn add_project(path: &Path, name: &str) -> Result<()> {
    let mut doc = read(path)?;
    let aot = projects_mut(&mut doc);
    if find_project(aot, name).is_some() {
        return Err(anyhow!("a project named `{name}` already exists"));
    }
    let mut t = Table::new();
    t["name"] = value(name);
    indent(&mut t, &["name"], 0);
    aot.push(t);
    write(path, &doc)
}

/// Every `(connection name, keyring account)` pair in a project whose account
/// is the one dbx would have generated.
fn managed_accounts(project_table: &Table, project: &str) -> Vec<(String, String)> {
    let Some(conns) = project_table
        .get("connection")
        .and_then(|i| i.as_array_of_tables())
    else {
        return Vec::new();
    };
    conns
        .iter()
        .filter_map(|t| {
            let name = t.get("name")?.as_str()?.to_string();
            let account = t.get("keyring")?.as_str()?.to_string();
            (account == keyring_account(project, &name)).then_some((name, account))
        })
        .collect()
}

/// Moves a keyring secret to a new account. Returns false when the old secret
/// could not be read, in which case nothing was changed.
fn migrate_secret(old: &str, new: &str) -> bool {
    if old == new {
        return true;
    }
    let Ok(entry) = keyring::Entry::new("dbx", old) else {
        return false;
    };
    let Ok(pw) = entry.get_password() else {
        return false;
    };
    if set_password(new, &pw).is_err() {
        return false;
    }
    let _ = delete_password(old);
    true
}

/// Renames a project and moves its connections' keyring accounts along with it,
/// since the account is `<project>/<name>`. A secret that cannot be moved is
/// left where it is and reported, so the connection keeps working.
pub fn rename_project(path: &Path, old: &str, new: &str) -> Result<Vec<String>> {
    let mut doc = read(path)?;
    let aot = projects_mut(&mut doc);
    if old != new && find_project(aot, new).is_some() {
        return Err(anyhow!("a project named `{new}` already exists"));
    }
    let t = find_project(aot, old).ok_or_else(|| anyhow!("no project named `{old}`"))?;

    let mut warnings = Vec::new();
    if old != new {
        let managed = managed_accounts(t, old);
        let mut moved: Vec<(String, String)> = Vec::new();
        for (conn_name, old_account) in managed {
            let new_account = keyring_account(new, &conn_name);
            if migrate_secret(&old_account, &new_account) {
                moved.push((conn_name, new_account));
            } else {
                warnings.push(format!(
                    "could not move the stored password for `{conn_name}`; it stays under \
                     keyring account `{old_account}`"
                ));
            }
        }
        if let Some(conns) = t
            .get_mut("connection")
            .and_then(|i| i.as_array_of_tables_mut())
        {
            for (conn_name, new_account) in moved {
                if let Some(i) = conn_index(conns, &conn_name)
                    && let Some(ct) = conns.get_mut(i)
                {
                    ct["keyring"] = value(&new_account);
                }
            }
        }
    }
    let t = find_project(projects_mut(&mut doc), old).expect("found above");
    t["name"] = value(new);
    write(path, &doc)?;
    Ok(warnings)
}

/// Removes a project, deleting the keyring secrets dbx created for it.
pub fn remove_project(path: &Path, name: &str) -> Result<Vec<String>> {
    let mut doc = read(path)?;
    let aot = projects_mut(&mut doc);
    let idx = aot
        .iter()
        .position(|t| t.get("name").and_then(|v| v.as_str()) == Some(name))
        .ok_or_else(|| anyhow!("no project named `{name}`"))?;
    let accounts: Vec<String> = managed_accounts(aot.get(idx).expect("index above"), name)
        .into_iter()
        .map(|(_, a)| a)
        .collect();
    aot.remove(idx);
    write(path, &doc)?;

    let mut warnings = Vec::new();
    for a in accounts {
        if let Err(e) = delete_password(&a) {
            warnings.push(format!("{e:#}"));
        }
    }
    Ok(warnings)
}

/// Inserts or updates a connection. `original` is `(project, name)` as it was
/// before an edit, so renames and moves between projects work.
pub fn upsert_connection(
    path: &Path,
    project: &str,
    original: Option<(&str, &str)>,
    conn: &Connection,
) -> Result<()> {
    let mut doc = read(path)?;

    // A rename or a move leaves an entry behind under the old identity.
    if let Some((old_project, old_name)) = original
        && (old_project != project || old_name != conn.name)
    {
        let aot = projects_mut(&mut doc);
        if let Some(p) = find_project(aot, old_project) {
            let conns = connections_mut(p);
            if let Some(i) = conn_index(conns, old_name) {
                conns.remove(i);
            }
        }
    }

    let aot = projects_mut(&mut doc);
    if find_project(aot, project).is_none() {
        let mut t = Table::new();
        t["name"] = value(project);
        indent(&mut t, &["name"], 0);
        aot.push(t);
    }
    let p = find_project(aot, project).expect("created above");
    let conns = connections_mut(p);
    match conn_index(conns, &conn.name) {
        Some(i) => apply(conns.get_mut(i).expect("index from conn_index"), conn),
        None => {
            let mut t = Table::new();
            apply(&mut t, conn);
            indent(&mut t, CONN_KEYS, 1);
            conns.push(t);
        }
    }
    write(path, &doc)
}

pub fn remove_connection(path: &Path, project: &str, name: &str) -> Result<()> {
    let mut doc = read(path)?;
    let aot = projects_mut(&mut doc);
    let p = find_project(aot, project).ok_or_else(|| anyhow!("no project named `{project}`"))?;
    let conns = connections_mut(p);
    let idx = conn_index(conns, name).ok_or_else(|| anyhow!("no connection `{project}/{name}`"))?;
    conns.remove(idx);
    write(path, &doc)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn conn(name: &str, engine: Engine) -> Connection {
        Connection {
            name: name.into(),
            engine,
            host: "db.internal".into(),
            port: Some(5555),
            database: "acme".into(),
            user: "svc".into(),
            keyring: Some(format!("Acme/{name}")),
            password_command: None,
            password: None,
            sslmode: Some("require".into()),
            trust_cert: false,
            read_only: false,
        }
    }

    fn tmp(body: &str) -> (tempdir::Dir, std::path::PathBuf) {
        let dir = tempdir::Dir::new();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, body).unwrap();
        (dir, path)
    }

    /// A minimal scoped temp directory; avoids a dev-dependency.
    mod tempdir {
        use std::path::{Path, PathBuf};
        pub struct Dir(PathBuf);
        impl Dir {
            pub fn new() -> Self {
                let p = std::env::temp_dir().join(format!(
                    "dbx-store-test-{}-{:?}",
                    std::process::id(),
                    std::thread::current().id()
                ));
                let _ = std::fs::remove_dir_all(&p);
                std::fs::create_dir_all(&p).unwrap();
                Dir(p)
            }
            pub fn path(&self) -> &Path {
                &self.0
            }
        }
        impl Drop for Dir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }

    const WITH_COMMENTS: &str = r#"# my carefully curated config
[ui]
page_size = 250   # trailing comment

[[project]]
name = "Acme"

  # the local one
  [[project.connection]]
  name = "local"
  engine = "postgres"
  host = "127.0.0.1"
  database = "acme"
  user = "postgres"
  sslmode = "disable"
"#;

    #[test]
    fn upsert_preserves_comments_and_formatting() {
        let (_d, path) = tmp(WITH_COMMENTS);
        upsert_connection(&path, "Acme", None, &conn("warehouse", Engine::Mssql)).unwrap();
        let out = std::fs::read_to_string(&path).unwrap();
        assert!(out.contains("# my carefully curated config"));
        assert!(out.contains("page_size = 250   # trailing comment"));
        assert!(out.contains("# the local one"));
        assert!(out.contains("name = \"warehouse\""));
        // Still valid, and the new entry is really there.
        let cfg = Config::parse(&out).unwrap();
        assert_eq!(cfg.projects[0].connections.len(), 2);
        assert_eq!(cfg.projects[0].connections[1].name, "warehouse");
        assert_eq!(cfg.projects[0].connections[1].engine, Engine::Mssql);
    }

    #[test]
    fn upsert_updates_in_place_without_duplicating() {
        let (_d, path) = tmp(WITH_COMMENTS);
        let mut c = conn("local", Engine::Postgres);
        c.host = "moved.example.com".into();
        upsert_connection(&path, "Acme", Some(("Acme", "local")), &c).unwrap();
        let out = std::fs::read_to_string(&path).unwrap();
        let cfg = Config::parse(&out).unwrap();
        assert_eq!(cfg.projects[0].connections.len(), 1);
        assert_eq!(cfg.projects[0].connections[0].host, "moved.example.com");
        assert!(
            out.contains("# the local one"),
            "comment must survive an edit"
        );
    }

    #[test]
    fn rename_does_not_leave_the_old_entry_behind() {
        let (_d, path) = tmp(WITH_COMMENTS);
        let c = conn("renamed", Engine::Postgres);
        upsert_connection(&path, "Acme", Some(("Acme", "local")), &c).unwrap();
        let cfg = Config::parse(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let names: Vec<&str> = cfg.projects[0]
            .connections
            .iter()
            .map(|c| c.name.as_str())
            .collect();
        assert_eq!(names, ["renamed"]);
    }

    #[test]
    fn moving_between_projects_creates_the_target_and_empties_the_source() {
        let (_d, path) = tmp(WITH_COMMENTS);
        let c = conn("local", Engine::Postgres);
        upsert_connection(&path, "Beta", Some(("Acme", "local")), &c).unwrap();
        let cfg = Config::parse(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(cfg.projects.len(), 2);
        assert!(cfg.projects[0].connections.is_empty());
        assert_eq!(cfg.projects[1].name, "Beta");
        assert_eq!(cfg.projects[1].connections[0].name, "local");
    }

    #[test]
    fn secrets_never_reach_the_document() {
        let (_d, path) = tmp(WITH_COMMENTS);
        let c = conn("warehouse", Engine::Mssql);
        upsert_connection(&path, "Acme", None, &c).unwrap();
        let out = std::fs::read_to_string(&path).unwrap();
        assert!(out.contains("keyring = \"Acme/warehouse\""));
        assert!(!out.contains("password ="));
    }

    #[test]
    fn mssql_entries_omit_sslmode_and_defaults_are_left_out() {
        let (_d, path) = tmp(WITH_COMMENTS);
        upsert_connection(&path, "Acme", None, &conn("ms", Engine::Mssql)).unwrap();
        let out = std::fs::read_to_string(&path).unwrap();
        let block = out.split("name = \"ms\"").nth(1).unwrap();
        assert!(!block.contains("sslmode"));
        assert!(!block.contains("trust_cert"), "false is the default");
        assert!(!block.contains("read_only"));
    }

    #[test]
    fn remove_connection_deletes_only_that_entry() {
        let (_d, path) = tmp(WITH_COMMENTS);
        upsert_connection(&path, "Acme", None, &conn("second", Engine::Postgres)).unwrap();
        remove_connection(&path, "Acme", "local").unwrap();
        let cfg = Config::parse(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(cfg.projects[0].connections.len(), 1);
        assert_eq!(cfg.projects[0].connections[0].name, "second");
    }

    #[test]
    fn project_lifecycle() {
        let (_d, path) = tmp(WITH_COMMENTS);
        add_project(&path, "Beta").unwrap();
        assert!(add_project(&path, "Beta").is_err(), "duplicate must fail");
        assert!(rename_project(&path, "Beta", "Gamma").unwrap().is_empty());
        let cfg = Config::parse(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(cfg.projects[1].name, "Gamma");
        remove_project(&path, "Gamma").unwrap();
        let cfg = Config::parse(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(cfg.projects.len(), 1);
        assert!(remove_project(&path, "Nope").is_err());
    }

    #[test]
    fn works_on_an_empty_or_missing_file() {
        let dir = tempdir::Dir::new();
        let path = dir.path().join("fresh.toml");
        upsert_connection(&path, "New", None, &conn("first", Engine::Postgres)).unwrap();
        let cfg = Config::parse(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(cfg.projects[0].name, "New");
        assert_eq!(cfg.projects[0].connections[0].name, "first");
    }

    #[test]
    fn generated_entries_are_indented_like_hand_written_ones() {
        let (_d, path) = tmp(WITH_COMMENTS);
        upsert_connection(&path, "Acme", None, &conn("warehouse", Engine::Mssql)).unwrap();
        add_project(&path, "Beta").unwrap();
        upsert_connection(&path, "Beta", None, &conn("reporting", Engine::Postgres)).unwrap();
        let out = std::fs::read_to_string(&path).unwrap();
        eprintln!("----\n{out}----");
        assert!(out.contains("\n  [[project.connection]]\n  name = \"warehouse\""));
        assert!(out.contains("\n[[project]]\nname = \"Beta\""));
    }

    #[test]
    fn renaming_a_project_rewrites_managed_keyring_accounts() {
        let (_d, path) = tmp(WITH_COMMENTS);
        // `local` has no keyring key, so nothing to migrate; add one that does.
        upsert_connection(&path, "Acme", None, &conn("warehouse", Engine::Mssql)).unwrap();
        // No secret exists in the keyring, so the move is reported, not silent.
        let warnings = rename_project(&path, "Acme", "Globex").unwrap();
        let out = std::fs::read_to_string(&path).unwrap();
        let cfg = Config::parse(&out).unwrap();
        assert_eq!(cfg.projects[0].name, "Globex");
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("warehouse"));
        // The account still points somewhere that resolves.
        assert_eq!(
            cfg.projects[0].connections[1].keyring.as_deref(),
            Some("Acme/warehouse")
        );
    }

    #[test]
    fn managed_accounts_ignores_hand_written_keyring_names() {
        let (_d, path) = tmp(WITH_COMMENTS);
        let mut c = conn("custom", Engine::Postgres);
        c.keyring = Some("my-own-account".into());
        upsert_connection(&path, "Acme", None, &c).unwrap();
        let warnings = rename_project(&path, "Acme", "Renamed").unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
        let cfg = Config::parse(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            cfg.projects[0].connections[1].keyring.as_deref(),
            Some("my-own-account"),
            "a hand-picked account must be left alone"
        );
    }

    #[test]
    fn keyring_account_includes_the_project() {
        assert_eq!(keyring_account("Acme", "local"), "Acme/local");
    }
}
