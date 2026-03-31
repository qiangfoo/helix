//! Stateless proxy Component that delegates to the active Application.
//!
//! Sits in the layer stack above TabManager. Events flow top-down:
//! overlays → ActiveAppProxy → TabManager (global keymaps).

use helix_core::Position;
use helix_view::graphics::{CursorKind, Rect};
use helix_view::Editor;
use tui::buffer::Buffer as Surface;

use crate::compositor::{self, Component, Context, EventResult};

use super::app::{take_app_state, restore_app_state, AppState};

/// A stateless Component that delegates to `editor.app_state[active]`.
pub struct ActiveAppProxy;

impl Component for ActiveAppProxy {
    fn handle_event(
        &mut self,
        event: &compositor::Event,
        ctx: &mut Context,
    ) -> EventResult {
        let mut state_box = take_app_state(ctx.editor);
        let result = if let Some(state) = state_box.downcast_mut::<AppState>() {
            if let Some(app) = state.apps.get_mut(state.active) {
                app.handle_event(event, ctx)
            } else {
                EventResult::Ignored(None)
            }
        } else {
            EventResult::Ignored(None)
        };
        restore_app_state(ctx.editor, state_box);
        result
    }

    fn render(&mut self, _area: Rect, surface: &mut Surface, ctx: &mut Context) {
        let mut state_box = take_app_state(ctx.editor);
        if let Some(state) = state_box.downcast_mut::<AppState>() {
            let main_area = ctx.editor.main_area;
            if let Some(app) = state.apps.get_mut(state.active) {
                app.render(main_area, surface, ctx);
            }
        }
        restore_app_state(ctx.editor, state_box);
    }

    fn cursor(&self, _area: Rect, editor: &Editor) -> (Option<Position>, CursorKind) {
        if let Some(state) = editor.app_state.downcast_ref::<AppState>() {
            if let Some(app) = state.apps.get(state.active) {
                return app.cursor(editor.main_area, editor);
            }
        }
        (None, CursorKind::Hidden)
    }
}
