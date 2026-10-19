mod common;

use common::{Mcp, Repo};
use serde_json::json;

#[test]
fn tools_are_listed_with_annotations() {
    let repo = Repo::new();
    let mut mcp = repo.mcp();
    let resp = mcp.request("tools/list", json!({}));
    let tools = resp["result"]["tools"].as_array().unwrap();
    let names: Vec<_> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(
        names,
        ["list_changes", "diff", "apply_changes", "auto_commit", "unstack"]
    );

    let apply = &tools[2];
    assert_eq!(
        apply["annotations"],
        json!({"readOnlyHint": false, "destructiveHint": true, "openWorldHint": true})
    );
    assert_eq!(apply["inputSchema"]["required"], json!(["path", "lines"]));
}

#[test]
fn auto_commit_prompt() {
    let repo = Repo::new();
    let mut mcp = repo.mcp();
    let list = mcp.request("prompts/list", json!({}));
    assert_eq!(list["result"]["prompts"][0]["name"], "auto-commit");

    let got = mcp.request("prompts/get", json!({"name": "auto-commit"}));
    assert_eq!(
        got["result"]["messages"][0]["content"]["text"],
        "Call auto_commit and follow the instructions it returns."
    );
}

#[test]
fn argument_errors_are_tool_errors() {
    let repo = Repo::new();
    let mut mcp = repo.mcp();
    let result = mcp.call_raw("diff", json!({}));
    assert_eq!(result["isError"], true);
    assert!(result["content"][0]["text"].as_str().unwrap().contains("path"));

    let result = mcp.call_raw("no_such_tool", json!({}));
    assert_eq!(result["isError"], true);
    assert_eq!(result["content"][0]["text"], "Unknown tool: no_such_tool");
}

#[test]
fn git_failures_are_tool_errors() {
    // Not a git repository.
    let dir = std::env::temp_dir().join(format!("git-polite-not-a-repo-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut mcp = Mcp::start(&dir, &["mcp"]);
    let result = mcp.call_raw("list_changes", json!({}));
    assert_eq!(result["isError"], true);
    assert!(
        result["content"][0]["text"]
            .as_str()
            .unwrap()
            .starts_with("Error calling tool 'list_changes':")
    );
    drop(mcp);
    let _ = std::fs::remove_dir_all(dir);
}
