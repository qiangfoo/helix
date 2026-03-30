use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use helix_view::Editor;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
struct SessionData {
    files: Vec<PathBuf>,
}

fn session_dir() -> PathBuf {
    helix_loader::cache_dir().join("sessions")
}

fn session_file_path(worktree_root: &Path) -> PathBuf {
    let mut hasher = DefaultHasher::new();
    worktree_root.hash(&mut hasher);
    let hash = hasher.finish();
    session_dir().join(format!("{hash:x}.json"))
}

fn worktree_root() -> Option<PathBuf> {
    let cwd = std::env::current_dir().ok()?;
    helix_vcs::get_worktree_root(&cwd)
}

pub fn save_session(editor: &Editor) {
    let Some(root) = worktree_root() else {
        return;
    };

    let files: Vec<PathBuf> = editor.tabs
        .iter()
        .filter_map(|dv| dv.doc.path().cloned())
        .collect();

    if files.is_empty() {
        let path = session_file_path(&root);
        let _ = std::fs::remove_file(path);
        return;
    }

    let data = SessionData { files };

    let dir = session_dir();
    if let Err(err) = std::fs::create_dir_all(&dir) {
        log::error!("Failed to create session directory: {err}");
        return;
    }

    let path = session_file_path(&root);
    match serde_json::to_string(&data) {
        Ok(json) => {
            if let Err(err) = std::fs::write(&path, json) {
                log::error!("Failed to write session file: {err}");
            }
        }
        Err(err) => log::error!("Failed to serialize session: {err}"),
    }
}

pub fn load_session() -> Option<Vec<PathBuf>> {
    let root = worktree_root()?;
    let path = session_file_path(&root);
    let json = std::fs::read_to_string(&path).ok()?;
    let data: SessionData = serde_json::from_str(&json).ok()?;

    let files: Vec<PathBuf> = data.files.into_iter().filter(|f| f.exists()).collect();

    if files.is_empty() {
        None
    } else {
        Some(files)
    }
}
