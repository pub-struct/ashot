//! Snapshot-based undo/redo history over `EditState`. One `commit()` per
//! committed user operation (never per mouse-move) — see call sites in
//! `mod.rs` and `preview.rs`/`timeline.rs` in later stages.

use super::state::EditState;

pub struct History {
    stack: Vec<EditState>, // stack[0] is always the initial state; never popped past 1 entry
    redo: Vec<EditState>,
}

impl History {
    pub fn new(initial: EditState) -> Self {
        Self { stack: vec![initial], redo: Vec::new() }
    }

    pub fn current(&self) -> &EditState {
        self.stack.last().expect("stack never empties")
    }

    /// Push a new snapshot. No-op (does not push, does not clear redo) if
    /// `new_state == *self.current()`, so a drag that ends up back where it
    /// started does not pollute history.
    pub fn commit(&mut self, new_state: EditState) {
        if new_state == *self.current() {
            return;
        }
        self.stack.push(new_state);
        self.redo.clear(); // any new edit invalidates the redo branch
    }

    /// Returns the new current state, or None if already at the oldest snapshot.
    pub fn undo(&mut self) -> Option<&EditState> {
        if self.stack.len() <= 1 {
            return None;
        }
        self.redo.push(self.stack.pop().unwrap());
        Some(self.current())
    }

    /// Returns the new current state, or None if nothing to redo.
    pub fn redo(&mut self) -> Option<&EditState> {
        let next = self.redo.pop()?;
        self.stack.push(next);
        Some(self.current())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(cc: bool) -> EditState {
        let mut s = EditState::new(10.0);
        s.burn_cc = cc;
        s
    }

    #[test]
    fn commit_undo_redo() {
        let mut h = History::new(state(false));
        h.commit(state(true));
        assert!(h.current().burn_cc);
        assert!(!h.undo().unwrap().burn_cc);
        assert!(h.redo().unwrap().burn_cc);
    }

    #[test]
    fn no_op_on_unchanged_state() {
        let mut h = History::new(state(false));
        h.commit(state(false));
        assert!(h.undo().is_none()); // nothing was pushed
    }

    #[test]
    fn commit_after_undo_clears_redo() {
        let mut h = History::new(state(false));
        h.commit(state(true));
        h.undo();
        // A fresh edit made instead of redoing should invalidate the redo branch.
        let mut fresh = state(false);
        fresh.zooms.push(ashot_core::video::ZoomPoint { t: 1.0, cx: 0.0, cy: 0.0, level: 2.0, duration: 1.0 });
        h.commit(fresh);
        assert!(h.redo().is_none());
    }
}
