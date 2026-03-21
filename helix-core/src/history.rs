use crate::{ChangeSet, Rope, Selection, Transaction};
use std::num::NonZeroUsize;
use std::time::Instant;

#[derive(Debug, Clone)]
pub struct State {
    pub doc: Rope,
    pub selection: Selection,
}

/// Stores the history of changes to a buffer.
///
/// In this read-only viewer, History only tracks revision numbers for
/// view synchronization. Undo/redo functionality has been removed.
#[derive(Debug)]
pub struct History {
    revisions: Vec<Revision>,
    current: usize,
}

#[derive(Debug, Clone)]
struct Revision {
    parent: usize,
    last_child: Option<NonZeroUsize>,
    transaction: Transaction,
    inversion: Transaction,
    timestamp: Instant,
}

impl Default for History {
    fn default() -> Self {
        Self {
            revisions: vec![Revision {
                parent: 0,
                last_child: None,
                transaction: Transaction::from(ChangeSet::new("".into())),
                inversion: Transaction::from(ChangeSet::new("".into())),
                timestamp: Instant::now(),
            }],
            current: 0,
        }
    }
}

impl History {
    pub fn commit_revision(&mut self, transaction: &Transaction, original: &State) {
        self.commit_revision_at_timestamp(transaction, original, Instant::now());
    }

    pub fn commit_revision_at_timestamp(
        &mut self,
        transaction: &Transaction,
        original: &State,
        timestamp: Instant,
    ) {
        let inversion = transaction
            .invert(&original.doc)
            .with_selection(original.selection.clone());

        let new_current = self.revisions.len();
        self.revisions[self.current].last_child = NonZeroUsize::new(new_current);
        self.revisions.push(Revision {
            parent: self.current,
            last_child: None,
            transaction: transaction.clone(),
            inversion,
            timestamp,
        });
        self.current = new_current;
    }

    #[inline]
    pub fn current_revision(&self) -> usize {
        self.current
    }

    #[inline]
    pub const fn at_root(&self) -> bool {
        self.current == 0
    }

    /// Returns the changes since the given revision composed into a transaction.
    /// Returns None if there are no changes between the current and given revisions.
    pub fn changes_since(&self, revision: usize) -> Option<Transaction> {
        let lca = self.lowest_common_ancestor(revision, self.current);
        let up = self.path_up(revision, lca);
        let down = self.path_up(self.current, lca);
        let up_txns = up
            .iter()
            .rev()
            .map(|&n| self.revisions[n].inversion.clone());
        let down_txns = down.iter().map(|&n| self.revisions[n].transaction.clone());

        down_txns.chain(up_txns).reduce(|acc, tx| tx.compose(acc))
    }

    fn lowest_common_ancestor(&self, mut a: usize, mut b: usize) -> usize {
        use std::collections::HashSet;
        let mut a_path_set = HashSet::new();
        let mut b_path_set = HashSet::new();
        loop {
            a_path_set.insert(a);
            b_path_set.insert(b);
            if a_path_set.contains(&b) {
                return b;
            }
            if b_path_set.contains(&a) {
                return a;
            }
            a = self.revisions[a].parent;
            b = self.revisions[b].parent;
        }
    }

    fn path_up(&self, mut n: usize, a: usize) -> Vec<usize> {
        let mut path = Vec::new();
        while n != a {
            path.push(n);
            n = self.revisions[n].parent;
        }
        path
    }
}
