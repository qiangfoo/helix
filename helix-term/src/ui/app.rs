use std::any::Any;
use std::path::PathBuf;

use helix_core::Position;
use helix_view::graphics::{CursorKind, Rect};
use helix_view::Editor;
use tui::buffer::Buffer as Surface;

use crate::compositor::{Context, EventResult};

pub use helix_view::input::Event;
pub use helix_view::AppId;

/// Trait for tab applications. Each tab in the tab bar is an Application.
///
/// Applications own their rendering area and keybindings. The TabManager
/// handles the shared chrome (tab bar, commandline) and delegates
/// the main area to the active application.
pub trait Application: Any {
    fn as_any(&self) -> &dyn Any;
    fn as_any_mut(&mut self) -> &mut dyn Any;

    /// Unique identifier for this application instance.
    fn id(&self) -> AppId;

    /// Display name shown in the tab bar.
    fn name(&self, editor: &Editor) -> String;

    /// Process input events.
    fn handle_event(&mut self, event: &Event, ctx: &mut Context) -> EventResult;

    /// Render the application into the given area.
    fn render(&mut self, area: Rect, surface: &mut Surface, ctx: &mut Context);

    /// Get cursor position and kind.
    fn cursor(&self, area: Rect, editor: &Editor) -> (Option<Position>, CursorKind) {
        let _ = (area, editor);
        (None, CursorKind::Hidden)
    }

    /// Pending key sequence to display in the commandline area.
    fn pending_keys(&self) -> String {
        String::new()
    }

    /// Optional file path for icon lookup in the tab bar.
    fn icon_path(&self, _editor: &Editor) -> Option<PathBuf> {
        None
    }
}
