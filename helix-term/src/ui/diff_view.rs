use std::any::Any;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use helix_core::text_annotations::TextAnnotations;
use helix_core::Rope;
use crate::view::graphics::{Color, CursorKind, Rect, RectExt};
use crate::view::input::{KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use crate::view::keyboard::{KeyCode, KeyModifiers};
use crate::view::theme::Style;
use crate::view::view::ViewPosition;
use crate::view::{Document, Editor};

/// Dim an RGB color for use as a subtle background tint.
fn dim_color(color: Color) -> Color {
    match color {
        Color::Rgb(r, g, b) => Color::Rgb(r / 5, g / 5, b / 5),
        _ => color,
    }
}
use ratatui::buffer::Buffer as Surface;

use helix_vcs::{DiffLineKind, FileDiff};

use crate::compositor::{Context, EventResult};
use crate::ui::document::{render_document, LinePos};
use crate::ui::text_decorations::DecorationManager;
use crate::ui::EditorView;

use super::app::{AppId, Application, Event};

/// Identifier for deduplicating DiffView tabs.
#[derive(Clone, PartialEq, Eq)]
pub enum DiffKey {
    LocalChanges,
    CommitDiff { hash: String },
}

/// A processed file entry ready for rendering.
struct DiffFileEntry {
    rel_path: String,
    change_kind: char,
    display_lines: Vec<DisplayLine>,
}

/// A single line in the diff display.
struct DisplayLine {
    kind: DiffLineKind,
    text: String,
    old_lineno: Option<usize>,
    new_lineno: Option<usize>,
}

pub struct DiffView {
    app_id: AppId,
    diff_key: DiffKey,
    cwd: PathBuf,
    files: Vec<DiffFileEntry>,
    selected_file: usize,
    scroll_offset: usize,
    /// Lazily created Documents for syntax highlighting per file index.
    highlight_cache: HashMap<usize, Document>,
    /// Last rendered sidebar area, for mouse hit detection.
    sidebar_area: Rect,
}

impl DiffView {
    pub fn new(
        diff_key: DiffKey,
        cwd: PathBuf,
        file_diffs: Vec<FileDiff>,
        editor: &Editor,
    ) -> Self {
        let files = file_diffs
            .into_iter()
            .map(|fd| build_file_entry(fd))
            .collect();
        let _ = editor; // used in future for highlight_cache warm-up
        Self {
            app_id: AppId::next(),
            diff_key,
            cwd,
            files,
            selected_file: 0,
            scroll_offset: 0,
            highlight_cache: HashMap::new(),
            sidebar_area: Rect::default(),
        }
    }

    pub fn diff_key(&self) -> &DiffKey {
        &self.diff_key
    }

    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// Refresh with new file diffs (e.g. after file watcher triggers).
    pub fn refresh(&mut self, file_diffs: Vec<FileDiff>) {
        let selected_path = self
            .files
            .get(self.selected_file)
            .map(|f| f.rel_path.clone());
        self.files = file_diffs
            .into_iter()
            .map(|fd| build_file_entry(fd))
            .collect();
        self.selected_file = selected_path
            .and_then(|p| self.files.iter().position(|f| f.rel_path == p))
            .unwrap_or(0);
        self.highlight_cache.clear();
        self.scroll_offset = 0;
    }

    fn ensure_highlight_doc(&mut self, editor: &Editor) {
        let idx = self.selected_file;
        if self.highlight_cache.contains_key(&idx) {
            return;
        }
        if let Some(entry) = self.files.get(idx) {
            // Build a document from ALL display lines (including hunk headers)
            // so that document line N == display line N for correct decoration alignment.
            let text: String = entry
                .display_lines
                .iter()
                .map(|l| format!("{}\n", l.text))
                .collect();
            let rope = Rope::from(text.as_str());
            let mut doc = Document::from(
                rope,
                None,
                editor.config.clone(),
                editor.syn_loader.clone(),
            );
            // Set the path so language detection works from the extension
            doc.set_path(Some(Path::new(&entry.rel_path)));
            let loader = editor.syn_loader.load();
            doc.detect_language(&loader);
            self.highlight_cache.insert(idx, doc);
        }
    }

    fn handle_key_event(&mut self, key: KeyEvent, _ctx: &mut Context) -> EventResult {
        match key {
            // Ctrl+n: next file
            KeyEvent {
                code: KeyCode::Char('n'),
                modifiers,
            } if modifiers.contains(KeyModifiers::CONTROL) => {
                if !self.files.is_empty() {
                    self.selected_file = (self.selected_file + 1) % self.files.len();
                    self.scroll_offset = 0;
                }
                EventResult::Consumed(None)
            }
            // Ctrl+p: prev file
            KeyEvent {
                code: KeyCode::Char('p'),
                modifiers,
            } if modifiers.contains(KeyModifiers::CONTROL) => {
                if !self.files.is_empty() {
                    self.selected_file = if self.selected_file == 0 {
                        self.files.len() - 1
                    } else {
                        self.selected_file - 1
                    };
                    self.scroll_offset = 0;
                }
                EventResult::Consumed(None)
            }
            // j / Down: scroll down
            KeyEvent {
                code: KeyCode::Char('j') | KeyCode::Down,
                ..
            } => {
                self.scroll_offset = self.scroll_offset.saturating_add(1);
                self.clamp_scroll();
                EventResult::Consumed(None)
            }
            // k / Up: scroll up
            KeyEvent {
                code: KeyCode::Char('k') | KeyCode::Up,
                ..
            } => {
                self.scroll_offset = self.scroll_offset.saturating_sub(1);
                EventResult::Consumed(None)
            }
            // Ctrl+d: page down
            KeyEvent {
                code: KeyCode::Char('d'),
                modifiers,
            } if modifiers.contains(KeyModifiers::CONTROL) => {
                self.scroll_offset = self.scroll_offset.saturating_add(20);
                self.clamp_scroll();
                EventResult::Consumed(None)
            }
            // Ctrl+u: page up
            KeyEvent {
                code: KeyCode::Char('u'),
                modifiers,
            } if modifiers.contains(KeyModifiers::CONTROL) => {
                self.scroll_offset = self.scroll_offset.saturating_sub(20);
                EventResult::Consumed(None)
            }
            // g: top
            KeyEvent {
                code: KeyCode::Char('g'),
                ..
            } => {
                self.scroll_offset = 0;
                EventResult::Consumed(None)
            }
            // G: bottom
            KeyEvent {
                code: KeyCode::Char('G'),
                ..
            } => {
                if let Some(entry) = self.files.get(self.selected_file) {
                    self.scroll_offset = entry.display_lines.len().saturating_sub(1);
                }
                EventResult::Consumed(None)
            }
            _ => EventResult::Ignored(None),
        }
    }

    fn handle_mouse_event(&mut self, mouse: &MouseEvent) -> EventResult {
        let sidebar = self.sidebar_area;
        let in_sidebar = mouse.row >= sidebar.y
            && mouse.row < sidebar.y + sidebar.height
            && mouse.column >= sidebar.x
            && mouse.column < sidebar.x + sidebar.width;

        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if in_sidebar {
                    let clicked_idx = (mouse.row - sidebar.y) as usize;
                    if clicked_idx < self.files.len() {
                        self.selected_file = clicked_idx;
                        self.scroll_offset = 0;
                    }
                    return EventResult::Consumed(None);
                }
                EventResult::Consumed(None)
            }
            MouseEventKind::ScrollUp => {
                if !in_sidebar {
                    self.scroll_offset = self.scroll_offset.saturating_sub(3);
                }
                EventResult::Consumed(None)
            }
            MouseEventKind::ScrollDown => {
                if !in_sidebar {
                    self.scroll_offset = self.scroll_offset.saturating_add(3);
                    self.clamp_scroll();
                }
                EventResult::Consumed(None)
            }
            _ => EventResult::Ignored(None),
        }
    }

    fn clamp_scroll(&mut self) {
        if let Some(entry) = self.files.get(self.selected_file) {
            let max = entry.display_lines.len().saturating_sub(1);
            if self.scroll_offset > max {
                self.scroll_offset = max;
            }
        }
    }

    fn render_sidebar(&self, area: Rect, surface: &mut Surface, editor: &Editor) {
        let active_style = editor
            .theme
            .try_get("ui.bufferline.active")
            .unwrap_or_else(|| editor.theme.get("ui.statusline.active"));
        let inactive_style = editor
            .theme
            .try_get("ui.bufferline")
            .unwrap_or_else(|| editor.theme.get("ui.statusline.inactive"));
        let added_style = editor.theme.get("diff.plus");
        let deleted_style = editor.theme.get("diff.minus");
        let modified_style = editor.theme.get("diff.delta");

        let icons_enabled = editor.config().icons;

        for (i, entry) in self.files.iter().enumerate() {
            let y = area.y + i as u16;
            if y >= area.y + area.height {
                break;
            }

            let is_selected = i == self.selected_file;
            let base_style = if is_selected {
                active_style
            } else {
                inactive_style
            };

            // Clear the line
            for x in area.x..area.x + area.width {
                surface.set_stringn(x, y, " ", 1, base_style);
            }

            let mut x = area.x;
            let x_end = area.x + area.width;
            let rem = |x: u16| x_end.saturating_sub(x) as usize;

            // Change kind indicator
            let (kind_char, kind_style) = match entry.change_kind {
                'A' => ("A ", added_style),
                'D' => ("D ", deleted_style),
                'M' => ("M ", modified_style),
                'R' => ("R ", modified_style),
                _ => ("? ", base_style),
            };
            x = surface
                .set_stringn(x, y, kind_char, rem(x), kind_style.patch(base_style))
                .0;

            // File icon
            if icons_enabled {
                let icon = crate::ui::icons::file_icon(Path::new(&entry.rel_path));
                let icon_style = base_style.fg(icon.color);
                x = surface
                    .set_stringn(x, y, &format!("{} ", icon.icon), rem(x), icon_style)
                    .0;
            }

            // Filename (just the file name, not the full path)
            let display_name = Path::new(&entry.rel_path)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| entry.rel_path.clone());
            surface.set_stringn(x, y, &display_name, rem(x), base_style);
        }
    }

    fn render_diff_content(
        &mut self,
        area: Rect,
        surface: &mut Surface,
        cx: &mut Context,
    ) {
        let entry = match self.files.get(self.selected_file) {
            Some(e) => e,
            None => return,
        };

        if entry.display_lines.is_empty() {
            return;
        }

        // Build background styles: take the fg color from diff.plus/minus and
        // use it as a dimmed background so syntax-highlighted text remains readable.
        let added_bg = {
            let diff_style = cx.editor.theme.get("diff.plus");
            match diff_style.fg {
                Some(fg) => Style::default().bg(dim_color(fg)),
                None => Style::default().bg(crate::view::graphics::Color::Rgb(0, 40, 0)),
            }
        };
        let deleted_bg = {
            let diff_style = cx.editor.theme.get("diff.minus");
            match diff_style.fg {
                Some(fg) => Style::default().bg(dim_color(fg)),
                None => Style::default().bg(crate::view::graphics::Color::Rgb(40, 0, 0)),
            }
        };
        let hunk_header_style = cx
            .editor
            .theme
            .try_get("ui.statusline.inactive")
            .unwrap_or_else(|| cx.editor.theme.get("ui.text.inactive"));
        let linenr_style = cx.editor.theme.get("ui.linenr");

        // Gutter width: "old | new " = ~5+5+3 = 13 chars
        let gutter_width: u16 = 13;
        let gutter_area = Rect::new(area.x, area.y, gutter_width.min(area.width), area.height);
        let content_area = area.clip_left(gutter_width);

        if content_area.width == 0 {
            return;
        }

        self.ensure_highlight_doc(cx.editor);

        if self.highlight_cache.contains_key(&self.selected_file) {
            self.render_with_syntax(
                content_area,
                gutter_area,
                surface,
                cx,
                added_bg,
                deleted_bg,
                hunk_header_style,
                linenr_style,
            );
        }
    }

    fn render_with_syntax(
        &self,
        content_area: Rect,
        gutter_area: Rect,
        surface: &mut Surface,
        cx: &mut Context,
        added_bg: Style,
        deleted_bg: Style,
        hunk_header_style: Style,
        linenr_style: Style,
    ) {
        let entry = &self.files[self.selected_file];
        let doc = &self.highlight_cache[&self.selected_file];

        // Now document line N == display line N (hunk headers are included).
        let scroll_line = self.scroll_offset;

        // Set up view position
        let text = doc.text().slice(..);
        let anchor = if scroll_line < doc.text().len_lines() {
            text.line_to_char(scroll_line)
        } else {
            text.len_chars()
        };
        let offset = ViewPosition {
            anchor,
            horizontal_offset: 0,
            vertical_offset: 0,
        };

        let loader = cx.editor.syn_loader.load();
        let syntax_highlighter =
            EditorView::doc_syntax_highlighter(doc, offset.anchor, content_area.height, &loader);

        // Build decoration that sets line backgrounds based on diff line kind
        let line_kinds: Vec<DiffLineKind> = entry
            .display_lines
            .iter()
            .map(|dl| dl.kind)
            .collect();

        let draw_diff_bg = {
            let added_bg = added_bg;
            let deleted_bg = deleted_bg;
            let hunk_header_style = hunk_header_style;
            let line_kinds = line_kinds.clone();
            move |renderer: &mut crate::ui::document::TextRenderer, pos: LinePos| {
                if let Some(&kind) = line_kinds.get(pos.doc_line) {
                    let style = match kind {
                        DiffLineKind::Added => added_bg,
                        DiffLineKind::Deleted => deleted_bg,
                        DiffLineKind::HunkHeader => hunk_header_style,
                        DiffLineKind::Context => return,
                    };
                    let line_area = Rect::new(
                        renderer.viewport.x,
                        pos.visual_line,
                        renderer.viewport.width,
                        1,
                    );
                    renderer.set_style(line_area, style);
                }
            }
        };

        let mut decorations = DecorationManager::default();
        decorations.add_decoration(draw_diff_bg);

        render_document(
            surface,
            content_area,
            doc,
            offset,
            &TextAnnotations::default(),
            syntax_highlighter,
            Vec::new(),
            &cx.editor.theme,
            decorations,
        );

        // Render gutter for visible lines
        let mut visual_y = 0u16;
        for i in scroll_line..entry.display_lines.len() {
            if visual_y >= content_area.height {
                break;
            }
            let dl = &entry.display_lines[i];
            let y = gutter_area.y + visual_y;

            if dl.kind == DiffLineKind::HunkHeader {
                // Fill gutter with hunk header style
                for x in gutter_area.x..gutter_area.x + gutter_area.width {
                    surface.set_stringn(x, y, " ", 1, hunk_header_style);
                }
            } else {
                self.render_gutter_line(gutter_area, y, dl, linenr_style, surface);
            }

            visual_y += 1;
        }
    }

    fn render_gutter_line(
        &self,
        gutter_area: Rect,
        y: u16,
        dl: &DisplayLine,
        linenr_style: Style,
        surface: &mut Surface,
    ) {
        let old_str = dl
            .old_lineno
            .map(|n| format!("{:>5}", n))
            .unwrap_or_else(|| "     ".to_string());
        let new_str = dl
            .new_lineno
            .map(|n| format!("{:<5}", n))
            .unwrap_or_else(|| "     ".to_string());
        let gutter_text = format!("{} {} ", old_str, new_str);
        surface.set_stringn(
            gutter_area.x,
            y,
            &gutter_text,
            gutter_area.width as usize,
            linenr_style,
        );
    }
}

impl Application for DiffView {
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
        match &self.diff_key {
            DiffKey::LocalChanges => "Local Changes".to_string(),
            DiffKey::CommitDiff { hash } => {
                let short = if hash.len() > 7 { &hash[..7] } else { hash };
                format!("[{short}]")
            }
        }
    }

    fn handle_event(&mut self, event: &Event, ctx: &mut Context) -> EventResult {
        match event {
            Event::Key(key) => self.handle_key_event(*key, ctx),
            Event::Mouse(mouse) => self.handle_mouse_event(mouse),
            _ => EventResult::Ignored(None),
        }
    }

    fn render(&mut self, area: Rect, surface: &mut Surface, cx: &mut Context) {
        let bg = cx.editor.theme.get("ui.background");
        surface.set_style(area, bg);

        if self.files.is_empty() {
            let text_style = cx.editor.theme.get("ui.text.inactive");
            let msg = "No changes";
            let x = area.x + area.width.saturating_sub(msg.len() as u16) / 2;
            let y = area.y + area.height / 2;
            surface.set_string(x, y, msg, text_style);
            return;
        }

        // Layout: sidebar | separator | content
        let sidebar_width = 30u16.min(area.width / 3).max(15);
        let separator_width = 1u16;
        let sidebar_area = Rect::new(area.x, area.y, sidebar_width, area.height);
        let content_start = area.x + sidebar_width + separator_width;
        let content_width = area
            .width
            .saturating_sub(sidebar_width + separator_width);
        let content_area = Rect::new(content_start, area.y, content_width, area.height);

        // Render separator
        let sep_style = cx
            .editor
            .theme
            .try_get("ui.bufferline")
            .unwrap_or_else(|| cx.editor.theme.get("ui.statusline.inactive"));
        for y in area.y..area.y + area.height {
            surface.set_stringn(
                area.x + sidebar_width,
                y,
                "│",
                1,
                sep_style,
            );
        }

        self.sidebar_area = sidebar_area;
        self.render_sidebar(sidebar_area, surface, cx.editor);
        self.render_diff_content(content_area, surface, cx);
    }

    fn cursor(
        &self,
        _area: Rect,
        _editor: &Editor,
    ) -> (Option<helix_core::Position>, CursorKind) {
        (None, CursorKind::Hidden)
    }

    fn icon_path(&self, _editor: &Editor) -> Option<PathBuf> {
        self.files
            .get(self.selected_file)
            .map(|f| PathBuf::from(&f.rel_path))
    }
}

/// Convert a `FileDiff` into a `DiffFileEntry` with pre-computed display lines.
fn build_file_entry(fd: FileDiff) -> DiffFileEntry {
    let mut display_lines = Vec::new();

    for hunk in &fd.hunks {
        let mut old_line = hunk.old_start;
        let mut new_line = hunk.new_start;

        for (kind, text) in &hunk.lines {
            match kind {
                DiffLineKind::HunkHeader => {
                    display_lines.push(DisplayLine {
                        kind: DiffLineKind::HunkHeader,
                        text: text.clone(),
                        old_lineno: None,
                        new_lineno: None,
                    });
                }
                DiffLineKind::Context => {
                    display_lines.push(DisplayLine {
                        kind: DiffLineKind::Context,
                        text: text.clone(),
                        old_lineno: Some(old_line),
                        new_lineno: Some(new_line),
                    });
                    old_line += 1;
                    new_line += 1;
                }
                DiffLineKind::Deleted => {
                    display_lines.push(DisplayLine {
                        kind: DiffLineKind::Deleted,
                        text: text.clone(),
                        old_lineno: Some(old_line),
                        new_lineno: None,
                    });
                    old_line += 1;
                }
                DiffLineKind::Added => {
                    display_lines.push(DisplayLine {
                        kind: DiffLineKind::Added,
                        text: text.clone(),
                        old_lineno: None,
                        new_lineno: Some(new_line),
                    });
                    new_line += 1;
                }
            }
        }
    }

    DiffFileEntry {
        rel_path: fd.path,
        change_kind: fd.change_kind,
        display_lines,
    }
}
