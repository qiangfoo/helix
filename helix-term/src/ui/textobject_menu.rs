//! A popup Component for textobject selection (e.g. `mi` → select inner word/pair/etc).
//!
//! Pushed by `select_textobject_inner` / `select_textobject_around`. Shows hints
//! for available textobject types and handles the next keypress to perform the selection.

use helix_core::textobject::{self, TextObject};
use helix_core::Range;
use helix_view::graphics::{CursorKind, Rect};
use helix_view::info::Info;
use helix_view::Editor;
use tui::buffer::Buffer as Surface;

use crate::compositor::{self, Callback, Component, Context, EventResult};
use crate::layers::EditorLayers;

use super::editor::canonicalize_key;

pub const ID: &str = "textobject-menu";

pub struct TextObjectMenu {
    objtype: TextObject,
    count: usize,
    info: Info,
}

impl TextObjectMenu {
    pub fn new(objtype: TextObject, count: usize) -> Self {
        let title = match objtype {
            TextObject::Inside => "Match inside",
            TextObject::Around => "Match around",
            _ => "Textobject",
        };
        let help_text = [
            ("w", "Word"),
            ("W", "WORD"),
            ("p", "Paragraph"),
            ("t", "Type definition (tree-sitter)"),
            ("f", "Function (tree-sitter)"),
            ("a", "Argument/parameter (tree-sitter)"),
            ("c", "Comment (tree-sitter)"),
            ("T", "Test (tree-sitter)"),
            ("e", "Data structure entry (tree-sitter)"),
            ("m", "Closest surrounding pair (tree-sitter)"),
            ("g", "Change"),
            ("x", "(X)HTML element (tree-sitter)"),
            (" ", "... or any character acting as a pair"),
        ];
        let info = Info::new(title, &help_text);
        Self { objtype, count, info }
    }

    fn close_callback() -> Option<Callback> {
        Some(Box::new(|editor: &mut Editor| {
            editor.remove_layer(ID);
            editor.autoinfo = None;
        }))
    }
}

impl Component for TextObjectMenu {
    fn handle_event(
        &mut self,
        event: &compositor::Event,
        ctx: &mut Context,
    ) -> EventResult {
        let compositor::Event::Key(mut key) = *event else {
            return EventResult::Ignored(None);
        };
        canonicalize_key(&mut key);

        if key == crate::key!(Esc) {
            return EventResult::Consumed(Self::close_callback());
        }

        let Some(ch) = key.char() else {
            return EventResult::Consumed(Self::close_callback());
        };

        let objtype = self.objtype;
        let count = self.count;

        // Build the textobject motion and apply it
        let textobject_motion = move |editor: &mut Editor| {
            let (view, doc) = helix_view::current!(editor);
            let loader = editor.syn_loader.load();
            let text = doc.text().slice(..);

            let textobject_treesitter = |obj_name: &str, range: Range| -> Range {
                let Some(syntax) = doc.syntax() else {
                    return range;
                };
                textobject::textobject_treesitter(
                    text, range, objtype, obj_name, syntax, &loader, count,
                )
            };

            if ch == 'g' && doc.diff_handle().is_none() {
                editor.set_status("Diff is not available in current buffer");
                return;
            }

            let textobject_change = |range: Range| -> Range {
                let diff_handle = doc.diff_handle().unwrap();
                let diff = diff_handle.load();
                let line = range.cursor_line(text);
                let hunk_idx = if let Some(hunk_idx) = diff.hunk_at(line as u32, false) {
                    hunk_idx
                } else {
                    return range;
                };
                let hunk = diff.nth_hunk(hunk_idx).after;

                let start = text.line_to_char(hunk.start as usize);
                let end = text.line_to_char(hunk.end as usize);
                Range::new(start, end).with_direction(range.direction())
            };

            let selection = doc.selection(view.id).clone().transform(|range| {
                match ch {
                    'w' => textobject::textobject_word(text, range, objtype, count, false),
                    'W' => textobject::textobject_word(text, range, objtype, count, true),
                    't' => textobject_treesitter("class", range),
                    'f' => textobject_treesitter("function", range),
                    'a' => textobject_treesitter("parameter", range),
                    'c' => textobject_treesitter("comment", range),
                    'T' => textobject_treesitter("test", range),
                    'e' => textobject_treesitter("entry", range),
                    'x' => textobject_treesitter("xml-element", range),
                    'p' => textobject::textobject_paragraph(text, range, objtype, count),
                    'm' => textobject::textobject_pair_surround_closest(
                        doc.syntax(),
                        text,
                        range,
                        objtype,
                        count,
                    ),
                    'g' => textobject_change(range),
                    ch if !ch.is_ascii_alphanumeric() => textobject::textobject_pair_surround(
                        doc.syntax(),
                        text,
                        range,
                        objtype,
                        ch,
                        count,
                    ),
                    _ => range,
                }
            });
            doc.set_selection(view.id, selection);
        };

        // Apply motion directly
        textobject_motion(ctx.editor);

        // Close self via deferred callback
        EventResult::Consumed(Self::close_callback())
    }

    fn render(&mut self, area: Rect, surface: &mut Surface, ctx: &mut Context) {
        if ctx.editor.config().auto_info {
            self.info.render(area, surface, ctx);
        }
    }

    fn cursor(&self, _area: Rect, _editor: &Editor) -> (Option<helix_core::Position>, CursorKind) {
        (None, CursorKind::Hidden)
    }

    fn id(&self) -> Option<&'static str> {
        Some(ID)
    }
}
