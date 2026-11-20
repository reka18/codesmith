//! A footer menu's selection and scroll, as OpenCode's
//! `createFooterMenuState` (`mini/footer.menu`, over `ui/select-controller`):
//! the selection is clamped, and the window scrolls a margin before the
//! selected row reaches its top or bottom edge, so the rows around it
//! stay in view.

/// Rows a footer menu shows at most.
pub const ROWS: usize = 8;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Menu {
    pub selected: usize,
    pub offset: usize,
}

fn max_offset(count: usize, limit: usize) -> usize {
    count.saturating_sub(limit)
}

impl Menu {
    /// Move by `dir` (-1 or 1) among `count` rows shown `limit` at a time.
    pub fn step(&mut self, dir: i32, count: usize, limit: usize) {
        if count == 0 {
            *self = Menu::default();
            return;
        }
        let limit = limit.max(1);
        let next = (self.selected as i64 + dir as i64).clamp(0, count as i64 - 1) as usize;
        self.selected = next;
        let max = max_offset(count, limit);
        let margin = ((limit - 1) / 2).min(2);
        self.offset = if dir < 0 && next < self.offset + margin {
            next.saturating_sub(margin).min(max)
        } else if dir > 0 && next + margin + 1 > self.offset + limit {
            (next + margin + 1 - limit).min(max)
        } else {
            self.offset.min(max)
        };
    }

    /// Keep the selection in range and in view after `count` changed.
    pub fn reconcile(&mut self, count: usize, limit: usize) {
        let limit = limit.max(1);
        self.selected = self.selected.min(count.saturating_sub(1));
        let max = max_offset(count, limit);
        self.offset = if self.selected < self.offset {
            self.selected.min(max)
        } else if self.selected >= self.offset + limit {
            (self.selected + 1 - limit).min(max)
        } else {
            self.offset.min(max)
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // mini/footer.menu.test.ts: "footer menu scrolls before the selected
    // row hits the bottom edge".
    #[test]
    fn oc_the_menu_scrolls_before_the_selection_hits_the_bottom_edge() {
        let mut m = Menu::default();
        for _ in 0..6 {
            m.step(1, 20, ROWS);
        }
        assert_eq!((m.selected, m.offset), (6, 1));
    }

    // mini/footer.menu.test.ts: "footer menu scrolls before the selected
    // row hits the top edge".
    #[test]
    fn oc_the_menu_scrolls_before_the_selection_hits_the_top_edge() {
        let mut m = Menu::default();
        for _ in 0..13 {
            m.step(1, 20, ROWS);
        }
        for _ in 0..4 {
            m.step(-1, 20, ROWS);
        }
        assert_eq!((m.selected, m.offset), (9, 7));
    }

    #[test]
    fn the_selection_is_clamped_and_kept_in_view() {
        let mut m = Menu::default();
        m.step(-1, 3, ROWS);
        assert_eq!(m, Menu::default());
        for _ in 0..30 {
            m.step(1, 20, ROWS);
        }
        assert_eq!((m.selected, m.offset), (19, 12));
        m.reconcile(5, ROWS);
        assert_eq!((m.selected, m.offset), (4, 0));
    }
}
