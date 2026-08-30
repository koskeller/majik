//! Click / modifier selection over an ordered list of keys, shared by every grid.

use std::collections::BTreeSet;

use crate::model::GenerationId;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Modifiers {
    pub command: bool,
    pub shift: bool,
}

/// Selection over any ordered key (`GenerationId` for the generation feeds, `EntryId` for the grid).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Selection<K = GenerationId> {
    pub ids: BTreeSet<K>,
    /// Position of the last clicked item in the feed it was clicked in; the anchor for ⇧-ranges
    /// and arrow keys. Re-derived from `anchor` when the feed reorders (see [`Self::retain_in`]).
    pub last_index: Option<usize>,
    /// The item behind `last_index`, so the index can follow it when items are inserted or
    /// removed in front of it.
    anchor: Option<K>,
}

impl<K> Default for Selection<K> {
    fn default() -> Self {
        Self { ids: BTreeSet::new(), last_index: None, anchor: None }
    }
}

impl<K: Clone + Ord> Selection<K> {
    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    pub fn len(&self) -> usize {
        self.ids.len()
    }

    pub fn contains(&self, id: &K) -> bool {
        self.ids.contains(id)
    }

    pub fn clear(&mut self) {
        self.ids.clear();
        self.last_index = None;
        self.anchor = None;
    }

    pub fn select_all(&mut self, all: &[K]) {
        self.ids = all.iter().cloned().collect();
    }

    /// Single selection helper (e.g. after opening detail).
    pub fn single(&self) -> Option<&K> {
        if self.ids.len() == 1 {
            self.ids.iter().next()
        } else {
            None
        }
    }

    /// Drop ids that are no longer in the feed, and move `last_index` to wherever the anchor item
    /// now sits (items inserted or deleted in front of it shift its position). An anchor that was
    /// itself removed keeps its old index while that is still in range.
    pub fn retain_in(&mut self, all: &[K]) {
        let keep: BTreeSet<&K> = all.iter().collect();
        self.ids.retain(|id| keep.contains(id));
        match self.anchor.as_ref().and_then(|anchor| all.iter().position(|id| id == anchor)) {
            Some(index) => self.last_index = Some(index),
            None => {
                self.anchor = None;
                if self.last_index.is_some_and(|i| i >= all.len()) {
                    self.last_index = None;
                }
            }
        }
    }

    /// Left click: ⌘ toggles, ⇧ extends the range from the last index
    /// (additively), plain click replaces.
    pub fn click(&mut self, clicked: &K, index: usize, mods: Modifiers, all: &[K]) {
        if mods.command {
            if !self.ids.remove(clicked) {
                self.ids.insert(clicked.clone());
            }
        } else if mods.shift {
            if let Some(last) = self.last_index {
                let lo = last.min(index);
                let hi = last.max(index).min(all.len().saturating_sub(1));
                if lo <= hi {
                    for id in &all[lo..=hi] {
                        self.ids.insert(id.clone());
                    }
                }
            } else {
                self.ids = BTreeSet::from([clicked.clone()]);
            }
        } else {
            self.ids = BTreeSet::from([clicked.clone()]);
        }
        self.last_index = Some(index);
        self.anchor = Some(clicked.clone());
    }

    /// Finder-style right click: keep a selection that already contains
    /// the item, otherwise select just that item.
    pub fn right_click(&mut self, clicked: &K, index: usize) {
        if !self.ids.contains(clicked) {
            self.ids = BTreeSet::from([clicked.clone()]);
        }
        self.last_index = Some(index);
        self.anchor = Some(clicked.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(n: usize) -> Vec<GenerationId> {
        (0..n).map(|i| GenerationId(format!("id{i}"))).collect()
    }

    #[test]
    fn plain_click_replaces() {
        let all = ids(5);
        let mut s = Selection::default();
        s.click(&all[1], 1, Modifiers::default(), &all);
        s.click(&all[3], 3, Modifiers::default(), &all);
        assert_eq!(s.ids, BTreeSet::from([all[3].clone()]));
        assert_eq!(s.last_index, Some(3));
    }

    #[test]
    fn command_click_toggles() {
        let all = ids(5);
        let mut s = Selection::default();
        let cmd = Modifiers { command: true, shift: false };
        s.click(&all[1], 1, cmd, &all);
        s.click(&all[3], 3, cmd, &all);
        assert_eq!(s.len(), 2);
        s.click(&all[1], 1, cmd, &all);
        assert_eq!(s.ids, BTreeSet::from([all[3].clone()]));
    }

    #[test]
    fn shift_click_extends_range_additively() {
        let all = ids(6);
        let mut s = Selection::default();
        s.click(&all[4], 4, Modifiers::default(), &all);
        s.click(&all[1], 1, Modifiers { command: false, shift: true }, &all);
        assert_eq!(s.len(), 4);
        assert!(s.contains(&all[1]) && s.contains(&all[4]));
    }

    #[test]
    fn retain_in_follows_the_anchor_when_items_shift() {
        let all = ids(6);
        let mut s = Selection::default();
        s.click(&all[3], 3, Modifiers::default(), &all);
        // A new item lands at the top: the clicked item is now at 4.
        let mut inserted = vec![GenerationId("new".into())];
        inserted.extend(all.iter().cloned());
        s.retain_in(&inserted);
        assert_eq!(s.last_index, Some(4));
        // Deleting an item in front of it moves it back to 3.
        let mut deleted = inserted.clone();
        deleted.remove(0);
        deleted.remove(0);
        s.retain_in(&deleted);
        assert_eq!(s.last_index, Some(2));
        assert_eq!(s.single(), Some(&all[3]));
    }

    #[test]
    fn retain_in_keeps_an_in_range_index_when_the_anchor_is_gone() {
        let all = ids(6);
        let mut s = Selection::default();
        s.click(&all[2], 2, Modifiers::default(), &all);
        let without_anchor: Vec<GenerationId> = all.iter().filter(|id| **id != all[2]).cloned().collect();
        s.retain_in(&without_anchor);
        assert!(s.is_empty());
        assert_eq!(s.last_index, Some(2), "arrow keys resume from where the item was");
        s.retain_in(&all[..2]);
        assert_eq!(s.last_index, None, "unless that position no longer exists");
    }

    #[test]
    fn right_click_preserves_multi_selection() {
        let all = ids(4);
        let mut s = Selection::default();
        s.select_all(&all);
        s.right_click(&all[2], 2);
        assert_eq!(s.len(), 4);
        s.clear();
        s.right_click(&all[2], 2);
        assert_eq!(s.ids, BTreeSet::from([all[2].clone()]));
    }
}
