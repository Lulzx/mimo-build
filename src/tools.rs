// Built-in tools. Names + parameter schemas match the real mimo 0.2.11 tools captured
// over the wire (see ../re/capture/tools.json) so the verbatim system prompt is coherent.
// Wire shape here is Chat-Completions function-calling (works against both the proxy and
// api.x.ai); the real CLI uses the Responses API with the same tool semantics.

use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::io::Read;
use std::path::Path;
use std::process::Command;

use crate::api::ToolDef;
use crate::config::Config;
use crate::subagent::AgentDef;

/// Assemble the full tool set: filtered built-ins + plan/todo tools + (top level)
/// spawn_subagent + MCP tools.
pub fn assemble(cfg: &Config, agents: &[AgentDef], mcp_defs: Vec<ToolDef>) -> Vec<ToolDef> {
    let mut defs = builtin_defs(cfg);

    defs.push(def(
        "todo_write",
        "Create and manage a structured task list. The user sees this list live. Use for any task with 3+ steps. Each todo has content and status (pending|in_progress|completed). Mark a todo completed as soon as it is done.",
        json!({
            "type": "object",
            "properties": {
                "merge": {"type": "boolean", "description": "Merge with the existing list instead of replacing"},
                "todos": {
                    "type": "array",
                    "items": {"type": "object", "properties": {
                        "content": {"type": "string"},
                        "status": {"type": "string", "enum": ["pending", "in_progress", "completed"]}
                    }, "required": ["content", "status"]}
                }
            },
            "required": ["todos"]
        }),
    ));

    defs.push(def(
        "ask_user_question",
        "Ask the user a clarifying question when the task is genuinely ambiguous. Provide concise options when possible. Use sparingly — prefer making reasonable assumptions and proceeding.",
        json!({
            "type": "object",
            "properties": {
                "question": {"type": "string"},
                "options": {"type": "array", "items": {"type": "string"}}
            },
            "required": ["question"]
        }),
    ));

    // Background task management.
    defs.push(def(
        "get_command_or_subagent_output",
        "Retrieve the current output and status of a background task started with run_terminal_command(background:true).",
        json!({"type":"object","properties":{"task_id":{"type":"string"}},"required":["task_id"]}),
    ));
    defs.push(def(
        "wait_commands_or_subagents",
        "Wait for one or more background tasks to finish (up to timeout_ms), then return their output.",
        json!({"type":"object","properties":{
            "task_ids":{"type":"array","items":{"type":"string"}},
            "timeout_ms":{"type":"integer"}
        },"required":["task_ids"]}),
    ));
    defs.push(def(
        "kill_command_or_subagent",
        "Terminate a running background task by its task_id.",
        json!({"type":"object","properties":{"task_id":{"type":"string"}},"required":["task_id"]}),
    ));

    if cfg.plan_mode {
        defs.push(def(
            "enter_plan_mode",
            "Enter a read-only planning phase: explore with read_file/grep, then propose a plan via exit_plan_mode. Use when a task has genuine ambiguity about the right approach.",
            json!({"type":"object","properties":{}}),
        ));
        defs.push(def(
            "exit_plan_mode",
            "Exit plan mode and present your plan for user approval. Call this when you have finished planning. Provide the plan as markdown.",
            json!({
                "type": "object",
                "properties": {"plan": {"type": "string", "description": "The plan, as markdown"}},
                "required": ["plan"]
            }),
        ));
    }

    if cfg.subagents && cfg.subagent_depth == 0 && !agents.is_empty() {
        let names: Vec<&str> = agents.iter().map(|a| a.name.as_str()).collect();
        let menu = agents
            .iter()
            .map(|a| format!("- {}: {}", a.name, a.description))
            .collect::<Vec<_>>()
            .join("\n");
        defs.push(def(
            "spawn_subagent",
            &format!(
                "Launch a new agent with its own context window to handle a complex, multi-step task autonomously. Available subagent_type values:\n{menu}\n\nThe subagent returns a single final report."
            ),
            json!({
                "type": "object",
                "properties": {
                    "subagent_type": {"type": "string", "enum": names},
                    "description": {"type": "string", "description": "Short (3-5 word) task description"},
                    "prompt": {"type": "string", "description": "The full task for the subagent"}
                },
                "required": ["prompt", "description"]
            }),
        ));
    }

    defs.extend(mcp_defs);

    defs.retain(|d| {
        let name = d.function["name"].as_str().unwrap_or("");
        if cfg.disallowed_tools.iter().any(|t| t == name) {
            return false;
        }
        if let Some(allow) = &cfg.allowed_tools {
            return allow.iter().any(|t| t == name) || name.contains("__");
        }
        true
    });
    defs
}

fn builtin_defs(cfg: &Config) -> Vec<ToolDef> {
    let mut defs = vec![
        def(
            "read_file",
            "Reads a file from the local filesystem. Returns contents with inline line numbers. Use offset/limit for large files.",
            json!({
                "type": "object",
                "properties": {
                    "target_file": {"type": "string", "description": "Absolute or workspace-relative path"},
                    "offset": {"type": "integer", "description": "1-based start line"},
                    "limit": {"type": "integer", "description": "Max lines to read"}
                },
                "required": ["target_file"]
            }),
        ),
        def(
            "write",
            "Writes a file to the local filesystem, overwriting any existing file. Creates parent directories.",
            json!({
                "type": "object",
                "properties": {
                    "filePath": {"type": "string"},
                    "content": {"type": "string"}
                },
                "required": ["filePath", "content"]
            }),
        ),
        def(
            "search_replace",
            "Performs exact string replacements in files. You MUST read_file before editing. old_string must match uniquely unless replace_all is true.",
            json!({
                "type": "object",
                "properties": {
                    "file_path": {"type": "string"},
                    "old_string": {"type": "string"},
                    "new_string": {"type": "string"},
                    "replace_all": {"type": "boolean"}
                },
                "required": ["file_path", "old_string", "new_string"]
            }),
        ),
        def(
            "run_terminal_command",
            "Run a bash command and return its output. For terminal operations like git, npm, docker. Use dedicated file tools for file operations, not cat/sed/etc.",
            json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string"},
                    "timeout": {"type": "integer", "description": "Optional timeout in ms"},
                    "background": {"type": "boolean"}
                },
                "required": ["command"]
            }),
        ),
        def(
            "grep",
            "A powerful content search built on regex. ALWAYS use this for search instead of terminal grep/rg/find. Returns matching lines with file:line prefixes.",
            json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string"},
                    "path": {"type": "string", "description": "Directory or file (default: cwd)"},
                    "glob": {"type": "string", "description": "Glob filter, e.g. **/*.rs"},
                    "-i": {"type": "boolean", "description": "Case-insensitive"}
                },
                "required": ["pattern"]
            }),
        ),
        def(
            "list_dir",
            "Lists files and directories in a given path (relative to workspace root or absolute).",
            json!({
                "type": "object",
                "properties": {"target_directory": {"type": "string"}},
                "required": ["target_directory"]
            }),
        ),
    ];
    if cfg.web_search {
        defs.push(def(
            "web_search",
            "Search the web for up-to-date information, tailored for coding tasks. (Stub unless a search backend is configured.)",
            json!({
                "type": "object",
                "properties": {"query": {"type": "string"}, "allowed_domains": {"type": "array", "items": {"type": "string"}}},
                "required": ["query"]
            }),
        ));
        defs.push(def(
            "web_fetch",
            "Fetch the content of a specific URL and return it as text. WILL FAIL for authenticated/private URLs.",
            json!({
                "type": "object",
                "properties": {"url": {"type": "string"}},
                "required": ["url"]
            }),
        ));
    }
    defs
}

fn def(name: &str, desc: &str, params: Value) -> ToolDef {
    ToolDef {
        kind: "function".into(),
        function: json!({ "name": name, "description": desc, "parameters": params }),
    }
}

pub fn is_mutating(name: &str) -> bool {
    matches!(name, "write" | "search_replace" | "run_terminal_command")
}

pub fn summarize(name: &str, args: &Value) -> String {
    match name {
        "read_file" => format!("Read `{}`", args["target_file"].as_str().unwrap_or("?")),
        "write" => format!("Write `{}`", args["filePath"].as_str().unwrap_or("?")),
        "search_replace" => format!("Edit `{}`", args["file_path"].as_str().unwrap_or("?")),
        "run_terminal_command" => format!("Run: {}", args["command"].as_str().unwrap_or("?")),
        "grep" => format!("Grep `{}`", args["pattern"].as_str().unwrap_or("?")),
        "list_dir" => format!("List `{}`", args["target_directory"].as_str().unwrap_or(".")),
        "web_search" => format!("Web search: {}", args["query"].as_str().unwrap_or("?")),
        "web_fetch" => format!("Fetch: {}", args["url"].as_str().unwrap_or("?")),
        "todo_write" => "Update todos".to_string(),
        "exit_plan_mode" => "Submit plan for approval".to_string(),
        "enter_plan_mode" => "Enter plan mode".to_string(),
        "ask_user_question" => format!("Ask: {}", args["question"].as_str().unwrap_or("?")),
        "get_command_or_subagent_output" => format!("Get output: {}", args["task_id"].as_str().unwrap_or("?")),
        "wait_commands_or_subagents" => "Wait for background tasks".to_string(),
        "kill_command_or_subagent" => format!("Kill task: {}", args["task_id"].as_str().unwrap_or("?")),
        "spawn_subagent" => format!(
            "Subagent[{}]: {}",
            args["subagent_type"].as_str().unwrap_or("general-purpose"),
            args["description"].as_str().or(args["prompt"].as_str()).unwrap_or("")
        ),
        other => format!("{other}({args})"),
    }
}

pub fn execute(name: &str, args: &Value) -> Result<String> {
    match name {
        "read_file" => read_file(args),
        "write" => write_file(args),
        "search_replace" => search_replace(args),
        "run_terminal_command" => run_terminal_command(args),
        "grep" => grep(args),
        "list_dir" => list_dir(args),
        "web_search" => Ok("web_search is not wired to a backend in this build.".into()),
        other => Err(anyhow!("unknown tool: {other}")),
    }
}

fn read_file(args: &Value) -> Result<String> {
    let path = args["target_file"].as_str().ok_or_else(|| anyhow!("missing target_file"))?;
    let mut f = std::fs::File::open(path).map_err(|e| anyhow!("cannot open {path}: {e}"))?;
    let mut content = String::new();
    f.read_to_string(&mut content)?;
    let offset = args["offset"].as_u64().unwrap_or(1).max(1) as usize;
    let limit = args["limit"].as_u64().map(|l| l as usize);
    let mut out = String::new();
    for (i, line) in content.lines().enumerate() {
        let n = i + 1;
        if n < offset {
            continue;
        }
        if let Some(l) = limit {
            if n >= offset + l {
                break;
            }
        }
        out.push_str(&format!("{n}→{line}\n"));
    }
    if out.is_empty() {
        out.push_str("(empty or out-of-range)");
    }
    Ok(out)
}

fn write_file(args: &Value) -> Result<String> {
    let path = args["filePath"].as_str().ok_or_else(|| anyhow!("missing filePath"))?;
    let content = args["content"].as_str().unwrap_or("");
    if let Some(parent) = Path::new(path).parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::write(path, content)?;
    Ok(format!("Wrote {} bytes to {path}", content.len()))
}

fn search_replace(args: &Value) -> Result<String> {
    let path = args["file_path"].as_str().ok_or_else(|| anyhow!("missing file_path"))?;
    let old = args["old_string"].as_str().ok_or_else(|| anyhow!("missing old_string"))?;
    let new = args["new_string"].as_str().unwrap_or("");
    let replace_all = args["replace_all"].as_bool().unwrap_or(false);
    let content = std::fs::read_to_string(path).map_err(|e| anyhow!("cannot read {path}: {e}"))?;
    let count = content.matches(old).count();
    if count == 0 {
        return Err(anyhow!("old_string not found in {path}"));
    }
    if count > 1 && !replace_all {
        return Err(anyhow!(
            "old_string matched {count} times in {path}; pass replace_all=true or add more context"
        ));
    }
    let updated = if replace_all { content.replace(old, new) } else { content.replacen(old, new, 1) };
    std::fs::write(path, updated)?;
    Ok(format!("Edited {path} ({count} replacement(s))"))
}

fn run_terminal_command(args: &Value) -> Result<String> {
    let cmd = args["command"].as_str().ok_or_else(|| anyhow!("missing command"))?;
    let out = Command::new("bash").arg("-c").arg(cmd).output()?;
    let mut s = String::new();
    s.push_str(&String::from_utf8_lossy(&out.stdout));
    let err = String::from_utf8_lossy(&out.stderr);
    if !err.is_empty() {
        s.push_str(&err);
    }
    let code = out.status.code().unwrap_or(-1);
    if s.len() > 30_000 {
        s.truncate(30_000);
        s.push_str("\n... [truncated]");
    }
    Ok(format!("[exit code: {code}]\n{s}"))
}

fn grep(args: &Value) -> Result<String> {
    let pattern = args["pattern"].as_str().ok_or_else(|| anyhow!("missing pattern"))?;
    let ci = args["-i"].as_bool().unwrap_or(false);
    let re = regex::RegexBuilder::new(pattern)
        .case_insensitive(ci)
        .build()
        .map_err(|e| anyhow!("bad regex: {e}"))?;
    let root = args["path"].as_str().unwrap_or(".");
    let glob_filter = args["glob"].as_str().and_then(|g| glob::Pattern::new(g).ok());
    let mut out = String::new();
    let mut hits = 0;
    for entry in walkdir::WalkDir::new(root).into_iter().filter_map(|e| e.ok()) {
        if !entry.file_type().is_file() {
            continue;
        }
        let p = entry.path();
        if p.components().any(|c| {
            matches!(c.as_os_str().to_str(), Some(".git") | Some("target") | Some("node_modules"))
        }) {
            continue;
        }
        if let Some(g) = &glob_filter {
            if !g.matches_path(p) {
                continue;
            }
        }
        let Ok(text) = std::fs::read_to_string(p) else { continue };
        for (i, line) in text.lines().enumerate() {
            if re.is_match(line) {
                out.push_str(&format!("{}:{}: {}\n", p.display(), i + 1, line.trim()));
                hits += 1;
                if hits >= 200 {
                    out.push_str("... [truncated at 200 matches]\n");
                    return Ok(out);
                }
            }
        }
    }
    if out.is_empty() {
        out.push_str("(no matches)");
    }
    Ok(out)
}

fn list_dir(args: &Value) -> Result<String> {
    let path = args["target_directory"].as_str().unwrap_or(".");
    let mut entries: Vec<String> = vec![];
    for e in std::fs::read_dir(path)? {
        let e = e?;
        let name = e.file_name().to_string_lossy().to_string();
        let suffix = if e.file_type().map(|t| t.is_dir()).unwrap_or(false) { "/" } else { "" };
        entries.push(format!("{name}{suffix}"));
    }
    entries.sort();
    Ok(entries.join("\n"))
}
