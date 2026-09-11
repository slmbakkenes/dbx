//! Application state, focus model and action dispatch.

use crate::complete::{self, Completion, Sources};
use crate::config::{Config, Connection as ConnCfg, UiConfig};
use crate::db::worker::{ConnHandle, DbEvent, DbPayload, DbRequest, spawn};
use crate::db::{ConnId, Grid, ReqId, is_read_only, statement_at};
use crate::editor::{Editor, Mode};
use crate::form::{FieldKind, Form, FormMode, TestState};
use crate::grid::GridView;
use crate::keys::{Action, Context, KeySpec, Keymap, Lookup, load_keymap};
use crate::tree::{ConnState, NodeKind, Tree};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use std::collections::HashMap;
use std::path::PathBuf;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Tree,
    Editor,
    Grid,
}

/// Modal layers. Escape always dismisses the topmost one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Overlay {
    None,
    Help,
    Inspect(String),
    /// Text prompt: tree filter, grid search, or `:` command line.
    Prompt(PromptKind),
    Confirm(Confirm),
    Form(Box<Form>),
    /// A modal list to choose from: databases on a server.
    Picker(Box<Picker>),
}

/// The database switcher for one query tab.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Picker {
    pub title: String,
    /// The configured connection whose server is being browsed.
    pub base: ConnId,
    /// The tab that will be retargeted.
    pub tab: usize,
    pub items: Vec<String>,
    pub filter: String,
    pub selected: usize,
    /// Waiting on the server's database list.
    pub loading: bool,
}

impl Picker {
    pub fn visible(&self) -> Vec<&str> {
        let f = self.filter.to_ascii_lowercase();
        self.items
            .iter()
            .filter(|i| i.to_ascii_lowercase().contains(&f))
            .map(String::as_str)
            .collect()
    }

    pub fn step(&mut self, delta: isize) {
        let n = self.visible().len() as isize;
        if n == 0 {
            self.selected = 0;
            return;
        }
        self.selected = (((self.selected as isize + delta) % n + n) % n) as usize;
    }

    pub fn current(&self) -> Option<String> {
        self.visible().get(self.selected).map(|s| s.to_string())
    }
}

/// Something irreversible, waiting on a yes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Confirm {
    RunSql(String),
    DeleteConnection {
        id: ConnId,
        project: String,
        name: String,
        /// Keyring account that will be deleted with it, if dbx owns one.
        account: Option<String>,
    },
    DeleteProject {
        name: String,
        connections: Vec<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptKind {
    Filter,
    Search,
    Command,
}

impl PromptKind {
    pub fn sigil(self) -> &'static str {
        match self {
            PromptKind::Filter => "filter /",
            PromptKind::Search => "/",
            PromptKind::Command => ":",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MsgKind {
    Info,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TabKind {
    Data { schema: String, table: String },
    Structure { schema: String, table: String },
    Query,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TabStatus {
    Idle,
    Running(ReqId),
    Error(String),
}

/// A configured connection plus the project it belongs to.
#[derive(Debug, Clone)]
pub struct ConnEntry {
    pub project: String,
    pub cfg: ConnCfg,
    /// Set when this is a session-only clone pointed at another database on
    /// the same server. Such entries are never written back to the config.
    pub origin: Option<ConnId>,
}

/// Result of a "test connection", matched back by token so a stale reply is
/// ignored.
#[derive(Debug)]
pub struct TestOutcome {
    pub token: u64,
    pub result: Result<String, String>,
    pub hints: Vec<String>,
}

pub struct Tab {
    pub conn: ConnId,
    pub conn_name: String,
    pub title: String,
    pub kind: TabKind,
    pub view: GridView,
    pub editor: Editor,
    pub last_sql: String,
    pub status: TabStatus,
    /// Row offset of the current page, for Data tabs.
    pub offset: usize,
    /// Open completion popup, for Query tabs.
    pub completion: Option<Completion>,
}

impl Tab {
    pub fn has_editor(&self) -> bool {
        self.kind == TabKind::Query
    }
}

pub struct App {
    pub ui: UiConfig,
    pub config_path: PathBuf,
    /// Indexed by `ConnId`. Deleted slots become `None` so ids stay stable for
    /// open tabs and running workers.
    pub conns: Vec<Option<ConnEntry>>,
    handles: HashMap<ConnId, ConnHandle>,
    pub events_tx: UnboundedSender<DbEvent>,
    pub events_rx: UnboundedReceiver<DbEvent>,
    pub tree: Tree,
    /// Columns learned from opening a table, keyed by connection and table.
    /// Only ever read by the completer.
    columns: HashMap<(ConnId, String, String), Vec<String>>,
    pub tabs: Vec<Tab>,
    pub tab: usize,
    focus: Vec<Focus>,
    pub overlay: Overlay,
    pub input: String,
    pub pending: Vec<KeySpec>,
    pub keymap: Keymap,
    pub message: Option<(MsgKind, String)>,
    pub quit: bool,
    pub fullscreen: bool,
    pub spinner: usize,
    next_req: ReqId,
    /// Which tab is waiting on which request.
    inflight: HashMap<ReqId, usize>,
    /// Screen position of the completion popup's top-left, refreshed by the
    /// renderer each frame.
    pub completion_anchor: Option<(u16, u16)>,
    /// Viewport heights, refreshed by the renderer each frame.
    pub tree_height: usize,
    pub grid_height: usize,
    pub editor_height: usize,
    pub history: Vec<String>,
    history_pos: Option<usize>,
    pub help_scroll: usize,
    test_tx: UnboundedSender<TestOutcome>,
    pub test_rx: UnboundedReceiver<TestOutcome>,
    test_token: u64,
}

impl App {
    pub fn new(cfg: Config) -> Self {
        Self::with_path(cfg, crate::config::config_path().unwrap_or_default())
    }

    pub fn with_path(cfg: Config, config_path: PathBuf) -> Self {
        let (events_tx, events_rx) = unbounded_channel();
        let (test_tx, test_rx) = unbounded_channel();
        let mut conns = Vec::new();
        for p in &cfg.projects {
            for c in &p.connections {
                conns.push(Some(ConnEntry {
                    project: p.name.clone(),
                    cfg: c.clone(),
                    origin: None,
                }));
            }
        }
        let tree = Tree::from_config(&cfg);
        let (keymap, key_errors) = load_keymap();
        let message = if key_errors.is_empty() {
            None
        } else {
            Some((
                MsgKind::Error,
                format!("keys.toml: {}", key_errors.join("; ")),
            ))
        };
        Self {
            ui: cfg.ui,
            config_path,
            conns,
            handles: HashMap::new(),
            events_tx,
            events_rx,
            tree,
            columns: HashMap::new(),
            tabs: Vec::new(),
            tab: 0,
            focus: vec![Focus::Tree],
            overlay: Overlay::None,
            input: String::new(),
            pending: Vec::new(),
            keymap,
            message,
            quit: false,
            fullscreen: false,
            spinner: 0,
            next_req: 1,
            inflight: HashMap::new(),
            completion_anchor: None,
            tree_height: 20,
            grid_height: 20,
            editor_height: 8,
            history: Vec::new(),
            history_pos: None,
            help_scroll: 0,
            test_tx,
            test_rx,
            test_token: 0,
        }
    }

    // --- focus ---

    pub fn focus(&self) -> Focus {
        *self.focus.last().unwrap_or(&Focus::Tree)
    }

    fn push_focus(&mut self, f: Focus) {
        if self.focus() != f {
            self.focus.push(f);
        }
    }

    /// Returns false when already at the base pane, which is never a dead end.
    fn pop_focus(&mut self) -> bool {
        if self.focus.len() > 1 {
            self.focus.pop();
            true
        } else {
            false
        }
    }

    fn reset_focus(&mut self, f: Focus) {
        self.focus.clear();
        self.focus.push(f);
    }

    pub fn context(&self) -> Context {
        match self.focus() {
            Focus::Tree => Context::Tree,
            Focus::Grid => Context::Grid,
            Focus::Editor => Context::Editor,
        }
    }

    pub fn active_tab(&self) -> Option<&Tab> {
        self.tabs.get(self.tab)
    }

    pub fn active_tab_mut(&mut self) -> Option<&mut Tab> {
        self.tabs.get_mut(self.tab)
    }

    fn info(&mut self, msg: impl Into<String>) {
        self.message = Some((MsgKind::Info, msg.into()));
    }

    fn error(&mut self, msg: impl Into<String>) {
        self.message = Some((MsgKind::Error, msg.into()));
    }

    // --- connections ---

    fn handle(&mut self, id: ConnId) -> Option<&ConnHandle> {
        if !self.handles.contains_key(&id) {
            let cfg = self.conn(id)?.cfg.clone();
            let h = spawn(id, cfg, self.events_tx.clone());
            self.handles.insert(id, h);
        }
        self.handles.get(&id)
    }

    fn request(&mut self, id: ConnId, req: DbRequest) {
        if let Some(h) = self.handle(id) {
            h.send(req);
        } else {
            self.error(format!("no such connection: {id}"));
        }
    }

    fn new_req(&mut self) -> ReqId {
        let r = self.next_req;
        self.next_req += 1;
        r
    }

    pub fn shutdown(&mut self) {
        for h in self.handles.values() {
            h.send(DbRequest::Shutdown);
        }
    }

    // --- key handling ---

    pub fn on_key(&mut self, ev: KeyEvent) {
        if ev.kind == KeyEventKind::Release {
            return;
        }
        self.message = None;

        // 1. Text prompts consume everything.
        if let Overlay::Prompt(kind) = self.overlay {
            self.prompt_key(kind, ev);
            return;
        }
        // 2a. The database picker is modal and filters as you type.
        if matches!(self.overlay, Overlay::Picker(_)) {
            self.picker_key(ev);
            return;
        }
        // 2. The connection form is modal and edits text itself.
        if matches!(self.overlay, Overlay::Form(_)) {
            self.form_key(ev);
            return;
        }
        // 3. Confirmation dialog.
        if let Overlay::Confirm(what) = self.overlay.clone() {
            match ev.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    self.overlay = Overlay::None;
                    self.apply_confirmed(what);
                }
                _ => {
                    self.overlay = Overlay::None;
                    self.info("cancelled");
                }
            }
            return;
        }
        // 4. Dismissable overlays.
        if matches!(self.overlay, Overlay::Help | Overlay::Inspect(_)) {
            match KeySpec::from_event(&ev).code {
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter => {
                    self.overlay = Overlay::None;
                    self.help_scroll = 0;
                }
                KeyCode::Char('y') => {
                    if let Overlay::Inspect(text) = self.overlay.clone() {
                        self.copy(&text, "cell");
                    }
                }
                KeyCode::Char('j') | KeyCode::Down => self.help_scroll += 1,
                KeyCode::Char('k') | KeyCode::Up => {
                    self.help_scroll = self.help_scroll.saturating_sub(1)
                }
                KeyCode::Char('d') | KeyCode::PageDown => self.help_scroll += 10,
                KeyCode::Char('u') | KeyCode::PageUp => {
                    self.help_scroll = self.help_scroll.saturating_sub(10)
                }
                KeyCode::Char('g') => self.help_scroll = 0,
                _ => {}
            }
            return;
        }
        // 5. Insert mode in the query editor takes raw input, except Escape.
        if self.focus() == Focus::Editor
            && self
                .active_tab()
                .map(|t| t.editor.mode == Mode::Insert)
                .unwrap_or(false)
            && self.editor_insert_key(ev)
        {
            return;
        }

        // 6. Keymap.
        let key = KeySpec::from_event(&ev);
        match self.keymap.lookup(self.context(), &self.pending, key) {
            Lookup::Run(action) => {
                self.pending.clear();
                self.dispatch(action);
            }
            Lookup::Pending => self.pending.push(key),
            Lookup::None => {
                self.pending.clear();
            }
        }
    }

    /// Returns true when the key was consumed by insert mode.
    fn editor_insert_key(&mut self, ev: KeyEvent) -> bool {
        let ctrl = ev.modifiers.contains(KeyModifiers::CONTROL);
        // Let global chords through so no pane can trap the cursor.
        if ctrl {
            return false;
        }
        if self.active_tab().is_some_and(|t| t.completion.is_some()) && self.completion_key(ev) {
            return true;
        }
        // Typed characters can open the popup, so they are handled before the
        // borrow the rest of the arms take.
        if let KeyCode::Char(c) = ev.code {
            let Some(tab) = self.active_tab_mut() else {
                return false;
            };
            tab.editor.insert_char(c);
            self.auto_complete(c);
            return true;
        }
        let Some(tab) = self.active_tab_mut() else {
            return false;
        };
        match ev.code {
            KeyCode::Esc => {
                tab.editor.enter_normal();
                true
            }
            KeyCode::Enter => {
                tab.editor.newline();
                true
            }
            KeyCode::Backspace => {
                tab.editor.backspace();
                true
            }
            KeyCode::Tab => {
                for _ in 0..2 {
                    tab.editor.insert_char(' ');
                }
                true
            }
            KeyCode::Left => {
                tab.editor.move_col(-1);
                true
            }
            KeyCode::Right => {
                tab.editor.move_col(1);
                true
            }
            KeyCode::Up => {
                tab.editor.move_line(-1);
                true
            }
            KeyCode::Down => {
                tab.editor.move_line(1);
                true
            }
            _ => false,
        }
    }

    // --- databases ---

    /// The configured connection behind an id, which may be a session-only
    /// clone pointed at another database.
    pub fn base_conn(&self, id: ConnId) -> ConnId {
        self.conn(id).and_then(|e| e.origin).unwrap_or(id)
    }

    /// The database an id talks to.
    pub fn database_of(&self, id: ConnId) -> Option<&str> {
        self.conn(id).map(|e| e.cfg.database.as_str())
    }

    /// Session-only clones of a configured connection.
    fn derived_of(&self, base: ConnId) -> Vec<ConnId> {
        self.conns
            .iter()
            .enumerate()
            .filter(|(_, c)| c.as_ref().is_some_and(|e| e.origin == Some(base)))
            .map(|(i, _)| i)
            .collect()
    }

    /// Drops the clones of a connection, and anything pointing at them.
    fn forget_derived(&mut self, base: ConnId) {
        for id in self.derived_of(base) {
            self.drop_connection(id);
            self.tabs.retain(|t| t.conn != id);
            if let Some(slot) = self.conns.get_mut(id) {
                *slot = None;
            }
        }
        if self.tab >= self.tabs.len() {
            self.tab = self.tabs.len().saturating_sub(1);
        }
    }

    /// The id that talks to `database` on `base`'s server, cloning the
    /// configured connection the first time one is asked for. The clone lives
    /// for this session only and is never written to the config.
    fn conn_for_database(&mut self, base: ConnId, database: &str) -> Option<ConnId> {
        let base = self.base_conn(base);
        let entry = self.conn(base)?;
        if entry.cfg.database == database {
            return Some(base);
        }
        if let Some(id) = self
            .derived_of(base)
            .into_iter()
            .find(|id| self.conn(*id).is_some_and(|e| e.cfg.database == database))
        {
            return Some(id);
        }
        let mut cfg = entry.cfg.clone();
        cfg.database = database.to_string();
        let project = entry.project.clone();
        self.conns.push(Some(ConnEntry {
            project,
            cfg,
            origin: Some(base),
        }));
        Some(self.conns.len() - 1)
    }

    /// `ctrl+b`: pick the database the active query tab runs against.
    fn open_database_picker(&mut self) {
        let Some(tab) = self.active_tab() else {
            self.error("no open tabs — press ctrl+n for a query tab");
            return;
        };
        if tab.kind != TabKind::Query {
            self.error("switching databases applies to query tabs");
            return;
        }
        let base = self.base_conn(tab.conn);
        let current = self.database_of(tab.conn).unwrap_or_default().to_string();
        // Databases already listed in the tree need no round trip.
        let items = self.tree.databases(base);
        let selected = items.iter().position(|d| *d == current).unwrap_or(0);
        let loading = items.is_empty();
        self.overlay = Overlay::Picker(Box::new(Picker {
            title: format!("database — {}", self.conn_label(base)),
            base,
            tab: self.tab,
            items,
            filter: String::new(),
            selected,
            loading,
        }));
        if loading {
            self.request(base, DbRequest::Databases);
        }
    }

    /// Points a query tab at another database on the same server.
    fn switch_database(&mut self, tab_idx: usize, database: &str) {
        let Some(base) = self.tabs.get(tab_idx).map(|t| self.base_conn(t.conn)) else {
            return;
        };
        let Some(id) = self.conn_for_database(base, database) else {
            self.error("that connection is gone");
            return;
        };
        let label = self.conn_label(id);
        if let Some(tab) = self.tabs.get_mut(tab_idx) {
            tab.conn = id;
            tab.conn_name = label;
            tab.completion = None;
            tab.status = TabStatus::Idle;
        }
        // Warm the metadata for the new target so completion has something to
        // offer straight away.
        if !self.tree.is_loaded(id) {
            self.request(id, DbRequest::Schemas);
        }
        self.info(format!("query tab → {database}"));
    }

    fn picker_key(&mut self, ev: KeyEvent) {
        let Overlay::Picker(p) = &mut self.overlay else {
            return;
        };
        match ev.code {
            KeyCode::Esc => self.overlay = Overlay::None,
            KeyCode::Enter => {
                let (chosen, tab) = (p.current(), p.tab);
                self.overlay = Overlay::None;
                match chosen {
                    Some(db) => self.switch_database(tab, &db),
                    None => self.info("nothing to pick"),
                }
            }
            KeyCode::Tab | KeyCode::Down => p.step(1),
            KeyCode::BackTab | KeyCode::Up => p.step(-1),
            KeyCode::Backspace => {
                p.filter.pop();
                p.selected = 0;
            }
            KeyCode::Char(c) => {
                p.filter.push(c);
                p.selected = 0;
            }
            _ => {}
        }
    }

    // --- completion ---

    /// Keys the popup owns while it is open. Returns false for anything else,
    /// which closes the popup and lets the key take its normal course.
    fn completion_key(&mut self, ev: KeyEvent) -> bool {
        match ev.code {
            KeyCode::Esc => {
                self.complete_close();
                true
            }
            KeyCode::Enter => {
                self.complete_accept();
                true
            }
            KeyCode::Tab | KeyCode::Down => {
                self.complete_step(1);
                true
            }
            KeyCode::BackTab | KeyCode::Up => {
                self.complete_step(-1);
                true
            }
            // Typing narrows the list; a dot re-anchors it on what came before.
            KeyCode::Char(c) if complete::is_ident_char(c) || c == '.' => {
                if let Some(tab) = self.active_tab_mut() {
                    tab.editor.insert_char(c);
                }
                self.complete_refresh(false);
                true
            }
            KeyCode::Backspace => {
                if let Some(tab) = self.active_tab_mut() {
                    tab.editor.backspace();
                }
                self.complete_refresh(false);
                true
            }
            _ => {
                self.complete_close();
                false
            }
        }
    }

    /// Opens the popup while typing, the way a query editor is expected to.
    /// Quiet by design: an empty candidate list leaves no popup behind.
    fn auto_complete(&mut self, typed: char) {
        if !self.ui.auto_complete || self.focus() != Focus::Editor {
            return;
        }
        // A dot is a completion request in itself; a letter has to reach the
        // configured width first.
        if typed != '.' {
            if !complete::is_ident_char(typed) {
                return;
            }
            let min = self.ui.complete_after.max(1);
            let wide_enough = self.active_tab().is_some_and(|t| {
                let e = &t.editor;
                e.col - complete::word_start_col(&e.lines[e.line], e.col) >= min
            });
            if !wide_enough {
                return;
            }
        }
        self.complete_refresh(false);
    }

    /// The completion chord: opens the popup, or steps it when already open.
    fn complete_trigger(&mut self) {
        if self.focus() != Focus::Editor {
            return;
        }
        if self.active_tab().is_some_and(|t| t.completion.is_some()) {
            self.complete_step(1);
            return;
        }
        // Completion is an insert-mode affair, so step into insert first —
        // after the current character when the cursor rests on a word.
        if let Some(tab) = self.active_tab_mut()
            && tab.kind == TabKind::Query
            && tab.editor.mode == Mode::Normal
        {
            let e = &mut tab.editor;
            if e.lines[e.line]
                .chars()
                .nth(e.col)
                .is_some_and(complete::is_ident_char)
            {
                e.enter_insert_after();
            } else {
                e.enter_insert();
            }
        }
        self.complete_refresh(true);
    }

    /// Rebuilds the candidate list for the cursor. `announce` reports an empty
    /// result, so an explicit trigger says something and typing stays quiet.
    fn complete_refresh(&mut self, announce: bool) {
        let Some(tab) = self.active_tab() else {
            return;
        };
        if tab.kind != TabKind::Query {
            return;
        }
        let Some(src) = self.completion_sources(tab.conn) else {
            return;
        };
        let e = &tab.editor;
        let (line, col) = (e.line, e.col);
        let start_col = complete::word_start_col(&e.lines[line], col);
        let items = complete::candidates(&e.text(), e.byte_cursor(), &src);
        let empty = items.is_empty();
        if let Some(tab) = self.active_tab_mut() {
            tab.completion = if empty {
                None
            } else {
                Some(Completion::new(items, line, start_col))
            };
        }
        if empty && announce {
            self.info("no completions");
        }
    }

    /// Everything already known about a connection, for the completer.
    fn completion_sources(&self, conn: ConnId) -> Option<Sources> {
        let engine = self.conn(conn)?.cfg.engine;
        let (schemas, tables) = self.tree.identifiers(conn);
        let columns = self
            .columns
            .iter()
            .filter(|((id, _, _), _)| *id == conn)
            .map(|((_, s, t), cols)| ((s.clone(), t.clone()), cols.clone()))
            .collect();
        Some(Sources {
            engine,
            schemas,
            tables,
            columns,
        })
    }

    fn complete_step(&mut self, delta: isize) {
        if let Some(c) = self.active_tab_mut().and_then(|t| t.completion.as_mut()) {
            c.step(delta);
        }
    }

    fn complete_accept(&mut self) {
        let Some(tab) = self.active_tab_mut() else {
            return;
        };
        let Some(c) = tab.completion.take() else {
            return;
        };
        let Some(item) = c.items.get(c.selected) else {
            return;
        };
        // The buffer can have moved on under a stale popup; only replace what
        // the popup was actually anchored to.
        if c.line == tab.editor.line && c.start_col <= tab.editor.col {
            let text = item.text.clone();
            tab.editor.replace_to_cursor(c.start_col, &text);
        }
    }

    fn complete_close(&mut self) {
        if let Some(tab) = self.active_tab_mut() {
            tab.completion = None;
        }
    }

    /// Remembers a table's columns so completion can offer them later.
    fn remember_columns(&mut self, conn: ConnId, kind: &TabKind, grid: &Grid) {
        let (schema, table, cols) = match kind {
            TabKind::Data { schema, table } => (schema, table, grid.columns.clone()),
            TabKind::Structure { schema, table } => (
                schema,
                table,
                grid.rows
                    .iter()
                    .filter_map(|r| r.first().cloned().flatten())
                    .collect(),
            ),
            TabKind::Query => return,
        };
        if cols.is_empty() {
            return;
        }
        self.columns
            .insert((conn, schema.clone(), table.clone()), cols);
    }

    fn prompt_key(&mut self, kind: PromptKind, ev: KeyEvent) {
        match ev.code {
            KeyCode::Esc => {
                self.overlay = Overlay::None;
                if kind == PromptKind::Filter {
                    self.tree.filter.clear();
                    self.tree.clamp();
                }
                self.input.clear();
            }
            KeyCode::Enter => {
                let text = std::mem::take(&mut self.input);
                self.overlay = Overlay::None;
                match kind {
                    PromptKind::Filter => self.tree.clamp(),
                    PromptKind::Search => {
                        if let Some(tab) = self.active_tab_mut() {
                            tab.view.search = text.clone();
                            if !tab.view.search_step(true) {
                                self.error(format!("no match for `{text}`"));
                            }
                        }
                    }
                    PromptKind::Command => self.run_command(&text),
                }
            }
            KeyCode::Backspace => {
                self.input.pop();
                if kind == PromptKind::Filter {
                    self.tree.filter = self.input.clone();
                    self.tree.clamp();
                }
            }
            KeyCode::Char(c) => {
                self.input.push(c);
                if kind == PromptKind::Filter {
                    self.tree.filter = self.input.clone();
                    self.tree.clamp();
                }
            }
            _ => {}
        }
    }

    // --- actions ---

    pub fn dispatch(&mut self, action: Action) {
        use Action::*;
        if action != Complete {
            self.complete_close();
        }
        match action {
            Quit => self.quit = true,
            Help => {
                self.help_scroll = 0;
                self.overlay = Overlay::Help;
            }
            Escape => self.escape(),
            CommandMode => self.open_prompt(PromptKind::Command),
            CancelQuery => self.cancel_current(),
            Refresh => self.refresh(),
            ToggleFullscreen => self.fullscreen = !self.fullscreen,

            PaneLeft => self.reset_focus(Focus::Tree),
            PaneRight => self.enter_workspace(),
            PaneUp => {
                if self.focus() == Focus::Grid && self.active_tab().is_some_and(Tab::has_editor) {
                    self.reset_focus(Focus::Editor);
                }
            }
            PaneDown => {
                if self.focus() == Focus::Editor {
                    self.push_focus(Focus::Grid);
                }
            }
            FocusTree => self.reset_focus(Focus::Tree),
            FocusGrid => {
                if self.tabs.is_empty() {
                    self.error("no open tabs");
                } else {
                    self.reset_focus(Focus::Grid);
                }
            }
            FocusEditor => self.focus_editor(),
            NextTab => self.cycle_tab(1),
            PrevTab => self.cycle_tab(-1),
            CloseTab => self.close_tab(),
            NewQueryTab => self.new_query_tab(),

            Down | Up | Left | Right | Top | Bottom | HalfDown | HalfUp | RowStart | RowEnd => {
                self.movement(action)
            }

            Expand => self.tree_expand(true),
            Collapse => self.tree_expand(false),
            Toggle => self.tree_toggle(),
            Open => self.tree_open(false),
            OpenStructure => self.tree_open(true),
            Filter => self.open_prompt(PromptKind::Filter),
            NextConnection => self.tree.jump_connection(true),
            AddConnection => self.open_new_connection_form(),
            AddProject => {
                self.overlay = Overlay::Form(Box::new(Form::new_project(self.project_names())))
            }
            EditEntry => self.open_edit_form(),
            DeleteEntry => self.ask_delete(),
            TestConnection => self.test_selected_connection(),
            PrevConnection => self.tree.jump_connection(false),

            NextPage => self.page(1),
            PrevPage => self.page(-1),
            YankCell => {
                let text = self
                    .active_tab()
                    .map(|t| t.view.current_cell_text())
                    .unwrap_or_default();
                self.copy(&text, "cell");
            }
            YankRow => {
                let text = self
                    .active_tab()
                    .map(|t| t.view.current_row_text())
                    .unwrap_or_default();
                self.copy(&text, "row");
            }
            Inspect => {
                if let Some(t) = self.active_tab() {
                    let text = match t.view.current_cell() {
                        Some(Some(s)) => s.clone(),
                        Some(None) => "NULL".to_string(),
                        None => return,
                    };
                    self.overlay = Overlay::Inspect(text);
                }
            }
            Search => self.open_prompt(PromptKind::Search),
            SearchNext => self.search_step(true),
            SearchPrev => self.search_step(false),

            InsertMode => self.with_editor(|e| e.enter_insert()),
            InsertAfter => self.with_editor(|e| e.enter_insert_after()),
            InsertLineStart => self.with_editor(|e| {
                e.line_start();
                e.enter_insert();
            }),
            AppendLineEnd => self.with_editor(|e| {
                e.enter_insert();
                e.line_end();
            }),
            OpenLineBelow => self.with_editor(|e| e.open_below()),
            OpenLineAbove => self.with_editor(|e| e.open_above()),
            NormalMode => self.with_editor(|e| e.enter_normal()),
            DeleteLine => self.with_editor(|e| e.delete_line()),
            DeleteChar => self.with_editor(|e| e.delete_char()),
            Undo => self.with_editor(|e| {
                e.undo();
            }),
            WordForward => self.with_editor(|e| e.word_forward()),
            WordBack => self.with_editor(|e| e.word_back()),
            RunStatement => self.run_editor(false),
            RunAll => self.run_editor(true),
            HistoryPrev => self.history_step(true),
            HistoryNext => self.history_step(false),
            Complete => self.complete_trigger(),
            SwitchDatabase => self.open_database_picker(),
        }
    }

    fn escape(&mut self) {
        if self.overlay != Overlay::None {
            self.overlay = Overlay::None;
            self.input.clear();
            return;
        }
        if self.focus() == Focus::Editor
            && self
                .active_tab()
                .is_some_and(|t| t.editor.mode == Mode::Insert)
        {
            self.with_editor(|e| e.enter_normal());
            return;
        }
        if !self.pending.is_empty() {
            self.pending.clear();
            return;
        }
        if self.pop_focus() {
            return;
        }
        // At the base pane: shed state instead of doing nothing.
        if !self.tree.filter.is_empty() {
            self.tree.filter.clear();
            self.tree.clamp();
            self.info("filter cleared");
        }
    }

    fn open_prompt(&mut self, kind: PromptKind) {
        if kind == PromptKind::Search && self.tabs.is_empty() {
            self.error("no result grid to search");
            return;
        }
        self.input = if kind == PromptKind::Filter {
            self.tree.filter.clone()
        } else {
            String::new()
        };
        self.overlay = Overlay::Prompt(kind);
    }

    fn enter_workspace(&mut self) {
        if self.tabs.is_empty() {
            self.error("no open tabs — press ctrl+n for a query tab");
            return;
        }
        if self.focus() == Focus::Tree {
            let f = if self.active_tab().is_some_and(Tab::has_editor) {
                Focus::Editor
            } else {
                Focus::Grid
            };
            self.push_focus(f);
        }
    }

    fn focus_editor(&mut self) {
        if self.active_tab().is_some_and(Tab::has_editor) {
            self.reset_focus(Focus::Editor);
        } else {
            self.new_query_tab();
        }
    }

    fn with_editor(&mut self, f: impl FnOnce(&mut Editor)) {
        if let Some(tab) = self.active_tab_mut()
            && tab.kind == TabKind::Query
        {
            f(&mut tab.editor);
        }
    }

    fn movement(&mut self, action: Action) {
        use Action::*;
        let half_tree = (self.tree_height / 2).max(1) as isize;
        let half_grid = (self.grid_height / 2).max(1) as isize;
        let half_editor = (self.editor_height / 2).max(1) as isize;
        match self.focus() {
            Focus::Tree => match action {
                Down => self.tree.move_cursor(1),
                Up => self.tree.move_cursor(-1),
                HalfDown => self.tree.move_cursor(half_tree),
                HalfUp => self.tree.move_cursor(-half_tree),
                Top => self.tree.cursor = 0,
                Bottom => {
                    self.tree.cursor = self.tree.rows().len().saturating_sub(1);
                }
                _ => {}
            },
            Focus::Grid => {
                let Some(tab) = self.active_tab_mut() else {
                    return;
                };
                match action {
                    Down => tab.view.move_row(1),
                    Up => tab.view.move_row(-1),
                    Left => tab.view.move_col(-1),
                    Right => tab.view.move_col(1),
                    HalfDown => tab.view.move_row(half_grid),
                    HalfUp => tab.view.move_row(-half_grid),
                    Top => tab.view.top(),
                    Bottom => tab.view.bottom(),
                    RowStart => tab.view.row_start(),
                    RowEnd => tab.view.row_end(),
                    _ => {}
                }
            }
            Focus::Editor => {
                let Some(tab) = self.active_tab_mut() else {
                    return;
                };
                let e = &mut tab.editor;
                match action {
                    Down => e.move_line(1),
                    Up => e.move_line(-1),
                    Left => e.move_col(-1),
                    Right => e.move_col(1),
                    HalfDown => e.move_line(half_editor),
                    HalfUp => e.move_line(-half_editor),
                    Top => e.top(),
                    Bottom => e.bottom(),
                    RowStart => e.line_start(),
                    RowEnd => e.line_end(),
                    _ => {}
                }
            }
        }
    }

    fn search_step(&mut self, forward: bool) {
        let mut miss = false;
        if let Some(tab) = self.active_tab_mut() {
            miss = tab.view.search.is_empty() || !tab.view.search_step(forward);
        }
        if miss {
            self.error("no match");
        }
    }

    // --- tree actions ---

    fn tree_expand(&mut self, expand: bool) {
        let Some(row) = self.tree.selected_row() else {
            return;
        };
        if expand {
            if !row.has_children_slot {
                return;
            }
            if matches!(row.kind, NodeKind::Connection | NodeKind::Database)
                && !self.tree.node(&row.path).unwrap().loaded
            {
                self.load_children(&row);
                return;
            }
            if row.kind == NodeKind::Schema && !self.tree.node(&row.path).unwrap().loaded {
                self.load_children(&row);
                return;
            }
            if let Some(n) = self.tree.node_mut(&row.path) {
                n.expanded = true;
            }
        } else if row.expanded {
            if let Some(n) = self.tree.node_mut(&row.path) {
                n.expanded = false;
            }
        } else if row.path.len() > 1 {
            // Collapsing a leaf jumps to its parent, like a file tree.
            let parent = &row.path[..row.path.len() - 1];
            let rows = self.tree.rows();
            if let Some(i) = rows.iter().position(|r| r.path == parent) {
                self.tree.cursor = i;
            }
        }
    }

    fn tree_toggle(&mut self) {
        let Some(row) = self.tree.selected_row() else {
            return;
        };
        if row.expanded {
            self.tree_expand(false);
        } else {
            self.tree_expand(true);
        }
    }

    fn load_children(&mut self, row: &crate::tree::TreeRow) {
        let Some(conn) = row.conn else { return };
        match row.kind {
            NodeKind::Connection => {
                if let Some(n) = self.tree.node_mut(&row.path) {
                    n.loading = true;
                    n.state = ConnState::Connecting;
                }
                self.request(conn, DbRequest::Databases);
            }
            NodeKind::Database => {
                if let Some(n) = self.tree.node_mut(&row.path) {
                    n.loading = true;
                    n.state = ConnState::Connecting;
                }
                self.request(conn, DbRequest::Schemas);
            }
            NodeKind::Schema => {
                if let Some(n) = self.tree.node_mut(&row.path) {
                    n.loading = true;
                }
                let schema = row.name.clone();
                self.request(conn, DbRequest::Tables { schema });
            }
            _ => {}
        }
    }

    fn tree_open(&mut self, structure: bool) {
        let Some(row) = self.tree.selected_row() else {
            return;
        };
        match row.kind {
            NodeKind::Project => self.tree_toggle(),
            NodeKind::Connection | NodeKind::Database | NodeKind::Schema => {
                let loaded = self.tree.node(&row.path).map(|n| n.loaded).unwrap_or(false);
                if loaded {
                    self.tree_toggle();
                } else {
                    self.load_children(&row);
                }
            }
            NodeKind::Table => {
                let (Some(conn), Some(schema)) = (row.conn, row.schema.clone()) else {
                    return;
                };
                self.open_table(conn, schema, row.name.clone(), structure);
            }
        }
    }

    // --- tabs ---

    pub fn conn(&self, id: ConnId) -> Option<&ConnEntry> {
        self.conns.get(id).and_then(|c| c.as_ref())
    }

    fn conn_label(&self, id: ConnId) -> String {
        match self.conn(id) {
            Some(e) => format!("{}/{}", e.project, e.cfg.name),
            None => format!("conn{id}"),
        }
    }

    /// Project names, taken from the tree so they never go stale.
    fn project_names(&self) -> Vec<String> {
        self.tree.roots.iter().map(|p| p.name.clone()).collect()
    }

    /// Every live `(project, connection name)` pair, for the uniqueness check.
    fn existing_pairs(&self) -> Vec<(String, String)> {
        self.conns
            .iter()
            .flatten()
            .filter(|e| e.origin.is_none())
            .map(|e| (e.project.clone(), e.cfg.name.clone()))
            .collect()
    }

    fn open_table(&mut self, conn: ConnId, schema: String, table: String, structure: bool) {
        let kind = if structure {
            TabKind::Structure {
                schema: schema.clone(),
                table: table.clone(),
            }
        } else {
            TabKind::Data {
                schema: schema.clone(),
                table: table.clone(),
            }
        };
        let existing = self
            .tabs
            .iter()
            .position(|t| t.conn == conn && t.kind == kind);
        let idx = match existing {
            Some(i) => i,
            None => {
                let suffix = if structure { " ⋮" } else { "" };
                self.tabs.push(Tab {
                    conn,
                    conn_name: self.conn_label(conn),
                    title: format!("{schema}.{table}{suffix}"),
                    kind: kind.clone(),
                    view: GridView::default(),
                    editor: Editor::default(),
                    last_sql: String::new(),
                    status: TabStatus::Idle,
                    offset: 0,
                    completion: None,
                });
                self.tabs.len() - 1
            }
        };
        self.tab = idx;
        self.reset_focus(Focus::Tree);
        self.push_focus(Focus::Grid);
        self.load_tab(idx);
    }

    fn load_tab(&mut self, idx: usize) {
        let Some(tab) = self.tabs.get(idx) else {
            return;
        };
        let conn = tab.conn;
        let req = self.next_req;
        self.next_req += 1;
        let limit = self.ui.page_size;
        let request = match tab.kind.clone() {
            TabKind::Data { schema, table } => DbRequest::TableData {
                schema,
                table,
                limit,
                offset: tab.offset,
                req,
            },
            TabKind::Structure { schema, table } => DbRequest::Columns { schema, table, req },
            TabKind::Query => {
                if tab.last_sql.trim().is_empty() {
                    return;
                }
                DbRequest::Query {
                    sql: tab.last_sql.clone(),
                    req,
                    max_rows: limit,
                }
            }
        };
        self.inflight.insert(req, idx);
        if let Some(t) = self.tabs.get_mut(idx) {
            t.status = TabStatus::Running(req);
        }
        self.request(conn, request);
    }

    fn new_query_tab(&mut self) {
        let conn = self
            .tree
            .selected_row()
            .and_then(|r| r.conn)
            .or_else(|| self.active_tab().map(|t| t.conn))
            .or_else(|| self.conns.iter().position(|c| c.is_some()));
        let Some(conn) = conn else {
            self.error("no connections configured");
            return;
        };
        let n = self
            .tabs
            .iter()
            .filter(|t| t.kind == TabKind::Query)
            .count()
            + 1;
        self.tabs.push(Tab {
            conn,
            conn_name: self.conn_label(conn),
            title: format!("query {n}"),
            kind: TabKind::Query,
            view: GridView::default(),
            editor: Editor::default(),
            last_sql: String::new(),
            status: TabStatus::Idle,
            offset: 0,
            completion: None,
        });
        self.tab = self.tabs.len() - 1;
        self.reset_focus(Focus::Tree);
        self.push_focus(Focus::Editor);
        self.with_editor(|e| e.enter_insert());
    }

    fn close_tab(&mut self) {
        if self.tabs.is_empty() {
            return;
        }
        self.tabs.remove(self.tab);
        if self.tab >= self.tabs.len() {
            self.tab = self.tabs.len().saturating_sub(1);
        }
        if self.tabs.is_empty() {
            self.reset_focus(Focus::Tree);
        }
    }

    fn cycle_tab(&mut self, delta: isize) {
        if self.tabs.is_empty() {
            return;
        }
        let len = self.tabs.len() as isize;
        self.tab = (((self.tab as isize + delta) % len + len) % len) as usize;
        // A data tab has no editor, so never leave the focus pointing at one.
        if self.focus() == Focus::Editor && !self.active_tab().is_some_and(Tab::has_editor) {
            self.reset_focus(Focus::Tree);
            self.push_focus(Focus::Grid);
        }
    }

    fn page(&mut self, delta: isize) {
        let Some(tab) = self.active_tab() else { return };
        if !matches!(tab.kind, TabKind::Data { .. }) {
            self.error("paging only applies to table data tabs");
            return;
        }
        let size = self.ui.page_size;
        let next = (tab.offset as isize + delta * size as isize).max(0) as usize;
        if next == tab.offset && delta < 0 {
            self.info("already at the first page");
            return;
        }
        let idx = self.tab;
        if let Some(t) = self.tabs.get_mut(idx) {
            t.offset = next;
        }
        self.load_tab(idx);
    }

    fn refresh(&mut self) {
        match self.focus() {
            Focus::Tree => {
                let Some(row) = self.tree.selected_row() else {
                    return;
                };
                if let Some(n) = self.tree.node_mut(&row.path) {
                    n.loaded = false;
                }
                self.load_children(&row);
            }
            _ => {
                let idx = self.tab;
                if self.tabs.get(idx).is_some() {
                    self.load_tab(idx);
                }
            }
        }
    }

    fn cancel_current(&mut self) {
        let Some(tab) = self.active_tab() else {
            self.info("nothing running");
            return;
        };
        if let TabStatus::Running(_) = tab.status {
            let conn = tab.conn;
            if let Some(h) = self.handles.get(&conn) {
                h.cancel();
                self.info("cancel sent");
            }
        } else {
            self.info("nothing running");
        }
    }

    // --- running SQL ---

    fn run_editor(&mut self, all: bool) {
        let Some(tab) = self.active_tab() else { return };
        if tab.kind != TabKind::Query {
            self.error("not a query tab");
            return;
        }
        if tab.editor.is_blank() {
            self.error("nothing to run");
            return;
        }
        let text = tab.editor.text();
        let sql = if all {
            text
        } else {
            match statement_at(&text, tab.editor.byte_cursor()) {
                Some((s, e)) => text[s..e].trim().to_string(),
                None => text,
            }
        };
        if sql.trim().is_empty() {
            self.error("nothing to run");
            return;
        }
        self.run_sql(sql, false);
    }

    fn run_sql(&mut self, sql: String, confirmed: bool) {
        if !confirmed && self.ui.confirm_writes && !is_read_only(&sql) {
            self.overlay = Overlay::Confirm(Confirm::RunSql(sql));
            return;
        }
        let idx = self.tab;
        let Some(tab) = self.tabs.get_mut(idx) else {
            return;
        };
        tab.last_sql = sql.clone();
        let conn = tab.conn;
        let req = self.new_req();
        self.inflight.insert(req, idx);
        if let Some(t) = self.tabs.get_mut(idx) {
            t.status = TabStatus::Running(req);
        }
        if self.history.last() != Some(&sql) {
            self.history.push(sql.clone());
        }
        self.history_pos = None;
        let max_rows = self.ui.page_size;
        self.request(conn, DbRequest::Query { sql, req, max_rows });
        self.push_focus(Focus::Grid);
    }

    fn history_step(&mut self, back: bool) {
        if self.history.is_empty() {
            self.info("no query history");
            return;
        }
        let pos = match (self.history_pos, back) {
            (None, true) => self.history.len() - 1,
            (None, false) => return,
            (Some(p), true) => p.saturating_sub(1),
            (Some(p), false) => (p + 1).min(self.history.len() - 1),
        };
        self.history_pos = Some(pos);
        let sql = self.history[pos].clone();
        self.with_editor(|e| e.set_text(&sql));
    }

    fn run_command(&mut self, cmd: &str) {
        let cmd = cmd.trim();
        let (head, rest) = cmd.split_once(' ').unwrap_or((cmd, ""));
        match head {
            "q" | "quit" => self.quit = true,
            "h" | "help" => {
                self.help_scroll = 0;
                self.overlay = Overlay::Help;
            }
            "r" | "refresh" => self.refresh(),
            "new" | "tabnew" => self.new_query_tab(),
            "close" | "tabclose" => self.close_tab(),
            "db" | "database" => {
                let name = rest.trim();
                if name.is_empty() {
                    self.open_database_picker();
                } else if self.active_tab().is_some_and(|t| t.kind == TabKind::Query) {
                    let idx = self.tab;
                    self.switch_database(idx, name);
                } else {
                    self.error("switching databases applies to query tabs");
                }
            }
            "limit" => match rest.trim().parse::<usize>() {
                Ok(n) if n > 0 => {
                    self.ui.page_size = n;
                    self.info(format!("page size = {n}"));
                }
                _ => self.error("usage: :limit <rows>"),
            },
            "" => {}
            other => self.error(format!("unknown command `{other}` — try :help")),
        }
    }

    // --- connection form ---

    fn selected_project(&self) -> String {
        self.tree
            .selected_row()
            .and_then(|r| self.tree.project_of_row(&r))
            .unwrap_or_default()
    }

    fn open_new_connection_form(&mut self) {
        let form = Form::new_connection(
            &self.selected_project(),
            self.project_names(),
            self.existing_pairs(),
        );
        self.overlay = Overlay::Form(Box::new(form));
    }

    fn open_edit_form(&mut self) {
        let Some(row) = self.tree.selected_row() else {
            self.error("nothing selected");
            return;
        };
        // On a project row `e` renames the group; anywhere else it edits the
        // connection that row belongs to.
        if row.kind == NodeKind::Project {
            let form = Form::rename_project(&row.name, self.project_names());
            self.overlay = Overlay::Form(Box::new(form));
            return;
        }
        let Some(id) = row.conn.map(|c| self.base_conn(c)) else {
            self.error("select a connection to edit");
            return;
        };
        let Some(entry) = self.conn(id).cloned() else {
            self.error("that connection is gone");
            return;
        };
        let form = Form::edit_connection(
            &entry.project,
            &entry.cfg,
            self.project_names(),
            self.existing_pairs(),
        );
        self.overlay = Overlay::Form(Box::new(form));
    }

    fn form_key(&mut self, ev: KeyEvent) {
        let Overlay::Form(form) = &mut self.overlay else {
            return;
        };
        let ctrl = ev.modifiers.contains(KeyModifiers::CONTROL);

        if form.editing {
            match ev.code {
                KeyCode::Esc | KeyCode::Enter | KeyCode::Tab => form.editing = false,
                KeyCode::Backspace => form.backspace(),
                KeyCode::Char('u') if ctrl => form.clear_field(),
                KeyCode::Char(c) if !ctrl => form.type_char(c),
                _ => {}
            }
            return;
        }

        match ev.code {
            KeyCode::Char('s') if ctrl => self.save_form(),
            KeyCode::Char('t') if ctrl => self.test_form(),
            KeyCode::Esc | KeyCode::Char('q') => {
                self.overlay = Overlay::None;
                self.info("form closed");
            }
            KeyCode::Char('j') | KeyCode::Down | KeyCode::Tab => form.move_cursor(1),
            KeyCode::Char('k') | KeyCode::Up | KeyCode::BackTab => form.move_cursor(-1),
            KeyCode::Enter | KeyCode::Char('i') => {
                let id = form.current();
                match form.kind(id) {
                    FieldKind::Text | FieldKind::Secret => form.editing = true,
                    _ => form.cycle(1),
                }
            }
            KeyCode::Char(' ') | KeyCode::Char('l') | KeyCode::Right => form.cycle(1),
            KeyCode::Char('h') | KeyCode::Left => form.cycle(-1),
            _ => {}
        }
    }

    fn save_form(&mut self) {
        let Overlay::Form(boxed) = &self.overlay else {
            return;
        };
        let mut form = (**boxed).clone();
        if !form.validate() {
            let first = form.errors.first().cloned();
            // Put the cursor on the first problem.
            if let Some((id, _)) = &first
                && let Some(pos) = form.fields().iter().position(|f| f == id)
            {
                form.cursor = pos;
            }
            let first = first.map(|(_, m)| m);
            self.overlay = Overlay::Form(Box::new(form));
            if let Some(m) = first {
                self.error(m);
            }
            return;
        }

        let result = match form.mode.clone() {
            FormMode::NewProject => self.save_new_project(&form),
            FormMode::RenameProject { name } => self.save_renamed_project(&name, &form),
            FormMode::NewConnection => self.save_new_connection(&form),
            FormMode::EditConnection { project, name } => {
                self.save_edited_connection(&project, &name, &form)
            }
        };
        match result {
            Ok(msg) => {
                self.overlay = Overlay::None;
                self.info(msg);
            }
            Err(e) => {
                self.overlay = Overlay::Form(Box::new(form));
                self.error(format!("{e:#}"));
            }
        }
    }

    fn save_new_project(&mut self, form: &Form) -> anyhow::Result<String> {
        let name = form.project.trim().to_string();
        crate::store::add_project(&self.config_path, &name)?;
        self.tree.ensure_project(&name);
        Ok(format!("project `{name}` created"))
    }

    fn save_renamed_project(&mut self, old: &str, form: &Form) -> anyhow::Result<String> {
        let new = form.project.trim().to_string();
        if old == new {
            return Ok("nothing changed".into());
        }
        let warnings = crate::store::rename_project(&self.config_path, old, &new)?;
        self.tree.rename_project(old, &new);
        for e in self.conns.iter_mut().flatten() {
            if e.project == old {
                e.project = new.clone();
                // Keep the account in step with what store.rs wrote.
                if e.cfg.keyring.as_deref()
                    == Some(crate::store::keyring_account(old, &e.cfg.name).as_str())
                    && warnings.is_empty()
                {
                    e.cfg.keyring = Some(crate::store::keyring_account(&new, &e.cfg.name));
                }
            }
        }
        Ok(if warnings.is_empty() {
            format!("project renamed to `{new}`")
        } else {
            format!("project renamed to `{new}` — {}", warnings.join("; "))
        })
    }

    fn store_form_password(&self, form: &Form) -> anyhow::Result<()> {
        if let Some((account, pw)) = form.password_to_store() {
            crate::store::set_password(&account, &pw)?;
        }
        Ok(())
    }

    fn save_new_connection(&mut self, form: &Form) -> anyhow::Result<String> {
        let project = form.project.trim().to_string();
        let conn = form.to_connection();
        self.store_form_password(form)?;
        crate::store::upsert_connection(&self.config_path, &project, None, &conn)?;

        let id = self.conns.len();
        self.conns.push(Some(ConnEntry {
            project: project.clone(),
            cfg: conn.clone(),
            origin: None,
        }));
        self.tree
            .add_connection(&project, &conn.name, conn.engine, id);
        self.tree.focus_connection(id);
        Ok(format!("{project}/{} added", conn.name))
    }

    fn save_edited_connection(
        &mut self,
        old_project: &str,
        old_name: &str,
        form: &Form,
    ) -> anyhow::Result<String> {
        let Some(id) = self.conns.iter().position(|c| {
            c.as_ref().is_some_and(|e| {
                e.origin.is_none() && e.project == old_project && e.cfg.name == old_name
            })
        }) else {
            anyhow::bail!("{old_project}/{old_name} is no longer in the config");
        };
        let project = form.project.trim().to_string();
        let conn = form.to_connection();
        let previous = self.conn(id).map(|e| e.cfg.clone());

        self.store_form_password(form)?;
        crate::store::upsert_connection(
            &self.config_path,
            &project,
            Some((old_project, old_name)),
            &conn,
        )?;
        if let Some(stale) = form.stale_keyring_account() {
            let _ = crate::store::delete_password(&stale);
        }

        // Anything that changes which server we talk to invalidates the loaded
        // tree and the live worker.
        let repoint = previous.as_ref().is_some_and(|p| {
            p.host != conn.host
                || p.port_or_default() != conn.port_or_default()
                || p.database != conn.database
                || p.user != conn.user
                || p.engine != conn.engine
        });

        self.conns[id] = Some(ConnEntry {
            project: project.clone(),
            cfg: conn.clone(),
            origin: None,
        });
        self.tree
            .update_connection(id, &project, &conn.name, conn.engine);
        if repoint {
            self.drop_connection(id);
            self.forget_derived(id);
            self.tree.reset_connection_children(id);
        }
        for tab in self.tabs.iter_mut().filter(|t| t.conn == id) {
            tab.conn_name = format!("{project}/{}", conn.name);
        }
        self.tree.focus_connection(id);
        Ok(format!(
            "{project}/{} saved{}",
            conn.name,
            if repoint { " — will reconnect" } else { "" }
        ))
    }

    /// Shuts down a connection's worker so the next request reconnects.
    fn drop_connection(&mut self, id: ConnId) {
        if let Some(h) = self.handles.remove(&id) {
            h.send(DbRequest::Shutdown);
        }
        self.tree.set_state(id, ConnState::Idle);
    }

    // --- deleting ---

    fn ask_delete(&mut self) {
        let Some(row) = self.tree.selected_row() else {
            return;
        };
        match row.kind {
            NodeKind::Project => {
                let connections: Vec<String> = self
                    .conns
                    .iter()
                    .flatten()
                    .filter(|e| e.origin.is_none() && e.project == row.name)
                    .map(|e| e.cfg.name.clone())
                    .collect();
                self.overlay = Overlay::Confirm(Confirm::DeleteProject {
                    name: row.name.clone(),
                    connections,
                });
            }
            _ => {
                let Some(id) = row.conn.map(|c| self.base_conn(c)) else {
                    self.error("select a connection or project to delete");
                    return;
                };
                let Some(entry) = self.conn(id) else { return };
                let managed = crate::store::keyring_account(&entry.project, &entry.cfg.name);
                let account = entry
                    .cfg
                    .keyring
                    .as_deref()
                    .filter(|k| *k == managed)
                    .map(str::to_string);
                self.overlay = Overlay::Confirm(Confirm::DeleteConnection {
                    id,
                    project: entry.project.clone(),
                    name: entry.cfg.name.clone(),
                    account,
                });
            }
        }
    }

    fn apply_confirmed(&mut self, what: Confirm) {
        match what {
            Confirm::RunSql(sql) => self.run_sql(sql, true),
            Confirm::DeleteConnection {
                id,
                project,
                name,
                account,
            } => match crate::store::remove_connection(&self.config_path, &project, &name) {
                Ok(()) => {
                    if let Some(a) = &account {
                        let _ = crate::store::delete_password(a);
                    }
                    self.forget_connection(id);
                    self.info(format!("{project}/{name} deleted"));
                }
                Err(e) => self.error(format!("{e:#}")),
            },
            Confirm::DeleteProject { name, .. } => {
                match crate::store::remove_project(&self.config_path, &name) {
                    Ok(warnings) => {
                        for id in self.tree.remove_project(&name) {
                            self.forget_connection_state(id);
                        }
                        self.conns
                            .iter_mut()
                            .filter(|c| c.as_ref().is_some_and(|e| e.project == name))
                            .for_each(|c| *c = None);
                        if warnings.is_empty() {
                            self.info(format!("project `{name}` deleted"));
                        } else {
                            self.error(format!("project deleted — {}", warnings.join("; ")));
                        }
                    }
                    Err(e) => self.error(format!("{e:#}")),
                }
            }
        }
    }

    fn forget_connection(&mut self, id: ConnId) {
        self.tree.remove_connection(id);
        self.forget_connection_state(id);
        if let Some(slot) = self.conns.get_mut(id) {
            *slot = None;
        }
    }

    /// Closes the worker and any tabs belonging to a connection that is going
    /// away, without disturbing other connections' ids.
    fn forget_connection_state(&mut self, id: ConnId) {
        self.forget_derived(id);
        self.drop_connection(id);
        self.tabs.retain(|t| t.conn != id);
        if self.tab >= self.tabs.len() {
            self.tab = self.tabs.len().saturating_sub(1);
        }
        if self.tabs.is_empty() {
            self.reset_focus(Focus::Tree);
        }
    }

    // --- test connection ---

    fn spawn_test(&mut self, cfg: ConnCfg) -> u64 {
        self.test_token += 1;
        let token = self.test_token;
        let tx = self.test_tx.clone();
        tokio::spawn(async move {
            let outcome = match crate::diagnose::probe(&cfg).await {
                Ok(msg) => TestOutcome {
                    token,
                    result: Ok(msg),
                    hints: Vec::new(),
                },
                Err(e) => {
                    let msg = format!("{e:#}");
                    let hints = crate::diagnose::hints(&cfg, &msg);
                    TestOutcome {
                        token,
                        result: Err(msg),
                        hints,
                    }
                }
            };
            let _ = tx.send(outcome);
        });
        token
    }

    fn test_form(&mut self) {
        let Overlay::Form(boxed) = &self.overlay else {
            return;
        };
        let mut form = (**boxed).clone();
        if form.is_project_only() {
            self.info("nothing to test on a project");
            return;
        }
        if !form.validate() {
            let first = form.errors.first().map(|(_, m)| m.clone());
            self.overlay = Overlay::Form(Box::new(form));
            if let Some(m) = first {
                self.error(m);
            }
            return;
        }
        let mut cfg = form.to_connection();
        // A password typed but not yet saved has to be tried directly, since it
        // is not in the keyring yet.
        if let Some((_, pw)) = form.password_to_store() {
            cfg.keyring = None;
            cfg.password_command = None;
            cfg.password = Some(pw);
        }
        form.test = TestState::Running;
        self.overlay = Overlay::Form(Box::new(form));
        self.spawn_test(cfg);
    }

    fn test_selected_connection(&mut self) {
        let Some(id) = self.tree.selected_row().and_then(|r| r.conn) else {
            self.error("select a connection to test");
            return;
        };
        let Some(entry) = self.conn(id) else { return };
        let label = format!("{}/{}", entry.project, entry.cfg.name);
        let cfg = entry.cfg.clone();
        self.spawn_test(cfg);
        self.info(format!("testing {label}…"));
    }

    pub fn on_test_outcome(&mut self, outcome: TestOutcome) {
        if outcome.token != self.test_token {
            return; // superseded
        }
        let state = match &outcome.result {
            Ok(msg) => TestState::Ok(msg.clone()),
            Err(msg) => TestState::Failed(msg.clone(), outcome.hints.clone()),
        };
        if let Overlay::Form(form) = &mut self.overlay {
            form.test = state;
            return;
        }
        match outcome.result {
            Ok(msg) => self.info(format!("test: {msg}")),
            Err(msg) => self.error(format!("test failed: {msg}")),
        }
    }

    // --- clipboard ---

    fn copy(&mut self, text: &str, what: &str) {
        if text.is_empty() {
            self.info("nothing to copy");
            return;
        }
        crate::clipboard::copy(text);
        self.info(format!("{what} copied ({} chars)", text.chars().count()));
    }

    // --- database events ---

    pub fn on_db_event(&mut self, ev: DbEvent) {
        match ev.payload {
            DbPayload::Connected => self.tree.set_state(ev.conn, ConnState::Connected),
            DbPayload::Disconnected(why) => {
                self.tree.set_state(ev.conn, ConnState::Idle);
                self.info(format!("{}: {why}", self.conn_label(ev.conn)));
            }
            DbPayload::Databases(list) => {
                let base = self.base_conn(ev.conn);
                let mut mapped = Vec::with_capacity(list.len());
                for name in &list {
                    let id = self.conn_for_database(base, name).unwrap_or(base);
                    mapped.push((name.clone(), id));
                }
                self.tree.set_databases(base, mapped);
                self.tree.clamp();
                if let Overlay::Picker(p) = &mut self.overlay
                    && p.base == base
                {
                    p.items = list.clone();
                    p.loading = false;
                    p.selected = 0;
                }
                self.info(format!(
                    "{}: {} databases",
                    self.conn_label(base),
                    list.len()
                ));
            }
            DbPayload::Schemas(list) => {
                let n = list.len();
                self.tree.set_schemas(ev.conn, list);
                self.tree.clamp();
                self.info(format!("{}: {n} schemas", self.conn_label(ev.conn)));
            }
            DbPayload::Tables { schema, tables } => {
                self.tree.set_tables(ev.conn, &schema, tables);
                self.tree.clamp();
            }
            DbPayload::Result { req, grid, sql } => {
                let Some(idx) = self.inflight.remove(&req) else {
                    return;
                };
                let summary = summarise(&grid);
                if let Some(tab) = self.tabs.get(idx) {
                    let (conn, kind) = (tab.conn, tab.kind.clone());
                    self.remember_columns(conn, &kind, &grid);
                }
                if let Some(tab) = self.tabs.get_mut(idx) {
                    if tab.status == TabStatus::Running(req) {
                        tab.status = TabStatus::Idle;
                    }
                    if let Some(s) = sql {
                        tab.last_sql = s;
                    }
                    let offset = tab.offset;
                    tab.view.set_grid(grid);
                    tab.view.page_offset = offset;
                }
                self.info(summary);
            }
            DbPayload::Error { req, message } => {
                if let Some(idx) = req.and_then(|r| self.inflight.remove(&r))
                    && let Some(tab) = self.tabs.get_mut(idx)
                {
                    tab.status = TabStatus::Error(message.clone());
                }
                self.tree.set_state(ev.conn, ConnState::Failed);
                let label = self.conn_label(ev.conn);
                self.error(format!("{label}: {message}"));
            }
            DbPayload::Cancelled { req } => {
                if let Some(idx) = self.inflight.remove(&req)
                    && let Some(tab) = self.tabs.get_mut(idx)
                {
                    tab.status = TabStatus::Idle;
                }
                self.info("query cancelled");
            }
        }
    }

    pub fn tick(&mut self) {
        self.spinner = self.spinner.wrapping_add(1);
    }

    pub fn is_busy(&self) -> bool {
        !self.inflight.is_empty()
    }
}

fn summarise(grid: &Grid) -> String {
    match grid.affected {
        Some(n) => format!("{n} row(s) affected in {} ms", grid.elapsed_ms),
        None => {
            let more = if grid.complete { "" } else { " (truncated)" };
            format!(
                "{} row(s) × {} col(s) in {} ms{more}",
                grid.rows.len(),
                grid.columns.len(),
                grid.elapsed_ms
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::form::FieldId;
    use crate::keys::parse_key;

    fn app() -> App {
        let cfg = Config::parse(
            r#"
[[project]]
name = "Alpha"
  [[project.connection]]
  name = "a-db"
  engine = "postgres"
  host = "h"
  database = "d"
  user = "u"
[[project]]
name = "Beta"
  [[project.connection]]
  name = "b-db"
  engine = "mssql"
  host = "h"
  database = "d"
  user = "u"
"#,
        )
        .unwrap();
        App::new(cfg)
    }

    fn key(app: &mut App, spec: &str) {
        let k = parse_key(spec).unwrap();
        app.on_key(KeyEvent::new(k.code, k.mods));
    }

    /// Types literal characters, including ones that are not valid key specs.
    fn type_str(app: &mut App, text: &str) {
        for c in text.chars() {
            app.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
    }

    fn fake_grid() -> Grid {
        Grid {
            columns: vec!["id".into()],
            rows: vec![vec![Some("1".into())], vec![Some("2".into())]],
            affected: None,
            elapsed_ms: 3,
            complete: true,
        }
    }

    /// Opens a data tab without touching a real database.
    fn open_fake_table(app: &mut App) {
        app.tabs.push(Tab {
            conn: 0,
            conn_name: "Alpha/a-db".into(),
            title: "public.users".into(),
            kind: TabKind::Data {
                schema: "public".into(),
                table: "users".into(),
            },
            view: GridView::default(),
            editor: Editor::default(),
            last_sql: String::new(),
            status: TabStatus::Idle,
            offset: 0,
            completion: None,
        });
        app.tab = app.tabs.len() - 1;
        app.tabs[0].view.set_grid(fake_grid());
        app.reset_focus(Focus::Tree);
        app.push_focus(Focus::Grid);
    }

    /// An App whose config file is a real temp file, so save paths are exercised.
    fn app_with_file() -> (std::path::PathBuf, App) {
        let dir = std::env::temp_dir().join(format!(
            "dbx-app-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        let body = r#"# keep me
[[project]]
name = "Alpha"
  [[project.connection]]
  name = "a-db"
  engine = "postgres"
  host = "h"
  database = "d"
  user = "u"
  password_command = "echo x"
[[project]]
name = "Beta"
  [[project.connection]]
  name = "b-db"
  engine = "mssql"
  host = "h"
  database = "d"
  user = "u"
  password_command = "echo x"
"#;
        std::fs::write(&path, body).unwrap();
        let cfg = Config::parse(body).unwrap();
        let app = App::with_path(cfg, path.clone());
        (path, app)
    }

    fn saved(path: &std::path::Path) -> Config {
        Config::parse(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    /// Fills a form the way a user would, without a keyring round-trip.
    fn fill(form: &mut Form, name: &str, host: &str) {
        form.name = name.into();
        form.host = host.into();
        form.database = "db".into();
        form.user = "usr".into();
        form.has_stored_password = true; // pretend a secret already exists
    }

    #[test]
    fn a_opens_the_new_connection_form_on_the_selected_project() {
        let (_p, mut app) = app_with_file();
        app.tree.cursor = 2; // the Beta project row
        assert_eq!(app.tree.selected_row().unwrap().name, "Beta");
        app.dispatch(Action::AddConnection);
        let Overlay::Form(form) = &app.overlay else {
            panic!("form should be open");
        };
        assert_eq!(form.project, "Beta");
        assert_eq!(form.mode, FormMode::NewConnection);
        assert_eq!(form.current(), FieldId::Name);
    }

    #[test]
    fn the_project_is_inferred_from_any_row_depth() {
        let (_p, mut app) = app_with_file();
        app.tree.cursor = 3; // Beta/b-db, a connection row
        app.dispatch(Action::AddConnection);
        let Overlay::Form(form) = &app.overlay else {
            panic!()
        };
        assert_eq!(form.project, "Beta");
    }

    #[test]
    fn saving_a_new_connection_writes_the_config_and_the_tree() {
        let (path, mut app) = app_with_file();
        app.dispatch(Action::AddConnection);
        let Overlay::Form(form) = &mut app.overlay else {
            panic!()
        };
        fill(form, "warehouse", "wh.internal");
        app.save_form();

        assert_eq!(app.overlay, Overlay::None);
        let cfg = saved(&path);
        assert_eq!(cfg.projects[0].connections.len(), 2);
        assert_eq!(cfg.projects[0].connections[1].name, "warehouse");
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("# keep me"),
            "comments must survive"
        );
        // Tree and id table agree.
        let id = 2;
        assert_eq!(app.conn(id).unwrap().cfg.name, "warehouse");
        assert_eq!(app.tree.selected_row().unwrap().name, "warehouse");
    }

    #[test]
    fn saving_into_a_new_project_creates_the_group() {
        let (path, mut app) = app_with_file();
        app.dispatch(Action::AddConnection);
        let Overlay::Form(form) = &mut app.overlay else {
            panic!()
        };
        form.project = "Gamma".into();
        fill(form, "g-db", "g.internal");
        app.save_form();
        assert_eq!(saved(&path).projects.len(), 3);
        assert_eq!(app.project_names(), ["Alpha", "Beta", "Gamma"]);
    }

    #[test]
    fn an_invalid_form_stays_open_with_the_cursor_on_the_problem() {
        let (path, mut app) = app_with_file();
        let before = std::fs::read_to_string(&path).unwrap();
        app.dispatch(Action::AddConnection);
        let Overlay::Form(form) = &mut app.overlay else {
            panic!()
        };
        form.name = "a-db".into(); // already exists in Alpha
        form.database = "db".into();
        form.user = "usr".into();
        form.has_stored_password = true;
        app.save_form();

        let Overlay::Form(form) = &app.overlay else {
            panic!("form must stay open")
        };
        assert_eq!(form.current(), FieldId::Name);
        assert!(form.error_for(FieldId::Name).is_some());
        assert_eq!(app.message.as_ref().unwrap().0, MsgKind::Error);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            before,
            "a rejected save must not touch the file"
        );
    }

    #[test]
    fn e_on_a_connection_edits_it_and_the_edit_round_trips() {
        let (path, mut app) = app_with_file();
        app.tree.cursor = 1; // Alpha/a-db
        app.dispatch(Action::EditEntry);
        let Overlay::Form(form) = &mut app.overlay else {
            panic!()
        };
        assert_eq!(
            form.mode,
            FormMode::EditConnection {
                project: "Alpha".into(),
                name: "a-db".into()
            }
        );
        form.host = "moved.internal".into();
        app.save_form();

        assert_eq!(
            saved(&path).projects[0].connections[0].host,
            "moved.internal"
        );
        assert_eq!(app.conn(0).unwrap().cfg.host, "moved.internal");
        assert_eq!(app.conns.len(), 2, "editing must not add a slot");
    }

    #[test]
    fn repointing_a_connection_drops_its_loaded_objects() {
        let (_p, mut app) = app_with_file();
        app.tree.set_schemas(0, vec!["public".into()]);
        app.tree.cursor = 1;
        app.dispatch(Action::EditEntry);
        let Overlay::Form(form) = &mut app.overlay else {
            panic!()
        };
        form.host = "elsewhere".into();
        app.save_form();
        let path = app.tree.connection_path(0).unwrap();
        assert!(
            app.tree.node(&path).unwrap().children.is_empty(),
            "stale schemas from the old server must go"
        );
    }

    #[test]
    fn a_cosmetic_edit_keeps_loaded_objects() {
        let (_p, mut app) = app_with_file();
        app.tree.set_schemas(0, vec!["public".into()]);
        app.tree.cursor = 1;
        app.dispatch(Action::EditEntry);
        let Overlay::Form(form) = &mut app.overlay else {
            panic!()
        };
        form.read_only = true;
        app.save_form();
        let path = app.tree.connection_path(0).unwrap();
        assert_eq!(app.tree.node(&path).unwrap().children.len(), 1);
        assert!(app.conn(0).unwrap().cfg.read_only);
    }

    #[test]
    fn renaming_a_connection_updates_open_tab_labels() {
        let (_p, mut app) = app_with_file();
        open_fake_table(&mut app);
        app.tree.cursor = 1;
        app.dispatch(Action::EditEntry);
        let Overlay::Form(form) = &mut app.overlay else {
            panic!()
        };
        form.name = "a-db-2".into();
        app.save_form();
        assert_eq!(app.tabs[0].conn_name, "Alpha/a-db-2");
    }

    #[test]
    fn e_on_a_project_row_renames_the_group() {
        let (path, mut app) = app_with_file();
        app.tree.cursor = 0; // Alpha
        app.dispatch(Action::EditEntry);
        let Overlay::Form(form) = &mut app.overlay else {
            panic!()
        };
        assert!(form.is_project_only());
        form.project = "Globex".into();
        app.save_form();
        assert_eq!(saved(&path).projects[0].name, "Globex");
        assert_eq!(app.project_names()[0], "Globex");
        assert_eq!(app.conn(0).unwrap().project, "Globex");
    }

    #[test]
    fn capital_a_creates_an_empty_project() {
        let (path, mut app) = app_with_file();
        app.dispatch(Action::AddProject);
        let Overlay::Form(form) = &mut app.overlay else {
            panic!()
        };
        form.project = "Gamma".into();
        app.save_form();
        assert_eq!(saved(&path).projects.len(), 3);
        assert_eq!(app.tree.roots.len(), 3);
    }

    #[test]
    fn deleting_a_connection_asks_first_then_removes_it() {
        let (path, mut app) = app_with_file();
        app.tree.cursor = 1;
        app.dispatch(Action::DeleteEntry);
        let Overlay::Confirm(Confirm::DeleteConnection { project, name, .. }) = &app.overlay else {
            panic!("expected a delete confirmation")
        };
        assert_eq!((project.as_str(), name.as_str()), ("Alpha", "a-db"));

        let what = match app.overlay.clone() {
            Overlay::Confirm(c) => c,
            _ => unreachable!(),
        };
        app.apply_confirmed(what);
        assert!(saved(&path).projects[0].connections.is_empty());
        assert!(app.conn(0).is_none());
        assert!(app.tree.connection_path(0).is_none());
        // Beta's id is untouched, so its open tabs still resolve.
        assert_eq!(app.conn(1).unwrap().cfg.name, "b-db");
        assert_eq!(app.tree.connection_path(1), Some(vec![1, 0]));
    }

    #[test]
    fn deleting_a_connection_closes_its_tabs_only() {
        let (_p, mut app) = app_with_file();
        open_fake_table(&mut app); // conn 0
        app.tabs.push(Tab {
            conn: 1,
            conn_name: "Beta/b-db".into(),
            title: "other".into(),
            kind: TabKind::Query,
            view: GridView::default(),
            editor: Editor::default(),
            last_sql: String::new(),
            status: TabStatus::Idle,
            offset: 0,
            completion: None,
        });
        app.apply_confirmed(Confirm::DeleteConnection {
            id: 0,
            project: "Alpha".into(),
            name: "a-db".into(),
            account: None,
        });
        assert_eq!(app.tabs.len(), 1);
        assert_eq!(app.tabs[0].conn, 1);
    }

    #[test]
    fn the_delete_project_dialog_lists_what_it_will_take() {
        let (path, mut app) = app_with_file();
        app.tree.cursor = 0;
        app.dispatch(Action::DeleteEntry);
        let Overlay::Confirm(Confirm::DeleteProject { name, connections }) = app.overlay.clone()
        else {
            panic!("expected a project confirmation")
        };
        assert_eq!(name, "Alpha");
        assert_eq!(connections, ["a-db"]);
        app.apply_confirmed(Confirm::DeleteProject { name, connections });
        let cfg = saved(&path);
        assert_eq!(cfg.projects.len(), 1);
        assert_eq!(cfg.projects[0].name, "Beta");
        assert!(app.conn(0).is_none());
        assert_eq!(app.conn(1).unwrap().cfg.name, "b-db");
    }

    #[test]
    fn declining_a_delete_changes_nothing() {
        let (path, mut app) = app_with_file();
        let before = std::fs::read_to_string(&path).unwrap();
        app.tree.cursor = 1;
        app.dispatch(Action::DeleteEntry);
        key(&mut app, "n");
        assert_eq!(app.overlay, Overlay::None);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        assert!(app.conn(0).is_some());
    }

    #[test]
    fn form_keys_navigate_edit_and_cancel() {
        let (_p, mut app) = app_with_file();
        app.dispatch(Action::AddConnection);
        // Type into the name field.
        key(&mut app, "enter");
        type_str(&mut app, "wh");
        key(&mut app, "esc"); // leave the field, not the form
        let Overlay::Form(form) = &app.overlay else {
            panic!("esc in a field must not close the form")
        };
        assert_eq!(form.name, "wh");
        assert!(!form.editing);
        // Move to engine and flip it.
        key(&mut app, "j");
        key(&mut app, "l");
        let Overlay::Form(form) = &app.overlay else {
            panic!()
        };
        assert_eq!(form.engine, crate::config::Engine::Mssql);
        assert_eq!(form.port, "1433");
        key(&mut app, "esc");
        assert_eq!(app.overlay, Overlay::None);
    }

    #[test]
    fn typing_over_a_prefilled_host_replaces_it() {
        let (_p, mut app) = app_with_file();
        app.dispatch(Action::AddConnection);
        key(&mut app, "j"); // name -> engine
        key(&mut app, "j"); // -> host
        key(&mut app, "enter");
        type_str(&mut app, "sql.internal");
        let Overlay::Form(form) = &app.overlay else {
            panic!()
        };
        assert_eq!(form.host, "sql.internal");
    }

    #[test]
    fn ctrl_u_clears_the_field_being_edited() {
        let (_p, mut app) = app_with_file();
        app.dispatch(Action::AddConnection);
        key(&mut app, "enter");
        type_str(&mut app, "oops");
        key(&mut app, "ctrl+u");
        type_str(&mut app, "good");
        let Overlay::Form(form) = &app.overlay else {
            panic!()
        };
        assert_eq!(form.name, "good");
    }

    #[test]
    fn a_typed_password_is_masked_and_never_written_to_the_document() {
        let (path, mut app) = app_with_file();
        app.dispatch(Action::AddConnection);
        let Overlay::Form(form) = &mut app.overlay else {
            panic!()
        };
        form.name = "secretive".into();
        form.database = "db".into();
        form.user = "usr".into();
        form.cursor = form
            .fields()
            .iter()
            .position(|f| *f == FieldId::Password)
            .unwrap();
        for c in "topsecret".chars() {
            form.type_char(c);
        }
        assert_eq!(form.display(FieldId::Password), "•".repeat(9));
        let conn = form.to_connection();
        assert_eq!(conn.keyring.as_deref(), Some("Alpha/secretive"));
        // Write the document only (the keyring write is exercised separately).
        crate::store::upsert_connection(&path, "Alpha", None, &conn).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(!raw.contains("topsecret"));
        assert!(raw.contains("keyring = \"Alpha/secretive\""));
    }

    #[test]
    fn test_results_land_in_an_open_form() {
        let (_p, mut app) = app_with_file();
        app.dispatch(Action::AddConnection);
        app.test_token = 5;
        app.on_test_outcome(TestOutcome {
            token: 5,
            result: Err("Login failed for user 'globex'.".into()),
            hints: vec!["check the login type".into()],
        });
        let Overlay::Form(form) = &app.overlay else {
            panic!()
        };
        match &form.test {
            TestState::Failed(msg, hints) => {
                assert!(msg.contains("Login failed"));
                assert_eq!(hints.len(), 1);
            }
            other => panic!("expected a failure, got {other:?}"),
        }
    }

    #[test]
    fn test_results_fall_back_to_the_status_line() {
        let (_p, mut app) = app_with_file();
        app.test_token = 2;
        app.on_test_outcome(TestOutcome {
            token: 2,
            result: Ok("ok — 1 row(s) returned by `select 1`".into()),
            hints: Vec::new(),
        });
        assert_eq!(app.message.as_ref().unwrap().0, MsgKind::Info);
        assert!(app.message.as_ref().unwrap().1.contains("ok"));
    }

    #[test]
    fn stale_test_results_are_ignored() {
        let (_p, mut app) = app_with_file();
        app.test_token = 9;
        app.on_test_outcome(TestOutcome {
            token: 3,
            result: Ok("stale".into()),
            hints: Vec::new(),
        });
        assert!(app.message.is_none());
    }

    #[test]
    fn starts_focused_on_the_tree() {
        let app = app();
        assert_eq!(app.focus(), Focus::Tree);
        assert_eq!(app.context(), Context::Tree);
    }

    #[test]
    fn escape_from_the_grid_returns_to_the_tree() {
        let mut app = app();
        open_fake_table(&mut app);
        assert_eq!(app.focus(), Focus::Grid);
        key(&mut app, "esc");
        assert_eq!(app.focus(), Focus::Tree, "esc must escape the grid");
    }

    #[test]
    fn ctrl_h_returns_to_the_tree_from_anywhere() {
        let mut app = app();
        open_fake_table(&mut app);
        key(&mut app, "ctrl+h");
        assert_eq!(app.focus(), Focus::Tree);

        app.dispatch(Action::NewQueryTab);
        assert_eq!(app.focus(), Focus::Editor);
        key(&mut app, "ctrl+h");
        assert_eq!(app.focus(), Focus::Tree);
    }

    #[test]
    fn escape_at_the_base_pane_is_never_a_dead_end() {
        let mut app = app();
        app.tree.filter = "x".into();
        key(&mut app, "esc");
        assert_eq!(app.focus(), Focus::Tree);
        assert!(app.tree.filter.is_empty());
        // And again with nothing left to shed.
        key(&mut app, "esc");
        assert_eq!(app.focus(), Focus::Tree);
        assert!(!app.quit);
    }

    #[test]
    fn q_in_the_grid_goes_back_rather_than_quitting() {
        let mut app = app();
        open_fake_table(&mut app);
        key(&mut app, "q");
        assert!(!app.quit, "q in the grid must not quit the app");
        assert_eq!(app.focus(), Focus::Tree);
    }

    #[test]
    fn escape_leaves_insert_mode_before_leaving_the_pane() {
        let mut app = app();
        app.dispatch(Action::NewQueryTab);
        assert_eq!(app.active_tab().unwrap().editor.mode, Mode::Insert);
        key(&mut app, "esc");
        assert_eq!(app.active_tab().unwrap().editor.mode, Mode::Normal);
        assert_eq!(app.focus(), Focus::Editor);
        key(&mut app, "esc");
        assert_eq!(app.focus(), Focus::Tree);
    }

    #[test]
    fn insert_mode_types_text_but_lets_global_chords_through() {
        let mut app = app();
        app.dispatch(Action::NewQueryTab);
        type_str(&mut app, "select 1");
        assert_eq!(app.active_tab().unwrap().editor.text(), "select 1");
        key(&mut app, "ctrl+h");
        assert_eq!(app.focus(), Focus::Tree);
    }

    #[test]
    fn two_key_sequences_resolve() {
        let mut app = app();
        open_fake_table(&mut app);
        app.tabs[0].view.row = 1;
        key(&mut app, "g");
        assert_eq!(app.pending.len(), 1);
        key(&mut app, "g");
        assert!(app.pending.is_empty());
        assert_eq!(app.tabs[0].view.row, 0);
    }

    #[test]
    fn unknown_sequence_clears_pending_without_acting() {
        let mut app = app();
        open_fake_table(&mut app);
        key(&mut app, "g");
        key(&mut app, "z");
        assert!(app.pending.is_empty());
        assert_eq!(app.tabs[0].view.row, 0);
    }

    #[test]
    fn help_overlay_scrolls_and_closes() {
        let mut app = app();
        key(&mut app, "?");
        assert_eq!(app.overlay, Overlay::Help);
        key(&mut app, "j");
        key(&mut app, "j");
        assert_eq!(app.help_scroll, 2);
        key(&mut app, "k");
        assert_eq!(app.help_scroll, 1);
        key(&mut app, "esc");
        assert_eq!(app.overlay, Overlay::None);
        assert_eq!(app.help_scroll, 0);
    }

    #[test]
    fn tree_movement_and_filter_prompt() {
        let mut app = app();
        key(&mut app, "j");
        assert_eq!(app.tree.selected_row().unwrap().name, "a-db");
        key(&mut app, "/");
        assert_eq!(app.overlay, Overlay::Prompt(PromptKind::Filter));
        key(&mut app, "b");
        assert_eq!(app.tree.filter, "b");
        key(&mut app, "esc");
        assert_eq!(app.overlay, Overlay::None);
        assert!(app.tree.filter.is_empty());
    }

    #[test]
    fn cycling_to_a_data_tab_does_not_leave_focus_on_a_missing_editor() {
        let mut app = app();
        open_fake_table(&mut app);
        app.dispatch(Action::NewQueryTab);
        assert_eq!(app.focus(), Focus::Editor);
        app.dispatch(Action::NextTab);
        assert!(!app.active_tab().unwrap().has_editor());
        assert_eq!(app.focus(), Focus::Grid);
    }

    #[test]
    fn closing_the_last_tab_returns_focus_to_the_tree() {
        let mut app = app();
        open_fake_table(&mut app);
        app.dispatch(Action::CloseTab);
        assert!(app.tabs.is_empty());
        assert_eq!(app.focus(), Focus::Tree);
    }

    #[test]
    fn writes_ask_for_confirmation_once() {
        let mut app = app();
        app.dispatch(Action::NewQueryTab);
        app.with_editor(|e| e.set_text("delete from users"));
        app.dispatch(Action::RunAll);
        assert!(matches!(app.overlay, Overlay::Confirm(_)));
        key(&mut app, "n");
        assert_eq!(app.overlay, Overlay::None);
        assert!(!app.is_busy(), "declining must not run the statement");
    }

    #[tokio::test]
    async fn reads_run_without_confirmation() {
        let mut app = app();
        app.dispatch(Action::NewQueryTab);
        app.with_editor(|e| e.set_text("select 1"));
        app.dispatch(Action::RunAll);
        assert_eq!(app.overlay, Overlay::None);
        assert!(app.is_busy());
        assert_eq!(app.focus(), Focus::Grid);
    }

    #[tokio::test]
    async fn run_statement_uses_the_statement_under_the_cursor() {
        let mut app = app();
        app.dispatch(Action::NewQueryTab);
        app.with_editor(|e| e.set_text("select 1;\nselect 2"));
        app.with_editor(|e| {
            e.top();
        });
        app.dispatch(Action::RunStatement);
        assert_eq!(app.active_tab().unwrap().last_sql, "select 1");
    }

    #[test]
    fn commands_are_parsed() {
        let mut app = app();
        app.run_command("limit 42");
        assert_eq!(app.ui.page_size, 42);
        app.run_command("limit nope");
        assert_eq!(app.message.as_ref().unwrap().0, MsgKind::Error);
        app.run_command("bogus");
        assert!(app.message.as_ref().unwrap().1.contains("unknown command"));
        app.run_command("q");
        assert!(app.quit);
    }

    #[test]
    fn results_land_in_the_tab_that_asked() {
        let mut app = app();
        open_fake_table(&mut app);
        app.inflight.insert(7, 0);
        app.tabs[0].status = TabStatus::Running(7);
        app.on_db_event(DbEvent {
            conn: 0,
            payload: DbPayload::Result {
                req: 7,
                grid: fake_grid(),
                sql: Some("select * from users".into()),
            },
        });
        assert_eq!(app.tabs[0].status, TabStatus::Idle);
        assert_eq!(app.tabs[0].view.rows(), 2);
        assert_eq!(app.tabs[0].last_sql, "select * from users");
        assert!(!app.is_busy());
    }

    #[test]
    fn stale_results_are_dropped() {
        let mut app = app();
        open_fake_table(&mut app);
        app.tabs[0].status = TabStatus::Running(9);
        app.on_db_event(DbEvent {
            conn: 0,
            payload: DbPayload::Result {
                req: 1,
                grid: fake_grid(),
                sql: None,
            },
        });
        assert_eq!(app.tabs[0].status, TabStatus::Running(9));
    }

    #[test]
    fn errors_surface_on_the_tab_and_the_status_line() {
        let mut app = app();
        open_fake_table(&mut app);
        app.inflight.insert(3, 0);
        app.on_db_event(DbEvent {
            conn: 0,
            payload: DbPayload::Error {
                req: Some(3),
                message: "syntax error".into(),
            },
        });
        assert_eq!(
            app.tabs[0].status,
            TabStatus::Error("syntax error".to_string())
        );
        assert_eq!(app.message.as_ref().unwrap().0, MsgKind::Error);
        assert!(app.message.as_ref().unwrap().1.contains("Alpha/a-db"));
    }

    #[test]
    fn paging_is_rejected_outside_data_tabs() {
        let mut app = app();
        app.dispatch(Action::NewQueryTab);
        app.dispatch(Action::NextPage);
        assert_eq!(app.message.as_ref().unwrap().0, MsgKind::Error);
    }

    #[test]
    fn history_walks_backwards_then_forwards() {
        let mut app = app();
        app.dispatch(Action::NewQueryTab);
        app.history = vec!["select 1".into(), "select 2".into()];
        app.dispatch(Action::HistoryPrev);
        assert_eq!(app.active_tab().unwrap().editor.text(), "select 2");
        app.dispatch(Action::HistoryPrev);
        assert_eq!(app.active_tab().unwrap().editor.text(), "select 1");
        app.dispatch(Action::HistoryNext);
        assert_eq!(app.active_tab().unwrap().editor.text(), "select 2");
    }

    #[tokio::test]
    async fn tree_open_on_a_connection_requests_schemas() {
        let mut app = app();
        key(&mut app, "j");
        key(&mut app, "enter");
        let path = app.tree.connection_path(0).unwrap();
        assert!(app.tree.node(&path).unwrap().loading);
        assert_eq!(app.tree.node(&path).unwrap().state, ConnState::Connecting);
    }

    #[test]
    fn schemas_event_populates_and_expands() {
        let mut app = app();
        app.on_db_event(DbEvent {
            conn: 0,
            payload: DbPayload::Schemas(vec!["public".into()]),
        });
        let names: Vec<String> = app.tree.rows().iter().map(|r| r.name.clone()).collect();
        assert!(names.contains(&"public".to_string()));
    }

    /// Teaches the tree what a connection holds, the way a schema and table
    /// load would.
    fn load_fake_metadata(app: &mut App) {
        app.tree.set_schemas(0, vec!["public".into()]);
        app.tree.set_tables(0, "public", vec!["users".into()]);
    }

    fn texts(c: &Completion) -> Vec<&str> {
        c.items.iter().map(|i| i.text.as_str()).collect()
    }

    fn completion(app: &App) -> &Completion {
        app.active_tab()
            .unwrap()
            .completion
            .as_ref()
            .expect("popup should be open")
    }

    #[test]
    fn completion_offers_loaded_tables_and_accepts_one() {
        let mut app = app();
        load_fake_metadata(&mut app);
        app.dispatch(Action::NewQueryTab);
        type_str(&mut app, "select * from us");
        key(&mut app, "ctrl+space");
        assert_eq!(completion(&app).items[0].text, "users");
        key(&mut app, "enter");
        let tab = app.active_tab().unwrap();
        assert_eq!(tab.editor.text(), "select * from users");
        assert!(tab.completion.is_none());
        assert_eq!(tab.editor.mode, Mode::Insert);
    }

    #[test]
    fn typing_narrows_the_popup_and_esc_closes_it_without_leaving_insert() {
        let mut app = app();
        load_fake_metadata(&mut app);
        app.dispatch(Action::NewQueryTab);
        type_str(&mut app, "select ");
        key(&mut app, "ctrl+space");
        type_str(&mut app, "fro");
        assert_eq!(completion(&app).items[0].text, "from");
        assert_eq!(app.active_tab().unwrap().editor.text(), "select fro");
        key(&mut app, "esc");
        let tab = app.active_tab().unwrap();
        assert!(tab.completion.is_none());
        assert_eq!(tab.editor.mode, Mode::Insert, "esc closes the popup only");
    }

    #[test]
    fn columns_from_an_opened_table_feed_the_popup() {
        let mut app = app();
        load_fake_metadata(&mut app);
        open_fake_table(&mut app);
        app.inflight.insert(9, 0);
        app.on_db_event(DbEvent {
            conn: 0,
            payload: DbPayload::Result {
                req: 9,
                grid: fake_grid(),
                sql: None,
            },
        });
        app.dispatch(Action::NewQueryTab);
        type_str(&mut app, "select users.i");
        key(&mut app, "ctrl+space");
        assert_eq!(completion(&app).items[0].text, "id");
    }

    #[test]
    fn triggering_from_normal_mode_completes_the_word_under_the_cursor() {
        let mut app = app();
        load_fake_metadata(&mut app);
        app.dispatch(Action::NewQueryTab);
        type_str(&mut app, "select * from use");
        key(&mut app, "esc");
        key(&mut app, "ctrl+space");
        key(&mut app, "enter");
        assert_eq!(
            app.active_tab().unwrap().editor.text(),
            "select * from users"
        );
    }

    #[test]
    fn nothing_to_complete_says_so_and_opens_no_popup() {
        let mut app = app();
        app.dispatch(Action::NewQueryTab);
        type_str(&mut app, "zzz");
        key(&mut app, "ctrl+space");
        assert!(app.active_tab().unwrap().completion.is_none());
        assert_eq!(app.message.as_ref().unwrap().0, MsgKind::Info);
    }

    #[test]
    fn an_open_popup_never_traps_a_global_chord() {
        let mut app = app();
        load_fake_metadata(&mut app);
        app.dispatch(Action::NewQueryTab);
        type_str(&mut app, "select ");
        key(&mut app, "ctrl+space");
        key(&mut app, "ctrl+h");
        assert_eq!(app.focus(), Focus::Tree);
        assert!(app.tabs[0].completion.is_none());
    }

    #[test]
    fn the_popup_follows_typing() {
        let mut app = app();
        load_fake_metadata(&mut app);
        app.dispatch(Action::NewQueryTab);
        type_str(&mut app, "s");
        assert!(
            texts(completion(&app)).contains(&"select"),
            "one character should be enough"
        );
        type_str(&mut app, "elect * from us");
        assert_eq!(completion(&app).items[0].text, "users");
        key(&mut app, "enter");
        assert_eq!(
            app.active_tab().unwrap().editor.text(),
            "select * from users"
        );
    }

    #[test]
    fn auto_complete_can_be_turned_off() {
        let mut app = app();
        load_fake_metadata(&mut app);
        app.ui.auto_complete = false;
        app.dispatch(Action::NewQueryTab);
        type_str(&mut app, "sel");
        assert!(app.active_tab().unwrap().completion.is_none());
        // The chord still works with auto-open off.
        key(&mut app, "ctrl+p");
        assert!(texts(completion(&app)).contains(&"select"));
    }

    #[test]
    fn complete_after_holds_the_popup_back() {
        let mut app = app();
        app.ui.complete_after = 3;
        app.dispatch(Action::NewQueryTab);
        type_str(&mut app, "se");
        assert!(app.active_tab().unwrap().completion.is_none());
        type_str(&mut app, "l");
        assert!(texts(completion(&app)).contains(&"select"));
    }

    #[test]
    fn a_dot_opens_the_popup_whatever_the_threshold() {
        let mut app = app();
        load_fake_metadata(&mut app);
        app.ui.complete_after = 99;
        app.dispatch(Action::NewQueryTab);
        type_str(&mut app, "select * from public");
        assert!(app.active_tab().unwrap().completion.is_none());
        type_str(&mut app, ".");
        assert_eq!(texts(completion(&app)), vec!["users"]);
    }

    #[test]
    fn typing_a_string_literal_never_opens_the_popup() {
        let mut app = app();
        load_fake_metadata(&mut app);
        app.dispatch(Action::NewQueryTab);
        type_str(&mut app, "select 'sel");
        assert!(app.active_tab().unwrap().completion.is_none());
        type_str(&mut app, "' , sel");
        assert!(texts(completion(&app)).contains(&"select"));
    }

    fn picker(app: &App) -> &Picker {
        let Overlay::Picker(p) = &app.overlay else {
            panic!("picker should be open, got {:?}", app.overlay);
        };
        p
    }

    /// The database list a server would have sent.
    fn databases_arrive(app: &mut App, conn: ConnId, names: &[&str]) {
        app.on_db_event(DbEvent {
            conn,
            payload: DbPayload::Databases(names.iter().map(|s| s.to_string()).collect()),
        });
    }

    #[tokio::test]
    async fn listing_databases_clones_the_connection_once_per_database() {
        let mut app = app();
        databases_arrive(&mut app, 0, &["d", "reporting", "staging"]);
        assert_eq!(app.tree.databases(0), ["d", "reporting", "staging"]);
        // The configured database keeps the connection's own id; the others
        // get session-only clones.
        assert_eq!(app.conn_for_database(0, "d"), Some(0));
        let reporting = app.conn_for_database(0, "reporting").unwrap();
        assert_ne!(reporting, 0);
        let entry = app.conn(reporting).unwrap();
        assert_eq!(entry.cfg.database, "reporting");
        assert_eq!(entry.cfg.host, "h", "the server details are inherited");
        assert_eq!(entry.origin, Some(0));

        // Relisting reuses them instead of piling up.
        let before = app.conns.len();
        databases_arrive(&mut app, 0, &["d", "reporting", "staging"]);
        assert_eq!(app.conns.len(), before);
        assert_eq!(app.conn_for_database(0, "reporting"), Some(reporting));
    }

    #[tokio::test]
    async fn the_picker_filters_and_retargets_the_query_tab() {
        let mut app = app();
        databases_arrive(&mut app, 0, &["d", "reporting", "staging"]);
        app.dispatch(Action::NewQueryTab);
        key(&mut app, "ctrl+b");
        assert_eq!(picker(&app).items.len(), 3);
        assert!(!picker(&app).loading, "the tree already had the list");
        type_str(&mut app, "rep");
        assert_eq!(picker(&app).visible(), ["reporting"]);
        key(&mut app, "enter");
        assert_eq!(app.overlay, Overlay::None);
        let tab = app.active_tab().unwrap();
        assert_eq!(app.database_of(tab.conn), Some("reporting"));
        assert_eq!(app.base_conn(tab.conn), 0);
    }

    #[tokio::test]
    async fn the_picker_asks_the_server_when_the_tree_has_no_list() {
        let mut app = app();
        app.dispatch(Action::NewQueryTab);
        key(&mut app, "ctrl+b");
        assert!(picker(&app).loading);
        databases_arrive(&mut app, 0, &["d", "reporting"]);
        assert!(!picker(&app).loading);
        assert_eq!(picker(&app).items, ["d", "reporting"]);
        key(&mut app, "esc");
        assert_eq!(app.overlay, Overlay::None);
    }

    #[tokio::test]
    async fn db_command_switches_without_the_picker() {
        let mut app = app();
        databases_arrive(&mut app, 0, &["d", "reporting"]);
        app.dispatch(Action::NewQueryTab);
        app.run_command("db reporting");
        assert_eq!(
            app.database_of(app.active_tab().unwrap().conn),
            Some("reporting")
        );
    }

    #[test]
    fn switching_databases_is_refused_on_a_table_tab() {
        let mut app = app();
        open_fake_table(&mut app);
        app.dispatch(Action::SwitchDatabase);
        assert_eq!(app.overlay, Overlay::None);
        assert_eq!(app.message.as_ref().unwrap().0, MsgKind::Error);
    }

    #[tokio::test]
    async fn each_tab_keeps_its_own_database() {
        let mut app = app();
        databases_arrive(&mut app, 0, &["d", "reporting"]);
        app.dispatch(Action::NewQueryTab);
        app.dispatch(Action::NewQueryTab);
        app.run_command("db reporting");
        assert_eq!(app.database_of(app.tabs[0].conn), Some("d"));
        assert_eq!(app.database_of(app.tabs[1].conn), Some("reporting"));
        // A new tab opens on whatever the current one is pointed at.
        app.dispatch(Action::NewQueryTab);
        assert_eq!(app.database_of(app.tabs[2].conn), Some("reporting"));
    }

    #[tokio::test]
    async fn clones_stay_out_of_the_config_and_go_with_their_connection() {
        let (_p, mut app) = app_with_file();
        databases_arrive(&mut app, 0, &["d", "reporting"]);
        app.dispatch(Action::NewQueryTab);
        app.run_command("db reporting");
        let clone = app.active_tab().unwrap().conn;
        assert_eq!(
            app.existing_pairs()
                .iter()
                .filter(|(_, name)| name == "a-db")
                .count(),
            1,
            "a clone must not double up the connection's name"
        );
        app.apply_confirmed(Confirm::DeleteConnection {
            id: 0,
            project: "Alpha".into(),
            name: "a-db".into(),
            account: None,
        });
        assert!(app.conn(clone).is_none(), "the clone went with it");
        assert!(app.tabs.iter().all(|t| t.conn != clone));
    }
}
