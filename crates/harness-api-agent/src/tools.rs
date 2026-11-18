//! Built-in coding tools of the API agent and their ACP reporting/permissions (phase-9-parity.md §9.7).

use std::collections::HashSet;
use std::fmt::Write as _;
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::llm::ToolCall;
use crate::mcp::McpSet;
use crate::Agent;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
/// Longest tool result handed back to the model.
pub const OUTPUT_MAX: usize = 30_000;
const READ_LINES: usize = 2000;
const LINE_MAX: usize = 2000;
const READ_BYTES_MAX: u64 = 10 * 1024 * 1024;
const LIST_MAX: usize = 1000;
const SEARCH_MAX: usize = 200;
const COMMAND_DEFAULT_SECS: u64 = 120;
const COMMAND_MAX_SECS: u64 = 600;

const WRITE_TOOLS: [&str; 3] = ["write_file", "edit_file", "run_command"];

fn function(name: &str, description: &str, parameters: &Value) -> Value {
    json!({"type": "function", "function": {"name": name, "description": description, "parameters": parameters}})
}

/// Chat-API tool definitions of the built-in tools for a mode (`readOnly` hides the write tools).
pub fn definitions(mode: &str, background: bool) -> Vec<Value> {
    let path = json!({"type": "string", "description": "File path, absolute or relative to the working directory"});
    let mut out = vec![
        function(
            "read_file",
            "Read a text file. Returns numbered lines. Use offset/limit for large files; take a generous range \
             once instead of several small reads of the same file.",
            &json!({"type": "object", "properties": {
                "path": path,
                "offset": {"type": "integer", "description": "First line to read (1-based)"},
                "limit": {"type": "integer", "description": "Number of lines (default 2000)"}
            }, "required": ["path"]}),
        ),
        function(
            "list_dir",
            "List the entries of a directory (directories end with /).",
            &json!({"type": "object", "properties": {"path": {"type": "string", "description": "Directory, default the working directory"}}}),
        ),
        function(
            "search",
            "Search file contents with a regular expression (respects .gitignore). Returns path:line: text. \
             Combine alternatives in one pattern (a|b|c) rather than searching several times.",
            &json!({"type": "object", "properties": {
                "pattern": {"type": "string", "description": "Regular expression"},
                "path": {"type": "string", "description": "File or directory to search, default the working directory"},
                "glob": {"type": "string", "description": "Only files matching this glob, e.g. *.rs"},
                "case_insensitive": {"type": "boolean"}
            }, "required": ["pattern"]}),
        ),
    ];
    if mode != "readOnly" {
        out.push(function(
            "write_file",
            "Create or overwrite a file with the given content.",
            &json!({"type": "object", "properties": {"path": path, "content": {"type": "string"}}, "required": ["path", "content"]}),
        ));
        out.push(function(
            "edit_file",
            "Replace an exact string in a file. old_string must match exactly once unless replace_all is true. Read the file first.",
            &json!({"type": "object", "properties": {
                "path": path,
                "old_string": {"type": "string"},
                "new_string": {"type": "string"},
                "replace_all": {"type": "boolean"}
            }, "required": ["path", "old_string", "new_string"]}),
        ));
        out.push(function(
            "run_command",
            if cfg!(windows) {
                "Run a PowerShell command in the working directory and return its exit code and output."
            } else {
                "Run a shell command (sh -c) in the working directory and return its exit code and output."
            },
            &command_schema(background),
        ));
    }
    out.extend(crate::web::definitions());
    out
}

fn command_schema(background: bool) -> Value {
    let mut schema = json!({"type": "object", "properties": {
        "command": {"type": "string"},
        "timeout_secs": {"type": "integer", "description": "Default 120, at most 600"}
    }, "required": ["command"]});
    if background {
        schema["properties"]["background"] = json!({
            "type": "boolean",
            "description": "Run without waiting (servers, long builds, watchers). Returns at once; the result arrives in a new turn when the command ends. No timeout."
        });
    }
    schema
}

pub fn shell(command: &str) -> tokio::process::Command {
    #[cfg_attr(not(windows), allow(unused_mut))]
    let mut process = if cfg!(windows) {
        let mut p = tokio::process::Command::new("powershell.exe");
        p.args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &format!("[Console]::OutputEncoding=[Text.Encoding]::UTF8; {command}"),
        ]);
        p
    } else {
        let mut p = tokio::process::Command::new("sh");
        p.args(["-c", command]);
        p
    };
    #[cfg(windows)]
    process.creation_flags(CREATE_NO_WINDOW);
    process
}

/// Per-call context.
pub struct Ctx<'a> {
    pub agent: &'a Agent,
    pub session: &'a str,
    pub cwd: &'a Path,
    pub mode: &'a str,
    pub always: &'a mut HashSet<String>,
    pub token: &'a CancellationToken,
    pub mcp: &'a McpSet,
    pub images: &'a std::sync::Mutex<Vec<Value>>,
}

const IMAGE_BYTES_MAX: usize = 5 * 1024 * 1024;

fn image_mime(path: &Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    match ext.as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        _ => None,
    }
}

/// Lexical normalisation (no filesystem access, works for files that do not exist yet).
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

fn resolve(cwd: &Path, path: &str) -> PathBuf {
    let path = Path::new(path.trim());
    if path.is_absolute() {
        normalize(path)
    } else {
        normalize(&cwd.join(path))
    }
}

pub fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let head: String = text.chars().take(max / 2).collect();
    let tail: String = {
        let rev: Vec<char> = text.chars().rev().take(max / 2).collect();
        rev.into_iter().rev().collect()
    };
    format!("{head}\n… [output truncated] …\n{tail}")
}

struct Outcome {
    text: String,
    ok: bool,
    content: Vec<Value>,
}

impl Outcome {
    fn ok(text: String) -> Self {
        Self {
            content: vec![
                json!({"type": "content", "content": {"type": "text", "text": truncate(&text, 4000)}}),
            ],
            text,
            ok: true,
        }
    }

    fn err(text: impl Into<String>) -> Self {
        let text = text.into();
        Self {
            content: vec![json!({"type": "content", "content": {"type": "text", "text": text}})],
            text: format!("Error: {text}"),
            ok: false,
        }
    }
}

fn str_arg<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}

fn title_of(name: &str, args: &Value) -> (String, &'static str) {
    let path = str_arg(args, "path").unwrap_or(".");
    match name {
        "read_file" => (format!("Read {path}"), "read"),
        "list_dir" => (format!("List {path}"), "read"),
        "search" => (
            format!("Search {}", str_arg(args, "pattern").unwrap_or_default()),
            "search",
        ),
        "write_file" => (format!("Write {path}"), "edit"),
        "edit_file" => (format!("Edit {path}"), "edit"),
        "run_command" => (
            format!(
                "Run {}",
                str_arg(args, "command")
                    .unwrap_or_default()
                    .chars()
                    .take(120)
                    .collect::<String>()
            ),
            "execute",
        ),
        other if crate::web::handles(other) => (crate::web::title(other, args), "fetch"),
        other => (other.to_owned(), "other"),
    }
}

impl Ctx<'_> {
    fn update(&self, value: &Value) {
        self.agent.update(self.session, value);
    }

    fn needs_permission(&self, name: &str, args: &Value) -> bool {
        if !WRITE_TOOLS.contains(&name) || self.always.contains(name) {
            return false;
        }
        if name == "run_command" {
            return true;
        }
        let inside = str_arg(args, "path")
            .is_some_and(|p| resolve(self.cwd, p).starts_with(normalize(self.cwd)));
        !(self.mode == "acceptEdits" && inside)
    }

    /// Asks through ACP; `true` when allowed.
    async fn ask(&mut self, name: &str, tool: &Value) -> bool {
        let response = self
            .agent
            .request(
                "session/request_permission",
                json!({
                    "sessionId": self.session,
                    "toolCall": tool,
                    "options": [
                        {"optionId": "allow", "name": "Allow once", "kind": "allow_once"},
                        {"optionId": "allow-always", "name": "Always allow", "kind": "allow_always"},
                        {"optionId": "deny", "name": "Deny", "kind": "reject_once"}
                    ]
                }),
            )
            .await;
        let outcome = response.get("outcome").cloned().unwrap_or(Value::Null);
        if outcome.get("outcome").and_then(Value::as_str) != Some("selected") {
            return false;
        }
        match outcome.get("optionId").and_then(Value::as_str) {
            Some("allow") => true,
            Some("allow-always") => {
                self.always.insert(name.to_owned());
                true
            }
            _ => false,
        }
    }

    /// Runs one model tool call with ACP reporting; returns the text for the `tool` message.
    pub async fn run(&mut self, call: &ToolCall) -> String {
        let args: Value = if call.arguments.trim().is_empty() {
            json!({})
        } else {
            match serde_json::from_str(&call.arguments) {
                Ok(args) => args,
                Err(e) => return format!("Error: arguments are not valid JSON ({e})"),
            }
        };
        let (title, kind) = title_of(&call.name, &args);
        let id = format!("{}-{}", call.id, uuid::Uuid::new_v4().simple());
        let mut tool = json!({"toolCallId": id, "title": title, "kind": kind, "status": "pending", "rawInput": args});
        if let Some(path) = str_arg(&args, "path") {
            tool["locations"] = json!([{"path": resolve(self.cwd, path).to_string_lossy()}]);
        }
        let mut update = tool.clone();
        update["sessionUpdate"] = json!("tool_call");
        self.update(&update);
        let outcome = if self.mode == "readOnly" && WRITE_TOOLS.contains(&call.name.as_str()) {
            Outcome::err("refused: the session is in read-only mode")
        } else if self.needs_permission(&call.name, &args) && !self.ask(&call.name, &tool).await {
            Outcome::err("the user denied this action")
        } else {
            self.update(&json!({"sessionUpdate": "tool_call_update", "toolCallId": id, "status": "in_progress"}));
            self.execute(&call.name, &args).await
        };
        self.update(&json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": id,
            "status": if outcome.ok { "completed" } else { "failed" },
            "content": outcome.content
        }));
        truncate(&outcome.text, OUTPUT_MAX)
    }

    async fn execute(&self, name: &str, args: &Value) -> Outcome {
        match name {
            "read_file" => self.read_file(args).await,
            "list_dir" => self.list_dir(args).await,
            "search" => self.search(args).await,
            "write_file" => self.write_file(args).await,
            "edit_file" => self.edit_file(args).await,
            "run_command" => self.run_command(args).await,
            other if crate::web::handles(other) => {
                match crate::web::run(&self.agent.http, other, args).await {
                    Ok(text) => Outcome::ok(text),
                    Err(text) => Outcome::err(text),
                }
            }
            other if McpSet::handles(other) => match self.mcp.call(other, args.clone()).await {
                Ok(text) => Outcome::ok(text),
                Err(text) => Outcome::err(text),
            },
            other => Outcome::err(format!("unknown tool {other}")),
        }
    }

    fn read_image(&self, path: &Path, mime: &str, bytes: &[u8]) -> Outcome {
        use base64::Engine as _;
        if !self.agent.vision() {
            return Outcome::err("image file: this model cannot view images");
        }
        if bytes.len() > IMAGE_BYTES_MAX {
            return Outcome::err("image is larger than 5 MB");
        }
        let data = base64::engine::general_purpose::STANDARD.encode(bytes);
        let Ok(mut images) = self.images.lock() else {
            return Outcome::err("image could not be attached");
        };
        images.push(crate::llm::image_part(mime, &data));
        Outcome::ok(format!(
            "Image {} ({} bytes) is attached in the next message.",
            path.display(),
            bytes.len()
        ))
    }

    async fn read_file(&self, args: &Value) -> Outcome {
        let Some(path) = str_arg(args, "path") else {
            return Outcome::err("path is required");
        };
        let path = resolve(self.cwd, path);
        match tokio::fs::metadata(&path).await {
            Ok(meta) if meta.is_dir() => return Outcome::err("path is a directory; use list_dir"),
            Ok(meta) if meta.len() > READ_BYTES_MAX => {
                return Outcome::err("file is larger than 10 MB")
            }
            Ok(_) => {}
            Err(e) => return Outcome::err(format!("{}: {e}", path.display())),
        }
        let bytes = match tokio::fs::read(&path).await {
            Ok(bytes) => bytes,
            Err(e) => return Outcome::err(format!("{}: {e}", path.display())),
        };
        if let Some(mime) = image_mime(&path) {
            return self.read_image(&path, mime, &bytes);
        }
        if bytes.iter().take(8192).any(|b| *b == 0) {
            return Outcome::err("binary file");
        }
        let text = String::from_utf8_lossy(&bytes);
        let offset = args
            .get("offset")
            .and_then(Value::as_u64)
            .and_then(|v| usize::try_from(v).ok())
            .unwrap_or(1)
            .max(1);
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .and_then(|v| usize::try_from(v).ok())
            .unwrap_or(READ_LINES)
            .clamp(1, READ_LINES);
        let total = text.lines().count();
        let mut out = String::new();
        for (i, line) in text.lines().enumerate().skip(offset - 1).take(limit) {
            let line: String = line.chars().take(LINE_MAX).collect();
            let _ = writeln!(out, "{:>6}\t{line}", i + 1);
        }
        let last = (offset - 1 + limit).min(total);
        if last < total {
            let _ = writeln!(
                out,
                "… {} more lines (file has {total}); use offset={}",
                total - last,
                last + 1
            );
        }
        if out.is_empty() {
            out = "(empty file)".into();
        }
        let mut outcome = Outcome::ok(out);
        outcome.content = vec![
            json!({"type": "content", "content": {"type": "text", "text": format!("{} lines", last.saturating_sub(offset - 1))}}),
        ];
        outcome
    }

    async fn list_dir(&self, args: &Value) -> Outcome {
        let path = resolve(self.cwd, str_arg(args, "path").unwrap_or("."));
        let mut entries = match tokio::fs::read_dir(&path).await {
            Ok(entries) => entries,
            Err(e) => return Outcome::err(format!("{}: {e}", path.display())),
        };
        let mut names = Vec::new();
        while let Ok(Some(entry)) = entries.next_entry().await {
            let mut name = entry.file_name().to_string_lossy().into_owned();
            if entry.file_type().await.is_ok_and(|t| t.is_dir()) {
                name.push('/');
            }
            names.push(name);
        }
        names.sort();
        let more = names.len().saturating_sub(LIST_MAX);
        names.truncate(LIST_MAX);
        let mut out = names.join("\n");
        if more > 0 {
            let _ = write!(out, "\n… {more} more entries");
        }
        if out.is_empty() {
            out = "(empty directory)".into();
        }
        Outcome::ok(out)
    }

    async fn search(&self, args: &Value) -> Outcome {
        let Some(pattern) = str_arg(args, "pattern").map(str::to_owned) else {
            return Outcome::err("pattern is required");
        };
        let root = resolve(self.cwd, str_arg(args, "path").unwrap_or("."));
        let glob = str_arg(args, "glob").map(str::to_owned);
        let insensitive = args
            .get("case_insensitive")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let cwd = self.cwd.to_path_buf();
        let result = tokio::task::spawn_blocking(move || {
            search_blocking(&root, &cwd, &pattern, glob.as_deref(), insensitive)
        })
        .await;
        match result {
            Ok(Ok(text)) => Outcome::ok(text),
            Ok(Err(e)) => Outcome::err(e),
            Err(e) => Outcome::err(e.to_string()),
        }
    }

    async fn write_file(&self, args: &Value) -> Outcome {
        let (Some(path), Some(content)) = (str_arg(args, "path"), str_arg(args, "content")) else {
            return Outcome::err("path and content are required");
        };
        let path = resolve(self.cwd, path);
        let old = tokio::fs::read_to_string(&path).await.ok();
        if let Some(parent) = path.parent() {
            if let Err(e) = tokio::fs::create_dir_all(parent).await {
                return Outcome::err(format!("{}: {e}", parent.display()));
            }
        }
        if let Err(e) = tokio::fs::write(&path, content).await {
            return Outcome::err(format!("{}: {e}", path.display()));
        }
        let mut outcome = Outcome::ok(format!(
            "wrote {} ({} bytes)",
            path.display(),
            content.len()
        ));
        outcome.content = vec![
            json!({"type": "diff", "path": path.to_string_lossy(), "oldText": old, "newText": content}),
        ];
        outcome
    }

    async fn edit_file(&self, args: &Value) -> Outcome {
        let (Some(path), Some(old_string), Some(new_string)) = (
            str_arg(args, "path"),
            str_arg(args, "old_string"),
            str_arg(args, "new_string"),
        ) else {
            return Outcome::err("path, old_string and new_string are required");
        };
        if old_string.is_empty() {
            return Outcome::err("old_string must not be empty; use write_file to create a file");
        }
        let replace_all = args
            .get("replace_all")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let path = resolve(self.cwd, path);
        let old = match tokio::fs::read_to_string(&path).await {
            Ok(text) => text,
            Err(e) => return Outcome::err(format!("{}: {e}", path.display())),
        };
        let count = old.matches(old_string).count();
        if count == 0 {
            return Outcome::err("old_string was not found in the file");
        }
        if count > 1 && !replace_all {
            return Outcome::err(format!(
                "old_string was found {count} times; add more context or set replace_all"
            ));
        }
        let new = if replace_all {
            old.replace(old_string, new_string)
        } else {
            old.replacen(old_string, new_string, 1)
        };
        if let Err(e) = tokio::fs::write(&path, &new).await {
            return Outcome::err(format!("{}: {e}", path.display()));
        }
        let mut outcome = Outcome::ok(format!(
            "edited {} ({count} replacement(s))",
            path.display()
        ));
        outcome.content = vec![
            json!({"type": "diff", "path": path.to_string_lossy(), "oldText": old, "newText": new}),
        ];
        outcome
    }

    async fn run_command(&self, args: &Value) -> Outcome {
        let Some(command) = str_arg(args, "command") else {
            return Outcome::err("command is required");
        };
        let secs = args
            .get("timeout_secs")
            .and_then(Value::as_u64)
            .unwrap_or(COMMAND_DEFAULT_SECS)
            .clamp(1, COMMAND_MAX_SECS);
        if self.agent.async_tasks() && args.get("background").and_then(Value::as_bool) == Some(true)
        {
            return match crate::background::start_command(
                self.agent,
                self.session,
                self.cwd,
                command,
            ) {
                Ok(text) => Outcome::ok(text),
                Err(text) => Outcome::err(text),
            };
        }
        let mut process = shell(command);
        process
            .current_dir(self.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let child = match process.spawn() {
            Ok(child) => child,
            Err(e) => return Outcome::err(format!("cannot start the shell: {e}")),
        };
        let output = tokio::select! {
            () = self.token.cancelled() => return Outcome::err("cancelled"),
            out = tokio::time::timeout(Duration::from_secs(secs), child.wait_with_output()) => out,
        };
        let output = match output {
            Err(_) => return Outcome::err(format!("timed out after {secs} s")),
            Ok(Err(e)) => return Outcome::err(e.to_string()),
            Ok(Ok(output)) => output,
        };
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let code = output
            .status
            .code()
            .map_or_else(|| "killed".to_owned(), |c| c.to_string());
        let mut text = format!("exit code: {code}\n");
        if !stdout.trim().is_empty() {
            text.push_str(stdout.trim_end());
            text.push('\n');
        }
        if !stderr.trim().is_empty() {
            text.push_str("stderr:\n");
            text.push_str(stderr.trim_end());
        }
        let text = truncate(&text, OUTPUT_MAX);
        if output.status.success() {
            Outcome::ok(text)
        } else {
            let mut outcome = Outcome::ok(text);
            outcome.ok = false;
            outcome
        }
    }
}

fn search_blocking(
    root: &Path,
    cwd: &Path,
    pattern: &str,
    glob: Option<&str>,
    insensitive: bool,
) -> Result<String, String> {
    use grep_regex::RegexMatcherBuilder;
    use grep_searcher::sinks::UTF8;
    use grep_searcher::Searcher;

    let matcher = RegexMatcherBuilder::new()
        .case_insensitive(insensitive)
        .build(pattern)
        .map_err(|e| format!("invalid pattern: {e}"))?;
    let mut walk = ignore::WalkBuilder::new(root);
    if let Some(glob) = glob {
        let mut overrides = ignore::overrides::OverrideBuilder::new(root);
        overrides
            .add(glob)
            .map_err(|e| format!("invalid glob: {e}"))?;
        walk.overrides(
            overrides
                .build()
                .map_err(|e| format!("invalid glob: {e}"))?,
        );
    }
    let mut hits: Vec<String> = Vec::new();
    let mut searcher = Searcher::new();
    for entry in walk.build().flatten() {
        if hits.len() >= SEARCH_MAX {
            break;
        }
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let path = entry.path();
        let shown = path.strip_prefix(cwd).unwrap_or(path).display().to_string();
        let _ = searcher.search_path(
            &matcher,
            path,
            UTF8(|line, text| {
                let text: String = text.trim_end().chars().take(300).collect();
                hits.push(format!("{shown}:{line}: {text}"));
                Ok(hits.len() < SEARCH_MAX)
            }),
        );
    }
    if hits.is_empty() {
        return Ok("no matches".into());
    }
    let mut out = hits.join("\n");
    if hits.len() >= SEARCH_MAX {
        let _ = write!(out, "\n… stopped at {SEARCH_MAX} matches");
    }
    Ok(out)
}
