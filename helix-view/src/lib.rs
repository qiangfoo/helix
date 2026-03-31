#[macro_use]
pub mod macros;

pub mod annotations;
pub mod clipboard;
pub mod doc_view;
pub mod document;
pub mod editor;
pub mod events;
pub mod expansion;
pub mod graphics;
pub mod gutter;
pub mod handlers;
pub mod info;
pub mod input;
pub mod keyboard;
pub mod register;
pub mod tab;
pub mod theme;
pub mod tree;
pub mod view;

use std::sync::atomic::{AtomicU64, Ordering};

/// Unique identifier for an application tab / document.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AppId(u64);

static NEXT_APP_ID: AtomicU64 = AtomicU64::new(1);

impl AppId {
    pub fn next() -> Self {
        Self(NEXT_APP_ID.fetch_add(1, Ordering::Relaxed))
    }
}

impl Default for AppId {
    fn default() -> AppId {
        AppId::next()
    }
}

impl std::fmt::Display for AppId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_fmt(format_args!("{}", self.0))
    }
}

slotmap::new_key_type! {
    pub struct ViewId;
}

pub enum Align {
    Top,
    Center,
    Bottom,
}

pub fn align_view(doc: &mut Document, view: &View, align: Align) {
    let doc_text = doc.text().slice(..);
    let cursor = doc.selection(view.id).primary().cursor(doc_text);
    let viewport = view.inner_area(doc);
    let last_line_height = viewport.height.saturating_sub(1);
    let mut view_offset = doc.view_offset(view.id);

    let relative = match align {
        Align::Center => last_line_height / 2,
        Align::Top => 0,
        Align::Bottom => last_line_height,
    };

    let text_fmt = doc.text_format(viewport.width, None);
    (view_offset.anchor, view_offset.vertical_offset) = char_idx_at_visual_offset(
        doc_text,
        cursor,
        -(relative as isize),
        0,
        &text_fmt,
        &view.text_annotations(doc, None),
    );
    doc.set_view_offset(view.id, view_offset);
}

pub use doc_view::DocView;
pub use document::Document;
pub use editor::Editor;
use helix_core::char_idx_at_visual_offset;
pub use theme::Theme;
pub use view::View;
