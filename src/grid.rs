//! Result-grid viewport: cursor, scrolling, search, yank.

use crate::db::Grid;

#[derive(Debug, Default)]
pub struct GridView {
    pub grid: Grid,
    pub row: usize,
    pub col: usize,
    pub row_off: usize,
    pub col_off: usize,
    pub search: String,
    /// Offset of the first row of this page within the whole table.
    pub page_offset: usize,
}

impl GridView {
    pub fn set_grid(&mut self, grid: Grid) {
        self.grid = grid;
        self.row = 0;
        self.col = 0;
        self.row_off = 0;
        self.col_off = 0;
    }

    pub fn rows(&self) -> usize {
        self.grid.rows.len()
    }

    pub fn cols(&self) -> usize {
        self.grid.columns.len()
    }

    pub fn move_row(&mut self, delta: isize) {
        if self.rows() == 0 {
            return;
        }
        let max = self.rows() as isize - 1;
        self.row = (self.row as isize + delta).clamp(0, max) as usize;
    }

    pub fn move_col(&mut self, delta: isize) {
        if self.cols() == 0 {
            return;
        }
        let max = self.cols() as isize - 1;
        self.col = (self.col as isize + delta).clamp(0, max) as usize;
    }

    pub fn top(&mut self) {
        self.row = 0;
    }

    pub fn bottom(&mut self) {
        self.row = self.rows().saturating_sub(1);
    }

    pub fn row_start(&mut self) {
        self.col = 0;
    }

    pub fn row_end(&mut self) {
        self.col = self.cols().saturating_sub(1);
    }

    /// Keeps the cursor inside a viewport `height` rows tall.
    pub fn scroll_into_view(&mut self, height: usize) {
        if height == 0 {
            return;
        }
        if self.row < self.row_off {
            self.row_off = self.row;
        } else if self.row >= self.row_off + height {
            self.row_off = self.row + 1 - height;
        }
    }

    pub fn cell(&self, row: usize, col: usize) -> Option<&Option<String>> {
        self.grid.rows.get(row)?.get(col)
    }

    pub fn current_cell(&self) -> Option<&Option<String>> {
        self.cell(self.row, self.col)
    }

    pub fn current_cell_text(&self) -> String {
        match self.current_cell() {
            Some(Some(s)) => s.clone(),
            Some(None) => String::new(),
            None => String::new(),
        }
    }

    /// The selected row as tab-separated text, NULLs rendered as `NULL`.
    pub fn current_row_text(&self) -> String {
        self.grid
            .rows
            .get(self.row)
            .map(|r| {
                r.iter()
                    .map(|c| c.clone().unwrap_or_else(|| "NULL".into()))
                    .collect::<Vec<_>>()
                    .join("\t")
            })
            .unwrap_or_default()
    }

    /// Next cell matching `search`, scanning row-major from the cursor.
    pub fn find(&self, forward: bool) -> Option<(usize, usize)> {
        if self.search.is_empty() || self.rows() == 0 || self.cols() == 0 {
            return None;
        }
        let needle = self.search.to_ascii_lowercase();
        let total = self.rows() * self.cols();
        let start = self.row * self.cols() + self.col;
        for step in 1..=total {
            let idx = if forward {
                (start + step) % total
            } else {
                (start + total - step) % total
            };
            let (r, c) = (idx / self.cols(), idx % self.cols());
            let hay = match self.cell(r, c) {
                Some(Some(s)) => s.to_ascii_lowercase(),
                Some(None) => "null".to_string(),
                None => continue,
            };
            if hay.contains(&needle) {
                return Some((r, c));
            }
        }
        None
    }

    pub fn search_step(&mut self, forward: bool) -> bool {
        match self.find(forward) {
            Some((r, c)) => {
                self.row = r;
                self.col = c;
                true
            }
            None => false,
        }
    }

    /// Rendered width per column, capped at `max`.
    pub fn widths(&self, max: usize) -> Vec<usize> {
        self.grid
            .columns
            .iter()
            .enumerate()
            .map(|(i, name)| {
                let body = self
                    .grid
                    .rows
                    .iter()
                    .map(|r| match r.get(i) {
                        Some(Some(s)) => s.chars().count(),
                        Some(None) => 4,
                        None => 0,
                    })
                    .max()
                    .unwrap_or(0);
                body.max(name.chars().count()).clamp(3, max)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grid() -> Grid {
        Grid {
            columns: vec!["id".into(), "name".into(), "note".into()],
            rows: vec![
                vec![Some("1".into()), Some("alice".into()), None],
                vec![Some("2".into()), Some("bob".into()), Some("hello".into())],
                vec![Some("3".into()), Some("Carol".into()), Some("bye".into())],
            ],
            affected: None,
            elapsed_ms: 1,
            complete: true,
        }
    }

    fn view() -> GridView {
        let mut v = GridView::default();
        v.set_grid(grid());
        v
    }

    #[test]
    fn movement_clamps_at_edges() {
        let mut v = view();
        v.move_row(-1);
        assert_eq!(v.row, 0);
        v.move_row(99);
        assert_eq!(v.row, 2);
        v.move_col(99);
        assert_eq!(v.col, 2);
        v.row_start();
        assert_eq!(v.col, 0);
        v.row_end();
        assert_eq!(v.col, 2);
    }

    #[test]
    fn movement_is_safe_on_empty_grid() {
        let mut v = GridView::default();
        v.move_row(5);
        v.move_col(5);
        v.bottom();
        assert_eq!((v.row, v.col), (0, 0));
        assert!(v.current_cell().is_none());
    }

    #[test]
    fn scrolling_follows_the_cursor() {
        let mut v = view();
        v.row = 2;
        v.scroll_into_view(2);
        assert_eq!(v.row_off, 1);
        v.row = 0;
        v.scroll_into_view(2);
        assert_eq!(v.row_off, 0);
    }

    #[test]
    fn search_is_case_insensitive_and_wraps() {
        let mut v = view();
        v.search = "carol".into();
        assert!(v.search_step(true));
        assert_eq!((v.row, v.col), (2, 1));
        v.search = "alice".into();
        assert!(v.search_step(true));
        assert_eq!((v.row, v.col), (0, 1));
    }

    #[test]
    fn search_matches_nulls_by_name() {
        let mut v = view();
        v.search = "null".into();
        assert!(v.search_step(true));
        assert_eq!((v.row, v.col), (0, 2));
    }

    #[test]
    fn search_misses_report_false() {
        let mut v = view();
        v.search = "zzz".into();
        assert!(!v.search_step(true));
        assert_eq!((v.row, v.col), (0, 0));
    }

    #[test]
    fn yank_renders_nulls_explicitly() {
        let mut v = view();
        v.col = 2;
        assert_eq!(v.current_cell_text(), "");
        assert_eq!(v.current_row_text(), "1\talice\tNULL");
    }

    #[test]
    fn widths_respect_the_cap() {
        let mut v = view();
        v.grid.rows[0][1] = Some("x".repeat(100));
        let w = v.widths(10);
        assert_eq!(w[1], 10);
        assert_eq!(w[0], 3);
    }
}
