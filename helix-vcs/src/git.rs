use anyhow::{bail, Context, Result};
use arc_swap::ArcSwap;
use gix::filter::plumbing::driver::apply::Delay;
use std::fmt::Write as _;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use gix::bstr::ByteSlice;
use gix::diff::Rewrites;
use gix::dir::entry::Status;
use gix::objs::tree::EntryKind;
use gix::sec::trust::DefaultForLevel;
use gix::status::{
    index_worktree::Item,
    plumbing::index_as_worktree::{Change, EntryStatus},
    UntrackedFiles,
};
use gix::{Commit, ObjectId, Repository, ThreadSafeRepository};
use crate::{CommitInfo, FileChange};

#[cfg(test)]
mod test;

#[inline]
fn get_repo_dir(file: &Path) -> Result<&Path> {
    file.parent().context("file has no parent directory")
}

pub fn get_diff_base(file: &Path) -> Result<Vec<u8>> {
    debug_assert!(!file.exists() || file.is_file());
    debug_assert!(file.is_absolute());
    let file = gix::path::realpath(file).context("resolve symlinks")?;

    // TODO cache repository lookup

    let repo_dir = get_repo_dir(&file)?;
    let repo = open_repo(repo_dir)
        .context("failed to open git repo")?
        .to_thread_local();
    let head = repo.head_commit()?;
    let file_oid = find_file_in_commit(&repo, &head, &file)?;

    let file_object = repo.find_object(file_oid)?;
    let data = file_object.detach().data;
    // Get the actual data that git would make out of the git object.
    // This will apply the user's git config or attributes like crlf conversions.
    if let Some(work_dir) = repo.workdir() {
        let rela_path = file.strip_prefix(work_dir)?;
        let rela_path = gix::path::try_into_bstr(rela_path)?;
        let (mut pipeline, _) = repo.filter_pipeline(None)?;
        let mut worktree_outcome =
            pipeline.convert_to_worktree(&data, rela_path.as_ref(), Delay::Forbid)?;
        let mut buf = Vec::with_capacity(data.len());
        worktree_outcome.read_to_end(&mut buf)?;
        Ok(buf)
    } else {
        Ok(data)
    }
}

pub fn get_repo_info(file: &Path) -> Result<(Arc<ArcSwap<Box<str>>>, Option<String>)> {
    debug_assert!(!file.exists() || file.is_file());
    debug_assert!(file.is_absolute());
    let file = gix::path::realpath(file).context("resolve symlinks")?;

    let repo_dir = get_repo_dir(&file)?;
    let repo = open_repo(repo_dir)
        .context("failed to open git repo")?
        .to_thread_local();

    let head_ref = repo.head_ref()?;
    let head_commit = repo.head_commit()?;
    let head_name = match head_ref {
        Some(reference) => reference.name().shorten().to_string(),
        None => head_commit.id.to_hex_with_len(8).to_string(),
    };

    let worktree_name = repo.workdir().and_then(|w| {
        w.file_name()
            .map(|n| n.to_string_lossy().to_string())
    });

    Ok((
        Arc::new(ArcSwap::from_pointee(head_name.into_boxed_str())),
        worktree_name,
    ))
}

pub fn get_worktree_root(cwd: &Path) -> Result<PathBuf> {
    let repo = open_repo(cwd)?.to_thread_local();
    let workdir = repo
        .workdir()
        .ok_or_else(|| anyhow::anyhow!("bare repository"))?;
    Ok(workdir.to_path_buf())
}

pub fn for_each_changed_file(cwd: &Path, f: impl Fn(Result<FileChange>) -> bool) -> Result<()> {
    status(&open_repo(cwd)?.to_thread_local(), f)
}

fn open_repo(path: &Path) -> Result<ThreadSafeRepository> {
    // custom open options
    let mut git_open_opts_map = gix::sec::trust::Mapping::<gix::open::Options>::default();

    // On windows various configuration options are bundled as part of the installations
    // This path depends on the install location of git and therefore requires some overhead to lookup
    // This is basically only used on windows and has some overhead hence it's disabled on other platforms.
    // `gitoxide` doesn't use this as default
    let config = gix::open::permissions::Config {
        system: true,
        git: true,
        user: true,
        env: true,
        includes: true,
        git_binary: cfg!(windows),
    };
    // change options for config permissions without touching anything else
    git_open_opts_map.reduced = git_open_opts_map
        .reduced
        .permissions(gix::open::Permissions {
            config,
            ..gix::open::Permissions::default_for_level(gix::sec::Trust::Reduced)
        });
    git_open_opts_map.full = git_open_opts_map.full.permissions(gix::open::Permissions {
        config,
        ..gix::open::Permissions::default_for_level(gix::sec::Trust::Full)
    });

    let open_options = gix::discover::upwards::Options {
        dot_git_only: true,
        ..Default::default()
    };

    let res = ThreadSafeRepository::discover_with_environment_overrides_opts(
        path,
        open_options,
        git_open_opts_map,
    )?;

    Ok(res)
}

/// Returns the path to the `.git` directory for the given working directory.
pub fn get_git_dir(cwd: &Path) -> Result<PathBuf> {
    let repo = open_repo(cwd)?.to_thread_local();
    Ok(repo.git_dir().to_path_buf())
}

/// Emulates the result of running `git status` from the command line.
fn status(repo: &Repository, f: impl Fn(Result<FileChange>) -> bool) -> Result<()> {
    let work_dir = repo
        .workdir()
        .ok_or_else(|| anyhow::anyhow!("working tree not found"))?
        .to_path_buf();

    let status_platform = repo
        .status(gix::progress::Discard)?
        // Here we discard the `status.showUntrackedFiles` config, as it makes little sense in
        // our case to not list new (untracked) files. We could have respected this config
        // if the default value weren't `Collapsed` though, as this default value would render
        // the feature unusable to many.
        .untracked_files(UntrackedFiles::Files)
        // Turn on file rename detection, which is off by default.
        .index_worktree_rewrites(Some(Rewrites {
            copies: None,
            percentage: Some(0.5),
            limit: 1000,
            ..Default::default()
        }));

    // No filtering based on path
    let empty_patterns = vec![];

    let status_iter = status_platform.into_index_worktree_iter(empty_patterns)?;

    for item in status_iter {
        let Ok(item) = item.map_err(|err| f(Err(err.into()))) else {
            continue;
        };
        let change = match item {
            Item::Modification {
                rela_path, status, ..
            } => {
                let path = work_dir.join(rela_path.to_path()?);
                match status {
                    EntryStatus::Conflict { .. } => FileChange::Conflict { path },
                    EntryStatus::Change(Change::Removed) => FileChange::Deleted { path },
                    EntryStatus::Change(Change::Modification { .. }) => {
                        FileChange::Modified { path }
                    }
                    // Files marked with `git add --intent-to-add`. Such files
                    // still show up as new in `git status`, so it's appropriate
                    // to show them the same way as untracked files in the
                    // "changed file" picker. One example of this being used
                    // is Jujutsu, a Git-compatible VCS. It marks all new files
                    // with `--intent-to-add` automatically.
                    EntryStatus::IntentToAdd => FileChange::Untracked { path },
                    _ => continue,
                }
            }
            Item::DirectoryContents { entry, .. } if entry.status == Status::Untracked => {
                FileChange::Untracked {
                    path: work_dir.join(entry.rela_path.to_path()?),
                }
            }
            Item::Rewrite {
                source,
                dirwalk_entry,
                ..
            } => FileChange::Renamed {
                from_path: work_dir.join(source.rela_path().to_path()?),
                to_path: work_dir.join(dirwalk_entry.rela_path.to_path()?),
            },
            _ => continue,
        };
        if !f(Ok(change)) {
            break;
        }
    }

    Ok(())
}

/// Finds the object that contains the contents of a file at a specific commit.
fn find_file_in_commit(repo: &Repository, commit: &Commit, file: &Path) -> Result<ObjectId> {
    let repo_dir = repo.workdir().context("repo has no worktree")?;
    let rel_path = file.strip_prefix(repo_dir)?;
    let tree = commit.tree()?;
    let tree_entry = tree
        .lookup_entry_by_path(rel_path)?
        .context("file is untracked")?;
    match tree_entry.mode().kind() {
        // not a file, everything is new, do not show diff
        mode @ (EntryKind::Tree | EntryKind::Commit | EntryKind::Link) => {
            bail!("entry at {} is not a file but a {mode:?}", file.display())
        }
        // found a file
        EntryKind::Blob | EntryKind::BlobExecutable => Ok(tree_entry.object_id()),
    }
}

fn format_relative_time(seconds_since_epoch: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let delta = now - seconds_since_epoch;
    if delta < 60 {
        "just now".to_string()
    } else if delta < 3600 {
        let mins = delta / 60;
        format!("{mins} min ago")
    } else if delta < 86400 {
        let hours = delta / 3600;
        format!("{hours} hours ago")
    } else if delta < 86400 * 30 {
        let days = delta / 86400;
        format!("{days} days ago")
    } else if delta < 86400 * 365 {
        let months = delta / (86400 * 30);
        format!("{months} months ago")
    } else {
        let years = delta / (86400 * 365);
        format!("{years} years ago")
    }
}

pub fn get_commit_log(cwd: &Path, max_count: usize) -> Result<Vec<CommitInfo>> {
    let repo = open_repo(cwd)?.to_thread_local();
    let head = repo.head_commit()?;
    let mut commits = Vec::with_capacity(max_count);

    let walk = head.ancestors().all()?;
    for info in walk.take(max_count) {
        let info = info?;
        let commit = info.object()?;
        let hash = commit.id.to_string();
        let short_hash = commit.id.to_hex_with_len(7).to_string();
        let message = commit
            .message()?
            .summary()
            .to_string();
        let author_sig = commit.author()?;
        let author = author_sig.name.to_string();
        let date = author_sig
            .time()
            .map(|t| format_relative_time(t.seconds))
            .unwrap_or_default();

        commits.push(CommitInfo {
            hash,
            short_hash,
            message,
            author,
            date,
        });
    }

    Ok(commits)
}

/// Generate a unified diff string for a given file between two blob contents.
fn unified_diff_for_blobs(old: &[u8], new: &[u8], old_name: &str, new_name: &str) -> String {
    let old_str = String::from_utf8_lossy(old);
    let new_str = String::from_utf8_lossy(new);

    let input = imara_diff::InternedInput::new(old_str.as_ref(), new_str.as_ref());
    let mut diff = imara_diff::Diff::default();
    diff.compute_with(
        imara_diff::Algorithm::Histogram,
        &input.before,
        &input.after,
        input.interner.num_tokens(),
    );

    let printer = imara_diff::BasicLineDiffPrinter(&input.interner);
    let unified = diff.unified_diff(
        &printer,
        imara_diff::UnifiedDiffConfig::default(),
        &input,
    );
    let unified_str = unified.to_string();
    if unified_str.is_empty() {
        return String::new();
    }

    let mut result = String::new();
    let _ = writeln!(result, "--- a/{old_name}");
    let _ = writeln!(result, "+++ b/{new_name}");
    result.push_str(&unified_str);
    result
}

/// Generate a unified diff for a commit vs its parent.
pub fn get_commit_diff(cwd: &Path, commit_hash: &str) -> Result<String> {
    let repo = open_repo(cwd)?.to_thread_local();
    let oid = gix::ObjectId::from_hex(commit_hash.as_bytes())?;
    let commit = repo.find_commit(oid)?;
    let tree = commit.tree()?;

    let parent_tree = commit
        .parent_ids()
        .next()
        .and_then(|pid| pid.object().ok())
        .and_then(|obj| obj.into_commit().tree().ok());

    let mut output = String::new();

    // Compare trees entry by entry
    diff_trees(&repo, parent_tree.as_ref(), &tree, "", &mut output)?;

    Ok(output)
}

fn collect_tree_entries(
    tree: &gix::Tree<'_>,
) -> Result<std::collections::BTreeMap<String, (EntryKind, ObjectId)>> {
    let mut entries = std::collections::BTreeMap::new();
    for entry in tree.iter() {
        let entry = entry?;
        let name = entry.filename().to_string();
        entries.insert(name, (entry.mode().kind(), entry.object_id()));
    }
    Ok(entries)
}

fn diff_trees(
    repo: &Repository,
    old_tree: Option<&gix::Tree<'_>>,
    new_tree: &gix::Tree<'_>,
    prefix: &str,
    output: &mut String,
) -> Result<()> {
    let old_entries = match old_tree {
        Some(t) => collect_tree_entries(t)?,
        None => std::collections::BTreeMap::new(),
    };
    let new_entries = collect_tree_entries(new_tree)?;

    // All paths from both trees
    let mut all_paths: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
    for k in old_entries.keys() {
        all_paths.insert(k.as_str());
    }
    for k in new_entries.keys() {
        all_paths.insert(k.as_str());
    }

    for name in all_paths {
        let path = if prefix.is_empty() {
            name.to_string()
        } else {
            format!("{prefix}/{name}")
        };

        let old_entry = old_entries.get(name);
        let new_entry = new_entries.get(name);

        match (old_entry, new_entry) {
            (Some(&(EntryKind::Tree, old_oid)), Some(&(EntryKind::Tree, new_oid))) => {
                if old_oid != new_oid {
                    let old_t = repo.find_object(old_oid)?.into_tree();
                    let new_t = repo.find_object(new_oid)?.into_tree();
                    diff_trees(repo, Some(&old_t), &new_t, &path, output)?;
                }
            }
            (None, Some(&(EntryKind::Tree, new_oid))) => {
                let new_t = repo.find_object(new_oid)?.into_tree();
                diff_trees(repo, None, &new_t, &path, output)?;
            }
            (Some(&(EntryKind::Tree, old_oid)), None) => {
                let old_t = repo.find_object(old_oid)?.into_tree();
                diff_deleted_tree(repo, &old_t, &path, output)?;
            }
            (Some(&(_, old_oid)), Some(&(_, new_oid)))
                if matches!(
                    old_entries.get(name).map(|e| e.0),
                    Some(EntryKind::Blob | EntryKind::BlobExecutable)
                ) && matches!(
                    new_entries.get(name).map(|e| e.0),
                    Some(EntryKind::Blob | EntryKind::BlobExecutable)
                ) =>
            {
                if old_oid != new_oid {
                    let old_blob = repo.find_object(old_oid)?.detach().data;
                    let new_blob = repo.find_object(new_oid)?.detach().data;
                    let diff_text = unified_diff_for_blobs(&old_blob, &new_blob, &path, &path);
                    if !diff_text.is_empty() {
                        output.push_str(&diff_text);
                    }
                }
            }
            (None, Some(&(kind, new_oid)))
                if matches!(kind, EntryKind::Blob | EntryKind::BlobExecutable) =>
            {
                let new_blob = repo.find_object(new_oid)?.detach().data;
                let diff_text = unified_diff_for_blobs(&[], &new_blob, &path, &path);
                if !diff_text.is_empty() {
                    output.push_str(&diff_text);
                }
            }
            (Some(&(kind, old_oid)), None)
                if matches!(kind, EntryKind::Blob | EntryKind::BlobExecutable) =>
            {
                let old_blob = repo.find_object(old_oid)?.detach().data;
                let diff_text = unified_diff_for_blobs(&old_blob, &[], &path, &path);
                if !diff_text.is_empty() {
                    output.push_str(&diff_text);
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn diff_deleted_tree(
    repo: &Repository,
    tree: &gix::Tree<'_>,
    prefix: &str,
    output: &mut String,
) -> Result<()> {
    for entry in tree.iter() {
        let entry = entry?;
        let name = entry.filename().to_string();
        let path = format!("{prefix}/{name}");
        match entry.mode().kind() {
            EntryKind::Tree => {
                let sub = repo.find_object(entry.object_id())?.into_tree();
                diff_deleted_tree(repo, &sub, &path, output)?;
            }
            EntryKind::Blob | EntryKind::BlobExecutable => {
                let old_blob = repo.find_object(entry.object_id())?.detach().data;
                let diff_text = unified_diff_for_blobs(&old_blob, &[], &path, &path);
                if !diff_text.is_empty() {
                    output.push_str(&diff_text);
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// Generate a unified diff of local working tree changes vs HEAD.
pub fn get_local_diff(cwd: &Path) -> Result<String> {
    let repo = open_repo(cwd)?.to_thread_local();
    let work_dir = repo
        .workdir()
        .context("no working tree")?
        .to_path_buf();
    let head = repo.head_commit()?;

    // Collect changed files first, then generate diffs
    let changes = std::cell::RefCell::new(Vec::new());
    status(&repo, |change| {
        if let Ok(change) = change {
            changes.borrow_mut().push(change);
        }
        true
    })?;
    let changes = changes.into_inner();

    let mut output = String::new();
    for change in &changes {
        let result: Result<()> = (|| {
            match change {
                FileChange::Modified { path } | FileChange::Conflict { path } => {
                    let old_oid = find_file_in_commit(&repo, &head, path)?;
                    let old_blob = repo.find_object(old_oid)?.detach().data;
                    let new_blob = std::fs::read(path)?;
                    let rel = path.strip_prefix(&work_dir).unwrap_or(path);
                    let rel_str = rel.to_string_lossy();
                    let diff_text =
                        unified_diff_for_blobs(&old_blob, &new_blob, &rel_str, &rel_str);
                    if !diff_text.is_empty() {
                        output.push_str(&diff_text);
                    }
                }
                FileChange::Untracked { path } => {
                    let new_blob = std::fs::read(path)?;
                    let rel = path.strip_prefix(&work_dir).unwrap_or(path);
                    let rel_str = rel.to_string_lossy();
                    let diff_text = unified_diff_for_blobs(&[], &new_blob, &rel_str, &rel_str);
                    if !diff_text.is_empty() {
                        output.push_str(&diff_text);
                    }
                }
                FileChange::Deleted { path } => {
                    let old_oid = find_file_in_commit(&repo, &head, path)?;
                    let old_blob = repo.find_object(old_oid)?.detach().data;
                    let rel = path.strip_prefix(&work_dir).unwrap_or(path);
                    let rel_str = rel.to_string_lossy();
                    let diff_text = unified_diff_for_blobs(&old_blob, &[], &rel_str, &rel_str);
                    if !diff_text.is_empty() {
                        output.push_str(&diff_text);
                    }
                }
                FileChange::Renamed {
                    from_path, to_path, ..
                } => {
                    let old_oid = find_file_in_commit(&repo, &head, from_path)?;
                    let old_blob = repo.find_object(old_oid)?.detach().data;
                    let new_blob = std::fs::read(to_path)?;
                    let old_rel = from_path.strip_prefix(&work_dir).unwrap_or(from_path);
                    let new_rel = to_path.strip_prefix(&work_dir).unwrap_or(to_path);
                    let diff_text = unified_diff_for_blobs(
                        &old_blob,
                        &new_blob,
                        &old_rel.to_string_lossy(),
                        &new_rel.to_string_lossy(),
                    );
                    if !diff_text.is_empty() {
                        output.push_str(&diff_text);
                    }
                }
            }
            Ok(())
        })();
        if let Err(e) = result {
            log::debug!("Error generating diff for file: {e:#}");
        }
    }

    Ok(output)
}
