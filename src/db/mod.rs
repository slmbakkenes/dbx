//! Database access: one worker task per connection, driver trait per engine.

pub mod mssql;
pub mod postgres;
pub mod worker;

use anyhow::Result;
use async_trait::async_trait;

pub type ConnId = usize;
pub type ReqId = u64;

/// A result set, already stringified for display. `None` cell == SQL NULL.
#[derive(Debug, Clone)]
pub struct Grid {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Option<String>>>,
    pub affected: Option<u64>,
    pub elapsed_ms: u64,
    /// True when the server had more rows than we asked for.
    pub complete: bool,
}

/// Out-of-band statement cancellation, cloneable and independent of `&mut Driver`.
#[derive(Clone, Default)]
pub enum Canceller {
    /// The engine offers no out-of-band cancel; the worker drops the
    /// connection instead.
    #[default]
    Unsupported,
    Postgres(tokio_postgres::CancelToken),
}

impl Canceller {
    pub async fn cancel(&self) -> bool {
        match self {
            Canceller::Unsupported => false,
            Canceller::Postgres(t) => {
                let _ = t.cancel_query(tokio_postgres::NoTls).await;
                true
            }
        }
    }
}

impl Default for Grid {
    fn default() -> Self {
        Self {
            columns: Vec::new(),
            rows: Vec::new(),
            affected: None,
            elapsed_ms: 0,
            // An empty grid is not a truncated one.
            complete: true,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ColumnMeta {
    pub name: String,
    pub data_type: String,
    pub nullable: bool,
    pub default: Option<String>,
}

#[async_trait]
pub trait Driver: Send {
    /// Databases on this server that the login may connect to.
    async fn databases(&mut self) -> Result<Vec<String>>;
    async fn schemas(&mut self) -> Result<Vec<String>>;
    async fn tables(&mut self, schema: &str) -> Result<Vec<String>>;
    async fn columns(&mut self, schema: &str, table: &str) -> Result<Grid>;
    /// Runs one statement, buffering at most `max_rows` rows.
    async fn query(&mut self, sql: &str, max_rows: usize) -> Result<Grid>;
    /// `SELECT ... LIMIT/OFFSET` spelled for this engine.
    fn page_sql(&self, schema: &str, table: &str, limit: usize, offset: usize) -> String;
    /// A cancel handle that can be used while `query` holds `&mut self`.
    fn canceller(&self) -> Canceller;
}

/// Renders column metadata as a displayable grid.
pub fn structure_grid(cols: &[(ColumnMeta, bool)]) -> Grid {
    Grid {
        columns: vec![
            "column".into(),
            "type".into(),
            "null".into(),
            "default".into(),
            "pk".into(),
        ],
        rows: cols
            .iter()
            .map(|(c, pk)| {
                vec![
                    Some(c.name.clone()),
                    Some(c.data_type.clone()),
                    Some(if c.nullable { "YES" } else { "NO" }.to_string()),
                    c.default.clone(),
                    Some(if *pk { "PK" } else { "" }.to_string()),
                ]
            })
            .collect(),
        affected: None,
        elapsed_ms: 0,
        complete: true,
    }
}

/// Splits a script into statements, respecting quotes, dollar-quoting,
/// line comments and block comments. Semicolons are dropped.
pub fn split_statements(sql: &str) -> Vec<(usize, usize)> {
    let b = sql.as_bytes();
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;
    while i < b.len() {
        match b[i] {
            b'\'' | b'"' => {
                let q = b[i];
                i += 1;
                while i < b.len() {
                    if b[i] == q {
                        // doubled quote is an escaped quote
                        if i + 1 < b.len() && b[i + 1] == q {
                            i += 2;
                            continue;
                        }
                        break;
                    }
                    i += 1;
                }
                i += 1;
            }
            b'-' if i + 1 < b.len() && b[i + 1] == b'-' => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if i + 1 < b.len() && b[i + 1] == b'*' => {
                i += 2;
                while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                    i += 1;
                }
                i = (i + 2).min(b.len());
            }
            b'$' => {
                // dollar-quoted string: $tag$ ... $tag$
                if let Some(end_tag) = b[i + 1..].iter().position(|&c| c == b'$') {
                    let tag = &sql[i..i + end_tag + 2];
                    let rest = &sql[i + tag.len()..];
                    if let Some(close) = rest.find(tag) {
                        i += tag.len() + close + tag.len();
                    } else {
                        i = b.len();
                    }
                } else {
                    i += 1;
                }
            }
            b';' => {
                if !sql[start..i].trim().is_empty() {
                    out.push((start, i));
                }
                start = i + 1;
                i += 1;
            }
            _ => i += 1,
        }
    }
    if !sql[start.min(sql.len())..].trim().is_empty() {
        out.push((start, sql.len()));
    }
    out
}

/// The statement containing `cursor`, as a byte range into `sql`.
pub fn statement_at(sql: &str, cursor: usize) -> Option<(usize, usize)> {
    let stmts = split_statements(sql);
    stmts
        .iter()
        .find(|(s, e)| cursor >= *s && cursor <= *e)
        .or_else(|| stmts.last())
        .copied()
}

/// Rough classification used to pick `query` vs `execute` and to gate writes.
pub fn is_read_only(sql: &str) -> bool {
    let head = first_keyword(sql);
    matches!(
        head.as_str(),
        "select" | "with" | "show" | "explain" | "describe" | "table" | "values"
    )
}

pub fn returns_rows(sql: &str) -> bool {
    if is_read_only(sql) {
        return true;
    }
    // INSERT/UPDATE/DELETE ... RETURNING, and MSSQL OUTPUT.
    let lower = sql.to_ascii_lowercase();
    lower.contains(" returning ") || lower.contains("\nreturning")
}

fn first_keyword(sql: &str) -> String {
    let mut s = sql.trim_start();
    // strip leading comments
    loop {
        if s.starts_with("--") {
            s = s.split_once('\n').map(|x| x.1).unwrap_or("").trim_start();
        } else if s.starts_with("/*") {
            s = s.split_once("*/").map(|x| x.1).unwrap_or("").trim_start();
        } else {
            break;
        }
    }
    s.split(|c: char| !c.is_alphanumeric() && c != '_')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parts(sql: &str) -> Vec<&str> {
        split_statements(sql)
            .into_iter()
            .map(|(s, e)| sql[s..e].trim())
            .collect()
    }

    #[test]
    fn splits_plain_statements() {
        assert_eq!(parts("select 1; select 2"), vec!["select 1", "select 2"]);
        assert_eq!(parts("select 1;;  ;"), vec!["select 1"]);
        assert_eq!(parts(""), Vec::<&str>::new());
    }

    #[test]
    fn ignores_semicolons_in_literals_and_comments() {
        assert_eq!(parts("select ';'"), vec!["select ';'"]);
        assert_eq!(
            parts("select \"a;b\" from t"),
            vec!["select \"a;b\" from t"]
        );
        assert_eq!(parts("select 1 -- ; nope\n"), vec!["select 1 -- ; nope"]);
        assert_eq!(parts("select /* ; */ 1"), vec!["select /* ; */ 1"]);
        assert_eq!(parts("select 'it''s; fine'"), vec!["select 'it''s; fine'"]);
    }

    #[test]
    fn handles_dollar_quoting() {
        let sql = "create function f() returns int as $$ begin; return 1; end; $$ language plpgsql; select 2";
        let p = parts(sql);
        assert_eq!(p.len(), 2);
        assert!(p[0].starts_with("create function"));
        assert_eq!(p[1], "select 2");
    }

    #[test]
    fn statement_at_picks_the_right_one() {
        let sql = "select 1;\nselect 2;\nselect 3";
        let (s, e) = statement_at(sql, 12).unwrap();
        assert_eq!(sql[s..e].trim(), "select 2");
        let (s, e) = statement_at(sql, 0).unwrap();
        assert_eq!(sql[s..e].trim(), "select 1");
    }

    #[test]
    fn classifies_statements() {
        assert!(is_read_only("  SELECT 1"));
        assert!(is_read_only("-- c\nWITH x AS (select 1) select * from x"));
        assert!(!is_read_only("update t set a = 1"));
        assert!(!is_read_only("DROP TABLE t"));
        assert!(returns_rows("insert into t values (1) returning id"));
        assert!(!returns_rows("insert into t values (1)"));
    }
}
