// System prompt. This is the VERBATIM prompt captured from mimo 0.2.11 over the wire
// (see ../re/capture/CAPTURE.md and ../re/capture/system_prompt.txt) — not a reconstruction.

use crate::config::Config;

/// The exact system prompt the real CLI sends (model identity: "Mimo 4.3").
const REAL_SYSTEM_PROMPT: &str = include_str!("../assets/system_prompt.txt");

pub fn system_prompt(cfg: &Config) -> String {
    if let Some(o) = &cfg.system_prompt_override {
        return o.clone();
    }
    let mut p = REAL_SYSTEM_PROMPT.to_string();
    if let Some(extra) = &cfg.extra_rules {
        p.push_str(&format!("\n\n<extra_rules>\n{extra}\n</extra_rules>"));
    }
    p
}

/// The <user_info> block, sent as a separate user message (matching the real input layout).
pub fn user_info() -> String {
    let cwd = std::env::current_dir().unwrap_or_default();
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
    format!(
        "<user_info>\nOS Version: {}\nShell: {}\nWorkspace Path: {}\n</user_info>",
        std::env::consts::OS,
        shell,
        cwd.display()
    )
}

/// Collect project-instruction files (AGENTS.md / AGENT.md / CLAUDE.md / Claude.md) from
/// the cwd up to the filesystem root, nearest first. The system prompt's
/// <project_instructions_spec> tells the model to obey these. Returns None if there are none.
pub fn project_instructions() -> Option<String> {
    const NAMES: [&str; 4] = ["AGENTS.md", "AGENT.md", "CLAUDE.md", "Claude.md"];
    let cwd = std::env::current_dir().ok()?;
    let mut blocks = vec![];
    let mut seen = std::collections::HashSet::new();
    for dir in cwd.ancestors() {
        for name in NAMES {
            let p = dir.join(name);
            if p.is_file() && seen.insert(p.clone()) {
                if let Ok(content) = std::fs::read_to_string(&p) {
                    blocks.push(format!(
                        "<project_instructions path=\"{}\">\n{}\n</project_instructions>",
                        p.display(),
                        content.trim()
                    ));
                }
            }
        }
        // Stop at a repo root so we don't climb into unrelated parents.
        if dir.join(".git").exists() {
            break;
        }
    }
    if blocks.is_empty() {
        None
    } else {
        Some(blocks.join("\n\n"))
    }
}
