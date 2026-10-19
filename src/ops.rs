//! The operations behind each tool: listing, single-file diff, partial
//! staging and the auto-commit summary.

use std::collections::BTreeSet;

use base64::Engine;
use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
use serde::Serialize;

use crate::diff::{Hunk, collect_worktree_diff, count_changes, diff_size, flat_file_lines_with_numbers, line_stats};
use crate::git::{Git, GitError};

pub const UNIFIED_LIST_DEFAULT: i64 = 20;
pub const UNIFIED_APPLY: i64 = 3;
pub const PAGE_SIZE_FILES_DEFAULT: i64 = 50;
pub const PAGE_SIZE_BYTES_DEFAULT: i64 = 30 * 1024;
/// Diffs larger than this are truncated in `list_changes` to protect LLM context.
pub const MAX_DIFF_BYTES: usize = 10 * 1024;

// ---------- list ----------

#[derive(Debug, Serialize)]
pub struct ListResult {
    pub page_token_next: Option<String>,
    pub files: Vec<ListedFile>,
    pub stats: ListStats,
}

#[derive(Debug, Serialize)]
pub struct ListedFile {
    pub path: String,
    pub binary: bool,
    pub status: &'static str,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub lines: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct ListStats {
    pub files: usize,
    pub lines: usize,
    pub truncated_files: usize,
    pub page_bytes: usize,
}

const TOKEN_ENGINE: GeneralPurpose = GeneralPurpose::new(
    &base64::alphabet::URL_SAFE,
    GeneralPurposeConfig::new()
        .with_encode_padding(false)
        .with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

/// Page tokens are url-safe base64 of `{"file_index": N}`, byte-compatible
/// with the Python implementation.
fn encode_page_token(file_index: usize) -> String {
    TOKEN_ENGINE.encode(format!("{{\"file_index\": {file_index}}}"))
}

/// An invalid token restarts from the first file.
fn decode_page_token(token: &str) -> usize {
    let Ok(bytes) = TOKEN_ENGINE.decode(token.trim()) else {
        return 0;
    };
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return 0;
    };
    match value.get("file_index") {
        Some(v) => v
            .as_u64()
            .or_else(|| v.as_f64().filter(|f| *f >= 0.0).map(|f| f as u64))
            .unwrap_or(0) as usize,
        None => 0,
    }
}

fn utf8_len(lines: &[String]) -> usize {
    lines.iter().map(String::len).sum()
}

/// List changed files with numbered lines.
///
/// Pagination stops when the cumulative output size reaches
/// `page_size_bytes` (always including at least one file) or the file count
/// reaches `page_size_files`.
pub fn list_files(
    paths: &[String],
    page_token: Option<&str>,
    page_size_files: i64,
    page_size_bytes: i64,
    unified: i64,
) -> Result<ListResult, GitError> {
    let wt = collect_worktree_diff(paths, unified)?;
    let all_paths: Vec<&String> = wt.files.keys().collect();
    let page_size_files = page_size_files.max(1) as usize;

    let mut i = page_token.map(decode_page_token).unwrap_or(0);
    let mut out_files: Vec<ListedFile> = Vec::new();
    let mut truncated_count = 0;
    let mut cumulative_bytes: usize = 0;

    while i < all_paths.len() {
        if out_files.len() >= page_size_files {
            break;
        }
        if cumulative_bytes > 0 && cumulative_bytes as i64 >= page_size_bytes {
            break;
        }
        let path = all_paths[i];
        let file = &wt.files[path];
        let status = wt.status(path);
        i += 1;

        if file.binary {
            // Binary and truncated files don't count toward the page size.
            out_files.push(ListedFile {
                path: path.clone(),
                binary: true,
                status,
                truncated: false,
                reason: None,
                lines: Vec::new(),
            });
            continue;
        }

        let size = diff_size(&file.hunks);
        if size > MAX_DIFF_BYTES {
            out_files.push(ListedFile {
                path: path.clone(),
                binary: false,
                status,
                truncated: true,
                reason: Some(format!(
                    "diff too large ({:.1} KB, max {} KB)",
                    size as f64 / 1024.0,
                    MAX_DIFF_BYTES / 1024
                )),
                lines: Vec::new(),
            });
            truncated_count += 1;
            continue;
        }

        let lines = flat_file_lines_with_numbers(&file.hunks);
        cumulative_bytes += utf8_len(&lines);
        out_files.push(ListedFile {
            path: path.clone(),
            binary: false,
            status,
            truncated: false,
            reason: None,
            lines,
        });
    }

    let page_token_next = (i < all_paths.len()).then(|| encode_page_token(i));
    let total_lines = out_files.iter().filter(|f| !f.binary).map(|f| f.lines.len()).sum();
    Ok(ListResult {
        page_token_next,
        stats: ListStats {
            files: out_files.len(),
            lines: total_lines,
            truncated_files: truncated_count,
            page_bytes: cumulative_bytes,
        },
        files: out_files,
    })
}

// ---------- diff (single file, never truncated) ----------

#[derive(Debug, Serialize)]
pub struct DiffResult {
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub binary: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lines: Option<Vec<String>>,
    pub size_bytes: usize,
}

pub fn diff_file(path: &str, unified: i64) -> Result<DiffResult, GitError> {
    let wt = collect_worktree_diff(&[path.to_string()], unified)?;
    let Some(file) = wt.files.get(path) else {
        return Ok(DiffResult {
            path: path.to_string(),
            error: Some("No changes found for this file".into()),
            binary: None,
            status: None,
            lines: None,
            size_bytes: 0,
        });
    };
    let lines = if file.binary {
        Vec::new()
    } else {
        flat_file_lines_with_numbers(&file.hunks)
    };
    Ok(DiffResult {
        path: path.to_string(),
        error: None,
        binary: Some(file.binary),
        status: Some(wt.status(path)),
        size_bytes: utf8_len(&lines),
        lines: Some(lines),
    })
}

// ---------- apply (partial staging for one file) ----------

#[derive(Debug, Serialize)]
pub struct ApplyResult {
    pub applied: Vec<Applied>,
    pub skipped: Vec<Skipped>,
    pub stats: ApplyStats,
}

#[derive(Debug, Serialize)]
pub struct Applied {
    pub file: String,
    pub applied_count: usize,
    pub after_applying: AfterApplying,
}

#[derive(Debug, Serialize)]
pub struct AfterApplying {
    pub diff: Vec<String>,
    pub unstaged_lines: usize,
}

#[derive(Debug, Serialize)]
pub struct Skipped {
    pub file: String,
    pub number: usize,
    pub reason: &'static str,
}

#[derive(Debug, Serialize)]
pub struct ApplyStats {
    pub files: usize,
    pub changes_applied: usize,
    pub changes_skipped: usize,
}

/// Parse `NNNN,MMMM,PPPP-QQQQ` into change numbers.
pub fn parse_number_tokens(spec: &str) -> Result<Vec<usize>, String> {
    fn number(s: &str) -> Option<usize> {
        (s.len() >= 4 && s.bytes().all(|b| b.is_ascii_digit()))
            .then(|| s.parse().ok())
            .flatten()
    }
    let mut nums = Vec::new();
    for tok in spec.split(',') {
        let t = tok.trim();
        if t.is_empty() {
            continue;
        }
        if let Some(n) = number(t) {
            nums.push(n);
            continue;
        }
        if let Some((a, b)) = t.split_once('-')
            && let (Some(a), Some(b)) = (number(a), number(b))
        {
            if a > b {
                return Err(format!("invalid range: {t}"));
            }
            if b - a > 1_000_000 {
                return Err(format!("range too large: {t}"));
            }
            nums.extend(a..=b);
            continue;
        }
        return Err(format!("invalid token: {t}"));
    }
    Ok(nums)
}

/// Apply the selected changes (by sequential change number) to the old file
/// content. Fails if the hunks don't match the content (drift).
pub fn apply_selected_changes_to_old(
    old_lines: &[Vec<u8>],
    hunks: &[Hunk],
    want: &BTreeSet<usize>,
) -> Result<Vec<Vec<u8>>, String> {
    let show = |b: &[u8]| format!("{:?}", String::from_utf8_lossy(b));
    let mut new: Vec<Vec<u8>> = Vec::with_capacity(old_lines.len());
    let mut old_pos = 1usize; // 1-origin
    let mut num = 1usize;

    let mut sorted: Vec<&Hunk> = hunks.iter().collect();
    sorted.sort_by_key(|h| (h.old_start, h.new_start));

    for h in sorted {
        let pre_start = h.old_start.max(1);
        if pre_start - 1 > old_lines.len() + 1 {
            return Err(format!(
                "Hunk old_start={pre_start} is out of bounds (file has {} lines)",
                old_lines.len()
            ));
        }
        while old_pos < pre_start && old_pos - 1 < old_lines.len() {
            new.push(old_lines[old_pos - 1].clone());
            old_pos += 1;
        }

        for ln in &h.lines {
            let Some((&sign, text)) = ln.split_first() else {
                continue;
            };
            match sign {
                b' ' => {
                    let Some(old) = old_lines.get(old_pos - 1) else {
                        return Err(format!(
                            "Context line at position {old_pos} is beyond file end (file has {} lines)",
                            old_lines.len()
                        ));
                    };
                    if old.as_slice() != text {
                        return Err(format!(
                            "Context mismatch at line {old_pos}:\n  Expected: {}\n  Got:      {}",
                            show(text),
                            show(old)
                        ));
                    }
                    new.push(old.clone());
                    old_pos += 1;
                }
                b'-' => {
                    let Some(old) = old_lines.get(old_pos - 1) else {
                        return Err(format!(
                            "Deletion at position {old_pos} is beyond file end (file has {} lines)",
                            old_lines.len()
                        ));
                    };
                    if old.as_slice() != text {
                        return Err(format!(
                            "Deletion mismatch at line {old_pos}:\n  Expected to delete: {}\n  Found:              {}",
                            show(text),
                            show(old)
                        ));
                    }
                    if !want.contains(&num) {
                        new.push(old.clone()); // keep if not selected
                    }
                    old_pos += 1;
                    num += 1;
                }
                b'+' => {
                    if want.contains(&num) {
                        new.push(text.to_vec());
                    }
                    num += 1;
                }
                other => {
                    return Err(format!(
                        "Unexpected diff line marker '{}' at line {old_pos} (expected ' ', '+', or '-')",
                        other as char
                    ));
                }
            }
        }
    }
    new.extend(old_lines.iter().skip(old_pos - 1).cloned());
    Ok(new)
}

/// Whether the staged content should end with a newline.
///
/// Selecting the trailing pseudo-line (number one past the last change)
/// opts in; otherwise explicit "\ No newline" markers decide, falling back
/// to the old content's state.
pub fn determine_new_trailing_newline(hunks: &[Hunk], want: &BTreeSet<usize>, default: bool) -> bool {
    let max_selected = want.last().copied().unwrap_or(0);
    let trailing_newline_selected = max_selected > count_changes(hunks);
    for hunk in hunks {
        if hunk.new_missing_final_newline {
            return trailing_newline_selected;
        }
        if hunk.old_missing_final_newline {
            return trailing_newline_selected;
        }
    }
    trailing_newline_selected || default
}

/// (content lines, had trailing newline) of the staged version of `path`.
fn read_index_lines(path: &str) -> Result<(Vec<Vec<u8>>, bool), GitError> {
    let out = Git::new(["cat-file", "blob", &format!(":{path}")]).output()?;
    if !out.success() {
        return Ok((Vec::new(), false)); // not in the index yet
    }
    let content = out.stdout;
    let had_trailing_nl = content.ends_with(b"\n");
    let mut lines: Vec<Vec<u8>> = content.split(|&b| b == b'\n').map(<[u8]>::to_vec).collect();
    if had_trailing_nl || content.is_empty() {
        lines.pop();
    }
    Ok((lines, had_trailing_nl))
}

fn detect_mode(path: &str) -> Result<String, GitError> {
    let out = Git::new(["ls-files", "-s", "--", path]).output()?;
    if let Some(mode) = out.stdout_string().split_whitespace().next() {
        return Ok(mode.to_string());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(path)
            && meta.permissions().mode() & 0o100 != 0
        {
            return Ok("100755".into());
        }
    }
    Ok("100644".into())
}

fn update_index_with_content(path: &str, mode: &str, content: &[u8]) -> Result<(), GitError> {
    // --stdin hashes the bytes as-is (no filters); the content is already in
    // index form because both sides came from the index and `git diff`.
    let oid = Git::new(["hash-object", "-w", "--stdin"]).stdin(content).run_string()?;
    Git::new(["update-index", "--add", "--cacheinfo", mode, &oid, path]).run()?;
    Ok(())
}

fn skipped_all(path: &str, want: &BTreeSet<usize>, reason: &'static str) -> ApplyResult {
    ApplyResult {
        applied: Vec::new(),
        skipped: want
            .iter()
            .map(|&number| Skipped {
                file: path.to_string(),
                number,
                reason,
            })
            .collect(),
        stats: ApplyStats {
            files: 0,
            changes_applied: 0,
            changes_skipped: want.len(),
        },
    }
}

/// Stage the selected changes of one file into the index.
pub fn apply_one_file(path: &str, want_numbers: &[usize]) -> Result<ApplyResult, GitError> {
    let want: BTreeSet<usize> = want_numbers.iter().copied().collect();
    let wt = collect_worktree_diff(&[path.to_string()], UNIFIED_APPLY)?;
    let file = wt.files.get(path);
    if file.is_some_and(|f| f.binary) {
        return Ok(skipped_all(path, &want, "binary"));
    }
    let hunks = match file {
        Some(f) if !f.hunks.is_empty() => &f.hunks,
        _ => return Ok(skipped_all(path, &want, "drift")),
    };

    let (old_lines, had_trailing_nl) = read_index_lines(path)?;
    let Ok(new_lines) = apply_selected_changes_to_old(&old_lines, hunks, &want) else {
        return Ok(skipped_all(path, &want, "drift"));
    };

    let mode = detect_mode(path)?;
    let mut content = new_lines.join(&b'\n');
    if determine_new_trailing_newline(hunks, &want, had_trailing_nl) {
        content.push(b'\n');
    }
    update_index_with_content(path, &mode, &content)?;

    let remaining = collect_worktree_diff(&[path.to_string()], UNIFIED_APPLY)?;
    let diff = match remaining.files.get(path) {
        Some(f) if !f.binary => flat_file_lines_with_numbers(&f.hunks),
        _ => Vec::new(),
    };
    let unstaged_lines = diff
        .iter()
        .filter(|l| l.len() >= 4 && l.as_bytes()[..4].iter().all(u8::is_ascii_digit))
        .count();
    Ok(ApplyResult {
        applied: vec![Applied {
            file: path.to_string(),
            applied_count: want.len(),
            after_applying: AfterApplying { diff, unstaged_lines },
        }],
        skipped: Vec::new(),
        stats: ApplyStats {
            files: 1,
            changes_applied: want.len(),
            changes_skipped: 0,
        },
    })
}

// ---------- auto_commit ----------

#[derive(Debug, Serialize)]
pub struct AutoCommitReport {
    pub recent_commits: Vec<String>,
    pub summary: AutoCommitSummary,
    pub files: Vec<FileStat>,
    pub tool_schemas: serde_json::Map<String, serde_json::Value>,
    pub next: &'static str,
}

#[derive(Debug, Serialize)]
pub struct AutoCommitSummary {
    pub total_files: usize,
    pub total_additions: usize,
    pub total_deletions: usize,
    pub total_changes: usize,
}

#[derive(Debug, Serialize)]
pub struct FileStat {
    pub path: String,
    pub binary: bool,
    pub status: &'static str,
    pub additions: usize,
    pub deletions: usize,
    pub changes: usize,
}

pub const AUTO_COMMIT_NEXT: &str = "Now follow these steps to create focused commits:\n\n\
1. **Review file statistics**: Above you see all changed files with line counts \
(additions/deletions). Use list_changes or diff tools to view detailed changes for specific files.\n\n\
2. **Analyze the changes**: Identify logical groups that should be committed together \
(e.g., related bug fixes, new features, refactoring).\n\n\
3. **Plan commits**: Decide how many commits you need and what each should contain. \
Use the recent commit messages above as a style reference.\n\n\
4. **Use TodoWrite**: If you can manage todos, create a todo item for each planned commit \
with its intended commit message. Mark them as pending.\n\n\
5. **Stage changes**: For each commit:\n   \
- Mark the todo as in_progress\n   \
- Use list_changes to view the numbered changes for relevant files\n   \
- Use apply_changes to stage the relevant line numbers\n   \
- Run `git commit -m \"your message\"` to create the commit\n   \
- Mark the todo as completed\n\n\
6. **Verify**: After all commits, run `git log` to verify and `git status` to ensure \
no changes were missed.\n\n\
Remember: Create focused, atomic commits. Each commit should represent one logical change.";

/// Recent commit subjects plus per-file statistics (no diff content, to
/// keep the response small).
pub fn auto_commit_report(
    tool_schemas: serde_json::Map<String, serde_json::Value>,
) -> Result<AutoCommitReport, GitError> {
    let recent_commits = match Git::new(["log", "--no-merges", "--pretty=format:%s", "-5"]).run() {
        Ok(out) => String::from_utf8_lossy(&out)
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(String::from)
            .collect(),
        Err(e) => vec![format!("Error getting commits: {e}")],
    };

    let wt = collect_worktree_diff(&[], UNIFIED_LIST_DEFAULT)?;
    let mut files = Vec::new();
    let (mut total_additions, mut total_deletions) = (0, 0);
    for (path, file) in &wt.files {
        let (additions, deletions) = if file.binary { (0, 0) } else { line_stats(&file.hunks) };
        total_additions += additions;
        total_deletions += deletions;
        files.push(FileStat {
            path: path.clone(),
            binary: file.binary,
            status: wt.status(path),
            additions,
            deletions,
            changes: additions + deletions,
        });
    }

    Ok(AutoCommitReport {
        recent_commits,
        summary: AutoCommitSummary {
            total_files: files.len(),
            total_additions,
            total_deletions,
            total_changes: total_additions + total_deletions,
        },
        files,
        tool_schemas,
        next: AUTO_COMMIT_NEXT,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(nums: &[usize]) -> BTreeSet<usize> {
        nums.iter().copied().collect()
    }

    fn lines(text: &[&str]) -> Vec<Vec<u8>> {
        text.iter().map(|l| l.as_bytes().to_vec()).collect()
    }

    #[test]
    fn number_tokens() {
        assert_eq!(parse_number_tokens("0001,0003-0005, 0010").unwrap(), [1, 3, 4, 5, 10]);
        assert_eq!(parse_number_tokens("10000").unwrap(), [10000]);
        assert_eq!(parse_number_tokens("12").unwrap_err(), "invalid token: 12");
        assert_eq!(
            parse_number_tokens("0005-0003").unwrap_err(),
            "invalid range: 0005-0003"
        );
        assert!(parse_number_tokens("").unwrap().is_empty());
    }

    #[test]
    fn page_token_roundtrip_matches_python() {
        assert_eq!(encode_page_token(1), "eyJmaWxlX2luZGV4IjogMX0");
        assert_eq!(decode_page_token("eyJmaWxlX2luZGV4IjogMX0"), 1);
        assert_eq!(decode_page_token("eyJmaWxlX2luZGV4IjogMX0="), 1);
        assert_eq!(decode_page_token("garbage!"), 0);
    }

    #[test]
    fn apply_addition() {
        let hunks = [Hunk::from_text(2, 1, 2, 2, &[" line 2", "+new line", " line 3"])];
        let old = lines(&["line 1", "line 2", "line 3"]);
        let new = apply_selected_changes_to_old(&old, &hunks, &set(&[1])).unwrap();
        assert_eq!(new, lines(&["line 1", "line 2", "new line", "line 3"]));
    }

    #[test]
    fn apply_deletion() {
        let hunks = [Hunk::from_text(1, 3, 1, 2, &[" line 1", "-line 2", " line 3"])];
        let old = lines(&["line 1", "line 2", "line 3"]);
        let new = apply_selected_changes_to_old(&old, &hunks, &set(&[1])).unwrap();
        assert_eq!(new, lines(&["line 1", "line 3"]));
    }

    #[test]
    fn unselected_changes_are_skipped() {
        let hunks = [Hunk::from_text(2, 1, 2, 2, &[" line 2", "+new line", " line 3"])];
        let old = lines(&["line 1", "line 2", "line 3"]);
        assert_eq!(apply_selected_changes_to_old(&old, &hunks, &set(&[])).unwrap(), old);
    }

    #[test]
    fn context_mismatch_is_reported() {
        let hunks = [Hunk::from_text(2, 1, 2, 1, &[" wrong context", "+new line"])];
        let old = lines(&["line 1", "line 2", "line 3"]);
        let err = apply_selected_changes_to_old(&old, &hunks, &set(&[1])).unwrap_err();
        assert!(
            err.contains("Context mismatch") && err.contains("Expected:") && err.contains("Got:"),
            "{err}"
        );
    }

    #[test]
    fn deletion_mismatch_is_reported() {
        let hunks = [Hunk::from_text(1, 2, 1, 1, &[" line 1", "-wrong line"])];
        let old = lines(&["line 1", "line 2", "line 3"]);
        let err = apply_selected_changes_to_old(&old, &hunks, &set(&[1])).unwrap_err();
        assert!(
            err.contains("Deletion mismatch") && err.contains("Expected to delete:") && err.contains("Found:"),
            "{err}"
        );
    }

    #[test]
    fn empty_body_lines_are_ignored() {
        let hunks = [Hunk::from_text(1, 2, 1, 3, &[" line 1", "", "+new line", " line 2"])];
        let old = lines(&["line 1", "line 2"]);
        let new = apply_selected_changes_to_old(&old, &hunks, &set(&[1])).unwrap();
        assert_eq!(new, lines(&["line 1", "new line", "line 2"]));
    }

    #[test]
    fn crlf_lines_are_preserved() {
        let hunks = [Hunk::from_text(1, 2, 1, 2, &[" a\r", "-b\r", "+B\r"])];
        let old = lines(&["a\r", "b\r"]);
        let new = apply_selected_changes_to_old(&old, &hunks, &set(&[1, 2])).unwrap();
        assert_eq!(new, lines(&["a\r", "B\r"]));
    }

    #[test]
    fn trailing_newline_decision() {
        let plain = [Hunk::from_text(1, 1, 1, 2, &[" a", "+b"])];
        // pseudo-line 0002 opts in; otherwise the old state is kept
        assert!(determine_new_trailing_newline(&plain, &set(&[1, 2]), false));
        assert!(!determine_new_trailing_newline(&plain, &set(&[1]), false));
        assert!(determine_new_trailing_newline(&plain, &set(&[1]), true));

        let mut no_eol = Hunk::from_text(1, 1, 1, 1, &["-a", "+b"]);
        no_eol.new_missing_final_newline = true;
        assert!(!determine_new_trailing_newline(&[no_eol], &set(&[1, 2]), true));
    }
}
