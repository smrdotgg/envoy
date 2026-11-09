//! Rows picked for one action on several at once (multi-select), and the row the keyboard is on
//! while they are. Kept by message id, so it survives the list being read again: rows that
//! disappear drop out ([`Selection::retain`]).

use std::collections::HashSet;

use envoy_core::model::{MessageId, Summary};

#[derive(Clone, Debug, Default)]
pub struct Selection {
    ids: HashSet<MessageId>,
    /// Where a range starts (shift-click, shift+j/k): the row last toggled, or where the
    /// keyboard was when a range began.
    anchor: Option<MessageId>,
    /// The selection as it was when the range from `anchor` began: drawing the range again
    /// (shift-click further down, shift+k back up) replaces only the range's part.
    base: HashSet<MessageId>,
    /// The keyboard's row while selecting: j and k move it, and x toggles it, without opening
    /// anything (the reader shows the selection).
    cursor: Option<MessageId>,
}

fn index(list: &[Summary], id: MessageId) -> Option<usize> {
    list.iter().position(|s| s.id == id)
}

impl Selection {
    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    pub fn len(&self) -> usize {
        self.ids.len()
    }

    pub fn contains(&self, id: MessageId) -> bool {
        self.ids.contains(&id)
    }

    /// The keyboard's row, while anything is selected.
    pub fn cursor(&self) -> Option<MessageId> {
        self.cursor.filter(|_| !self.is_empty())
    }

    /// The selected rows' ids, in the list's order.
    pub fn ids(&self, list: &[Summary]) -> Vec<MessageId> {
        list.iter().filter(|s| self.ids.contains(&s.id)).map(|s| s.id).collect()
    }

    /// Selects `id`, or unselects it if it was (x, mod-click, a row's checkbox). A range starts
    /// from it next. Unselecting the last row leaves nothing selected.
    pub fn toggle(&mut self, id: MessageId) {
        if !self.ids.remove(&id) {
            self.ids.insert(id);
        }
        if self.ids.is_empty() {
            self.clear();
            return;
        }
        self.anchor = Some(id);
        self.base = self.ids.clone();
        self.cursor = Some(id);
    }

    /// Selects the rows from the range's start to `to`, both included, keeping what was
    /// selected before the range began (shift-click). With no start in the list, the range
    /// starts at `from` (the open row, say), else at `to` itself.
    pub fn select_range(&mut self, list: &[Summary], from: Option<MessageId>, to: MessageId) {
        let Some(end) = index(list, to) else { return };
        let anchor = match self.anchor.filter(|a| index(list, *a).is_some()) {
            Some(a) => a,
            None => {
                let a = from.filter(|f| index(list, *f).is_some()).unwrap_or(to);
                self.anchor = Some(a);
                self.base = self.ids.clone();
                a
            }
        };
        let start = index(list, anchor).unwrap_or(end);
        let (lo, hi) = (start.min(end), start.max(end));
        self.ids = self.base.iter().copied().chain(list[lo..=hi].iter().map(|s| s.id)).collect();
        self.cursor = Some(to);
    }

    /// Moves the keyboard's row by `delta` from `from` (shift+j, shift+k), selecting the rows
    /// from the range's start to it. Returns the row it moved to.
    pub fn extend(&mut self, list: &[Summary], from: MessageId, delta: isize) -> Option<MessageId> {
        let at = index(list, from)?;
        if self.anchor.is_none_or(|a| index(list, a).is_none()) {
            self.anchor = Some(from);
            self.base = self.ids.clone();
        }
        let to = list[(at as isize + delta).clamp(0, list.len() as isize - 1) as usize].id;
        self.select_range(list, Some(from), to);
        Some(to)
    }

    /// Moves the keyboard's row by `delta` (j, k while selecting), from the top if it's nowhere.
    /// Returns the row it moved to.
    pub fn move_cursor(&mut self, list: &[Summary], delta: isize) -> Option<MessageId> {
        if list.is_empty() {
            return None;
        }
        let to = match self.cursor.and_then(|c| index(list, c)) {
            Some(i) => (i as isize + delta).clamp(0, list.len() as isize - 1) as usize,
            None => 0,
        };
        self.cursor = Some(list[to].id);
        self.cursor
    }

    /// Every row loaded (mod+a).
    pub fn select_all(&mut self, list: &[Summary]) {
        self.ids = list.iter().map(|s| s.id).collect();
        self.anchor = None;
        self.base = self.ids.clone();
        if self.cursor.is_none_or(|c| !self.ids.contains(&c)) {
            self.cursor = list.first().map(|s| s.id);
        }
        if self.ids.is_empty() {
            self.clear();
        }
    }

    pub fn clear(&mut self) {
        *self = Selection::default();
    }

    /// Keeps only the rows still in the (whole) list: after it's read again, or rows were taken
    /// off it.
    pub fn retain(&mut self, list: &[Summary]) {
        if self.is_empty() {
            return;
        }
        let present: HashSet<MessageId> = list.iter().map(|s| s.id).collect();
        self.ids.retain(|id| present.contains(id));
        self.base.retain(|id| present.contains(id));
        if self.anchor.is_some_and(|a| !present.contains(&a)) {
            self.anchor = None;
        }
        if self.cursor.is_some_and(|c| !present.contains(&c)) {
            self.cursor = None;
        }
        if self.ids.is_empty() {
            self.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Rows 1..=n, top to bottom.
    fn list(n: i64) -> Vec<Summary> {
        let from = envoy_core::model::Address { name: "Maya Chen".into(), email: "maya@northwind.dev".into() };
        let row = Summary {
            id: 0,
            account: "demo".into(),
            mailbox: "INBOX".into(),
            uid: 1,
            thread: String::new(),
            subject: "Design review moved to Thursday".into(),
            from,
            to: Vec::new(),
            date: 0,
            flags: 0,
            preview: String::new(),
            has_attachments: false,
            thread_count: 1,
            participants: String::new(),
            thread_unread: false,
            category: Default::default(),
        };
        (1..=n).map(|id| Summary { id, thread: format!("t{id}"), ..row.clone() }).collect()
    }

    #[test]
    fn toggling_selects_and_unselects() {
        let rows = list(5);
        let mut s = Selection::default();
        s.toggle(2);
        s.toggle(4);
        assert_eq!(s.ids(&rows), vec![2, 4], "in the list's order");
        assert_eq!(s.cursor(), Some(4));
        s.toggle(2);
        assert_eq!(s.ids(&rows), vec![4]);
        s.toggle(4);
        assert!(s.is_empty() && s.cursor().is_none(), "unselecting the last leaves nothing");
    }

    #[test]
    fn shift_click_selects_a_range_and_redraws_it_from_the_same_start() {
        let rows = list(8);
        let mut s = Selection::default();
        s.toggle(1); // kept: selected before the range
        s.toggle(4);
        s.select_range(&rows, None, 6);
        assert_eq!(s.ids(&rows), vec![1, 4, 5, 6]);
        // Further, then back past the start: only the range changes.
        s.select_range(&rows, None, 8);
        assert_eq!(s.ids(&rows), vec![1, 4, 5, 6, 7, 8]);
        s.select_range(&rows, None, 3);
        assert_eq!(s.ids(&rows), vec![1, 3, 4]);
        // With nothing toggled yet, the range starts at the row given (the open one).
        let mut s = Selection::default();
        s.select_range(&rows, Some(2), 4);
        assert_eq!(s.ids(&rows), vec![2, 3, 4]);
    }

    #[test]
    fn shift_j_and_k_extend_from_where_the_keyboard_is() {
        let rows = list(6);
        let mut s = Selection::default();
        // From the open row, nothing selected yet.
        assert_eq!(s.extend(&rows, 2, 1), Some(3));
        assert_eq!(s.extend(&rows, 3, 1), Some(4));
        assert_eq!(s.ids(&rows), vec![2, 3, 4]);
        // Back up shrinks the range, as in a native list.
        assert_eq!(s.extend(&rows, 4, -1), Some(3));
        assert_eq!(s.ids(&rows), vec![2, 3]);
        // It stops at the ends.
        assert_eq!(s.extend(&rows, 3, -1), Some(2));
        assert_eq!(s.extend(&rows, 2, -1), Some(1));
        assert_eq!(s.extend(&rows, 1, -1), Some(1));
        assert_eq!(s.ids(&rows), vec![1, 2]);
        // j moves the keyboard alone; x there starts a new range from it.
        assert_eq!(s.move_cursor(&rows, 3), Some(4));
        s.toggle(4);
        s.extend(&rows, 4, 1);
        assert_eq!(s.ids(&rows), vec![1, 2, 4, 5]);
    }

    #[test]
    fn select_all_takes_every_loaded_row() {
        let rows = list(4);
        let mut s = Selection::default();
        s.select_all(&rows);
        assert_eq!((s.len(), s.cursor()), (4, Some(1)));
        s.select_all(&[]);
        assert!(s.is_empty());
    }

    #[test]
    fn a_refresh_keeps_the_rows_still_there() {
        let rows = list(6);
        let mut s = Selection::default();
        s.toggle(2);
        s.select_range(&rows, None, 5);
        // Rows 3 and 5 went (archived elsewhere); new mail came in on top.
        let mut fresh: Vec<Summary> = rows.iter().filter(|r| r.id != 3 && r.id != 5).cloned().collect();
        fresh.insert(0, Summary { id: 99, ..rows[0].clone() });
        s.retain(&fresh);
        assert_eq!(s.ids(&fresh), vec![2, 4], "by id, wherever they are now");
        assert_eq!(s.cursor(), None, "the keyboard's row went");
        assert_eq!(s.move_cursor(&fresh, 1), Some(99), "and starts again from the top");
        s.retain(&[]);
        assert!(s.is_empty());
    }
}
