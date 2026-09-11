//! Keyword and identifier completion for the query editor.
//!
//! Everything here is pure: candidates come from what the app already knows —
//! the keyword list for the engine, the schemas and tables the tree has
//! loaded, and the columns learned from opening a table. No queries are sent
//! to complete, so the popup is instant and works offline.

use crate::config::Engine;
use std::collections::HashMap;

/// How many candidates the popup will ever hold.
const MAX_ITEMS: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    Column,
    Table,
    Schema,
    Keyword,
}

impl Kind {
    pub fn tag(self) -> &'static str {
        match self {
            Kind::Column => "col",
            Kind::Table => "tbl",
            Kind::Schema => "sch",
            Kind::Keyword => "kw",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub text: String,
    pub kind: Kind,
    /// Where it came from: the schema for a table, `schema.table` for a column.
    pub detail: String,
}

impl Candidate {
    fn new(text: impl Into<String>, kind: Kind, detail: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            kind,
            detail: detail.into(),
        }
    }
}

/// The cached metadata for one connection, assembled by the caller.
#[derive(Debug, Clone)]
pub struct Sources {
    pub engine: Engine,
    pub schemas: Vec<String>,
    /// `(schema, table)`, as loaded into the tree.
    pub tables: Vec<(String, String)>,
    /// Columns known for a `(schema, table)`.
    pub columns: HashMap<(String, String), Vec<String>>,
}

impl Sources {
    fn default_schema(&self) -> &'static str {
        match self.engine {
            Engine::Postgres => "public",
            Engine::Mssql => "dbo",
        }
    }
}

/// An open popup: the candidates plus where in the buffer they replace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    pub items: Vec<Candidate>,
    pub selected: usize,
    /// Buffer line the replaced word sits on.
    pub line: usize,
    /// Char column the replaced word starts at.
    pub start_col: usize,
    /// First visible row, kept in range by `step`.
    pub scroll: usize,
}

impl Completion {
    pub fn new(items: Vec<Candidate>, line: usize, start_col: usize) -> Self {
        Self {
            items,
            selected: 0,
            line,
            start_col,
            scroll: 0,
        }
    }

    /// Moves the selection, wrapping at both ends.
    pub fn step(&mut self, delta: isize) {
        if self.items.is_empty() {
            return;
        }
        let n = self.items.len() as isize;
        self.selected = (((self.selected as isize + delta) % n + n) % n) as usize;
    }

    /// Keeps the selection inside a window `height` rows tall.
    pub fn scroll_into_view(&mut self, height: usize) {
        if height == 0 {
            return;
        }
        if self.selected < self.scroll {
            self.scroll = self.selected;
        } else if self.selected >= self.scroll + height {
            self.scroll = self.selected + 1 - height;
        }
    }
}

pub fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '$' | '#')
}

fn is_ident_byte(b: u8) -> bool {
    b >= 0x80 || (b as char).is_ascii_alphanumeric() || matches!(b, b'_' | b'$' | b'#')
}

/// Char column where the identifier ending at `col` starts.
pub fn word_start_col(line: &str, col: usize) -> usize {
    let chars: Vec<char> = line.chars().take(col).collect();
    let mut i = chars.len();
    while i > 0 && is_ident_char(chars[i - 1]) {
        i -= 1;
    }
    i
}

fn eq(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

#[derive(Debug, Clone, PartialEq)]
enum Tok<'a> {
    Word(&'a str),
    Dot,
    Punct(char),
}

/// Tokenises SQL well enough to find clause keywords and table references.
/// String literals and comments collapse to a single punctuation token.
fn tokens(s: &str) -> Vec<Tok<'_>> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < b.len() {
        match b[i] {
            b'\'' => {
                i += 1;
                while i < b.len() && b[i] != b'\'' {
                    i += 1;
                }
                i += 1;
                out.push(Tok::Punct('\''));
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
            q @ (b'"' | b'[') => {
                let close = if q == b'"' { b'"' } else { b']' };
                let start = i + 1;
                i += 1;
                while i < b.len() && b[i] != close {
                    i += 1;
                }
                out.push(Tok::Word(&s[start..i.min(s.len())]));
                i += 1;
            }
            b'.' => {
                out.push(Tok::Dot);
                i += 1;
            }
            c => {
                if (c as char).is_ascii_whitespace() {
                    i += 1;
                    continue;
                }
                let start = i;
                while i < b.len() && is_ident_byte(b[i]) {
                    i += 1;
                }
                if i > start {
                    out.push(Tok::Word(&s[start..i]));
                } else {
                    out.push(Tok::Punct(c as char));
                    i += 1;
                }
            }
        }
    }
    out
}

/// A table named in a `from`/`join`/`update`/`into` clause.
#[derive(Debug, Clone, PartialEq, Eq)]
struct TableRef {
    schema: Option<String>,
    table: String,
    alias: Option<String>,
}

/// Words that can never be an alias, so `from t where` does not read as
/// a table aliased `where`.
const CLAUSE_WORDS: &[&str] = &[
    "select",
    "from",
    "where",
    "group",
    "order",
    "having",
    "limit",
    "offset",
    "join",
    "inner",
    "left",
    "right",
    "full",
    "cross",
    "outer",
    "natural",
    "on",
    "using",
    "union",
    "except",
    "intersect",
    "set",
    "values",
    "returning",
    "and",
    "or",
    "not",
    "into",
    "update",
    "insert",
    "delete",
    "with",
    "as",
    "when",
    "then",
    "else",
    "end",
    "window",
    "fetch",
    "for",
    "apply",
    "lateral",
    "top",
    "distinct",
    "case",
    "by",
    "asc",
    "desc",
    "output",
];

const TABLE_WORDS: &[&str] = &["from", "join", "into", "update", "table", "apply"];

fn is_clause_word(w: &str) -> bool {
    CLAUSE_WORDS.iter().any(|c| eq(c, w))
}

fn table_refs(toks: &[Tok<'_>]) -> Vec<TableRef> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < toks.len() {
        let introduces = match &toks[i] {
            Tok::Word(w) => TABLE_WORDS.iter().any(|t| eq(t, w)),
            _ => false,
        };
        i += 1;
        if !introduces {
            continue;
        }
        // One clause may list several tables: `from a, b bee`.
        while let Some(r) = parse_ref(toks, &mut i) {
            out.push(r);
            if !matches!(toks.get(i), Some(Tok::Punct(','))) {
                break;
            }
            i += 1;
        }
    }
    out
}

/// Parses `[schema.]table [[as] alias]` starting at `i`.
fn parse_ref(toks: &[Tok<'_>], i: &mut usize) -> Option<TableRef> {
    let mut parts: Vec<String> = Vec::new();
    match toks.get(*i) {
        Some(Tok::Word(w)) if !is_clause_word(w) => {
            parts.push((*w).to_string());
            *i += 1;
        }
        _ => return None,
    }
    while matches!(toks.get(*i), Some(Tok::Dot)) {
        *i += 1;
        match toks.get(*i) {
            Some(Tok::Word(w)) => {
                parts.push((*w).to_string());
                *i += 1;
            }
            _ => break,
        }
    }
    // `as` is optional; either way the alias is a plain word.
    if matches!(toks.get(*i), Some(Tok::Word(w)) if eq(w, "as")) {
        *i += 1;
    }
    let alias = match toks.get(*i) {
        Some(Tok::Word(w)) if !is_clause_word(w) => {
            *i += 1;
            Some((*w).to_string())
        }
        _ => None,
    };
    let table = parts.pop()?;
    Some(TableRef {
        schema: parts.pop(),
        table,
        alias,
    })
}

/// Splits the text before the cursor into `(partial word, everything before it)`.
fn split_partial(before: &str) -> (String, &str) {
    let start = before
        .char_indices()
        .rev()
        .take_while(|(_, c)| is_ident_char(*c))
        .last()
        .map(|(i, _)| i)
        .unwrap_or(before.len());
    (before[start..].to_string(), &before[..start])
}

/// The dotted path in front of the partial word: `a.` → `["a"]`,
/// `public.users.` → `["public", "users"]`.
fn qualifier(mut before: &str) -> Vec<String> {
    let mut parts = Vec::new();
    while parts.len() < 2 {
        let trimmed = before.trim_end();
        let Some(rest) = trimmed.strip_suffix('.') else {
            break;
        };
        let (part, r) = trailing_ident(rest.trim_end());
        if part.is_empty() {
            break;
        }
        parts.push(part);
        before = r;
    }
    parts.reverse();
    parts
}

fn trailing_ident(s: &str) -> (String, &str) {
    for (open, close) in [('"', '"'), (']', '[')] {
        if s.ends_with(open)
            && let Some(p) = s[..s.len() - 1].rfind(close)
        {
            return (s[p + 1..s.len() - 1].to_string(), &s[..p]);
        }
    }
    let start = s
        .char_indices()
        .rev()
        .take_while(|(_, c)| is_ident_char(*c))
        .last()
        .map(|(i, _)| i)
        .unwrap_or(s.len());
    (s[start..].to_string(), &s[..start])
}

/// True when the cursor sits where a table name belongs.
fn expects_table(before: &str) -> bool {
    let toks = tokens(before);
    let mut seen_comma = false;
    for t in toks.iter().rev() {
        match t {
            Tok::Word(w) => {
                if TABLE_WORDS.iter().any(|k| eq(k, w)) {
                    return true;
                }
                if is_clause_word(w) || !seen_comma {
                    return false;
                }
            }
            Tok::Dot => {}
            Tok::Punct(',') => seen_comma = true,
            Tok::Punct(_) => return false,
        }
    }
    false
}

fn column_candidates(src: &Sources, schema: Option<&str>, table: &str) -> Vec<Candidate> {
    let mut out = Vec::new();
    for ((s, t), cols) in &src.columns {
        if !eq(t, table) || schema.is_some_and(|want| !eq(s, want)) {
            continue;
        }
        let detail = format!("{s}.{t}");
        out.extend(
            cols.iter()
                .map(|c| Candidate::new(c.clone(), Kind::Column, detail.clone())),
        );
    }
    out.sort_by(|a, b| a.text.cmp(&b.text));
    out
}

fn table_candidates(src: &Sources) -> Vec<Candidate> {
    let default = src.default_schema();
    let mut out = Vec::new();
    for (s, t) in &src.tables {
        out.push(Candidate::new(t.clone(), Kind::Table, s.clone()));
        // Outside the default schema the qualified form is what you need.
        if !eq(s, default) {
            out.push(Candidate::new(format!("{s}.{t}"), Kind::Table, s.clone()));
        }
    }
    out
}

/// True when the cursor sits inside a string literal or a comment, where an
/// identifier is not what you are typing.
pub fn in_literal(sql: &str, cursor: usize) -> bool {
    let b = sql.as_bytes();
    let end = cursor.min(b.len());
    let mut i = 0usize;
    while i < end {
        match b[i] {
            b'\'' => {
                i += 1;
                loop {
                    if i >= end {
                        // No closing quote before the cursor: it is inside.
                        return true;
                    }
                    if b[i] == b'\'' {
                        // A doubled quote is an escaped one, not the end.
                        if i + 1 < end && b[i + 1] == b'\'' {
                            i += 2;
                            continue;
                        }
                        i += 1;
                        break;
                    }
                    i += 1;
                }
            }
            b'-' if i + 1 < b.len() && b[i + 1] == b'-' => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
                if i >= end {
                    return true;
                }
            }
            b'/' if i + 1 < b.len() && b[i + 1] == b'*' => {
                i += 2;
                while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                    i += 1;
                }
                if i >= end {
                    return true;
                }
                i = (i + 2).min(b.len());
            }
            _ => i += 1,
        }
    }
    false
}

/// Every candidate for the cursor, already filtered and ranked.
pub fn candidates(sql: &str, cursor: usize, src: &Sources) -> Vec<Candidate> {
    let cursor = cursor.min(sql.len());
    if in_literal(sql, cursor) {
        return Vec::new();
    }
    let (s, e) = crate::db::statement_at(sql, cursor).unwrap_or((0, sql.len()));
    let stmt = &sql[s.min(cursor)..e.max(cursor).min(sql.len())];
    let before = &sql[s.min(cursor)..cursor];
    let (prefix, head) = split_partial(before);
    let quals = qualifier(head);
    let refs = table_refs(&tokens(stmt));

    let mut out = Vec::new();
    match quals.len() {
        // `schema.table.` — columns of exactly that table.
        2 => out.extend(column_candidates(src, Some(&quals[0]), &quals[1])),
        // `x.` — a schema, an alias, or a table name.
        1 => {
            let q = &quals[0];
            if let Some(schema) = src.schemas.iter().find(|s| eq(s, q)) {
                out.extend(
                    src.tables
                        .iter()
                        .filter(|(s, _)| eq(s, schema))
                        .map(|(s, t)| Candidate::new(t.clone(), Kind::Table, s.clone())),
                );
            }
            match refs
                .iter()
                .find(|r| r.alias.as_deref().is_some_and(|a| eq(a, q)) || eq(&r.table, q))
            {
                Some(r) => out.extend(column_candidates(src, r.schema.as_deref(), &r.table)),
                None => out.extend(column_candidates(src, None, q)),
            }
        }
        _ if expects_table(head) => {
            out.extend(table_candidates(src));
            out.extend(
                src.schemas
                    .iter()
                    .map(|s| Candidate::new(s.clone(), Kind::Schema, "schema")),
            );
        }
        _ => {
            for r in &refs {
                out.extend(column_candidates(src, r.schema.as_deref(), &r.table));
            }
            out.extend(table_candidates(src));
            out.extend(
                keywords(src.engine)
                    .iter()
                    .map(|k| Candidate::new(*k, Kind::Keyword, "")),
            );
        }
    }
    rank(out, &prefix)
}

fn rank(mut items: Vec<Candidate>, prefix: &str) -> Vec<Candidate> {
    let p = prefix.to_ascii_lowercase();
    items.retain(|c| c.text.to_ascii_lowercase().starts_with(&p));
    items.sort_by(|a, b| {
        a.kind
            .cmp(&b.kind)
            .then_with(|| {
                a.text
                    .to_ascii_lowercase()
                    .cmp(&b.text.to_ascii_lowercase())
            })
            .then_with(|| a.text.cmp(&b.text))
    });
    items.dedup_by(|a, b| a.text == b.text && a.kind == b.kind);
    items.truncate(MAX_ITEMS);
    // A single candidate that is what you already typed is not a completion.
    if items.len() == 1 && eq(&items[0].text, prefix) {
        items.clear();
    }
    items
}

const COMMON_KEYWORDS: &[&str] = &[
    "and",
    "as",
    "asc",
    "avg",
    "between",
    "by",
    "case",
    "cast",
    "coalesce",
    "count",
    "create table",
    "cross join",
    "delete from",
    "desc",
    "distinct",
    "drop table",
    "else",
    "end",
    "exists",
    "from",
    "full join",
    "group by",
    "having",
    "in",
    "inner join",
    "insert into",
    "is not null",
    "is null",
    "join",
    "left join",
    "like",
    "limit",
    "max",
    "min",
    "not",
    "null",
    "offset",
    "on",
    "or",
    "order by",
    "primary key",
    "right join",
    "select",
    "set",
    "sum",
    "then",
    "truncate table",
    "union",
    "union all",
    "update",
    "values",
    "when",
    "where",
    "with",
];

const PG_KEYWORDS: &[&str] = &[
    "array_agg",
    "boolean",
    "current_timestamp",
    "distinct on",
    "generate_series",
    "ilike",
    "jsonb",
    "lateral",
    "now()",
    "on conflict do nothing",
    "on conflict do update",
    "returning",
    "serial",
    "string_agg",
    "text",
    "timestamptz",
];

const MSSQL_KEYWORDS: &[&str] = &[
    "cross apply",
    "datetime2",
    "getdate()",
    "identity",
    "iif",
    "isnull",
    "len",
    "nvarchar",
    "outer apply",
    "row_number() over",
    "string_agg",
    "top",
    "try_convert",
    "with (nolock)",
];

fn keywords(engine: Engine) -> Vec<&'static str> {
    let extra = match engine {
        Engine::Postgres => PG_KEYWORDS,
        Engine::Mssql => MSSQL_KEYWORDS,
    };
    COMMON_KEYWORDS.iter().chain(extra).copied().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sources() -> Sources {
        let mut columns = HashMap::new();
        columns.insert(
            ("public".to_string(), "users".to_string()),
            vec!["id".into(), "email".into(), "created_at".into()],
        );
        columns.insert(
            ("sales".to_string(), "orders".to_string()),
            vec!["id".into(), "user_id".into(), "total".into()],
        );
        Sources {
            engine: Engine::Postgres,
            schemas: vec!["public".into(), "sales".into()],
            tables: vec![
                ("public".into(), "users".into()),
                ("sales".into(), "orders".into()),
            ],
            columns,
        }
    }

    /// Completes at the `|` in `sql`.
    fn at(sql: &str) -> Vec<Candidate> {
        let cursor = sql.find('|').expect("mark the cursor with |");
        candidates(&sql.replace('|', ""), cursor, &sources())
    }

    fn texts(items: &[Candidate]) -> Vec<&str> {
        items.iter().map(|c| c.text.as_str()).collect()
    }

    #[test]
    fn word_start_col_finds_the_identifier() {
        assert_eq!(word_start_col("select id fr", 12), 10);
        assert_eq!(word_start_col("select ", 7), 7);
        assert_eq!(word_start_col("a.b", 3), 2); // the dot is not part of the word
    }

    #[test]
    fn keywords_filter_by_prefix() {
        let items = at("sel|");
        assert!(texts(&items).contains(&"select"));
        assert!(items.iter().all(|c| c.text.starts_with("sel")));
    }

    #[test]
    fn engine_keywords_are_separate() {
        let mut src = sources();
        assert!(
            candidates("ilik", 4, &src)
                .iter()
                .any(|c| c.text == "ilike")
        );
        src.engine = Engine::Mssql;
        assert!(candidates("ilik", 4, &src).is_empty());
        assert!(candidates("to", 2, &src).iter().any(|c| c.text == "top"));
    }

    #[test]
    fn after_from_only_tables_and_schemas() {
        let items = at("select * from |");
        assert!(items.iter().all(|c| c.kind != Kind::Keyword));
        assert!(texts(&items).contains(&"users"));
        assert!(texts(&items).contains(&"sales.orders"));
        // The default schema needs no qualifier, so it is not offered twice.
        assert!(!texts(&items).contains(&"public.users"));
    }

    #[test]
    fn after_join_and_inside_a_table_list() {
        assert!(texts(&at("select * from users u, |")).contains(&"users"));
        assert!(texts(&at("select * from users u join |")).contains(&"users"));
    }

    #[test]
    fn select_list_offers_columns_of_the_tables_in_scope() {
        let items = at("select | from users");
        assert!(texts(&items).contains(&"email"));
        // Columns rank above tables and keywords.
        assert_eq!(items[0].kind, Kind::Column);
    }

    #[test]
    fn columns_come_from_a_from_clause_after_the_cursor() {
        assert_eq!(texts(&at("select ema| from users")), vec!["email"]);
    }

    #[test]
    fn alias_resolves_to_its_table() {
        let items = at("select u.| from users u");
        assert_eq!(texts(&items), vec!["created_at", "email", "id"]);
    }

    #[test]
    fn alias_with_as_and_a_qualified_table() {
        assert_eq!(
            texts(&at("select o.tot| from sales.orders as o")),
            vec!["total"]
        );
    }

    #[test]
    fn bare_table_name_qualifies_its_own_columns() {
        assert_eq!(texts(&at("select users.em| from users")), vec!["email"]);
    }

    #[test]
    fn schema_dot_lists_that_schemas_tables() {
        assert_eq!(texts(&at("select * from sales.|")), vec!["orders"]);
    }

    #[test]
    fn schema_table_dot_lists_columns() {
        assert_eq!(
            texts(&at("select public.users.|")),
            vec!["created_at", "email", "id"]
        );
    }

    #[test]
    fn quoted_and_bracketed_qualifiers_resolve() {
        assert_eq!(texts(&at("select \"users\".em|")), vec!["email"]);
        assert_eq!(texts(&at("select [users].em|")), vec!["email"]);
    }

    #[test]
    fn scope_is_the_statement_under_the_cursor() {
        let items = at("select * from sales.orders;\nselect | from users");
        assert!(texts(&items).contains(&"email"));
        assert!(!texts(&items).contains(&"total"));
    }

    #[test]
    fn a_word_that_is_already_complete_offers_nothing() {
        assert!(at("select * from users u where u.email|").is_empty());
    }

    #[test]
    fn unknown_alias_yields_nothing_rather_than_everything() {
        assert!(at("select x.|").is_empty());
    }

    #[test]
    fn strings_and_comments_do_not_introduce_tables() {
        let items = at("select 'from orders' , -- from orders\n  |");
        assert!(!texts(&items).contains(&"total"));
    }

    #[test]
    fn where_is_never_read_as_an_alias() {
        assert!(texts(&at("select * from users where |")).contains(&"email"));
    }

    #[test]
    fn nothing_completes_inside_a_string_or_comment() {
        assert!(at("select 'sel|").is_empty());
        assert!(at("select * -- fro|").is_empty());
        assert!(at("select /* fro| */").is_empty());
        // A closed literal or comment does not swallow what comes after it.
        assert!(!at("select 'a' || fro|").is_empty());
        assert!(!at("select /* x */ fro|").is_empty());
        assert!(!at("select 'it''s' , fro|").is_empty());
    }

    #[test]
    fn step_wraps_both_ways() {
        let mut c = Completion::new(
            vec![
                Candidate::new("a", Kind::Keyword, ""),
                Candidate::new("b", Kind::Keyword, ""),
            ],
            0,
            0,
        );
        c.step(-1);
        assert_eq!(c.selected, 1);
        c.step(1);
        assert_eq!(c.selected, 0);
    }
}
