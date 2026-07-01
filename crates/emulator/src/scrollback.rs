//! Bounded scrollback buffer of completed rows.

use crate::grid::Row;
use std::collections::VecDeque;

pub const DEFAULT_SCROLLBACK_LINES: usize = 10_000;

#[derive(Debug, Clone)]
pub struct Scrollback {
    rows: VecDeque<Row>,
    cap: usize,
}

impl Default for Scrollback {
    fn default() -> Self {
        Self::with_cap(DEFAULT_SCROLLBACK_LINES)
    }
}

impl Scrollback {
    pub fn with_cap(cap: usize) -> Self {
        Self {
            rows: VecDeque::with_capacity(cap.min(1024)),
            cap,
        }
    }

    /// Push a row, evicting from the front while over the cap. Returns the
    /// number of front rows evicted (0 or 1 in steady state), so the caller can
    /// shift anything anchored to an absolute scrollback index.
    pub fn push(&mut self, row: Row) -> usize {
        self.rows.push_back(row);
        let mut evicted = 0;
        while self.rows.len() > self.cap {
            self.rows.pop_front();
            evicted += 1;
        }
        evicted
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub const fn rows(&self) -> &VecDeque<Row> {
        &self.rows
    }

    pub const fn rows_mut(&mut self) -> &mut VecDeque<Row> {
        &mut self.rows
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_respects_cap() {
        let mut s = Scrollback::with_cap(3);
        for _ in 0..5 {
            s.push(Row::blank(1));
        }
        assert_eq!(s.len(), 3);
    }

    #[test]
    fn fifo_order_when_overflowing() {
        let mut s = Scrollback::with_cap(2);
        let mut a = Row::blank(1);
        a.cells[0].grapheme = "A".into();
        let mut b = Row::blank(1);
        b.cells[0].grapheme = "B".into();
        let mut c = Row::blank(1);
        c.cells[0].grapheme = "C".into();
        s.push(a);
        s.push(b);
        s.push(c);
        let texts: Vec<&str> = s.rows().iter().map(|r| r.cells[0].grapheme.as_str()).collect();
        assert_eq!(texts, vec!["B", "C"]);
    }

    #[test]
    fn empty_default() {
        let s = Scrollback::default();
        assert!(s.is_empty());
    }
}
