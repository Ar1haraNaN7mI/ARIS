//! Builtin filesystem, search, shell, web, and todo tools protected by PathGuard.

use aris_fs::FsService;
use aris_tools::{ToolContext, ToolDefinition, ToolError, ToolHandler, ToolRegistry};
use async_trait::async_trait;
use parking_lot::RwLock;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use tokio::process::Command;

pub fn register_builtin_tools(registry: &ToolRegistry, fs: Arc<FsService>) {
    let todos = Arc::new(TodoStore::default());
    registry.register(Arc::new(crate::report_calculate::ReportCalculate));
    registry.register(Arc::new(crate::project_checks::ProjectChecks(fs.clone())));
    registry.register(Arc::new(ReadFileTool { fs: fs.clone() }));
    registry.register(Arc::new(WriteFileTool { fs: fs.clone() }));
    registry.register(Arc::new(EditFileTool { fs: fs.clone() }));
    registry.register(Arc::new(ApplyPatchTool { fs: fs.clone() }));
    registry.register(Arc::new(ListDirTool { fs: fs.clone() }));
    registry.register(Arc::new(GlobTool { fs: fs.clone() }));
    registry.register(Arc::new(GrepTool { fs: fs.clone() }));
    registry.register(Arc::new(ShellTool));
    registry.register(Arc::new(WebFetchTool));
    registry.register(Arc::new(TodoWriteTool {
        store: todos.clone(),
    }));
    registry.register(Arc::new(TodoReadTool { store: todos }));
}

fn resolve_path(ctx: &ToolContext, path: &str) -> PathBuf {
    let p = PathBuf::from(path);
    if p.is_absolute() {
        p
    } else {
        ctx.cwd.join(p)
    }
}

fn cancelled(ctx: &ToolContext) -> bool {
    *ctx.cancel.borrow()
}

struct ReadFileTool {
    fs: Arc<FsService>,
}

#[async_trait]
impl ToolHandler for ReadFileTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::builtin(
            "read_file",
            "Read a UTF-8 text file with optional 1-based offset and line limit (default 400 lines).",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "offset": { "type": "integer", "description": "1-based start line" },
                    "limit": { "type": "integer", "description": "max lines to return" }
                },
                "required": ["path"]
            }),
        )
    }

    async fn call(&self, args: Value, ctx: &ToolContext) -> Result<String, ToolError> {
        if cancelled(ctx) {
            return Err(ToolError::Message("cancelled".into()));
        }
        let path = args
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::Message("path required".into()))?;
        let offset = args
            .get("offset")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize);
        let limit = args
            .get("limit")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize);
        let resolved = resolve_path(ctx, path);
        self.fs
            .read_range(&resolved, offset, limit)
            .map_err(|e| ToolError::Message(e.to_string()))
    }
}

struct WriteFileTool {
    fs: Arc<FsService>,
}

#[async_trait]
impl ToolHandler for WriteFileTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::builtin(
            "write_file",
            "Write a UTF-8 text file. PathGuard denies writes to core crates/Cargo/target.",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "content": { "type": "string" }
                },
                "required": ["path", "content"]
            }),
        )
    }

    async fn call(&self, args: Value, ctx: &ToolContext) -> Result<String, ToolError> {
        if cancelled(ctx) {
            return Err(ToolError::Message("cancelled".into()));
        }
        let path = args
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::Message("path required".into()))?;
        let content = args
            .get("content")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::Message("content required".into()))?;
        let resolved = resolve_path(ctx, path);
        self.fs
            .write_text(&resolved, content)
            .map_err(|e| ToolError::Message(e.to_string()))?;
        Ok(format!("wrote {}", resolved.display()))
    }
}

struct EditFileTool {
    fs: Arc<FsService>,
}

#[async_trait]
impl ToolHandler for EditFileTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::builtin(
            "edit_file",
            "Replace text in a file. PathGuard denies core kernel paths.",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "old_string": { "type": "string" },
                    "new_string": { "type": "string" },
                    "replace_all": { "type": "boolean" }
                },
                "required": ["path", "old_string", "new_string"]
            }),
        )
    }

    async fn call(&self, args: Value, ctx: &ToolContext) -> Result<String, ToolError> {
        if cancelled(ctx) {
            return Err(ToolError::Message("cancelled".into()));
        }
        let path = args
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::Message("path required".into()))?;
        let old = args
            .get("old_string")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::Message("old_string required".into()))?;
        let new = args
            .get("new_string")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::Message("new_string required".into()))?;
        let replace_all = args
            .get("replace_all")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let resolved = resolve_path(ctx, path);
        self.fs
            .edit_replace(&resolved, old, new, replace_all)
            .map_err(|e| ToolError::Message(e.to_string()))
    }
}

struct ApplyPatchTool {
    fs: Arc<FsService>,
}

#[async_trait]
impl ToolHandler for ApplyPatchTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::builtin(
            "apply_patch",
            "Apply a multi-file patch. Use *** Begin Patch and *** End Patch around file headers. For Update File, use @@ and prefix context with a space, additions with +, removals with -. Add File takes +lines; Delete File takes just the header. Also accepts closed <<<<<<< SEARCH / ======= / >>>>>>> REPLACE hunks. Supply unique context for existing content.",
            json!({
                "type": "object",
                "properties": {
                    "patch": { "type": "string", "examples": [
                        "*** Begin Patch\n*** Update File: src/example.py\n@@\n-return False\n+return True\n*** End Patch"
                    ] }
                },
                "required": ["patch"]
            }),
        )
    }

    async fn call(&self, args: Value, ctx: &ToolContext) -> Result<String, ToolError> {
        if cancelled(ctx) {
            return Err(ToolError::Message("cancelled".into()));
        }
        let patch = args
            .get("patch")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::Message("patch required".into()))?;
        // Resolve relative paths inside patch against cwd by rewriting headers.
        let rewritten = rewrite_patch_paths(patch, &ctx.cwd);
        self.fs
            .apply_patch(&rewritten)
            .map_err(|e| ToolError::Message(e.to_string()))
    }
}

fn rewrite_patch_paths(patch: &str, cwd: &Path) -> String {
    let mut out = String::new();
    for line in patch.lines() {
        if let Some(rest) = line.strip_prefix("*** Update File:") {
            let p = PathBuf::from(rest.trim());
            let abs = if p.is_absolute() { p } else { cwd.join(p) };
            out.push_str(&format!("*** Update File: {}\n", abs.display()));
        } else if let Some(rest) = line.strip_prefix("*** Add File:") {
            let p = PathBuf::from(rest.trim());
            let abs = if p.is_absolute() { p } else { cwd.join(p) };
            out.push_str(&format!("*** Add File: {}\n", abs.display()));
        } else if let Some(rest) = line.strip_prefix("*** Delete File:") {
            let p = PathBuf::from(rest.trim());
            let abs = if p.is_absolute() { p } else { cwd.join(p) };
            out.push_str(&format!("*** Delete File: {}\n", abs.display()));
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

struct ListDirTool {
    fs: Arc<FsService>,
}

#[async_trait]
impl ToolHandler for ListDirTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::builtin(
            "list_dir",
            "List directory entries (non-recursive).",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" }
                },
                "required": ["path"]
            }),
        )
    }

    async fn call(&self, args: Value, ctx: &ToolContext) -> Result<String, ToolError> {
        let path = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
        let resolved = resolve_path(ctx, path);
        let entries = self
            .fs
            .list_dir(&resolved)
            .map_err(|e| ToolError::Message(e.to_string()))?;
        Ok(entries
            .into_iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join("\n"))
    }
}

struct GlobTool {
    fs: Arc<FsService>,
}

#[async_trait]
impl ToolHandler for GlobTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::builtin(
            "glob",
            "Find files under a root matching a glob pattern (e.g. **/*.rs).",
            json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string" },
                    "root": { "type": "string" },
                    "max": { "type": "integer" }
                },
                "required": ["pattern"]
            }),
        )
    }

    async fn call(&self, args: Value, ctx: &ToolContext) -> Result<String, ToolError> {
        let pattern = args
            .get("pattern")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::Message("pattern required".into()))?;
        let root = args
            .get("root")
            .and_then(|v| v.as_str())
            .map(|p| resolve_path(ctx, p))
            .unwrap_or_else(|| ctx.cwd.clone());
        let max = args.get("max").and_then(|v| v.as_u64()).unwrap_or(200) as usize;
        let hits = self
            .fs
            .glob(&root, pattern, max)
            .map_err(|e| ToolError::Message(e.to_string()))?;
        if hits.is_empty() {
            return Ok("(no files)".into());
        }
        Ok(hits
            .into_iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join("\n"))
    }
}

struct GrepTool {
    fs: Arc<FsService>,
}

#[async_trait]
impl ToolHandler for GrepTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::builtin(
            "grep",
            "Search UTF-8 source files in stable path order. Matches use path:line:text. Context uses path-line-text. Paths are relative to root or the name of a single searched file. Directory scans skip .git, .aris, node_modules and target; set root explicitly to search inside them.",
            json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string" },
                    "root": { "type": "string", "description": "File or directory; defaults to the workspace cwd." },
                    "glob": { "type": "string" },
                    "case_insensitive": { "type": "boolean" },
                    "literal": { "type": "boolean", "description": "Match the exact search text instead of interpreting a regex." },
                    "context_lines": { "type": "integer", "minimum": 0, "description": "Number of surrounding lines on each side of a match." },
                    "offset": { "type": "integer", "minimum": 0, "description": "Number of matching lines to skip. Continue with the next offset shown in the result." },
                    "max_hits": { "type": "integer", "minimum": 0, "description": "Matching lines per page (default 80). Context lines do not count." }
                },
                "required": ["pattern"]
            }),
        )
    }

    async fn call(&self, args: Value, ctx: &ToolContext) -> Result<String, ToolError> {
        #[derive(serde::Deserialize)]
        struct Args {
            pattern: String,
            root: Option<String>,
            glob: Option<String>,
            case_insensitive: Option<bool>,
            literal: Option<bool>,
            context_lines: Option<usize>,
            offset: Option<usize>,
            max_hits: Option<usize>,
        }
        let args: Args =
            serde_json::from_value(args).map_err(|e| ToolError::Message(e.to_string()))?;
        let root = resolve_path(ctx, args.root.as_deref().unwrap_or("."));
        let options = aris_fs::GrepOptions {
            glob: args.glob.as_deref(),
            case_insensitive: args.case_insensitive.unwrap_or_default(),
            literal: args.literal.unwrap_or_default(),
            context_lines: args.context_lines.unwrap_or_default(),
            offset: args.offset.unwrap_or_default(),
            max_hits: args
                .max_hits
                .unwrap_or(aris_fs::GrepOptions::default().max_hits),
        };
        self.fs
            .grep_with_options(&root, &args.pattern, options)
            .map_err(|e| ToolError::Message(e.to_string()))
    }
}

struct ShellTool;

#[async_trait]
impl ToolHandler for ShellTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::builtin(
            "shell",
            "Run a shell command in the workspace cwd. Honors cancel. Do not mutate core crates.",
            json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string" }
                },
                "required": ["command"]
            }),
        )
    }

    async fn call(&self, args: Value, ctx: &ToolContext) -> Result<String, ToolError> {
        if cancelled(ctx) {
            return Err(ToolError::Message("cancelled".into()));
        }
        let command = args
            .get("command")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::Message("command required".into()))?;
        if looks_like_core_mutation(command) {
            return Err(ToolError::Message(
                "shell blocked: refuses mutating core crates/Cargo".into(),
            ));
        }
        run_shell_cancellable(ctx, command).await
    }
}

async fn run_shell_cancellable(ctx: &ToolContext, command: &str) -> Result<String, ToolError> {
    #[cfg(windows)]
    let child = Command::new("powershell")
        // powershell.exe otherwise flattens a native program's nonzero code to 1.
        .args([
            "-NoProfile",
            "-Command",
            &format!(
                "{command}\nif (-not $?) {{ if ($LASTEXITCODE) {{ exit $LASTEXITCODE }}; exit 1 }}"
            ),
        ])
        .current_dir(&ctx.cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| ToolError::Message(e.to_string()))?;

    #[cfg(not(windows))]
    let child = Command::new("bash")
        .args(["-lc", command])
        .current_dir(&ctx.cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| ToolError::Message(e.to_string()))?;

    let mut cancel = ctx.cancel.clone();
    let stopped = async {
        if cancel.wait_for(|value| *value).await.is_err() {
            std::future::pending::<()>().await;
        }
    };
    // Drain both pipes while the process runs. Verbose compilers and test suites
    // can fill a pipe before they exit, even when no one is watching the UI.
    let output = tokio::select! {
        biased;
        _ = stopped => return Err(ToolError::Message("shell cancelled".into())),
        result = child.wait_with_output() => result.map_err(|e| ToolError::Message(e.to_string()))?,
    };
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    if !output.stderr.is_empty() {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&String::from_utf8_lossy(&output.stderr));
    }
    text.push_str(&format!("\n[exit {}]", output.status.code().unwrap_or(-1)));
    if output.status.success() {
        Ok(text)
    } else {
        Err(ToolError::Message(text))
    }
}

fn looks_like_core_mutation(command: &str) -> bool {
    let lower = command.to_lowercase();
    let hits_core =
        lower.contains("crates/") || lower.contains("cargo.toml") || lower.contains("\\crates\\");
    let mutates = lower.contains("rm ")
        || lower.contains("del ")
        || lower.contains("remove-item")
        || lower.contains("move-item")
        || lower.contains('>')
        || lower.contains("set-content")
        || lower.contains("out-file");
    hits_core && mutates
}

struct WebFetchTool;

#[async_trait]
impl ToolHandler for WebFetchTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::builtin(
            "web_fetch",
            "Fetch a public URL and return its text, including the end of the page.",
            json!({
                "type": "object",
                "properties": {
                    "url": { "type": "string" }
                },
                "required": ["url"]
            }),
        )
    }

    async fn call(&self, args: Value, ctx: &ToolContext) -> Result<String, ToolError> {
        if cancelled(ctx) {
            return Err(ToolError::Message("cancelled".into()));
        }
        let url = args
            .get("url")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::Message("url required".into()))?;
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return Err(ToolError::Message("only http(s) URLs allowed".into()));
        }
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|e| ToolError::Message(e.to_string()))?;
        let resp = client
            .get(url)
            .header("User-Agent", "aris/0.1")
            .send()
            .await
            .map_err(|e| ToolError::Message(e.to_string()))?;
        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| ToolError::Message(e.to_string()))?;
        let stripped = strip_html_light(&text);
        Ok(format!("HTTP {status}\n{stripped}"))
    }
}

fn strip_html_light(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut in_tag = false;
    for c in input.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[derive(Default)]
struct TodoStore {
    items: RwLock<Vec<TodoItem>>,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct TodoItem {
    id: String,
    content: String,
    status: String,
}

struct TodoWriteTool {
    store: Arc<TodoStore>,
}

#[async_trait]
impl ToolHandler for TodoWriteTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::builtin(
            "todo_write",
            "Replace the in-session todo list. status: pending|in_progress|completed|cancelled.",
            json!({
                "type": "object",
                "properties": {
                    "todos": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "id": { "type": "string" },
                                "content": { "type": "string" },
                                "status": { "type": "string" }
                            },
                            "required": ["id", "content", "status"]
                        }
                    }
                },
                "required": ["todos"]
            }),
        )
    }

    async fn call(&self, args: Value, _ctx: &ToolContext) -> Result<String, ToolError> {
        let todos = args
            .get("todos")
            .cloned()
            .ok_or_else(|| ToolError::Message("todos required".into()))?;
        let items: Vec<TodoItem> =
            serde_json::from_value(todos).map_err(|e| ToolError::Message(e.to_string()))?;
        *self.store.items.write() = items;
        Ok(serde_json::to_string_pretty(&*self.store.items.read()).unwrap_or_default())
    }
}

struct TodoReadTool {
    store: Arc<TodoStore>,
}

#[async_trait]
impl ToolHandler for TodoReadTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::builtin(
            "todo_read",
            "Read the current in-session todo list.",
            json!({ "type": "object", "properties": {} }),
        )
    }

    async fn call(&self, _args: Value, _ctx: &ToolContext) -> Result<String, ToolError> {
        Ok(serde_json::to_string_pretty(&*self.store.items.read()).unwrap_or_default())
    }
}

#[cfg(test)]
mod shell_receipt_tests {
    use super::*;

    #[tokio::test]
    async fn verbose_failing_process_drains_both_pipes_and_preserves_exit_status() {
        let ctx = ToolContext {
            agent_id: "shell-test".into(),
            cwd: std::env::temp_dir(),
            outer_home: PathBuf::new(),
            workspace_outer: PathBuf::new(),
            cancel: tokio::sync::watch::channel(false).1,
        };
        #[cfg(windows)]
        let command="[Console]::Out.Write(('x' * 200000)); [Console]::Error.Write(('y' * 200000)); cmd.exe /c exit 7";
        #[cfg(not(windows))]
        let command = "printf '%0200000d' 0; printf '%0200000d' 0 >&2; exit 7";
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(20),
            run_shell_cancellable(&ctx, command),
        )
        .await
        .expect("verbose process should finish");
        let text = result.unwrap_err().to_string();
        assert!(text.len() >= 400000);
        assert!(text.ends_with("[exit 7]"));
    }
}
