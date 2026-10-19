//! MCP server over stdio.

use std::sync::{Arc, Mutex};

use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, GetPromptRequestParams, GetPromptResponse,
    GetPromptResult, Implementation, ListPromptsResult, ListToolsResult, PaginatedRequestParams, Prompt, PromptMessage,
    Role, ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData as McpError, RoleServer, ServerHandler, ServiceExt};
use serde::de::{self, DeserializeOwned, Deserializer};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::git::GitError;
use crate::ops::{self, PAGE_SIZE_BYTES_DEFAULT, PAGE_SIZE_FILES_DEFAULT, UNIFIED_LIST_DEFAULT};
use crate::{format, unstack};

/// Tool names, descriptions, input schemas and annotations.
const TOOLS_JSON: &str = include_str!("tools.json");

const PROMPT_NAME: &str = "auto-commit";
const PROMPT_DESCRIPTION: &str = "Organize and commit changes using git-polite tools.";
const PROMPT_TEXT: &str = "Call auto_commit and follow the instructions it returns.";

#[derive(Clone)]
pub struct GitPoliteServer {
    structured: bool,
    tools: Arc<Vec<Tool>>,
    /// Tool calls run one at a time so concurrent requests never interleave
    /// index updates.
    git_lock: Arc<Mutex<()>>,
}

impl GitPoliteServer {
    pub fn new(structured: bool) -> Self {
        let tools: Vec<Tool> = serde_json::from_str(TOOLS_JSON).expect("tools.json is valid");
        GitPoliteServer {
            structured,
            tools: Arc::new(tools),
            git_lock: Arc::new(Mutex::new(())),
        }
    }

    /// `{name: {description, inputSchema}}` for the tools auto_commit points to.
    fn tool_schemas(&self) -> Map<String, Value> {
        ["list_changes", "diff", "apply_changes"]
            .iter()
            .filter_map(|name| self.tools.iter().find(|t| t.name == *name))
            .map(|t| {
                let mut entry = Map::new();
                entry.insert(
                    "description".into(),
                    Value::String(t.description.as_deref().unwrap_or("").to_string()),
                );
                entry.insert("inputSchema".into(), Value::Object((*t.input_schema).clone()));
                (t.name.to_string(), Value::Object(entry))
            })
            .collect()
    }

    fn dispatch(&self, name: &str, args: Value) -> CallToolResult {
        let structured = self.structured;
        match name {
            "list_changes" => call(name, args, |a: ListChangesArgs| {
                let result = ops::list_files(
                    &a.paths.0,
                    a.page_token.as_deref(),
                    a.page_size_files.map_or(PAGE_SIZE_FILES_DEFAULT, |n| n.0),
                    a.page_size_bytes.map_or(PAGE_SIZE_BYTES_DEFAULT, |n| n.0),
                    a.unified.map_or(UNIFIED_LIST_DEFAULT, |n| n.0),
                )?;
                Ok(render(structured, &result, format::list_text))
            }),
            "diff" => call(name, args, |a: DiffArgs| {
                let result = ops::diff_file(&a.path, a.unified.map_or(UNIFIED_LIST_DEFAULT, |n| n.0))?;
                Ok(render(structured, &result, format::diff_text))
            }),
            "apply_changes" => call(name, args, |a: ApplyArgs| {
                let numbers = match ops::parse_number_tokens(&a.lines) {
                    Ok(numbers) => numbers,
                    Err(error) => {
                        let error = ErrorOnly { error };
                        return Ok(render(structured, &error, |e| format::error_text(&e.error)));
                    }
                };
                let result = ops::apply_one_file(&a.path, &numbers)?;
                Ok(render(structured, &result, format::apply_text))
            }),
            "auto_commit" => call(name, args, |_: NoArgs| {
                let report = ops::auto_commit_report(self.tool_schemas())?;
                Ok(render(structured, &report, format::auto_commit_text))
            }),
            "unstack" => call(name, args, |a: UnstackArgs| {
                let parent = a.parent.as_deref().unwrap_or("origin/main");
                let result = unstack::do_unstack(&a.branches.0, parent);
                Ok(render(structured, &result, format::unstack_text))
            }),
            _ => text_error(format!("Unknown tool: {name}")),
        }
    }
}

fn text_error(message: String) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(message)])
}

/// Parse arguments, run the tool, and turn failures into error results the
/// calling agent can read.
fn call<A: DeserializeOwned>(
    tool: &str,
    args: Value,
    run: impl FnOnce(A) -> Result<CallToolResult, GitError>,
) -> CallToolResult {
    let args = match serde_json::from_value(args) {
        Ok(args) => args,
        Err(e) => return text_error(format!("Invalid arguments for tool '{tool}': {e}")),
    };
    match run(args) {
        Ok(result) => result,
        Err(e) => text_error(format!("Error calling tool '{tool}': {e}")),
    }
}

/// Plain text by default; with `--structured-output`, JSON text plus
/// `structuredContent`.
fn render<T: Serialize>(structured: bool, value: &T, text: impl FnOnce(&T) -> String) -> CallToolResult {
    if structured {
        CallToolResult::structured(serde_json::to_value(value).expect("tool results serialize"))
    } else {
        CallToolResult::success(vec![ContentBlock::text(text(value))])
    }
}

#[derive(Serialize)]
struct ErrorOnly {
    error: String,
}

// ---------- arguments ----------

/// An integer that also accepts integral floats and numeric strings, as
/// agents sometimes send `"50"` for 50.
struct Int(i64);

impl<'de> Deserialize<'de> for Int {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = Value::deserialize(d)?;
        let n = match &v {
            Value::Number(n) => n
                .as_i64()
                .or_else(|| n.as_f64().filter(|f| f.fract() == 0.0).map(|f| f as i64)),
            Value::String(s) => s.trim().parse().ok(),
            _ => None,
        };
        n.map(Int)
            .ok_or_else(|| de::Error::custom(format!("expected an integer, got {v}")))
    }
}

/// A list of strings; `null` means empty and a bare string means one item.
#[derive(Default)]
struct StringList(Vec<String>);

impl<'de> Deserialize<'de> for StringList {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        match Option::<OneOrMany>::deserialize(d)? {
            None => Ok(StringList::default()),
            Some(OneOrMany::One(s)) => Ok(StringList(vec![s])),
            Some(OneOrMany::Many(v)) => Ok(StringList(v)),
        }
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum OneOrMany {
    One(String),
    Many(Vec<String>),
}

/// `{branch: [commits]}` in the order the client sent it.
struct BranchMap(Vec<(String, Vec<String>)>);

impl<'de> Deserialize<'de> for BranchMap {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let map = Map::<String, Value>::deserialize(d)?;
        map.into_iter()
            .map(|(branch, commits)| {
                serde_json::from_value::<Vec<String>>(commits)
                    .map(|commits| (branch.clone(), commits))
                    .map_err(|e| de::Error::custom(format!("branch '{branch}': {e}")))
            })
            .collect::<Result<_, _>>()
            .map(BranchMap)
    }
}

#[derive(Deserialize)]
struct ListChangesArgs {
    #[serde(default)]
    paths: StringList,
    #[serde(default)]
    page_token: Option<String>,
    #[serde(default)]
    page_size_files: Option<Int>,
    #[serde(default)]
    page_size_bytes: Option<Int>,
    #[serde(default)]
    unified: Option<Int>,
}

#[derive(Deserialize)]
struct DiffArgs {
    path: String,
    #[serde(default)]
    unified: Option<Int>,
}

#[derive(Deserialize)]
struct ApplyArgs {
    path: String,
    lines: String,
}

#[derive(Deserialize)]
struct NoArgs {}

#[derive(Deserialize)]
struct UnstackArgs {
    branches: BranchMap,
    #[serde(default)]
    parent: Option<String>,
}

// ---------- protocol ----------

impl ServerHandler for GitPoliteServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().enable_prompts().build())
            .with_server_info(Implementation::new("git-polite", env!("CARGO_PKG_VERSION")))
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult::with_all_items(self.tools.to_vec()))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let name = request.name.to_string();
        let args = Value::Object(request.arguments.unwrap_or_default());
        let server = self.clone();
        // git runs as blocking subprocesses; keep them off the runtime thread
        // so pings and cancellations are still handled meanwhile.
        let result = tokio::task::spawn_blocking(move || {
            let _guard = server.git_lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            server.dispatch(&name, args)
        })
        .await
        .map_err(|e| McpError::internal_error(format!("tool task failed: {e}"), None))?;
        Ok(result.into())
    }

    async fn list_prompts(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListPromptsResult, McpError> {
        Ok(ListPromptsResult::with_all_items(vec![Prompt::new(
            PROMPT_NAME,
            Some(PROMPT_DESCRIPTION),
            Some(Vec::new()),
        )]))
    }

    async fn get_prompt(
        &self,
        request: GetPromptRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<GetPromptResponse, McpError> {
        if request.name != PROMPT_NAME {
            return Err(McpError::invalid_params(
                format!("Unknown prompt: {}", request.name),
                None,
            ));
        }
        let message = PromptMessage::new_text(Role::User, PROMPT_TEXT);
        Ok(GetPromptResult::new(vec![message])
            .with_description(PROMPT_DESCRIPTION)
            .into())
    }
}

/// Serve MCP over stdin/stdout until the client disconnects.
pub fn run(structured: bool) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // A single-threaded runtime keeps the resident footprint small; git work
    // happens on tokio's blocking pool.
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    runtime.block_on(async move {
        let service = GitPoliteServer::new(structured).serve(rmcp::transport::stdio()).await?;
        service.waiting().await?;
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tools_json_matches_expected_names() {
        let server = GitPoliteServer::new(false);
        let names: Vec<_> = server.tools.iter().map(|t| t.name.to_string()).collect();
        assert_eq!(
            names,
            ["list_changes", "diff", "apply_changes", "auto_commit", "unstack"]
        );
        assert_eq!(server.tool_schemas().len(), 3);
    }

    #[test]
    fn lenient_arguments() {
        let a: ListChangesArgs = serde_json::from_value(serde_json::json!({
            "paths": "README.md", "page_size_files": "7", "unified": 3.0, "page_token": null
        }))
        .unwrap();
        assert_eq!(a.paths.0, ["README.md"]);
        assert_eq!(a.page_size_files.unwrap().0, 7);
        assert_eq!(a.unified.unwrap().0, 3);
        assert!(serde_json::from_value::<ListChangesArgs>(serde_json::json!({"unified": "x"})).is_err());

        let u: UnstackArgs = serde_json::from_value(serde_json::json!({
            "branches": {"b": ["2"], "a": ["1"]}
        }))
        .unwrap();
        let order: Vec<_> = u.branches.0.iter().map(|(b, _)| b.as_str()).collect();
        assert_eq!(order, ["b", "a"], "client order is preserved");
    }

    #[test]
    fn unknown_tool_is_a_tool_error() {
        let result = GitPoliteServer::new(false).dispatch("nope", Value::Object(Map::new()));
        assert_eq!(result.is_error, Some(true));
    }
}
