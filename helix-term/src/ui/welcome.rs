use std::any::Any;
use std::path::PathBuf;

use crate::view::graphics::{CursorKind, Rect};
use crate::view::input::KeyEvent;
use crate::view::keyboard::KeyCode;
use crate::view::Editor;
use ratatui::buffer::Buffer as Surface;

use crate::compositor::{self, Context, EventResult};
use crate::ui;
use crate::ui::overlay::overlaid;

use super::app::{AppId, Application, Event};

pub struct WelcomePage {
    app_id: AppId,
}

impl WelcomePage {
    pub fn new() -> Self {
        Self {
            app_id: AppId::next(),
        }
    }

    fn handle_key_event(&mut self, key: KeyEvent, _ctx: &mut Context) -> EventResult {
        match key {
            // 'f' opens file picker
            KeyEvent {
                code: KeyCode::Char('f'),
                ..
            } => {
                let root = helix_core::find_workspace().0;
                if root.exists() {
                    let callback: compositor::Callback = Box::new(
                        move |editor: &mut crate::view::Editor| {
                            use crate::layers::EditorLayers;
                            let picker = ui::file_picker(editor, root);
                            editor.push_layer(Box::new(overlaid(picker)));
                        },
                    );
                    EventResult::Consumed(Some(callback))
                } else {
                    EventResult::Consumed(None)
                }
            }
            // 'e' opens file explorer
            KeyEvent {
                code: KeyCode::Char('e'),
                ..
            } => {
                let callback: compositor::Callback = Box::new(
                    move |editor: &mut crate::view::Editor| {
                        use crate::layers::EditorLayers;
                        let cwd = helix_stdx::env::current_working_dir();
                        if let Ok(picker) = ui::file_explorer(cwd, editor) {
                            editor.push_layer(Box::new(overlaid(picker)));
                        }
                    },
                );
                EventResult::Consumed(Some(callback))
            }
            _ => EventResult::Ignored(None),
        }
    }
}

impl Application for WelcomePage {
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }

    fn id(&self) -> AppId {
        self.app_id
    }

    fn name(&self, _editor: &Editor) -> String {
        "Welcome".to_string()
    }

    fn handle_event(&mut self, event: &Event, ctx: &mut Context) -> EventResult {
        match event {
            Event::Key(key) => self.handle_key_event(*key, ctx),
            _ => EventResult::Ignored(None),
        }
    }

    fn render(&mut self, area: Rect, surface: &mut Surface, cx: &mut Context) {
        let text_style = cx.editor.theme.get("ui.text");
        let dim_style = cx.editor.theme.get("ui.text.inactive");

        let cwd = helix_stdx::env::current_working_dir();
        let project_name = cwd
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| cwd.display().to_string());

        let branch = helix_vcs::get_branch_name(&cwd);

        let mut lines: Vec<(&str, String)> = Vec::new();

        // Project name
        lines.push(("text", project_name.clone()));

        // Branch
        if let Some(ref branch) = branch {
            lines.push(("dim", format!("on {}", branch)));
        }

        // Blank line
        lines.push(("dim", String::new()));

        // Keybinding hints
        lines.push(("dim", "f   file picker".to_string()));
        lines.push(("dim", "e   file explorer".to_string()));
        lines.push(("dim", ":   command mode".to_string()));

        // Center vertically
        let total_lines = lines.len() as u16;
        let start_y = area.y + area.height.saturating_sub(total_lines) / 2;

        for (i, (kind, line)) in lines.iter().enumerate() {
            let y = start_y + i as u16;
            if y >= area.y + area.height {
                break;
            }

            let style = match *kind {
                "dim" => dim_style,
                _ => text_style,
            };

            // Center horizontally
            let line_width = line.len() as u16;
            let x = area.x + area.width.saturating_sub(line_width) / 2;
            surface.set_string(x, y, line, style);
        }
    }

    fn cursor(&self, _area: Rect, _editor: &Editor) -> (Option<helix_core::Position>, CursorKind) {
        (None, CursorKind::Hidden)
    }

    fn icon_path(&self, _editor: &Editor) -> Option<PathBuf> {
        None
    }
}
