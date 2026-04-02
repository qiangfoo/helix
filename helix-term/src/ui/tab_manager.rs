use std::mem::take;
use std::path::PathBuf;
use std::sync::Arc;

use arc_swap::ArcSwap;
use helix_core::unicode::width::UnicodeWidthStr;
use crate::view::{
    document::SCRATCH_BUFFER_NAME,
    graphics::{Color, CursorKind, Rect, RectExt},
    input::{MouseButton, MouseEventKind},
    Editor,
};
use ratatui::buffer::Buffer as Surface;

use crate::commands;
use crate::compositor::{self, Component, Context, EventResult};
use crate::config::Config;
use crate::events::{OnModeSwitch, PostCommand};
use crate::keymap::{KeymapResult, Keymaps};

use super::app::EditorApps;
use super::EditorView;

pub struct TabManager {
    tab_regions: Vec<(u16, u16, usize)>, // (x_start, x_end, tab_index)
    keymaps: Keymaps,
}

impl TabManager {
    pub fn new(config: Arc<ArcSwap<Config>>) -> Self {
        let keymaps = {
            let keys = Box::new(arc_swap::access::Map::new(
                Arc::clone(&config),
                |config: &Config| &config.keys,
            ));
            Keymaps::new(keys)
        };
        Self {
            tab_regions: Vec::new(),
            keymaps,
        }
    }

    /// Global key handler — fallback for events not handled by the active app.
    /// Uses stateless keymaps lookup + pushes KeyMenu on Pending.
    fn global_handle_key_event(
        &self,
        mut key: crate::view::input::KeyEvent,
        ctx: &mut Context,
    ) -> EventResult {
        crate::ui::editor::canonicalize_key(&mut key);
        ctx.editor.status_msg = None;
        ctx.editor.reset_idle_timer();

        let mut cx = commands::Context {
            editor: ctx.editor,
            count: None,
            callback: Vec::new(),
            jobs: ctx.jobs,
        };

        let mode = cx.editor.mode();
        let key_result = self.keymaps.get(mode, key);

        let mut last_mode = mode;
        let mut execute_command = |command: &commands::MappableCommand| {
            command.execute(&mut cx);
            helix_event::dispatch(PostCommand { command, cx: &mut cx });
            let current_mode = cx.editor.mode();
            if current_mode != last_mode {
                helix_event::dispatch(OnModeSwitch {
                    old_mode: last_mode,
                    new_mode: current_mode,
                    cx: &mut cx,
                });
            }
            last_mode = current_mode;
        };

        match &key_result {
            KeymapResult::Matched(command) => {
                execute_command(command);
            }
            KeymapResult::Pending(node) => {
                // Push KeyMenu as overlay for multi-key sequence
                let menu = crate::ui::key_menu::KeyMenu::new(node.clone());
                cx.push_layer(Box::new(menu));
            }
            KeymapResult::MatchedSequence(commands) => {
                for command in commands {
                    execute_command(command);
                }
            }
            KeymapResult::NotFound => {
                return EventResult::Ignored(None);
            }
        }

        let callbacks = take(&mut cx.callback);

        if cx.editor.should_close() {
            return EventResult::Ignored(None);
        }

        let callback = if callbacks.is_empty() {
            None
        } else {
            let callback: crate::compositor::Callback =
                Box::new(move |editor: &mut Editor| {
                    for callback in callbacks {
                        callback(editor)
                    }
                });
            Some(callback)
        };

        EventResult::Consumed(callback)
    }

    // ------------------------------------------------------------------
    // Rendering helpers
    // ------------------------------------------------------------------

    fn render_tab_bar(
        &mut self,
        editor: &Editor,
        viewport: Rect,
        surface: &mut Surface,
    ) {
        let tab_active_style = editor
            .theme
            .try_get("ui.bufferline.active")
            .unwrap_or_else(|| editor.theme.get("ui.statusline.active"));

        let tab_inactive_style = editor
            .theme
            .try_get("ui.bufferline")
            .unwrap_or_else(|| editor.theme.get("ui.statusline.inactive"));

        let mut x = viewport.x;
        self.tab_regions.clear();

        for i in 0..editor.apps.len() {
            let app = &editor.apps[i];
            let name = app.name(editor);
            let is_active = i == editor.active_app;

            let style = if is_active {
                tab_active_style
                    .underline_style(crate::view::graphics::UnderlineStyle::Reset)
                    .underline_color(Color::Reset)
            } else {
                tab_inactive_style.bg(Color::Reset)
            };

            let used_width = x.saturating_sub(viewport.x);
            let rem_width = surface.area.width.saturating_sub(used_width);

            let x_start = x;

            if editor.config().icons {
                let scratch = PathBuf::from(SCRATCH_BUFFER_NAME);
                let icon_path = app.icon_path(editor);
                let icon =
                    crate::ui::icons::file_icon(icon_path.as_ref().unwrap_or(&scratch));
                let icon_style = style.fg(icon.color);
                x = surface
                    .set_stringn(
                        x,
                        viewport.y + 1,
                        &format!(" {} ", icon.icon),
                        rem_width as usize,
                        icon_style,
                    )
                    .0;
                let rem_width = surface.area.width.saturating_sub(x);
                x = surface
                    .set_stringn(
                        x,
                        viewport.y + 1,
                        &format!("{} ", name),
                        rem_width as usize,
                        style,
                    )
                    .0;
            } else {
                x = surface
                    .set_stringn(
                        x,
                        viewport.y + 1,
                        &format!(" {} ", name),
                        rem_width as usize,
                        style,
                    )
                    .0;
            }

            self.tab_regions.push((x_start, x, i));

            if is_active && viewport.height > 1 {
                let indicator_style = tab_active_style
                    .underline_style(crate::view::graphics::UnderlineStyle::Reset)
                    .underline_color(Color::Reset);
                for ix in x_start..x {
                    surface.set_stringn(ix, viewport.y, "▔", 1, indicator_style);
                }
            }

            if x >= surface.area.right() {
                break;
            }
        }
    }

    fn render_commandline(
        &self,
        editor: &Editor,
        area: Rect,
        surface: &mut Surface,
    ) {
        let key_width = 15u16;
        let mut status_msg_width = 0;

        if let Some((status_msg, severity)) = &editor.status_msg {
            status_msg_width = status_msg.width();
            use crate::view::editor::Severity;
            let style = if *severity == Severity::Error {
                editor.theme.get("error")
            } else {
                editor.theme.get("ui.text")
            };

            surface.set_string(area.x, area.y, status_msg, style);
        }

        if area.width.saturating_sub(status_msg_width as u16) > key_width {
            let mut disp = String::new();
            if let Some(dv) = editor.active_doc_view() {
                if let Some(count) = dv.count {
                    disp.push_str(&count.to_string());
                }
            }
            if let Some(app) = editor.apps.get(editor.active_app) {
                disp.push_str(&app.pending_keys());
            }

            let style = editor.theme.get("ui.text");
            surface.set_string(
                area.x + area.width.saturating_sub(key_width),
                area.y,
                disp.get(disp.len().saturating_sub(key_width as usize)..)
                    .unwrap_or(&disp),
                style,
            );
        }
    }

    /// Clean up stale EditorView entries whose backing DocView no longer exists.
    fn cleanup_stale_shells(editor: &mut Editor) {
        if editor.apps.len() == 1 {
            if editor.apps[0].as_any().downcast_ref::<super::welcome::WelcomePage>().is_some() {
                return;
            }
        }

        // Remove EditorView apps whose DocView no longer exists
        editor.apps.retain(|app| {
            if let Some(ev) = app.as_any().downcast_ref::<EditorView>() {
                return editor.doc_views.contains_key(&ev.id());
            }
            true
        });

        if editor.apps.is_empty() {
            editor.apps.push(Box::new(super::welcome::WelcomePage::new()));
            editor.active_app = 0;
            return;
        }

        if editor.active_app >= editor.apps.len() {
            editor.active_app = editor.apps.len().saturating_sub(1);
        }
    }
}

impl Component for TabManager {
    fn handle_event(
        &mut self,
        event: &compositor::Event,
        ctx: &mut Context,
    ) -> EventResult {
        // Handle mouse clicks on the tab bar
        if let compositor::Event::Mouse(mouse) = event {
            if let MouseEventKind::Down(MouseButton::Left) = mouse.kind {
                let row = mouse.row;
                let column = mouse.column;
                if row < 2 {
                    if let Some(&(_, _, tab_index)) = self
                        .tab_regions
                        .iter()
                        .find(|(x_start, x_end, _)| column >= *x_start && column < *x_end)
                    {
                        ctx.editor.switch_app(tab_index);
                        return EventResult::Consumed(None);
                    }
                }
            }
        }

        // Global keymaps — handle key events that fell through from ActiveAppProxy
        if let compositor::Event::Key(key) = event {
            return self.global_handle_key_event(*key, ctx);
        }

        EventResult::Ignored(None)
    }

    fn render(&mut self, area: Rect, surface: &mut Surface, cx: &mut Context) {
        Self::cleanup_stale_shells(cx.editor);

        surface.set_style(area, cx.editor.theme.get("ui.background"));

        use crate::view::editor::BufferLine;
        let app_count = cx.editor.app_count();
        let show_tab_bar = match cx.editor.config().bufferline {
            BufferLine::Always => true,
            BufferLine::Multiple if app_count > 1 => true,
            _ => false,
        };
        let tab_bar_height: u16 = if show_tab_bar { 2 } else { 0 };
        let commandline_height: u16 = 1;

        let main_area = area
            .clip_top(tab_bar_height)
            .clip_bottom(commandline_height);
        cx.editor.main_area = main_area;

        if tab_bar_height > 0 {
            self.render_tab_bar(cx.editor, area.with_height(tab_bar_height), surface);
        }

        let commandline_area = Rect::new(
            area.x,
            area.y + area.height.saturating_sub(commandline_height),
            area.width,
            commandline_height,
        );
        self.render_commandline(cx.editor, commandline_area, surface);

        // NOTE: Active app rendering is handled by ActiveAppProxy in the layer stack.
        // NOTE: Autoinfo/KeyMenu rendering is handled by KeyMenu component in the layer stack.
    }

    fn cursor(&self, _area: Rect, _editor: &Editor) -> (Option<helix_core::Position>, CursorKind) {
        (None, CursorKind::Hidden)
    }
}
