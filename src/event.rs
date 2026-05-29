// Output abstraction. The agent loop emits events instead of printing directly, so the
// same loop drives both the plain stdout path (headless / inline REPL) and the ratatui TUI.

use std::io::{self, Write};
use tokio::sync::{mpsc, oneshot};

/// Events the agent emits as a turn progresses.
pub enum UiEvent {
    AssistantDelta(String),
    ToolStart(String),
    ToolDone,
    Todos(Vec<(String, String)>), // (content, status)
    Info(String),
    Error(String),
    /// Ask the UI to approve a tool call or plan; the UI replies via the oneshot.
    Approval { summary: String, plan: Option<String>, reply: oneshot::Sender<bool> },
    /// Ask the user a question with optional preset options; reply is the chosen/typed text.
    Question { question: String, options: Vec<String>, reply: oneshot::Sender<String> },
    Status { model: String, plan_mode: bool },
    TurnDone,
}

/// Where agent output goes.
#[derive(Clone)]
pub enum Emitter {
    Stdout,
    Channel(mpsc::UnboundedSender<UiEvent>),
}

impl Emitter {
    pub fn assistant_delta(&self, s: &str) {
        match self {
            Emitter::Stdout => {
                print!("{s}");
                io::stdout().flush().ok();
            }
            Emitter::Channel(tx) => {
                tx.send(UiEvent::AssistantDelta(s.to_string())).ok();
            }
        }
    }

    /// Called once at the end of a streamed assistant message (newline on stdout).
    pub fn assistant_end(&self, had_text: bool) {
        if let Emitter::Stdout = self {
            if had_text {
                println!();
            }
        }
    }

    pub fn tool_start(&self, summary: &str) {
        match self {
            Emitter::Stdout => println!("\x1b[2m• {summary}\x1b[0m"),
            Emitter::Channel(tx) => {
                tx.send(UiEvent::ToolStart(summary.to_string())).ok();
            }
        }
    }

    pub fn tool_done(&self, _summary: &str) {
        if let Emitter::Channel(tx) = self {
            tx.send(UiEvent::ToolDone).ok();
        }
    }

    pub fn todos(&self, items: Vec<(String, String)>) {
        match self {
            Emitter::Stdout => {
                println!("\x1b[2m• Todos:\x1b[0m");
                for (content, status) in &items {
                    let mark = match status.as_str() {
                        "completed" => "\x1b[32m✔\x1b[0m",
                        "in_progress" => "\x1b[33m▸\x1b[0m",
                        _ => "\x1b[2m○\x1b[0m",
                    };
                    println!("  {mark} {content}");
                }
            }
            Emitter::Channel(tx) => {
                tx.send(UiEvent::Todos(items)).ok();
            }
        }
    }

    pub fn status(&self, model: &str, plan_mode: bool) {
        if let Emitter::Channel(tx) = self {
            tx.send(UiEvent::Status { model: model.to_string(), plan_mode }).ok();
        }
    }

    pub fn info(&self, s: &str) {
        match self {
            Emitter::Stdout => println!("\x1b[2m{s}\x1b[0m"),
            Emitter::Channel(tx) => {
                tx.send(UiEvent::Info(s.to_string())).ok();
            }
        }
    }

    pub fn error(&self, s: &str) {
        match self {
            Emitter::Stdout => eprintln!("\x1b[31m{s}\x1b[0m"),
            Emitter::Channel(tx) => {
                tx.send(UiEvent::Error(s.to_string())).ok();
            }
        }
    }

    /// Ask the user a question. On stdout, prints options and reads a line; in the TUI,
    /// shows a selection modal. Returns the chosen option label or the typed text.
    pub async fn ask(&self, question: &str, options: &[String]) -> String {
        match self {
            Emitter::Stdout => {
                println!("\n\x1b[1;36m? {question}\x1b[0m");
                for (i, o) in options.iter().enumerate() {
                    println!("  {}. {o}", i + 1);
                }
                print!("\x1b[33manswer\x1b[0m (number or text): ");
                io::stdout().flush().ok();
                let mut line = String::new();
                io::stdin().read_line(&mut line).ok();
                let line = line.trim().to_string();
                if let Ok(n) = line.parse::<usize>() {
                    if n >= 1 && n <= options.len() {
                        return options[n - 1].clone();
                    }
                }
                line
            }
            Emitter::Channel(tx) => {
                let (rtx, rrx) = oneshot::channel();
                if tx
                    .send(UiEvent::Question {
                        question: question.to_string(),
                        options: options.to_vec(),
                        reply: rtx,
                    })
                    .is_err()
                {
                    return String::new();
                }
                rrx.await.unwrap_or_default()
            }
        }
    }

    /// Request approval. On stdout, reads y/N from stdin; in the TUI, shows a modal.
    pub async fn request_approval(&self, summary: &str, plan: Option<String>) -> bool {
        match self {
            Emitter::Stdout => {
                if let Some(p) = &plan {
                    println!("\n\x1b[1;36m── Proposed plan ──\x1b[0m\n{p}\n");
                    print!("\x1b[33m? Approve this plan?\x1b[0m [y/N] ");
                } else {
                    print!("\x1b[33m? Approve:\x1b[0m {summary} [y/N] ");
                }
                io::stdout().flush().ok();
                let mut line = String::new();
                io::stdin().read_line(&mut line).ok();
                matches!(line.trim().to_lowercase().as_str(), "y" | "yes")
            }
            Emitter::Channel(tx) => {
                let (rtx, rrx) = oneshot::channel();
                if tx.send(UiEvent::Approval { summary: summary.to_string(), plan, reply: rtx }).is_err() {
                    return false;
                }
                rrx.await.unwrap_or(false)
            }
        }
    }
}
