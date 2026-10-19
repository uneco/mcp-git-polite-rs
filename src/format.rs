//! Plain-text renderings of tool results: box drawing for LLMs and ANSI
//! colors for the CLI's `--format pretty`.

use serde_json::Value;

use crate::ops::{ApplyResult, AutoCommitReport, DiffResult, ListResult};
use crate::unstack::UnstackResult;

const ANSI_RED: &str = "\x1b[31m";
const ANSI_GREEN: &str = "\x1b[32m";
const ANSI_CYAN: &str = "\x1b[36m";
const ANSI_RESET: &str = "\x1b[0m";
const ANSI_BOLD: &str = "\x1b[1m";

/// Leading change number of a line like `0005: + text`.
fn change_number(line: &str) -> Option<usize> {
    if !line.starts_with(|c: char| c.is_ascii_digit()) {
        return None;
    }
    line.split(':').next()?.trim().parse().ok()
}

fn file_box(out: &mut Vec<String>, path: &str, status: &str, diff_lines: &[String], note: Option<&str>) {
    out.push(format!("┌──── {path} ({status})"));
    if let Some(note) = note {
        out.push(format!("│ ({note})"));
        out.push("└────".into());
        return;
    }
    let mut numbers = Vec::new();
    for line in diff_lines {
        out.push(format!("│ {line}"));
        numbers.extend(change_number(line));
    }
    match (numbers.first(), numbers.last()) {
        (Some(first), Some(last)) if first == last => out.push(format!("└──── all lines: {first:04}")),
        (Some(first), Some(last)) => out.push(format!("└──── all lines: {first:04}-{last:04}")),
        _ => out.push("└────".into()),
    }
}

pub fn error_text(message: &str) -> String {
    format!("Error: {message}")
}

pub fn list_text(result: &ListResult) -> String {
    let mut out = Vec::new();
    for f in &result.files {
        if f.binary {
            file_box(&mut out, &f.path, f.status, &[], Some("binary file"));
        } else if f.truncated {
            let note = format!("truncated: {}", f.reason.as_deref().unwrap_or("diff too large"));
            file_box(&mut out, &f.path, f.status, &[], Some(&note));
        } else {
            file_box(&mut out, &f.path, f.status, &f.lines, None);
        }
        out.push(String::new());
    }

    let stats = &result.stats;
    let mut parts = vec![format!("{} file(s)", stats.files), format!("{} change(s)", stats.lines)];
    if stats.page_bytes > 0 {
        parts.push(format!("{} bytes", stats.page_bytes));
    }
    let mut summary = parts.join(", ");
    if stats.truncated_files > 0 {
        summary.push_str(&format!(" ({} truncated)", stats.truncated_files));
    }
    out.push(summary);
    if let Some(token) = &result.page_token_next {
        out.push(format!("page_token_next: {token}"));
    }
    out.join("\n")
}

pub fn diff_text(result: &DiffResult) -> String {
    if let Some(error) = &result.error {
        return error_text(error);
    }
    let mut out = Vec::new();
    let status = result.status.unwrap_or("modified");
    if result.binary == Some(true) {
        file_box(&mut out, &result.path, status, &[], Some("binary file"));
    } else {
        file_box(
            &mut out,
            &result.path,
            status,
            result.lines.as_deref().unwrap_or(&[]),
            None,
        );
    }
    out.push(String::new());
    out.push(format!("{} bytes", result.size_bytes));
    out.join("\n")
}

pub fn apply_text(result: &ApplyResult) -> String {
    let mut out = Vec::new();
    for a in &result.applied {
        out.push(format!("Applied {} change(s) to {}", a.applied_count, a.file));
        let unstaged = a.after_applying.unstaged_lines;
        if unstaged > 0 {
            out.push(format!("{unstaged} unstaged change(s) remaining"));
        }
        if !a.after_applying.diff.is_empty() {
            out.push(String::new());
            file_box(&mut out, &a.file, "remaining", &a.after_applying.diff, None);
        }
    }
    for s in &result.skipped {
        out.push(format!("Skipped {} #{}: {}", s.file, s.number, s.reason));
    }
    out.push(String::new());
    out.push(format!(
        "{} applied, {} skipped",
        result.stats.changes_applied, result.stats.changes_skipped
    ));
    out.join("\n")
}

/// Render a JSON value the way Python's `str()` would (for schema defaults).
fn py_str(v: &Value) -> String {
    fn py_repr(v: &Value) -> String {
        match v {
            Value::String(s) => format!("'{s}'"),
            other => py_str(other),
        }
    }
    match v {
        Value::Null => "None".into(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        Value::Array(items) => format!("[{}]", items.iter().map(py_repr).collect::<Vec<_>>().join(", ")),
        Value::Object(map) => format!(
            "{{{}}}",
            map.iter()
                .map(|(k, v)| format!("'{k}': {}", py_repr(v)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn collect_schema_types(prop: &Value, out: &mut Vec<String>) {
    if let Some(t) = prop.get("type") {
        out.push(match t {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        });
    } else if let Some(Value::Array(variants)) = prop.get("anyOf") {
        for variant in variants {
            collect_schema_types(variant, out);
        }
    }
}

fn schema_type_label(prop: &Value) -> String {
    let mut types = Vec::new();
    collect_schema_types(prop, &mut types);
    let mut unique: Vec<String> = Vec::new();
    for t in types {
        if !unique.contains(&t) {
            unique.push(t);
        }
    }
    if unique.is_empty() {
        "any".into()
    } else {
        unique.join(" | ")
    }
}

fn schema_description(prop: &Value) -> String {
    if let Some(Value::String(d)) = prop.get("description") {
        return d.clone();
    }
    if let Some(Value::Array(variants)) = prop.get("anyOf") {
        for variant in variants {
            let d = schema_description(variant);
            if !d.is_empty() {
                return d;
            }
        }
    }
    String::new()
}

/// Markdown summary for `auto_commit`.
pub fn auto_commit_text(report: &AutoCommitReport) -> String {
    let mut out = Vec::new();
    if !report.recent_commits.is_empty() {
        out.push("# Recent commits".to_string());
        out.push(String::new());
        out.extend(report.recent_commits.iter().map(|m| format!("- {m}")));
        out.push(String::new());
    }

    let s = &report.summary;
    out.push("# Changes".into());
    out.push(String::new());
    out.push(format!(
        "{} file(s) changed, {} addition(s), {} deletion(s)",
        s.total_files, s.total_additions, s.total_deletions
    ));
    out.push(String::new());
    for f in &report.files {
        if f.binary {
            out.push(format!("- `{}` ({}) binary", f.path, f.status));
        } else {
            out.push(format!(
                "- `{}` ({}) +{} -{}",
                f.path, f.status, f.additions, f.deletions
            ));
        }
    }
    out.push(String::new());

    if !report.tool_schemas.is_empty() {
        out.push("# Available tools".into());
        out.push(String::new());
        for (name, schema) in &report.tool_schemas {
            out.push(format!("## {name}"));
            if let Some(Value::String(desc)) = schema.get("description")
                && !desc.is_empty()
            {
                out.push(desc.clone());
            }
            out.push(String::new());
            let input_schema = schema.get("inputSchema");
            let props = input_schema
                .and_then(|s| s.get("properties"))
                .and_then(Value::as_object);
            let required: Vec<&str> = input_schema
                .and_then(|s| s.get("required"))
                .and_then(Value::as_array)
                .map(|r| r.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            match props {
                Some(props) if !props.is_empty() => {
                    out.push("Parameters:".into());
                    for (pname, pdef) in props {
                        let default = pdef
                            .get("default")
                            .map(|d| format!(", default={}", py_str(d)))
                            .unwrap_or_default();
                        let req = if required.contains(&pname.as_str()) {
                            " (required)"
                        } else {
                            ""
                        };
                        out.push(format!(
                            "- `{pname}` ({}{default}){req}: {}",
                            schema_type_label(pdef),
                            schema_description(pdef)
                        ));
                    }
                }
                _ => out.push("Parameters: none".into()),
            }
            out.push(String::new());
        }
    }

    out.push("# Next steps".into());
    out.push(String::new());
    out.push(report.next.to_string());
    out.join("\n")
}

pub fn unstack_text(result: &UnstackResult) -> String {
    let mut out = Vec::new();
    for b in &result.created_branches {
        let head: String = b.head_sha.chars().take(7).collect();
        out.push(format!(
            "Created {} ({} commit(s), head: {head})",
            b.name,
            b.commits_applied.len()
        ));
    }
    for e in &result.errors {
        out.push(format!(
            "Error [{}]: {}",
            e.branch.as_deref().unwrap_or("None"),
            e.error
        ));
    }
    out.push(String::new());
    out.push(format!(
        "{}/{} branches created",
        result.stats.successful_branches, result.stats.total_branches
    ));
    out.join("\n")
}

fn colorize_diff_line(line: &str) -> String {
    let bytes = line.as_bytes();
    if bytes.len() >= 7 && bytes[4] == b':' {
        match bytes[6] {
            b'+' => return format!("{ANSI_GREEN}{line}{ANSI_RESET}"),
            b'-' => return format!("{ANSI_RED}{line}{ANSI_RESET}"),
            _ => {}
        }
    }
    line.to_string()
}

/// `list --format pretty`
pub fn list_pretty(result: &ListResult) -> String {
    let mut out = Vec::new();
    for f in &result.files {
        let label = if f.status == "modified" {
            String::new()
        } else {
            format!(" ({})", f.status)
        };
        out.push(format!("{ANSI_CYAN}{ANSI_BOLD}{}{label}{ANSI_RESET}", f.path));
        if f.binary {
            out.push("  (binary file)".into());
        } else if f.truncated {
            out.push(format!(
                "  (truncated: {})",
                f.reason.as_deref().unwrap_or("diff too large")
            ));
        } else {
            out.extend(f.lines.iter().map(|l| colorize_diff_line(l)));
        }
        out.push(String::new());
    }
    out.push(format!(
        "--- {} file(s), {} line(s)",
        result.stats.files, result.stats.lines
    ));
    if result.stats.truncated_files > 0 {
        out.push(format!("    ({} file(s) truncated)", result.stats.truncated_files));
    }
    if result.page_token_next.is_some() {
        out.push("    (more pages available)".into());
    }
    out.join("\n")
}

/// `apply --format pretty`
pub fn apply_pretty(result: &ApplyResult) -> String {
    let mut out = Vec::new();
    for a in &result.applied {
        out.push(format!(
            "{ANSI_GREEN}Applied {} change(s) to {}{ANSI_RESET}",
            a.applied_count, a.file
        ));
        let unstaged = a.after_applying.unstaged_lines;
        if unstaged > 0 {
            out.push(format!("  {unstaged} unstaged change(s) remaining"));
        }
        if !a.after_applying.diff.is_empty() {
            out.push(String::new());
            out.push(format!(
                "{ANSI_CYAN}{ANSI_BOLD}Remaining changes in {}:{ANSI_RESET}",
                a.file
            ));
            out.extend(a.after_applying.diff.iter().map(|l| colorize_diff_line(l)));
        }
    }
    for s in &result.skipped {
        out.push(format!(
            "{ANSI_RED}Skipped {} #{}: {}{ANSI_RESET}",
            s.file, s.number, s.reason
        ));
    }
    out.push(String::new());
    out.push(format!(
        "--- {} applied, {} skipped",
        result.stats.changes_applied, result.stats.changes_skipped
    ));
    out.join("\n")
}

pub fn error_pretty(message: &str) -> String {
    format!("{ANSI_RED}Error: {message}{ANSI_RESET}")
}
