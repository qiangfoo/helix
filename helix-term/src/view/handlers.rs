use std::path::PathBuf;

use helix_event::send_blocking;
use tokio::sync::mpsc::Sender;

use crate::view::handlers::lsp::SignatureHelpInvoked;
use crate::view::Editor;

#[derive(Debug)]
pub enum FileWatcherCommand {
    Watch { path: PathBuf },
    Unwatch { path: PathBuf },
    /// Watch a working tree directory recursively for local changes diff buffers.
    WatchWorktree { worktree: PathBuf },
    UnwatchWorktree { worktree: PathBuf },
}

pub mod diagnostics;
pub mod lsp;

pub struct Handlers {
    pub signature_hints: Sender<lsp::SignatureHelpEvent>,
    pub document_colors: Sender<lsp::DocumentColorsEvent>,
    pub document_links: Sender<lsp::DocumentLinksEvent>,
    pub pull_diagnostics: Sender<lsp::PullDiagnosticsEvent>,
    pub pull_all_documents_diagnostics: Sender<lsp::PullAllDocumentsDiagnosticsEvent>,
    pub file_watcher: Sender<FileWatcherCommand>,
}

impl Handlers {
    pub fn trigger_signature_help(&self, invocation: SignatureHelpInvoked, editor: &Editor) {
        let event = match invocation {
            SignatureHelpInvoked::Automatic => {
                if !editor.config().lsp.auto_signature_help {
                    return;
                }
                lsp::SignatureHelpEvent::Trigger
            }
            SignatureHelpInvoked::Manual => lsp::SignatureHelpEvent::Invoked,
        };
        send_blocking(&self.signature_hints, event)
    }
}

pub fn register_hooks(handlers: &Handlers) {
    lsp::register_hooks(handlers);
}
