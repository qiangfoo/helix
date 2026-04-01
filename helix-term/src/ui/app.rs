use std::any::Any;
use std::path::PathBuf;
use std::sync::Arc;

use arc_swap::ArcSwap;
use helix_core::Position;
use helix_view::graphics::{CursorKind, Rect};
use helix_view::Editor;
use ratatui::buffer::Buffer as Surface;

use crate::compositor::{Context, EventResult};
use crate::config::Config;
use crate::keymap::Keymaps;
use crate::layers::LayerState;

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
// Typed helpers for accessing editor.apps (Vec<Box<dyn Any>>)
// Each element is a Box<Box<dyn Application>> erased to Box<dyn Any>.
// ---------------------------------------------------------------------------

type AppBox = Box<dyn Application>;

pub fn get_app(editor: &Editor, idx: usize) -> Option<&dyn Application> {
    editor.apps.get(idx)?.downcast_ref::<AppBox>().map(|b| &**b)
}

pub fn get_app_mut(editor: &mut Editor, idx: usize) -> Option<&mut dyn Application> {
    editor
        .apps
        .get_mut(idx)?
        .downcast_mut::<AppBox>()
        .map(|b| &mut **b)
}

fn push_app(editor: &mut Editor, app: Box<dyn Application>) {
    editor.apps.push(Box::new(app));
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
        self.layer_state_mut::<LayerState>().term_config = Some(config);
    }

    fn app_count(&self) -> usize {
        self.apps.len()
    }

    fn active_app_index(&self) -> usize {
        self.active_app
    }

    fn app_names(&self) -> Vec<String> {
        (0..self.apps.len())
            .filter_map(|i| get_app(self, i).map(|a| a.name(self)))
            .collect()
    }

    fn add_app(&mut self, app: Box<dyn Application>) {
        // Remove welcome tab before adding a real tab
        if self.apps.len() == 1 {
            if get_app(self, 0)
                .and_then(|a| a.as_any().downcast_ref::<super::welcome::WelcomePage>())
                .is_some()
            {
                self.apps.remove(0);
            }
        }
        push_app(self, app);
        self.active_app = self.apps.len() - 1;
    }

    fn close_app_at(&mut self, index: usize) {
        use super::diff_view::{DiffKey, DiffView};
        use helix_view::handlers::FileWatcherCommand;

        // Extract info we need before mutating, to avoid borrow conflicts
        let (worktree_to_unwatch, editor_tab_index) = {
            if index >= self.apps.len() {
                return;
            }
            let app = match get_app(self, index) {
                Some(a) => a,
                None => return,
            };
            let worktree = app
                .as_any()
                .downcast_ref::<DiffView>()
                .and_then(|dv| match dv.diff_key() {
                    DiffKey::LocalChanges => Some(dv.cwd().to_path_buf()),
                    _ => None,
                });
            let tab_idx = app
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
                for app_any in &mut self.apps {
                    if let Some(app) = app_any.downcast_mut::<AppBox>() {
                        if let Some(ev) = app.as_any_mut().downcast_mut::<super::EditorView>() {
                            if ev.tab_index > tab_index {
                                ev.tab_index -= 1;
                            }
                        }
                    }
                }
                if self.active_tab >= self.tabs.len() && !self.tabs.is_empty() {
                    self.active_tab = self.tabs.len() - 1;
                }
            }
        }

        self.apps.remove(index);

        if self.apps.is_empty() {
            push_app(self, Box::new(super::welcome::WelcomePage::new()));
            self.active_app = 0;
            return;
        }
        if self.active_app >= self.apps.len() {
            self.active_app = self.apps.len() - 1;
        } else if self.active_app > index {
            self.active_app -= 1;
        }

        // Sync editor.active_tab to the current active EditorView
        if let Some(ev) = get_app(self, self.active_app)
            .and_then(|a| a.as_any().downcast_ref::<super::EditorView>())
        {
            self.active_tab = ev.tab_index;
        }
    }

    fn close_active_app(&mut self) {
        let index = self.active_app;
        self.close_app_at(index);
    }

    fn close_other_apps(&mut self) {
        let active = self.active_app;
        let count = self.apps.len();
        for i in (0..count).rev() {
            if i != active {
                self.close_app_at(i);
            }
        }
    }

    fn close_all_apps(&mut self) {
        let count = self.apps.len();
        for _ in 0..count {
            self.close_app_at(0);
        }
    }

    fn next_app(&mut self) {
        if !self.apps.is_empty() {
            self.active_app = (self.active_app + 1) % self.apps.len();
            // Sync editor.active_tab
            if let Some(ev) = get_app(self, self.active_app)
                .and_then(|a| a.as_any().downcast_ref::<super::EditorView>())
            {
                self.active_tab = ev.tab_index;
            }
        }
    }

    fn prev_app(&mut self) {
        if !self.apps.is_empty() {
            self.active_app = if self.active_app == 0 {
                self.apps.len() - 1
            } else {
                self.active_app - 1
            };
            if let Some(ev) = get_app(self, self.active_app)
                .and_then(|a| a.as_any().downcast_ref::<super::EditorView>())
            {
                self.active_tab = ev.tab_index;
            }
        }
    }

    fn switch_app(&mut self, index: usize) {
        if index < self.apps.len() {
            self.active_app = index;
            if let Some(ev) = get_app(self, self.active_app)
                .and_then(|a| a.as_any().downcast_ref::<super::EditorView>())
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
                        for j in 0..self.apps.len() {
                            if let Some(ev) = get_app(self, j)
                                .and_then(|a| a.as_any().downcast_ref::<super::EditorView>())
                            {
                                if ev.tab_index == i {
                                    self.active_app = j;
                                    break;
                                }
                            }
                        }
                        return;
                    }
                }
            }
        }

        let keymaps = self.make_keymaps();
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
        for i in 0..self.apps.len() {
            if let Some(existing) = get_app(self, i)
                .and_then(|a| a.as_any().downcast_ref::<super::diff_view::DiffView>())
            {
                if existing.diff_key() == diff_view.diff_key() {
                    self.active_app = i;
                    return;
                }
            }
        }

        self.add_app(Box::new(diff_view));
    }

    fn make_keymaps(&self) -> Keymaps {
        let config = self
            .layer_state::<LayerState>()
            .term_config
            .as_ref()
            .expect("term_config not initialized");
        let keys = Box::new(arc_swap::access::Map::new(
            Arc::clone(config),
            |config: &Config| &config.keys,
        ));
        Keymaps::new(keys)
    }
}
