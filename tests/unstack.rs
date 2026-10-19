mod common;

use common::Repo;
use serde_json::{Value, json};

/// Scenario 003: base commit plus four commits that each add a file.
fn linear_history() -> Repo {
    let repo = Repo::new();
    repo.write("README.md", "# Test Repository\n");
    repo.commit_all("Initial commit");
    for i in 0..4 {
        repo.write(&format!("feature_{i}.txt"), format!("Feature {i} implementation\n"));
        repo.commit_all(&format!("Add feature {i}"));
    }
    repo
}

/// Scenario 002: A (empty file) -> B (adds 3 lines) -> C (removes line 2).
fn dependent_history() -> Repo {
    let repo = Repo::new();
    repo.write("file.txt", "");
    repo.commit_all("Initial commit (empty file)");
    repo.write("file.txt", "hello,\nmy name is john.\nnice to meet you.\n");
    repo.commit_all("Add introduction (3 lines)");
    repo.write("file.txt", "hello,\nnice to meet you.\n");
    repo.commit_all("Remove name line");
    repo
}

fn unstack(repo: &Repo, branches: Value, parent: &str) -> Value {
    repo.mcp()
        .call("unstack", json!({"branches": branches, "parent": parent}))
}

fn branch_files(repo: &Repo, branch: &str) -> Vec<String> {
    repo.git(&["ls-tree", "--name-only", branch])
        .lines()
        .map(String::from)
        .collect()
}

#[test]
fn single_branch_single_commit() {
    let repo = linear_history();
    let head_before = repo.rev("HEAD");
    let feature_0 = repo.rev("HEAD~3");

    let result = unstack(&repo, json!({"feat/test": [feature_0]}), "HEAD~4");
    assert_eq!(
        result["stats"],
        json!({"total_branches": 1, "successful_branches": 1, "failed_branches": 0})
    );
    let branch = &result["created_branches"][0];
    assert_eq!(branch["name"], "feat/test");
    assert_eq!(branch["commits_applied"][0]["ref"], json!(feature_0));
    assert_eq!(branch["head_sha"], json!(repo.rev("feat/test")));

    assert_eq!(branch_files(&repo, "feat/test"), ["README.md", "feature_0.txt"]);
    assert_eq!(
        repo.git(&["log", "-1", "--format=%s%n%an <%ae>", "feat/test"]),
        "Add feature 0\nTest User <test@example.com>\n"
    );
    assert_eq!(
        repo.git(&["log", "-1", "--format=%aI", "feat/test"]),
        repo.git(&["log", "-1", "--format=%aI", &feature_0])
    );
    assert_eq!(repo.rev("HEAD"), head_before, "current branch untouched");
    assert_eq!(repo.git(&["status", "--porcelain"]), "", "working tree untouched");
}

#[test]
fn multiple_parallel_branches() {
    let repo = linear_history();
    let (f0, f1, f2) = (repo.rev("HEAD~3"), repo.rev("HEAD~2"), repo.rev("HEAD~1"));

    let result = unstack(&repo, json!({"feat/a": [f0, f2], "feat/b": [f1]}), "HEAD~4");
    assert_eq!(result["stats"]["successful_branches"], 2);
    assert_eq!(result["stats"]["failed_branches"], 0);
    let names: Vec<_> = result["created_branches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["name"].clone())
        .collect();
    assert_eq!(names, [json!("feat/a"), json!("feat/b")]);

    assert_eq!(
        branch_files(&repo, "feat/a"),
        ["README.md", "feature_0.txt", "feature_2.txt"]
    );
    assert_eq!(branch_files(&repo, "feat/b"), ["README.md", "feature_1.txt"]);
    assert_eq!(
        repo.git(&["log", "--format=%s", "feat/a"]),
        "Add feature 2\nAdd feature 0\nInitial commit\n"
    );
}

#[test]
fn existing_branch_is_not_overwritten() {
    let repo = linear_history();
    repo.git(&["branch", "existing-branch"]);
    let before = repo.rev("existing-branch");

    let result = unstack(&repo, json!({"existing-branch": [repo.rev("HEAD~3")]}), "HEAD~4");
    assert_eq!(result["stats"]["successful_branches"], 0);
    assert_eq!(result["stats"]["failed_branches"], 1);
    assert!(
        result["errors"][0]["error"]
            .as_str()
            .unwrap()
            .contains("already exists")
    );
    assert_eq!(repo.rev("existing-branch"), before);
}

#[test]
fn unknown_parent_fails_every_branch() {
    let repo = linear_history();
    let result = unstack(&repo, json!({"a": ["HEAD"], "b": ["HEAD"]}), "no-such-ref");
    assert_eq!(
        result["stats"],
        json!({"total_branches": 2, "successful_branches": 0, "failed_branches": 2})
    );
    assert_eq!(result["errors"][0]["branch"], Value::Null);
    assert_eq!(result["errors"][0]["commit"], "no-such-ref");
}

#[test]
fn independent_commit_succeeds() {
    let repo = dependent_history();
    let result = unstack(
        &repo,
        json!({"feat/add-lines": [repo.rev("HEAD~1")]}),
        &repo.rev("HEAD~2"),
    );
    assert_eq!(result["stats"]["successful_branches"], 1, "{result}");
    assert_eq!(
        repo.git(&["show", "feat/add-lines:file.txt"]),
        "hello,\nmy name is john.\nnice to meet you.\n"
    );
}

#[test]
fn dependent_commit_reports_a_conflict() {
    let repo = dependent_history();
    let result = unstack(
        &repo,
        json!({"feat/delete-line": [repo.rev("HEAD")]}),
        &repo.rev("HEAD~2"),
    );
    assert_eq!(result["stats"]["successful_branches"], 0);
    assert_eq!(result["stats"]["failed_branches"], 1);
    let error = &result["errors"][0];
    assert_eq!(error["branch"], "feat/delete-line");
    assert!(
        error["error"].as_str().unwrap().to_lowercase().contains("conflict"),
        "{error}"
    );
    assert_eq!(
        repo.git(&["branch", "--list", "feat/delete-line"]),
        "",
        "no branch on conflict"
    );
}

#[test]
fn sequential_commits_succeed() {
    let repo = dependent_history();
    let result = unstack(
        &repo,
        json!({"feat/both": [repo.rev("HEAD~1"), repo.rev("HEAD")]}),
        &repo.rev("HEAD~2"),
    );
    assert_eq!(result["stats"]["successful_branches"], 1, "{result}");
    assert_eq!(
        result["created_branches"][0]["commits_applied"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(repo.git(&["show", "feat/both:file.txt"]), "hello,\nnice to meet you.\n");
}

#[test]
fn multiline_commit_messages_survive() {
    let repo = Repo::new();
    repo.write("a.txt", "a\n");
    repo.commit_all("base");
    repo.write("b.txt", "b\n");
    repo.git(&["add", "-A"]);
    repo.git(&[
        "commit",
        "-q",
        "-m",
        "Subject line",
        "-m",
        "Body with \"quotes\" and 日本語\n\n- bullet",
    ]);

    unstack(&repo, json!({"copy": ["HEAD"]}), "HEAD~1");
    assert_eq!(
        repo.git(&["log", "-1", "--format=%B", "copy"]),
        repo.git(&["log", "-1", "--format=%B", "HEAD"])
    );
}
