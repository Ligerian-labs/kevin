//! Bounded buffers.
//!
//! `plan/07-api-and-tui.md` §4 caps what the TUI keeps in memory: 5 000 log
//! lines for the focused task, 500 events per run timeline. A long-running
//! agent produces far more than that, so every growing collection in the model
//! goes through [`Ring`]: pushing past the capacity drops the oldest item and
//! counts it, and the counter is what the UI shows as "… older lines dropped".

use std::collections::VecDeque;

/// A fixed-capacity FIFO that drops from the front when full.
#[derive(Debug, Clone)]
pub struct Ring<T> {
    items: VecDeque<T>,
    capacity: usize,
    dropped: u64,
}

impl<T> Ring<T> {
    /// An empty ring holding at most `capacity` items (at least one).
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        Self {
            items: VecDeque::with_capacity(capacity.min(1024)),
            capacity,
            dropped: 0,
        }
    }

    /// Appends `item`, evicting the oldest one when the ring is full.
    pub fn push(&mut self, item: T) {
        if self.items.len() == self.capacity {
            self.items.pop_front();
            self.dropped += 1;
        }
        self.items.push_back(item);
    }

    /// Appends every item of `iter`.
    pub fn extend(&mut self, iter: impl IntoIterator<Item = T>) {
        for item in iter {
            self.push(item);
        }
    }

    /// How many items are held right now.
    #[must_use]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Whether nothing is held.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// The cap.
    #[must_use]
    pub const fn capacity(&self) -> usize {
        self.capacity
    }

    /// How many items were evicted since the ring was created.
    #[must_use]
    pub const fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Oldest first.
    pub fn iter(&self) -> impl DoubleEndedIterator<Item = &T> + ExactSizeIterator {
        self.items.iter()
    }

    /// The `n` most recent items, oldest first.
    pub fn tail(&self, n: usize) -> impl DoubleEndedIterator<Item = &T> + ExactSizeIterator {
        self.items.range(self.items.len().saturating_sub(n)..)
    }

    /// The item at `index`, counting from the oldest.
    #[must_use]
    pub fn get(&self, index: usize) -> Option<&T> {
        self.items.get(index)
    }

    /// The most recent item.
    #[must_use]
    pub fn last(&self) -> Option<&T> {
        self.items.back()
    }

    /// Empties the ring; the drop counter is kept.
    pub fn clear(&mut self) {
        self.items.clear();
    }
}

impl<'a, T> IntoIterator for &'a Ring<T> {
    type Item = &'a T;
    type IntoIter = std::collections::vec_deque::Iter<'a, T>;

    fn into_iter(self) -> Self::IntoIter {
        self.items.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::Ring;

    #[test]
    fn pushing_past_capacity_drops_the_oldest() {
        let mut ring = Ring::new(3);
        ring.extend(1..=5);
        assert_eq!(ring.len(), 3);
        assert_eq!(ring.dropped(), 2);
        assert_eq!(ring.iter().copied().collect::<Vec<_>>(), vec![3, 4, 5]);
    }

    #[test]
    fn tail_returns_the_most_recent_items() {
        let mut ring = Ring::new(10);
        ring.extend(1..=5);
        assert_eq!(ring.tail(2).copied().collect::<Vec<_>>(), vec![4, 5]);
        assert_eq!(ring.tail(50).count(), 5);
    }

    #[test]
    fn capacity_is_never_zero() {
        let mut ring = Ring::new(0);
        ring.push(7);
        assert_eq!(ring.len(), 1);
    }
}
