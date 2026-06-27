// The agent loop: stream a completion, run any tool calls, feed results back, repeat
// until the model stops or max_turns is hit. All output goes through an Emitter so the
// same loop drives stdout (headless/REPL) and the ratatui TUI.

use anyhow::Result;
use std::collections::HashMap;

use crate::api::{self, Message};
use crate::config::Config;
use crate::event::Emitter;
use crate::mcp::McpManager;
use crate::prompt;
use crate::subagent;
use crate::tools;

#[derive(Clone)]
struct PlanEntry {
    content: String,
    status: String,
}

pub struct Agent {
    pub cfg: Config,
    pub messages: Vec<Message>,
    pub id: String,
    emitter: Emitter,
    plan_approved: bool,
    recent_calls: HashMap<String, u32>,
    mcp: McpManager,
    plan: Vec<PlanEntry>,
    tasks: crate::bgtask::TaskRegistry,
}

impl Agent {
    /// Top-level agent writing to stdout (headless / inline REPL).
    pub fn new(cfg: Config) -> Self {
        Self::new_with(cfg, Emitter::Stdout)
    }

    /// Top-level agent with an explicit emitter (the TUI passes a channel). Initializes
    /// MCP and applies any --agent definition override.
    pub fn new_with(cfg: Config, emitter: Emitter) -> Self {
        let mut cfg = cfg;
        let system = match &cfg.agent_override {
            Some(name) => match subagent::find(name) {
                Some(def) => {
                    if def.read_only {
                        for t in ["write", "search_replace"] {
                            cfg.disallowed_tools.push(t.to_string());
                        }
                    }
                    if let Some(m) = &def.model {
                        cfg.model = m.clone();
                    }
                    emitter.info(&format!("agent: {} ({})", def.name, def.description));
                    def.system_prompt
                }
                None => {
                    emitter.info(&format!("unknown --agent '{name}', using default"));
                    prompt::system_prompt(&cfg)
                }
            },
            None => prompt::system_prompt(&cfg),
        };
        let mcp = McpManager::init();
        let mut messages = vec![Message::system(system), Message::user(prompt::user_info())];
        if let Some(pi) = prompt::project_instructions() {
            emitter.info(&format!("loaded project instructions ({} files)", pi.matches("<project_instructions").count()));
            messages.push(Message::user(pi));
        }
        if let Some(mem) = crate::memory::recall_context() {
            messages.push(Message::user(mem));
        }
        Agent {
            cfg,
            messages,
            id: crate::session::new_id(),
            emitter,
            plan_approved: false,
            recent_calls: HashMap::new(),
            mcp,
            plan: vec![],
            tasks: Default::default(),
        }
    }

    /// Child agent (subagent) with an explicit system prompt and no MCP; inherits the
    /// parent's emitter so its output reaches the same UI.
    pub fn with_system(cfg: Config, system_prompt: String, emitter: Emitter) -> Self {
        let user_info = prompt::user_info();
        Agent {
            cfg,
            messages: vec![Message::system(format!("{system_prompt}\n\n{user_info}"))],
            id: crate::session::new_id(),
            emitter,
            plan_approved: true,
            recent_calls: HashMap::new(),
            mcp: McpManager::default(),
            plan: vec![],
            tasks: Default::default(),
        }
    }

    /// /flush — distil this conversation into a durable memory file.
    pub async fn flush_memory(&self) -> String {
        match crate::memory::flush(&self.cfg, &self.emitter, &self.messages).await {
            Ok(s) => s,
            Err(e) => format!("flush failed: {e}"),
        }
    }

    /// /dream — consolidate/prune the memory store.
    pub async fn dream_memory(&self) -> String {
        match crate::memory::dream(&self.cfg, &self.emitter).await {
            Ok(s) => s,
            Err(e) => format!("dream failed: {e}"),
        }
    }

    pub fn load_history(&mut self, messages: Vec<Message>, id: String) {
        if !messages.is_empty() {
            self.messages = messages;
        }
        self.id = id;
    }

    /// Run one user turn to completion. Returns the final assistant text.
    pub async fn run_turn(&mut self, user_input: &str) -> Result<String> {
        self.messages.push(Message::user(format!("<user_query>\n{user_input}\n</user_query>")));
        let agents = subagent::registry();

        let mut final_text = String::new();
        for _turn in 0..self.cfg.max_turns {
            let tools = tools::assemble(&self.cfg, &agents, self.mcp.tool_defs());
            let assistant = match api::stream_chat(&self.cfg, &self.messages, tools, &self.emitter).await {
                Ok(a) => a,
                Err(e) => {
                    self.emitter.error(&format!("error: {e}"));
                    return Ok(final_text);
                }
            };

            self.messages.push(Message {
                role: "assistant".into(),
                content: if assistant.content.is_empty() { None } else { Some(assistant.content.clone()) },
                tool_calls: if assistant.tool_calls.is_empty() { None } else { Some(assistant.tool_calls.clone()) },
                tool_call_id: None,
            });
            if !assistant.content.is_empty() {
                final_text = assistant.content.clone();
            }

            if assistant.tool_calls.is_empty() {
                return Ok(final_text);
            }

            for call in assistant.tool_calls.clone() {
                let name = call.function.name.clone();
                let args: serde_json::Value =
                    serde_json::from_str(&call.function.arguments).unwrap_or(serde_json::json!({}));

                // Doom-loop guard.
                let sig = format!("{name}:{}", call.function.arguments);
                let n = self.recent_calls.entry(sig).or_insert(0);
                *n += 1;
                if *n >= 4 {
                    let warn = "DOOM LOOP DETECTED: identical tool call repeated. Stop and take a different approach.";
                    self.messages.push(Message::tool(&call.id, warn));
                    self.emitter.error(warn);
                    continue;
                }

                let summary = tools::summarize(&name, &args);

                if name == "exit_plan_mode" {
                    let result = self.handle_exit_plan(&args).await;
                    self.messages.push(Message::tool(&call.id, result));
                    continue;
                }

                // Plan-mode gating for mutating tools.
                let blocked = self.cfg.plan_mode
                    && !self.cfg.always_approve
                    && !self.plan_approved
                    && tools::is_mutating(&name)
                    && !is_markdown_edit(&name, &args);
                if blocked {
                    self.emitter.info(&format!("plan mode: {summary} (blocked — call exit_plan_mode to request approval)"));
                    self.messages.push(Message::tool(
                        &call.id,
                        "Blocked: plan mode is active. Present your plan via exit_plan_mode and wait for approval before mutating the workspace.",
                    ));
                    continue;
                }

                // Per-call approval for mutating tools (skipped when always-approve).
                if tools::is_mutating(&name) && !self.cfg.always_approve {
                    if let crate::event::Decision::Reject(feedback) =
                        self.emitter.request_approval(&summary, None).await
                    {
                        self.emitter.info("rejected");
                        let msg = match feedback {
                            Some(f) if !f.is_empty() => format!(
                                "The user rejected this tool execution with feedback: {f}"
                            ),
                            _ => "The user rejected this tool execution. Refer to their next message for guidance.".to_string(),
                        };
                        self.messages.push(Message::tool(&call.id, &msg));
                        continue;
                    }
                }

                if name != "todo_write" {
                    self.emitter.tool_start(&summary);
                }
                let result = self.exec_tool(&name, &args).await;
                // Emit an inline diff for successful edits.
                if name == "search_replace" && !result.starts_with("error") {
                    if let (Some(old), Some(new), Some(path)) = (
                        args["old_string"].as_str(),
                        args["new_string"].as_str(),
                        args["file_path"].as_str(),
                    ) {
                        let content = std::fs::read_to_string(path).ok();
                        let start = content
                            .as_ref()
                            .and_then(|c| c.find(new).map(|b| c[..b].matches('\n').count() + 1))
                            .unwrap_or(0);
                        // Up to 3 unchanged lines before the change, as context (grok-style).
                        let context: Vec<String> = match (&content, start) {
                            (Some(c), s) if s > 1 => {
                                let lines: Vec<&str> = c.lines().collect();
                                let from = (s - 1).saturating_sub(3);
                                lines[from..s - 1].iter().map(|l| l.to_string()).collect()
                            }
                            _ => vec![],
                        };
                        self.emitter.diff(start, &context, old, new);
                    }
                }
                self.emitter.tool_meta(&tool_meta(&name, &args, &result));
                self.messages.push(Message::tool(&call.id, result));
            }
        }
        self.emitter.info(&format!("(reached max-turns={})", self.cfg.max_turns));
        Ok(final_text)
    }

    async fn exec_tool(&mut self, name: &str, args: &serde_json::Value) -> String {
        match name {
            "spawn_subagent" => {
                let agent_type = args["subagent_type"].as_str().unwrap_or("general-purpose");
                let prompt = args["prompt"].as_str().unwrap_or("");
                subagent::run_task(&self.cfg, agent_type, prompt, self.cfg.subagent_depth, &self.emitter).await
            }
            "todo_write" => self.update_plan(args),
            "enter_plan_mode" => {
                self.cfg.plan_mode = true;
                self.plan_approved = false;
                "Entered plan mode (read-only). Explore, then call exit_plan_mode with your plan.".to_string()
            }
            "run_terminal_command" if args["background"].as_bool().unwrap_or(false) => {
                let cmd = args["command"].as_str().unwrap_or("");
                match self.tasks.spawn(cmd).await {
                    Ok(id) => format!("[background task started: {id}] Use get_command_or_subagent_output to read its output."),
                    Err(e) => format!("error: {e}"),
                }
            }
            "get_command_or_subagent_output" => {
                self.tasks.output(args["task_id"].as_str().unwrap_or("")).await
            }
            "wait_commands_or_subagents" => {
                let ids: Vec<String> = args["task_ids"]
                    .as_array()
                    .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                    .unwrap_or_default();
                let timeout = args["timeout_ms"].as_u64().unwrap_or(30_000);
                self.tasks.wait(&ids, timeout).await
            }
            "kill_command_or_subagent" => {
                self.tasks.kill(args["task_id"].as_str().unwrap_or("")).await
            }
            "ask_user_question" => {
                let q = args["question"].as_str().unwrap_or("");
                let opts: Vec<String> = args["options"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|o| {
                                o.as_str()
                                    .map(String::from)
                                    .or_else(|| o.get("label").and_then(|l| l.as_str()).map(String::from))
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                let answer = self.emitter.ask(q, &opts).await;
                format!("The user answered: {answer}")
            }
            "web_fetch" => web_fetch(args).await,
            "web_search" => "web_search is not wired to a search backend in this build.".to_string(),
            "update_goal" => {
                let mut result = crate::goal::dispatch(name, args).unwrap_or_default();
                // A `completed:true` only sticks if the LLM classifier confirms (mirrors the original).
                if args["completed"].as_bool().unwrap_or(false) {
                    if crate::goal::classify_completion(&self.cfg, &self.emitter, &self.messages).await {
                        result.push_str("\nClassifier confirmed: goal complete.");
                    } else {
                        result.push_str("\nClassifier did NOT confirm completion; keep working.");
                    }
                }
                result
            }
            "image_gen" | "image_edit" | "video_gen" => {
                crate::image::dispatch(name, args, &self.cfg).await.unwrap_or_else(|| format!("error: {name}"))
            }
            "scheduler_create" | "scheduler_delete" | "scheduler_list" | "monitor" => {
                crate::scheduler::dispatch(name, args, &self.emitter)
                    .await
                    .unwrap_or_else(|| format!("error: {name}"))
            }
            "memory_search" | "memory_get" => {
                crate::memory::dispatch(name, args).unwrap_or_else(|| format!("error: {name}"))
            }
            _ if self.mcp.owns(name) => match self.mcp.call(name, args) {
                Ok(r) => r,
                Err(e) => format!("error: {e}"),
            },
            _ => match tools::execute(name, args) {
                Ok(r) => r,
                Err(e) => format!("error: {e}"),
            },
        }
    }

    fn update_plan(&mut self, args: &serde_json::Value) -> String {
        if !args["merge"].as_bool().unwrap_or(false) {
            self.plan.clear();
        }
        if let Some(entries) = args["todos"].as_array() {
            for e in entries {
                self.plan.push(PlanEntry {
                    content: e["content"].as_str().unwrap_or("").to_string(),
                    status: e["status"].as_str().unwrap_or("pending").to_string(),
                });
            }
        }
        self.emitter
            .todos(self.plan.iter().map(|e| (e.content.clone(), e.status.clone())).collect());
        "Todos updated.".to_string()
    }

    async fn handle_exit_plan(&mut self, args: &serde_json::Value) -> String {
        let plan = args["plan"].as_str().unwrap_or("").to_string();
        if self.cfg.always_approve {
            self.approve_plan();
            return "Plan auto-approved. Proceed with execution.".to_string();
        }
        match self.emitter.request_approval("approve plan", Some(plan)).await {
            crate::event::Decision::Allow => {
                self.approve_plan();
                "The user approved the plan. Plan mode is now off — proceed with execution.".to_string()
            }
            crate::event::Decision::Reject(feedback) => match feedback {
                Some(f) if !f.is_empty() => format!(
                    "The user did not approve the plan. Their feedback: {f}. Revise the plan accordingly."
                ),
                _ => "The user did not approve the plan. Continue planning and ask what they would like to change.".to_string(),
            },
        }
    }

    pub fn approve_plan(&mut self) {
        self.plan_approved = true;
        self.cfg.plan_mode = false;
    }

    /// Running MCP servers as `(name, tool count)`, for the `/mcp` command.
    pub fn mcp_summary(&self) -> Vec<(String, usize)> {
        self.mcp.summary()
    }

    /// Conservatively compact history: keep the leading system/bootstrap block and the most
    /// recent `keep_tail` messages, replacing the middle with a single marker. The tail start
    /// is advanced past any `tool` message so a tool reply is never orphaned from its call.
    /// Returns the number of messages dropped (0 if nothing was compacted).
    pub fn compact(&mut self, keep_tail: usize) -> usize {
        let n = self.messages.len();
        let head = self
            .messages
            .iter()
            .take_while(|m| m.role == "system" || m.role == "user")
            .count()
            .min(2);
        if n <= head + keep_tail + 1 {
            return 0;
        }
        let mut start = n - keep_tail;
        while start < n && self.messages[start].role == "tool" {
            start += 1;
        }
        let mut compacted: Vec<Message> = self.messages[..head].to_vec();
        compacted.push(Message::user("[Earlier conversation compacted to save context.]"));
        compacted.extend_from_slice(&self.messages[start..]);
        let dropped = n - compacted.len();
        self.messages = compacted;
        dropped
    }

    /// Called when a turn is cancelled mid-flight (Ctrl+C). Keeps the message history valid by
    /// giving any dangling tool_calls from the last assistant message a synthetic response.
    pub fn note_cancelled(&mut self) {
        if let Some(ai) = self.messages.iter().rposition(|m| m.role == "assistant") {
            if let Some(calls) = self.messages[ai].tool_calls.clone() {
                let answered: std::collections::HashSet<String> = self.messages[ai + 1..]
                    .iter()
                    .filter_map(|m| m.tool_call_id.clone())
                    .collect();
                for c in calls {
                    if !answered.contains(&c.id) {
                        self.messages.push(Message::tool(&c.id, "[Tool call cancelled by the user.]"));
                    }
                }
            }
        }
    }

    pub fn reset(&mut self) {
        self.messages = vec![
            Message::system(prompt::system_prompt(&self.cfg)),
            Message::user(prompt::user_info()),
        ];
        self.plan_approved = false;
        self.plan.clear();
        self.recent_calls.clear();
        self.id = crate::session::new_id();
    }
}

/// Result metadata appended to an activity line (e.g. "12 lines", "+3 -1", "5 matches").
fn tool_meta(name: &str, args: &serde_json::Value, result: &str) -> String {
    match name {
        "read_file" => {
            let n = result.lines().filter(|l| l.contains('→')).count();
            if n > 0 { format!("{n} lines") } else { String::new() }
        }
        "grep" => {
            if result.starts_with("(no matches") {
                "no matches".into()
            } else {
                let n = result.lines().filter(|l| l.contains(':')).count();
                format!("{n} matches")
            }
        }
        "list_dir" => {
            let n = result.lines().filter(|l| !l.trim().is_empty()).count();
            format!("{n} items")
        }
        "search_replace" => {
            let add = args["new_string"].as_str().map(|s| s.lines().count().max(1)).unwrap_or(0);
            let rem = args["old_string"].as_str().map(|s| s.lines().count().max(1)).unwrap_or(0);
            format!("+{add} -{rem}")
        }
        "run_terminal_command" => result
            .strip_prefix("[exit code: ")
            .and_then(|r| r.split(']').next())
            .and_then(|c| c.trim().parse::<i32>().ok())
            .filter(|c| *c != 0)
            .map(|c| format!("exit {c}"))
            .unwrap_or_default(),
        _ => String::new(),
    }
}

fn is_markdown_edit(name: &str, args: &serde_json::Value) -> bool {
    let path = match name {
        "write" => args["filePath"].as_str(),
        "search_replace" => args["file_path"].as_str(),
        _ => None,
    };
    path.map(|p| p.ends_with(".md")).unwrap_or(false)
}

async fn web_fetch(args: &serde_json::Value) -> String {
    let Some(url) = args["url"].as_str() else { return "error: missing url".into() };
    let client = match reqwest::Client::builder().user_agent("mimo-rs/0.2.11").build() {
        Ok(c) => c,
        Err(e) => return format!("error: {e}"),
    };
    match client.get(url).send().await {
        Ok(resp) => {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            let text = strip_html(&body);
            let mut out = format!("[HTTP {status}] {url}\n{text}");
            if out.len() > 20_000 {
                out.truncate(20_000);
                out.push_str("\n... [truncated]");
            }
            out
        }
        Err(e) => format!("error fetching {url}: {e}"),
    }
}

fn strip_html(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}
