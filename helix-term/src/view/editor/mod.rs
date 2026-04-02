mod config;

pub use config::*;

use crate::ui::app::Application;
use crate::view::{
    document::Mode,
    doc_view::DocView,
    events::DocumentFocusLost,
    graphics::{CursorKind, Rect},
    handlers::Handlers,
    info::Info,
    register::Registers,
    theme::{self, Theme},
    tree,
    Document, AppId, View, ViewId,
};
use helix_event::dispatch;
use helix_vcs::DiffProviderRegistry;

use futures_util::{future, StreamExt};
use helix_lsp::{Call, LanguageServerId};

use std::{
    borrow::Cow,
    cell::Cell,
    collections::{BTreeMap, HashMap, VecDeque},
    fs, io,
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
};

use tokio::{
    sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender},
    time::{sleep, Duration, Instant, Sleep},
};

pub use helix_core::diagnostic::Severity;
use helix_core::{
    diagnostic::DiagnosticProvider,
    syntax::{
        self,
        config::LanguageServerFeature,
    },
    Position, Range, Selection, Uri,
};
use helix_lsp::lsp;

use arc_swap::{
    access::{DynAccess, DynGuard},
    ArcSwap,
};

pub const DIR_STACK_CAP: usize = 10;

type Diagnostics = BTreeMap<Uri, Vec<(lsp::Diagnostic, DiagnosticProvider)>>;

pub struct EditorState {
    /// Application instances (the "tabs" in the UI).
    pub apps: Vec<Box<dyn Application>>,
    /// Index of the currently active application.
    pub active_app: usize,

    /// Document state indexed by AppId. Only EditorView apps have entries here.
    pub doc_views: HashMap<AppId, DocView>,

    pub registers: Registers,
    pub language_servers: helix_lsp::Registry,
    pub diagnostics: Diagnostics,
    pub diff_providers: DiffProviderRegistry,

    pub syn_loader: Arc<ArcSwap<syntax::Loader>>,
    pub theme_loader: Arc<theme::Loader>,
    /// last_theme is used for theme previews. We store the current theme here,
    /// and if previewing is cancelled, we can return to it.
    pub last_theme: Option<Theme>,
    /// The currently applied editor theme. While previewing a theme, the previewed theme
    /// is set here.
    pub theme: Theme,

    pub status_msg: Option<(Cow<'static, str>, Severity)>,
    pub autoinfo: Option<Info>,

    pub config: Arc<dyn DynAccess<Config> + Send + Sync>,

    pub idle_timer: Pin<Box<Sleep>>,
    pub(crate) redraw_timer: Pin<Box<Sleep>>,
    last_motion: Option<Motion>,
    pub last_cwd: Option<PathBuf>,
    pub dir_stack: VecDeque<PathBuf>,

    pub exit_code: i32,
    /// Set to true when the editor should exit (e.g. :quit).
    pub should_exit: bool,

    pub config_events: (UnboundedSender<ConfigEvent>, UnboundedReceiver<ConfigEvent>),
    pub needs_redraw: bool,
    pub handlers: Handlers,

    /// Typed UI layer state, managed by helix-term.
    /// Use `layer_state()` / `layer_state_mut()` for typed access.
    pub layer_state: Box<dyn std::any::Any>,

    /// Content area computed by the tab bar chrome layer.
    /// Excludes tab bar and commandline; applications render into this area.
    pub main_area: Rect,
}

impl EditorState {
    /// Typed accessor for the layer state. Panics if not initialized.
    pub fn layer_state<T: 'static>(&self) -> &T {
        self.layer_state
            .downcast_ref::<T>()
            .expect("Editor.layer_state type mismatch")
    }

    /// Typed mutable accessor for the layer state. Panics if not initialized.
    pub fn layer_state_mut<T: 'static>(&mut self) -> &mut T {
        self.layer_state
            .downcast_mut::<T>()
            .expect("Editor.layer_state type mismatch")
    }

    /// Get the active app's DocView (immutable). Returns None if active app has no document.
    pub fn active_doc_view(&self) -> Option<&DocView> {
        let id = self.apps.get(self.active_app)?.id();
        self.doc_views.get(&id)
    }

    /// Get the active app's DocView (mutable). Returns None if active app has no document.
    pub fn active_doc_view_mut(&mut self) -> Option<&mut DocView> {
        let id = self.apps.get(self.active_app)?.id();
        self.doc_views.get_mut(&id)
    }
}

pub type Editor = EditorState;

pub type Motion = Box<dyn Fn(&mut EditorState)>;

#[derive(Debug)]
pub enum EditorEvent {
    ConfigEvent(ConfigEvent),
    LanguageServerMessage((LanguageServerId, Call)),
    IdleTimer,
    Redraw,
}

#[derive(Debug, Clone)]
pub enum ConfigEvent {
    Refresh,
    Update(Box<Config>),
    ThemeChanged,
}

enum ThemeAction {
    Set,
    Preview,
}

#[derive(Debug, Copy, Clone)]
pub enum Action {
    Load,
    Replace,
    HorizontalSplit,
    VerticalSplit,
}

impl Action {
    /// Whether to align the view to the cursor after executing this action
    pub fn align_view(&self, view: &View, new_doc: AppId) -> bool {
        !matches!((self, view.doc == new_doc), (Action::Load, false))
    }
}

/// Error thrown on failed document closed
pub enum CloseError {
    /// Document doesn't exist
    DoesNotExist,
    /// Buffer is modified
    BufferModified(String),
    /// Document failed to save
    SaveError(anyhow::Error),
}

impl EditorState {
    pub fn new(
        mut area: Rect,
        theme_loader: Arc<theme::Loader>,
        syn_loader: Arc<ArcSwap<syntax::Loader>>,
        config: Arc<dyn DynAccess<Config> + Send + Sync>,
        handlers: Handlers,
    ) -> Self {
        let language_servers = helix_lsp::Registry::new(syn_loader.clone());
        let conf = config.load();
        // HAXX: offset the render area height by 1 to account for prompt/commandline
        area.height -= 1;

        Self {
            apps: Vec::new(),
            active_app: 0,
            doc_views: HashMap::new(),
            theme: theme_loader.default(),
            language_servers,
            diagnostics: Diagnostics::new(),
            diff_providers: DiffProviderRegistry::default(),
            syn_loader,
            theme_loader,
            last_theme: None,
            registers: Registers::new(Box::new(arc_swap::access::Map::new(
                Arc::clone(&config),
                |config: &Config| &config.clipboard_provider,
            ))),
            status_msg: None,
            autoinfo: None,
            idle_timer: Box::pin(sleep(conf.idle_timeout)),
            redraw_timer: Box::pin(sleep(Duration::MAX)),
            last_motion: None,
            last_cwd: None,
            config,
            exit_code: 0,
            should_exit: false,
            config_events: unbounded_channel(),
            needs_redraw: false,
            handlers,
            dir_stack: VecDeque::with_capacity(DIR_STACK_CAP),
            layer_state: Box::new(()),
            main_area: Rect::default(),
        }
    }

    pub fn popup_border(&self) -> bool {
        self.config().popup_border == PopupBorderConfig::All
            || self.config().popup_border == PopupBorderConfig::Popup
    }

    pub fn menu_border(&self) -> bool {
        self.config().popup_border == PopupBorderConfig::All
            || self.config().popup_border == PopupBorderConfig::Menu
    }

    pub fn apply_motion<F: Fn(&mut Self) + 'static>(&mut self, motion: F) {
        motion(self);
        self.last_motion = Some(Box::new(motion));
    }

    pub fn repeat_last_motion(&mut self, count: usize) {
        if let Some(motion) = self.last_motion.take() {
            for _ in 0..count {
                motion(self);
            }
            self.last_motion = Some(motion);
        }
    }

    /// Current editing mode for the [`EditorState`].
    pub fn mode(&self) -> Mode {
        self.active_doc_view().map_or(Mode::Normal, |dv| dv.mode)
    }

    /// Insert a DocView for the given AppId and launch language servers.
    pub fn add_doc_view(&mut self, app_id: AppId, dv: DocView) {
        let doc_id = dv.doc.id();
        self.doc_views.insert(app_id, dv);
        self.launch_language_servers(doc_id);
    }

    /// Remove the DocView for the given AppId.
    pub fn remove_doc_view(&mut self, app_id: &AppId) -> Option<DocView> {
        self.doc_views.remove(app_id)
    }

    pub fn config(&self) -> DynGuard<Config> {
        self.config.load()
    }

    /// Call if the config has changed to let the editor update all
    /// relevant members.
    pub fn refresh_config(&mut self, old_config: &Config) {
        let config = self.config();
        self.reset_idle_timer();
        self._refresh();
        helix_event::dispatch(crate::view::events::ConfigDidChange {
            editor: self,
            old: old_config,
            new: &config,
        })
    }

    pub fn clear_idle_timer(&mut self) {
        // equivalent to internal Instant::far_future() (30 years)
        self.idle_timer
            .as_mut()
            .reset(Instant::now() + Duration::from_secs(86400 * 365 * 30));
    }

    pub fn reset_idle_timer(&mut self) {
        let config = self.config();
        self.idle_timer
            .as_mut()
            .reset(Instant::now() + config.idle_timeout);
    }

    pub fn clear_status(&mut self) {
        self.status_msg = None;
    }

    #[inline]
    pub fn set_status<T: Into<Cow<'static, str>>>(&mut self, status: T) {
        let status = status.into();
        log::debug!("editor status: {}", status);
        self.status_msg = Some((status, Severity::Info));
    }

    #[inline]
    pub fn set_error<T: Into<Cow<'static, str>>>(&mut self, error: T) {
        let error = error.into();
        log::debug!("editor error: {}", error);
        self.status_msg = Some((error, Severity::Error));
    }

    #[inline]
    pub fn set_warning<T: Into<Cow<'static, str>>>(&mut self, warning: T) {
        let warning = warning.into();
        log::warn!("editor warning: {}", warning);
        self.status_msg = Some((warning, Severity::Warning));
    }

    #[inline]
    pub fn get_status(&self) -> Option<(&Cow<'static, str>, &Severity)> {
        self.status_msg.as_ref().map(|(status, sev)| (status, sev))
    }

    /// Returns true if the current status is an error
    #[inline]
    pub fn is_err(&self) -> bool {
        self.status_msg
            .as_ref()
            .map(|(_, sev)| *sev == Severity::Error)
            .unwrap_or(false)
    }

    pub fn unset_theme_preview(&mut self) {
        if let Some(last_theme) = self.last_theme.take() {
            self.set_theme(last_theme);
        }
        // None likely occurs when the user types ":theme" and then exits before previewing
    }

    pub fn set_theme_preview(&mut self, theme: Theme) {
        self.set_theme_impl(theme, ThemeAction::Preview);
    }

    pub fn set_theme(&mut self, theme: Theme) {
        self.set_theme_impl(theme, ThemeAction::Set);
    }

    fn set_theme_impl(&mut self, theme: Theme, preview: ThemeAction) {
        // `ui.selection` is the only scope required to be able to render a theme.
        if theme.find_highlight_exact("ui.selection").is_none() {
            self.set_error("Invalid theme: `ui.selection` required");
            return;
        }

        let scopes = theme.scopes();
        (*self.syn_loader).load().set_scopes(scopes.to_vec());

        match preview {
            ThemeAction::Preview => {
                let last_theme = std::mem::replace(&mut self.theme, theme);
                // only insert on first preview: this will be the last theme the user has saved
                self.last_theme.get_or_insert(last_theme);
            }
            ThemeAction::Set => {
                self.last_theme = None;
                self.theme = theme;
            }
        }

        self._refresh();
    }

    #[inline]
    pub fn language_server_by_id(
        &self,
        language_server_id: LanguageServerId,
    ) -> Option<&helix_lsp::Client> {
        self.language_servers
            .get_by_id(language_server_id)
            .map(|client| &**client)
    }

    /// Refreshes the language server for a given document
    pub fn refresh_language_servers(&mut self, doc_id: AppId) {
        self.launch_language_servers(doc_id)
    }

    /// moves/renames a path, invoking any event handlers (currently only lsp)
    /// and calling `set_doc_path` if the file is open in the editor
    pub fn move_path(&mut self, old_path: &Path, new_path: &Path) -> io::Result<()> {
        let new_path = helix_stdx::path::canonicalize(new_path);
        // sanity check
        if old_path == new_path {
            return Ok(());
        }
        let is_dir = old_path.is_dir();
        let language_servers: Vec<_> = self
            .language_servers
            .iter_clients()
            .filter(|client| client.is_initialized())
            .cloned()
            .collect();
        for language_server in language_servers {
            let Some(request) = language_server.will_rename(old_path, &new_path, is_dir) else {
                continue;
            };
            let edit = match helix_lsp::block_on(request) {
                Ok(edit) => edit.unwrap_or_default(),
                Err(err) => {
                    log::error!("invalid willRename response: {err:?}");
                    continue;
                }
            };
            if let Err(err) = self.apply_workspace_edit(language_server.offset_encoding(), &edit) {
                log::error!("failed to apply workspace edit: {err:?}")
            }
        }

        if old_path.exists() {
            fs::rename(old_path, &new_path)?;
        }

        // Check if the current doc matches the old path
        if let Some(dv) = self.active_doc_view() {
            let matches = dv.doc.path().map(|p| p == old_path).unwrap_or(false);
            if matches {
                let doc_id = dv.doc.id();
                self.set_doc_path(doc_id, &new_path);
            }
        }
        let is_dir = new_path.is_dir();
        for ls in self.language_servers.iter_clients() {
            if !ls.is_initialized() {
                continue;
            }
            ls.did_rename(old_path, &new_path, is_dir);
        }
        self.language_servers
            .file_event_handler
            .file_changed(old_path.to_owned());
        self.language_servers
            .file_event_handler
            .file_changed(new_path);
        Ok(())
    }

    pub fn set_doc_path(&mut self, doc_id: AppId, path: &Path) {
        let dv = self.active_doc_view_mut().expect("no active doc view");
        let doc = &mut dv.doc;
        let old_path = doc.path();

        if let Some(old_path) = old_path {
            if old_path == path {
                return;
            }
            for language_server in doc.language_servers() {
                language_server.text_document_did_close(doc.identifier());
            }
        }
        doc.language_servers.clear();
        doc.set_path(Some(path));
        doc.detect_editor_config();
        self.refresh_doc_language(doc_id)
    }

    pub fn refresh_doc_language(&mut self, doc_id: AppId) {
        let loader = self.syn_loader.load();
        let app_id = self.apps.get(self.active_app).map(|a| a.id());
        if let Some(dv) = app_id.and_then(|id| self.doc_views.get_mut(&id)) {
            dv.doc.detect_language(&loader);
            dv.doc.detect_editor_config();
            dv.doc.detect_indent_and_line_ending();
        }
        self.refresh_language_servers(doc_id);
        if let Some(dv) = app_id.and_then(|id| self.doc_views.get_mut(&id)) {
            let diagnostics = EditorState::doc_diagnostics(&self.language_servers, &self.diagnostics, &dv.doc);
            dv.doc.replace_diagnostics(diagnostics, &[], None);
            dv.doc.reset_all_inlay_hints();
        }
    }

    /// Launch a language server for a given document
    pub fn launch_language_servers(&mut self, _doc_id: AppId) {
        if !self.config().lsp.enable {
            return;
        }
        let dv = match self.active_doc_view_mut() {
            Some(dv) => dv,
            None => return,
        };
        let doc = &mut dv.doc;
        let Some(doc_url) = doc.url() else {
            return;
        };
        let (lang, path) = (doc.language.clone(), doc.path().cloned());
        let config = doc.config.load();
        let root_dirs = &config.workspace_lsp_roots;

        let language_servers = lang.as_ref().map_or_else(HashMap::default, |language| {
            self.language_servers
                .get(language, path.as_ref(), root_dirs, false)
                .filter_map(|(lang, client)| match client {
                    Ok(client) => Some((lang, client)),
                    Err(err) => {
                        if let helix_lsp::Error::ExecutableNotFound(err) = err {
                            log::debug!(
                                "Language server not found for `{}` {} {}", language.scope, lang, err,
                            );
                        } else {
                            log::error!(
                                "Failed to initialize the language servers for `{}` - `{}` {{ {} }}",
                                language.scope,
                                lang,
                                err
                            );
                        }
                        None
                    }
                })
                .collect::<HashMap<_, _>>()
        });

        // Re-borrow after language_servers access
        let dv = self.active_doc_view_mut().expect("no active doc view");
        let doc = &mut dv.doc;

        if language_servers.is_empty() && doc.language_servers.is_empty() {
            return;
        }

        let language_id = doc.language_id().map(ToOwned::to_owned).unwrap_or_default();

        let doc_language_servers_not_in_registry =
            doc.language_servers.iter().filter(|(name, doc_ls)| {
                language_servers
                    .get(*name)
                    .is_none_or(|ls| ls.id() != doc_ls.id())
            });

        for (_, language_server) in doc_language_servers_not_in_registry {
            language_server.text_document_did_close(doc.identifier());
        }

        let language_servers_not_in_doc = language_servers.iter().filter(|(name, ls)| {
            doc.language_servers
                .get(*name)
                .is_none_or(|doc_ls| ls.id() != doc_ls.id())
        });

        for (_, language_server) in language_servers_not_in_doc {
            language_server.text_document_did_open(
                doc_url.clone(),
                doc.version(),
                doc.text(),
                language_id.clone(),
            );
        }

        doc.language_servers = language_servers;
    }

    /// Close a view by removing it from the tree.
    /// Returns `true` if the tree became empty (caller should close the tab via TabManager).
    pub fn close(&mut self, view_id: ViewId) -> bool {
        let dv = match self.active_doc_view_mut() {
            Some(dv) => dv,
            None => return false,
        };
        dv.tree.remove(view_id);
        if dv.tree.views().count() == 0 {
            true
        } else {
            self._refresh();
            false
        }
    }

    fn _refresh(&mut self) {
        let config = self.config();
        let dv = match self.active_doc_view_mut() {
            Some(dv) => dv,
            None => return,
        };

        if !config.lsp.display_inlay_hints {
            dv.doc.reset_all_inlay_hints();
        }

        let (doc, tree) = dv.doc_and_tree_mut();
        for (view, _) in tree.views_mut() {
            view.sync_changes(doc);
            view.gutters = config.gutters.clone();
            view.ensure_cursor_in_view(doc, config.scrolloff)
        }
    }

    pub fn resize(&mut self, area: Rect) {
        let dv = match self.active_doc_view_mut() {
            Some(dv) => dv,
            None => return,
        };
        if dv.tree.resize(area) {
            self._refresh();
        };
    }

    pub fn focus(&mut self, view_id: ViewId) {
        let dv = match self.active_doc_view() {
            Some(dv) => dv,
            None => return,
        };
        if dv.tree.focus == view_id {
            return;
        }

        // Reset mode to normal.
        self.enter_normal_mode();
        self.ensure_cursor_in_view(view_id);
        // Update jumplist selections with new document changes.
        {
            let dv = self.active_doc_view_mut().unwrap();
            let (doc, tree) = dv.doc_and_tree_mut();
            for (view, _focused) in tree.views_mut() {
                view.sync_changes(doc);
            }
        }

        let dv = self.active_doc_view_mut().unwrap();
        let prev_id = std::mem::replace(&mut dv.tree.focus, view_id);
        dv.doc.mark_as_focused();

        let focus_lost = self.active_doc_view().unwrap().tree.get(prev_id).doc;
        dispatch(DocumentFocusLost {
            editor: self,
            doc: focus_lost,
        });
    }

    pub fn focus_next(&mut self) {
        let next = self.active_doc_view().unwrap().tree.next();
        self.focus(next);
    }

    pub fn focus_prev(&mut self) {
        let prev = self.active_doc_view().unwrap().tree.prev();
        self.focus(prev);
    }

    pub fn focus_direction(&mut self, direction: tree::Direction) {
        let dv = self.active_doc_view().unwrap();
        let current_view = dv.tree.focus;
        if let Some(id) = dv.tree.find_split_in_direction(current_view, direction) {
            self.focus(id)
        }
    }

    pub fn swap_split_in_direction(&mut self, direction: tree::Direction) {
        self.active_doc_view_mut().unwrap().tree.swap_split_in_direction(direction);
    }

    pub fn transpose_view(&mut self) {
        self.active_doc_view_mut().unwrap().tree.transpose();
    }

    pub fn should_close(&self) -> bool {
        self.should_exit
    }

    pub fn ensure_cursor_in_view(&mut self, id: ViewId) {
        let config = self.config();
        let dv = match self.active_doc_view_mut() {
            Some(dv) => dv,
            None => return,
        };
        let (doc, tree) = dv.doc_and_tree_mut();
        let view = tree.get(id);
        view.ensure_cursor_in_view(doc, config.scrolloff)
    }

    /// Returns all supported diagnostics for the document
    pub fn doc_diagnostics<'a>(
        language_servers: &'a helix_lsp::Registry,
        diagnostics: &'a Diagnostics,
        document: &Document,
    ) -> impl Iterator<Item = helix_core::Diagnostic> + 'a {
        EditorState::doc_diagnostics_with_filter(language_servers, diagnostics, document, |_, _| true)
    }

    /// Returns all supported diagnostics for the document
    /// filtered by `filter` which is invocated with the raw `lsp::Diagnostic` and the language server id it came from
    pub fn doc_diagnostics_with_filter<'a>(
        language_servers: &'a helix_lsp::Registry,
        diagnostics: &'a Diagnostics,
        document: &Document,
        filter: impl Fn(&lsp::Diagnostic, &DiagnosticProvider) -> bool + 'a,
    ) -> impl Iterator<Item = helix_core::Diagnostic> + 'a {
        let text = document.text().clone();
        let language_config = document.language.clone();
        document
            .uri()
            .and_then(|uri| diagnostics.get(&uri))
            .map(|diags| {
                diags.iter().filter_map(move |(diagnostic, provider)| {
                    let server_id = provider.language_server_id()?;
                    let ls = language_servers.get_by_id(server_id)?;
                    language_config
                        .as_ref()
                        .and_then(|c| {
                            c.language_servers.iter().find(|features| {
                                features.name == ls.name()
                                    && features.has_feature(LanguageServerFeature::Diagnostics)
                            })
                        })
                        .and_then(|_| {
                            if filter(diagnostic, provider) {
                                Document::lsp_diagnostic_to_diagnostic(
                                    &text,
                                    language_config.as_deref(),
                                    diagnostic,
                                    provider.clone(),
                                    ls.offset_encoding(),
                                )
                            } else {
                                None
                            }
                        })
                })
            })
            .into_iter()
            .flatten()
    }

    /// Gets the primary cursor position in screen coordinates,
    /// or `None` if the primary cursor is not visible on screen.
    pub fn cursor(&self) -> (Option<Position>, CursorKind) {
        let dv = match self.active_doc_view() {
            Some(dv) => dv,
            None => return (None, CursorKind::default()),
        };
        let config = self.config();
        let (view, doc) = current_ref!(self);
        if let Some(mut pos) = dv.cursor_cache.get(view, doc) {
            let inner = view.inner_area(doc);
            pos.col += inner.x as usize;
            pos.row += inner.y as usize;
            let cursorkind = config.cursor_shape.from_mode(dv.mode);
            (Some(pos), cursorkind)
        } else {
            (None, CursorKind::default())
        }
    }

    /// Closes language servers with timeout. The default timeout is 10000 ms, use
    /// `timeout` parameter to override this.
    pub async fn close_language_servers(
        &self,
        timeout: Option<u64>,
    ) -> Result<(), tokio::time::error::Elapsed> {
        for client in self.language_servers.iter_clients() {
            self.language_servers
                .file_event_handler
                .remove_client(client.id());
        }

        tokio::time::timeout(
            Duration::from_millis(timeout.unwrap_or(3000)),
            future::join_all(
                self.language_servers
                    .iter_clients()
                    .map(|client| client.force_shutdown()),
            ),
        )
        .await
        .map(|_| ())
    }

    pub async fn wait_event(&mut self) -> EditorEvent {
        loop {
            tokio::select! {
                biased;

                Some(config_event) = self.config_events.1.recv() => {
                    return EditorEvent::ConfigEvent(config_event)
                }
                Some(message) = self.language_servers.incoming.next() => {
                    return EditorEvent::LanguageServerMessage(message)
                }
                _ = helix_event::redraw_requested() => {
                    if  !self.needs_redraw{
                        self.needs_redraw = true;
                        let timeout = Instant::now() + Duration::from_millis(33);
                        if timeout < self.idle_timer.deadline() && timeout < self.redraw_timer.deadline(){
                            self.redraw_timer.as_mut().reset(timeout)
                        }
                    }
                }

                _ = &mut self.redraw_timer  => {
                    self.redraw_timer.as_mut().reset(Instant::now() + Duration::from_secs(86400 * 365 * 30));
                    return EditorEvent::Redraw
                }
                _ = &mut self.idle_timer  => {
                    return EditorEvent::IdleTimer
                }
            }
        }
    }

    /// Switches the editor into normal mode.
    pub fn enter_normal_mode(&mut self) {
        use helix_core::graphemes;

        let dv = match self.active_doc_view_mut() {
            Some(dv) => dv,
            None => return,
        };

        if dv.mode == Mode::Normal {
            return;
        }

        dv.mode = Mode::Normal;
        let (doc, tree) = dv.doc_and_tree_mut();
        let view = tree.get_mut(tree.focus);

        // if leaving append mode, move cursor back by 1
        if doc.restore_cursor {
            let text = doc.text().slice(..);
            let selection = doc.selection(view.id).clone().transform(|range| {
                let mut head = range.to();
                if range.head > range.anchor {
                    head = graphemes::prev_grapheme_boundary(text, head);
                }

                Range::new(range.from(), head)
            });

            doc.set_selection(view.id, selection);
            doc.restore_cursor = false;
        }
    }

    /// Returns the id of a view that this doc contains a selection for,
    /// making sure it is synced with the current changes.
    pub fn get_synced_view_id(&mut self, _id: AppId) -> ViewId {
        let dv = self.active_doc_view_mut().expect("no active doc view");
        let focus = dv.tree.focus;
        let (doc, tree) = dv.doc_and_tree_mut();
        let current_view = tree.get_mut(focus);
        if doc.selections().contains_key(&current_view.id) {
            if current_view.doc != _id {
                current_view.sync_changes(doc);
            }
            current_view.id
        } else if let Some(view_id) = doc.selections().keys().next() {
            let view_id = *view_id;
            let view = tree.get_mut(view_id);
            view.sync_changes(doc);
            view_id
        } else {
            doc.ensure_view_init(current_view.id);
            current_view.id
        }
    }

    pub fn set_cwd(&mut self, path: &Path) -> std::io::Result<()> {
        self.last_cwd = helix_stdx::env::set_current_working_dir(path)?;
        self.clear_doc_relative_paths();
        Ok(())
    }

    pub fn get_last_cwd(&mut self) -> Option<&Path> {
        self.last_cwd.as_deref()
    }

    pub fn jump_forward(&mut self, view_id: ViewId, count: usize) {
        let dv = self.active_doc_view_mut().expect("no active doc view");
        if let Some((doc_id, selection)) = dv.tree.get_mut(view_id).jumps.forward(count).cloned() {
            self.jump_to(view_id, doc_id, selection);
        }
    }

    pub fn jump_backward(&mut self, view_id: ViewId, count: usize) {
        let dv = self.active_doc_view_mut().expect("no active doc view");
        let (doc, tree) = dv.doc_and_tree_mut();
        let view = tree.get_mut(view_id);
        if let Some((doc_id, selection)) = view
            .jumps
            .backward(view_id, doc, count)
            .cloned()
        {
            self.jump_to(view_id, doc_id, selection);
        }
    }

    fn jump_to(&mut self, view_id: ViewId, _dest_doc_id: AppId, mut selection: Selection) {
        {
            let dv = self.active_doc_view_mut().unwrap();
            let (doc, tree) = dv.doc_and_tree_mut();
            let view = tree.get_mut(view_id);
            if let Some(transaction) = view.changes_to_sync(doc) {
                let text = doc.text().slice(..);
                selection = selection.map(transaction.changes()).ensure_invariants(text);
            }
        }
        let scrolloff = self.config.load().scrolloff;
        let dv = self.active_doc_view_mut().unwrap();
        let focus = dv.tree.focus;
        let (doc, tree) = dv.doc_and_tree_mut();
        let view = tree.get_mut(focus);
        doc.set_selection(view_id, selection);
        view.ensure_cursor_in_view_center(doc, scrolloff);
    }
}

#[derive(Default)]
pub struct CursorCache(Cell<Option<Option<Position>>>);

impl CursorCache {
    pub fn get(&self, view: &View, doc: &Document) -> Option<Position> {
        if let Some(pos) = self.0.get() {
            return pos;
        }

        let text = doc.text().slice(..);
        let cursor = doc.selection(view.id).primary().cursor(text);
        let res = view.screen_coords_at_pos(doc, text, cursor);
        self.set(res);
        res
    }

    pub fn set(&self, cursor_pos: Option<Position>) {
        self.0.set(Some(cursor_pos))
    }

    pub fn reset(&self) {
        self.0.set(None)
    }
}
