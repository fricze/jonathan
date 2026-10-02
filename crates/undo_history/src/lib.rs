//! Generic per-key undo/redo stack bookkeeping: push, pop-and-reverse, and
//! dirty tracking by stack length. Knows nothing about what an entry
//! contains or how to reverse one -- the caller supplies that via a
//! closure, so this crate stays independent of any particular storage
//! model (CSV sheets, a text buffer, anything with undoable edits).
//!
//! The one real invariant this type enforces: a new edit always clears
//! that same key's redo branch (never another key's), and a file is dirty
//! iff its current undo-stack length differs from the length recorded at
//! its last save -- true through any sequence of edit/undo/redo, since
//! undo pops one entry and redo pushes one back, symmetrically.

use std::collections::{HashMap, HashSet};
use std::hash::Hash;

pub struct UndoHistory<K, E> {
    undo_stack: HashMap<K, Vec<E>>,
    redo_stack: HashMap<K, Vec<E>>,
    clean_marker: HashMap<K, usize>,
    dirty: HashSet<K>,
}

// Manual impl instead of #[derive(Default)]: the derive macro adds a
// `K: Default, E: Default` bound even though nothing here actually needs
// one -- an empty HashMap/HashSet doesn't require its key/value types to
// implement Default.
impl<K, E> Default for UndoHistory<K, E> {
    fn default() -> Self {
        Self {
            undo_stack: HashMap::new(),
            redo_stack: HashMap::new(),
            clean_marker: HashMap::new(),
            dirty: HashSet::new(),
        }
    }
}

impl<K: Eq + Hash + Clone, E> UndoHistory<K, E> {
    pub fn new() -> Self {
        Self::default()
    }

    /// Push a new entry onto `key`'s undo stack, clear that same key's redo
    /// stack (a new edit invalidates its redo branch, but never another
    /// key's), and refresh its dirty status. Every mutation that records
    /// history should go through this so those three steps can't drift
    /// apart.
    pub fn push(&mut self, key: &K, entry: E) {
        self.undo_stack.entry(key.clone()).or_default().push(entry);
        self.redo_stack.remove(key);
        self.refresh_dirty(key);
    }

    /// Pop `key`'s undo stack, apply `reverse` to get (an entry that undoes
    /// that pop, a result to report), push the reverse entry onto `key`'s
    /// redo stack, and return the result. `None` if the stack was empty or
    /// `reverse` itself returned `None` (the entry's target no longer
    /// exists) -- in the latter case the popped entry is dropped, not
    /// requeued, since there is nothing left to reverse it into.
    ///
    /// Use this when reversing an entry doesn't need anything from `self`
    /// beyond the entry itself. When it does (the reversal needs `&mut`
    /// access to the same struct this history lives in as a field), use
    /// `pop_undo`/`push_redo_entry` directly instead -- see their docs.
    pub fn undo<R>(&mut self, key: &K, reverse: impl FnOnce(E) -> Option<(E, R)>) -> Option<R> {
        let entry = self.undo_stack.get_mut(key)?.pop()?;
        let (redo_entry, result) = reverse(entry)?;
        self.redo_stack.entry(key.clone()).or_default().push(redo_entry);
        self.refresh_dirty(key);
        Some(result)
    }

    /// Mirror of `undo` between the redo and undo stacks.
    pub fn redo<R>(&mut self, key: &K, reverse: impl FnOnce(E) -> Option<(E, R)>) -> Option<R> {
        let entry = self.redo_stack.get_mut(key)?.pop()?;
        let (undo_entry, result) = reverse(entry)?;
        self.undo_stack.entry(key.clone()).or_default().push(undo_entry);
        self.refresh_dirty(key);
        Some(result)
    }

    /// Pop the top of `key`'s undo stack, without reversing it. Pair with
    /// `push_redo_entry` once you've computed the reverse -- split into two
    /// steps (instead of `undo`'s single closure call) so the reversal can
    /// borrow `&mut self` on the same struct this history is a field of,
    /// which a closure captured by `undo` while it also holds `&mut self`
    /// on this history could not do.
    pub fn pop_undo(&mut self, key: &K) -> Option<E> {
        self.undo_stack.get_mut(key)?.pop()
    }

    /// Mirror of `pop_undo` on the redo stack.
    pub fn pop_redo(&mut self, key: &K) -> Option<E> {
        self.redo_stack.get_mut(key)?.pop()
    }

    /// Push `entry` onto `key`'s redo stack and refresh dirty status --
    /// the second half of a manual undo (see `pop_undo`). Does NOT clear
    /// the redo stack first (unlike `push`, which is for new edits, not
    /// for recording an undo's own reversal).
    pub fn push_redo_entry(&mut self, key: &K, entry: E) {
        self.redo_stack.entry(key.clone()).or_default().push(entry);
        self.refresh_dirty(key);
    }

    /// Push `entry` onto `key`'s undo stack and refresh dirty status --
    /// the second half of a manual redo (see `pop_redo`). Does NOT clear
    /// the redo stack (unlike `push`).
    pub fn push_undo_entry(&mut self, key: &K, entry: E) {
        self.undo_stack.entry(key.clone()).or_default().push(entry);
        self.refresh_dirty(key);
    }

    /// Record `key` as clean at its current undo-stack length (call after a
    /// successful save).
    pub fn mark_clean(&mut self, key: &K) {
        let len = self.undo_stack.get(key).map_or(0, |s| s.len());
        self.clean_marker.insert(key.clone(), len);
        self.refresh_dirty(key);
    }

    pub fn is_dirty(&self, key: &K) -> bool {
        self.dirty.contains(key)
    }

    pub fn any_dirty(&self) -> bool {
        !self.dirty.is_empty()
    }

    /// The entry that would be reversed by the next `undo(key, ...)` call,
    /// without popping it.
    pub fn peek_undo(&self, key: &K) -> Option<&E> {
        self.undo_stack.get(key)?.last()
    }

    /// The entry that would be reversed by the next `redo(key, ...)` call,
    /// without popping it.
    pub fn peek_redo(&self, key: &K) -> Option<&E> {
        self.redo_stack.get(key)?.last()
    }

    pub fn undo_len(&self, key: &K) -> usize {
        self.undo_stack.get(key).map_or(0, |s| s.len())
    }

    pub fn redo_len(&self, key: &K) -> usize {
        self.redo_stack.get(key).map_or(0, |s| s.len())
    }

    fn refresh_dirty(&mut self, key: &K) {
        let current_len = self.undo_stack.get(key).map_or(0, |s| s.len());
        let clean_len = self.clean_marker.get(key).copied().unwrap_or(0);
        if current_len == clean_len {
            self.dirty.remove(key);
        } else {
            self.dirty.insert(key.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_makes_dirty_and_undo_makes_clean_again() {
        let mut h: UndoHistory<String, i32> = UndoHistory::new();
        let key = "f.csv".to_string();
        assert!(!h.is_dirty(&key));

        h.push(&key, 1);
        assert!(h.is_dirty(&key));

        let result = h.undo(&key, |e| Some((e, e)));
        assert_eq!(result, Some(1));
        assert!(!h.is_dirty(&key));
    }

    #[test]
    fn undo_then_redo_round_trips() {
        let mut h: UndoHistory<String, i32> = UndoHistory::new();
        let key = "f.csv".to_string();
        h.push(&key, 1);
        h.push(&key, 2);

        assert_eq!(h.undo(&key, |e| Some((e, e))), Some(2));
        assert_eq!(h.redo(&key, |e| Some((e, e))), Some(2));
        assert!(h.is_dirty(&key));

        assert_eq!(h.undo(&key, |e| Some((e, e))), Some(2));
        assert_eq!(h.undo(&key, |e| Some((e, e))), Some(1));
        assert!(!h.is_dirty(&key));
    }

    #[test]
    fn new_push_clears_redo_stack() {
        let mut h: UndoHistory<String, i32> = UndoHistory::new();
        let key = "f.csv".to_string();
        h.push(&key, 1);
        h.undo(&key, |e| Some((e, e)));
        // redo stack now has one entry -- a fresh push should clear it.
        h.push(&key, 2);
        assert_eq!(h.redo(&key, |e| Some((e, e))), None);
    }

    #[test]
    fn undo_on_empty_stack_is_noop() {
        let mut h: UndoHistory<String, i32> = UndoHistory::new();
        let key = "f.csv".to_string();
        assert_eq!(h.undo(&key, |e| Some((e, e))), None);
    }

    #[test]
    fn undo_missing_target_drops_entry_without_dirtying_redo() {
        let mut h: UndoHistory<String, i32> = UndoHistory::new();
        let key = "f.csv".to_string();
        h.push(&key, 1);
        // reverse returns None -- as if the entry's target no longer exists.
        assert_eq!(h.undo(&key, |_e| None::<(i32, i32)>), None);
        // The entry was popped and dropped -- undo stack is now empty, and
        // nothing was pushed onto the redo stack either.
        assert_eq!(h.undo(&key, |e| Some((e, e))), None);
        assert_eq!(h.redo(&key, |e| Some((e, e))), None);
    }

    #[test]
    fn separate_keys_have_independent_history() {
        let mut h: UndoHistory<String, i32> = UndoHistory::new();
        h.push(&"a.csv".to_string(), 1);
        assert!(h.is_dirty(&"a.csv".to_string()));
        assert!(!h.is_dirty(&"b.csv".to_string()));
    }

    #[test]
    fn dirty_tracked_relative_to_clean_marker_not_zero() {
        let mut h: UndoHistory<String, i32> = UndoHistory::new();
        let key = "f.csv".to_string();
        h.push(&key, 1);
        h.mark_clean(&key);
        assert!(!h.is_dirty(&key));

        h.push(&key, 2);
        assert!(h.is_dirty(&key));

        h.undo(&key, |e| Some((e, e)));
        assert!(!h.is_dirty(&key));
    }

    #[test]
    fn peek_does_not_pop() {
        let mut h: UndoHistory<String, i32> = UndoHistory::new();
        let key = "f.csv".to_string();
        h.push(&key, 1);
        h.push(&key, 2);
        assert_eq!(h.peek_undo(&key), Some(&2));
        assert_eq!(h.undo_len(&key), 2);
        assert_eq!(h.peek_redo(&key), None);

        h.undo(&key, |e| Some((e, e)));
        assert_eq!(h.peek_redo(&key), Some(&2));
        assert_eq!(h.redo_len(&key), 1);
    }

    #[test]
    fn manual_pop_and_push_mirrors_undo() {
        let mut h: UndoHistory<String, i32> = UndoHistory::new();
        let key = "f.csv".to_string();
        h.push(&key, 1);
        h.push(&key, 2);

        let popped = h.pop_undo(&key).expect("undo stack should have an entry");
        assert_eq!(popped, 2);
        h.push_redo_entry(&key, popped);

        assert_eq!(h.undo_len(&key), 1);
        assert_eq!(h.peek_redo(&key), Some(&2));
        assert!(h.is_dirty(&key));
    }

    #[test]
    fn any_dirty_reflects_all_keys() {
        let mut h: UndoHistory<String, i32> = UndoHistory::new();
        assert!(!h.any_dirty());
        h.push(&"a.csv".to_string(), 1);
        assert!(h.any_dirty());
        h.undo(&"a.csv".to_string(), |e| Some((e, e)));
        assert!(!h.any_dirty());
    }
}
