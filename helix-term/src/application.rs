use arc_swap::{access::Map, ArcSwap};
use futures_util::Stream;
use helix_core::{diagnostic::Severity, syntax, Selection};
use helix_lsp::{
    lsp::{self, notification::Notification},
    util::lsp_range_to_range,
    LanguageServerId, LspProgressMap,
};
// get_relative_path is no longer used after removing document write handling
use crate::view::{
    align_view,
    document::DocumentOpenError,
    editor::{ConfigEvent, EditorEvent},
    graphics::Rect,
    theme,
    Align, Document, Editor,
};
use serde_json::json;

use crate::{
    args::Args,
    compositor::Event,
    config::Config,
    handlers,
    job::Jobs,
    layers::EditorLayers,
    terminal as helix_terminal,
    ui::{self, overlay::overlaid, EditorApps},
};

use log::{error, info, warn};
use std::{
    io::{stdin, IsTerminal},
    path::Path,
    sync::Arc,
};

#[cfg_attr(windows, allow(unused_imports))]
use anyhow::{Context, Error};

#[cfg(not(windows))]
use {signal_hook::consts::signal, signal_hook_tokio::Signals};

#[cfg(windows)]
type Signals = futures_util::stream::Empty<()>;

type TerminalEvent = crossterm::event::Event;

#[cfg(not(feature = "integration"))]
type Terminal = helix_terminal::HelixTerminal;
#[cfg(feature = "integration")]
type Terminal = helix_terminal::TestTerminal;

pub struct Application {
    terminal: Terminal,
    pub editor: Editor,

    config: Arc<ArcSwap<Config>>,

    signals: Signals,
    jobs: Jobs,
    lsp_progress: LspProgressMap,

    theme_mode: Option<theme::Mode>,
}

#[cfg(feature = "integration")]
fn setup_integration_logging() {
    let level = std::env::var("HELIX_LOG_LEVEL")
        .map(|lvl| lvl.parse().unwrap())
        .unwrap_or(log::LevelFilter::Info);

    // Separate file config so we can include year, month and day in file logs
    let _ = fern::Dispatch::new()
        .format(|out, message, record| {
            out.finish(format_args!(
                "{} {} [{}] {}",
                chrono::Local::now().format("%Y-%m-%dT%H:%M:%S%.3f"),
                record.target(),
                record.level(),
                message
            ))
        })
        .level(level)
        .chain(std::io::stdout())
        .apply();
}

impl Application {
    pub fn new(args: Args, config: Config, lang_loader: syntax::Loader) -> Result<Self, Error> {
        #[cfg(feature = "integration")]
        setup_integration_logging();


        let mut theme_parent_dirs = vec![helix_loader::config_dir()];
        theme_parent_dirs.extend(helix_loader::runtime_dirs().iter().cloned());
        let theme_loader = theme::Loader::new(&theme_parent_dirs);

        #[cfg(not(feature = "integration"))]
        let mut terminal = Terminal::new((&config.editor).into())
            .context("failed to create terminal")?;
        #[cfg(feature = "integration")]
        let mut terminal = Terminal::new(120, 150)?;

        let theme_mode = terminal.get_theme_mode();
        let area = terminal.size();
        let config = Arc::new(ArcSwap::from_pointee(config));
        let handlers = handlers::setup(config.clone());
        let mut editor = Editor::new(
            area,
            Arc::new(theme_loader),
            Arc::new(ArcSwap::from_pointee(lang_loader)),
            Arc::new(Map::new(Arc::clone(&config), |config: &Config| {
                &config.editor
            })),
            handlers,
        );
        Self::load_configured_theme(&mut editor, &config.load(), &mut terminal, theme_mode);

        editor.init_layers(area);
        editor.init_apps(Arc::clone(&config));
        let tab_manager = ui::TabManager::new(Arc::clone(&config));
        editor.push_layer(Box::new(tab_manager));
        editor.push_layer(Box::new(ui::ActiveAppProxy));

        let jobs = Jobs::new();

        // Helper to create a Document from a path
        let open_doc = |path: &Path, editor: &Editor| -> Result<Document, DocumentOpenError> {
            Document::open(
                path,
                None,
                true,
                editor.config.clone(),
                editor.syn_loader.clone(),
            )
        };

        // Collect documents and an optional picker to push
        let mut docs_to_open: Vec<Document> = Vec::new();
        let mut picker_to_push: Option<Box<dyn crate::compositor::Component>> = None;

        if args.load_tutor {
            let path = helix_loader::runtime_file(Path::new("tutor"));
            let mut doc = open_doc(&path, &editor)?;
            doc.set_path(None);
            docs_to_open.push(doc);
        } else if !args.files.is_empty() {
            let mut files_it = args.files.into_iter().peekable();

            // If the first file is a directory, skip it and open a picker
            if let Some((first, _)) = files_it.next_if(|(p, _)| p.is_dir()) {
                let picker = ui::file_picker(&editor, first);
                picker_to_push = Some(Box::new(overlaid(picker)));
            }

            if files_it.peek().is_some() {
                let mut nr_of_files = 0;
                for (file, _pos) in files_it {
                    if file.is_dir() {
                        return Err(anyhow::anyhow!(
                            "expected a path to file, but found a directory: {file:?}. (to open a directory pass it as first argument)"
                        ));
                    }
                    match open_doc(&file, &editor) {
                        Err(DocumentOpenError::IrregularFile) => continue,
                        Err(err) => return Err(anyhow::anyhow!(err)),
                        Ok(doc) => {
                            nr_of_files += 1;
                            docs_to_open.push(doc);
                        }
                    }
                }

                if nr_of_files > 0 {
                    editor.set_status(format!(
                        "Loaded {} file{}.",
                        nr_of_files,
                        if nr_of_files == 1 { "" } else { "s" }
                    ));
                }
            }
        } else if stdin().is_terminal() || cfg!(feature = "integration") {
            if let Some(session_files) = crate::session::load_session() {
                let mut nr_of_files = 0;
                for file in &session_files {
                    match open_doc(file, &editor) {
                        Ok(doc) => {
                            nr_of_files += 1;
                            docs_to_open.push(doc);
                        }
                        Err(err) => log::warn!("Failed to restore session file {}: {err}", file.display()),
                    }
                }
                if nr_of_files > 0 {
                    editor.set_status(format!(
                        "Restored {} file{}.",
                        nr_of_files,
                        if nr_of_files == 1 { "" } else { "s" }
                    ));
                }
            }
        } else {
            docs_to_open.push(Document::default(editor.config.clone(), editor.syn_loader.clone()));
        }

        // Add all documents as tabs
        for doc in docs_to_open {
            editor.add_editor_app(doc);
        }
        if editor.tab_count() > 0 {
            editor.switch_app(0);
            editor.active_tab = 0;
        } else {
            // Ensure a welcome page if nothing was opened
            if editor.app_count() == 0 {
                editor.add_app(Box::new(ui::welcome::WelcomePage::new()));
            }
        }

        // Push the picker on top if needed
        if let Some(picker) = picker_to_push {
            editor.push_layer(picker);
        }

        #[cfg(windows)]
        let signals = futures_util::stream::empty();
        #[cfg(not(windows))]
        let signals = Signals::new([
            signal::SIGTSTP,
            signal::SIGCONT,
            signal::SIGUSR1,
            signal::SIGTERM,
            signal::SIGINT,
        ])
        .context("build signal handler")?;

        let app = Self {
            terminal,
            editor,
            config,
            signals,
            jobs,
            lsp_progress: LspProgressMap::new(),
            theme_mode,
        };

        Ok(app)
    }

    async fn render(&mut self) {
        {
            use crate::layers::LayerState;
            let ls = self.editor.layer_state_mut::<LayerState>();
            if ls.full_redraw {
                self.terminal.clear().expect("Cannot clear the terminal");
                ls.full_redraw = false;
            }
        }

        helix_event::start_frame();
        self.editor.needs_redraw = false;

        let area = self
            .terminal
            .autoresize()
            .expect("Unable to determine terminal size");

        // TODO: need to recalculate view tree if necessary

        let surface = self.terminal.current_buffer_mut();

        self.editor.render_layers(area, surface, &mut self.jobs);
        let (pos, kind) = self.editor.layer_cursor(area);
        // reset cursor cache
        if let Some(dv) = self.editor.tabs.get_mut(self.editor.active_tab) {
            dv.cursor_cache().reset();
        }

        let pos = pos.map(|pos| (pos.col as u16, pos.row as u16));
        self.terminal.draw(pos, kind).unwrap();
    }

    pub async fn event_loop<S>(&mut self, input_stream: &mut S)
    where
        S: Stream<Item = std::io::Result<TerminalEvent>> + Unpin,
    {
        self.render().await;

        loop {
            if !self.event_loop_until_idle(input_stream).await {
                break;
            }
        }
    }

    pub async fn event_loop_until_idle<S>(&mut self, input_stream: &mut S) -> bool
    where
        S: Stream<Item = std::io::Result<TerminalEvent>> + Unpin,
    {
        loop {
            if self.editor.should_close() {
                return false;
            }

            use futures_util::StreamExt;

            tokio::select! {
                biased;

                Some(signal) = self.signals.next() => {
                    if !self.handle_signals(signal).await {
                        return false;
                    };
                }
                Some(event) = input_stream.next() => {
                    self.handle_terminal_events(event).await;
                }
                Some(callback) = self.jobs.callbacks.recv() => {
                    self.jobs.handle_callback(&mut self.editor, Ok(Some(callback)));
                    self.render().await;
                }
                Some(msg) = self.jobs.status_messages.recv() => {
                    let severity = match msg.severity{
                        helix_event::status::Severity::Hint => Severity::Hint,
                        helix_event::status::Severity::Info => Severity::Info,
                        helix_event::status::Severity::Warning => Severity::Warning,
                        helix_event::status::Severity::Error => Severity::Error,
                    };
                    // TODO: show multiple status messages at once to avoid clobbering
                    self.editor.status_msg = Some((msg.message, severity));
                    helix_event::request_redraw();
                }
                Some(callback) = self.jobs.wait_futures.next() => {
                    self.jobs.handle_callback(&mut self.editor, callback);
                    self.render().await;
                }
                event = self.editor.wait_event() => {
                    let _idle_handled = self.handle_editor_event(event).await;

                    #[cfg(feature = "integration")]
                    {
                        if _idle_handled {
                            return true;
                        }
                    }
                }
            }

            // for integration tests only, reset the idle timer after every
            // event to signal when test events are done processing
            #[cfg(feature = "integration")]
            {
                self.editor.reset_idle_timer();
            }
        }
    }

    pub fn handle_config_events(&mut self, config_event: ConfigEvent) {
        let old_editor_config = self.editor.config();

        match config_event {
            ConfigEvent::Refresh => self.refresh_config(),

            // Since only the Application can make changes to Editor's config,
            // the Editor must send up a new copy of a modified config so that
            // the Application can apply it.
            ConfigEvent::Update(editor_config) => {
                let mut app_config = (*self.config.load().clone()).clone();
                app_config.editor = *editor_config;
                if let Err(err) = self.terminal.reconfigure((&app_config.editor).into()) {
                    self.editor.set_error(err.to_string());
                };
                self.config.store(Arc::new(app_config));
            }
            ConfigEvent::ThemeChanged => {
                let _ = self.terminal.set_background_color(
                    self.editor
                        .theme
                        .try_get_exact("ui.background")
                        .and_then(|style| style.bg),
                );
                return;
            }
        }

        // Update all the relevant members in the editor after updating
        // the configuration.
        self.editor.refresh_config(&old_editor_config);

        // reset view position in case softwrap was enabled/disabled
        let scrolloff = self.editor.config().scrolloff;
        {
            let (doc, tree) = self.editor.tabs[self.editor.active_tab].doc_and_tree_mut();
            for (view, _) in tree.views() {
                view.ensure_cursor_in_view(doc, scrolloff);
            }
        }
    }

    fn refresh_config(&mut self) {
        let mut refresh_config = || -> Result<(), Error> {
            let default_config = Config::load_default()
                .map_err(|err| anyhow::anyhow!("Failed to load config: {}", err))?;

            // Update the syntax language loader before setting the theme. Setting the theme will
            // call `Loader::set_scopes` which must be done before the documents are re-parsed for
            // the sake of locals highlighting.
            let lang_loader = helix_core::config::user_lang_loader()?;
            self.editor.syn_loader.store(Arc::new(lang_loader));
            Self::load_configured_theme(
                &mut self.editor,
                &default_config,
                &mut self.terminal,
                self.theme_mode,
            );

            // Re-parse the open document with the new language config.
            {
                let lang_loader = self.editor.syn_loader.load();
                let document = self.editor.tabs[self.editor.active_tab].doc_mut();
                // Re-detect .editorconfig
                document.detect_editor_config();
                document.detect_language(&lang_loader);
                let diagnostics = Editor::doc_diagnostics(
                    &self.editor.language_servers,
                    &self.editor.diagnostics,
                    document,
                );
                document.replace_diagnostics(diagnostics, &[], None);
            }

            self.terminal.reconfigure((&default_config.editor).into())?;
            // Store new config
            self.config.store(Arc::new(default_config));
            Ok(())
        };

        match refresh_config() {
            Ok(_) => {
                self.editor.set_status("Config refreshed");
            }
            Err(err) => {
                self.editor.set_error(err.to_string());
            }
        }
    }

    /// Load the theme set in configuration
    fn load_configured_theme(
        editor: &mut Editor,
        config: &Config,
        terminal: &mut Terminal,
        mode: Option<theme::Mode>,
    ) {
        let true_color = terminal.supports_true_color()
            || config.editor.true_color
            || crate::true_color();
        let theme = config
            .theme
            .as_ref()
            .and_then(|theme_config| {
                let theme = theme_config.choose(mode);
                editor
                    .theme_loader
                    .load(theme)
                    .map_err(|e| {
                        log::warn!("failed to load theme `{}` - {}", theme, e);
                        e
                    })
                    .ok()
                    .filter(|theme| {
                        let colors_ok = true_color || theme.is_16_color();
                        if !colors_ok {
                            log::warn!(
                                "loaded theme `{}` but cannot use it because true color \
                                support is not enabled",
                                theme.name()
                            );
                        }
                        colors_ok
                    })
            })
            .unwrap_or_else(|| editor.theme_loader.default_theme(true_color));
        let background_color = theme
            .try_get_exact("ui.background")
            .and_then(|style| style.bg);
        editor.set_theme(theme);
        let _ = terminal.set_background_color(background_color);
    }

    #[cfg(windows)]
    // no signal handling available on windows
    pub async fn handle_signals(&mut self, _signal: ()) -> bool {
        true
    }

    #[cfg(not(windows))]
    pub async fn handle_signals(&mut self, signal: i32) -> bool {
        match signal {
            signal::SIGTSTP => {
                self.restore_term().unwrap();

                // SAFETY:
                //
                // - helix must have permissions to send signals to all processes in its signal
                //   group, either by already having the requisite permission, or by having the
                //   user's UID / EUID / SUID match that of the receiving process(es).
                let res = unsafe {
                    // A pid of 0 sends the signal to the entire process group, allowing the user to
                    // regain control of their terminal if the editor was spawned under another process
                    // (e.g. when running `git commit`).
                    //
                    // We have to send SIGSTOP (not SIGTSTP) to the entire process group, because,
                    // as mentioned above, the terminal will get stuck if `helix` was spawned from
                    // an external process and that process waits for `helix` to complete. This may
                    // be an issue with signal-hook-tokio, but the author of signal-hook believes it
                    // could be a tokio issue instead:
                    // https://github.com/vorner/signal-hook/issues/132
                    libc::kill(0, signal::SIGSTOP)
                };

                if res != 0 {
                    let err = std::io::Error::last_os_error();
                    eprintln!("{}", err);
                    let res = err.raw_os_error().unwrap_or(1);
                    std::process::exit(res);
                }
            }
            signal::SIGCONT => {
                // Copy/Paste from same issue from neovim:
                // https://github.com/neovim/neovim/issues/12322
                // https://github.com/neovim/neovim/pull/13084
                for retries in 1..=10 {
                    match self.terminal.claim() {
                        Ok(()) => break,
                        Err(err) if retries == 10 => panic!("Failed to claim terminal: {}", err),
                        Err(_) => continue,
                    }
                }

                // redraw the terminal
                let area = self.terminal.size();
                self.editor.resize_layers(area);
                self.terminal.clear().expect("couldn't clear terminal");

                self.render().await;
            }
            signal::SIGUSR1 => {
                self.refresh_config();
                self.render().await;
            }
            signal::SIGTERM | signal::SIGINT => {
                self.restore_term().unwrap();
                return false;
            }
            _ => unreachable!(),
        }

        true
    }

    pub async fn handle_idle_timeout(&mut self) {
        let should_render = self.editor.handle_layer_event(&Event::IdleTimeout, &mut self.jobs);
        if should_render || self.editor.needs_redraw {
            self.render().await;
        }
    }

    // Document writes are not supported in the read-only viewer.
    // This method is kept as a no-op stub.

    #[inline(always)]
    pub async fn handle_editor_event(&mut self, event: EditorEvent) -> bool {
        log::debug!("received editor event: {:?}", event);

        match event {
            EditorEvent::ConfigEvent(event) => {
                self.handle_config_events(event);
                self.render().await;
            }
            EditorEvent::LanguageServerMessage((id, call)) => {
                self.handle_language_server_message(call, id).await;
                // limit render calls for fast language server messages
                helix_event::request_redraw();
            }
            EditorEvent::Redraw => {
                self.render().await;
            }
            EditorEvent::IdleTimer => {
                self.editor.clear_idle_timer();
                self.handle_idle_timeout().await;

                #[cfg(feature = "integration")]
                {
                    return true;
                }
            }
        }

        false
    }

    pub async fn handle_terminal_events(&mut self, event: std::io::Result<TerminalEvent>) {
        // Handle key events
        let should_redraw = match event.unwrap() {
            crossterm::event::Event::Resize(width, height) => {
                self.terminal
                    .resize(Rect::new(0, 0, width, height))
                    .expect("Unable to resize terminal");

                let area = self.terminal.size();

                self.editor.resize_layers(area);

                self.editor
                    .handle_layer_event(&Event::Resize(width, height), &mut self.jobs)
            }
            // Ignore keyboard release events.
            crossterm::event::Event::Key(crossterm::event::KeyEvent {
                kind: crossterm::event::KeyEventKind::Release,
                ..
            }) => false,
            event => self.editor.handle_layer_event(&event.into(), &mut self.jobs),
        };

        // Drain any pending macro keys queued by callbacks
        {
            let keys = self.editor.drain_pending_keys();
            for key in keys {
                self.editor
                    .handle_layer_event(&Event::Key(key.into()), &mut self.jobs);
            }
        }

        if should_redraw && !self.editor.should_close() {
            self.render().await;
        }
    }

    pub async fn handle_language_server_message(
        &mut self,
        call: helix_lsp::Call,
        server_id: LanguageServerId,
    ) {
        use helix_lsp::{Call, MethodCall, Notification};

        macro_rules! language_server {
            () => {
                match self.editor.language_server_by_id(server_id) {
                    Some(language_server) => language_server,
                    None => {
                        warn!("can't find language server with id `{}`", server_id);
                        return;
                    }
                }
            };
        }

        match call {
            Call::Notification(helix_lsp::jsonrpc::Notification { method, params, .. }) => {
                let notification = match Notification::parse(&method, params) {
                    Ok(notification) => notification,
                    Err(helix_lsp::Error::Unhandled) => {
                        info!("Ignoring Unhandled notification from Language Server");
                        return;
                    }
                    Err(err) => {
                        error!(
                            "Ignoring unknown notification from Language Server: {}",
                            err
                        );
                        return;
                    }
                };

                match notification {
                    Notification::Initialized => {
                        let language_server = language_server!();

                        // Trigger a workspace/didChangeConfiguration notification after initialization.
                        // This might not be required by the spec but Neovim does this as well, so it's
                        // probably a good idea for compatibility.
                        if let Some(config) = language_server.config() {
                            language_server.did_change_configuration(config.clone());
                        }

                        helix_event::dispatch(crate::view::events::LanguageServerInitialized {
                            editor: &mut self.editor,
                            server_id,
                        });
                    }
                    Notification::PublishDiagnostics(params) => {
                        let uri = match helix_core::Uri::try_from(params.uri) {
                            Ok(uri) => uri,
                            Err(err) => {
                                log::error!("{err}");
                                return;
                            }
                        };
                        let language_server = language_server!();
                        if !language_server.is_initialized() {
                            log::error!("Discarding publishDiagnostic notification sent by an uninitialized server: {}", language_server.name());
                            return;
                        }
                        let provider = helix_core::diagnostic::DiagnosticProvider::Lsp {
                            server_id,
                            identifier: None,
                        };
                        self.editor.handle_lsp_diagnostics(
                            &provider,
                            uri,
                            params.version,
                            params.diagnostics,
                        );
                    }
                    Notification::ShowMessage(params) => {
                        self.handle_show_message(params.typ, params.message);
                    }
                    Notification::LogMessage(params) => {
                        log::info!("window/logMessage: {:?}", params);
                    }
                    Notification::ProgressMessage(params)
                        if !self
                            .editor
                            .has_layer(std::any::type_name::<ui::Prompt>()) =>
                    {
                        let lsp::ProgressParams {
                            token,
                            value: lsp::ProgressParamsValue::WorkDone(work),
                        } = params;
                        let (title, message, percentage) = match &work {
                            lsp::WorkDoneProgress::Begin(lsp::WorkDoneProgressBegin {
                                title,
                                message,
                                percentage,
                                ..
                            }) => (Some(title), message, percentage),
                            lsp::WorkDoneProgress::Report(lsp::WorkDoneProgressReport {
                                message,
                                percentage,
                                ..
                            }) => (None, message, percentage),
                            lsp::WorkDoneProgress::End(lsp::WorkDoneProgressEnd { message }) => {
                                if message.is_some() {
                                    (None, message, &None)
                                } else {
                                    self.lsp_progress.end_progress(server_id, &token);
                                    if !self.lsp_progress.is_progressing(server_id) {
                                        // TODO: spinner stop (spinners to be moved to Editor)
                                    }
                                    self.editor.clear_status();

                                    // we want to render to clear any leftover spinners or messages
                                    return;
                                }
                            }
                        };

                        if self.editor.config().lsp.display_progress_messages {
                            let title =
                                title.or_else(|| self.lsp_progress.title(server_id, &token));
                            if title.is_some() || percentage.is_some() || message.is_some() {
                                use std::fmt::Write as _;
                                let mut status = format!("{}: ", language_server!().name());
                                if let Some(percentage) = percentage {
                                    write!(status, "{percentage:>2}% ").unwrap();
                                }
                                if let Some(title) = title {
                                    status.push_str(title);
                                }
                                if title.is_some() && message.is_some() {
                                    status.push_str(" ⋅ ");
                                }
                                if let Some(message) = message {
                                    status.push_str(message);
                                }
                                self.editor.set_status(status);
                            }
                        }

                        match work {
                            lsp::WorkDoneProgress::Begin(begin_status) => {
                                self.lsp_progress
                                    .begin(server_id, token.clone(), begin_status);
                            }
                            lsp::WorkDoneProgress::Report(report_status) => {
                                self.lsp_progress
                                    .update(server_id, token.clone(), report_status);
                            }
                            lsp::WorkDoneProgress::End(_) => {
                                self.lsp_progress.end_progress(server_id, &token);
                                if !self.lsp_progress.is_progressing(server_id) {
                                    // TODO: spinner stop (spinners to be moved to Editor)
                                };
                            }
                        }
                    }
                    Notification::ProgressMessage(_params) => {
                        // do nothing
                    }
                    Notification::Exit => {
                        self.editor.set_status("Language server exited");

                        // LSPs may produce diagnostics for files that haven't been opened in helix,
                        // we need to clear those and remove the entries from the list if this leads to
                        // an empty diagnostic list for said files
                        for diags in self.editor.diagnostics.values_mut() {
                            diags.retain(|(_, provider)| {
                                provider.language_server_id() != Some(server_id)
                            });
                        }

                        self.editor.diagnostics.retain(|_, diags| !diags.is_empty());

                        // Clear any diagnostics for the document with this server open.
                        self.editor.tabs[self.editor.active_tab].doc_mut().clear_diagnostics_for_language_server(server_id);

                        helix_event::dispatch(crate::view::events::LanguageServerExited {
                            editor: &mut self.editor,
                            server_id,
                        });

                        // Remove the language server from the registry.
                        self.editor.language_servers.remove_by_id(server_id);
                    }
                }
            }
            Call::MethodCall(helix_lsp::jsonrpc::MethodCall {
                method, params, id, ..
            }) => {
                let reply = match MethodCall::parse(&method, params) {
                    Err(helix_lsp::Error::Unhandled) => {
                        error!(
                            "Language Server: Method {} not found in request {}",
                            method, id
                        );
                        Err(helix_lsp::jsonrpc::Error {
                            code: helix_lsp::jsonrpc::ErrorCode::MethodNotFound,
                            message: format!("Method not found: {}", method),
                            data: None,
                        })
                    }
                    Err(err) => {
                        log::error!(
                            "Language Server: Received malformed method call {} in request {}: {}",
                            method,
                            id,
                            err
                        );
                        Err(helix_lsp::jsonrpc::Error {
                            code: helix_lsp::jsonrpc::ErrorCode::ParseError,
                            message: format!("Malformed method call: {}", method),
                            data: None,
                        })
                    }
                    Ok(MethodCall::WorkDoneProgressCreate(params)) => {
                        self.lsp_progress.create(server_id, params.token);

                        // TODO: spinner start (spinners to be moved to Editor)

                        Ok(serde_json::Value::Null)
                    }
                    Ok(MethodCall::ApplyWorkspaceEdit(params)) => {
                        let language_server = language_server!();
                        if language_server.is_initialized() {
                            let offset_encoding = language_server.offset_encoding();
                            let res = self
                                .editor
                                .apply_workspace_edit(offset_encoding, &params.edit);

                            Ok(json!(lsp::ApplyWorkspaceEditResponse {
                                applied: res.is_ok(),
                                failure_reason: res.as_ref().err().map(|err| err.kind.to_string()),
                                failed_change: res
                                    .as_ref()
                                    .err()
                                    .map(|err| err.failed_change_idx as u32),
                            }))
                        } else {
                            Err(helix_lsp::jsonrpc::Error {
                                code: helix_lsp::jsonrpc::ErrorCode::InvalidRequest,
                                message: "Server must be initialized to request workspace edits"
                                    .to_string(),
                                data: None,
                            })
                        }
                    }
                    Ok(MethodCall::WorkspaceFolders) => {
                        Ok(json!(&*language_server!().workspace_folders().await))
                    }
                    Ok(MethodCall::WorkspaceConfiguration(params)) => {
                        let language_server = language_server!();
                        let result: Vec<_> = params
                            .items
                            .iter()
                            .map(|item| {
                                let mut config = language_server.config()?;
                                if let Some(section) = item.section.as_ref() {
                                    // for some reason some lsps send an empty string (observed in 'vscode-eslint-language-server')
                                    if !section.is_empty() {
                                        for part in section.split('.') {
                                            config = config.get(part)?;
                                        }
                                    }
                                }
                                Some(config)
                            })
                            .collect();
                        Ok(json!(result))
                    }
                    Ok(MethodCall::RegisterCapability(params)) => {
                        if let Some(client) = self.editor.language_servers.get_by_id(server_id) {
                            for reg in params.registrations {
                                match reg.method.as_str() {
                                    lsp::notification::DidChangeWatchedFiles::METHOD => {
                                        let Some(options) = reg.register_options else {
                                            continue;
                                        };
                                        let ops: lsp::DidChangeWatchedFilesRegistrationOptions =
                                            match serde_json::from_value(options) {
                                                Ok(ops) => ops,
                                                Err(err) => {
                                                    log::warn!("Failed to deserialize DidChangeWatchedFilesRegistrationOptions: {err}");
                                                    continue;
                                                }
                                            };
                                        self.editor.language_servers.file_event_handler.register(
                                            client.id(),
                                            Arc::downgrade(client),
                                            reg.id,
                                            ops,
                                        )
                                    }
                                    _ => {
                                        // Language Servers based on the `vscode-languageserver-node` library often send
                                        // client/registerCapability even though we do not enable dynamic registration
                                        // for most capabilities. We should send a MethodNotFound JSONRPC error in this
                                        // case but that rejects the registration promise in the server which causes an
                                        // exit. So we work around this by ignoring the request and sending back an OK
                                        // response.
                                        log::warn!("Ignoring a client/registerCapability request because dynamic capability registration is not enabled. Please report this upstream to the language server");
                                    }
                                }
                            }
                        }

                        Ok(serde_json::Value::Null)
                    }
                    Ok(MethodCall::UnregisterCapability(params)) => {
                        for unreg in params.unregisterations {
                            match unreg.method.as_str() {
                                lsp::notification::DidChangeWatchedFiles::METHOD => {
                                    self.editor
                                        .language_servers
                                        .file_event_handler
                                        .unregister(server_id, unreg.id);
                                }
                                _ => {
                                    log::warn!("Received unregistration request for unsupported method: {}", unreg.method);
                                }
                            }
                        }
                        Ok(serde_json::Value::Null)
                    }
                    Ok(MethodCall::ShowDocument(params)) => {
                        let language_server = language_server!();
                        let offset_encoding = language_server.offset_encoding();

                        let result = self.handle_show_document(params, offset_encoding);
                        Ok(json!(result))
                    }
                    Ok(MethodCall::WorkspaceDiagnosticRefresh) => {
                        let language_server = language_server!().id();

                        if self.editor.tabs[self.editor.active_tab].doc().supports_language_server(language_server) {
                            let doc_id = self.editor.tabs[self.editor.active_tab].doc().id();
                            handlers::diagnostics::request_document_diagnostics(
                                &mut self.editor,
                                doc_id,
                            );
                        }

                        Ok(serde_json::Value::Null)
                    }
                    Ok(MethodCall::ShowMessageRequest(params)) => {
                        if let Some(actions) = params.actions.filter(|a| !a.is_empty()) {
                            let id = id.clone();
                            let select = ui::Select::new(
                                params.message,
                                actions,
                                (),
                                move |editor, action, event| {
                                    let reply = match event {
                                        ui::PromptEvent::Update => return,
                                        ui::PromptEvent::Validate => Some(action.clone()),
                                        ui::PromptEvent::Abort => None,
                                    };
                                    if let Some(language_server) =
                                        editor.language_server_by_id(server_id)
                                    {
                                        if let Err(err) =
                                            language_server.reply(id.clone(), Ok(json!(reply)))
                                        {
                                            log::error!(
                                                "Failed to send reply to server '{}' request {id}: {err}",
                                                language_server.name()
                                            );
                                        }
                                    }
                                },
                            );
                            self.editor
                                .replace_or_push_layer("lsp-show-message-request", select);
                            // Avoid sending a reply. The `Select` callback above sends the reply.
                            return;
                        } else {
                            self.handle_show_message(params.typ, params.message);
                            Ok(serde_json::Value::Null)
                        }
                    }
                };

                let language_server = language_server!();
                if let Err(err) = language_server.reply(id.clone(), reply) {
                    log::error!(
                        "Failed to send reply to server '{}' request {id}: {err}",
                        language_server.name()
                    );
                }
            }
            Call::Invalid { id } => log::error!("LSP invalid method call id={:?}", id),
        }
    }

    fn handle_show_message(&mut self, message_type: lsp::MessageType, message: String) {
        if self.config.load().editor.lsp.display_messages {
            match message_type {
                lsp::MessageType::ERROR => self.editor.set_error(message),
                lsp::MessageType::WARNING => self.editor.set_warning(message),
                _ => self.editor.set_status(message),
            }
        }
    }

    fn handle_show_document(
        &mut self,
        params: lsp::ShowDocumentParams,
        offset_encoding: helix_lsp::OffsetEncoding,
    ) -> lsp::ShowDocumentResult {
        if let lsp::ShowDocumentParams {
            external: Some(true),
            uri,
            ..
        } = params
        {
            self.jobs.callback(crate::open_external_url_callback(uri));
            return lsp::ShowDocumentResult { success: true };
        };

        let lsp::ShowDocumentParams {
            uri,
            selection,
            take_focus: _,
            ..
        } = params;

        let uri = match helix_core::Uri::try_from(uri) {
            Ok(uri) => uri,
            Err(err) => {
                log::error!("{err}");
                return lsp::ShowDocumentResult { success: false };
            }
        };
        // If `Uri` gets another variant other than `Path` this may not be valid.
        let path = uri.as_path().expect("URIs are valid paths");

        let doc = match Document::open(
            path,
            None,
            true,
            self.editor.config.clone(),
            self.editor.syn_loader.clone(),
        ) {
            Ok(doc) => doc,
            Err(err) => {
                log::error!("failed to open path: {:?}: {:?}", uri, err);
                return lsp::ShowDocumentResult { success: false };
            }
        };

        // Add the document as a new tab
        self.editor.add_editor_app(doc);

        if let Some(range) = selection {
            let (view, doc) = current!(self.editor);
            if let Some(new_range) = lsp_range_to_range(doc.text(), range, offset_encoding) {
                doc.set_selection(view.id, Selection::single(new_range.head, new_range.anchor));
                align_view(doc, view, Align::Center);
            } else {
                log::warn!("lsp position out of bounds - {:?}", range);
            };
        };
        lsp::ShowDocumentResult { success: true }
    }

    fn restore_term(&mut self) -> std::io::Result<()> {
        self.terminal.restore()
    }

    #[cfg(not(feature = "integration"))]
    pub fn event_stream(&self) -> impl Stream<Item = std::io::Result<TerminalEvent>> + Unpin {
        crossterm::event::EventStream::new()
    }

    #[cfg(feature = "integration")]
    pub fn event_stream(&self) -> impl Stream<Item = std::io::Result<TerminalEvent>> + Unpin {
        use std::{
            pin::Pin,
            task::{Context, Poll},
        };

        /// A dummy stream that never polls as ready.
        pub struct DummyEventStream;

        impl Stream for DummyEventStream {
            type Item = std::io::Result<TerminalEvent>;

            fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
                Poll::Pending
            }
        }

        DummyEventStream
    }

    pub async fn run<S>(&mut self, input_stream: &mut S) -> Result<i32, Error>
    where
        S: Stream<Item = std::io::Result<TerminalEvent>> + Unpin,
    {
        self.terminal.claim()?;

        self.event_loop(input_stream).await;

        let close_errs = self.close().await;

        self.restore_term()?;

        for err in close_errs {
            self.editor.exit_code = 1;
            eprintln!("Error: {}", err);
        }

        Ok(self.editor.exit_code)
    }

    pub async fn close(&mut self) -> Vec<anyhow::Error> {
        // [NOTE] we intentionally do not return early for errors because we
        //        want to try to run as much cleanup as we can, regardless of
        //        errors along the way

        crate::session::save_session(&self.editor);

        let mut errs = Vec::new();

        if let Err(err) = self
            .jobs
            .finish(&mut self.editor)
            .await
        {
            log::error!("Error executing job: {}", err);
            errs.push(err);
        };

        if self.editor.close_language_servers(None).await.is_err() {
            log::error!("Timed out waiting for language servers to shutdown");
            errs.push(anyhow::format_err!(
                "Timed out waiting for language servers to shutdown"
            ));
        }

        errs
    }
}

impl ui::menu::Item for lsp::MessageActionItem {
    type Data = ();
    fn format(&self, _data: &Self::Data) -> Vec<ratatui::text::Line<'_>> {
        vec![self.title.as_str().into()]
    }
}
