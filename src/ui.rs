//! Rendering. Layout is tree | workspace, with a one-line status bar.

use crate::app::{App, Confirm, Focus, MsgKind, Overlay, Tab, TabKind, TabStatus};
use crate::editor::Mode;
use crate::form::{FieldKind, Form, TestState};
use crate::keys::{Action, Context};
use crate::tree::{ConnState, NodeKind};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};

const SPINNER: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];
const TREE_WIDTH: u16 = 34;

struct Theme {
    accent: Color,
    dim: Color,
    err: Color,
    ok: Color,
    focus_border: Color,
    idle_border: Color,
}

const T: Theme = Theme {
    accent: Color::Cyan,
    dim: Color::DarkGray,
    err: Color::Red,
    ok: Color::Green,
    focus_border: Color::Cyan,
    idle_border: Color::DarkGray,
};

fn border(focused: bool) -> Style {
    if focused {
        Style::default().fg(T.focus_border)
    } else {
        Style::default().fg(T.idle_border)
    }
}

pub fn draw(f: &mut Frame, app: &mut App) {
    let root = Layout::vertical([Constraint::Min(3), Constraint::Length(1)]).split(f.area());
    let body = root[0];
    let status = root[1];

    let hide_tree = app.fullscreen && app.focus() != Focus::Tree;
    let cols = if hide_tree {
        Layout::horizontal([Constraint::Length(0), Constraint::Min(10)]).split(body)
    } else {
        Layout::horizontal([Constraint::Length(TREE_WIDTH), Constraint::Min(10)]).split(body)
    };

    if !hide_tree {
        draw_tree(f, cols[0], app);
    }
    draw_workspace(f, cols[1], app);
    draw_status(f, status, app);

    match app.overlay.clone() {
        Overlay::None => {}
        Overlay::Help => draw_help(f, f.area(), app),
        Overlay::Inspect(text) => draw_inspect(f, f.area(), &text),
        Overlay::Confirm(what) => draw_confirm(f, f.area(), &what),
        Overlay::Form(form) => draw_form(f, f.area(), &form),
        Overlay::Prompt(_) => {}
    }
}

fn draw_tree(f: &mut Frame, area: Rect, app: &mut App) {
    let focused = app.focus() == Focus::Tree;
    let title = if app.tree.filter.is_empty() {
        " connections ".to_string()
    } else {
        format!(" connections  /{} ", app.tree.filter)
    };
    let block = Block::bordered()
        .border_style(border(focused))
        .title(Span::styled(
            title,
            Style::default().fg(if focused { T.accent } else { T.dim }),
        ));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let height = inner.height as usize;
    app.tree_height = height;
    let rows = app.tree.rows();
    // Keep the cursor on screen.
    if app.tree.cursor < app.tree.offset {
        app.tree.offset = app.tree.cursor;
    } else if height > 0 && app.tree.cursor >= app.tree.offset + height {
        app.tree.offset = app.tree.cursor + 1 - height;
    }
    if app.tree.offset + height > rows.len() {
        app.tree.offset = rows.len().saturating_sub(height);
    }

    let mut lines = Vec::new();
    for (i, row) in rows
        .iter()
        .enumerate()
        .skip(app.tree.offset)
        .take(height.max(1))
    {
        let selected = i == app.tree.cursor;
        let indent = "  ".repeat(row.depth);
        let marker = if !row.has_children_slot {
            " "
        } else if row.loading {
            SPINNER[(app.spinner / 2) % SPINNER.len()]
        } else if row.expanded {
            "▾"
        } else {
            "▸"
        };
        let icon = match row.kind {
            NodeKind::Project => "▣",
            NodeKind::Connection => match row.state {
                ConnState::Connected => "●",
                ConnState::Failed => "✗",
                _ => "○",
            },
            NodeKind::Schema => "◇",
            NodeKind::Table => "▤",
        };
        let name_style = match row.kind {
            NodeKind::Project => Style::default().fg(T.accent).add_modifier(Modifier::BOLD),
            NodeKind::Connection => match row.state {
                ConnState::Connected => Style::default().fg(T.ok),
                ConnState::Failed => Style::default().fg(T.err),
                _ => Style::default(),
            },
            _ => Style::default(),
        };
        let engine = row
            .engine
            .map(|e| format!(" {}", e.label()))
            .unwrap_or_default();
        let mut spans = vec![
            Span::raw(indent),
            Span::styled(format!("{marker} "), Style::default().fg(T.dim)),
            Span::styled(format!("{icon} "), name_style),
            Span::styled(row.name.clone(), name_style),
        ];
        if !engine.is_empty() {
            spans.push(Span::styled(engine, Style::default().fg(T.dim)));
        }
        let line = Line::from(spans);
        lines.push(if selected {
            line.style(if focused {
                Style::default().bg(T.accent).fg(Color::Black)
            } else {
                Style::default().bg(Color::Indexed(238))
            })
        } else {
            line
        });
    }
    if rows.is_empty() {
        let msg = if app.tree.filter.is_empty() {
            "  no projects configured"
        } else {
            "  no matches"
        };
        lines.push(Line::styled(msg, Style::default().fg(T.dim)));
    }
    f.render_widget(Paragraph::new(lines), inner);
}

fn draw_workspace(f: &mut Frame, area: Rect, app: &mut App) {
    let chunks = Layout::vertical([Constraint::Length(1), Constraint::Min(3)]).split(area);
    draw_tabs(f, chunks[0], app);
    let body = chunks[1];

    if app.tabs.is_empty() {
        let hint = vec![
            Line::from(""),
            Line::styled(
                "  nothing open",
                Style::default().fg(T.dim).add_modifier(Modifier::BOLD),
            ),
            Line::from(""),
            Line::styled(
                "  enter   open the selected table",
                Style::default().fg(T.dim),
            ),
            Line::styled("  ctrl+n  new query tab", Style::default().fg(T.dim)),
            Line::styled("  ?       keybindings", Style::default().fg(T.dim)),
        ];
        let block = Block::bordered().border_style(border(false));
        let inner = block.inner(body);
        f.render_widget(block, body);
        f.render_widget(Paragraph::new(hint), inner);
        return;
    }

    let has_editor = app.tabs[app.tab].kind == TabKind::Query;
    if has_editor {
        let split = if app.fullscreen && app.focus() == Focus::Grid {
            Layout::vertical([Constraint::Length(0), Constraint::Min(3)]).split(body)
        } else if app.fullscreen && app.focus() == Focus::Editor {
            Layout::vertical([Constraint::Min(3), Constraint::Length(0)]).split(body)
        } else {
            Layout::vertical([Constraint::Percentage(40), Constraint::Percentage(60)]).split(body)
        };
        if split[0].height > 0 {
            draw_editor(f, split[0], app);
        }
        if split[1].height > 0 {
            draw_grid(f, split[1], app);
        }
    } else {
        draw_grid(f, body, app);
    }
}

fn draw_tabs(f: &mut Frame, area: Rect, app: &App) {
    let mut spans = Vec::new();
    for (i, t) in app.tabs.iter().enumerate() {
        let active = i == app.tab;
        let mark = match &t.status {
            TabStatus::Running(_) => SPINNER[(app.spinner / 2) % SPINNER.len()],
            TabStatus::Error(_) => "!",
            TabStatus::Idle => "",
        };
        let label = format!(" {}{} ", t.title, if mark.is_empty() { "" } else { mark });
        spans.push(Span::styled(
            label,
            if active {
                Style::default()
                    .bg(T.accent)
                    .fg(Color::Black)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(T.dim)
            },
        ));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_editor(f: &mut Frame, area: Rect, app: &mut App) {
    let focused = app.focus() == Focus::Editor;
    let mode = app.tabs[app.tab].editor.mode;
    let title = format!(
        " query — {} ",
        match mode {
            Mode::Insert => "INSERT",
            Mode::Normal => "NORMAL",
        }
    );
    let block = Block::bordered()
        .border_style(border(focused))
        .title(Span::styled(
            title,
            Style::default().fg(if focused { T.accent } else { T.dim }),
        ));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let height = inner.height as usize;
    app.editor_height = height;
    let tab = &mut app.tabs[app.tab];
    tab.editor.scroll_into_view(height);
    let off = tab.editor.row_off;

    let gutter = 4usize;
    let lines: Vec<Line> = tab
        .editor
        .lines
        .iter()
        .enumerate()
        .skip(off)
        .take(height)
        .map(|(i, text)| {
            Line::from(vec![
                Span::styled(
                    format!("{:>width$} ", i + 1, width = gutter - 1),
                    Style::default().fg(T.dim),
                ),
                Span::raw(text.clone()),
            ])
        })
        .collect();
    f.render_widget(Paragraph::new(lines), inner);

    if focused {
        let cx = inner.x + gutter as u16 + tab.editor.col as u16;
        let cy = inner.y + (tab.editor.line.saturating_sub(off)) as u16;
        if cx < inner.right() && cy < inner.bottom() {
            f.set_cursor_position((cx, cy));
        }
    }
}

fn fit(s: &str, w: usize) -> String {
    let n = s.chars().count();
    if n <= w {
        format!("{s:<w$}")
    } else if w <= 1 {
        "…".repeat(w)
    } else {
        let mut out: String = s.chars().take(w - 1).collect();
        out.push('…');
        out
    }
}

fn draw_grid(f: &mut Frame, area: Rect, app: &mut App) {
    let focused = app.focus() == Focus::Grid;
    let tab_idx = app.tab;
    let max_w = app.ui.max_column_width;
    let title = grid_title(&app.tabs[tab_idx]);
    let block = Block::bordered()
        .border_style(border(focused))
        .title(Span::styled(
            title,
            Style::default().fg(if focused { T.accent } else { T.dim }),
        ));
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    if let TabStatus::Error(msg) = app.tabs[tab_idx].status.clone() {
        let p = Paragraph::new(Text::from(vec![
            Line::styled(
                "error",
                Style::default().fg(T.err).add_modifier(Modifier::BOLD),
            ),
            Line::from(""),
            Line::raw(msg),
        ]))
        .wrap(Wrap { trim: false });
        f.render_widget(p, inner);
        return;
    }

    let tab = &mut app.tabs[tab_idx];
    if tab.view.cols() == 0 {
        let text = match &tab.status {
            TabStatus::Running(_) => "running…".to_string(),
            _ => match tab.view.grid.affected {
                Some(n) => format!("{n} row(s) affected"),
                None => "no results yet".to_string(),
            },
        };
        f.render_widget(
            Paragraph::new(Line::styled(text, Style::default().fg(T.dim))),
            inner,
        );
        return;
    }

    // Row-number gutter, sized for the last absolute row number on this page.
    let last_no = tab.view.page_offset + tab.view.rows();
    let gutter = last_no.to_string().len().max(3) + 1;
    let widths = tab.view.widths(max_w);
    let avail = inner.width as usize - gutter.min(inner.width as usize);

    // Scroll horizontally just enough to keep the cursor column visible.
    if tab.view.col < tab.view.col_off {
        tab.view.col_off = tab.view.col;
    }
    loop {
        let mut used = 0usize;
        let mut last = tab.view.col_off;
        for (i, w) in widths.iter().enumerate().skip(tab.view.col_off) {
            if used + w + 1 > avail && i > tab.view.col_off {
                break;
            }
            used += w + 1;
            last = i;
        }
        if tab.view.col > last && tab.view.col_off < widths.len() - 1 {
            tab.view.col_off += 1;
        } else {
            break;
        }
    }

    let visible: Vec<usize> = {
        let mut used = 0usize;
        let mut v = Vec::new();
        for (i, w) in widths.iter().enumerate().skip(tab.view.col_off) {
            if used + w + 1 > avail && !v.is_empty() {
                break;
            }
            used += w + 1;
            v.push(i);
        }
        v
    };

    let body_height = inner.height.saturating_sub(1) as usize;
    app.grid_height = body_height;
    let tab = &mut app.tabs[tab_idx];
    tab.view.scroll_into_view(body_height);

    let mut lines = Vec::with_capacity(body_height + 1);

    // Header.
    let mut header = vec![Span::styled(" ".repeat(gutter), Style::default().fg(T.dim))];
    for &c in &visible {
        let hi = c == tab.view.col;
        header.push(Span::styled(
            format!("{} ", fit(&tab.view.grid.columns[c], widths[c])),
            Style::default()
                .fg(if hi { T.accent } else { Color::Reset })
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
        ));
    }
    lines.push(Line::from(header));

    for r in tab.view.row_off..(tab.view.row_off + body_height).min(tab.view.rows()) {
        let on_row = r == tab.view.row;
        let mut spans = vec![Span::styled(
            format!(
                "{:>width$} ",
                tab.view.page_offset + r + 1,
                width = gutter - 1
            ),
            Style::default().fg(if on_row { T.accent } else { T.dim }),
        )];
        for &c in &visible {
            let cell = tab.view.cell(r, c);
            let (text, mut style) = match cell {
                Some(Some(s)) => (s.replace(['\n', '\t'], " "), Style::default()),
                Some(None) => (
                    "NULL".to_string(),
                    Style::default().fg(T.dim).add_modifier(Modifier::ITALIC),
                ),
                None => (String::new(), Style::default()),
            };
            if on_row && c == tab.view.col {
                style = if focused {
                    Style::default().bg(T.accent).fg(Color::Black)
                } else {
                    Style::default().bg(Color::Indexed(238))
                };
            } else if on_row {
                style = style.bg(Color::Indexed(236));
            }
            spans.push(Span::styled(format!("{} ", fit(&text, widths[c])), style));
        }
        lines.push(Line::from(spans));
    }

    f.render_widget(Paragraph::new(lines), inner);
}

fn grid_title(tab: &Tab) -> String {
    let mut parts = vec![tab.conn_name.clone()];
    match &tab.kind {
        TabKind::Data { schema, table } => parts.push(format!("{schema}.{table}")),
        TabKind::Structure { schema, table } => parts.push(format!("{schema}.{table} structure")),
        TabKind::Query => parts.push("results".into()),
    }
    if tab.view.page_offset > 0 {
        parts.push(format!("rows {}+", tab.view.page_offset + 1));
    }
    if !tab.view.grid.complete {
        parts.push("truncated".into());
    }
    format!(" {} ", parts.join(" · "))
}

fn draw_status(f: &mut Frame, area: Rect, app: &App) {
    // A prompt takes over the status line entirely.
    if let Overlay::Prompt(kind) = app.overlay {
        let line = Line::from(vec![
            Span::styled(
                kind.sigil().to_string(),
                Style::default().fg(T.accent).add_modifier(Modifier::BOLD),
            ),
            Span::raw(app.input.clone()),
            Span::styled("▏", Style::default().fg(T.accent)),
        ]);
        f.render_widget(Paragraph::new(line), area);
        return;
    }

    let pane = match app.focus() {
        Focus::Tree => "TREE",
        Focus::Grid => "GRID",
        Focus::Editor => match app.active_tab().map(|t| t.editor.mode) {
            Some(Mode::Insert) => "INSERT",
            _ => "EDITOR",
        },
    };
    let mut left = vec![
        Span::styled(
            format!(" {pane} "),
            Style::default()
                .bg(T.accent)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
    ];
    if app.is_busy() {
        left.push(Span::styled(
            format!("{} ", SPINNER[(app.spinner / 2) % SPINNER.len()]),
            Style::default().fg(T.accent),
        ));
    }
    if let Some(tab) = app.active_tab() {
        left.push(Span::styled(
            format!("{}  ", tab.conn_name),
            Style::default().fg(T.dim),
        ));
        if tab.view.cols() > 0 {
            left.push(Span::styled(
                format!(
                    "r{}/{}  c{}/{}  ",
                    tab.view.row + 1,
                    tab.view.rows(),
                    tab.view.col + 1,
                    tab.view.cols()
                ),
                Style::default().fg(T.dim),
            ));
        }
    }
    if !app.pending.is_empty() {
        let keys: String = app.pending.iter().map(|k| k.render()).collect();
        left.push(Span::styled(
            format!("{keys}  "),
            Style::default().fg(T.accent),
        ));
    }
    if let Some((kind, msg)) = &app.message {
        left.push(Span::styled(
            msg.clone(),
            Style::default().fg(match kind {
                MsgKind::Error => T.err,
                MsgKind::Info => T.dim,
            }),
        ));
    }
    let hint = "  ?:help  ctrl+h/l:panes  esc:back ";
    let used: usize = left.iter().map(|s| s.content.chars().count()).sum();
    let pad = (area.width as usize)
        .saturating_sub(used + hint.chars().count())
        .min(area.width as usize);
    left.push(Span::raw(" ".repeat(pad)));
    left.push(Span::styled(hint, Style::default().fg(T.dim)));
    f.render_widget(Paragraph::new(Line::from(left)), area);
}

fn centered(area: Rect, pct_x: u16, pct_y: u16) -> Rect {
    let v = Layout::vertical([
        Constraint::Percentage((100 - pct_y) / 2),
        Constraint::Percentage(pct_y),
        Constraint::Percentage((100 - pct_y) / 2),
    ])
    .split(area);
    Layout::horizontal([
        Constraint::Percentage((100 - pct_x) / 2),
        Constraint::Percentage(pct_x),
        Constraint::Percentage((100 - pct_x) / 2),
    ])
    .split(v[1])[1]
}

fn draw_help(f: &mut Frame, area: Rect, app: &App) {
    let rect = centered(area, 76, 84);
    f.render_widget(Clear, rect);
    let block = Block::bordered()
        .border_style(Style::default().fg(T.accent))
        .title(" keybindings — j/k scroll, esc close ");
    let inner = block.inner(rect);
    f.render_widget(block, rect);

    let mut lines = Vec::new();
    for ctx in [
        Context::Global,
        Context::Tree,
        Context::Grid,
        Context::Editor,
    ] {
        let mut entries = app.keymap.entries(ctx);
        if entries.is_empty() {
            continue;
        }
        entries.sort_by_key(|(_, a)| a.name());
        lines.push(Line::styled(
            match ctx {
                Context::Global => "global",
                Context::Tree => "tree",
                Context::Grid => "grid",
                Context::Editor => "query editor",
            },
            Style::default().fg(T.accent).add_modifier(Modifier::BOLD),
        ));
        let mut by_action: Vec<(Action, Vec<String>)> = Vec::new();
        for (k, a) in entries {
            match by_action.iter_mut().find(|(x, _)| *x == a) {
                Some((_, ks)) => ks.push(k),
                None => by_action.push((a, vec![k])),
            }
        }
        for (a, ks) in by_action {
            lines.push(Line::from(vec![
                Span::styled(
                    format!("  {:<18}", ks.join(", ")),
                    Style::default().fg(T.ok),
                ),
                Span::raw(a.name().replace('-', " ")),
            ]));
        }
        lines.push(Line::from(""));
    }
    f.render_widget(
        Paragraph::new(lines)
            .scroll((0, 0))
            .wrap(Wrap { trim: false }),
        inner,
    );
}

fn draw_inspect(f: &mut Frame, area: Rect, text: &str) {
    let rect = centered(area, 72, 60);
    f.render_widget(Clear, rect);
    let block = Block::bordered()
        .border_style(Style::default().fg(T.accent))
        .title(format!(
            " cell — {} chars — y copy, esc close ",
            text.chars().count()
        ));
    let inner = block.inner(rect);
    f.render_widget(block, rect);
    f.render_widget(
        Paragraph::new(text.to_string()).wrap(Wrap { trim: false }),
        inner,
    );
}

fn draw_confirm(f: &mut Frame, area: Rect, what: &Confirm) {
    let (title, body, verb) = match what {
        Confirm::RunSql(sql) => (
            " this statement writes — run it? ".to_string(),
            vec![Line::raw(sql.clone())],
            "run",
        ),
        Confirm::DeleteConnection {
            project,
            name,
            account,
            ..
        } => {
            let mut body = vec![
                Line::from(vec![
                    Span::raw("delete connection "),
                    Span::styled(
                        format!("{project}/{name}"),
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                ]),
                Line::from(""),
                Line::styled("  · its entry in config.toml", Style::default().fg(T.dim)),
            ];
            match account {
                Some(a) => body.push(Line::styled(
                    format!("  · its stored password (keyring dbx/{a})"),
                    Style::default().fg(T.dim),
                )),
                None => body.push(Line::styled(
                    "  · no stored password is managed by dbx, so none is removed",
                    Style::default().fg(T.dim),
                )),
            }
            (" delete connection? ".to_string(), body, "delete")
        }
        Confirm::DeleteProject { name, connections } => {
            let mut body = vec![
                Line::from(vec![
                    Span::raw("delete project "),
                    Span::styled(name.clone(), Style::default().add_modifier(Modifier::BOLD)),
                ]),
                Line::from(""),
            ];
            if connections.is_empty() {
                body.push(Line::styled(
                    "  it has no connections",
                    Style::default().fg(T.dim),
                ));
            } else {
                body.push(Line::styled(
                    format!(
                        "  and these {} connection(s), with their stored passwords:",
                        connections.len()
                    ),
                    Style::default().fg(T.dim),
                ));
                for c in connections {
                    body.push(Line::styled(
                        format!("  · {name}/{c}"),
                        Style::default().fg(T.err),
                    ));
                }
            }
            (" delete project? ".to_string(), body, "delete")
        }
    };

    let height = (body.len() + 6).clamp(9, 22) as u16;
    let rect = centered_lines(area, 68, height);
    f.render_widget(Clear, rect);
    let block = Block::bordered()
        .border_style(Style::default().fg(T.err))
        .title(title);
    let inner = block.inner(rect);
    f.render_widget(block, rect);
    let mut lines = body;
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("y", Style::default().fg(T.ok).add_modifier(Modifier::BOLD)),
        Span::raw(format!(" {verb}    ")),
        Span::styled(
            "n/esc",
            Style::default().fg(T.err).add_modifier(Modifier::BOLD),
        ),
        Span::raw(" cancel"),
    ]));
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

/// A centred box with an explicit height, for dialogs that size to content.
fn centered_lines(area: Rect, pct_x: u16, height: u16) -> Rect {
    let h = height.min(area.height);
    let top = area.height.saturating_sub(h) / 2;
    let v = Layout::vertical([
        Constraint::Length(top),
        Constraint::Length(h),
        Constraint::Min(0),
    ])
    .split(area);
    Layout::horizontal([
        Constraint::Percentage((100 - pct_x) / 2),
        Constraint::Percentage(pct_x),
        Constraint::Percentage((100 - pct_x) / 2),
    ])
    .split(v[1])[1]
}

fn draw_form(f: &mut Frame, area: Rect, form: &Form) {
    let fields = form.fields();
    let height = (fields.len() + 9).clamp(12, 30) as u16;
    let rect = centered_lines(area, 62, height);
    f.render_widget(Clear, rect);
    let block = Block::bordered()
        .border_style(Style::default().fg(T.accent))
        .title(Span::styled(
            form.title(),
            Style::default().fg(T.accent).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(rect);
    f.render_widget(block, rect);

    const LABEL_W: usize = 12;
    let mut lines: Vec<Line> = Vec::new();
    for (i, id) in fields.iter().enumerate() {
        let selected = i == form.cursor;
        let editing = selected && form.editing;
        let value = form.display(*id);
        let value_style = if editing {
            Style::default().fg(Color::Black).bg(T.accent)
        } else if selected {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        let mut spans = vec![
            Span::styled(
                if selected { " ▸ " } else { "   " },
                Style::default().fg(T.accent),
            ),
            Span::styled(
                format!("{:<LABEL_W$}", form.label(*id)),
                Style::default().fg(T.dim),
            ),
        ];
        let shown = if editing {
            format!("{value}▏")
        } else if value.is_empty() {
            "—".to_string()
        } else {
            value
        };
        spans.push(Span::styled(shown, value_style));
        if matches!(form.kind(*id), FieldKind::Select | FieldKind::Toggle)
            && selected
            && !form.editing
        {
            spans.push(Span::styled("  ‹h/l›", Style::default().fg(T.dim)));
        }
        lines.push(Line::from(spans));
        if let Some(err) = form.error_for(*id) {
            lines.push(Line::styled(
                format!("   {:<LABEL_W$}{err}", ""),
                Style::default().fg(T.err),
            ));
        }
    }

    lines.push(Line::from(""));
    if !form.is_project_only() && !form.projects.is_empty() {
        lines.push(Line::styled(
            format!("   projects: {}", form.projects.join(", ")),
            Style::default().fg(T.dim),
        ));
    }
    match &form.test {
        TestState::Idle => {}
        TestState::Running => {
            lines.push(Line::styled("   testing…", Style::default().fg(T.accent)))
        }
        TestState::Ok(msg) => lines.push(Line::styled(
            format!("   ✓ {msg}"),
            Style::default().fg(T.ok),
        )),
        TestState::Failed(msg, hints) => {
            lines.push(Line::styled(
                format!("   ✗ {msg}"),
                Style::default().fg(T.err),
            ));
            for h in hints.iter().take(2) {
                lines.push(Line::styled(
                    format!("     {h}"),
                    Style::default().fg(T.dim),
                ));
            }
        }
    }
    lines.push(Line::from(""));
    lines.push(Line::styled(
        if form.editing {
            "   enter/esc done · ctrl+u clear field"
        } else {
            "   enter edit · h/l change · ctrl+t test · ctrl+s save · esc cancel"
        },
        Style::default().fg(T.dim),
    ));

    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

#[cfg(test)]
mod tests {
    use super::fit;

    #[test]
    fn fit_pads_and_truncates() {
        assert_eq!(fit("ab", 4), "ab  ");
        assert_eq!(fit("abcdef", 4), "abc…");
        assert_eq!(fit("abc", 3), "abc");
        assert_eq!(fit("héllo", 3), "hé…");
        assert_eq!(fit("abc", 1), "…");
        assert_eq!(fit("abc", 0), "");
    }
}
