mod common;

use common::{Repo, numbered, repo_with_changes};
use serde_json::{Value, json};

fn files_by_path(result: &Value) -> std::collections::BTreeMap<String, Value> {
    result["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| (f["path"].as_str().unwrap().to_string(), f.clone()))
        .collect()
}

#[test]
fn lists_modified_added_and_deleted_files() {
    let repo = repo_with_changes();
    let result = repo.mcp().call("list_changes", json!({}));
    let files = files_by_path(&result);
    assert_eq!(files.len(), 3);
    assert_eq!(files["README.md"]["status"], "modified");
    assert_eq!(files["README.md"]["binary"], false);
    assert_eq!(files["new_file.txt"]["status"], "added");
    assert_eq!(files["deleted.txt"]["status"], "deleted");
}

#[test]
fn modified_file_has_numbered_lines() {
    let repo = repo_with_changes();
    let result = repo.mcp().call("list_changes", json!({"paths": ["README.md"]}));
    let readme = &result["files"][0];
    assert_eq!(
        readme["lines"],
        json!([
            "        # Test Repository",
            "        Original content",
            "0001: + Modified line",
            "0002: + "
        ])
    );
}

#[test]
fn untracked_file_shows_all_additions() {
    let repo = repo_with_changes();
    let result = repo.mcp().call("list_changes", json!({"paths": ["new_file.txt"]}));
    let file = &result["files"][0];
    assert_eq!(file["status"], "added");
    let changes = numbered(&file["lines"]);
    assert!(!changes.is_empty());
    assert!(changes.iter().all(|l| &l[6..7] == "+"));
}

#[test]
fn path_filter_limits_to_one_file() {
    let repo = repo_with_changes();
    let result = repo.mcp().call("list_changes", json!({"paths": ["README.md"]}));
    let paths: Vec<_> = result["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["path"].clone())
        .collect();
    assert_eq!(paths, [json!("README.md")]);
}

#[test]
fn pagination_by_file_count() {
    let repo = repo_with_changes();
    let mut mcp = repo.mcp();
    let mut seen = Vec::new();
    let mut token = Value::Null;
    for _ in 0..5 {
        let page = mcp.call("list_changes", json!({"page_size_files": 1, "page_token": token}));
        assert_eq!(page["files"].as_array().unwrap().len(), 1);
        seen.push(page["files"][0]["path"].as_str().unwrap().to_string());
        token = page["page_token_next"].clone();
        if token.is_null() {
            break;
        }
    }
    assert_eq!(seen, ["README.md", "deleted.txt", "new_file.txt"]);
}

#[test]
fn large_diffs_are_truncated_but_diff_shows_everything() {
    let repo = Repo::new();
    repo.write("small.txt", "a\n");
    repo.commit_all("init");
    let big: String = (0..2000).map(|i| format!("line number {i}\n")).collect();
    repo.write("big.txt", &big);

    let mut mcp = repo.mcp();
    let listed = mcp.call("list_changes", json!({}));
    let big_entry = &listed["files"][0];
    assert_eq!(big_entry["path"], "big.txt");
    assert_eq!(big_entry["truncated"], true);
    assert!(big_entry["reason"].as_str().unwrap().starts_with("diff too large ("));
    assert_eq!(listed["stats"]["truncated_files"], 1);

    let full = mcp.call("diff", json!({"path": "big.txt"}));
    assert_eq!(
        numbered(&full["lines"]).len(),
        2001,
        "2000 lines plus the trailing newline"
    );
    assert!(full["size_bytes"].as_u64().unwrap() > 10 * 1024);
}

#[test]
fn diff_reports_files_without_changes() {
    let repo = repo_with_changes();
    let result = repo.mcp().call("diff", json!({"path": "nope.txt"}));
    assert_eq!(
        result,
        json!({"path": "nope.txt", "error": "No changes found for this file", "size_bytes": 0})
    );
}

#[test]
fn binary_files_are_flagged() {
    let repo = Repo::new();
    repo.write("img.bin", [0u8, 1, 2, 3, 0, 255]);
    repo.write("gone.bin", [0u8, 9, 9]);
    repo.commit_all("init");
    repo.write("img.bin", [0u8, 4, 5, 6, 0, 254]);
    repo.write("new.bin", [0u8, 7]);
    repo.remove("gone.bin");

    let result = repo.mcp().call("list_changes", json!({}));
    let files = files_by_path(&result);
    for path in ["img.bin", "new.bin", "gone.bin"] {
        assert_eq!(files[path]["binary"], true, "{path}");
        assert_eq!(files[path]["lines"], json!([]), "{path}");
    }
    assert_eq!(files["gone.bin"]["status"], "deleted");
    assert_eq!(files["new.bin"]["status"], "added");
}

#[test]
fn unusual_paths_are_reported_verbatim() {
    let repo = Repo::new();
    repo.write("a b.txt", "x\n");
    repo.write("日本語.txt", "x\n");
    repo.commit_all("init");
    repo.write("a b.txt", "y\n");
    repo.write("日本語.txt", "y\n");
    repo.write("新規 file.txt", "z\n");

    let result = repo.mcp().call("list_changes", json!({}));
    let paths: Vec<String> = files_by_path(&result).into_keys().collect();
    assert_eq!(paths, ["a b.txt", "新規 file.txt", "日本語.txt"]);
}

#[test]
fn removed_lines_that_look_like_diff_headers_keep_their_numbers() {
    // A removed "-- comment" appears as "--- comment" in the diff, and an
    // added "++ x" as "+++ x". They must stay inside the hunk.
    let repo = Repo::new();
    repo.write("q.sql", "-- old comment\nselect 1;\n");
    repo.commit_all("init");
    repo.write("q.sql", "++ new\nselect 1;\n");

    let result = repo.mcp().call("list_changes", json!({}));
    assert_eq!(result["files"].as_array().unwrap().len(), 1);
    assert_eq!(
        numbered(&result["files"][0]["lines"]),
        ["0001: - -- old comment", "0002: + ++ new", "0003: + "]
    );
}

#[test]
fn text_mode_renders_boxes() {
    let repo = repo_with_changes();
    let text = repo
        .mcp_text()
        .call_text("list_changes", json!({"paths": ["README.md"]}));
    assert_eq!(
        text,
        "┌──── README.md (modified)\n│         # Test Repository\n│         Original content\n│ 0001: + Modified line\n│ 0002: + \n└──── all lines: 0001-0002\n\n1 file(s), 4 change(s), 78 bytes"
    );
}

#[test]
fn auto_commit_summarizes_without_diff_content() {
    let repo = repo_with_changes();
    let report = repo.mcp().call("auto_commit", json!({}));
    assert_eq!(
        report["recent_commits"],
        json!(["Add file to be deleted", "Initial commit"])
    );
    assert_eq!(
        report["summary"],
        json!({"total_files": 3, "total_additions": 2, "total_deletions": 1, "total_changes": 3})
    );
    let schemas: Vec<_> = report["tool_schemas"].as_object().unwrap().keys().cloned().collect();
    assert_eq!(schemas, ["list_changes", "diff", "apply_changes"]);
}
