use std::any::Any;
use std::path::PathBuf;
use std::sync::Arc;

use arc_swap::ArcSwap;
use helix_core::Position;
use helix_view::graphics::{CursorKind, Rect};
use helix_view::Editor;
use tui::buffer::Buffer as Surface;

use crate::compositor::{Context, EventResult};
use crate::config::Config;
use crate::keymap::Keymaps;

pub use helix_view::input::Event;
pub use helix_view::AppId;

/// Trait for tab applications. Each tab in the tab bar is an Application.
///
/// Applications own their rendering area and keybindings. The TabManager
/// handles the shared chrome (tab bar, commandline) and delegates
/// the main area to the active application via ActiveAppProxy.
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

// ---------------------------------------------------------------------------
// AppState — stored as Editor.app_state (opaque Box<dyn Any>)
// ---------------------------------------------------------------------------

/// Concrete application state stored in `Editor.app_state`.
pub struct AppState {
    pub apps: Vec<Box<dyn Application>>,
    pub active: usize,
    config: Arc<ArcSwap<Config>>,
}

impl AppState {
    pub fn new(config: Arc<ArcSwap<Config>>) -> Self {
        Self {
            apps: Vec::new(),
            active: 0,
            config,
        }
    }

    pub fn make_keymaps(&self) -> Keymaps {
        let keys = Box::new(arc_swap::access::Map::new(
            Arc::clone(&self.config),
            |config: &Config| &config.keys,
        ));
        Keymaps::new(keys)
    }
}

/// Take app_state out of Editor for mutable access during event handling.
/// Must be restored with `restore_app_state` before returning.
pub fn take_app_state(editor: &mut Editor) -> Box<dyn Any> {
    std::mem::replace(&mut editor.app_state, Box::new(()))
}

/// Restore app_state into Editor after event handling.
pub fn restore_app_state(editor: &mut Editor, state: Box<dyn Any>) {
    editor.app_state = state;
}

fn app_state(editor: &Editor) -> &AppState {
    editor
        .app_state
        .downcast_ref::<AppState>()
        .expect("Editor.app_state must be AppState")
}

fn app_state_mut(editor: &mut Editor) -> &mut AppState {
    editor
        .app_state
        .downcast_mut::<AppState>()
        .expect("Editor.app_state must be AppState")
}

// ---------------------------------------------------------------------------
// EditorApps — extension trait on Editor for application management
// ---------------------------------------------------------------------------

/// Extension trait that adds application management methods to Editor.
pub trait EditorApps {
    fn init_apps(&mut self, config: Arc<ArcSwap<Config>>);
    fn app_count(&self) -> usize;
    fn active_app_index(&self) -> usize;
    fn app_names(&self) -> Vec<String>;

    fn add_app(&mut self, app: Box<dyn Application>);
    fn close_active_app(&mut self);
    fn close_app_at(&mut self, index: usize);
    fn close_other_apps(&mut self);
    fn close_all_apps(&mut self);
    fn next_app(&mut self);
    fn prev_app(&mut self);
    fn switch_app(&mut self, index: usize);

    fn add_editor_app(&mut self, doc: helix_view::Document);
    fn add_diff_app(&mut self, diff_view: super::diff_view::DiffView);
    fn make_keymaps(&self) -> Keymaps;
}

impl EditorApps for Editor {
    fn init_apps(&mut self, config: Arc<ArcSwap<Config>>) {
        self.app_state = Box::new(AppState::new(config));
    }

    fn app_count(&self) -> usize {
        app_state(self).apps.len()
    }

    fn active_app_index(&self) -> usize {
        app_state(self).active
    }

    fn app_names(&self) -> Vec<String> {
        app_state(self)
            .apps
            .iter()
            .map(|a| a.name(self))
            .collect()
    }

    fn add_app(&mut self, app: Box<dyn Application>) {
        let state = app_state_mut(self);
        // Remove welcome tab before adding a real tab
        if state.apps.len() == 1 {
            if state.apps[0]
                .as_any()
                .downcast_ref::<super::welcome::WelcomePage>()
                .is_some()
            {
                state.apps.remove(0);
            }
        }
        state.apps.push(app);
        state.active = state.apps.len() - 1;
    }

    fn close_app_at(&mut self, index: usize) {
        use super::diff_view::{DiffKey, DiffView};
        use helix_view::handlers::FileWatcherCommand;

        // Extract info we need before mutating, to avoid borrow conflicts
        let (worktree_to_unwatch, editor_tab_index) = {
            let state = app_state(self);
            if index >= state.apps.len() {
                return;
            }
            let worktree = state.apps[index]
                .as_any()
                .downcast_ref::<DiffView>()
                .and_then(|dv| match dv.diff_key() {
                    DiffKey::LocalChanges => Some(dv.cwd().to_path_buf()),
                    _ => None,
                });
            let tab_idx = state.apps[index]
                .as_any()
                .downcast_ref::<super::EditorView>()
                .map(|ev| ev.tab_index);
            (worktree, tab_idx)
        };

        // Unwatch worktree for LocalChanges DiffView tabs
        if let Some(worktree) = worktree_to_unwatch {
            helix_event::send_blocking(
                &self.handlers.file_watcher,
                FileWatcherCommand::UnwatchWorktree { worktree },
            );
        }

        // If the tab being closed is an EditorView, remove its backing DocView
        if let Some(tab_index) = editor_tab_index {
            if tab_index < self.tabs.len() {
                self.tabs.remove(tab_index);
                // Fix up tab_index on remaining EditorViews
                let state = app_state_mut(self);
                for app in &mut state.apps {
                    if let Some(ev) = app.as_any_mut().downcast_mut::<super::EditorView>() {
                        if ev.tab_index > tab_index {
                            ev.tab_index -= 1;
                        }
                    }
                }
                if self.active_tab >= self.tabs.len() && !self.tabs.is_empty() {
                    self.active_tab = self.tabs.len() - 1;
                }
            }
        }

        let state = app_state_mut(self);
        state.apps.remove(index);

        if state.apps.is_empty() {
            state.apps.push(Box::new(super::welcome::WelcomePage::new()));
            state.active = 0;
            return;
        }
        if state.active >= state.apps.len() {
            state.active = state.apps.len() - 1;
        } else if state.active > index {
            state.active -= 1;
        }

        // Sync editor.active_tab to the current active EditorView
        if let Some(ev) = state.apps[state.active]
            .as_any()
            .downcast_ref::<super::EditorView>()
        {
            self.active_tab = ev.tab_index;
        }
    }

    fn close_active_app(&mut self) {
        let index = app_state(self).active;
        self.close_app_at(index);
    }

    fn close_other_apps(&mut self) {
        let active = app_state(self).active;
        let count = app_state(self).apps.len();
        for i in (0..count).rev() {
            if i != active {
                self.close_app_at(i);
            }
        }
    }

    fn close_all_apps(&mut self) {
        let count = app_state(self).apps.len();
        for _ in 0..count {
            self.close_app_at(0);
        }
    }

    fn next_app(&mut self) {
        let state = app_state_mut(self);
        if !state.apps.is_empty() {
            state.active = (state.active + 1) % state.apps.len();
            // Sync editor.active_tab
            if let Some(ev) = state.apps[state.active]
                .as_any()
                .downcast_ref::<super::EditorView>()
            {
                self.active_tab = ev.tab_index;
            }
        }
    }

    fn prev_app(&mut self) {
        let state = app_state_mut(self);
        if !state.apps.is_empty() {
            state.active = if state.active == 0 {
                state.apps.len() - 1
            } else {
                state.active - 1
            };
            if let Some(ev) = state.apps[state.active]
                .as_any()
                .downcast_ref::<super::EditorView>()
            {
                self.active_tab = ev.tab_index;
            }
        }
    }

    fn switch_app(&mut self, index: usize) {
        let state = app_state_mut(self);
        if index < state.apps.len() {
            state.active = index;
            if let Some(ev) = state.apps[state.active]
                .as_any()
                .downcast_ref::<super::EditorView>()
            {
                self.active_tab = ev.tab_index;
            }
        }
    }

    fn add_editor_app(&mut self, doc: helix_view::Document) {
        // If the file is already open in an existing tab, activate it instead.
        if let Some(new_path) = doc.path().and_then(|p| std::fs::canonicalize(p).ok()) {
            for (i, tab) in self.tabs.iter().enumerate() {
                if let Some(existing) =
                    tab.doc().path().and_then(|p| std::fs::canonicalize(p).ok())
                {
                    if existing == new_path {
                        self.activate_tab(i);
                        // Find the corresponding app and switch to it
                        let state = app_state_mut(self);
                        for (j, app) in state.apps.iter().enumerate() {
                            if let Some(ev) =
                                app.as_any().downcast_ref::<super::EditorView>()
                            {
                                if ev.tab_index == i {
                                    state.active = j;
                                    break;
                                }
                            }
                        }
                        return;
                    }
                }
            }
        }

        let keymaps = app_state(self).make_keymaps();
        let dv = helix_view::DocView::new(doc);
        let tab_index = self.add_tab(Box::new(dv));
        let editor_view = Box::new(super::EditorView::new(keymaps, tab_index));
        self.add_app(editor_view);
        self.active_tab = tab_index;
    }

    fn add_diff_app(&mut self, diff_view: super::diff_view::DiffView) {
        use super::diff_view::DiffKey;
        use helix_view::handlers::FileWatcherCommand;

        // Register worktree watch for LocalChanges diffs
        if matches!(diff_view.diff_key(), DiffKey::LocalChanges) {
            let worktree = diff_view.cwd().to_path_buf();
            helix_event::send_blocking(
                &self.handlers.file_watcher,
                FileWatcherCommand::WatchWorktree { worktree },
            );
        }

        // Check for existing DiffView with same key
        let state = app_state(self);
        for (i, app) in state.apps.iter().enumerate() {
            if let Some(existing) = app.as_any().downcast_ref::<super::diff_view::DiffView>() {
                if existing.diff_key() == diff_view.diff_key() {
                    app_state_mut(self).active = i;
                    return;
                }
            }
        }

        self.add_app(Box::new(diff_view));
    }

    fn make_keymaps(&self) -> Keymaps {
        app_state(self).make_keymaps()
    }
}
