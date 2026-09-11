//! The sidebar tree: project → connection → database → schema → table.

use crate::config::{Config, Engine};
use crate::db::ConnId;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    Project,
    Connection,
    Database,
    Schema,
    Table,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnState {
    Idle,
    Connecting,
    Connected,
    Failed,
}

#[derive(Debug, Clone)]
pub struct Node {
    pub kind: NodeKind,
    pub name: String,
    pub expanded: bool,
    pub loading: bool,
    pub loaded: bool,
    pub children: Vec<Node>,
    /// Set on connection, schema and table nodes.
    pub conn: Option<ConnId>,
    /// Set on schema and table nodes.
    pub schema: Option<String>,
    pub engine: Option<Engine>,
    pub state: ConnState,
}

impl Node {
    fn new(kind: NodeKind, name: impl Into<String>) -> Self {
        Self {
            kind,
            name: name.into(),
            expanded: false,
            loading: false,
            loaded: false,
            children: Vec::new(),
            conn: None,
            schema: None,
            engine: None,
            state: ConnState::Idle,
        }
    }

    pub fn has_children_slot(&self) -> bool {
        !matches!(self.kind, NodeKind::Table)
    }
}

/// One visible line, resolved from the tree for rendering and navigation.
#[derive(Debug, Clone)]
pub struct TreeRow {
    pub path: Vec<usize>,
    pub depth: usize,
    pub kind: NodeKind,
    pub name: String,
    pub expanded: bool,
    pub loading: bool,
    pub has_children_slot: bool,
    pub conn: Option<ConnId>,
    pub schema: Option<String>,
    pub engine: Option<Engine>,
    pub state: ConnState,
}

#[derive(Debug, Default)]
pub struct Tree {
    pub roots: Vec<Node>,
    pub cursor: usize,
    pub offset: usize,
    pub filter: String,
}

impl Tree {
    pub fn from_config(cfg: &Config) -> Self {
        let mut roots = Vec::new();
        let mut id: ConnId = 0;
        for p in &cfg.projects {
            let mut proj = Node::new(NodeKind::Project, &p.name);
            proj.expanded = true;
            proj.loaded = true;
            for c in &p.connections {
                let mut node = Node::new(NodeKind::Connection, &c.name);
                node.conn = Some(id);
                node.engine = Some(c.engine);
                proj.children.push(node);
                id += 1;
            }
            roots.push(proj);
        }
        Self {
            roots,
            cursor: 0,
            offset: 0,
            filter: String::new(),
        }
    }

    fn matches(&self, name: &str) -> bool {
        self.filter.is_empty()
            || name
                .to_ascii_lowercase()
                .contains(&self.filter.to_ascii_lowercase())
    }

    /// True when this node or any descendant matches the active filter.
    fn subtree_matches(&self, node: &Node) -> bool {
        if self.matches(&node.name) {
            return true;
        }
        node.children.iter().any(|c| self.subtree_matches(c))
    }

    pub fn rows(&self) -> Vec<TreeRow> {
        let mut out = Vec::new();
        for (i, node) in self.roots.iter().enumerate() {
            self.walk(node, vec![i], 0, &mut out);
        }
        out
    }

    fn walk(&self, node: &Node, path: Vec<usize>, depth: usize, out: &mut Vec<TreeRow>) {
        if !self.filter.is_empty() && !self.subtree_matches(node) {
            return;
        }
        // With a filter active, containers open up so hits are reachable.
        let expanded = node.expanded || (!self.filter.is_empty() && !node.children.is_empty());
        out.push(TreeRow {
            path: path.clone(),
            depth,
            kind: node.kind,
            name: node.name.clone(),
            expanded,
            loading: node.loading,
            has_children_slot: node.has_children_slot(),
            conn: node.conn,
            schema: node.schema.clone(),
            engine: node.engine,
            state: node.state,
        });
        if expanded {
            for (i, child) in node.children.iter().enumerate() {
                let mut p = path.clone();
                p.push(i);
                self.walk(child, p, depth + 1, out);
            }
        }
    }

    pub fn node(&self, path: &[usize]) -> Option<&Node> {
        let mut cur = self.roots.get(*path.first()?)?;
        for i in &path[1..] {
            cur = cur.children.get(*i)?;
        }
        Some(cur)
    }

    pub fn node_mut(&mut self, path: &[usize]) -> Option<&mut Node> {
        let mut cur = self.roots.get_mut(*path.first()?)?;
        for i in &path[1..] {
            cur = cur.children.get_mut(*i)?;
        }
        Some(cur)
    }

    pub fn selected_row(&self) -> Option<TreeRow> {
        self.rows().into_iter().nth(self.cursor)
    }

    pub fn clamp(&mut self) {
        let len = self.rows().len();
        if len == 0 {
            self.cursor = 0;
        } else if self.cursor >= len {
            self.cursor = len - 1;
        }
    }

    pub fn move_cursor(&mut self, delta: isize) {
        let len = self.rows().len() as isize;
        if len == 0 {
            return;
        }
        let next = (self.cursor as isize + delta).clamp(0, len - 1);
        self.cursor = next as usize;
    }

    /// Jumps to the next (or previous) connection node.
    pub fn jump_connection(&mut self, forward: bool) {
        let rows = self.rows();
        let idxs: Vec<usize> = rows
            .iter()
            .enumerate()
            .filter(|(_, r)| r.kind == NodeKind::Connection)
            .map(|(i, _)| i)
            .collect();
        if idxs.is_empty() {
            return;
        }
        let target = if forward {
            idxs.iter().find(|i| **i > self.cursor).copied()
        } else {
            idxs.iter().rev().find(|i| **i < self.cursor).copied()
        };
        self.cursor = target.unwrap_or(if forward {
            idxs[0]
        } else {
            *idxs.last().unwrap()
        });
    }

    /// Finds the path to the connection node for `id`. A database opened on
    /// the side is not a configured connection, so it has none.
    pub fn connection_path(&self, id: ConnId) -> Option<Vec<usize>> {
        for (pi, p) in self.roots.iter().enumerate() {
            for (ci, c) in p.children.iter().enumerate() {
                if c.kind == NodeKind::Connection && c.conn == Some(id) {
                    return Some(vec![pi, ci]);
                }
            }
        }
        None
    }

    pub fn connection_mut(&mut self, id: ConnId) -> Option<&mut Node> {
        let path = self.connection_path(id)?;
        self.node_mut(&path)
    }

    /// Every connection and database node that speaks through `id`. A
    /// connection and its configured database share one id, so this can
    /// return both.
    fn owner_paths(&self, id: ConnId) -> Vec<Vec<usize>> {
        let mut out = Vec::new();
        for (pi, p) in self.roots.iter().enumerate() {
            for (ci, c) in p.children.iter().enumerate() {
                if c.conn == Some(id) {
                    out.push(vec![pi, ci]);
                }
                for (di, d) in c.children.iter().enumerate() {
                    if d.kind == NodeKind::Database && d.conn == Some(id) {
                        out.push(vec![pi, ci, di]);
                    }
                }
            }
        }
        out
    }

    /// The node schemas belong under: the database node when there is one,
    /// the connection itself before its databases are listed.
    fn owner_path(&self, id: ConnId) -> Option<Vec<usize>> {
        self.owner_paths(id).into_iter().max_by_key(Vec::len)
    }

    fn owner_mut(&mut self, id: ConnId) -> Option<&mut Node> {
        let path = self.owner_path(id)?;
        self.node_mut(&path)
    }

    /// Replaces a connection's databases, keeping the schemas already loaded
    /// under the ones that survive.
    pub fn set_databases(&mut self, id: ConnId, databases: Vec<(String, ConnId)>) {
        if let Some(node) = self.connection_mut(id) {
            let existing: Vec<Node> = std::mem::take(&mut node.children);
            node.children = databases
                .into_iter()
                .map(|(name, conn)| {
                    if let Some(old) = existing.iter().find(|c| c.name == name) {
                        return old.clone();
                    }
                    let mut n = Node::new(NodeKind::Database, &name);
                    n.conn = Some(conn);
                    n
                })
                .collect();
            node.loading = false;
            node.loaded = true;
            node.expanded = true;
            node.state = ConnState::Connected;
        }
    }

    /// True when schemas have been loaded for a connection or one of its
    /// databases.
    pub fn is_loaded(&self, id: ConnId) -> bool {
        self.owner_path(id)
            .and_then(|p| self.node(&p))
            .is_some_and(|n| n.loaded)
    }

    /// Database names already listed under a connection.
    pub fn databases(&self, id: ConnId) -> Vec<String> {
        self.connection_path(id)
            .and_then(|p| self.node(&p))
            .map(|n| {
                n.children
                    .iter()
                    .filter(|c| c.kind == NodeKind::Database)
                    .map(|c| c.name.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn set_schemas(&mut self, id: ConnId, schemas: Vec<String>) {
        if let Some(node) = self.owner_mut(id) {
            let existing: Vec<Node> = std::mem::take(&mut node.children);
            node.children = schemas
                .into_iter()
                .map(|s| {
                    // Keep already-loaded tables when refreshing.
                    if let Some(old) = existing.iter().find(|c| c.name == s) {
                        old.clone()
                    } else {
                        let mut n = Node::new(NodeKind::Schema, &s);
                        n.conn = Some(id);
                        n.schema = Some(s.clone());
                        n
                    }
                })
                .collect();
            node.loading = false;
            node.loaded = true;
            node.expanded = true;
            node.state = ConnState::Connected;
        }
    }

    pub fn set_tables(&mut self, id: ConnId, schema: &str, tables: Vec<String>) {
        if let Some(node) = self.owner_mut(id)
            && let Some(sn) = node.children.iter_mut().find(|c| c.name == schema)
        {
            sn.children = tables
                .into_iter()
                .map(|t| {
                    let mut n = Node::new(NodeKind::Table, t);
                    n.conn = Some(id);
                    n.schema = Some(schema.to_string());
                    n
                })
                .collect();
            sn.loading = false;
            sn.loaded = true;
            sn.expanded = true;
        }
    }

    // --- structural edits driven by the connection form ---

    /// Schemas and `(schema, table)` pairs already loaded for a connection.
    pub fn identifiers(&self, id: ConnId) -> (Vec<String>, Vec<(String, String)>) {
        let mut schemas = Vec::new();
        let mut tables = Vec::new();
        if let Some(owner) = self.owner_path(id).and_then(|p| self.node(&p)) {
            for s in &owner.children {
                if s.kind != NodeKind::Schema {
                    continue;
                }
                schemas.push(s.name.clone());
                for t in &s.children {
                    if t.kind == NodeKind::Table {
                        tables.push((s.name.clone(), t.name.clone()));
                    }
                }
            }
        }
        (schemas, tables)
    }

    pub fn project_index(&self, name: &str) -> Option<usize> {
        self.roots.iter().position(|p| p.name == name)
    }

    /// Returns the index of the project, creating it if it is new.
    pub fn ensure_project(&mut self, name: &str) -> usize {
        if let Some(i) = self.project_index(name) {
            return i;
        }
        let mut p = Node::new(NodeKind::Project, name);
        p.expanded = true;
        p.loaded = true;
        self.roots.push(p);
        self.roots.len() - 1
    }

    #[cfg(test)]
    pub fn project_of(&self, id: ConnId) -> Option<String> {
        let path = self.connection_path(id)?;
        self.roots.get(path[0]).map(|p| p.name.clone())
    }

    /// The project a row belongs to, whatever depth it sits at.
    pub fn project_of_row(&self, row: &TreeRow) -> Option<String> {
        self.roots.get(*row.path.first()?).map(|p| p.name.clone())
    }

    pub fn add_connection(&mut self, project: &str, name: &str, engine: Engine, id: ConnId) {
        let pi = self.ensure_project(project);
        let mut n = Node::new(NodeKind::Connection, name);
        n.conn = Some(id);
        n.engine = Some(engine);
        self.roots[pi].children.push(n);
        self.roots[pi].expanded = true;
    }

    /// Renames a connection, and moves it between projects when needed.
    /// Loaded schemas and tables come along.
    pub fn update_connection(&mut self, id: ConnId, project: &str, name: &str, engine: Engine) {
        let Some(path) = self.connection_path(id) else {
            return;
        };
        let current_project = self.roots[path[0]].name.clone();
        if current_project == project {
            if let Some(n) = self.node_mut(&path) {
                n.name = name.to_string();
                n.engine = Some(engine);
            }
            return;
        }
        let mut node = self.roots[path[0]].children.remove(path[1]);
        node.name = name.to_string();
        node.engine = Some(engine);
        let pi = self.ensure_project(project);
        self.roots[pi].children.push(node);
        self.roots[pi].expanded = true;
        self.clamp();
    }

    /// Drops loaded schemas so a re-pointed connection does not show stale
    /// objects from the old server.
    pub fn reset_connection_children(&mut self, id: ConnId) {
        if let Some(n) = self.connection_mut(id) {
            n.children.clear();
            n.loaded = false;
            n.expanded = false;
            n.state = ConnState::Idle;
        }
    }

    pub fn remove_connection(&mut self, id: ConnId) {
        if let Some(path) = self.connection_path(id) {
            self.roots[path[0]].children.remove(path[1]);
            self.clamp();
        }
    }

    pub fn rename_project(&mut self, old: &str, new: &str) {
        if let Some(i) = self.project_index(old) {
            self.roots[i].name = new.to_string();
        }
    }

    /// Removes a project and reports the connection ids that went with it.
    pub fn remove_project(&mut self, name: &str) -> Vec<ConnId> {
        let Some(i) = self.project_index(name) else {
            return Vec::new();
        };
        let removed = self.roots.remove(i);
        self.clamp();
        removed.children.iter().filter_map(|c| c.conn).collect()
    }

    /// Puts the cursor on a connection, expanding its project first.
    pub fn focus_connection(&mut self, id: ConnId) {
        if let Some(path) = self.connection_path(id) {
            self.roots[path[0]].expanded = true;
            if let Some(i) = self.rows().iter().position(|r| r.path == path) {
                self.cursor = i;
            }
        }
    }

    pub fn set_state(&mut self, id: ConnId, state: ConnState) {
        for path in self.owner_paths(id) {
            if let Some(node) = self.node_mut(&path) {
                node.state = state;
                if state != ConnState::Connecting {
                    node.loading = false;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn cfg() -> Config {
        Config::parse(
            r#"
[[project]]
name = "Alpha"
  [[project.connection]]
  name = "a-db"
  engine = "postgres"
  host = "h"
  database = "d"
  user = "u"
  [[project.connection]]
  name = "a-warehouse"
  engine = "mssql"
  host = "h"
  database = "d"
  user = "u"
[[project]]
name = "Beta"
  [[project.connection]]
  name = "b-db"
  engine = "postgres"
  host = "h"
  database = "d"
  user = "u"
"#,
        )
        .unwrap()
    }

    #[test]
    fn builds_grouped_tree_with_stable_ids() {
        let t = Tree::from_config(&cfg());
        let rows = t.rows();
        let names: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["Alpha", "a-db", "a-warehouse", "Beta", "b-db"]);
        assert_eq!(rows[1].conn, Some(0));
        assert_eq!(rows[2].conn, Some(1));
        assert_eq!(rows[4].conn, Some(2));
        assert_eq!(rows[1].depth, 1);
    }

    #[test]
    fn schemas_and_tables_nest_under_connection() {
        let mut t = Tree::from_config(&cfg());
        t.set_schemas(0, vec!["public".into(), "audit".into()]);
        t.set_tables(0, "public", vec!["users".into()]);
        let rows = t.rows();
        let names: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "Alpha",
                "a-db",
                "public",
                "users",
                "audit",
                "a-warehouse",
                "Beta",
                "b-db"
            ]
        );
    }

    #[test]
    fn refreshing_schemas_keeps_loaded_tables() {
        let mut t = Tree::from_config(&cfg());
        t.set_schemas(0, vec!["public".into()]);
        t.set_tables(0, "public", vec!["users".into()]);
        t.set_schemas(0, vec!["public".into(), "new".into()]);
        let rows = t.rows();
        let names: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
        assert!(names.contains(&"users"));
        assert!(names.contains(&"new"));
    }

    #[test]
    fn filter_reveals_matches_without_manual_expansion() {
        let mut t = Tree::from_config(&cfg());
        t.set_schemas(0, vec!["public".into()]);
        t.set_tables(0, "public", vec!["users".into(), "orders".into()]);
        t.node_mut(&[0, 0]).unwrap().expanded = false;
        t.filter = "ORD".into();
        let rows = t.rows();
        let names: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["Alpha", "a-db", "public", "orders"]);
    }

    #[test]
    fn cursor_stays_in_bounds() {
        let mut t = Tree::from_config(&cfg());
        t.move_cursor(100);
        assert_eq!(t.cursor, 4);
        t.move_cursor(-100);
        assert_eq!(t.cursor, 0);
        t.cursor = 4;
        t.filter = "nothing-matches".into();
        t.clamp();
        assert_eq!(t.cursor, 0);
    }

    #[test]
    fn jump_connection_cycles_and_wraps() {
        let mut t = Tree::from_config(&cfg());
        t.jump_connection(true);
        assert_eq!(t.selected_row().unwrap().name, "a-db");
        t.jump_connection(true);
        assert_eq!(t.selected_row().unwrap().name, "a-warehouse");
        t.jump_connection(true);
        assert_eq!(t.selected_row().unwrap().name, "b-db");
        t.jump_connection(true);
        assert_eq!(t.selected_row().unwrap().name, "a-db");
        t.jump_connection(false);
        assert_eq!(t.selected_row().unwrap().name, "b-db");
    }

    #[test]
    fn add_connection_creates_a_missing_project() {
        let mut t = Tree::from_config(&cfg());
        t.add_connection("Gamma", "g-db", Engine::Postgres, 3);
        let rows = t.rows();
        let names: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names.last(), Some(&"g-db"));
        assert_eq!(t.project_of(3).as_deref(), Some("Gamma"));
    }

    #[test]
    fn renaming_in_place_keeps_loaded_children() {
        let mut t = Tree::from_config(&cfg());
        t.set_schemas(0, vec!["public".into()]);
        t.set_tables(0, "public", vec!["users".into()]);
        t.update_connection(0, "Alpha", "a-db-renamed", Engine::Mssql);
        let rows = t.rows();
        let names: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
        assert!(names.contains(&"a-db-renamed"));
        assert!(
            names.contains(&"users"),
            "loaded tables must survive a rename"
        );
        assert_eq!(
            t.node(&t.connection_path(0).unwrap()).unwrap().engine,
            Some(Engine::Mssql)
        );
    }

    #[test]
    fn moving_a_connection_between_projects_carries_its_children() {
        let mut t = Tree::from_config(&cfg());
        t.set_schemas(0, vec!["public".into()]);
        t.update_connection(0, "Beta", "a-db", Engine::Postgres);
        assert_eq!(t.project_of(0).as_deref(), Some("Beta"));
        let rows = t.rows();
        let names: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
        assert!(names.contains(&"public"));
        // Alpha keeps its other connection.
        assert_eq!(t.roots[0].children.len(), 1);
    }

    #[test]
    fn resetting_children_clears_stale_objects() {
        let mut t = Tree::from_config(&cfg());
        t.set_schemas(0, vec!["public".into()]);
        t.reset_connection_children(0);
        let path = t.connection_path(0).unwrap();
        let n = t.node(&path).unwrap();
        assert!(n.children.is_empty());
        assert!(!n.loaded);
    }

    #[test]
    fn removing_a_project_reports_its_connections() {
        let mut t = Tree::from_config(&cfg());
        let gone = t.remove_project("Alpha");
        assert_eq!(gone, vec![0, 1]);
        assert_eq!(t.roots.len(), 1);
        assert!(t.connection_path(0).is_none());
    }

    #[test]
    fn removing_a_connection_leaves_the_rest_addressable() {
        let mut t = Tree::from_config(&cfg());
        t.remove_connection(0);
        assert!(t.connection_path(0).is_none());
        assert_eq!(t.connection_path(1), Some(vec![0, 0]));
        assert_eq!(t.connection_path(2), Some(vec![1, 0]));
    }

    #[test]
    fn focus_connection_moves_the_cursor_onto_it() {
        let mut t = Tree::from_config(&cfg());
        t.focus_connection(2);
        assert_eq!(t.selected_row().unwrap().name, "b-db");
    }

    #[test]
    fn connection_path_resolves_ids() {
        let t = Tree::from_config(&cfg());
        assert_eq!(t.connection_path(2), Some(vec![1, 0]));
        assert_eq!(t.connection_path(9), None);
    }

    #[test]
    fn databases_nest_between_connection_and_schema() {
        let mut t = Tree::from_config(&cfg());
        // Connection 0 is configured for `d`; `other` is opened beside it.
        t.set_databases(0, vec![("d".into(), 0), ("other".into(), 9)]);
        assert_eq!(t.databases(0), vec!["d".to_string(), "other".to_string()]);
        t.set_schemas(0, vec!["public".into()]);
        t.set_schemas(9, vec!["audit".into()]);
        t.set_tables(9, "audit", vec!["events".into()]);
        let rows = t.rows();
        let names: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "Alpha",
                "a-db",
                "d",
                "public",
                "other",
                "audit",
                "events",
                "a-warehouse",
                "Beta",
                "b-db"
            ]
        );
        // The table row speaks through the database's own connection.
        let events = t.rows().into_iter().find(|r| r.name == "events").unwrap();
        assert_eq!(events.conn, Some(9));
        assert_eq!(events.schema.as_deref(), Some("audit"));
    }

    #[test]
    fn identifiers_come_from_the_database_a_connection_sits_on() {
        let mut t = Tree::from_config(&cfg());
        t.set_databases(0, vec![("d".into(), 0), ("other".into(), 9)]);
        t.set_schemas(0, vec!["public".into()]);
        t.set_tables(0, "public", vec!["users".into()]);
        t.set_schemas(9, vec!["audit".into()]);
        t.set_tables(9, "audit", vec!["events".into()]);
        assert_eq!(
            t.identifiers(0),
            (
                vec!["public".to_string()],
                vec![("public".to_string(), "users".to_string())]
            )
        );
        assert_eq!(
            t.identifiers(9),
            (
                vec!["audit".to_string()],
                vec![("audit".to_string(), "events".to_string())]
            )
        );
    }

    #[test]
    fn relisting_databases_keeps_loaded_schemas() {
        let mut t = Tree::from_config(&cfg());
        t.set_databases(0, vec![("d".into(), 0)]);
        t.set_schemas(0, vec!["public".into()]);
        t.set_databases(0, vec![("d".into(), 0), ("new".into(), 9)]);
        assert_eq!(t.databases(0), vec!["d".to_string(), "new".to_string()]);
        assert_eq!(t.identifiers(0).0, vec!["public".to_string()]);
    }

    #[test]
    fn a_database_is_not_a_configured_connection() {
        let mut t = Tree::from_config(&cfg());
        t.set_databases(0, vec![("d".into(), 0), ("other".into(), 9)]);
        assert_eq!(t.connection_path(0), Some(vec![0, 0]));
        assert_eq!(t.connection_path(9), None, "clones are not in the config");
        // State reaches both the connection and the database it sits on.
        t.set_state(0, ConnState::Failed);
        let rows = t.rows();
        assert_eq!(rows[1].state, ConnState::Failed);
        assert_eq!(rows[2].state, ConnState::Failed);
    }
}
