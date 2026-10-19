//! Test fixtures: throwaway git repositories and an MCP client that talks to
//! the real binary over stdio.

#![allow(dead_code)]

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::{Value, json};

pub const BIN: &str = env!("CARGO_BIN_EXE_git-polite");

pub struct Repo {
    pub dir: PathBuf,
}

impl Repo {
    /// An empty repository with a fixed identity and no line-ending conversion.
    pub fn new() -> Repo {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "git-polite-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let repo = Repo { dir };
        repo.git(&["init", "-q", "-b", "main"]);
        repo.git(&["config", "user.email", "test@example.com"]);
        repo.git(&["config", "user.name", "Test User"]);
        repo.git(&["config", "core.autocrlf", "false"]);
        repo.git(&["config", "commit.gpgsign", "false"]);
        repo
    }

    pub fn git_output(&self, args: &[&str]) -> std::process::Output {
        Command::new("git")
            .args(args)
            .current_dir(&self.dir)
            .stdin(Stdio::null())
            .output()
            .expect("git runs")
    }

    /// Run git and return stdout; panics on failure.
    pub fn git(&self, args: &[&str]) -> String {
        let out = self.git_output(args);
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    pub fn git_bytes(&self, args: &[&str]) -> Vec<u8> {
        let out = self.git_output(args);
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        out.stdout
    }

    pub fn write(&self, path: &str, content: impl AsRef<[u8]>) {
        let full = self.dir.join(path);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(full, content).unwrap();
    }

    pub fn append(&self, path: &str, content: &str) {
        let mut existing = std::fs::read(self.dir.join(path)).unwrap();
        existing.extend_from_slice(content.as_bytes());
        self.write(path, existing);
    }

    pub fn remove(&self, path: &str) {
        std::fs::remove_file(self.dir.join(path)).unwrap();
    }

    pub fn commit_all(&self, message: &str) {
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "-m", message]);
    }

    pub fn rev(&self, reference: &str) -> String {
        self.git(&["rev-parse", reference]).trim().to_string()
    }

    /// Staged content of `path`.
    pub fn staged(&self, path: &str) -> Vec<u8> {
        self.git_bytes(&["cat-file", "blob", &format!(":{path}")])
    }

    pub fn cli(&self, args: &[&str]) -> std::process::Output {
        Command::new(BIN)
            .args(args)
            .current_dir(&self.dir)
            .stdin(Stdio::null())
            .output()
            .expect("git-polite runs")
    }

    pub fn mcp(&self) -> Mcp {
        Mcp::start(&self.dir, &["mcp", "--structured-output"])
    }

    pub fn mcp_text(&self) -> Mcp {
        Mcp::start(&self.dir, &["mcp"])
    }

    pub fn path(&self) -> &Path {
        &self.dir
    }
}

impl Drop for Repo {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Scenario 001: a modified, an untracked and a deleted file.
pub fn repo_with_changes() -> Repo {
    let repo = Repo::new();
    repo.write("README.md", "# Test Repository\nOriginal content\n");
    repo.commit_all("Initial commit");
    repo.write("deleted.txt", "This file will be deleted\n");
    repo.commit_all("Add file to be deleted");
    repo.append("README.md", "Modified line\n");
    repo.write("new_file.txt", "New untracked file\n");
    repo.remove("deleted.txt");
    repo
}

pub struct Mcp {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl Mcp {
    pub fn start(dir: &Path, args: &[&str]) -> Mcp {
        let mut child = Command::new(BIN)
            .args(args)
            .current_dir(dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("server starts");
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let mut mcp = Mcp {
            child,
            stdin,
            stdout,
            next_id: 1,
        };
        mcp.request(
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "test", "version": "0"}
            }),
        );
        mcp.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        mcp
    }

    fn send(&mut self, msg: &Value) {
        writeln!(self.stdin, "{msg}").unwrap();
        self.stdin.flush().unwrap();
    }

    /// Send a request and return the whole response message.
    pub fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        loop {
            let mut line = String::new();
            assert!(self.stdout.read_line(&mut line).unwrap() > 0, "server closed stdout");
            let msg: Value = serde_json::from_str(&line).unwrap();
            if msg["id"] == json!(id) {
                return msg;
            }
        }
    }

    /// Call a tool and return its `result`.
    pub fn call_raw(&mut self, tool: &str, arguments: Value) -> Value {
        let resp = self.request("tools/call", json!({"name": tool, "arguments": arguments}));
        assert!(resp.get("error").is_none(), "protocol error: {resp}");
        resp["result"].clone()
    }

    /// Call a tool on a `--structured-output` server and return its structured content.
    pub fn call(&mut self, tool: &str, arguments: Value) -> Value {
        let result = self.call_raw(tool, arguments);
        assert_eq!(result["isError"], json!(false), "tool error: {result}");
        result["structuredContent"].clone()
    }

    /// Call a tool on a text-mode server and return its text.
    pub fn call_text(&mut self, tool: &str, arguments: Value) -> String {
        let result = self.call_raw(tool, arguments);
        result["content"][0]["text"].as_str().unwrap().to_string()
    }
}

impl Drop for Mcp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Lines that carry a change number (`0001: + ...`).
pub fn numbered(lines: &Value) -> Vec<String> {
    lines
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l.as_str().unwrap().to_string())
        .filter(|l| l.len() >= 4 && l.as_bytes()[..4].iter().all(u8::is_ascii_digit))
        .collect()
}

pub fn number_of(line: &str) -> usize {
    line[..4].parse().unwrap()
}
