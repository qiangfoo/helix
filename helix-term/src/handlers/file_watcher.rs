use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Duration;

use helix_core::Rope;
use helix_event::register_hook;
use helix_view::document::DiffSource;
use helix_view::events::{DocumentDidClose, DocumentDidOpen};
use helix_view::handlers::{FileWatcherCommand, Handlers};
use helix_view::{DocumentId, Editor};
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc::{self, Sender};

use crate::job;

pub fn spawn() -> Sender<FileWatcherCommand> {
    let (cmd_tx, mut cmd_rx) = mpsc::channel::<FileWatcherCommand>(128);

    // notify requires a std::sync::mpsc sender for its event callback
    let (fs_tx, fs_rx) = std::sync::mpsc::channel();

    let mut watcher = RecommendedWatcher::new(fs_tx, notify::Config::default())
        .expect("failed to create file watcher");

    tokio::spawn(async move {
        // Bridge std::sync::mpsc to tokio: spawn a blocking thread that reads
        // from the std channel and sends to a tokio channel.
        let (event_tx, mut event_rx) = mpsc::channel::<notify::Event>(256);
        tokio::task::spawn_blocking(move || {
            while let Ok(result) = fs_rx.recv() {
                if let Ok(event) = result {
                    if event_tx.blocking_send(event).is_err() {
                        break;
                    }
                }
            }
        });

        let debounce = Duration::from_millis(500);
        let mut pending_paths: HashSet<PathBuf> = HashSet::new();
        let sleep = tokio::time::sleep(debounce);
        tokio::pin!(sleep);
        let mut debounce_active = false;

        // Reference-counted worktree watches for local changes diff buffers
        let mut worktree_refcounts: HashMap<PathBuf, usize> = HashMap::new();

        loop {
            tokio::select! {
                cmd = cmd_rx.recv() => {
                    match cmd {
                        Some(FileWatcherCommand::Watch { path }) => {
                            log::debug!("file_watcher: watching {:?}", path);
                            if let Err(e) = watcher.watch(&path, RecursiveMode::NonRecursive) {
                                log::warn!("file_watcher: failed to watch {:?}: {}", path, e);
                            }
                        }
                        Some(FileWatcherCommand::Unwatch { path }) => {
                            log::debug!("file_watcher: unwatching {:?}", path);
                            let _ = watcher.unwatch(&path);
                        }
                        Some(FileWatcherCommand::WatchWorktree { worktree }) => {
                            let count = worktree_refcounts.entry(worktree.clone()).or_insert(0);
                            *count += 1;
                            if *count == 1 {
                                log::debug!("file_watcher: watching worktree {:?}", worktree);
                                if let Err(e) = watcher.watch(&worktree, RecursiveMode::Recursive) {
                                    log::warn!("file_watcher: failed to watch worktree {:?}: {}", worktree, e);
                                }
                            }
                        }
                        Some(FileWatcherCommand::UnwatchWorktree { worktree }) => {
                            if let Some(count) = worktree_refcounts.get_mut(&worktree) {
                                *count -= 1;
                                if *count == 0 {
                                    worktree_refcounts.remove(&worktree);
                                    log::debug!("file_watcher: unwatching worktree {:?}", worktree);
                                    let _ = watcher.unwatch(&worktree);
                                }
                            }
                        }
                        None => break, // channel closed
                    }
                }
                Some(event) = event_rx.recv() => {
                    match event.kind {
                        // Accept content/name modifications and creates.
                        // Exclude pure metadata changes (permissions, ownership).
                        // macOS FSEvents reports Modify(Any) for content changes.
                        EventKind::Modify(notify::event::ModifyKind::Metadata(_)) => {}
                        EventKind::Modify(_) | EventKind::Create(_) => {
                            log::debug!("file_watcher: event {:?} paths {:?}", event.kind, event.paths);
                            for path in event.paths {
                                pending_paths.insert(helix_stdx::path::canonicalize(&path));
                            }
                            // Reset the debounce timer
                            sleep.as_mut().reset(tokio::time::Instant::now() + debounce);
                            debounce_active = true;
                        }
                        _ => {}
                    }
                }
                () = &mut sleep, if debounce_active => {
                    debounce_active = false;
                    let paths: Vec<PathBuf> = pending_paths.drain().collect();
                    if paths.is_empty() {
                        continue;
                    }

                    let mut file_paths = Vec::new();
                    let mut diff_refresh_needed = false;
                    for path in paths {
                        if worktree_refcounts.keys().any(|wt| path.starts_with(wt)) {
                            diff_refresh_needed = true;
                            // Also allow regular file reload (but not for .git/ internals)
                            if !path.components().any(|c| c.as_os_str() == ".git") {
                                file_paths.push(path);
                            }
                        } else {
                            file_paths.push(path);
                        }
                    }

                    if !file_paths.is_empty() {
                        dispatch_reloads(file_paths);
                    }
                    if diff_refresh_needed {
                        dispatch_diff_refreshes();
                    }
                }
            }
        }
    });

    cmd_tx
}

fn dispatch_reloads(paths: Vec<PathBuf>) {
    job::dispatch_blocking(move |editor, _compositor| {
        if !editor.config().auto_reload {
            return;
        }

        let scrolloff = editor.config().scrolloff;

        for path in &paths {
            let doc_id = match editor.document_id_by_path(path) {
                Some(id) => id,
                None => {
                    log::debug!("file_watcher: no document found for path {:?}", path);
                    continue;
                }
            };

            let doc = editor.documents.get(&doc_id).unwrap();
            let view_ids: Vec<_> = doc.selections().keys().cloned().collect();
            if view_ids.is_empty() {
                continue;
            }

            // Use direct field access to avoid borrow conflicts
            let doc = editor.documents.get_mut(&doc_id).unwrap();
            let view = editor.tree.get_mut(view_ids[0]);
            view.sync_changes(doc);

            let diff_providers = &editor.diff_providers;
            if let Err(error) = doc.reload(view, diff_providers) {
                log::warn!("Failed to reload {:?}: {}", path, error);
                continue;
            }

            if let Some(path) = editor.documents.get(&doc_id).and_then(|d| d.path().cloned()) {
                editor
                    .language_servers
                    .file_event_handler
                    .file_changed(path);
            }

            for &view_id in &view_ids {
                let doc = editor.documents.get_mut(&doc_id).unwrap();
                let view = editor.tree.get_mut(view_id);
                if view.doc == doc_id {
                    view.ensure_cursor_in_view(doc, scrolloff);
                }
            }

            let name = editor
                .documents
                .get(&doc_id)
                .and_then(|d| d.path())
                .and_then(|p| p.file_name())
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            editor.set_status(format!("Reloaded '{}'", name));
        }
    });
}

fn dispatch_diff_refreshes() {
    job::dispatch_blocking(move |editor, _compositor| {
        let diff_providers = editor.diff_providers.clone();

        let diff_docs: Vec<(DocumentId, DiffSource)> = editor
            .documents()
            .filter_map(|doc| doc.diff_source.as_ref().map(|src| (doc.id(), src.clone())))
            .collect();

        if diff_docs.is_empty() {
            return;
        }

        // Regenerate diffs in a blocking background task
        tokio::task::spawn_blocking(move || {
            let results: Vec<(DocumentId, String)> = diff_docs
                .into_iter()
                .filter_map(|(id, source)| {
                    let text = match &source {
                        DiffSource::LocalChanges { cwd } => {
                            diff_providers.get_local_diff(cwd).unwrap_or_default()
                        }
                        // CommitDiff doesn't refresh (hash is immutable)
                        DiffSource::CommitDiff { .. } => return None,
                    };
                    Some((id, text))
                })
                .collect();

            // Apply results back on the main thread
            job::dispatch_blocking(move |editor, _compositor| {
                let scrolloff = editor.config().scrolloff;
                for (doc_id, new_text) in results {
                    apply_diff_update(editor, doc_id, &new_text, scrolloff);
                }
            });
        });
    });
}

fn apply_diff_update(editor: &mut Editor, doc_id: DocumentId, new_text: &str, scrolloff: usize) {
    let doc = match editor.documents.get_mut(&doc_id) {
        Some(d) => d,
        None => return,
    };

    let new_rope = Rope::from(new_text);
    let transaction = helix_core::diff::compare_ropes(doc.text(), &new_rope);
    if transaction.changes().is_empty() {
        return;
    }

    let view_ids: Vec<_> = doc.selections().keys().cloned().collect();
    if let Some(&view_id) = view_ids.first() {
        doc.apply(&transaction, view_id);

        for &vid in &view_ids {
            let doc = editor.documents.get_mut(&doc_id).unwrap();
            let view = editor.tree.get_mut(vid);
            if view.doc == doc_id {
                view.ensure_cursor_in_view(doc, scrolloff);
            }
        }
    }
}

pub fn register_hooks(handlers: &Handlers) {
    let tx = handlers.file_watcher.clone();
    register_hook!(move |event: &mut DocumentDidOpen<'_>| {
        let doc = event.editor.document(event.doc).unwrap();
        if let Some(path) = doc.path().cloned() {
            helix_event::send_blocking(&tx, FileWatcherCommand::Watch { path });
        }
        // For local changes diff buffers, watch the worktree recursively
        if let Some(DiffSource::LocalChanges { cwd }) = &doc.diff_source {
            helix_event::send_blocking(
                &tx,
                FileWatcherCommand::WatchWorktree { worktree: cwd.clone() },
            );
        }
        Ok(())
    });

    let tx = handlers.file_watcher.clone();
    register_hook!(move |event: &mut DocumentDidClose<'_>| {
        if let Some(path) = event.doc.path().cloned() {
            helix_event::send_blocking(&tx, FileWatcherCommand::Unwatch { path });
        }
        if let Some(DiffSource::LocalChanges { cwd }) = &event.doc.diff_source {
            helix_event::send_blocking(
                &tx,
                FileWatcherCommand::UnwatchWorktree { worktree: cwd.clone() },
            );
        }
        Ok(())
    });
}
