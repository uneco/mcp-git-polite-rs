//! Thin wrapper around the `git` CLI.
//!
//! All I/O is done in bytes: file content is never decoded, so CRLF line
//! endings and non-UTF-8 text survive a round trip through the index.

use std::fmt;
use std::io::Write;
use std::process::{Command, Stdio};

#[derive(Debug, Clone)]
pub struct GitError {
    pub args: Vec<String>,
    pub code: Option<i32>,
    pub stderr: String,
}

impl fmt::Display for GitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let stderr = self.stderr.trim();
        if !stderr.is_empty() {
            return f.write_str(stderr);
        }
        match self.code {
            Some(code) => write!(f, "git {} exited with status {code}", self.args.join(" ")),
            None => write!(f, "git {} was terminated by a signal", self.args.join(" ")),
        }
    }
}

impl std::error::Error for GitError {}

pub struct Output {
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: String,
}

impl Output {
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }

    pub fn stdout_string(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }
}

/// A single git invocation. Build with [`Git::new`], then call
/// [`Git::run`] (fails on non-zero exit) or [`Git::output`] (never fails on
/// exit status).
pub struct Git<'a> {
    args: Vec<String>,
    input: Option<&'a [u8]>,
    envs: Vec<(&'a str, String)>,
}

impl<'a> Git<'a> {
    pub fn new<I, S>(args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Git {
            args: args.into_iter().map(Into::into).collect(),
            input: None,
            envs: Vec::new(),
        }
    }

    pub fn stdin(mut self, input: &'a [u8]) -> Self {
        self.input = Some(input);
        self
    }

    pub fn env(mut self, key: &'a str, value: impl Into<String>) -> Self {
        self.envs.push((key, value.into()));
        self
    }

    pub fn output(&self) -> Result<Output, GitError> {
        let mut cmd = Command::new("git");
        // Paths come back raw instead of C-quoted; the diff parser still
        // unquotes the few that git quotes regardless (tabs, quotes, ...).
        cmd.args(["-c", "core.quotePath=false"]);
        // `git diff` otherwise rewrites the index to refresh stat info, taking
        // index.lock. With many agents in one repository that makes another
        // agent's `update-index` fail, so keep read-only calls lock-free.
        cmd.args(["-c", "diff.autoRefreshIndex=false"]);
        cmd.args(&self.args);
        if std::env::var_os("LC_ALL").is_none() {
            cmd.env("LC_ALL", "C");
        }
        if std::env::var_os("LANG").is_none() {
            cmd.env("LANG", "C");
        }
        for (key, value) in &self.envs {
            cmd.env(key, value);
        }
        // stdin must never be inherited: when running as a stdio MCP server
        // on Windows, git blocks forever on the parent's stdin pipe.
        cmd.stdin(if self.input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        });
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }

        let spawn_error = |e: std::io::Error| GitError {
            args: self.args.clone(),
            code: None,
            stderr: format!("failed to run git: {e}"),
        };
        let mut child = cmd.spawn().map_err(spawn_error)?;
        // Feed stdin from a separate thread so a child that writes a lot
        // before draining its input cannot deadlock against us.
        let writer = match (self.input, child.stdin.take()) {
            (Some(input), Some(mut stdin)) => {
                let input = input.to_vec();
                Some(std::thread::spawn(move || stdin.write_all(&input)))
            }
            _ => None,
        };
        let out = child.wait_with_output().map_err(spawn_error)?;
        if let Some(writer) = writer {
            // A write error (e.g. git exited early) shows up in the exit code.
            let _ = writer.join();
        }
        Ok(Output {
            code: out.status.code(),
            stdout: out.stdout,
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }

    pub fn run(&self) -> Result<Vec<u8>, GitError> {
        let out = self.output()?;
        if out.success() {
            Ok(out.stdout)
        } else {
            Err(GitError {
                args: self.args.clone(),
                code: out.code,
                stderr: out.stderr,
            })
        }
    }

    /// Run and return trimmed stdout as a string.
    pub fn run_string(&self) -> Result<String, GitError> {
        Ok(String::from_utf8_lossy(&self.run()?).trim().to_string())
    }
}

/// Split NUL-separated output (from `-z` commands) into paths.
pub fn split_nul(bytes: &[u8]) -> Vec<String> {
    bytes
        .split(|&b| b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect()
}
