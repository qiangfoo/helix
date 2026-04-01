use std::num::NonZeroUsize;

use helix_core::{Range, Selection};

use crate::view::document::{Document, Mode};
use crate::view::editor::{CursorCache, GutterConfig};
use crate::view::graphics::Rect;
use crate::view::tab::Tab;
use crate::view::tree::Tree;
use crate::view::View;

/// Per-tab state: owns a document, view tree, and mode.
/// This struct exists so that `commands::Context` can borrow it
/// separately from the UI-specific fields on `EditorView`.
pub struct DocView {
    pub doc: Document,
    pub tree: Tree,
    pub mode: Mode,
    pub count: Option<NonZeroUsize>,
    pub last_selection: Option<Selection>,
    pub mouse_down_range: Option<Range>,
    pub cursor_cache: CursorCache,
}

impl DocView {
    pub fn new(mut doc: Document) -> Self {
        let doc_id = doc.id();
        let mut tree = Tree::new(Rect::default());
        let view = View::new(doc_id, GutterConfig::default());
        let view_id = tree.split(view, crate::view::tree::Layout::Vertical);
        doc.ensure_view_init(view_id);
        Self {
            doc,
            tree,
            mode: Mode::Normal,
            count: None,
            last_selection: None,
            mouse_down_range: None,
            cursor_cache: CursorCache::default(),
        }
    }
}

impl Tab for DocView {
    fn doc(&self) -> &Document {
        &self.doc
    }
    fn doc_mut(&mut self) -> &mut Document {
        &mut self.doc
    }
    fn tree(&self) -> &Tree {
        &self.tree
    }
    fn tree_mut(&mut self) -> &mut Tree {
        &mut self.tree
    }
    fn mode(&self) -> Mode {
        self.mode
    }
    fn set_mode(&mut self, mode: Mode) {
        self.mode = mode;
    }
    fn count(&self) -> Option<NonZeroUsize> {
        self.count
    }
    fn set_count(&mut self, count: Option<NonZeroUsize>) {
        self.count = count;
    }
    fn last_selection(&self) -> &Option<Selection> {
        &self.last_selection
    }
    fn set_last_selection(&mut self, sel: Option<Selection>) {
        self.last_selection = sel;
    }
    fn mouse_down_range(&self) -> &Option<Range> {
        &self.mouse_down_range
    }
    fn set_mouse_down_range(&mut self, range: Option<Range>) {
        self.mouse_down_range = range;
    }
    fn cursor_cache(&self) -> &CursorCache {
        &self.cursor_cache
    }
    fn doc_and_tree_mut(&mut self) -> (&mut Document, &mut Tree) {
        (&mut self.doc, &mut self.tree)
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}
