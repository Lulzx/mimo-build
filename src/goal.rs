// Goal state machine + LLM completion classifier. Mirrors the real CLI's
// `update_goal` tool and `/goal` slash command: a persistent objective with an
// explicit lifecycle (Active → Paused/Blocked/Complete), file-backed under
// ~/.mimo/goal.json. Two rules from the original are preserved:
//   * `completed:true` only flips the goal to Complete once an LLM classifier
//     confirms the goal is actually done (the model can't self-certify).
//   * the model must register 3 consecutive `blocked` attempts before the goal
//     is auto-paused; any non-blocked update resets the counter.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::api::{Message, ToolDef};
use crate::config::Config;
use crate::event::Emitter;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum GoalState {
    Active,
    Paused,
    Blocked,
    Complete,
}

impl GoalState {
    fn label(&self) -> &'static str {
        match self {
            GoalState::Active => "Active",
            GoalState::Paused => "Paused",
            GoalState::Blocked => "Blocked",
            GoalState::Complete => "Complete",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Goal {
    pub objective: String,
    pub state: GoalState,
    #[serde(default)]
    pub blocked_attempts: u32,
}

const BLOCKED_LIMIT: u32 = 3;

fn goal_path() -> std::path::PathBuf {
    crate::config::mimo_home().join("goal.json")
}

fn load() -> Option<Goal> {
    let text = std::fs::read_to_string(goal_path()).ok()?;
    serde_json::from_str(&text).ok()
}

fn save(goal: &Goal) {
    let path = goal_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    if let Ok(text) = serde_json::to_string_pretty(goal) {
        std::fs::write(path, text).ok();
    }
}

/// Tool schema for `update_goal`. Matches the real CLI's parameter shape.
pub fn tool_defs() -> Vec<ToolDef> {
    vec![ToolDef {
        kind: "function".into(),
        function: json!({
            "name": "update_goal",
            "description": "Update the status of the active goal as you make progress. Set completed:true when you believe the goal is fully accomplished (a classifier will verify before it is marked Complete). Provide blocked_reason when you cannot make progress; after 3 consecutive blocked updates the goal is auto-paused. Use next_steps to record what you intend to do next.",
            "parameters": {
                "type": "object",
                "properties": {
                    "completed": {"type": "boolean", "description": "True when the goal appears fully accomplished"},
                    "blocked_reason": {"type": "string", "description": "Why progress is currently blocked"},
                    "next_steps": {"type": "string", "description": "What you plan to do next"}
                }
            }
        }),
    }]
}

/// Handle the `update_goal` tool. Returns a status string for the model, or
/// None when `name` is not ours.
pub fn dispatch(name: &str, args: &Value) -> Option<String> {
    if name != "update_goal" {
        return None;
    }

    let Some(mut goal) = load() else {
        return Some(
            "No active goal. Use the /goal command to set an objective before calling update_goal."
                .to_string(),
        );
    };

    let completed = args["completed"].as_bool().unwrap_or(false);
    let blocked_reason = args["blocked_reason"].as_str().filter(|s| !s.is_empty());
    let next_steps = args["next_steps"].as_str().filter(|s| !s.is_empty());

    let msg = if let Some(reason) = blocked_reason {
        // Blocked update: count consecutive attempts; auto-pause at the limit.
        goal.blocked_attempts += 1;
        if goal.blocked_attempts >= BLOCKED_LIMIT {
            goal.state = GoalState::Paused;
            format!(
                "Goal auto-paused after {} consecutive blocked attempts. Latest reason: {reason}. Resolve the blocker (or ask the user) before resuming.",
                goal.blocked_attempts
            )
        } else {
            goal.state = GoalState::Blocked;
            let remaining = BLOCKED_LIMIT - goal.blocked_attempts;
            format!(
                "Goal marked Blocked ({}/{} consecutive). Reason: {reason}. Retry — keep working the problem; the goal will be auto-paused after {remaining} more blocked attempt(s).",
                goal.blocked_attempts, BLOCKED_LIMIT
            )
        }
    } else if completed {
        // Non-blocked update resets the counter; completion needs verification.
        goal.blocked_attempts = 0;
        goal.state = GoalState::Active;
        "Completion requested. The goal is NOT yet marked Complete — a classifier will verify whether the objective is actually accomplished. Keep going if anything remains.".to_string()
    } else {
        goal.blocked_attempts = 0;
        goal.state = GoalState::Active;
        match next_steps {
            Some(steps) => format!("Goal is Active. Next steps recorded: {steps}"),
            None => "Goal is Active. Progress noted.".to_string(),
        }
    };

    save(&goal);
    Some(msg)
}

/// `/goal <objective>`: create a fresh Active goal.
pub fn set_goal(objective: &str) -> String {
    let goal = Goal {
        objective: objective.to_string(),
        state: GoalState::Active,
        blocked_attempts: 0,
    };
    save(&goal);
    format!("Goal set (Active): {objective}")
}

/// `/goal` with no argument: report the current goal + state.
pub fn status() -> String {
    match load() {
        Some(g) => {
            let mut s = format!("Goal [{}]: {}", g.state.label(), g.objective);
            if g.blocked_attempts > 0 {
                s.push_str(&format!(" (blocked attempts: {}/{})", g.blocked_attempts, BLOCKED_LIMIT));
            }
            s
        }
        None => "No active goal. Use /goal <objective> to set one.".to_string(),
    }
}

/// `/goal clear`: remove the persisted goal.
pub fn clear() {
    std::fs::remove_file(goal_path()).ok();
}

const CLASSIFIER_SYSTEM: &str = "You are a strict goal-completion classifier. You are given a stated goal and a recent transcript of an agent's work. Decide whether the goal has actually been fully accomplished based only on the evidence in the transcript. Be conservative: if there is any doubt, partial work, or unverified claims, the goal is NOT complete. Respond with ONLY a single JSON object and nothing else: {\"complete\": true} or {\"complete\": false}.";

/// LLM classifier: ask the model whether the stated goal is actually complete,
/// given the recent transcript. Parses `{"complete": bool}`. Fail-open is false:
/// any parse/transport error returns false so completion is never falsely granted.
pub async fn classify_completion(
    cfg: &Config,
    emitter: &Emitter,
    messages: &[Message],
) -> bool {
    let Some(goal) = load() else {
        return false;
    };

    // Recent transcript: the tail of the conversation, text content only.
    let mut transcript = String::new();
    let start = messages.len().saturating_sub(12);
    for m in &messages[start..] {
        if let Some(content) = &m.content {
            if content.is_empty() {
                continue;
            }
            transcript.push_str(&format!("[{}] {}\n", m.role, content));
        }
    }

    let prompt = format!(
        "Stated goal:\n{}\n\nRecent transcript:\n{}\n\nIs the stated goal actually complete? Respond with ONLY {{\"complete\": true}} or {{\"complete\": false}}.",
        goal.objective, transcript
    );

    let probe = vec![Message::system(CLASSIFIER_SYSTEM), Message::user(prompt)];

    let assistant = match crate::api::stream_chat(cfg, &probe, vec![], emitter).await {
        Ok(a) => a,
        Err(e) => {
            emitter.error(&format!("goal classifier error: {e}"));
            return false;
        }
    };

    parse_complete(&assistant.content)
}

/// Extract `{"complete": bool}` from the model output. Tolerates surrounding
/// prose by scanning for the first JSON object; fail-open is false.
fn parse_complete(content: &str) -> bool {
    let candidate = match (content.find('{'), content.rfind('}')) {
        (Some(a), Some(b)) if b >= a => &content[a..=b],
        _ => return false,
    };
    serde_json::from_str::<Value>(candidate)
        .ok()
        .and_then(|v| v["complete"].as_bool())
        .unwrap_or(false)
}
