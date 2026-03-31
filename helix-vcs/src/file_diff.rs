use crate::{DiffLineKind, FileDiffHunk};

/// Build structured hunks for a pair of old/new blob contents.
///
/// Returns a list of `FileDiffHunk` with context lines (up to 3 surrounding
/// each change, matching unified-diff conventions).
pub fn structured_diff_for_blobs(old: &[u8], new: &[u8]) -> Vec<FileDiffHunk> {
    let old_str = String::from_utf8_lossy(old);
    let new_str = String::from_utf8_lossy(new);

    let input = imara_diff::InternedInput::new(old_str.as_ref(), new_str.as_ref());
    let mut diff = imara_diff::Diff::compute(imara_diff::Algorithm::Histogram, &input);
    diff.postprocess_lines(&input);

    let old_lines: Vec<&str> = input
        .before
        .iter()
        .map(|&tok| input.interner[tok].as_ref())
        .collect();
    let new_lines: Vec<&str> = input
        .after
        .iter()
        .map(|&tok| input.interner[tok].as_ref())
        .collect();

    let raw_hunks: Vec<_> = diff.hunks().collect();
    if raw_hunks.is_empty() {
        return Vec::new();
    }

    // Group raw hunks that are within 6 lines of context into unified hunks
    // (3 lines after one hunk + 3 lines before the next = 6 lines gap)
    const CONTEXT: usize = 3;
    let mut groups: Vec<Vec<&imara_diff::Hunk>> = Vec::new();
    let mut current_group: Vec<&imara_diff::Hunk> = vec![&raw_hunks[0]];

    for hunk in &raw_hunks[1..] {
        let prev = current_group.last().unwrap();
        let gap_before = hunk.before.start as usize - prev.before.end as usize;
        let gap_after = hunk.after.start as usize - prev.after.end as usize;
        let gap = gap_before.min(gap_after);
        if gap <= CONTEXT * 2 {
            current_group.push(hunk);
        } else {
            groups.push(std::mem::take(&mut current_group));
            current_group.push(hunk);
        }
    }
    groups.push(current_group);

    let mut result = Vec::with_capacity(groups.len());

    for group in &groups {
        let first = group.first().unwrap();
        let last = group.last().unwrap();

        let ctx_before_start = (first.before.start as usize).saturating_sub(CONTEXT);
        let ctx_after_end_old = (last.before.end as usize + CONTEXT).min(old_lines.len());
        let ctx_after_end_new = (last.after.end as usize + CONTEXT).min(new_lines.len());

        let mut lines: Vec<(DiffLineKind, String)> = Vec::new();
        let mut old_pos = ctx_before_start;
        let mut new_pos = (first.after.start as usize).saturating_sub(CONTEXT);

        let hunk_old_start = ctx_before_start + 1; // 1-indexed
        let hunk_new_start = new_pos + 1;

        // Add hunk header line
        lines.push((
            DiffLineKind::HunkHeader,
            format!(
                "@@ -{},{} +{},{} @@",
                hunk_old_start,
                ctx_after_end_old - ctx_before_start,
                hunk_new_start,
                ctx_after_end_new - ((first.after.start as usize).saturating_sub(CONTEXT)),
            ),
        ));

        for hunk in group.iter() {
            let hunk_old_start_idx = hunk.before.start as usize;
            let hunk_old_end_idx = hunk.before.end as usize;
            let hunk_new_start_idx = hunk.after.start as usize;
            let hunk_new_end_idx = hunk.after.end as usize;

            // Context lines before this hunk (shared between old and new)
            while old_pos < hunk_old_start_idx {
                let line = strip_newline(old_lines.get(old_pos).copied().unwrap_or(""));
                lines.push((DiffLineKind::Context, line.to_string()));
                old_pos += 1;
                new_pos += 1;
            }

            // Deleted lines
            for i in hunk_old_start_idx..hunk_old_end_idx {
                let line = strip_newline(old_lines.get(i).copied().unwrap_or(""));
                lines.push((DiffLineKind::Deleted, line.to_string()));
            }
            old_pos = hunk_old_end_idx;

            // Added lines
            for i in hunk_new_start_idx..hunk_new_end_idx {
                let line = strip_newline(new_lines.get(i).copied().unwrap_or(""));
                lines.push((DiffLineKind::Added, line.to_string()));
            }
            new_pos = hunk_new_end_idx;
        }

        // Trailing context
        while old_pos < ctx_after_end_old {
            let line = strip_newline(old_lines.get(old_pos).copied().unwrap_or(""));
            lines.push((DiffLineKind::Context, line.to_string()));
            old_pos += 1;
            new_pos += 1;
        }

        // Compute actual counts from lines
        let old_count = lines
            .iter()
            .filter(|(k, _)| matches!(k, DiffLineKind::Context | DiffLineKind::Deleted))
            .count();
        let new_count = lines
            .iter()
            .filter(|(k, _)| matches!(k, DiffLineKind::Context | DiffLineKind::Added))
            .count();

        result.push(FileDiffHunk {
            old_start: hunk_old_start,
            old_count,
            new_start: hunk_new_start,
            new_count,
            lines,
        });
    }

    result
}

fn strip_newline(s: &str) -> &str {
    s.strip_suffix('\n')
        .or_else(|| s.strip_suffix("\r\n"))
        .unwrap_or(s)
}
