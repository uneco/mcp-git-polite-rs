//! Split linear history into parallel branches without touching the working
//! tree: each commit is replayed with `merge-tree --write-tree` and
//! `commit-tree`, then the branch ref is created with `update-ref`.

use serde::Serialize;

use crate::git::{Git, GitError};

#[derive(Debug, Serialize)]
pub struct UnstackResult {
    pub created_branches: Vec<CreatedBranch>,
    pub errors: Vec<UnstackError>,
    pub stats: UnstackStats,
}

#[derive(Debug, Serialize)]
pub struct CreatedBranch {
    pub name: String,
    pub commits_applied: Vec<AppliedCommit>,
    pub head_sha: String,
}

#[derive(Debug, Serialize)]
pub struct AppliedCommit {
    #[serde(rename = "ref")]
    pub reference: String,
    pub original_sha: String,
    pub new_sha: String,
}

#[derive(Debug, Serialize)]
pub struct UnstackError {
    pub branch: Option<String>,
    pub commit: Option<String>,
    pub error: String,
}

#[derive(Debug, Serialize)]
pub struct UnstackStats {
    pub total_branches: usize,
    pub successful_branches: usize,
    pub failed_branches: usize,
}

fn resolve_commit(reference: &str) -> Result<String, GitError> {
    Git::new([
        "rev-parse",
        "--verify",
        "--end-of-options",
        &format!("{reference}^{{commit}}"),
    ])
    .run_string()
}

fn is_object_id(s: &str) -> bool {
    matches!(s.len(), 40 | 64) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Replay `commit` onto `onto`, returning the new commit's id.
fn cherry_pick(commit: &str, onto: &str) -> Result<String, String> {
    let fail = |e: GitError| format!("Failed to apply commit: {e}");
    let original_parent = Git::new(["rev-parse", "--verify", "--end-of-options", &format!("{commit}^")])
        .run_string()
        .map_err(fail)?;

    let merge = Git::new([
        "merge-tree",
        "--write-tree",
        &format!("--merge-base={original_parent}"),
        onto,
        commit,
    ])
    .output()
    .map_err(fail)?;
    let tree = match merge.code {
        Some(0) => {
            let stdout = merge.stdout_string();
            let first = stdout.lines().next().unwrap_or("").trim().to_string();
            if !is_object_id(&first) {
                return Err(format!("Unexpected merge-tree output: {first}"));
            }
            first
        }
        Some(1) => return Err("Merge conflict detected during cherry-pick".into()),
        _ => {
            return Err(format!(
                "Failed to apply commit: {} (merge-tree --write-tree --merge-base needs Git 2.40 or later)",
                merge.stderr.trim()
            ));
        }
    };

    let meta = Git::new([
        "log",
        "-1",
        "--no-show-signature",
        "--format=%an%x00%ae%x00%aI%x00%B",
        commit,
    ])
    .run()
    .map_err(fail)?;
    let meta = String::from_utf8_lossy(&meta);
    let mut fields = meta.splitn(4, '\0');
    let mut next = || fields.next().unwrap_or("").trim().to_string();
    let (name, email, date, message) = (next(), next(), next(), next());

    // `-m msg` would store "msg\n"; feeding stdin keeps that while avoiding
    // command-line length and quoting limits.
    let message = if message.is_empty() { message } else { message + "\n" };
    Git::new(["commit-tree", &tree, "-p", onto])
        .stdin(message.as_bytes())
        .env("GIT_AUTHOR_NAME", name)
        .env("GIT_AUTHOR_EMAIL", email)
        .env("GIT_AUTHOR_DATE", date)
        .run_string()
        .map_err(fail)
}

/// Create one branch per entry, each starting at `parent` and containing the
/// listed commits replayed in order. Existing branches are never touched.
pub fn do_unstack(branches: &[(String, Vec<String>)], parent: &str) -> UnstackResult {
    let total = branches.len();
    let parent_sha = match resolve_commit(parent) {
        Ok(sha) => sha,
        Err(e) => {
            return UnstackResult {
                created_branches: Vec::new(),
                errors: vec![UnstackError {
                    branch: None,
                    commit: Some(parent.to_string()),
                    error: format!("Failed to resolve parent '{parent}': {e}"),
                }],
                stats: UnstackStats {
                    total_branches: total,
                    successful_branches: 0,
                    failed_branches: total,
                },
            };
        }
    };

    let mut created = Vec::new();
    let mut errors = Vec::new();
    'branches: for (name, commits) in branches {
        let exists = Git::new(["rev-parse", "--verify", "--quiet", &format!("refs/heads/{name}")])
            .output()
            .is_ok_and(|o| o.success());
        if exists {
            errors.push(UnstackError {
                branch: Some(name.clone()),
                commit: None,
                error: format!("Branch '{name}' already exists"),
            });
            continue;
        }

        let mut head = parent_sha.clone();
        let mut applied = Vec::new();
        for reference in commits {
            let result = resolve_commit(reference)
                .map_err(|e| format!("Failed to apply commit: {e}"))
                .and_then(|sha| cherry_pick(&sha, &head).map(|new| (sha, new)));
            match result {
                Ok((original_sha, new_sha)) => {
                    head = new_sha.clone();
                    applied.push(AppliedCommit {
                        reference: reference.clone(),
                        original_sha,
                        new_sha,
                    });
                }
                Err(error) => {
                    errors.push(UnstackError {
                        branch: Some(name.clone()),
                        commit: Some(reference.clone()),
                        error,
                    });
                    continue 'branches;
                }
            }
        }

        // The empty old value makes update-ref refuse if the branch appeared
        // in the meantime.
        match Git::new(["update-ref", &format!("refs/heads/{name}"), &head, ""]).run() {
            Ok(_) => created.push(CreatedBranch {
                name: name.clone(),
                commits_applied: applied,
                head_sha: head,
            }),
            Err(e) => errors.push(UnstackError {
                branch: Some(name.clone()),
                commit: None,
                error: format!("Failed to create branch ref: {e}"),
            }),
        }
    }

    UnstackResult {
        stats: UnstackStats {
            total_branches: total,
            successful_branches: created.len(),
            failed_branches: errors.len(),
        },
        created_branches: created,
        errors,
    }
}
