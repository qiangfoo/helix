use crate::{Rope, Selection, Transaction};

/// Simple state snapshot used in tests.
#[derive(Debug, Clone)]
pub struct State {
    pub doc: Rope,
    pub selection: Selection,
}

/// Stores a linear log of transactions for view synchronization.
///
/// In this read-only viewer, History only tracks forward transactions
/// so that views can catch up on document changes via `changes_since`.
/// Undo/redo functionality has been removed.
#[derive(Debug)]
pub struct History {
    revisions: Vec<Transaction>,
    current: usize,
}

impl Default for History {
    fn default() -> Self {
        Self {
            revisions: Vec::new(),
            current: 0,
        }
    }
}

impl History {
    /// Append a transaction to the linear history log.
    pub fn commit_changeset(&mut self, transaction: Transaction) {
        self.revisions.push(transaction);
        self.current = self.revisions.len();
    }

    #[inline]
    pub fn current_revision(&self) -> usize {
        self.current
    }

    /// Returns the changes since the given revision composed into a transaction.
    /// Returns None if there are no changes between the current and given revisions.
    pub fn changes_since(&self, revision: usize) -> Option<Transaction> {
        if revision >= self.current {
            return None;
        }
        self.revisions[revision..self.current]
            .iter()
            .cloned()
            .reduce(|acc, tx| acc.compose(tx))
    }
}
