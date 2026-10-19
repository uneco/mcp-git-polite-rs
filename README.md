# git-polite (Rust)

[![CI](https://img.shields.io/github/actions/workflow/status/uneco/mcp-git-polite-rs/ci.yml?branch=main)](https://github.com/uneco/mcp-git-polite-rs/actions)
[![License](https://img.shields.io/github/license/uneco/mcp-git-polite-rs)](LICENSE)

Git staging on autopilot — let AI organize your changes into clean, focused commits.

This is a Rust rewrite of [mcp-git-polite](https://github.com/uneco/mcp-git-polite). It exposes the same MCP tools with the same output, but runs as a single small binary, so dozens of agents can each keep a git-polite server open without the cost of a Python runtime per agent.

| Windows, after `initialize` + one `list_changes` | Python (`uv run`) | Rust |
|---|---|---|
| Private memory per server | 108 MB | 1.0 MB |
| Working set per server | 117.6 MB | 5.5 MB |
| Private memory, 30 servers | ~3.2 GB (estimated) | 28.9 MB (measured) |
| Time to answer `initialize` | 1.7–3.3 s | 3–99 ms |

## Features

- **Autopilot Mode**: Let AI analyze your changes and create multiple focused commits automatically
- **Line-Level Staging**: Stage individual additions and deletions by line number
- **Untracked File Support**: Stage parts of newly created files (not just modified files)
- **Range Selection**: Apply multiple changes at once using ranges (e.g., `0001-0005,0020-0025`)
- **LLM-Friendly Output**: Byte-based pagination and smart truncation protect context windows
- **Binary File Detection**: Automatically detects and skips binary files
- **Unstack**: Split linear commits into parallel branches without touching the working tree

## Installation

Download a binary for your platform from [Releases](https://github.com/uneco/mcp-git-polite-rs/releases) and put `git-polite` (`git-polite.exe` on Windows) on your `PATH`, or build it with Cargo:

```bash
cargo install --locked --git https://github.com/uneco/mcp-git-polite-rs
```

Git must be installed. `unstack` needs Git 2.40 or later (`merge-tree --write-tree --merge-base`).

## MCP Client Configuration

Start the binary directly — wrapping it in `uvx`, `npx` or `cargo run` adds a launcher process that costs more memory than the server itself.

**Claude Code:**

```bash
claude mcp add -s user git-polite -- git-polite mcp
```

**Claude Desktop / JSON configuration:**

```json
{
  "mcpServers": {
    "git-polite": {
      "command": "git-polite",
      "args": ["mcp"]
    }
  }
}
```

Use the absolute path to the binary as `command` if it is not on your `PATH`. Add `--structured-output` after `mcp` to get JSON results (`structuredContent`) instead of plain text.

**Docker:**

```json
{
  "mcpServers": {
    "git-polite": {
      "command": "docker",
      "args": [
        "run", "-i", "--rm",
        "-v", "${workspaceFolder}:/workspace",
        "-w", "/workspace",
        "ghcr.io/uneco/mcp-git-polite-rs:latest",
        "mcp"
      ]
    }
  }
}
```

## MCP Tools

1. **list_changes**: List unstaged git changes (including untracked files) as numbered lines
   - Smart truncation: diffs larger than 10KB are truncated to protect LLM context
   - Parameters:
     - `paths` (optional): List of file paths to filter
     - `page_token` (optional): Pagination token
     - `page_size_files` (optional, default: 50): Max files per page
     - `page_size_bytes` (optional, default: 30KB): Max cumulative bytes per page
     - `unified` (optional, default: 20): Context lines around changes
   - For truncated files, use the `diff` tool to view the complete content

2. **diff**: View the complete diff for a single file without truncation
   - Returns the same numbered line format as `list_changes`, enabling partial staging
   - Parameters:
     - `path` (required): File path to view diff for
     - `unified` (optional, default: 20): Context lines around changes

3. **apply_changes**: Stage selected changes to the git index by number (works for untracked files too)
   - Parameters:
     - `path`: File path to apply changes to
     - `lines`: Change numbers (format: `NNNN,MMMM,PPPP-QQQQ`)

4. **auto_commit**: Start autopilot mode to organize all changes into focused commits
   - Shows recent commit messages for style reference
   - Summarizes unstaged changes per file (statistics only, to save tokens)
   - Guides the agent through creating multiple atomic commits

5. **unstack**: Split linear commits into parallel branches for separate PRs
   - Parameters:
     - `branches`: Map of branch name to the commits to cherry-pick, in order
     - `parent` (optional, default: `origin/main`): Base commit for every branch
   - Never changes the current branch or the working tree; existing branches are never overwritten

The server also provides an `auto-commit` prompt that tells the agent to call `auto_commit`.

## How It Works

1. **List Phase**: Parses `git diff` output (and `git diff --no-index` for untracked files) and numbers each addition (`+`) and deletion (`-`) sequentially
2. **Truncation Check**: Files whose diff exceeds 10KB are marked as truncated
3. **Display**: Changes are shown with their numbers and surrounding context. Files are marked as `added` (untracked), `modified` or `deleted`
4. **Pagination**: Results are paginated by cumulative output size (default 30KB per page)
5. **Apply Phase**: Reads the staged version of the file from the index, applies only the selected changes and writes the result back to the index

When the new file ends with a newline, an extra empty change (e.g. `0003: + `) stands for that trailing newline; select it to stage the newline as well.

## Output Format

### List Output (`--structured-output`)

```json
{
  "page_token_next": "optional-token",
  "files": [
    {
      "path": "src/main.py",
      "binary": false,
      "status": "modified",
      "lines": [
        "0001: + new line 1",
        "        context line",
        "0002: - deleted line",
        "0003: + new line 2",
        "        ..."
      ]
    },
    {
      "path": "src/refactored_module.py",
      "binary": false,
      "status": "modified",
      "truncated": true,
      "reason": "diff too large (45.2 KB, max 10 KB)",
      "lines": []
    }
  ],
  "stats": {
    "files": 2,
    "lines": 5,
    "truncated_files": 1,
    "page_bytes": 4532
  }
}
```

### Apply Output (`--structured-output`)

```json
{
  "applied": [
    {
      "file": "src/main.py",
      "applied_count": 3,
      "after_applying": {
        "diff": ["0001: + remaining", "0002: - unstaged", "0003: + changes"],
        "unstaged_lines": 3
      }
    }
  ],
  "skipped": [],
  "stats": {
    "files": 1,
    "changes_applied": 3,
    "changes_skipped": 0
  }
}
```

Without `--structured-output` the same results are rendered as plain text boxes for the LLM.

## Command Line

The binary also works as a plain CLI:

```bash
git-polite list [--paths PATH ...] [--unified N] [--format json|pretty]
git-polite apply PATH 0001-0003,0007 [--format json|pretty]
```

## Differences from the Python version

Tool names, schemas, descriptions and outputs are the same. These behaviors changed:

- **CRLF files keep their line endings.** The Python version staged CRLF files with LF endings, leaving every line as an unstaged change.
- **Content is staged byte for byte.** Files that are not valid UTF-8 (e.g. Shift_JIS) can be staged; the Python version failed on them.
- **Removed `-- ` / added `++ ` lines no longer break numbering.** Such lines show up as `--- ` / `+++ ` in a diff; hunks are now read by their line counts, so they are never taken for file headers.
- **Non-ASCII and unusual file names work.** Paths are read unquoted (`core.quotePath=false`, `ls-files -z`) and C-quoted names in diff headers are decoded.
- **Diffs ignore user diff settings** that would change their shape (`diff.noprefix`, `diff.mnemonicPrefix`, textconv drivers, external diff tools).
- **Listing never writes the index.** `git diff` normally rewrites the index to refresh stat info, taking `index.lock`; with `diff.autoRefreshIndex=false`, an agent's `list_changes` can no longer make another agent's `apply_changes` in the same repository fail on `index.lock`.
- **`unstack` requires Git 2.40+.** The fallback for older Git built branches from the original commit's full tree, which was wrong; it was removed. Conflicts are detected from `merge-tree`'s exit status, commit messages are passed on stdin, and branch creation refuses to overwrite a branch that appeared concurrently.
- Change numbers above 9999 are accepted (`10000`), and unmerged files (combined diffs) are skipped instead of being mixed into the previous file.

## Development

```bash
cargo test
```

The tests create throwaway repositories and talk to the real binary over MCP, so no Docker or Python is needed. On Windows without the Visual Studio C++ build tools, use the GNU toolchain: `cargo +stable-x86_64-pc-windows-gnu test`.

## License

MIT License - See [LICENSE](LICENSE) for details.
