// Cross-session memory. Mirrors the real CLI's --experimental-memory feature set:
//   memory_search / memory_get tools (model-facing), recall_context() injection at
//   session start, /flush (LLM-summarize this conversation into a durable note) and
//   /dream (consolidate + dedupe all notes).
//
// Layout: mimo_home()/memory/ holds one markdown file per memory, plus an index file
// MEMORY.md with one bullet per memory (a human/LLM-readable table of contents).

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use serde_json::{json, Value};

use crate::api::{stream_chat, Message, ToolDef};
use crate::config::{mimo_home, Config};
use crate::event::Emitter;

/// Directory holding the memory files.
fn memory_dir() -> PathBuf {
    mimo_home().join("memory")
}

/// Path to the MEMORY.md index (one bullet per memory).
fn index_path() -> PathBuf {
    memory_dir().join("MEMORY.md")
}

/// Ensure the memory directory exists. Best-effort.
fn ensure_dir() {
    std::fs::create_dir_all(memory_dir()).ok();
}

/// Tool definitions exposed to the model: memory_search + memory_get.
pub fn tool_defs() -> Vec<ToolDef> {
    vec![
        def(
            "memory_search",
            "Search durable cross-session memory for facts saved in earlier sessions (decisions, file locations, user preferences). Keyword scan over saved notes; returns matching file names with short snippets. Use before asking the user something they may have told you before.",
            json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "Keywords to look for across saved memories"}
                },
                "required": ["query"]
            }),
        ),
        def(
            "memory_get",
            "Retrieve the full contents of a single memory note by its file name (as returned by memory_search, e.g. `mem-1a2b3c.md`).",
            json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string", "description": "Memory file name, e.g. mem-1a2b3c.md"}
                },
                "required": ["name"]
            }),
        ),
    ]
}

fn def(name: &str, desc: &str, params: Value) -> ToolDef {
    ToolDef {
        kind: "function".into(),
        function: json!({ "name": name, "description": desc, "parameters": params }),
    }
}

/// Dispatch a memory tool call. Returns Some(result) if the tool belongs to this
/// module, None otherwise (so the caller can fall through to other tool sets).
pub fn dispatch(name: &str, args: &Value) -> Option<String> {
    match name {
        "memory_search" => Some(memory_search(args)),
        "memory_get" => Some(memory_get(args)),
        _ => None,
    }
}

/// Keyword scan over memory/*.md (excluding the MEMORY.md index). Splits the query
/// into lowercased terms and returns files whose contents contain any term, with a
/// short snippet around the first hit.
fn memory_search(args: &Value) -> String {
    let query = args["query"].as_str().unwrap_or("").trim();
    if query.is_empty() {
        return "memory_search: empty query".into();
    }
    let terms: Vec<String> = query
        .split_whitespace()
        .map(|t| t.to_lowercase())
        .filter(|t| !t.is_empty())
        .collect();
    if terms.is_empty() {
        return "memory_search: empty query".into();
    }

    let mut out = String::new();
    let mut hits = 0;
    for file in memory_files() {
        let Ok(text) = std::fs::read_to_string(&file) else { continue };
        let lower = text.to_lowercase();
        if !terms.iter().any(|t| lower.contains(t)) {
            continue;
        }
        let name = file.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        let snippet = snippet_around(&text, &lower, &terms);
        out.push_str(&format!("## {name}\n{snippet}\n\n"));
        hits += 1;
        if hits >= 20 {
            out.push_str("... [truncated at 20 matches]\n");
            break;
        }
    }
    if out.is_empty() {
        return format!("No memories matched: {query}");
    }
    format!("Found {hits} memory note(s) for `{query}`:\n\n{out}")
}

/// Build a short snippet (~3 lines) around the first line containing any term.
fn snippet_around(text: &str, lower: &str, terms: &[String]) -> String {
    let first = terms
        .iter()
        .filter_map(|t| lower.find(t.as_str()))
        .min()
        .unwrap_or(0);
    // Map the byte offset back to a line index.
    let line_idx = lower[..first].matches('\n').count();
    let lines: Vec<&str> = text.lines().collect();
    let start = line_idx.saturating_sub(1);
    let end = (line_idx + 2).min(lines.len());
    let mut s = String::new();
    for line in &lines[start..end] {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        s.push_str(&format!("  {trimmed}\n"));
    }
    if s.is_empty() {
        s.push_str("  (matched)\n");
    }
    s.trim_end().to_string()
}

/// Retrieve a single memory file's contents by name. Refuses path traversal.
fn memory_get(args: &Value) -> String {
    let name = args["name"].as_str().unwrap_or("").trim();
    if name.is_empty() {
        return "memory_get: missing name".into();
    }
    // Only allow a bare file name within the memory dir.
    if name.contains('/') || name.contains('\\') || name.contains("..") {
        return format!("memory_get: invalid name '{name}'");
    }
    let path = memory_dir().join(name);
    match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) => format!("memory_get: cannot read '{name}': {e}"),
    }
}

/// List the markdown memory files (excludes the MEMORY.md index), sorted by name.
fn memory_files() -> Vec<PathBuf> {
    let mut files = vec![];
    let Ok(rd) = std::fs::read_dir(memory_dir()) else { return files };
    for entry in rd.filter_map(|e| e.ok()) {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        if name == "MEMORY.md" {
            continue;
        }
        if name.ends_with(".md") {
            files.push(path);
        }
    }
    files.sort();
    files
}

/// Read MEMORY.md and wrap it for injection at session start. None if there is no
/// index or it is empty, so callers can skip injecting an empty block.
pub fn recall_context() -> Option<String> {
    let text = std::fs::read_to_string(index_path()).ok()?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(format!("<memory_index>\n{trimmed}\n</memory_index>"))
}

/// System prompt used to coax the model into extracting durable facts.
const FLUSH_SYSTEM: &str = "\
You are a memory assistant for a coding agent. Read the conversation transcript and \
extract only DURABLE, REUSABLE facts that would help in a future, unrelated session: \
project decisions, where important code or config lives (file paths), build/test \
commands, conventions, and explicit user preferences. Ignore transient chatter, \
one-off questions, and anything specific to a single completed action.\n\n\
Respond with concise markdown bullets. Begin with a single `# <short title>` line \
naming the topic, then the bullets. Do NOT add commentary or explanations. If there \
is nothing durable worth remembering, respond with exactly: NOTHING_TO_SAVE";

/// System prompt used to consolidate the whole memory store.
const DREAM_SYSTEM: &str = "\
You are a memory assistant for a coding agent. You are given the full set of saved \
memory notes. Consolidate them: merge duplicates, drop stale or contradictory items \
(keep the most recent/most specific), and group related facts under clear headings. \
Preserve every durable fact (file locations, decisions, commands, user preferences) \
but make the result tighter and non-redundant.\n\n\
Respond with the consolidated memory as markdown using `##` section headings and \
bullets. Output only the consolidated markdown, no commentary.";

/// Marker the model returns when a conversation has nothing worth saving.
const NOTHING: &str = "NOTHING_TO_SAVE";

/// Summarize the current conversation into a new durable memory note.
///
/// Calls stream_chat with a memory-assistant system prompt, writes the result to a
/// fresh file under memory/, and appends a one-line pointer to MEMORY.md. Skips
/// writing if the model decides there is nothing durable to keep.
pub async fn flush(cfg: &Config, emitter: &Emitter, messages: &[Message]) -> Result<String> {
    let transcript = render_transcript(messages);
    if transcript.trim().is_empty() {
        return Ok("Memory flush: nothing to summarize.".into());
    }

    emitter.info("Flushing conversation to memory...");
    let prompt = format!(
        "Extract durable memory from this conversation transcript:\n\n{transcript}"
    );
    let convo = vec![Message::system(FLUSH_SYSTEM), Message::user(prompt)];
    let assistant = stream_chat(cfg, &convo, vec![], emitter).await?;

    let note = assistant.content.trim().to_string();
    if note.is_empty() || note.contains(NOTHING) {
        emitter.info("Memory flush: nothing durable to save.");
        return Ok("Memory flush: nothing durable to save.".into());
    }

    ensure_dir();
    let slug = slug_for(&note);
    let file_name = format!("mem-{slug}.md");
    let path = memory_dir().join(&file_name);
    std::fs::write(&path, format!("{note}\n"))?;

    append_index_pointer(&file_name, &note)?;

    emitter.info(&format!("Saved memory: {file_name}"));
    Ok(format!("Saved memory note `{file_name}`."))
}

/// Consolidate every memory note into a tighter set and rewrite MEMORY.md.
///
/// Reads all memory/*.md, asks the model to merge/dedupe/prune, then rewrites the
/// MEMORY.md index with the consolidated result. The individual note files are left
/// in place (the index is the injected source of truth).
pub async fn dream(cfg: &Config, emitter: &Emitter) -> Result<String> {
    let files = memory_files();
    if files.is_empty() {
        return Ok("Memory dream: no memories to consolidate.".into());
    }

    let mut corpus = String::new();
    for file in &files {
        let Ok(text) = std::fs::read_to_string(file) else { continue };
        let name = file.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        corpus.push_str(&format!("--- {name} ---\n{}\n\n", text.trim()));
    }
    if corpus.trim().is_empty() {
        return Ok("Memory dream: no readable memories.".into());
    }

    emitter.info(&format!("Consolidating {} memory note(s)...", files.len()));
    let prompt = format!("Consolidate these memory notes:\n\n{corpus}");
    let convo = vec![Message::system(DREAM_SYSTEM), Message::user(prompt)];
    let assistant = stream_chat(cfg, &convo, vec![], emitter).await?;

    let consolidated = assistant.content.trim().to_string();
    if consolidated.is_empty() {
        return Ok("Memory dream: model returned nothing; index unchanged.".into());
    }

    ensure_dir();
    let header = "# Memory index\n\nConsolidated durable facts across sessions.\n";
    std::fs::write(index_path(), format!("{header}\n{consolidated}\n"))?;

    emitter.info("Rewrote MEMORY.md index.");
    Ok(format!("Consolidated {} note(s) into MEMORY.md.", files.len()))
}

/// Render the conversation into a plain transcript for the summarizer. Skips empty
/// system frames and renders tool calls/results compactly.
fn render_transcript(messages: &[Message]) -> String {
    let mut out = String::new();
    for m in messages {
        let role = m.role.as_str();
        if let Some(content) = &m.content {
            let content = content.trim();
            if !content.is_empty() {
                out.push_str(&format!("[{role}] {content}\n"));
            }
        }
        if let Some(calls) = &m.tool_calls {
            for c in calls {
                out.push_str(&format!(
                    "[{role} -> tool] {}({})\n",
                    c.function.name, c.function.arguments
                ));
            }
        }
    }
    out
}

/// Append a one-line pointer to the MEMORY.md index. Derives the bullet text from the
/// note's first non-empty line (its title), stripped of leading markdown markers.
fn append_index_pointer(file_name: &str, note: &str) -> Result<()> {
    let title = note
        .lines()
        .map(|l| l.trim())
        .find(|l| !l.is_empty())
        .unwrap_or(file_name)
        .trim_start_matches('#')
        .trim()
        .trim_start_matches('-')
        .trim();
    let bullet = format!("- [{file_name}] {title}\n");

    ensure_dir();
    let existing = std::fs::read_to_string(index_path()).unwrap_or_default();
    let body = if existing.trim().is_empty() {
        format!("# Memory index\n\nConsolidated durable facts across sessions.\n\n{bullet}")
    } else {
        format!("{}{bullet}", ensure_trailing_newline(&existing))
    };
    std::fs::write(index_path(), body)?;
    Ok(())
}

fn ensure_trailing_newline(s: &str) -> String {
    if s.ends_with('\n') {
        s.to_string()
    } else {
        format!("{s}\n")
    }
}

/// Generate a short, filesystem-safe slug from a content hash plus a time nonce, so
/// repeated flushes of similar content don't collide.
fn slug_for(content: &str) -> String {
    let mut hasher = DefaultHasher::new();
    content.hash(&mut hasher);
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    nonce.hash(&mut hasher);
    format!("{:08x}", (hasher.finish() & 0xffff_ffff) as u32)
}
