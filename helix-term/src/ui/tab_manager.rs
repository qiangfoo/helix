use std::path::PathBuf;
use std::sync::Arc;

use arc_swap::ArcSwap;
use helix_core::unicode::width::UnicodeWidthStr;
use helix_view::{
    document::SCRATCH_BUFFER_NAME,
    graphics::{Color, CursorKind, Rect},
    input::{MouseButton, MouseEventKind},
    Editor,
};
use tui::buffer::Buffer as Surface;

use crate::compositor::{self, Component, Context, EventResult};
use crate::config::Config;
use crate::keymap::Keymaps;

use super::app::Application;
use super::EditorView;

pub struct TabManager {
    tabs: Vec<Box<dyn Application>>,
    active: usize,
    tab_regions: Vec<(u16, u16, usize)>, // (x_start, x_end, tab_index)
    config: Arc<ArcSwap<Config>>,
}

impl TabManager {
    pub fn new(config: Arc<ArcSwap<Config>>) -> Self {
        Self {
            tabs: Vec::new(),
            active: 0,
            tab_regions: Vec::new(),
            config,
        }
    }

    fn make_keymaps(&self) -> Keymaps {
        let keys = Box::new(arc_swap::access::Map::new(
            Arc::clone(&self.config),
            |config: &Config| &config.keys,
        ));
        Keymaps::new(keys)
    }

    /// Create a new EditorView tab for the given document.
    /// Adds the DocView to `editor.tabs` and creates a thin EditorView shell.
    pub fn new_editor_tab(&mut self, doc: helix_view::Document, editor: &mut helix_view::Editor) {
        let dv = helix_view::DocView::new(doc);
        let tab_index = editor.add_tab(dv);
        let keymaps = self.make_keymaps();
        let editor_view = Box::new(EditorView::new(keymaps, tab_index));
        self.add_tab(editor_view);
        editor.active_tab = tab_index;
    }

    /// Add a tab and make it active.
    pub fn add_tab(&mut self, app: Box<dyn Application>) {
        self.tabs.push(app);
        self.active = self.tabs.len() - 1;
    }

    /// Close the tab at `index`. Returns true if there are no tabs left.
    pub fn close_tab(&mut self, index: usize) -> bool {
        if index >= self.tabs.len() {
            return self.tabs.is_empty();
        }
        self.tabs.remove(index);
        if self.tabs.is_empty() {
            return true;
        }
        if self.active >= self.tabs.len() {
            self.active = self.tabs.len() - 1;
        } else if self.active > index {
            self.active -= 1;
        }
        false
    }

    pub fn next_tab(&mut self) {
        if !self.tabs.is_empty() {
            self.active = (self.active + 1) % self.tabs.len();
        }
    }

    pub fn prev_tab(&mut self) {
        if !self.tabs.is_empty() {
            self.active = if self.active == 0 {
                self.tabs.len() - 1
            } else {
                self.active - 1
            };
        }
    }

    pub fn activate_tab(&mut self, index: usize) {
        if index < self.tabs.len() {
            self.active = index;
        }
    }

    pub fn active_tab(&self) -> Option<&dyn Application> {
        self.tabs.get(self.active).map(|t| t.as_ref())
    }

    pub fn active_tab_mut(&mut self) -> Option<&mut Box<dyn Application>> {
        self.tabs.get_mut(self.active)
    }

    pub fn tab_count(&self) -> usize {
        self.tabs.len()
    }

    pub fn tabs(&self) -> &[Box<dyn Application>] {
        &self.tabs
    }

    pub fn active_index(&self) -> usize {
        self.active
    }

    /// Sync TabManager's shell list with Editor's tabs.
    /// Removes extra shells and updates tab_index on each EditorView.
    fn sync_with_editor(&mut self, editor: &helix_view::Editor) {
        // Remove shells if editor has fewer tabs
        while self.tabs.len() > editor.tab_count() {
            self.tabs.pop();
        }
        // Update active index
        self.active = editor.active_tab;
        // Update tab_index on each EditorView shell
        for (i, tab) in self.tabs.iter_mut().enumerate() {
            if let Some(ev) = tab.as_any_mut().downcast_mut::<EditorView>() {
                ev.tab_index = i;
            }
        }
    }

    /// Find the first EditorView tab (for spinner/keymap access).
    pub fn editor_view(&mut self) -> Option<&mut EditorView> {
        self.tabs
            .iter_mut()
            .find_map(|tab| tab.as_any_mut().downcast_mut::<EditorView>())
    }

    /// Find the active tab's EditorView (if it is one).
    pub fn active_editor_view(&mut self) -> Option<&mut EditorView> {
        self.tabs
            .get_mut(self.active)
            .and_then(|tab| tab.as_any_mut().downcast_mut::<EditorView>())
    }

    fn render_tab_bar(&mut self, editor: &Editor, viewport: Rect, surface: &mut Surface) {
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

        for (i, tab) in self.tabs.iter().enumerate() {
            let name = tab.name(editor);
            let is_active = i == self.active;

            let style = if is_active {
                tab_active_style
                    .underline_style(helix_view::graphics::UnderlineStyle::Reset)
                    .underline_color(Color::Reset)
            } else {
                tab_inactive_style.bg(Color::Reset)
            };

            let used_width = x.saturating_sub(viewport.x);
            let rem_width = surface.area.width.saturating_sub(used_width);

            let x_start = x;

            // Render icon + name on row 2 (bottom row of tab bar)
            if editor.config().icons {
                let scratch = PathBuf::from(SCRATCH_BUFFER_NAME);
                let icon_path = tab.icon_path(editor);
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

            // Draw top-bar indicator on first row for active tab
            if is_active && viewport.height > 1 {
                let indicator_style = tab_active_style
                    .underline_style(helix_view::graphics::UnderlineStyle::Reset)
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

        // Render status message
        if let Some((status_msg, severity)) = &editor.status_msg {
            status_msg_width = status_msg.width();
            use helix_view::editor::Severity;
            let style = if *severity == Severity::Error {
                editor.theme.get("error")
            } else {
                editor.theme.get("ui.text")
            };

            surface.set_string(area.x, area.y, status_msg, style);
        }

        // Render pending keys from active tab
        if area.width.saturating_sub(status_msg_width as u16) > key_width {
            let mut disp = String::new();
            if let Some(count) = editor.tabs[editor.active_tab].count {
                disp.push_str(&count.to_string());
            }
            if let Some(tab) = self.active_tab() {
                disp.push_str(&tab.pending_keys());
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
                        self.activate_tab(tab_index);
                        ctx.editor.active_tab = tab_index;
                        return EventResult::Consumed(None);
                    }
                }
            }
        }

        // Sync shells with editor's tab state
        self.sync_with_editor(ctx.editor);

        // Delegate to active tab
        if let Some(tab) = self.tabs.get_mut(self.active) {
            tab.handle_event(event, ctx)
        } else {
            EventResult::Ignored(None)
        }
    }

    fn render(&mut self, area: Rect, surface: &mut Surface, cx: &mut Context) {
        // Sync shells with editor's tab state
        self.sync_with_editor(cx.editor);

        // Clear with background color
        surface.set_style(area, cx.editor.theme.get("ui.background"));

        use helix_view::editor::BufferLine;
        let show_tab_bar = match cx.editor.config().bufferline {
            BufferLine::Always => true,
            BufferLine::Multiple if self.tabs.len() > 1 => true,
            _ => false,
        };
        let tab_bar_height: u16 = if show_tab_bar { 2 } else { 0 };
        let commandline_height: u16 = 1;

        // Compute main area
        let main_area = area
            .clip_top(tab_bar_height)
            .clip_bottom(commandline_height);

        // Render tab bar
        if tab_bar_height > 0 {
            self.render_tab_bar(cx.editor, area.with_height(tab_bar_height), surface);
        }

        // Render commandline
        let commandline_area = Rect::new(
            area.x,
            area.y + area.height.saturating_sub(commandline_height),
            area.width,
            commandline_height,
        );
        self.render_commandline(cx.editor, commandline_area, surface);

        // Each tab owns its own document/tree — no need to sync with Editor

        // Render active tab
        if let Some(tab) = self.tabs.get_mut(self.active) {
            tab.render(main_area, surface, cx);
        }
    }

    fn cursor(&self, area: Rect, editor: &Editor) -> (Option<helix_core::Position>, CursorKind) {
        if let Some(tab) = self.active_tab() {
            tab.cursor(area, editor)
        } else {
            (None, CursorKind::Hidden)
        }
    }
}
