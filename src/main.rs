use std::io::Write;
use std::process::ExitCode;

use git_polite::ops::{self, PAGE_SIZE_BYTES_DEFAULT, PAGE_SIZE_FILES_DEFAULT, UNIFIED_LIST_DEFAULT};
use git_polite::{format, server};

const USAGE: &str = "\
usage: git-polite list [--paths PATH ...] [--page-token TOKEN] [--page-size-files N]
                       [--page-size-bytes N] [--unified N] [--format json|pretty]
       git-polite apply PATH LINES [--format json|pretty]
       git-polite mcp [--structured-output]

Git line-level staging.

commands:
  list   List file diffs as numbered lines with context
  apply  Stage selected change numbers (NNNN,MMMM,PPPP-QQQQ) of one file
  mcp    Run as an MCP server over stdio";

#[derive(Clone, Copy, PartialEq)]
enum Format {
    Json,
    Pretty,
}

enum Command {
    List {
        paths: Vec<String>,
        page_token: Option<String>,
        page_size_files: i64,
        page_size_bytes: i64,
        unified: i64,
        format: Format,
    },
    Apply {
        path: String,
        lines: String,
        format: Format,
    },
    Mcp {
        structured: bool,
    },
    Help,
    Version,
}

/// Splits `--opt=value` and hands out option values.
struct Args {
    items: std::collections::VecDeque<String>,
}

impl Args {
    fn next(&mut self) -> Option<String> {
        self.items.pop_front()
    }

    fn peek_is_value(&self) -> bool {
        self.items.front().is_some_and(|a| !a.starts_with("--"))
    }

    fn value(&mut self, opt: &str) -> Result<String, String> {
        if self.peek_is_value() {
            Ok(self.next().unwrap())
        } else {
            Err(format!("argument {opt}: expected one argument"))
        }
    }

    fn int(&mut self, opt: &str) -> Result<i64, String> {
        let v = self.value(opt)?;
        v.parse()
            .map_err(|_| format!("argument {opt}: invalid int value: '{v}'"))
    }

    fn format(&mut self) -> Result<Format, String> {
        match self.value("--format")?.as_str() {
            "json" => Ok(Format::Json),
            "pretty" => Ok(Format::Pretty),
            other => Err(format!(
                "argument --format: invalid choice: '{other}' (choose from 'json', 'pretty')"
            )),
        }
    }
}

fn parse(raw: Vec<String>) -> Result<Command, String> {
    let mut items = std::collections::VecDeque::new();
    for arg in raw {
        match arg.split_once('=') {
            Some((opt, value)) if opt.starts_with("--") => {
                items.push_back(opt.to_string());
                items.push_back(value.to_string());
            }
            _ => items.push_back(arg),
        }
    }
    let mut args = Args { items };

    let Some(cmd) = args.next() else {
        return Err("the following arguments are required: command".into());
    };
    match cmd.as_str() {
        "-h" | "--help" | "help" => Ok(Command::Help),
        "-V" | "--version" => Ok(Command::Version),
        "list" => {
            let mut paths = Vec::new();
            let mut page_token = None;
            let mut page_size_files = PAGE_SIZE_FILES_DEFAULT;
            let mut page_size_bytes = PAGE_SIZE_BYTES_DEFAULT;
            let mut unified = UNIFIED_LIST_DEFAULT;
            let mut format = Format::Json;
            while let Some(opt) = args.next() {
                match opt.as_str() {
                    "--paths" => {
                        while args.peek_is_value() {
                            paths.push(args.next().unwrap());
                        }
                    }
                    "--page-token" => page_token = Some(args.value(&opt)?),
                    "--page-size-files" => page_size_files = args.int(&opt)?,
                    "--page-size-bytes" => page_size_bytes = args.int(&opt)?,
                    "--unified" => unified = args.int(&opt)?,
                    "--format" => format = args.format()?,
                    "-h" | "--help" => return Ok(Command::Help),
                    other => return Err(format!("unrecognized arguments: {other}")),
                }
            }
            Ok(Command::List {
                paths,
                page_token,
                page_size_files,
                page_size_bytes,
                unified,
                format,
            })
        }
        "apply" => {
            let mut positional = Vec::new();
            let mut format = Format::Json;
            while let Some(arg) = args.next() {
                match arg.as_str() {
                    "--format" => format = args.format()?,
                    "-h" | "--help" => return Ok(Command::Help),
                    other if other.starts_with("--") => return Err(format!("unrecognized arguments: {other}")),
                    _ => positional.push(arg),
                }
            }
            match <[String; 2]>::try_from(positional) {
                Ok([path, lines]) => Ok(Command::Apply { path, lines, format }),
                Err(_) => Err("apply takes exactly two arguments: path lines".into()),
            }
        }
        "mcp" => {
            let mut structured = false;
            while let Some(opt) = args.next() {
                match opt.as_str() {
                    "--structured-output" => structured = true,
                    "-h" | "--help" => return Ok(Command::Help),
                    other => return Err(format!("unrecognized arguments: {other}")),
                }
            }
            Ok(Command::Mcp { structured })
        }
        other => Err(format!(
            "argument command: invalid choice: '{other}' (choose from 'list', 'apply', 'mcp')"
        )),
    }
}

fn print_json<T: serde::Serialize>(value: &T) {
    let mut out = std::io::stdout().lock();
    let _ = serde_json::to_writer(&mut out, value);
    let _ = writeln!(out);
}

fn run(cmd: Command) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    match cmd {
        Command::Help => println!("{USAGE}"),
        Command::Version => println!("git-polite {}", env!("CARGO_PKG_VERSION")),
        Command::List {
            paths,
            page_token,
            page_size_files,
            page_size_bytes,
            unified,
            format,
        } => {
            let result = ops::list_files(&paths, page_token.as_deref(), page_size_files, page_size_bytes, unified)?;
            match format {
                Format::Json => print_json(&result),
                Format::Pretty => println!("{}", format::list_pretty(&result)),
            }
        }
        Command::Apply { path, lines, format } => {
            let numbers = match ops::parse_number_tokens(&lines) {
                Ok(numbers) => numbers,
                Err(e) => {
                    match format {
                        Format::Json => eprintln!("{}", serde_json::json!({ "error": e })),
                        Format::Pretty => eprintln!("{}", format::error_pretty(&e)),
                    }
                    std::process::exit(2);
                }
            };
            let result = ops::apply_one_file(&path, &numbers)?;
            match format {
                Format::Json => print_json(&result),
                Format::Pretty => println!("{}", format::apply_pretty(&result)),
            }
        }
        Command::Mcp { structured } => server::run(structured)?,
    }
    Ok(())
}

fn main() -> ExitCode {
    let raw: Vec<String> = std::env::args_os()
        .skip(1)
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let cmd = match parse(raw) {
        Ok(cmd) => cmd,
        Err(msg) => {
            eprintln!("{USAGE}\n\ngit-polite: error: {msg}");
            return ExitCode::from(2);
        }
    };
    match run(cmd) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("git-polite: error: {e}");
            ExitCode::FAILURE
        }
    }
}
