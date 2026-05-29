// Session persistence. Transcripts are stored as JSON under ~/.mimo/sessions/<id>.json
// (mirroring the real CLI's resumable sessions). Supports -c/--continue (latest for the
// current cwd) and -r/--resume [id].

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::agent::Agent;
use crate::api::Message;
use crate::config::mimo_home;

#[derive(Serialize, Deserialize)]
struct Stored {
    id: String,
    cwd: String,
    model: String,
    updated: u64,
    messages: Vec<Message>,
}

fn dir() -> PathBuf {
    mimo_home().join("sessions")
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

pub fn new_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{nanos:x}{:x}", std::process::id())
}

/// What the user asked to resume.
pub enum Resume {
    Continue,        // most recent for this cwd
    Id(String),      // specific id
    Latest,          // most recent overall (-r with no id)
}

/// Translate CLI flags into a resume target.
pub fn target(continue_flag: bool, resume: &Option<Option<String>>) -> Option<Resume> {
    if continue_flag {
        return Some(Resume::Continue);
    }
    match resume {
        Some(Some(id)) => Some(Resume::Id(id.clone())),
        Some(None) => Some(Resume::Latest),
        None => None,
    }
}

pub fn save(agent: &Agent) {
    // Don't persist trivial sessions (system + user_info only, no real exchange).
    if agent.messages.len() <= 2 {
        return;
    }
    let stored = Stored {
        id: agent.id.clone(),
        cwd: std::env::current_dir().unwrap_or_default().to_string_lossy().to_string(),
        model: agent.cfg.model.clone(),
        updated: now(),
        messages: agent.messages.clone(),
    };
    let d = dir();
    if std::fs::create_dir_all(&d).is_err() {
        return;
    }
    if let Ok(json) = serde_json::to_string(&stored) {
        std::fs::write(d.join(format!("{}.json", agent.id)), json).ok();
    }
}

fn load_stored(id: &str) -> Option<Stored> {
    let text = std::fs::read_to_string(dir().join(format!("{id}.json"))).ok()?;
    serde_json::from_str(&text).ok()
}

fn all_sorted() -> Vec<Stored> {
    let mut sessions: Vec<Stored> = vec![];
    if let Ok(entries) = std::fs::read_dir(dir()) {
        for e in entries.flatten() {
            if let Ok(text) = std::fs::read_to_string(e.path()) {
                if let Ok(s) = serde_json::from_str::<Stored>(&text) {
                    sessions.push(s);
                }
            }
        }
    }
    sessions.sort_by(|a, b| b.updated.cmp(&a.updated));
    sessions
}

/// Resolve a resume target into (id, messages).
pub fn resolve(target: &Resume) -> Option<(String, Vec<Message>)> {
    let stored = match target {
        Resume::Id(id) => load_stored(id)?,
        Resume::Latest => all_sorted().into_iter().next()?,
        Resume::Continue => {
            let cwd = std::env::current_dir().unwrap_or_default().to_string_lossy().to_string();
            all_sorted().into_iter().find(|s| s.cwd == cwd)?
        }
    };
    Some((stored.id, stored.messages))
}

/// Load a resume target into an agent, if resolvable. Returns true on success.
pub fn apply(agent: &mut Agent, target: &Resume) -> bool {
    match resolve(target) {
        Some((id, messages)) => {
            let n = messages.len();
            agent.load_history(messages, id);
            eprintln!("\x1b[2m  resumed session {} ({n} messages)\x1b[0m", agent.id);
            true
        }
        None => {
            eprintln!("\x1b[33m  no matching session to resume\x1b[0m");
            false
        }
    }
}
