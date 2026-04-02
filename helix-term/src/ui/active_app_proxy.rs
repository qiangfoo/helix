//! Stateless proxy Component that delegates to the active Application.
//!
//! Sits in the layer stack above TabManager. Events flow top-down:
//! overlays → ActiveAppProxy → TabManager (global keymaps).

use helix_core::Position;
use crate::view::graphics::{CursorKind, Rect};
use crate::view::Editor;
use ratatui::buffer::Buffer as Surface;

use crate::compositor::{self, Component, Context, EventResult};
use crate::ui::app::Application;

/// Placeholder Application used during take/restore to split borrows.
/// Stores the real app's ID so that `editor.apps[idx].id()` returns the
/// correct value even while the real app is temporarily taken out.
struct Placeholder {
    real_id: crate::view::AppId,
}

impl Application for Placeholder {
    fn as_any(&self) -> &dyn std::any::Any { self }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any { self }
    fn id(&self) -> crate::view::AppId { self.real_id }
    fn name(&self, _: &Editor) -> String { String::new() }
    fn handle_event(&mut self, _: &crate::view::input::Event, _: &mut Context) -> EventResult {
        EventResult::Ignored(None)
    }
    fn render(&mut self, _: Rect, _: &mut Surface, _: &mut Context) {}
}

/// A stateless Component that delegates to `editor.apps[active_app]`.
pub struct ActiveAppProxy;

impl Component for ActiveAppProxy {
    fn handle_event(
        &mut self,
        event: &compositor::Event,
        ctx: &mut Context,
    ) -> EventResult {
        let idx = ctx.editor.active_app;
        if idx >= ctx.editor.apps.len() {
            return EventResult::Ignored(None);
        }
        // Take the active app out to split the borrow with ctx.editor
        let real_id = ctx.editor.apps[idx].id();
        let mut app = std::mem::replace(&mut ctx.editor.apps[idx], Box::new(Placeholder { real_id }));
        let result = app.handle_event(event, ctx);
        ctx.editor.apps[idx] = app;
        result
    }

    fn render(&mut self, _area: Rect, surface: &mut Surface, ctx: &mut Context) {
        let idx = ctx.editor.active_app;
        if idx >= ctx.editor.apps.len() {
            return;
        }
        let main_area = ctx.editor.main_area;
        let real_id = ctx.editor.apps[idx].id();
        let mut app = std::mem::replace(&mut ctx.editor.apps[idx], Box::new(Placeholder { real_id }));
        app.render(main_area, surface, ctx);
        ctx.editor.apps[idx] = app;
    }

    fn cursor(&self, _area: Rect, editor: &Editor) -> (Option<Position>, CursorKind) {
        if let Some(app) = editor.apps.get(editor.active_app) {
            return app.cursor(editor.main_area, editor);
        }
        (None, CursorKind::Hidden)
    }
}
