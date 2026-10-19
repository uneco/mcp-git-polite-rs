mod common;

use common::{Repo, number_of, numbered, repo_with_changes};
use serde_json::{Value, json};

fn changes(repo: &Repo, path: &str) -> Vec<String> {
    let result = repo.mcp().call("list_changes", json!({"paths": [path], "unified": 3}));
    numbered(&result["files"][0]["lines"])
}

fn apply(repo: &Repo, path: &str, lines: &str) -> Value {
    repo.mcp().call("apply_changes", json!({"path": path, "lines": lines}))
}

fn apply_all(repo: &Repo, path: &str) -> Value {
    let all = changes(repo, path);
    let spec = format!("{}-{}", &all[0][..4], &all[all.len() - 1][..4]);
    apply(repo, path, &spec)
}

fn staged_names(repo: &Repo) -> String {
    repo.git(&["diff", "--cached", "--name-only"])
}

// ---------- scenario 001: with changes ----------

#[test]
fn single_line_of_modified_file() {
    let repo = repo_with_changes();
    let first = number_of(&changes(&repo, "README.md")[0]);
    let result = apply(&repo, "README.md", &format!("{first:04}"));
    assert_eq!(result["applied"][0]["file"], "README.md");
    assert_eq!(result["applied"][0]["applied_count"], 1);
    assert_eq!(result["skipped"], json!([]));
    assert!(staged_names(&repo).contains("README.md"));
}

#[test]
fn part_of_untracked_file() {
    let repo = repo_with_changes();
    let first = number_of(&changes(&repo, "new_file.txt")[0]);
    let result = apply(&repo, "new_file.txt", &format!("{first:04}"));
    assert_eq!(result["applied"][0]["file"], "new_file.txt");
    assert!(staged_names(&repo).contains("new_file.txt"));
}

#[test]
fn range_of_lines() {
    let repo = repo_with_changes();
    let result = apply(&repo, "README.md", "0001-0002");
    assert_eq!(result["applied"][0]["applied_count"], 2);
    assert_eq!(
        repo.staged("README.md"),
        b"# Test Repository\nOriginal content\nModified line\n"
    );
    assert_eq!(repo.git(&["diff", "README.md"]), "", "nothing left unstaged");
}

#[test]
fn result_structure() {
    let repo = repo_with_changes();
    let result = apply(&repo, "README.md", "0001");
    assert_eq!(
        result["stats"],
        json!({"files": 1, "changes_applied": 1, "changes_skipped": 0})
    );
    let after = &result["applied"][0]["after_applying"];
    assert!(after["diff"].is_array());
    assert_eq!(
        after["unstaged_lines"].as_u64().unwrap() as usize,
        numbered(&after["diff"]).len()
    );
}

#[test]
fn file_without_changes_is_skipped_as_drift() {
    let repo = repo_with_changes();
    let result = apply(&repo, "unchanged.txt", "0001,0002");
    assert_eq!(result["applied"], json!([]));
    assert_eq!(
        result["skipped"][0],
        json!({"file": "unchanged.txt", "number": 1, "reason": "drift"})
    );
    assert_eq!(result["stats"]["changes_skipped"], 2);
}

#[test]
fn invalid_numbers_are_rejected() {
    let repo = repo_with_changes();
    assert_eq!(apply(&repo, "README.md", "12"), json!({"error": "invalid token: 12"}));
    assert_eq!(
        apply(&repo, "README.md", "0003-0001"),
        json!({"error": "invalid range: 0003-0001"})
    );
    assert_eq!(staged_names(&repo), "");
}

#[test]
fn binary_file_is_skipped() {
    let repo = Repo::new();
    repo.write("b.bin", [0u8, 1]);
    repo.commit_all("init");
    repo.write("b.bin", [0u8, 2]);
    let result = apply(&repo, "b.bin", "0001");
    assert_eq!(result["skipped"][0]["reason"], "binary");
}

// ---------- scenario 004: trailing newline handling ----------

fn newline_repo() -> Repo {
    let repo = Repo::new();
    repo.write("no_newline.txt", "line 1\nline 2\nline 3");
    repo.write("with_newline.txt", "line 1\nline 2\nline 3\n");
    repo.write("remove_newline.txt", "line 1\nline 2\n");
    repo.write("add_newline.txt", "line 1\nline 2");
    repo.commit_all("init");
    repo.write("no_newline.txt", "line 1\nmodified line 2\nline 3\nadded line 4");
    repo.write("with_newline.txt", "line 1\nmodified line 2\nline 3\nadded line 4\n");
    repo.write("remove_newline.txt", "line 1\nline 2");
    repo.write("add_newline.txt", "line 1\nline 2\n");
    repo
}

#[test]
fn file_without_trailing_newline_stays_without() {
    let repo = newline_repo();
    apply(&repo, "no_newline.txt", "0001");
    let staged = repo.staged("no_newline.txt");
    assert!(!staged.ends_with(b"\n"), "{:?}", String::from_utf8_lossy(&staged));
    assert_eq!(staged, b"line 1\nline 3");
}

#[test]
fn file_with_trailing_newline_keeps_it() {
    let repo = newline_repo();
    apply(&repo, "with_newline.txt", "0001");
    assert_eq!(repo.staged("with_newline.txt"), b"line 1\nline 3\n");
}

#[test]
fn removing_the_trailing_newline() {
    let repo = newline_repo();
    apply_all(&repo, "remove_newline.txt");
    assert_eq!(repo.staged("remove_newline.txt"), b"line 1\nline 2");
}

#[test]
fn adding_the_trailing_newline() {
    let repo = newline_repo();
    apply_all(&repo, "add_newline.txt");
    assert_eq!(repo.staged("add_newline.txt"), b"line 1\nline 2\n");
}

#[test]
fn applying_everything_matches_git_add() {
    let repo = newline_repo();
    for path in [
        "no_newline.txt",
        "with_newline.txt",
        "remove_newline.txt",
        "add_newline.txt",
    ] {
        apply_all(&repo, path);
        assert_eq!(repo.git(&["diff", "--", path]), "", "{path} fully staged");
    }
}

// ---------- regressions fixed in the Rust port ----------

#[test]
fn crlf_line_endings_are_preserved() {
    let repo = Repo::new();
    repo.write("win.txt", "a\r\nb\r\nc\r\n");
    repo.commit_all("init");
    repo.write("win.txt", "a\r\nB\r\nc\r\nd\r\n");

    apply_all(&repo, "win.txt");
    assert_eq!(repo.staged("win.txt"), b"a\r\nB\r\nc\r\nd\r\n");
    assert_eq!(repo.git(&["diff", "win.txt"]), "");
}

#[test]
fn lines_that_look_like_diff_headers_apply_cleanly() {
    let repo = Repo::new();
    repo.write("q.sql", "-- old comment\nselect 1;\n");
    repo.commit_all("init");
    repo.write("q.sql", "++ new\nselect 1;\n");

    apply(&repo, "q.sql", "0001");
    assert_eq!(repo.staged("q.sql"), b"select 1;\n");
    apply(&repo, "q.sql", "0001");
    assert_eq!(repo.staged("q.sql"), b"++ new\nselect 1;\n");
}

#[test]
fn non_utf8_content_is_staged_byte_for_byte() {
    let repo = Repo::new();
    let sjis_old = b"\x82\xa0\n\x82\xa2\n".to_vec(); // "あ\nい\n" in Shift_JIS
    repo.write("sjis.txt", &sjis_old);
    repo.commit_all("init");
    let mut sjis_new = sjis_old.clone();
    sjis_new.extend_from_slice(b"\x82\xa4\n"); // "う"
    repo.write("sjis.txt", &sjis_new);

    apply_all(&repo, "sjis.txt");
    assert_eq!(repo.staged("sjis.txt"), sjis_new);
}

#[test]
fn non_ascii_paths_can_be_staged() {
    let repo = Repo::new();
    repo.write("日本語 file.txt", "one\n");
    repo.commit_all("init");
    repo.write("日本語 file.txt", "one\ntwo\n");

    let result = apply(&repo, "日本語 file.txt", "0001-0002");
    assert_eq!(result["applied"][0]["applied_count"], 2);
    assert_eq!(repo.staged("日本語 file.txt"), b"one\ntwo\n");
}

#[test]
fn hunks_far_apart_are_selected_independently() {
    let repo = Repo::new();
    let base: String = (1..=40).map(|i| format!("l{i}\n")).collect();
    repo.write("f.txt", &base);
    repo.commit_all("init");
    let edited = base.replace("l2\n", "L2\n").replace("l39\n", "L39\n");
    repo.write("f.txt", &edited);

    // 0001/0002 are the first hunk, 0003/0004 the second.
    apply(&repo, "f.txt", "0003,0004");
    let staged = String::from_utf8(repo.staged("f.txt")).unwrap();
    assert!(staged.contains("l2\n") && staged.contains("L39\n"), "{staged}");
}

#[test]
fn cli_list_and_apply() {
    let repo = repo_with_changes();
    let out = repo.cli(&["list", "--paths", "README.md", "--unified", "3"]);
    assert!(out.status.success());
    let listed: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(listed["files"][0]["path"], "README.md");

    let out = repo.cli(&["apply", "README.md", "0001-0002"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let applied: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(applied["stats"]["changes_applied"], 2);

    let out = repo.cli(&["apply", "README.md", "1"]);
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(
        String::from_utf8_lossy(&out.stderr).trim(),
        r#"{"error":"invalid token: 1"}"#
    );
}
