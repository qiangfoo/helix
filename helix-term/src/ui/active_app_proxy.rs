//! Stateless proxy Component that delegates to the active Application.
//!
//! Sits in the layer stack above TabManager. Events flow top-down:
//! overlays → ActiveAppProxy → TabManager (global keymaps).

use helix_core::Position;
use helix_view::graphics::{CursorKind, Rect};
use helix_view::Editor;
use ratatui::buffer::Buffer as Surface;

use crate::compositor::{self, Component, Context, EventResult};

use super::app::{get_app, Application};

type AppBox = Box<dyn Application>;

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
        let mut app_any = std::mem::replace(&mut ctx.editor.apps[idx], Box::new(()));
        let result = if let Some(app) = app_any.downcast_mut::<AppBox>() {
            app.handle_event(event, ctx)
        } else {
            EventResult::Ignored(None)
        };
        ctx.editor.apps[idx] = app_any;
        result
    }

    fn render(&mut self, _area: Rect, surface: &mut Surface, ctx: &mut Context) {
        let idx = ctx.editor.active_app;
        if idx >= ctx.editor.apps.len() {
            return;
        }
        let main_area = ctx.editor.main_area;
        let mut app_any = std::mem::replace(&mut ctx.editor.apps[idx], Box::new(()));
        if let Some(app) = app_any.downcast_mut::<AppBox>() {
            app.render(main_area, surface, ctx);
        }
        ctx.editor.apps[idx] = app_any;
    }

    fn cursor(&self, _area: Rect, editor: &Editor) -> (Option<Position>, CursorKind) {
        if let Some(app) = get_app(editor, editor.active_app) {
            return app.cursor(editor.main_area, editor);
        }
        (None, CursorKind::Hidden)
    }
}
