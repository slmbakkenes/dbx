//! Keymap: parsing, sequences, and the default vim-flavoured bindings.
//!
//! Every binding is user-overridable from `keys.toml` in the config dir.

use anyhow::{Result, anyhow};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    // Global
    Quit,
    Help,
    CancelQuery,
    Refresh,
    CommandMode,
    Escape,
    PaneLeft,
    PaneRight,
    PaneUp,
    PaneDown,
    FocusTree,
    FocusEditor,
    FocusGrid,
    NextTab,
    PrevTab,
    CloseTab,
    NewQueryTab,
    ToggleFullscreen,
    // Shared movement
    Down,
    Up,
    Left,
    Right,
    HalfDown,
    HalfUp,
    Top,
    Bottom,
    // Tree
    Expand,
    Collapse,
    Toggle,
    Open,
    OpenStructure,
    Filter,
    NextConnection,
    PrevConnection,
    AddConnection,
    AddProject,
    EditEntry,
    DeleteEntry,
    TestConnection,
    // Grid
    RowStart,
    RowEnd,
    NextPage,
    PrevPage,
    YankCell,
    YankRow,
    Inspect,
    Search,
    SearchNext,
    SearchPrev,
    // Editor
    InsertMode,
    InsertAfter,
    InsertLineStart,
    AppendLineEnd,
    OpenLineBelow,
    OpenLineAbove,
    NormalMode,
    RunStatement,
    RunAll,
    DeleteLine,
    DeleteChar,
    Undo,
    WordForward,
    WordBack,
    HistoryPrev,
    HistoryNext,
}

impl Action {
    pub fn name(self) -> &'static str {
        use Action::*;
        match self {
            Quit => "quit",
            Help => "help",
            CancelQuery => "cancel-query",
            Refresh => "refresh",
            CommandMode => "command-mode",
            Escape => "escape",
            PaneLeft => "pane-left",
            PaneRight => "pane-right",
            PaneUp => "pane-up",
            PaneDown => "pane-down",
            FocusTree => "focus-tree",
            FocusEditor => "focus-editor",
            FocusGrid => "focus-grid",
            NextTab => "next-tab",
            PrevTab => "prev-tab",
            CloseTab => "close-tab",
            NewQueryTab => "new-query-tab",
            ToggleFullscreen => "toggle-fullscreen",
            Down => "down",
            Up => "up",
            Left => "left",
            Right => "right",
            HalfDown => "half-down",
            HalfUp => "half-up",
            Top => "top",
            Bottom => "bottom",
            Expand => "expand",
            Collapse => "collapse",
            Toggle => "toggle",
            Open => "open",
            OpenStructure => "open-structure",
            Filter => "filter",
            NextConnection => "next-connection",
            PrevConnection => "prev-connection",
            AddConnection => "add-connection",
            AddProject => "add-project",
            EditEntry => "edit-entry",
            DeleteEntry => "delete-entry",
            TestConnection => "test-connection",
            RowStart => "row-start",
            RowEnd => "row-end",
            NextPage => "next-page",
            PrevPage => "prev-page",
            YankCell => "yank-cell",
            YankRow => "yank-row",
            Inspect => "inspect",
            Search => "search",
            SearchNext => "search-next",
            SearchPrev => "search-prev",
            InsertMode => "insert-mode",
            InsertAfter => "insert-after",
            InsertLineStart => "insert-line-start",
            AppendLineEnd => "append-line-end",
            OpenLineBelow => "open-line-below",
            OpenLineAbove => "open-line-above",
            NormalMode => "normal-mode",
            RunStatement => "run-statement",
            RunAll => "run-all",
            DeleteLine => "delete-line",
            DeleteChar => "delete-char",
            Undo => "undo",
            WordForward => "word-forward",
            WordBack => "word-back",
            HistoryPrev => "history-prev",
            HistoryNext => "history-next",
        }
    }

    pub fn all() -> &'static [Action] {
        use Action::*;
        &[
            Quit,
            Help,
            CancelQuery,
            Refresh,
            CommandMode,
            Escape,
            PaneLeft,
            PaneRight,
            PaneUp,
            PaneDown,
            FocusTree,
            FocusEditor,
            FocusGrid,
            NextTab,
            PrevTab,
            CloseTab,
            NewQueryTab,
            ToggleFullscreen,
            Down,
            Up,
            Left,
            Right,
            HalfDown,
            HalfUp,
            Top,
            Bottom,
            Expand,
            Collapse,
            Toggle,
            Open,
            OpenStructure,
            Filter,
            NextConnection,
            PrevConnection,
            AddConnection,
            AddProject,
            EditEntry,
            DeleteEntry,
            TestConnection,
            RowStart,
            RowEnd,
            NextPage,
            PrevPage,
            YankCell,
            YankRow,
            Inspect,
            Search,
            SearchNext,
            SearchPrev,
            InsertMode,
            InsertAfter,
            InsertLineStart,
            AppendLineEnd,
            OpenLineBelow,
            OpenLineAbove,
            NormalMode,
            RunStatement,
            RunAll,
            DeleteLine,
            DeleteChar,
            Undo,
            WordForward,
            WordBack,
            HistoryPrev,
            HistoryNext,
        ]
    }
}

impl FromStr for Action {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        let want = s.trim().to_ascii_lowercase().replace('_', "-");
        Action::all()
            .iter()
            .copied()
            .find(|a| a.name() == want)
            .ok_or_else(|| anyhow!("unknown action `{s}`"))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct KeySpec {
    pub code: KeyCode,
    pub mods: KeyModifiers,
}

impl KeySpec {
    pub fn from_event(ev: &KeyEvent) -> Self {
        let mut mods = ev.modifiers;
        // A shifted character already carries its case in `code`; keeping the
        // SHIFT bit would stop `G` from matching a `G` binding.
        if let KeyCode::Char(_) = ev.code {
            mods.remove(KeyModifiers::SHIFT);
        }
        mods.remove(KeyModifiers::NONE);
        Self {
            code: ev.code,
            mods,
        }
    }

    pub fn render(&self) -> String {
        let mut s = String::new();
        if self.mods.contains(KeyModifiers::CONTROL) {
            s.push_str("ctrl+");
        }
        if self.mods.contains(KeyModifiers::ALT) {
            s.push_str("alt+");
        }
        if self.mods.contains(KeyModifiers::SHIFT) {
            s.push_str("shift+");
        }
        s.push_str(&match self.code {
            KeyCode::Char(' ') => "space".to_string(),
            KeyCode::Char(c) => c.to_string(),
            KeyCode::Esc => "esc".to_string(),
            KeyCode::Enter => "enter".to_string(),
            KeyCode::Tab => "tab".to_string(),
            KeyCode::BackTab => "backtab".to_string(),
            KeyCode::Backspace => "backspace".to_string(),
            KeyCode::Delete => "delete".to_string(),
            KeyCode::Left => "left".to_string(),
            KeyCode::Right => "right".to_string(),
            KeyCode::Up => "up".to_string(),
            KeyCode::Down => "down".to_string(),
            KeyCode::Home => "home".to_string(),
            KeyCode::End => "end".to_string(),
            KeyCode::PageUp => "pageup".to_string(),
            KeyCode::PageDown => "pagedown".to_string(),
            KeyCode::F(n) => format!("f{n}"),
            other => format!("{other:?}").to_ascii_lowercase(),
        });
        s
    }
}

/// Parses `"ctrl+h"`, `"G"`, `"esc"`, `"space"`, `"f5"`.
pub fn parse_key(spec: &str) -> Result<KeySpec> {
    let mut mods = KeyModifiers::NONE;
    let mut rest = spec.trim();
    loop {
        let lower = rest.to_ascii_lowercase();
        let stripped = if lower.starts_with("ctrl+") || lower.starts_with("c-") {
            mods |= KeyModifiers::CONTROL;
            Some(&rest[rest.find(['+', '-']).unwrap() + 1..])
        } else if lower.starts_with("alt+") || lower.starts_with("a-") || lower.starts_with("m-") {
            mods |= KeyModifiers::ALT;
            Some(&rest[rest.find(['+', '-']).unwrap() + 1..])
        } else if lower.starts_with("shift+") || lower.starts_with("s-") {
            mods |= KeyModifiers::SHIFT;
            Some(&rest[rest.find(['+', '-']).unwrap() + 1..])
        } else {
            None
        };
        match stripped {
            Some(r) => rest = r,
            None => break,
        }
    }
    if rest.is_empty() {
        return Err(anyhow!("empty key spec `{spec}`"));
    }
    let code = match rest.to_ascii_lowercase().as_str() {
        "esc" | "escape" => KeyCode::Esc,
        "enter" | "cr" | "return" => KeyCode::Enter,
        "tab" => KeyCode::Tab,
        "backtab" | "shift+tab" => KeyCode::BackTab,
        "backspace" | "bs" => KeyCode::Backspace,
        "delete" | "del" => KeyCode::Delete,
        "space" => KeyCode::Char(' '),
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pageup" | "pgup" => KeyCode::PageUp,
        "pagedown" | "pgdn" => KeyCode::PageDown,
        _ => {
            let mut chars = rest.chars();
            let first = chars.next().unwrap();
            if rest.len() > 1 && (first == 'f' || first == 'F') && rest[1..].parse::<u8>().is_ok() {
                KeyCode::F(rest[1..].parse().unwrap())
            } else if chars.next().is_none() {
                if mods.contains(KeyModifiers::SHIFT) && first.is_ascii_lowercase() {
                    mods.remove(KeyModifiers::SHIFT);
                    KeyCode::Char(first.to_ascii_uppercase())
                } else {
                    KeyCode::Char(first)
                }
            } else {
                return Err(anyhow!("unrecognised key `{rest}` in `{spec}`"));
            }
        }
    };
    if let KeyCode::Char(_) = code {
        mods.remove(KeyModifiers::SHIFT);
    }
    Ok(KeySpec { code, mods })
}

/// Parses `"g g"` or `"gg"` into a sequence of at most two keys.
pub fn parse_sequence(spec: &str) -> Result<Vec<KeySpec>> {
    let spec = spec.trim();
    if spec.contains(' ') {
        return spec.split_whitespace().map(parse_key).collect();
    }
    // A single key wins ("f5", "$"); only then is a two-character spec a
    // sequence ("gg", "dd").
    if let Ok(k) = parse_key(spec) {
        return Ok(vec![k]);
    }
    if spec.chars().count() == 2 && spec.chars().all(|c| c.is_ascii_alphanumeric()) {
        return spec.chars().map(|c| parse_key(&c.to_string())).collect();
    }
    Err(anyhow!("unrecognised key sequence `{spec}`"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Context {
    Global,
    Tree,
    Grid,
    Editor,
}

#[derive(Debug, Clone, Default)]
pub struct Keymap {
    bindings: BTreeMap<Context, Vec<(Vec<KeySpec>, Action)>>,
}

#[derive(Debug, PartialEq)]
pub enum Lookup {
    /// Exact match.
    Run(Action),
    /// A longer sequence starts with this key; wait for the next one.
    Pending,
    None,
}

impl Keymap {
    pub fn insert(&mut self, ctx: Context, seq: Vec<KeySpec>, action: Action) {
        let list = self.bindings.entry(ctx).or_default();
        list.retain(|(s, _)| s != &seq);
        list.push((seq, action));
    }

    /// Resolves `pending + key` in `ctx`, falling back to `Global`.
    pub fn lookup(&self, ctx: Context, pending: &[KeySpec], key: KeySpec) -> Lookup {
        let mut seq = pending.to_vec();
        seq.push(key);
        let mut pending_hit = false;
        for c in [ctx, Context::Global] {
            let Some(list) = self.bindings.get(&c) else {
                continue;
            };
            for (bind, action) in list {
                if bind == &seq {
                    return Lookup::Run(*action);
                }
                if bind.len() > seq.len() && bind.starts_with(&seq) {
                    pending_hit = true;
                }
            }
        }
        if pending_hit {
            Lookup::Pending
        } else {
            Lookup::None
        }
    }

    pub fn entries(&self, ctx: Context) -> Vec<(String, Action)> {
        self.bindings
            .get(&ctx)
            .map(|l| {
                l.iter()
                    .map(|(s, a)| {
                        (
                            s.iter().map(|k| k.render()).collect::<Vec<_>>().join(" "),
                            *a,
                        )
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
}

#[derive(Debug, Deserialize, Default)]
pub struct KeymapFile {
    #[serde(default)]
    pub global: BTreeMap<String, String>,
    #[serde(default)]
    pub tree: BTreeMap<String, String>,
    #[serde(default)]
    pub grid: BTreeMap<String, String>,
    #[serde(default)]
    pub editor: BTreeMap<String, String>,
}

pub fn default_keymap() -> Keymap {
    use Action::*;
    use Context::*;
    let mut km = Keymap::default();
    let defaults: &[(Context, &str, Action)] = &[
        // Global — these work from every pane, so no pane is ever a dead end.
        (Global, "ctrl+c", CancelQuery),
        (Global, "ctrl+q", Quit),
        (Global, "?", Help),
        (Global, ":", CommandMode),
        (Global, "esc", Escape),
        (Global, "ctrl+h", PaneLeft),
        (Global, "ctrl+l", PaneRight),
        (Global, "ctrl+j", PaneDown),
        (Global, "ctrl+k", PaneUp),
        (Global, "ctrl+t", FocusTree),
        (Global, "ctrl+e", FocusEditor),
        (Global, "ctrl+g", FocusGrid),
        (Global, "ctrl+n", NewQueryTab),
        (Global, "ctrl+w", CloseTab),
        (Global, "ctrl+f", ToggleFullscreen),
        (Global, "f5", Refresh),
        (Global, "alt+l", NextTab),
        (Global, "alt+h", PrevTab),
        // Tree
        (Tree, "j", Down),
        (Tree, "k", Up),
        (Tree, "down", Down),
        (Tree, "up", Up),
        (Tree, "l", Expand),
        (Tree, "h", Collapse),
        (Tree, "right", Expand),
        (Tree, "left", Collapse),
        (Tree, "space", Toggle),
        (Tree, "enter", Open),
        (Tree, "s", OpenStructure),
        (Tree, "gg", Top),
        (Tree, "G", Bottom),
        (Tree, "ctrl+d", HalfDown),
        (Tree, "ctrl+u", HalfUp),
        (Tree, "/", Filter),
        (Tree, "q", Quit),
        (Tree, "r", Refresh),
        (Tree, "J", NextConnection),
        (Tree, "K", PrevConnection),
        (Tree, "a", AddConnection),
        (Tree, "A", AddProject),
        (Tree, "e", EditEntry),
        (Tree, "D", DeleteEntry),
        (Tree, "t", TestConnection),
        // Grid
        (Grid, "j", Down),
        (Grid, "k", Up),
        (Grid, "h", Left),
        (Grid, "l", Right),
        (Grid, "down", Down),
        (Grid, "up", Up),
        (Grid, "left", Left),
        (Grid, "right", Right),
        (Grid, "0", RowStart),
        (Grid, "$", RowEnd),
        (Grid, "gg", Top),
        (Grid, "G", Bottom),
        (Grid, "ctrl+d", HalfDown),
        (Grid, "ctrl+u", HalfUp),
        (Grid, "pagedown", NextPage),
        (Grid, "pageup", PrevPage),
        (Grid, "L", NextPage),
        (Grid, "H", PrevPage),
        (Grid, "y", YankCell),
        (Grid, "Y", YankRow),
        (Grid, "enter", Inspect),
        (Grid, "/", Search),
        (Grid, "n", SearchNext),
        (Grid, "N", SearchPrev),
        (Grid, "q", FocusTree),
        (Grid, "r", Refresh),
        // Editor (normal mode; insert mode takes raw input)
        (Editor, "i", InsertMode),
        (Editor, "a", InsertAfter),
        (Editor, "I", InsertLineStart),
        (Editor, "A", AppendLineEnd),
        (Editor, "o", OpenLineBelow),
        (Editor, "O", OpenLineAbove),
        (Editor, "j", Down),
        (Editor, "k", Up),
        (Editor, "h", Left),
        (Editor, "l", Right),
        (Editor, "down", Down),
        (Editor, "up", Up),
        (Editor, "left", Left),
        (Editor, "right", Right),
        (Editor, "w", WordForward),
        (Editor, "b", WordBack),
        (Editor, "0", RowStart),
        (Editor, "$", RowEnd),
        (Editor, "gg", Top),
        (Editor, "G", Bottom),
        (Editor, "dd", DeleteLine),
        (Editor, "x", DeleteChar),
        (Editor, "u", Undo),
        (Editor, "ctrl+r", RunStatement),
        (Editor, "ctrl+a", RunAll),
        (Editor, "enter", RunStatement),
        (Editor, "[", HistoryPrev),
        (Editor, "]", HistoryNext),
    ];
    for (ctx, spec, action) in defaults {
        let seq = parse_sequence(spec).expect("default keymap must parse");
        km.insert(*ctx, seq, *action);
    }
    km
}

/// Applies user overrides on top of the defaults. Errors are collected, not fatal.
pub fn apply_overrides(km: &mut Keymap, file: &KeymapFile) -> Vec<String> {
    let mut errs = Vec::new();
    let groups: [(Context, &BTreeMap<String, String>); 4] = [
        (Context::Global, &file.global),
        (Context::Tree, &file.tree),
        (Context::Grid, &file.grid),
        (Context::Editor, &file.editor),
    ];
    for (ctx, map) in groups {
        for (spec, action) in map {
            match (parse_sequence(spec), action.parse::<Action>()) {
                (Ok(seq), Ok(a)) => km.insert(ctx, seq, a),
                (Err(e), _) | (_, Err(e)) => errs.push(format!("{spec} = {action}: {e}")),
            }
        }
    }
    errs
}

pub fn load_keymap() -> (Keymap, Vec<String>) {
    let mut km = default_keymap();
    let mut errs = Vec::new();
    if let Ok(dir) = crate::config::config_dir() {
        let path = dir.join("keys.toml");
        if path.exists() {
            match std::fs::read_to_string(&path) {
                Ok(raw) => match toml::from_str::<KeymapFile>(&raw) {
                    Ok(file) => errs.extend(apply_overrides(&mut km, &file)),
                    Err(e) => errs.push(format!("keys.toml: {e}")),
                },
                Err(e) => errs.push(format!("keys.toml: {e}")),
            }
        }
    }
    (km, errs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(s: &str) -> KeySpec {
        parse_key(s).unwrap()
    }

    #[test]
    fn parses_modifiers() {
        assert_eq!(
            k("ctrl+h"),
            KeySpec {
                code: KeyCode::Char('h'),
                mods: KeyModifiers::CONTROL
            }
        );
        assert_eq!(k("c-h"), k("ctrl+h"));
        assert_eq!(k("esc").code, KeyCode::Esc);
        assert_eq!(k("f5").code, KeyCode::F(5));
        assert_eq!(k("space").code, KeyCode::Char(' '));
    }

    #[test]
    fn shift_letter_normalises_to_uppercase_char() {
        assert_eq!(k("shift+g"), k("G"));
        assert_eq!(k("G").mods, KeyModifiers::NONE);
    }

    #[test]
    fn event_normalisation_matches_binding() {
        let ev = KeyEvent::new(KeyCode::Char('G'), KeyModifiers::SHIFT);
        assert_eq!(KeySpec::from_event(&ev), k("G"));
    }

    #[test]
    fn parses_sequences() {
        assert_eq!(parse_sequence("gg").unwrap(), vec![k("g"), k("g")]);
        assert_eq!(parse_sequence("g g").unwrap(), vec![k("g"), k("g")]);
        assert_eq!(parse_sequence("$").unwrap(), vec![k("$")]);
        assert_eq!(parse_sequence("ctrl+d").unwrap(), vec![k("ctrl+d")]);
        // A function key is one key, not the sequence f,5.
        assert_eq!(parse_sequence("f5").unwrap(), vec![k("f5")]);
        assert!(parse_sequence("nope").is_err());
    }

    #[test]
    fn connection_management_is_bound_in_the_tree() {
        let km = default_keymap();
        for (spec, want) in [
            ("a", Action::AddConnection),
            ("A", Action::AddProject),
            ("e", Action::EditEntry),
            ("D", Action::DeleteEntry),
            ("t", Action::TestConnection),
        ] {
            assert_eq!(km.lookup(Context::Tree, &[], k(spec)), Lookup::Run(want));
        }
    }

    #[test]
    fn function_keys_bind_as_single_keys() {
        let km = default_keymap();
        assert_eq!(
            km.lookup(Context::Tree, &[], k("f5")),
            Lookup::Run(Action::Refresh)
        );
    }

    #[test]
    fn lookup_handles_pending_sequences() {
        let km = default_keymap();
        assert_eq!(km.lookup(Context::Grid, &[], k("g")), Lookup::Pending);
        assert_eq!(
            km.lookup(Context::Grid, &[k("g")], k("g")),
            Lookup::Run(Action::Top)
        );
        assert_eq!(km.lookup(Context::Grid, &[k("g")], k("z")), Lookup::None);
    }

    #[test]
    fn global_bindings_reach_every_context() {
        let km = default_keymap();
        for ctx in [Context::Tree, Context::Grid, Context::Editor] {
            assert_eq!(
                km.lookup(ctx, &[], k("ctrl+h")),
                Lookup::Run(Action::PaneLeft),
                "ctrl+h must work in {ctx:?}"
            );
            assert_eq!(
                km.lookup(ctx, &[], k("esc")),
                Lookup::Run(Action::Escape),
                "esc must work in {ctx:?}"
            );
        }
    }

    #[test]
    fn overrides_replace_defaults() {
        let mut km = default_keymap();
        let file = KeymapFile {
            grid: BTreeMap::from([("y".to_string(), "yank-row".to_string())]),
            ..Default::default()
        };
        assert!(apply_overrides(&mut km, &file).is_empty());
        assert_eq!(
            km.lookup(Context::Grid, &[], k("y")),
            Lookup::Run(Action::YankRow)
        );
    }

    #[test]
    fn bad_overrides_are_reported_not_fatal() {
        let mut km = default_keymap();
        let file = KeymapFile {
            grid: BTreeMap::from([("y".to_string(), "explode".to_string())]),
            ..Default::default()
        };
        let errs = apply_overrides(&mut km, &file);
        assert_eq!(errs.len(), 1);
        assert!(errs[0].contains("unknown action"));
        assert_eq!(
            km.lookup(Context::Grid, &[], k("y")),
            Lookup::Run(Action::YankCell)
        );
    }

    #[test]
    fn every_action_name_round_trips() {
        for a in Action::all() {
            assert_eq!(a.name().parse::<Action>().unwrap(), *a);
        }
    }
}
