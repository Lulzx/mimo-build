// Subagents: the `task` tool spawns a child Agent session with its own context window
// and a role-specific system prompt. Definitions come from a built-in registry plus
// markdown frontmatter files in ~/.mimo/bundled/agents and ./.mimo/agents (same format
// as the real CLI). ${{ tools.by_kind.* }} templates are resolved to this build's tools.

use crate::agent::Agent;
use crate::config::{mimo_home, Config};
use crate::event::Emitter;

#[derive(Clone, Debug)]
pub struct AgentDef {
    pub name: String,
    pub description: String,
    pub system_prompt: String,
    pub read_only: bool,
    pub model: Option<String>,
}

pub const MAX_DEPTH: u32 = 2;

/// All available subagent types: built-ins overlaid with any on-disk definitions.
pub fn registry() -> Vec<AgentDef> {
    let mut defs = builtin();
    for dir in [mimo_home().join("bundled/agents"), std::path::PathBuf::from(".mimo/agents")] {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for e in entries.flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) != Some("md") {
                continue;
            }
            if let Ok(text) = std::fs::read_to_string(&p) {
                if let Some(def) = parse_md(&text) {
                    // On-disk definitions override built-ins of the same name.
                    defs.retain(|d| d.name != def.name);
                    defs.push(def);
                }
            }
        }
    }
    defs
}

pub fn find(name: &str) -> Option<AgentDef> {
    registry().into_iter().find(|d| d.name == name)
}

/// Run a subagent task to completion; returns its final text (the "summary").
pub async fn run_task(
    parent: &Config,
    agent_type: &str,
    prompt: &str,
    depth: u32,
    emitter: &Emitter,
) -> String {
    if depth >= MAX_DEPTH {
        return "error: subagent recursion depth exceeded".to_string();
    }
    let Some(def) = find(agent_type) else {
        return format!("error: unknown subagent_type '{agent_type}'");
    };

    let mut cfg = parent.clone();
    cfg.always_approve = true; // subagents are non-interactive
    cfg.plan_mode = false;
    cfg.subagent_depth = depth + 1;
    if let Some(m) = &def.model {
        cfg.model = m.clone();
    }
    if def.read_only {
        for t in ["write", "search_replace"] {
            if !cfg.disallowed_tools.iter().any(|x| x == t) {
                cfg.disallowed_tools.push(t.to_string());
            }
        }
    }

    emitter.info(&format!("┌─ subagent[{agent_type}] ▸ {}", first_line(prompt)));
    let mut child = Agent::with_system(cfg, def.system_prompt.clone(), emitter.clone());
    let out = Box::pin(child.run_turn(prompt)).await.unwrap_or_default();
    emitter.info(&format!("└─ subagent[{agent_type}] done"));
    out
}

fn first_line(s: &str) -> String {
    let l = s.lines().next().unwrap_or("").trim();
    if l.len() > 80 {
        format!("{}…", &l[..80])
    } else {
        l.to_string()
    }
}

/// Resolve the real CLI's prompt templating to this build's concrete tool names.
pub fn resolve_templates(s: &str) -> String {
    s.replace("${{ tools.by_kind.execute }}", "run_terminal_command")
        .replace("${{ tools.by_kind.list }}", "list_dir")
        .replace("${{ tools.by_kind.search }}", "grep")
        .replace("${{ tools.by_kind.read }}", "read_file")
        .replace("${{ tools.by_kind.write }}", "write")
        .replace("${{ tools.by_kind.edit }}", "search_replace")
        .replace("${{ tools.by_kind.task }}", "spawn_subagent")
        .replace("${{ tools.by_kind.plan }}", "todo_write")
        .replace("${{ tools.by_kind.web_search }}", "web_search")
        .replace("${{ tools.by_kind.web_fetch }}", "web_fetch")
}

/// Parse a `--- frontmatter --- body` agent definition file.
fn parse_md(text: &str) -> Option<AgentDef> {
    let rest = text.strip_prefix("---")?;
    let end = rest.find("\n---")?;
    let front = &rest[..end];
    let body = rest[end + 4..].trim_start_matches('\n');

    let mut name = String::new();
    let mut description = String::new();
    let mut read_only = false;
    let mut model = None;
    for line in front.lines() {
        let line = line.trim_end();
        if let Some(v) = line.strip_prefix("name:") {
            name = v.trim().to_string();
        } else if let Some(v) = line.strip_prefix("description:") {
            let v = v.trim();
            if v != ">" && !v.is_empty() {
                description = v.to_string();
            }
        } else if let Some(v) = line.strip_prefix("permission_mode:") {
            read_only = v.trim() == "plan";
        } else if let Some(v) = line.strip_prefix("model:") {
            let v = v.trim();
            if v != "inherit" && !v.is_empty() {
                model = Some(v.to_string());
            }
        }
    }
    if name.is_empty() {
        return None;
    }
    if description.is_empty() {
        description = format!("{name} subagent");
    }
    Some(AgentDef {
        name,
        description,
        system_prompt: resolve_templates(body.trim()),
        read_only,
        model,
    })
}

fn builtin() -> Vec<AgentDef> {
    vec![
        AgentDef {
            name: "explore".into(),
            description: "Fast, read-only codebase exploration. Find files by pattern, search code, answer questions about the codebase. Specify thoroughness: quick | medium | very thorough.".into(),
            read_only: true,
            model: None,
            system_prompt: resolve_templates(
                "You are a fast, read-only codebase exploration agent.\n\n\
                 === READ-ONLY MODE ===\nYou have NO file editing tools. Use ${{ tools.by_kind.execute }} only for read-only commands.\n\n\
                 - Use ${{ tools.by_kind.list }} for file patterns, ${{ tools.by_kind.search }} for content, ${{ tools.by_kind.read }} for known paths.\n\
                 - Start broad, narrow down. Issue independent searches in parallel.\n\
                 - Return absolute file paths and relevant code snippets in your final response.",
            ),
        },
        AgentDef {
            name: "plan".into(),
            description: "Read-only software architect. Returns a step-by-step implementation plan and the critical files.".into(),
            read_only: true,
            model: None,
            system_prompt: resolve_templates(
                "You are a read-only software architect. Explore the codebase and design an implementation plan.\n\n\
                 Process: 1) Understand requirements. 2) Explore with ${{ tools.by_kind.list }}/${{ tools.by_kind.search }}/${{ tools.by_kind.read }}. 3) Design, considering trade-offs and existing patterns. 4) Detail a step-by-step strategy.\n\n\
                 End your response with:\n### Critical Files for Implementation\n- path/to/file - [reason]",
            ),
        },
        AgentDef {
            name: "general-purpose".into(),
            description: "General-purpose agent for researching complex questions, searching code, and executing multi-step tasks. Has full read/write/execute and can spawn child agents.".into(),
            read_only: false,
            model: None,
            system_prompt: resolve_templates(
                "Complete the assigned task directly. Do what was asked; nothing more, nothing less. Respond with a detailed writeup when done.\n\n\
                 - Use ${{ tools.by_kind.search }}/${{ tools.by_kind.list }} for broad searches; ${{ tools.by_kind.read }} for known paths.\n\
                 - NEVER create files unless necessary. NEVER create *.md docs unless explicitly requested.\n\
                 - Make the smallest change that solves the problem; follow existing patterns.\n\
                 - Return absolute file paths and relevant snippets in your final response.",
            ),
        },
    ]
}
