//! Unified diff parsing and the numbered "flat lines" view shown to agents.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashSet};

use crate::git::{Git, GitError, split_nul};

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Hunk {
    pub old_start: usize,
    pub old_lines: usize,
    pub new_start: usize,
    pub new_lines: usize,
    /// Body lines with their leading ' ', '+' or '-' marker and without the
    /// line terminator. Kept as raw bytes so CR and non-UTF-8 text survive.
    pub lines: Vec<Vec<u8>>,
    /// True if the old side lacks a trailing newline.
    pub old_missing_final_newline: bool,
    /// True if the new side lacks a trailing newline.
    pub new_missing_final_newline: bool,
}

impl Hunk {
    /// Convenience constructor for tests and callers holding text lines.
    pub fn from_text(old_start: usize, old_lines: usize, new_start: usize, new_lines: usize, lines: &[&str]) -> Self {
        Hunk {
            old_start,
            old_lines,
            new_start,
            new_lines,
            lines: lines.iter().map(|l| l.as_bytes().to_vec()).collect(),
            ..Hunk::default()
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct FileDiff {
    pub binary: bool,
    pub hunks: Vec<Hunk>,
}

/// Parsed diff keyed by path. A `BTreeMap` keeps paths sorted, which gives
/// pagination a stable order.
pub type ParsedDiff = BTreeMap<String, FileDiff>;

/// Parse `@@ -a[,b] +c[,d] @@`. Omitted counts mean 1.
pub fn parse_hunk_header(line: &[u8]) -> Option<(usize, usize, usize, usize)> {
    let rest = line.strip_prefix(b"@@ -")?;
    let (old, rest) = parse_range(rest)?;
    let rest = rest.strip_prefix(b" +")?;
    let (new, rest) = parse_range(rest)?;
    rest.starts_with(b" @@").then_some((old.0, old.1, new.0, new.1))
}

fn parse_range(s: &[u8]) -> Option<((usize, usize), &[u8])> {
    let (start, rest) = parse_number(s)?;
    match rest.strip_prefix(b",") {
        Some(rest) => {
            let (count, rest) = parse_number(rest)?;
            Some(((start, count), rest))
        }
        None => Some(((start, 1), rest)),
    }
}

fn parse_number(s: &[u8]) -> Option<(usize, &[u8])> {
    let len = s.iter().take_while(|b| b.is_ascii_digit()).count();
    if len == 0 {
        return None;
    }
    let n = std::str::from_utf8(&s[..len]).ok()?.parse().ok()?;
    Some((n, &s[len..]))
}

/// Undo git's C-style path quoting (`"a\tb"`, `"\346\227\245"`).
fn unquote_c_style(s: &str) -> String {
    let Some(inner) = s.strip_prefix('"').and_then(|s| s.strip_suffix('"')) else {
        return s.to_string();
    };
    let bytes = inner.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b != b'\\' || i + 1 >= bytes.len() {
            out.push(b);
            i += 1;
            continue;
        }
        let esc = bytes[i + 1];
        i += 2;
        let decoded = match esc {
            b'a' => 0x07,
            b'b' => 0x08,
            b't' => b'\t',
            b'n' => b'\n',
            b'v' => 0x0b,
            b'f' => 0x0c,
            b'r' => b'\r',
            b'0'..=b'7' => {
                let mut value = u32::from(esc - b'0');
                let mut digits = 1;
                while digits < 3 && i < bytes.len() && (b'0'..=b'7').contains(&bytes[i]) {
                    value = value * 8 + u32::from(bytes[i] - b'0');
                    i += 1;
                    digits += 1;
                }
                value as u8
            }
            other => other,
        };
        out.push(decoded);
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Path from a `--- ` / `+++ ` header. Git appends a tab when the name
/// contains a space, and C-quotes names with unusual characters.
fn header_path(raw: &[u8]) -> String {
    let s = String::from_utf8_lossy(raw);
    let s = s.strip_suffix('\t').unwrap_or(&s);
    unquote_c_style(s)
}

/// Path from `Binary files <a> and <b> differ`.
fn binary_line_path(line: &[u8]) -> Option<String> {
    let s = String::from_utf8_lossy(line);
    let rest = s.strip_prefix("Binary files ")?.strip_suffix(" differ")?;
    if let Some(a) = rest.strip_suffix(" and /dev/null") {
        let a = unquote_c_style(a);
        return Some(a.strip_prefix("a/").unwrap_or(&a).to_string());
    }
    for (idx, _) in rest.match_indices(" and ") {
        let b = &rest[idx + 5..];
        if b.starts_with("b/") || b.starts_with("\"b/") {
            let b = unquote_c_style(b);
            return Some(b.strip_prefix("b/").unwrap_or(&b).to_string());
        }
    }
    None
}

fn mark_missing_newline(hunk: &mut Hunk) {
    match hunk.lines.last().and_then(|l| l.first()) {
        Some(b'-') => hunk.old_missing_final_newline = true,
        Some(b'+') => hunk.new_missing_final_newline = true,
        _ => {}
    }
}

/// Parse git's unified diff output, merging the result into `files`.
///
/// Hunk bodies are consumed by the line counts in their header rather than by
/// sniffing prefixes, so content lines such as a removed `-- comment` (shown
/// as `--- comment`) are never mistaken for file headers.
pub fn parse_unified_diff_into(diff: &[u8], files: &mut ParsedDiff) {
    let mut cur_path: Option<String> = None;
    let mut a_path: Option<String> = None;
    let mut in_hunk = false;
    let mut old_left = 0usize;
    let mut new_left = 0usize;
    // Combined diffs (`diff --cc` for unmerged paths) cannot be staged by line.
    let mut skipping = false;

    let mut lines: Vec<&[u8]> = diff.split(|&b| b == b'\n').collect();
    if lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }

    for raw in lines {
        if old_left > 0 || new_left > 0 {
            let Some(hunk) = cur_path
                .as_ref()
                .and_then(|p| files.get_mut(p))
                .and_then(|f| f.hunks.last_mut())
            else {
                unreachable!("line counts are only set after pushing a hunk")
            };
            match raw.first() {
                // A blank line inside a hunk is an empty context line whose
                // leading space was dropped (diff.suppressBlankEmpty).
                None => {
                    hunk.lines.push(b" ".to_vec());
                    old_left = old_left.saturating_sub(1);
                    new_left = new_left.saturating_sub(1);
                    continue;
                }
                Some(b' ') => {
                    hunk.lines.push(raw.to_vec());
                    old_left = old_left.saturating_sub(1);
                    new_left = new_left.saturating_sub(1);
                    continue;
                }
                Some(b'-') => {
                    hunk.lines.push(raw.to_vec());
                    old_left = old_left.saturating_sub(1);
                    continue;
                }
                Some(b'+') => {
                    hunk.lines.push(raw.to_vec());
                    new_left = new_left.saturating_sub(1);
                    continue;
                }
                Some(b'\\') => {
                    mark_missing_newline(hunk);
                    continue;
                }
                // Malformed body: end the hunk and treat this as a header line.
                Some(_) => {
                    old_left = 0;
                    new_left = 0;
                }
            }
        }

        if raw.starts_with(b"\\") {
            // "\ No newline at end of file" after the last body line.
            if in_hunk
                && let Some(hunk) = cur_path
                    .as_ref()
                    .and_then(|p| files.get_mut(p))
                    .and_then(|f| f.hunks.last_mut())
            {
                mark_missing_newline(hunk);
            }
            continue;
        }
        if raw.starts_with(b"diff ") {
            cur_path = None;
            a_path = None;
            in_hunk = false;
            skipping = !raw.starts_with(b"diff --git ");
            continue;
        }
        if skipping {
            continue;
        }
        if let Some(rest) = raw.strip_prefix(b"--- ") {
            a_path = Some(header_path(rest));
            in_hunk = false;
            continue;
        }
        if let Some(rest) = raw.strip_prefix(b"+++ ") {
            let b_path = header_path(rest);
            let path = if let Some(p) = b_path.strip_prefix("b/") {
                p.to_string()
            } else if b_path == "/dev/null" {
                match a_path.as_deref().and_then(|a| a.strip_prefix("a/")) {
                    Some(a) => a.to_string(),
                    None => b_path,
                }
            } else {
                b_path
            };
            files.entry(path.clone()).or_default();
            cur_path = Some(path);
            in_hunk = false;
            continue;
        }
        if raw.starts_with(b"Binary files ") {
            if let Some(path) = binary_line_path(raw) {
                files.entry(path.clone()).or_default().binary = true;
                cur_path = Some(path);
                in_hunk = false;
            }
            continue;
        }
        if let Some((old_start, old_lines, new_start, new_lines)) = parse_hunk_header(raw) {
            if let Some(file) = cur_path.as_ref().and_then(|p| files.get_mut(p)) {
                file.hunks.push(Hunk {
                    old_start,
                    old_lines,
                    new_start,
                    new_lines,
                    ..Hunk::default()
                });
                in_hunk = true;
                old_left = old_lines;
                new_left = new_lines;
            }
            continue;
        }
        // Everything else (index, mode, rename, similarity lines) is ignored.
    }
}

pub fn parse_unified_diff(diff: &[u8]) -> ParsedDiff {
    let mut files = ParsedDiff::new();
    parse_unified_diff_into(diff, &mut files);
    files
}

/// Options that keep git's output parseable regardless of user config.
const DIFF_OPTS: [&str; 5] = [
    "--no-color",
    "--no-ext-diff",
    "--no-textconv",
    "--src-prefix=a/",
    "--dst-prefix=b/",
];

/// The working tree diff, including untracked files.
pub struct WorktreeDiff {
    pub files: ParsedDiff,
    pub untracked: HashSet<String>,
    pub deleted: HashSet<String>,
}

impl WorktreeDiff {
    pub fn status(&self, path: &str) -> &'static str {
        if self.untracked.contains(path) {
            "added"
        } else if self.deleted.contains(path) {
            "deleted"
        } else {
            "modified"
        }
    }
}

fn with_pathspec(mut args: Vec<String>, paths: &[String]) -> Vec<String> {
    if !paths.is_empty() {
        args.push("--".into());
        args.extend(paths.iter().cloned());
    }
    args
}

pub fn collect_worktree_diff(paths: &[String], unified: i64) -> Result<WorktreeDiff, GitError> {
    let unified_opt = format!("--unified={unified}");

    let mut args: Vec<String> = vec![
        "diff".into(),
        "--patch".into(),
        unified_opt.clone(),
        "--find-renames=50%".into(),
    ];
    args.extend(DIFF_OPTS.iter().map(|s| s.to_string()));
    let tracked = Git::new(with_pathspec(args, paths)).run()?;
    let mut files = parse_unified_diff(&tracked);

    let ls = |mode: &str| -> Result<Vec<String>, GitError> {
        let mut args = vec!["ls-files".to_string(), "-z".into(), mode.into()];
        if mode == "--others" {
            args.push("--exclude-standard".into());
        }
        Ok(split_nul(&Git::new(with_pathspec(args, paths)).output()?.stdout))
    };
    let untracked_files = ls("--others")?;
    let deleted: HashSet<String> = ls("--deleted")?.into_iter().collect();

    for file in &untracked_files {
        let mut args: Vec<String> = vec!["diff".into(), "--no-index".into(), unified_opt.clone()];
        args.extend(DIFF_OPTS.iter().map(|s| s.to_string()));
        args.extend(["--".into(), "/dev/null".into(), file.clone()]);
        // --no-index exits 1 when the files differ, so ignore the status.
        let out = Git::new(args).output()?;
        parse_unified_diff_into(&out.stdout, &mut files);
    }

    Ok(WorktreeDiff {
        files,
        untracked: untracked_files.into_iter().collect(),
        deleted,
    })
}

/// Text of a body line (without its marker) for display. A CR from a CRLF
/// file is hidden here but kept in the hunk for applying.
pub fn display_text(bytes: &[u8]) -> Cow<'_, str> {
    let bytes = bytes.strip_suffix(b"\r").unwrap_or(bytes);
    String::from_utf8_lossy(bytes)
}

fn sorted_hunks(hunks: &[Hunk]) -> Vec<&Hunk> {
    let mut sorted: Vec<&Hunk> = hunks.iter().collect();
    sorted.sort_by_key(|h| (h.old_start, h.new_start));
    sorted
}

/// Render hunks as numbered lines: `0001: + text` for changes, eight spaces
/// for context and `        ...` between hunks.
pub fn flat_file_lines_with_numbers(hunks: &[Hunk]) -> Vec<String> {
    let mut out = Vec::new();
    let mut n = 1usize;
    let sorted = sorted_hunks(hunks);
    for (i, h) in sorted.iter().enumerate() {
        if i > 0 {
            out.push("        ...".to_string());
        }
        for ln in &h.lines {
            let Some((&sign, text)) = ln.split_first() else {
                continue;
            };
            match sign {
                b'+' | b'-' => {
                    out.push(format!("{n:04}: {} {}", sign as char, display_text(text)));
                    n += 1;
                }
                b' ' => out.push(format!("        {}", display_text(text))),
                _ => {}
            }
        }
    }
    // A selectable pseudo-line standing for the new file's trailing newline.
    if let Some(last) = sorted.last()
        && !last.new_missing_final_newline
        && last.lines.iter().any(|l| l.first() == Some(&b'+'))
    {
        out.push(format!("{n:04}: + "));
    }
    out
}

/// (additions, deletions)
pub fn line_stats(hunks: &[Hunk]) -> (usize, usize) {
    let mut additions = 0;
    let mut deletions = 0;
    for ln in hunks.iter().flat_map(|h| &h.lines) {
        match ln.first() {
            Some(b'+') => additions += 1,
            Some(b'-') => deletions += 1,
            _ => {}
        }
    }
    (additions, deletions)
}

/// Byte size of the diff body, used to decide truncation.
pub fn diff_size(hunks: &[Hunk]) -> usize {
    hunks
        .iter()
        .flat_map(|h| &h.lines)
        .map(|l| l.strip_suffix(b"\r").unwrap_or(l).len())
        .sum()
}

/// Number of selectable (+/-) lines.
pub fn count_changes(hunks: &[Hunk]) -> usize {
    let (additions, deletions) = line_stats(hunks);
    additions + deletions
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hunk_header_counts() {
        assert_eq!(parse_hunk_header(b"@@ -10,5 +20,8 @@"), Some((10, 5, 20, 8)));
        assert_eq!(parse_hunk_header(b"@@ -10 +20,8 @@"), Some((10, 1, 20, 8)));
        assert_eq!(parse_hunk_header(b"@@ -10,5 +20 @@"), Some((10, 5, 20, 1)));
        assert_eq!(parse_hunk_header(b"@@ -10 +20 @@ fn main()"), Some((10, 1, 20, 1)));
        assert_eq!(parse_hunk_header(b"@@ -0,0 +1,3 @@"), Some((0, 0, 1, 3)));
        assert_eq!(parse_hunk_header(b"@@@ -1,2 -1,2 +1,3 @@@"), None);
    }

    #[test]
    fn content_that_looks_like_headers_stays_in_the_hunk() {
        let diff = b"diff --git a/q.sql b/q.sql\n\
            --- a/q.sql\n\
            +++ b/q.sql\n\
            @@ -1,2 +1,2 @@\n\
            --- old comment\n\
            +++ new comment\n\
            \x20select 1;\n";
        let files = parse_unified_diff(diff);
        assert_eq!(files.keys().collect::<Vec<_>>(), ["q.sql"]);
        let lines = &files["q.sql"].hunks[0].lines;
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], b"--- old comment");
        assert_eq!(lines[1], b"+++ new comment");
    }

    #[test]
    fn missing_newline_markers() {
        let diff = b"diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1,2 +1,2 @@\n a\n-b\n\\ No newline at end of file\n+c\n\\ No newline at end of file\n";
        let files = parse_unified_diff(diff);
        let h = &files["f"].hunks[0];
        assert!(h.old_missing_final_newline);
        assert!(h.new_missing_final_newline);
        assert_eq!(h.lines.len(), 3);
    }

    #[test]
    fn crlf_is_kept_but_not_displayed() {
        let diff = b"diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1 +1 @@\n-a\r\n+b\r\n";
        let files = parse_unified_diff(diff);
        assert_eq!(files["f"].hunks[0].lines[1], b"+b\r");
        assert_eq!(
            flat_file_lines_with_numbers(&files["f"].hunks),
            ["0001: - a", "0002: + b", "0003: + "]
        );
    }

    #[test]
    fn quoted_and_spaced_paths() {
        let diff = "diff --git a/a b.txt b/a b.txt\n--- a/a b.txt\t\n+++ b/a b.txt\t\n@@ -1 +1 @@\n-x\n+y\n\
            diff --git \"a/t\\tab\" \"b/t\\tab\"\n--- \"a/t\\tab\"\n+++ \"b/t\\tab\"\n@@ -1 +1 @@\n-x\n+y\n\
            diff --git \"a/\\346\\227\\245.txt\" \"b/\\346\\227\\245.txt\"\n--- \"a/\\346\\227\\245.txt\"\n+++ \"b/\\346\\227\\245.txt\"\n@@ -1 +1 @@\n-x\n+y\n";
        let files = parse_unified_diff(diff.as_bytes());
        let keys: Vec<_> = files.keys().cloned().collect();
        assert_eq!(keys, ["a b.txt", "t\tab", "日.txt"]);
    }

    #[test]
    fn binary_files() {
        let diff = b"diff --git a/new.bin b/new.bin\nBinary files /dev/null and b/new.bin differ\n\
            diff --git a/gone.bin b/gone.bin\nBinary files a/gone.bin and /dev/null differ\n";
        let files = parse_unified_diff(diff);
        assert!(files["new.bin"].binary);
        assert!(files["gone.bin"].binary);
    }

    #[test]
    fn deleted_file_uses_old_path() {
        let diff =
            b"diff --git a/d.txt b/d.txt\ndeleted file mode 100644\n--- a/d.txt\n+++ /dev/null\n@@ -1 +0,0 @@\n-gone\n";
        let files = parse_unified_diff(diff);
        assert_eq!(flat_file_lines_with_numbers(&files["d.txt"].hunks), ["0001: - gone"]);
    }

    #[test]
    fn combined_diffs_are_skipped() {
        let diff = b"diff --cc c.txt\nindex 1,2..3\n--- a/c.txt\n+++ b/c.txt\n@@@ -1,1 -1,1 +1,5 @@@\n++<<<<<<< ours\n";
        assert!(parse_unified_diff(diff).is_empty());
    }

    #[test]
    fn flat_lines_between_hunks() {
        let hunks = vec![
            Hunk::from_text(10, 1, 10, 2, &[" ctx", "+add"]),
            Hunk::from_text(1, 1, 1, 1, &["-old", "+new"]),
        ];
        assert_eq!(
            flat_file_lines_with_numbers(&hunks),
            [
                "0001: - old",
                "0002: + new",
                "        ...",
                "        ctx",
                "0003: + add",
                "0004: + "
            ]
        );
    }
}
