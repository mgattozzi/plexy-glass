//! Tab stops, default every 8 columns.

#[derive(Debug, Clone)]
pub struct TabStops {
    stops: Vec<bool>,
}

impl TabStops {
    pub const DEFAULT_INTERVAL: u16 = 8;

    pub fn new(cols: u16) -> Self {
        let mut stops = vec![false; cols as usize];
        for (i, s) in stops.iter_mut().enumerate() {
            if i % Self::DEFAULT_INTERVAL as usize == 0 {
                *s = true;
            }
        }
        Self { stops }
    }

    pub fn set(&mut self, col: u16) {
        if let Some(s) = self.stops.get_mut(col as usize) {
            *s = true;
        }
    }

    pub fn clear(&mut self, col: u16) {
        if let Some(s) = self.stops.get_mut(col as usize) {
            *s = false;
        }
    }

    pub fn clear_all(&mut self) {
        for s in self.stops.iter_mut() {
            *s = false;
        }
    }

    /// Column index of the next tab stop strictly greater than `col`.
    pub fn next(&self, col: u16) -> Option<u16> {
        ((col as usize + 1)..self.stops.len())
            .find(|&i| self.stops[i])
            .map(|i| i as u16)
    }

    /// Resize, preserving existing stops and seeding the new tail with defaults.
    pub fn resize(&mut self, cols: u16) {
        let old_len = self.stops.len();
        self.stops.resize(cols as usize, false);
        if cols as usize > old_len {
            for i in old_len..(cols as usize) {
                if i % Self::DEFAULT_INTERVAL as usize == 0 {
                    self.stops[i] = true;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_stops_every_eight() {
        let t = TabStops::new(40);
        for i in 0u16..40 {
            let expected = i % 8 == 0;
            let got = t.next(i.saturating_sub(1));
            if expected && i > 0 {
                assert_eq!(got, Some(i), "missed stop at {i}");
            }
        }
    }

    #[test]
    fn set_and_clear() {
        let mut t = TabStops::new(40);
        t.clear(8);
        assert_eq!(t.next(0), Some(16));
        t.set(5);
        assert_eq!(t.next(0), Some(5));
    }

    #[test]
    fn next_beyond_end_returns_none() {
        let t = TabStops::new(16);
        assert_eq!(t.next(15), None);
    }

    #[test]
    fn resize_grow_seeds_defaults() {
        let mut t = TabStops::new(8);
        t.resize(32);
        assert_eq!(t.next(8), Some(16));
        assert_eq!(t.next(16), Some(24));
    }

    #[test]
    fn resize_shrink_truncates() {
        let mut t = TabStops::new(40);
        t.resize(10);
        assert_eq!(t.next(8), None);
    }
}
