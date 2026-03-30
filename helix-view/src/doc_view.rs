use std::num::NonZeroUsize;

use helix_core::{Range, Selection};

use crate::document::{Document, Mode};
use crate::editor::{CursorCache, GutterConfig};
use crate::graphics::Rect;
use crate::tree::Tree;
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
        let view_id = tree.split(view, crate::tree::Layout::Vertical);
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
