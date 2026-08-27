//! Ordered tab collection with an always-valid active index.

/// A list of tabs plus the active one.
///
/// Invariant: `active < len()` whenever the collection is not empty.
pub(crate) struct Tabs<T> {
    list: Vec<T>,
    active: usize,
}

impl<T> Tabs<T> {
    pub(crate) fn new() -> Self {
        Self { list: Vec::new(), active: 0 }
    }

    pub(crate) fn len(&self) -> usize {
        self.list.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.list.is_empty()
    }

    pub(crate) fn active_index(&self) -> usize {
        self.active
    }

    pub(crate) fn active(&self) -> Option<&T> {
        self.list.get(self.active)
    }

    pub(crate) fn active_mut(&mut self) -> Option<&mut T> {
        self.list.get_mut(self.active)
    }

    pub(crate) fn get(&self, index: usize) -> Option<&T> {
        self.list.get(index)
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = &T> {
        self.list.iter()
    }

    pub(crate) fn iter_mut(&mut self) -> impl Iterator<Item = &mut T> {
        self.list.iter_mut()
    }

    /// Append a tab and make it active.
    pub(crate) fn push(&mut self, item: T) {
        self.list.push(item);
        self.active = self.list.len() - 1;
    }

    /// Remove a tab, keeping the active index valid.
    pub(crate) fn close(&mut self, index: usize) {
        if index >= self.list.len() {
            return;
        }
        self.list.remove(index);
        if self.active >= self.list.len() {
            self.active = self.list.len().saturating_sub(1);
        }
    }

    /// Make `index` active if it exists.
    pub(crate) fn switch(&mut self, index: usize) {
        if index < self.list.len() {
            self.active = index;
        }
    }
}

impl<T> Default for Tabs<T> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tabs(n: usize) -> Tabs<i32> {
        let mut tabs = Tabs::new();
        for i in 0..n {
            tabs.push(i as i32);
        }
        tabs
    }

    #[test]
    fn push_activates_new_tab() {
        let mut tabs = Tabs::new();
        tabs.push(1);
        tabs.push(2);
        assert_eq!(tabs.active(), Some(&2));
        assert_eq!(tabs.active_index(), 1);
    }

    #[test]
    fn close_adjusts_active() {
        let mut tabs = tabs(3);
        tabs.switch(2);
        tabs.close(2); // close active at the end: active moves back
        assert_eq!(tabs.active(), Some(&1));

        tabs.close(0); // close before active: active tab stays the same
        assert_eq!(tabs.active(), Some(&1));

        tabs.close(0); // closing the last tab empties the collection
        assert!(tabs.is_empty());
        assert_eq!(tabs.active(), None);
    }

    #[test]
    fn out_of_bounds_ops_are_noops() {
        let mut tabs = tabs(2);
        tabs.switch(5);
        assert_eq!(tabs.active_index(), 1);
        tabs.close(5);
        assert_eq!(tabs.len(), 2);
    }
}
