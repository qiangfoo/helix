use std::any::Any;
use std::num::NonZeroUsize;

use helix_core::{Range, Selection};

use crate::view::document::{Document, Mode};
use crate::view::editor::CursorCache;
use crate::view::tree::Tree;

/// Per-tab state trait. Editor stores tabs as trait objects.
///
/// The concrete implementation (`TabView`) lives in helix-term and adds
/// UI-specific fields (keymaps, spinners, etc.) on top of the document state.
pub trait Tab: Any {
    fn doc(&self) -> &Document;
    fn doc_mut(&mut self) -> &mut Document;
    fn tree(&self) -> &Tree;
    fn tree_mut(&mut self) -> &mut Tree;
    fn mode(&self) -> Mode;
    fn set_mode(&mut self, mode: Mode);
    fn count(&self) -> Option<NonZeroUsize>;
    fn set_count(&mut self, count: Option<NonZeroUsize>);
    fn last_selection(&self) -> &Option<Selection>;
    fn set_last_selection(&mut self, sel: Option<Selection>);
    fn mouse_down_range(&self) -> &Option<Range>;
    fn set_mouse_down_range(&mut self, range: Option<Range>);
    fn cursor_cache(&self) -> &CursorCache;
    /// Simultaneously borrow doc and tree mutably, bypassing trait-object borrow limitations.
    fn doc_and_tree_mut(&mut self) -> (&mut Document, &mut Tree);

    fn as_any(&self) -> &dyn Any;
    fn as_any_mut(&mut self) -> &mut dyn Any;
}
