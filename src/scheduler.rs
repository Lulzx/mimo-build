// Scheduler + monitor tools: scheduler_create / scheduler_delete / scheduler_list and monitor.
//
// Schedules are persisted to mimo_home()/schedules.json as a JSON list of entries. This is an
// honest reimplementation: it does NOT run a background cron daemon — the schedule is recorded
// to disk (so an external runner or a future daemon could pick it up) and the create/delete/list
// tools are pure file CRUD over that list. The monitor tool actually runs a command via
// tokio::process and streams a bounded window of stdout lines back through the emitter.

use std::process::Stdio;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

use crate::api::ToolDef;
use crate::config::mimo_home;
use crate::event::Emitter;

const MONITOR_DEFAULT_MAX_LINES: usize = 50;
const MONITOR_DEFAULT_MAX_SECONDS: u64 = 30;

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Schedule {
    id: String,
    command: String,
    interval_human: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_fire_at: Option<String>,
    recurring: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    when_to_use: Option<String>,
}

/// Tool schemas exposed to the model. Wire shape matches the rest of the built-in tool set.
pub fn tool_defs() -> Vec<ToolDef> {
    vec![
        def(
            "scheduler_create",
            "Persist a scheduled command to run on an interval. NOTE: this build records the schedule to disk (mimo_home/schedules.json); it does not run a live cron daemon. Returns the new schedule id.",
            json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "The command to run when the schedule fires"},
                    "interval_human": {"type": "string", "description": "Human interval, e.g. \"every 5m\", \"every 2h\", \"daily\""},
                    "recurring": {"type": "boolean", "description": "Whether the schedule repeats (default true)"},
                    "when_to_use": {"type": "string", "description": "Optional note on when this schedule is relevant"}
                },
                "required": ["command", "interval_human"]
            }),
        ),
        def(
            "scheduler_delete",
            "Delete a persisted schedule by its id.",
            json!({
                "type": "object",
                "properties": {"id": {"type": "string"}},
                "required": ["id"]
            }),
        ),
        def(
            "scheduler_list",
            "List all persisted schedules from mimo_home/schedules.json.",
            json!({"type": "object", "properties": {}}),
        ),
        def(
            "monitor",
            "Run a command and stream up to max_lines of stdout back, bounded by max_seconds, then return a summary. Use to watch a process / poll for output. Always terminates.",
            json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string"},
                    "max_lines": {"type": "integer", "description": "Max stdout lines to stream (default 50)"},
                    "max_seconds": {"type": "integer", "description": "Max wall-clock seconds to watch (default 30)"}
                },
                "required": ["command"]
            }),
        ),
    ]
}

/// Dispatch a scheduler/monitor tool call. Returns None if `name` isn't one of ours.
pub async fn dispatch(
    name: &str,
    args: &serde_json::Value,
    emitter: &Emitter,
) -> Option<String> {
    match name {
        "scheduler_create" => Some(scheduler_create(args)),
        "scheduler_delete" => Some(scheduler_delete(args)),
        "scheduler_list" => Some(scheduler_list()),
        "monitor" => Some(monitor(args, emitter).await),
        _ => None,
    }
}

fn def(name: &str, desc: &str, params: serde_json::Value) -> ToolDef {
    ToolDef {
        kind: "function".into(),
        function: json!({ "name": name, "description": desc, "parameters": params }),
    }
}

fn schedules_path() -> std::path::PathBuf {
    mimo_home().join("schedules.json")
}

fn load_schedules() -> Vec<Schedule> {
    let path = schedules_path();
    let Ok(text) = std::fs::read_to_string(&path) else { return Vec::new() };
    serde_json::from_str(&text).unwrap_or_default()
}

fn save_schedules(list: &[Schedule]) -> std::io::Result<()> {
    let path = schedules_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let text = serde_json::to_string_pretty(list).unwrap_or_else(|_| "[]".into());
    std::fs::write(&path, text)
}

/// New id = (max existing numeric id) + 1, formatted "sched_N".
fn next_id(list: &[Schedule]) -> String {
    let max = list
        .iter()
        .filter_map(|s| s.id.rsplit('_').next().and_then(|n| n.parse::<u64>().ok()))
        .max()
        .unwrap_or(0);
    format!("sched_{}", max + 1)
}

fn scheduler_create(args: &serde_json::Value) -> String {
    let Some(command) = args["command"].as_str().filter(|s| !s.is_empty()) else {
        return "error: missing command".into();
    };
    let Some(interval) = args["interval_human"].as_str().filter(|s| !s.is_empty()) else {
        return "error: missing interval_human".into();
    };
    let recurring = args["recurring"].as_bool().unwrap_or(true);
    let when_to_use = args["when_to_use"].as_str().filter(|s| !s.is_empty()).map(String::from);

    let mut list = load_schedules();
    let id = next_id(&list);
    let next_fire_at = estimate_next_fire(interval);
    list.push(Schedule {
        id: id.clone(),
        command: command.to_string(),
        interval_human: interval.to_string(),
        next_fire_at,
        recurring,
        when_to_use,
    });
    if let Err(e) = save_schedules(&list) {
        return format!("error: could not write schedules.json: {e}");
    }
    format!(
        "Created schedule {id} ({command:?} {interval}, recurring={recurring}). \
         Persisted to {}. Note: this build does not run a live cron daemon, so the command \
         will not fire on its own — the schedule is recorded for an external runner.",
        schedules_path().display()
    )
}

fn scheduler_delete(args: &serde_json::Value) -> String {
    let Some(id) = args["id"].as_str().filter(|s| !s.is_empty()) else {
        return "error: missing id".into();
    };
    let mut list = load_schedules();
    let before = list.len();
    list.retain(|s| s.id != id);
    if list.len() == before {
        return format!("error: no schedule with id {id}");
    }
    if let Err(e) = save_schedules(&list) {
        return format!("error: could not write schedules.json: {e}");
    }
    format!("Deleted schedule {id}.")
}

fn scheduler_list() -> String {
    let list = load_schedules();
    if list.is_empty() {
        return "No schedules persisted.".into();
    }
    let mut out = format!("{} schedule(s) in {}:\n", list.len(), schedules_path().display());
    for s in &list {
        out.push_str(&format!(
            "- {} | {} | {:?}{}{}\n",
            s.id,
            s.interval_human,
            s.command,
            if s.recurring { " | recurring" } else { " | once" },
            s.when_to_use.as_deref().map(|w| format!(" | when: {w}")).unwrap_or_default(),
        ));
    }
    out
}

/// Best-effort RFC-3339-ish timestamp for the next fire, derived from a human interval.
/// Returns None if the interval isn't recognized; this is a hint only, not a guarantee.
fn estimate_next_fire(interval_human: &str) -> Option<String> {
    let secs = parse_interval_secs(interval_human)?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?;
    let fire = now.as_secs() + secs;
    // Store as a plain unix-epoch-seconds marker; honest about precision without pulling chrono.
    Some(format!("epoch+{fire}"))
}

/// Parse a human interval like "every 5m", "every 2h", "daily", "hourly" into seconds.
fn parse_interval_secs(interval_human: &str) -> Option<u64> {
    let s = interval_human.trim().to_lowercase();
    let s = s.strip_prefix("every").map(str::trim).unwrap_or(&s);
    match s {
        "minute" => return Some(60),
        "hourly" | "hour" => return Some(3600),
        "daily" | "day" => return Some(86_400),
        "weekly" | "week" => return Some(604_800),
        _ => {}
    }
    // Forms like "5m", "2h", "30s", "1d".
    let s = s.replace(' ', "");
    let (num, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len()));
    let n: u64 = num.parse().ok()?;
    let mult = match unit {
        "s" | "sec" | "secs" => 1,
        "m" | "min" | "mins" => 60,
        "h" | "hr" | "hrs" => 3600,
        "d" | "day" | "days" => 86_400,
        _ => return None,
    };
    Some(n * mult)
}

/// Run `command`, streaming up to `max_lines` stdout lines via emitter.info, bounded by
/// `max_seconds` of wall-clock time. Always terminates: kills the child on either bound.
async fn monitor(args: &serde_json::Value, emitter: &Emitter) -> String {
    let Some(command) = args["command"].as_str().filter(|s| !s.is_empty()) else {
        return "error: missing command".into();
    };
    let max_lines = args["max_lines"].as_u64().map(|n| n as usize).unwrap_or(MONITOR_DEFAULT_MAX_LINES).max(1);
    let max_seconds = args["max_seconds"].as_u64().unwrap_or(MONITOR_DEFAULT_MAX_SECONDS).max(1);

    let mut child = match Command::new("bash")
        .arg("-c")
        .arg(command)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return format!("error: could not spawn command: {e}"),
    };

    let Some(stdout) = child.stdout.take() else {
        let _ = child.start_kill();
        return "error: could not capture stdout".into();
    };
    let mut reader = BufReader::new(stdout).lines();

    let deadline = Instant::now() + Duration::from_secs(max_seconds);
    let mut seen = 0usize;
    let mut exited: Option<i32> = None;
    let mut hit_line_cap = false;

    emitter.info(&format!("monitor: watching `{command}` (<= {max_lines} lines, <= {max_seconds}s)"));

    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        tokio::select! {
            line = reader.next_line() => {
                match line {
                    Ok(Some(line)) => {
                        emitter.info(&format!("monitor| {line}"));
                        seen += 1;
                        if seen >= max_lines {
                            hit_line_cap = true;
                            break;
                        }
                    }
                    // EOF: command's stdout closed. Reap the exit code and stop.
                    Ok(None) => {
                        if let Ok(status) = child.wait().await {
                            exited = Some(status.code().unwrap_or(-1));
                        }
                        break;
                    }
                    Err(e) => {
                        emitter.error(&format!("monitor: read error: {e}"));
                        break;
                    }
                }
            }
            _ = tokio::time::sleep(remaining) => {
                break;
            }
        }
    }

    // Always terminate the child so monitor is bounded.
    if exited.is_none() {
        let _ = child.start_kill();
    }

    let stop_reason = if let Some(code) = exited {
        format!("process exited (code {code})")
    } else if hit_line_cap {
        format!("reached max_lines={max_lines}")
    } else {
        format!("reached max_seconds={max_seconds}")
    };
    format!("monitor finished: streamed {seen} stdout line(s) from `{command}`; stopped because {stop_reason}.")
}
