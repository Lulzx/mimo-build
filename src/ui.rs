// Full-screen ratatui TUI, styled for parity with the real Mimo Build terminal UI:
// Tokyo Night truecolor palette on a near-black background, a top header (cwd + context
// usage), a borderless transcript with ◆ activity bullets (colored per tool) and a colored
// gutter, inline edit diffs, a working line, a rounded input box with a right-aligned mode
// title, a keybind footer, and a slash-command palette. See ../re/capture/ui-reference.md.

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Wrap};
use ratatui::Terminal;
use std::io::Stdout;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};

use crate::agent::Agent;
use crate::config::Config;
use crate::event::{Decision, Emitter, UiEvent};
use crate::session::{self, Resume};

// ---- Theme system ----
// grok ships a `/theme <name>` switcher; the palettes below mirror its options
// (groknight is the default, captured truecolor from the real binary; the rest are
// faithful renditions of each named scheme). Colors are read through accessor fns so a
// runtime `/theme` swap recolors the whole UI without threading a palette everywhere.
#[derive(Clone, Copy)]
struct Theme {
    bg: Color,
    user_bg: Color,
    dim: Color,
    gray: Color,
    txt: Color,
    white: Color,
    blue: Color,
    green: Color,
    red: Color,
    purple: Color,
    cyan: Color,
    orange: Color,
    /// the faint horizontal-rule color used on the welcome menu
    rule: Color,
}

const fn rgb(r: u8, g: u8, b: u8) -> Color {
    Color::Rgb(r, g, b)
}

// groknight — neutral-gray base, the shipped default (authoritative live capture).
const GROKNIGHT: Theme = Theme {
    bg: rgb(20, 20, 20),
    user_bg: rgb(28, 28, 28),
    dim: rgb(108, 108, 108),
    gray: rgb(88, 88, 88),
    txt: rgb(200, 200, 200),
    white: rgb(224, 224, 224),
    blue: rgb(122, 162, 247),
    green: rgb(158, 206, 106),
    red: rgb(247, 118, 142),
    purple: rgb(187, 154, 247),
    cyan: rgb(137, 221, 255),
    orange: rgb(224, 175, 104),
    rule: rgb(40, 40, 40),
};

// grokday — the light companion theme.
const GROKDAY: Theme = Theme {
    bg: rgb(250, 250, 250),
    user_bg: rgb(236, 236, 236),
    dim: rgb(140, 140, 140),
    gray: rgb(176, 176, 176),
    txt: rgb(48, 48, 48),
    white: rgb(20, 20, 20),
    blue: rgb(46, 125, 233),
    green: rgb(88, 117, 57),
    red: rgb(245, 42, 101),
    purple: rgb(152, 84, 241),
    cyan: rgb(0, 113, 151),
    orange: rgb(177, 92, 0),
    rule: rgb(220, 220, 220),
};

// tokyonight — blue-tinted dark.
const TOKYONIGHT: Theme = Theme {
    bg: rgb(26, 27, 38),
    user_bg: rgb(36, 40, 59),
    dim: rgb(86, 95, 137),
    gray: rgb(65, 72, 104),
    txt: rgb(169, 177, 214),
    white: rgb(192, 202, 245),
    blue: rgb(122, 162, 247),
    green: rgb(158, 206, 106),
    red: rgb(247, 118, 142),
    purple: rgb(187, 154, 247),
    cyan: rgb(125, 207, 255),
    orange: rgb(224, 175, 104),
    rule: rgb(41, 46, 66),
};

// rosepine-moon.
const ROSEPINE_MOON: Theme = Theme {
    bg: rgb(35, 33, 54),
    user_bg: rgb(57, 53, 82),
    dim: rgb(110, 106, 134),
    gray: rgb(68, 65, 90),
    txt: rgb(224, 222, 244),
    white: rgb(224, 222, 244),
    blue: rgb(62, 143, 176),
    green: rgb(156, 207, 216),
    red: rgb(235, 111, 146),
    purple: rgb(196, 167, 231),
    cyan: rgb(156, 207, 216),
    orange: rgb(246, 193, 119),
    rule: rgb(57, 53, 82),
};

// nord.
const NORD: Theme = Theme {
    bg: rgb(46, 52, 64),
    user_bg: rgb(59, 66, 82),
    dim: rgb(118, 128, 144),
    gray: rgb(76, 86, 106),
    txt: rgb(216, 222, 233),
    white: rgb(236, 239, 244),
    blue: rgb(129, 161, 193),
    green: rgb(163, 190, 140),
    red: rgb(191, 97, 106),
    purple: rgb(180, 142, 173),
    cyan: rgb(136, 192, 208),
    orange: rgb(208, 135, 112),
    rule: rgb(59, 66, 82),
};

// oscura-midnight — extra-dark, near-OLED black.
const OSCURA_MIDNIGHT: Theme = Theme {
    bg: rgb(8, 8, 12),
    user_bg: rgb(18, 18, 24),
    dim: rgb(96, 100, 112),
    gray: rgb(64, 68, 80),
    txt: rgb(196, 200, 208),
    white: rgb(232, 234, 240),
    blue: rgb(108, 152, 240),
    green: rgb(140, 200, 120),
    red: rgb(240, 110, 134),
    purple: rgb(176, 144, 240),
    cyan: rgb(120, 214, 248),
    orange: rgb(224, 168, 96),
    rule: rgb(28, 28, 36),
};

/// `/theme` options, in palette order. The first is the default.
const THEMES: &[(&str, Theme)] = &[
    ("groknight", GROKNIGHT),
    ("grokday", GROKDAY),
    ("tokyonight", TOKYONIGHT),
    ("rosepine-moon", ROSEPINE_MOON),
    ("nord", NORD),
    ("oscura-midnight", OSCURA_MIDNIGHT),
];

static THEME: std::sync::RwLock<Theme> = std::sync::RwLock::new(GROKNIGHT);
static THEME_NAME: std::sync::RwLock<String> = std::sync::RwLock::new(String::new());

/// Switch the active theme by name. Returns a grok-style status line.
fn set_theme(name: &str) -> String {
    let name = name.trim().to_lowercase();
    match THEMES.iter().find(|(n, _)| *n == name) {
        Some((n, t)) => {
            *THEME.write().unwrap() = *t;
            *THEME_NAME.write().unwrap() = n.to_string();
            format!("Theme set to {n}.")
        }
        None => {
            let names: Vec<&str> = THEMES.iter().map(|(n, _)| *n).collect();
            format!("Unknown theme '{name}'. Available: {}.", names.join(", "))
        }
    }
}

#[inline]
fn theme() -> Theme {
    *THEME.read().unwrap()
}

// Accessor fns: one per role, so call sites read `bg()` / `blue()` etc.
fn bg() -> Color { theme().bg }
fn user_bg() -> Color { theme().user_bg }
fn dim() -> Color { theme().dim }
fn gray() -> Color { theme().gray }
fn txt() -> Color { theme().txt }
fn white() -> Color { theme().white }
fn blue() -> Color { theme().blue }
fn green() -> Color { theme().green }
fn red() -> Color { theme().red }
fn purple() -> Color { theme().purple }
fn cyan() -> Color { theme().cyan }
fn orange() -> Color { theme().orange }

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const SPINNER_MS: u128 = 80; // braille frame duration

/// Time-based spinner frame so animation speed is constant regardless of redraw cadence.
fn spinner_frame(app: &App) -> usize {
    let ms = app.turn_start.map(|t| t.elapsed().as_millis()).unwrap_or(0);
    ((ms / SPINNER_MS) % SPINNER.len() as u128) as usize
}

/// Seconds since the current turn began — the clock all turn animations share.
fn anim_t(app: &App) -> f64 {
    app.turn_start.map(|t| t.elapsed().as_secs_f64()).unwrap_or(0.0)
}

// ---- color animation helpers ----

fn to_rgb(c: Color) -> (u8, u8, u8) {
    match c {
        Color::Rgb(r, g, b) => (r, g, b),
        _ => (128, 128, 128),
    }
}

/// Linearly interpolate between two colors (`t` clamped to 0..=1).
fn lerp_color(a: Color, b: Color, t: f64) -> Color {
    let t = t.clamp(0.0, 1.0);
    let (ar, ag, ab) = to_rgb(a);
    let (br, bg, bb) = to_rgb(b);
    let m = |x: u8, y: u8| (x as f64 + (y as f64 - x as f64) * t).round() as u8;
    Color::Rgb(m(ar, br), m(ag, bg), m(ab, bb))
}

/// Smooth 0→1→0 "breathing" pulse with the given period (seconds).
fn breathe(t: f64, period: f64) -> f64 {
    0.5 - 0.5 * (std::f64::consts::TAU * t / period).cos()
}

/// A shimmer: render `text` as per-char spans with a bright crest that sweeps across it,
/// fading back to `base`. Mirrors grok's animated "Thinking…"/status label.
fn shimmer_spans(text: &str, t: f64, base: Color, hi: Color) -> Vec<Span<'static>> {
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len().max(1) as f64;
    let crest_w = 2.4_f64; // half-width of the bright band, in chars
    let period = 1.9_f64; // one sweep + brief pause
    let travel = n + crest_w * 4.0;
    let crest = (t / period).fract() * travel - crest_w * 2.0;
    chars
        .into_iter()
        .enumerate()
        .map(|(i, ch)| {
            let d = (i as f64 - crest) / crest_w;
            let glow = (-(d * d)).exp(); // gaussian crest, 0..1
            Span::styled(ch.to_string(), Style::default().fg(lerp_color(base, hi, glow)).bg(bg()))
        })
        .collect()
}

/// Slash-command palette entries (name, description) — mirrors the real CLI's palette.
const COMMANDS: &[(&str, &str)] = &[
    ("/model", "Show or switch the model"),
    ("/plan", "Toggle plan mode"),
    ("/approve", "Approve the plan (enable edits)"),
    ("/always-approve", "Toggle auto-approval of all tools"),
    ("/theme", "Switch the color theme"),
    ("/context", "Show context-window usage"),
    ("/status", "Show session status"),
    ("/compact", "Compact the conversation history"),
    ("/copy", "Copy the last response to the clipboard"),
    ("/fork", "Branch this session into a peer agent"),
    ("/sessions", "List recent sessions"),
    ("/memory", "Save this session to memory"),
    ("/dream", "Consolidate stored memories"),
    ("/goal", "Set or show the current goal"),
    ("/mcp", "Show configured MCP servers"),
    ("/inspect", "Show resolved configuration"),
    ("/new", "Start a new session"),
    ("/home", "Return to the welcome screen"),
    ("/help", "Show available commands"),
    ("/quit", "Quit the application"),
];

enum Blk {
    User { text: String, ts: String },
    Tool { kind: String, summary: String, active: bool, meta: String },
    Thought { secs: f64, text: String, expanded: bool },
    Diff { start: usize, context: Vec<String>, old: String, new: String, at: Instant },
    Assistant { text: String, ts: String },
    Todos(Vec<(String, String)>),
    Info(String),
    Error(String),
}

enum Modal {
    Approval { summary: String, plan: Option<String>, sel: usize, feedback: Option<String>, reply: Option<oneshot::Sender<Decision>> },
    Question { question: String, options: Vec<String>, input: String, reply: Option<oneshot::Sender<String>> },
}

/// Radio options for an approval prompt, mirroring grok (Execute = 3, Edit/Write = 4, plan = 2).
/// The first option means "always-approve from now on"; the last always means reject.
fn approval_options(summary: &str, plan: &Option<String>) -> Vec<&'static str> {
    if plan.is_some() {
        return vec!["Yes, proceed", "No, keep planning (type to add feedback)"];
    }
    match summary.split([' ', ':']).next().unwrap_or("") {
        "Run" | "Execute" => vec![
            "Yes, and don't ask again for anything (always-approve mode)",
            "Yes, proceed",
            "No, reject (type to add feedback)",
        ],
        _ => vec![
            "Yes, and don't ask again for anything (always-approve mode)",
            "Yes, allow all edits during this session",
            "Yes",
            "No, reject (type to add feedback)",
        ],
    }
}

struct App {
    blocks: Vec<Blk>,
    input: String,
    streaming: Option<String>,
    busy: bool,
    turn_start: Option<Instant>,
    last_event: Instant,
    used_tokens: usize,
    scroll_from_bottom: u16,
    modal: Option<Modal>,
    palette_sel: usize,
    quit: bool,
    responding: bool,
    show_shortcuts: bool,
    file_sel: usize,
    nav: Option<usize>, // activity-navigation selection (ordinal among selectable blocks)
    title: String,
    todos_total: usize,
    todos_done: usize,
    cwd: String,
    branch: Option<String>, // current git branch, shown before the cwd like grok
    model: String,
    mode: String,
    entered: bool, // left the welcome screen (set on first keystroke), like grok
}

/// Current git branch for `cwd`, by reading `.git/HEAD` (no subprocess). `None` outside a repo.
fn git_branch(cwd: &str) -> Option<String> {
    let mut dir = std::path::PathBuf::from(cwd);
    loop {
        let head = dir.join(".git/HEAD");
        if let Ok(s) = std::fs::read_to_string(&head) {
            let s = s.trim();
            return Some(match s.strip_prefix("ref: refs/heads/") {
                Some(b) => b.to_string(),
                None => s.chars().take(7).collect(), // detached HEAD → short sha
            });
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// Approx. tokens always resident in context (system prompt + tool schemas) — seeds the
/// header counter so it reads like the real CLI instead of starting near zero.
const BASE_CONTEXT_TOKENS: usize = 2600;

impl App {
    fn new(cwd: String, model: String, mode: String) -> Self {
        App {
            blocks: vec![],
            input: String::new(),
            streaming: None,
            busy: false,
            turn_start: None,
            last_event: Instant::now(),
            used_tokens: BASE_CONTEXT_TOKENS,
            scroll_from_bottom: 0,
            modal: None,
            palette_sel: 0,
            quit: false,
            responding: false,
            show_shortcuts: false,
            file_sel: 0,
            nav: None,
            title: "mimo".to_string(),
            todos_total: 0,
            todos_done: 0,
            branch: git_branch(&cwd),
            cwd,
            model,
            mode,
            entered: false,
        }
    }

    /// True only on the pristine welcome screen (logo + menu, no counter). grok leaves this
    /// state on the first keystroke and does not return until a new session.
    fn on_welcome(&self) -> bool {
        self.blocks.is_empty() && self.streaming.is_none() && !self.busy && !self.entered
    }

    fn add_tokens(&mut self, s: &str) {
        self.used_tokens += s.len() / 4 + 1;
    }

    fn flush_stream(&mut self) {
        if let Some(s) = self.streaming.take() {
            let s = s.trim_end().to_string();
            if !s.is_empty() {
                self.blocks.push(Blk::Assistant { text: s, ts: now_label() });
            }
        }
    }

    fn apply(&mut self, ev: UiEvent) {
        match ev {
            UiEvent::AssistantDelta(d) => {
                self.add_tokens(&d);
                if !self.responding {
                    self.last_event = Instant::now(); // reset the op-timer at streaming start, not every token
                }
                self.responding = true;
                self.streaming.get_or_insert_with(String::new).push_str(&d);
            }
            UiEvent::Thought { secs, text } => {
                self.flush_stream();
                self.last_event = Instant::now();
                self.blocks.push(Blk::Thought { secs, text, expanded: false });
            }
            UiEvent::ToolStart(s) => {
                self.flush_stream();
                self.add_tokens(&s);
                self.last_event = Instant::now();
                // Only the newest tool is "active"; clear the marker on prior ones.
                for b in self.blocks.iter_mut() {
                    if let Blk::Tool { active, .. } = b {
                        *active = false;
                    }
                }
                let kind = s.split([' ', ':', '[']).next().unwrap_or("").to_string();
                self.blocks.push(Blk::Tool { kind, summary: s, active: true, meta: String::new() });
            }
            UiEvent::ToolMeta(m) => {
                // Attach result metadata to the most recent tool line and mark it done.
                for b in self.blocks.iter_mut().rev() {
                    if let Blk::Tool { active, meta, .. } = b {
                        *meta = m;
                        *active = false;
                        break;
                    }
                }
            }
            UiEvent::Diff { start_line, context, old, new } => {
                self.blocks.push(Blk::Diff { start: start_line, context, old, new, at: Instant::now() });
            }
            UiEvent::Todos(items) => {
                self.flush_stream();
                self.todos_total = items.len();
                self.todos_done = items.iter().filter(|(_, s)| s == "completed").count();
                self.blocks.push(Blk::Todos(items));
            }
            UiEvent::Info(s) => {
                self.flush_stream();
                self.blocks.push(Blk::Info(s));
            }
            UiEvent::Error(s) => {
                self.flush_stream();
                self.blocks.push(Blk::Error(s));
            }
            UiEvent::Approval { summary, plan, reply } => {
                self.flush_stream();
                self.modal = Some(Modal::Approval { summary, plan, sel: 0, feedback: None, reply: Some(reply) });
            }
            UiEvent::Question { question, options, reply } => {
                self.modal = Some(Modal::Question { question, options, input: String::new(), reply: Some(reply) });
            }
            UiEvent::Status { model, mode } => {
                self.model = model;
                self.mode = mode;
            }
            UiEvent::TurnDone { cancelled } => {
                self.flush_stream();
                if let Some(t) = self.turn_start.take() {
                    // grok reports whole seconds: "Turn completed in 11s." / "Turn cancelled …".
                    let secs = t.elapsed().as_secs_f64().round() as i64;
                    let verb = if cancelled { "cancelled by user" } else { "completed" };
                    self.blocks.push(Blk::Info(format!("Turn {verb} in {secs}s.")));
                }
                self.busy = false;
            }
        }
    }

    /// Commands matching the current `/...` input.
    fn palette_matches(&self) -> Vec<(&'static str, &'static str)> {
        if !self.input.starts_with('/') {
            return vec![];
        }
        let q = self.input.trim();
        COMMANDS.iter().filter(|(n, _)| n.starts_with(q)).cloned().collect()
    }

    /// Block indices that can be navigation-selected (tool + thought activity lines).
    fn selectable(&self) -> Vec<usize> {
        self.blocks
            .iter()
            .enumerate()
            .filter(|(_, b)| matches!(b, Blk::Tool { .. } | Blk::Thought { .. }))
            .map(|(i, _)| i)
            .collect()
    }

    /// The currently nav-selected block index, if any.
    fn selected_block(&self) -> Option<usize> {
        self.nav.and_then(|n| self.selectable().get(n).copied())
    }

    /// The partial path after a trailing `@token` in the input, if any.
    fn file_token(&self) -> Option<String> {
        let at = self.input.rfind('@')?;
        let partial = &self.input[at + 1..];
        if partial.contains(char::is_whitespace) {
            return None;
        }
        Some(partial.to_string())
    }

    /// Workspace files matching the current `@token` (for the attach dropdown).
    fn file_matches(&self) -> Vec<String> {
        let Some(p) = self.file_token() else { return vec![] };
        let pl = p.to_lowercase();
        let mut out = vec![];
        for e in walkdir::WalkDir::new(".").max_depth(5).into_iter().filter_map(|e| e.ok()) {
            if !e.file_type().is_file() {
                continue;
            }
            let path = e.path().strip_prefix("./").unwrap_or(e.path()).to_string_lossy().to_string();
            if path.split('/').any(|c| matches!(c, ".git" | "target" | "node_modules")) {
                continue;
            }
            if pl.is_empty() || path.to_lowercase().contains(&pl) {
                out.push(path);
            }
            if out.len() >= 200 {
                break;
            }
        }
        out.sort_by_key(|s| s.len());
        out.truncate(8);
        out
    }
}

/// Shell-command highlighting for `Run` lines (matches grok's scheme): command + bare words
/// blue, flags orange, redirections red, pipes/operators gray, quoted strings green.
fn highlight_cmd(cmd: &str) -> Vec<Span<'static>> {
    let mut spans = vec![];
    for (i, tok) in tokenize_cmd(cmd).into_iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" ".to_string(), Style::default().bg(bg())));
        }
        let color = if tok.starts_with('-') {
            orange()
        } else if matches!(tok.as_str(), "|" | "||" | "&&" | ";") {
            gray()
        } else if tok.contains('>') || tok.contains('<') {
            red() // redirections like 2>&1, >>, 2>/dev/null
        } else if tok.starts_with('"') || tok.starts_with('\'') {
            green() // quoted strings
        } else {
            blue() // command name, subcommands, paths, bare args
        };
        spans.push(Span::styled(tok, Style::default().fg(color).bg(bg())));
    }
    spans
}

/// Split a command on whitespace but keep quoted strings ("…" / '…') as single tokens.
fn tokenize_cmd(cmd: &str) -> Vec<String> {
    let mut toks = vec![];
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    for c in cmd.chars() {
        match quote {
            Some(q) => {
                cur.push(c);
                if c == q {
                    quote = None;
                }
            }
            None => {
                if c == '"' || c == '\'' {
                    quote = Some(c);
                    cur.push(c);
                } else if c.is_whitespace() {
                    if !cur.is_empty() {
                        toks.push(std::mem::take(&mut cur));
                    }
                } else {
                    cur.push(c);
                }
            }
        }
    }
    if !cur.is_empty() {
        toks.push(cur);
    }
    toks
}

fn diamond_color(kind: &str) -> Color {
    match kind {
        "Read" => red(),
        "Run" => green(),
        "Edit" | "Write" => blue(),
        "Grep" | "List" | "Glob" | "Fetch" => cyan(),
        "Subagent" | "Task" => purple(),
        "Ask" => orange(),
        _ => dim(),
    }
}

pub async fn run(cfg: Config, resume: Option<Resume>) -> Result<()> {
    let (ev_tx, mut ev_rx) = mpsc::unbounded_channel::<UiEvent>();
    let (in_tx, mut in_rx) = mpsc::unbounded_channel::<String>();
    let (cancel_tx, mut cancel_rx) = mpsc::unbounded_channel::<()>(); // Ctrl+C while busy
    let emitter = Emitter::Channel(ev_tx.clone());

    let cwd = std::env::current_dir().unwrap_or_default().display().to_string();
    let model = cfg.model.clone();
    let mode = cfg.mode_label();
    // Apply the persisted theme, if any, before the first frame.
    if let Some(t) = &cfg.theme {
        set_theme(t);
    }

    let agent_emitter = emitter.clone();
    tokio::spawn(async move {
        let mut agent = Agent::new_with(cfg, agent_emitter.clone());
        if let Some(r) = &resume {
            session::apply(&mut agent, r);
        }
        agent_emitter.status(&agent.cfg.model, &agent.cfg.mode_label());
        while let Some(line) = in_rx.recv().await {
            let mut cancelled = false;
            match line.as_str() {
                "/flush" => {
                    let s = agent.flush_memory().await;
                    agent_emitter.info(&s);
                }
                "/dream" => {
                    let s = agent.dream_memory().await;
                    agent_emitter.info(&s);
                }
                "/memory" => {
                    let s = agent.flush_memory().await;
                    agent_emitter.info(&s);
                }
                _ if line.starts_with('/') => handle_slash(&mut agent, &line, &agent_emitter),
                _ => {
                    while cancel_rx.try_recv().is_ok() {} // drop stale cancels from a prior turn
                    cancelled = tokio::select! {
                        _ = agent.run_turn(&line) => false,
                        _ = cancel_rx.recv() => true,
                    };
                    if cancelled {
                        agent.note_cancelled(); // keep history valid after aborting mid-turn
                    } else {
                        session::save(&agent);
                    }
                }
            }
            ev_tx.send(UiEvent::TurnDone { cancelled }).ok();
        }
    });

    enable_raw_mode()?;
    let mut out = std::io::stdout();
    crossterm::execute!(out, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(out);
    let mut terminal: Terminal<CrosstermBackend<Stdout>> = Terminal::new(backend)?;

    let mut app = App::new(cwd, model, mode);
    let res = event_loop(&mut terminal, &mut app, &in_tx, &cancel_tx, &mut ev_rx).await;

    disable_raw_mode()?;
    crossterm::execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    res
}

async fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut App,
    in_tx: &mpsc::UnboundedSender<String>,
    cancel_tx: &mpsc::UnboundedSender<()>,
    ev_rx: &mut mpsc::UnboundedReceiver<UiEvent>,
) -> Result<()> {
    let mut last_title = String::new();
    loop {
        // Dynamic window title (mirrors the real TitleConfig: spinner · turn-timer ·
        // action-required · session-name) — animates while a turn runs.
        let want = if app.modal.is_some() {
            format!("● action required — {} - mimo", app.title)
        } else if app.busy {
            let sp = SPINNER[spinner_frame(app)];
            let turn = app.turn_start.map(|t| t.elapsed().as_secs()).unwrap_or(0);
            let phase = if app.responding { "Responding" } else { "Working" };
            format!("{sp} {phase} {turn}s — {} - mimo", app.title)
        } else {
            format!("{} - mimo", app.title)
        };
        if want != last_title {
            crossterm::execute!(std::io::stdout(), crossterm::terminal::SetTitle(&want)).ok();
            last_title = want;
        }
        terminal.draw(|f| render(f, app))?;
        // ~30fps while busy (smooth spinner/shimmer/streaming) or while a diff is still
        // revealing; idle polls slowly to save CPU but still wakes instantly on input.
        let animating = app.busy
            || app.blocks.iter().any(|b| matches!(b, Blk::Diff { at, .. } if at.elapsed() < Duration::from_millis(600)));
        let timeout = if animating { Duration::from_millis(33) } else { Duration::from_millis(150) };
        if event::poll(timeout)? {
            if let Event::Key(k) = event::read()? {
                if k.kind == KeyEventKind::Press {
                    if app.modal.is_some() {
                        handle_modal_key(app, k.code, k.modifiers, in_tx);
                    } else {
                        handle_key(app, k.code, k.modifiers, in_tx, cancel_tx);
                    }
                }
            }
        }
        while let Ok(ev) = ev_rx.try_recv() {
            app.apply(ev);
        }
        if app.quit {
            break;
        }
    }
    Ok(())
}

fn handle_key(app: &mut App, code: KeyCode, mods: KeyModifiers, in_tx: &mpsc::UnboundedSender<String>, cancel_tx: &mpsc::UnboundedSender<()>) {
    if app.show_shortcuts {
        app.show_shortcuts = false; // any key dismisses the overlay
        return;
    }
    let has_palette = !app.palette_matches().is_empty();
    let has_files = !app.input.starts_with('/') && app.file_token().is_some() && !app.file_matches().is_empty();
    match code {
        // Ctrl+C cancels an in-flight turn (grok); when idle it quits. Ctrl+Q always quits.
        KeyCode::Char('c') if mods.contains(KeyModifiers::CONTROL) => {
            if app.busy {
                cancel_tx.send(()).ok();
            } else {
                app.quit = true;
            }
        }
        KeyCode::Char('q') if mods.contains(KeyModifiers::CONTROL) => app.quit = true,
        KeyCode::Char('.') if mods.contains(KeyModifiers::CONTROL) => app.show_shortcuts = true,
        KeyCode::Char('n') if mods.contains(KeyModifiers::CONTROL) => {
            app.blocks.clear();
            app.streaming = None;
            app.todos_total = 0;
            app.used_tokens = BASE_CONTEXT_TOKENS;
            app.nav = None;
            app.entered = false;
            in_tx.send("/clear".to_string()).ok();
        }
        KeyCode::Char('e') | KeyCode::Char('E') if mods.contains(KeyModifiers::CONTROL) => toggle_expand(app),
        KeyCode::Char(c) => {
            app.nav = None; // typing exits navigation
            app.entered = true; // first keystroke leaves the welcome screen
            app.input.push(c);
            app.palette_sel = 0;
            app.file_sel = 0;
        }
        KeyCode::Backspace => {
            app.input.pop();
            app.palette_sel = 0;
            app.file_sel = 0;
        }
        KeyCode::Tab if has_palette => {
            let matches = app.palette_matches();
            app.input = format!("{} ", matches[app.palette_sel.min(matches.len() - 1)].0);
        }
        KeyCode::Up if has_palette => {
            app.palette_sel = app.palette_sel.saturating_sub(1);
        }
        KeyCode::Down if has_palette => {
            let n = app.palette_matches().len();
            app.palette_sel = (app.palette_sel + 1).min(n.saturating_sub(1));
        }
        KeyCode::Tab if has_files => complete_file(app),
        KeyCode::Up if has_files => app.file_sel = app.file_sel.saturating_sub(1),
        KeyCode::Down if has_files => {
            let n = app.file_matches().len();
            app.file_sel = (app.file_sel + 1).min(n.saturating_sub(1));
        }
        // Activity navigation: Up enters/moves selection (when input is empty), Down moves
        // or exits, Left/Esc exits, Enter expands a selected Thought.
        KeyCode::Up if app.input.is_empty() => {
            let sel = app.selectable();
            if !sel.is_empty() {
                app.nav = Some(match app.nav {
                    Some(n) => n.saturating_sub(1),
                    None => sel.len() - 1,
                });
            }
        }
        KeyCode::Down if app.nav.is_some() => {
            let len = app.selectable().len();
            match app.nav {
                Some(n) if n + 1 < len => app.nav = Some(n + 1),
                _ => app.nav = None,
            }
        }
        KeyCode::Left | KeyCode::Esc if app.nav.is_some() => app.nav = None,
        KeyCode::Enter if app.nav.is_some() => toggle_expand(app),
        KeyCode::Enter => {
            if has_files {
                complete_file(app);
                return;
            }
            // If the palette is open, the selected command becomes the line.
            let line = if has_palette {
                let matches = app.palette_matches();
                matches[app.palette_sel.min(matches.len() - 1)].0.to_string()
            } else {
                app.input.trim().to_string()
            };
            app.input.clear();
            app.palette_sel = 0;
            app.scroll_from_bottom = 0;
            if line.is_empty() {
                return;
            }
            if matches!(line.as_str(), "/quit" | "/exit" | "/q") {
                app.quit = true;
                return;
            }
            // Commands handled entirely in the UI (no agent round-trip needed).
            if line.starts_with('/') && ui_command(app, &line, in_tx) {
                return;
            }
            if !line.starts_with('/') {
                app.flush_stream();
                if app.title == "mimo" {
                    let t: String = line.chars().take(40).collect();
                    app.title = t;
                }
                app.blocks.push(Blk::User { text: line.clone(), ts: now_label() });
                app.add_tokens(&line);
            }
            app.busy = true;
            app.responding = false;
            app.turn_start = Some(Instant::now());
            in_tx.send(line).ok();
        }
        KeyCode::PageUp => app.scroll_from_bottom = app.scroll_from_bottom.saturating_add(8),
        KeyCode::PageDown => app.scroll_from_bottom = app.scroll_from_bottom.saturating_sub(8),
        _ => {}
    }
}

// What to do with an approval once the `app.modal` borrow is released.
enum ApprovalAct {
    Allow(usize),               // chosen non-reject option index (0 = always-approve)
    EnterFeedback,              // reject chosen → open the feedback editor
    SubmitReject(Option<String>), // submit rejection (with optional typed feedback)
}

fn handle_modal_key(app: &mut App, code: KeyCode, mods: KeyModifiers, in_tx: &mpsc::UnboundedSender<String>) {
    let mut act: Option<ApprovalAct> = None;
    match app.modal.as_mut() {
        Some(Modal::Approval { summary, plan, sel, feedback, .. }) => {
            let n = approval_options(summary, plan).len();
            if let Some(fb) = feedback {
                // Feedback editor (after choosing "No, reject"): type a reason, Enter submits.
                match code {
                    KeyCode::Char(c) if !mods.contains(KeyModifiers::CONTROL) => fb.push(c),
                    KeyCode::Backspace => { fb.pop(); }
                    KeyCode::Enter => act = Some(ApprovalAct::SubmitReject(Some(fb.clone()).filter(|s| !s.is_empty()))),
                    KeyCode::Esc => act = Some(ApprovalAct::SubmitReject(None)),
                    _ => {}
                }
            } else if mods.contains(KeyModifiers::CONTROL) {
                // Ctrl+o = always-approve (option 1); Ctrl+c = reject.
                match code {
                    KeyCode::Char('o') => act = Some(ApprovalAct::Allow(0)),
                    KeyCode::Char('c') => act = Some(ApprovalAct::EnterFeedback),
                    _ => {}
                }
            } else {
                let pick = |i: usize| if i == n - 1 { ApprovalAct::EnterFeedback } else { ApprovalAct::Allow(i) };
                match code {
                    KeyCode::Up => *sel = sel.saturating_sub(1),
                    KeyCode::Down => *sel = (*sel + 1).min(n - 1),
                    KeyCode::Esc => act = Some(ApprovalAct::EnterFeedback),
                    KeyCode::Char(c @ '1'..='9') => {
                        let i = c as usize - '1' as usize;
                        if i < n {
                            act = Some(pick(i));
                        }
                    }
                    KeyCode::Char('y') | KeyCode::Char('Y') => act = Some(pick(n.saturating_sub(2))),
                    KeyCode::Char('n') | KeyCode::Char('N') => act = Some(ApprovalAct::EnterFeedback),
                    KeyCode::Enter => act = Some(pick(*sel)),
                    _ => {}
                }
            }
        }
        Some(Modal::Question { options, input, .. }) => match code {
            KeyCode::Char(c) => input.push(c),
            KeyCode::Backspace => {
                input.pop();
            }
            KeyCode::Esc => finish_question(app, String::new()),
            KeyCode::Enter => {
                let raw = input.trim().to_string();
                let answer = raw
                    .parse::<usize>()
                    .ok()
                    .filter(|n| *n >= 1 && *n <= options.len())
                    .map(|n| options[n - 1].clone())
                    .unwrap_or(raw);
                finish_question(app, answer);
            }
            _ => {}
        },
        None => {}
    }
    match act {
        Some(ApprovalAct::Allow(i)) => finish_allow(app, i, in_tx),
        Some(ApprovalAct::EnterFeedback) => {
            if let Some(Modal::Approval { feedback, .. }) = app.modal.as_mut() {
                *feedback = Some(String::new());
            }
        }
        Some(ApprovalAct::SubmitReject(fb)) => finish_reject(app, fb),
        None => {}
    }
}

/// Allow an approval. Option 0 (non-plan) also flips auto-approve on ("don't ask again").
fn finish_allow(app: &mut App, idx: usize, in_tx: &mpsc::UnboundedSender<String>) {
    let Some(Modal::Approval { summary, plan, reply, .. }) = app.modal.as_mut() else { return };
    let always = plan.is_none() && idx == 0;
    let s = summary.clone();
    if let Some(r) = reply.take() {
        r.send(Decision::Allow).ok();
    }
    app.modal = None;
    if always {
        in_tx.send("/yolo".to_string()).ok(); // tell the agent to stop asking
    }
    app.blocks.push(Blk::Info(format!("{} {s}", if always { "✓ approved (always)" } else { "✓ approved" })));
}

/// Reject an approval, forwarding any typed feedback to the model.
fn finish_reject(app: &mut App, feedback: Option<String>) {
    let Some(Modal::Approval { summary, reply, .. }) = app.modal.as_mut() else { return };
    let s = summary.clone();
    if let Some(r) = reply.take() {
        r.send(Decision::Reject(feedback.clone())).ok();
    }
    app.modal = None;
    let note = match feedback {
        Some(f) if !f.is_empty() => format!("✗ rejected {s} — \"{f}\""),
        _ => format!("✗ rejected {s}"),
    };
    app.blocks.push(Blk::Info(note));
}

fn finish_question(app: &mut App, answer: String) {
    if let Some(Modal::Question { reply, .. }) = app.modal.as_mut() {
        if let Some(r) = reply.take() {
            r.send(answer.clone()).ok();
        }
    }
    app.modal = None;
    app.blocks.push(Blk::Info(format!("answered: {answer}")));
}

/// Handle commands that live purely in the TUI (theme/context/copy/new/home). Returns true
/// if the command was consumed here; false to forward it to the agent (handle_slash).
fn ui_command(app: &mut App, line: &str, in_tx: &mpsc::UnboundedSender<String>) -> bool {
    let mut parts = line.splitn(2, ' ');
    let cmd = parts.next().unwrap_or("");
    let arg = parts.next().unwrap_or("").trim();
    match cmd {
        "/theme" => {
            if arg.is_empty() {
                let cur = THEME_NAME.read().unwrap();
                let cur = if cur.is_empty() { "groknight" } else { cur.as_str() };
                let names: Vec<&str> = THEMES.iter().map(|(n, _)| *n).collect();
                app.blocks.push(Blk::Info(format!("Theme: {cur}. Available: {}.", names.join(", "))));
            } else {
                let msg = set_theme(arg);
                // Persist only a valid switch (set_theme reports "Theme set to …" on success).
                if msg.starts_with("Theme set") {
                    Config::persist_theme(&arg.to_lowercase());
                }
                app.blocks.push(Blk::Info(msg));
            }
            true
        }
        "/context" => {
            let used = app.used_tokens;
            let pct = (used as f64 / 512_000.0 * 100.0).round() as i64;
            app.blocks.push(Blk::Info(format!("Context: {used} / 512000 tokens ({pct}%)")));
            true
        }
        "/sessions" => {
            let recent = session::list_recent(10);
            if recent.is_empty() {
                app.blocks.push(Blk::Info("No saved sessions yet.".into()));
            } else {
                app.blocks.push(Blk::Info("Recent sessions (resume with `mimo -r <id>`):".into()));
                for (id, model, age, title) in recent {
                    let short: String = id.chars().take(12).collect();
                    let title = if title.is_empty() { "(untitled)".into() } else { title };
                    app.blocks.push(Blk::Info(format!("  {short}  {age:>3} ago  {model}  {title}")));
                }
            }
            true
        }
        "/copy" => {
            let n: usize = arg.parse().unwrap_or(1).max(1);
            let text = app
                .blocks
                .iter()
                .rev()
                .filter_map(|b| if let Blk::Assistant { text, .. } = b { Some(text.clone()) } else { None })
                .nth(n - 1);
            let note = match text {
                Some(t) if copy_clipboard(&t) => format!("Copied response to clipboard ({} chars).", t.chars().count()),
                Some(_) => "Copy failed (no clipboard tool found).".into(),
                None => "Nothing to copy yet.".into(),
            };
            app.blocks.push(Blk::Info(note));
            true
        }
        "/new" | "/home" => {
            app.blocks.clear();
            app.streaming = None;
            app.todos_total = 0;
            app.todos_done = 0;
            app.used_tokens = BASE_CONTEXT_TOKENS;
            app.nav = None;
            app.entered = false;
            app.title = "mimo".to_string();
            in_tx.send("/clear".to_string()).ok(); // reset the agent's conversation too
            true
        }
        _ => false,
    }
}

/// Copy text to the system clipboard via the platform tool. Returns false if none is available.
fn copy_clipboard(s: &str) -> bool {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let candidates: &[(&str, &[&str])] = if cfg!(target_os = "macos") {
        &[("pbcopy", &[])]
    } else {
        &[("wl-copy", &[]), ("xclip", &["-selection", "clipboard"]), ("xsel", &["--clipboard", "--input"])]
    };
    for (bin, args) in candidates {
        if let Ok(mut child) = Command::new(bin).args(*args).stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null()).spawn() {
            if let Some(mut si) = child.stdin.take() {
                let _ = si.write_all(s.as_bytes());
            }
            if child.wait().map(|st| st.success()).unwrap_or(false) {
                return true;
            }
        }
    }
    false
}

fn now_label() -> String {
    chrono::Local::now().format("%-I:%M %p").to_string()
}

/// Expand/collapse the nav-selected Thought block.
fn toggle_expand(app: &mut App) {
    if let Some(i) = app.selected_block() {
        if let Some(Blk::Thought { expanded, .. }) = app.blocks.get_mut(i) {
            *expanded = !*expanded;
        }
    }
}

/// Replace the trailing `@token` with the selected workspace file path.
fn complete_file(app: &mut App) {
    let files = app.file_matches();
    if files.is_empty() {
        return;
    }
    let sel = app.file_sel.min(files.len() - 1);
    if let Some(at) = app.input.rfind('@') {
        app.input.truncate(at);
        app.input.push('@');
        app.input.push_str(&files[sel]);
        app.input.push(' ');
    }
    app.file_sel = 0;
}

// ---- rendering ----

fn render(f: &mut ratatui::Frame, app: &App) {
    let area = f.area();
    // Base background.
    f.render_widget(Block::default().style(Style::default().bg(bg())), area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // top margin (blank row above header, like the real CLI)
            Constraint::Length(1), // header
            Constraint::Min(1),    // transcript
            Constraint::Length(1), // working line / tip
            Constraint::Length(1), // blank gap above the box (the real CLI always leaves one)
            Constraint::Length(3), // input box (3 rows: top, prompt, bottom)
            Constraint::Length(1), // spacer between box and footer
            Constraint::Length(1), // footer
            Constraint::Length(1), // bottom margin (grok keeps the footer off the last row)
        ])
        .split(area);

    render_header(f, chunks[1], app);
    render_transcript(f, chunks[2], app);
    render_working(f, chunks[3], app);
    render_input(f, chunks[5], app);
    render_footer(f, chunks[7], app);

    // Dropdowns anchor just above the box: span transcript + working + gap rows so the
    // bottom rule lands directly above the input box (and the tip line is overdrawn), as grok does.
    let dropdown_area = Rect {
        x: chunks[2].x,
        y: chunks[2].y,
        width: chunks[2].width,
        height: chunks[2].height + chunks[3].height + chunks[4].height,
    };
    if app.input.starts_with('/') && !app.palette_matches().is_empty() {
        render_palette(f, dropdown_area, app);
    } else if app.file_token().is_some() && !app.file_matches().is_empty() {
        render_file_dropdown(f, dropdown_area, app);
    }
    if app.modal.is_some() {
        render_modal(f, area, app);
    }
    if app.show_shortcuts {
        render_shortcuts(f, area);
    }
}

fn render_header(f: &mut ratatui::Frame, area: Rect, app: &App) {
    // On the welcome screen the real CLI shows only the cwd — no context counter yet.
    let on_welcome = app.on_welcome();
    let used = if app.used_tokens >= 1000 {
        // One decimal once into the thousands, matching grok's `5.7K` style.
        format!("{:.1}K", app.used_tokens as f64 / 1000.0)
    } else {
        format!("{}", app.used_tokens)
    };
    // grok wraps the usage in pipes on both sides: `│ 5.7K / 512K │`, appending todos after.
    let right = if on_welcome {
        String::new()
    } else if app.todos_total > 0 {
        format!("│ {used} / 512K │ {}/{} ✓", app.todos_done, app.todos_total)
    } else {
        format!("│ {used} / 512K │")
    };
    let w = area.width as usize;
    // grok prefixes the cwd with the git branch: `  main /path`.
    let branch = app.branch.as_deref().map(|b| format!("{b} ")).unwrap_or_default();
    let left_len = 2 + branch.chars().count() + short_path(&app.cwd).chars().count();
    let pad = w.saturating_sub(left_len + right.chars().count());
    let line = Line::from(vec![
        Span::styled("  ".to_string(), Style::default().bg(bg())),
        Span::styled(branch, Style::default().fg(purple()).bg(bg())),
        Span::styled(short_path(&app.cwd), Style::default().fg(gray()).bg(bg())),
        Span::styled(" ".repeat(pad), Style::default().bg(bg())),
        Span::styled(right, Style::default().fg(dim()).bg(bg())),
    ]);
    f.render_widget(Paragraph::new(line).style(Style::default().bg(bg())), area);
}

fn render_transcript(f: &mut ratatui::Frame, area: Rect, app: &App) {
    // Empty state → the centered welcome view (logo + menu).
    if app.on_welcome() {
        render_welcome(f, area, app);
        return;
    }
    let inner_w = area.width.saturating_sub(4) as usize;
    let lines = transcript_lines(app, inner_w);
    let total = lines.len() as u16;
    let view_h = area.height;
    let max_scroll = total.saturating_sub(view_h);
    let scroll = max_scroll.saturating_sub(app.scroll_from_bottom);
    f.render_widget(
        Paragraph::new(lines).style(Style::default().bg(bg()).fg(txt())).scroll((scroll, 0)),
        area,
    );
    // Scrollbar on overflow (the █ thumb the real CLI shows).
    if total > view_h {
        let mut sb = ScrollbarState::new(total as usize).position(scroll as usize);
        f.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight).begin_symbol(None).end_symbol(None),
            area,
            &mut sb,
        );
    }
}

/// Build the transcript as styled, indented lines.
fn transcript_lines(app: &App, w: usize) -> Vec<Line<'static>> {
    let mut out: Vec<Line> = vec![];
    let indent = "     "; // 5-col left margin, matching grok
    let wrap = |t: &str, w: usize| -> Vec<String> {
        let mut v = vec![];
        for raw in t.split('\n') {
            if raw.is_empty() {
                v.push(String::new());
            } else {
                for p in textwrap::wrap(raw, w.max(8)) {
                    v.push(p.to_string());
                }
            }
        }
        v
    };


    let sel_idx = app.selected_block();
    let t = anim_t(app); // shared clock for breathing markers / diff reveals
    for (bi, blk) in app.blocks.iter().enumerate() {
        let selected = sel_idx == Some(bi);
        match blk {
            Blk::User { text, ts } => {
                // grok: 5-col indent, `❯ ` at col 6, text wraps with continuation aligned at col 8,
                // a dim right-aligned timestamp on the first line. Lightly shaded (selection bg).
                // Reserve room for the timestamp so it never collides with the text.
                let wrapped = textwrap::wrap(text, w.saturating_sub(7 + ts.chars().count() + 2).max(8));
                for (i, piece) in wrapped.iter().enumerate() {
                    let mut spans = vec![Span::styled("     ".to_string(), Style::default().bg(user_bg()))];
                    if i == 0 {
                        spans.push(Span::styled("❯ ".to_string(), Style::default().fg(white()).bg(user_bg()).add_modifier(Modifier::BOLD)));
                    } else {
                        spans.push(Span::styled("  ".to_string(), Style::default().bg(user_bg())));
                    }
                    spans.push(Span::styled(piece.to_string(), Style::default().fg(white()).bg(user_bg())));
                    let used = 7 + piece.chars().count();
                    if i == 0 {
                        let pad = w.saturating_sub(used + ts.chars().count() + 1);
                        spans.push(Span::styled(" ".repeat(pad), Style::default().bg(user_bg())));
                        spans.push(Span::styled(format!("{ts} "), Style::default().fg(dim()).bg(user_bg())));
                    } else {
                        spans.push(Span::styled(" ".repeat(w.saturating_sub(used)), Style::default().bg(user_bg())));
                    }
                    out.push(Line::from(spans));
                }
                out.push(Line::from(""));
            }
            Blk::Tool { kind, summary, active, meta } => {
                let dcol = diamond_color(kind);
                // grok marks only the running item with a thin `❙` bar at col 3; idle items get a
                // plain 5-col indent (no per-line gutter), bullet `◆` at col 6.
                let gutter = if *active {
                    // The running tool's bar breathes between dim and its tool color.
                    let c = lerp_color(dim(), dcol, 0.4 + 0.6 * breathe(t, 1.5));
                    Span::styled("  ❙  ".to_string(), Style::default().fg(c).add_modifier(Modifier::BOLD))
                } else if selected {
                    Span::styled("  ❙  ".to_string(), Style::default().fg(white()).add_modifier(Modifier::BOLD))
                } else {
                    Span::styled("     ".to_string(), Style::default().bg(bg()))
                };
                let (verb, mut rest) = match summary.split_once(' ') {
                    Some((v, r)) => (v.to_string(), r.to_string()),
                    None => (summary.clone(), String::new()),
                };
                // grok shows workspace-relative paths; shorten for path tools (never for Run,
                // whose command legitimately contains the path).
                if kind != "Run" {
                    rest = rest.replace(&format!("{}/", app.cwd), "");
                }
                let bullet = if selected { "› " } else { "◆ " };
                let verb_style = if selected {
                    Style::default().fg(white()).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(white())
                };
                let mut spans = vec![
                    gutter,
                    Span::styled(bullet.to_string(), Style::default().fg(dcol).add_modifier(if selected { Modifier::BOLD } else { Modifier::empty() })),
                    Span::styled(format!("{verb} "), verb_style),
                ];
                if kind == "Run" {
                    spans.extend(highlight_cmd(&rest));
                } else if rest.contains('/') {
                    spans.push(Span::styled(rest, Style::default().fg(blue())));
                } else {
                    spans.push(Span::styled(rest, Style::default().fg(dim())));
                }
                if !meta.is_empty() {
                    spans.push(Span::styled(format!(" ({meta})"), Style::default().fg(gray())));
                }
                out.push(if selected { highlight_row(spans, w) } else { Line::from(spans) });
            }
            Blk::Thought { secs, text, expanded } => {
                let bullet = if selected { "› " } else { "◆ " };
                let gutter = if selected {
                    Span::styled("  ❙  ".to_string(), Style::default().fg(white()).add_modifier(Modifier::BOLD))
                } else {
                    Span::styled("     ".to_string(), Style::default().bg(bg()))
                };
                let tstyle = if selected {
                    Style::default().fg(white()).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(gray())
                };
                let tspans = vec![
                    gutter,
                    Span::styled(bullet.to_string(), Style::default().fg(dim())),
                    Span::styled("Thought ".to_string(), tstyle),
                    Span::styled(format!("for {secs:.1}s"), Style::default().fg(dim())),
                ];
                out.push(if selected { highlight_row(tspans, w) } else { Line::from(tspans) });
                if *expanded && !text.is_empty() {
                    // Expanded reasoning sits under a heavy `┃` bar, like grok's live thinking block.
                    out.push(Line::from(""));
                    for raw in wrap(text, w.saturating_sub(3)) {
                        out.push(Line::from(vec![
                            Span::styled("  ┃   ".to_string(), Style::default().fg(gray())),
                            Span::styled(raw, Style::default().fg(dim())),
                        ]));
                    }
                    out.push(Line::from(""));
                }
            }
            Blk::Diff { start, context, old, new, at } => {
                // Wave reveal: rows fade in from the background top-to-bottom when the hunk lands
                // (grok's frequency_hz/wave_rows/fps diff animation). `reveal` per row is 0→1.
                let age = at.elapsed().as_secs_f64();
                let mut ri = 0usize;
                let reveal = |ri: usize| -> f64 {
                    const PER_ROW: f64 = 0.045; // crest speed down the hunk
                    const FADE: f64 = 0.20; // each row's fade-in duration
                    ((age - ri as f64 * PER_ROW) / FADE).clamp(0.0, 1.0)
                };
                // Unchanged context lines first (default color, no marker), numbered up to `start`.
                let ctx_start = start.saturating_sub(context.len());
                for (i, l) in context.iter().enumerate() {
                    out.push(diff_row(ctx_start + i, l, txt(), reveal(ri)));
                    ri += 1;
                }
                let mut n = *start;
                for l in old.split('\n') {
                    out.push(diff_row(n, l, red(), reveal(ri)));
                    n += 1;
                    ri += 1;
                }
                let mut n2 = *start;
                for l in new.split('\n') {
                    out.push(diff_row(n2, l, green(), reveal(ri)));
                    n2 += 1;
                    ri += 1;
                }
                out.push(Line::from(""));
            }
            Blk::Assistant { text, ts } => {
                out.extend(render_assistant(text, Some(ts), w));
            }
            Blk::Todos(items) => {
                for (content, status) in items {
                    let (mark, c) = match status.as_str() {
                        "completed" => ("✔", green()),
                        "in_progress" => ("▸", orange()),
                        _ => ("○", dim()),
                    };
                    out.push(Line::from(vec![
                        Span::styled(format!("{indent}{mark} "), Style::default().fg(c)),
                        Span::styled(content.clone(), Style::default().fg(txt())),
                    ]));
                }
            }
            Blk::Info(t) => out.push(Line::from(Span::styled(
                format!("{indent}{t}"),
                Style::default().fg(dim()).bg(bg()),
            ))),
            Blk::Error(t) => out.push(Line::from(Span::styled(
                format!("{indent}{t}"),
                Style::default().fg(red()).bg(bg()),
            ))),
        }
    }
    if let Some(s) = &app.streaming {
        // Live markdown rendering (tables box-draw as they complete), no timestamp yet.
        out.extend(render_assistant(s, None, w));
    }
    // An active approval is rendered inline at the foot of the transcript with a heavy `┃`
    // gutter and numbered radio options — grok's style, not a centered box.
    if let Some(Modal::Approval { summary, plan, sel, feedback, .. }) = &app.modal {
        out.extend(approval_lines(summary, plan, *sel, feedback, w));
    }
    out
}

/// The inline approval prompt block (heavy `┃` gutter, question/plan, numbered `(●)/(○)` radios).
/// When `feedback` is Some, the radios are replaced by a feedback editor (after "No, reject").
fn approval_lines(summary: &str, plan: &Option<String>, sel: usize, feedback: &Option<String>, w: usize) -> Vec<Line<'static>> {
    let mut out = vec![];
    // A row prefixed by the heavy bar at col 3 (content at col 6).
    let bar = |spans: Vec<Span<'static>>| -> Line<'static> {
        let mut v = vec![Span::styled("  ┃  ".to_string(), Style::default().fg(orange()).add_modifier(Modifier::BOLD))];
        v.extend(spans);
        Line::from(v)
    };
    let plain = |s: String, st: Style| bar(vec![Span::styled(s, st)]);
    out.push(bar(vec![]));
    if let Some(plan) = plan {
        out.push(plain("Proposed plan:".to_string(), Style::default().fg(white()).bg(bg()).add_modifier(Modifier::BOLD)));
        for l in plan.lines() {
            for piece in textwrap::wrap(l, w.saturating_sub(6).max(8)) {
                out.push(plain(piece.to_string(), Style::default().fg(txt()).bg(bg())));
            }
        }
    } else {
        let (verb, rest) = summary.split_once(' ').unwrap_or((summary, ""));
        if verb == "Run" || verb == "Execute" {
            out.push(plain("Allow Execute?".to_string(), Style::default().fg(white()).bg(bg()).add_modifier(Modifier::BOLD)));
            out.push(plain(rest.to_string(), Style::default().fg(blue()).bg(bg())));
        } else {
            out.push(plain(format!("Allow {verb} to {rest}?"), Style::default().fg(white()).bg(bg()).add_modifier(Modifier::BOLD)));
        }
    }
    out.push(bar(vec![]));
    if let Some(fb) = feedback {
        // Feedback editor: replaces the radios after the user picks "No, reject".
        out.push(plain("Rejected — add feedback for the agent (Enter to send, Esc to skip):".to_string(), Style::default().fg(red()).bg(bg())));
        out.push(bar(vec![
            Span::styled("❯ ".to_string(), Style::default().fg(blue()).bg(bg())),
            Span::styled(fb.clone(), Style::default().fg(txt()).bg(bg())),
            Span::styled("▏".to_string(), Style::default().fg(white()).bg(bg())),
        ]));
    } else {
        let opts = approval_options(summary, plan);
        for (i, opt) in opts.iter().enumerate() {
            let focused = i == sel.min(opts.len().saturating_sub(1));
            let radio = if focused { "(●)" } else { "(○)" };
            let rcol = if focused { green() } else { dim() };
            let tcol = if focused { white() } else { txt() };
            let tmod = if focused { Modifier::BOLD } else { Modifier::empty() };
            out.push(bar(vec![
                Span::styled(format!("{} ", i + 1), Style::default().fg(dim()).bg(bg())),
                Span::styled(format!("{radio} "), Style::default().fg(rcol).bg(bg())),
                Span::styled((*opt).to_string(), Style::default().fg(tcol).bg(bg()).add_modifier(tmod)),
            ]));
        }
    }
    out.push(bar(vec![]));
    let _ = w;
    out
}

/// Apply a full-width selection background to a row's spans (nav-selected items).
fn highlight_row(spans: Vec<Span<'static>>, w: usize) -> Line<'static> {
    let used: usize = spans.iter().map(|s| s.content.chars().count()).sum();
    let mut s: Vec<Span> = spans
        .into_iter()
        .map(|sp| Span::styled(sp.content, sp.style.bg(user_bg())))
        .collect();
    if used < w {
        s.push(Span::styled(" ".repeat(w - used), Style::default().bg(user_bg())));
    }
    Line::from(s)
}

// grok's diff: line number, two spaces, code; add/del conveyed by color (no `+`/`-` glyph).
// `reveal` (0→1) fades the row in from the background for the wave-reveal animation.
fn diff_row(n: usize, code: &str, color: Color, reveal: f64) -> Line<'static> {
    let num = lerp_color(bg(), dim(), reveal);
    let col = lerp_color(bg(), color, reveal);
    Line::from(vec![
        Span::styled(format!("     {n:>3}  "), Style::default().fg(num).bg(bg())),
        Span::styled(code.to_string(), Style::default().fg(col).bg(bg())),
    ])
}

/// Light markdown: `code`→cyan, **bold**→bold white, [text](url)→blue-underlined text + dim (url),
/// leading `- `/`* ` bullets → `·`.
fn render_md_line(l: &str, indent: &str) -> Line<'static> {
    let mut spans = vec![Span::styled(indent.to_string(), Style::default().bg(bg()))];
    // Normalize leading bullets to the real CLI's middot.
    let mut rest = l.to_string();
    let trimmed = rest.trim_start();
    if let Some(b) = trimmed.strip_prefix("- ").or_else(|| trimmed.strip_prefix("* ")) {
        let lead = &rest[..rest.len() - trimmed.len()];
        spans.push(Span::styled(format!("{lead}· "), Style::default().fg(dim()).bg(bg())));
        rest = b.to_string();
    }
    // Tokenize links first, then style the inter-link text.
    let link = regex::Regex::new(r"\[([^\]]+)\]\(([^)]+)\)").unwrap();
    let mut last = 0;
    for cap in link.captures_iter(&rest) {
        let m = cap.get(0).unwrap();
        spans.extend(style_inline(&rest[last..m.start()]));
        spans.push(Span::styled(cap[1].to_string(), Style::default().fg(blue()).bg(bg()).add_modifier(Modifier::UNDERLINED)));
        spans.push(Span::styled(format!(" ({})", &cap[2]), Style::default().fg(dim()).bg(bg())));
        last = m.end();
    }
    spans.extend(style_inline(&rest[last..]));
    Line::from(spans)
}

/// Style a link-free run: `code`→cyan, **bold**→bold white, else text.
fn style_inline(s: &str) -> Vec<Span<'static>> {
    let mut spans = vec![];
    let mut code = false;
    for (i, seg) in s.split('`').enumerate() {
        if code {
            if !seg.is_empty() {
                spans.push(Span::styled(seg.to_string(), Style::default().fg(cyan()).bg(bg())));
            }
        } else {
            // handle **bold** within non-code text
            for (j, part) in seg.split("**").enumerate() {
                if part.is_empty() {
                    continue;
                }
                let st = if j % 2 == 1 {
                    Style::default().fg(white()).bg(bg()).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(txt()).bg(bg())
                };
                spans.push(Span::styled(part.to_string(), st));
            }
        }
        let _ = i;
        code = !code;
    }
    spans
}

/// Render an assistant message: wrapped markdown, box-drawn tables, and a right-aligned
/// timestamp on the first line (matching the real CLI).
fn render_assistant(text: &str, ts: Option<&str>, w: usize) -> Vec<Line<'static>> {
    let indent = "     "; // 5-col left margin, matching grok
    let lines: Vec<&str> = text.split('\n').collect();
    let mut out: Vec<Line> = vec![];
    let mut first = true;
    let mut i = 0;
    while i < lines.len() {
        if is_table_row(lines[i]) && i + 1 < lines.len() && is_separator_row(lines[i + 1]) {
            // Always include header + separator, then any following table rows.
            let mut block = vec![lines[i], lines[i + 1]];
            let mut j = i + 2;
            while j < lines.len() && is_table_row(lines[j]) {
                block.push(lines[j]);
                j += 1;
            }
            out.extend(render_table(&block, indent));
            i = j;
            first = false;
            continue;
        }
        let raw = lines[i];
        let pieces = if raw.is_empty() { vec![String::new()] } else { textwrap::wrap(raw, w.max(8)).iter().map(|s| s.to_string()).collect() };
        for piece in pieces {
            let mut ln = render_md_line(&piece, indent);
            if first {
                if let Some(ts) = ts {
                    ln = with_right_ts(ln, ts, w);
                }
                first = false;
            }
            out.push(ln);
        }
        i += 1;
    }
    if ts.is_some() {
        out.push(Line::from(""));
    }
    out
}

fn is_table_row(l: &str) -> bool {
    let t = l.trim();
    t.starts_with('|') && t.matches('|').count() >= 2
}

fn is_separator_row(l: &str) -> bool {
    let t = l.trim();
    !t.is_empty() && t.contains('-') && t.chars().all(|c| matches!(c, '|' | '-' | ':' | ' '))
}

fn split_cells(l: &str) -> Vec<String> {
    let t = l.trim().trim_matches('|');
    t.split('|').map(|c| c.trim().to_string()).collect()
}

/// Render a markdown table block as a box-drawn table.
fn render_table(block: &[&str], indent: &str) -> Vec<Line<'static>> {
    let header = split_cells(block[0]);
    // skip(2) is panic-safe even when only header+separator are present (mid-stream).
    let body: Vec<Vec<String>> = block.iter().skip(2).map(|r| split_cells(r)).collect();
    let cols = header.len();
    let mut widths = vec![0usize; cols];
    for (i, c) in header.iter().enumerate() {
        widths[i] = widths[i].max(c.chars().count());
    }
    for row in &body {
        for (i, c) in row.iter().enumerate() {
            if i < cols {
                widths[i] = widths[i].max(c.chars().count());
            }
        }
    }
    let border = Style::default().fg(gray()).bg(bg());
    let seg = |l: &str, m: &str, r: &str| -> Line<'static> {
        let mut s = String::from(indent);
        s.push_str(l);
        for (i, wdt) in widths.iter().enumerate() {
            if i > 0 {
                s.push_str(m);
            }
            s.push_str(&"─".repeat(wdt + 2));
        }
        s.push_str(r);
        Line::from(Span::styled(s, border))
    };
    let row_line = |cells: &[String], header_row: bool| -> Line<'static> {
        let mut spans = vec![Span::styled(format!("{indent}│"), border)];
        for (i, wdt) in widths.iter().enumerate() {
            let cell = cells.get(i).map(|s| s.as_str()).unwrap_or("");
            let pad = wdt - cell.chars().count();
            let st = if header_row {
                Style::default().fg(white()).bg(bg()).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(txt()).bg(bg())
            };
            spans.push(Span::styled(format!(" {cell}{} ", " ".repeat(pad)), st));
            spans.push(Span::styled("│".to_string(), border));
        }
        Line::from(spans)
    };
    let mut out = vec![seg("┌", "┬", "┐"), row_line(&header, true), seg("├", "┼", "┤")];
    for row in &body {
        out.push(row_line(row, false));
    }
    out.push(seg("└", "┴", "┘"));
    out.push(Line::from(""));
    out
}

/// Pad a rendered line to the right and append a dim timestamp.
fn with_right_ts(line: Line<'static>, ts: &str, w: usize) -> Line<'static> {
    let used: usize = line.spans.iter().map(|s| s.content.chars().count()).sum();
    let pad = w.saturating_sub(used + ts.chars().count() + 1);
    let mut spans = line.spans;
    spans.push(Span::styled(" ".repeat(pad), Style::default().bg(bg())));
    spans.push(Span::styled(format!("{ts} "), Style::default().fg(dim()).bg(bg())));
    Line::from(spans)
}

/// Centered welcome view: an original Mimo emblem + wordmark + a keybind menu, vertically
/// centered in the transcript area (same structure as the real CLI's welcome screen).
fn render_welcome(f: &mut ratatui::Frame, area: Rect, _app: &App) {
    let w = area.width as usize;
    let center = |s: &str, style: Style| -> Line<'static> {
        let pad = w.saturating_sub(s.chars().count()) / 2;
        Line::from(vec![
            Span::styled(" ".repeat(pad), Style::default().bg(bg())),
            Span::styled(s.to_string(), style),
        ])
    };
    // Original abstract braille emblem (not affiliated with any other tool's logo).
    let emblem = ["⢀⣠⣤⣄⡀", "⢸⣿⠛⣿⡇", "⠈⠻⣿⠟⠁"];
    // grok's menu: lowercase `ctrl-x` keys, no model/mode subtitle under the logo.
    let menu = [("New session", "ctrl-n"), ("Resume session", "ctrl-r"), ("Quit", "ctrl-q")];
    const MW: usize = 39;
    let lmargin = w.saturating_sub(MW) / 2;

    let mut lines: Vec<Line> = vec![];
    for e in emblem {
        lines.push(center(e, Style::default().fg(blue()).bg(bg())));
    }
    lines.push(Line::from(""));
    lines.push(center("mimo", Style::default().fg(white()).bg(bg()).add_modifier(Modifier::BOLD)));
    lines.push(Line::from(""));
    lines.push(Line::from(""));
    for (i, (label, key)) in menu.iter().enumerate() {
        if i > 0 {
            lines.push(Line::from(vec![
                Span::styled(" ".repeat(lmargin), Style::default().bg(bg())),
                Span::styled("─".repeat(MW), Style::default().fg(theme().rule).bg(bg())),
            ]));
        }
        let gap = MW.saturating_sub(label.chars().count() + key.chars().count());
        lines.push(Line::from(vec![
            Span::styled(" ".repeat(lmargin), Style::default().bg(bg())),
            Span::styled(label.to_string(), Style::default().fg(txt()).bg(bg())),
            Span::styled(" ".repeat(gap), Style::default().bg(bg())),
            Span::styled(key.to_string(), Style::default().fg(dim()).bg(bg())),
        ]));
    }

    // Vertically center.
    let top = (area.height as usize).saturating_sub(lines.len()) / 2;
    let mut all: Vec<Line> = vec![Line::from(""); top];
    all.extend(lines);
    f.render_widget(Paragraph::new(all).style(Style::default().bg(bg())), area);
}

fn render_working(f: &mut ratatui::Frame, area: Rect, app: &App) {
    if !app.busy {
        // On the welcome screen, this row carries the tip (just above the input box).
        if app.on_welcome() {
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    "  Tip: Press Ctrl+. to see all keyboard shortcuts.",
                    Style::default().fg(dim()).bg(bg()),
                )))
                .style(Style::default().bg(bg())),
                area,
            );
        } else {
            f.render_widget(Block::default().style(Style::default().bg(bg())), area);
        }
        return;
    }
    let turn = app.turn_start.map(|t| t.elapsed().as_secs_f64()).unwrap_or(0.0);
    let local = app.last_event.elapsed().as_secs_f64();
    // Phases mirror the real CLI's turn_status: Streaming while text flows, Working once a
    // tool has run, Waiting before the first activity.
    let had_activity = app.blocks.iter().any(|b| matches!(b, Blk::Tool { .. }));
    let phase = if app.responding {
        "Thinking…"
    } else if had_activity {
        "Working…"
    } else {
        "Waiting…"
    };
    let t = anim_t(app);
    // Spinner breathes between dim and full accent; the phase word shimmers (grok-style).
    let spin_color = lerp_color(dim(), purple(), 0.35 + 0.65 * breathe(t, 1.6));
    let right = format!("{:.0}s ⇣{:.1}k [✗] ", turn, app.used_tokens as f64 / 1000.0);
    let timer = format!(" {local:.1}s");
    let w = area.width as usize;
    let left_len = 4 + 1 + 1 + phase.chars().count() + timer.chars().count();
    let pad = w.saturating_sub(left_len + right.chars().count());
    let mut spans = vec![
        Span::styled("    ".to_string(), Style::default().bg(bg())),
        Span::styled(SPINNER[spinner_frame(app)].to_string(), Style::default().fg(spin_color).bg(bg()).add_modifier(Modifier::BOLD)),
        Span::styled(" ".to_string(), Style::default().bg(bg())),
    ];
    spans.extend(shimmer_spans(phase, t, dim(), white()));
    spans.push(Span::styled(timer, Style::default().fg(dim()).bg(bg())));
    spans.push(Span::styled(" ".repeat(pad), Style::default().bg(bg())));
    spans.push(Span::styled(right, Style::default().fg(dim()).bg(bg())));
    f.render_widget(Paragraph::new(Line::from(spans)).style(Style::default().bg(bg())), area);
}

fn render_input(f: &mut ratatui::Frame, area: Rect, app: &App) {
    // grok appends the mode only for the elevated always-approve posture; plan/default show
    // a bare `Mimo Build`.
    let title = if app.mode == "always-approve" {
        Line::from(vec![
            Span::styled(" Mimo Build ", Style::default().fg(dim())),
            Span::styled("· ", Style::default().fg(gray())),
            Span::styled(format!("{} ", app.mode), Style::default().fg(dim())),
        ])
    } else {
        Line::from(Span::styled(" Mimo Build ", Style::default().fg(dim())))
    }
    .right_aligned();
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(gray()))
        .title_bottom(title)
        .style(Style::default().bg(bg()));
    // Empty box shows just the prompt caret (no placeholder), matching grok.
    let prompt_line = Line::from(vec![
        Span::styled(" ❯ ", Style::default().fg(blue())),
        Span::styled(app.input.clone(), Style::default().fg(txt())),
    ]);
    // Inset the box 2 columns each side (measured from the real CLI: box width = term-4).
    let r = Rect {
        x: area.x + 2,
        y: area.y,
        width: area.width.saturating_sub(4),
        height: area.height,
    };
    f.render_widget(Paragraph::new(prompt_line).block(block).style(Style::default().bg(bg())), r);
    // Single interior row; cursor at first text column: left border + " ❯ " (3 cells) = +4.
    f.set_cursor_position((r.x + 4 + app.input.chars().count() as u16, r.y + 1));
}

fn render_footer(f: &mut ratatui::Frame, area: Rect, app: &App) {
    // Welcome screen shows the version (right-aligned) instead of keybind hints.
    if app.on_welcome() {
        let v = "mimo 0.2.11 [stable] ";
        let pad = (area.width as usize).saturating_sub(v.chars().count());
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(" ".repeat(pad), Style::default().bg(bg())),
                Span::styled(v.to_string(), Style::default().fg(dim()).bg(bg())),
            ]))
            .style(Style::default().bg(bg())),
            area,
        );
        return;
    }
    // While an approval prompt is up, grok's footer is `1/N:select │ Ctrl+o:yolo │ Ctrl+c:cancel`;
    // the feedback editor swaps in `Enter:send │ Esc:skip`.
    if let Some(Modal::Approval { summary, plan, feedback, .. }) = &app.modal {
        let pairs: Vec<(String, &str)> = if feedback.is_some() {
            vec![("Enter".into(), "send"), ("Esc".into(), "skip")]
        } else {
            let n = approval_options(summary, plan).len();
            vec![(format!("1/{n}"), "select"), ("Ctrl+o".into(), "yolo"), ("Ctrl+c".into(), "cancel")]
        };
        let mut spans = vec![Span::styled("  ", Style::default().bg(bg()))];
        for (i, (k, v)) in pairs.iter().enumerate() {
            if i > 0 {
                spans.push(Span::styled("  │  ", Style::default().fg(gray()).bg(bg())));
            }
            spans.push(Span::styled(k.clone(), Style::default().fg(txt()).bg(bg()).add_modifier(Modifier::BOLD)));
            spans.push(Span::styled(format!(":{v}"), Style::default().fg(dim()).bg(bg())));
        }
        f.render_widget(Paragraph::new(Line::from(spans)).style(Style::default().bg(bg())), area);
        return;
    }
    // grok shows `Enter:send` only when the input has content; an empty box drops it.
    let pairs: &[(&str, &str)] = if app.nav.is_some() {
        &[("←", "collapse"), ("Enter", "open"), ("Ctrl+Shift+e", "expand thinking"), ("Ctrl+.", "shortcuts")]
    } else if app.busy {
        &[("Shift+Tab", "mode"), ("Ctrl+c", "cancel"), ("Ctrl+Enter", "interject"), ("Ctrl+.", "shortcuts")]
    } else if app.input.is_empty() {
        &[("Shift+Tab", "mode"), ("Ctrl+.", "shortcuts")]
    } else {
        &[("Enter", "send"), ("Shift+Tab", "mode"), ("Ctrl+.", "shortcuts")]
    };
    let mut spans = vec![Span::styled("  ", Style::default().bg(bg()))];
    for (i, (k, v)) in pairs.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled("  │  ", Style::default().fg(gray()).bg(bg())));
        }
        spans.push(Span::styled(k.to_string(), Style::default().fg(txt()).bg(bg()).add_modifier(Modifier::BOLD)));
        spans.push(Span::styled(format!(":{v}"), Style::default().fg(dim()).bg(bg())));
    }
    f.render_widget(Paragraph::new(Line::from(spans)).style(Style::default().bg(bg())), area);
}

/// Slash-command palette, drawn just above the input box. grok renders this as a band
/// bounded by horizontal rules (no box): a top rule carrying the match count on the right,
/// rows with a `❯` selection marker and a wide name column, a `█` scroll thumb, a bottom rule.
fn render_palette(f: &mut ratatui::Frame, area: Rect, app: &App) {
    let matches = app.palette_matches();
    let total = matches.len();
    const MAX_ROWS: usize = 6;
    let rows = total.min(MAX_ROWS);
    let h = rows as u16 + 2; // top rule + rows + bottom rule
    let w = area.width as usize;
    let inner = w.saturating_sub(4); // 2-col margin each side, matching grok
    let r = Rect { x: area.x, y: area.y + area.height.saturating_sub(h), width: area.width, height: h };
    f.render_widget(Clear, r);
    f.render_widget(Block::default().style(Style::default().bg(bg())), r);

    let rule = Style::default().fg(gray()).bg(bg());
    let sel = app.palette_sel.min(total.saturating_sub(1));
    let scrollable = total > rows;
    // Scroll the visible window so the selected row stays on screen.
    let offset = sel.saturating_sub(rows.saturating_sub(1)).min(total.saturating_sub(rows));
    // Thumb row tracks the selection's position through the list.
    let thumb_row = if scrollable && total > 1 { sel * (rows - 1) / (total - 1) } else { 0 };

    let mut lines = vec![];
    // Top rule with the match count tucked against the right end (grok shows the command count here).
    let count = total.to_string();
    let dashes = inner.saturating_sub(count.chars().count() + 1);
    lines.push(Line::from(Span::styled(
        format!("  {}{}{}", "─".repeat(dashes), count, "─"),
        rule,
    )));

    const NAMEW: usize = 24;
    // 5-space left margin; the `❯ ` selection marker overlays it so names always start at col 8.
    for (i, (name, desc)) in matches.iter().skip(offset).take(rows).enumerate() {
        let is_sel = offset + i == sel;
        let marker = if is_sel { "❯ " } else { "  " };
        let name_style = Style::default()
            .fg(if is_sel { white() } else { txt() })
            .bg(bg())
            .add_modifier(if is_sel { Modifier::BOLD } else { Modifier::empty() });
        let mut spans = vec![
            Span::styled(format!("     {marker}"), Style::default().fg(blue()).bg(bg())),
            Span::styled(format!("{name:<NAMEW$}"), name_style),
            Span::styled(desc.to_string(), Style::default().fg(dim()).bg(bg())),
        ];
        // On overflow, grok marks only the selection's position with a `█` at the rule's right edge.
        let used: usize = 7 + NAMEW + desc.chars().count();
        if scrollable && i == thumb_row && used + 2 < w {
            let pad = w - 2 - 1 - used; // thumb sits at col (w-2), aligned with the rule's end
            spans.push(Span::styled(" ".repeat(pad), Style::default().bg(bg())));
            spans.push(Span::styled("█".to_string(), Style::default().fg(gray()).bg(bg())));
        }
        lines.push(Line::from(spans));
    }
    lines.push(Line::from(Span::styled(format!("  {}", "─".repeat(inner)), rule)));
    f.render_widget(Paragraph::new(lines).style(Style::default().bg(bg())), r);
}

/// `@file` attach dropdown, drawn just above the input box.
fn render_file_dropdown(f: &mut ratatui::Frame, transcript_area: Rect, app: &App) {
    let files = app.file_matches();
    let rows = (files.len() as u16).min(8);
    let h = rows + 2;
    let area = Rect {
        x: transcript_area.x + 1,
        y: transcript_area.y + transcript_area.height.saturating_sub(h),
        width: transcript_area.width.saturating_sub(2),
        height: h,
    };
    f.render_widget(Clear, area);
    let mut lines = vec![];
    for (i, path) in files.iter().enumerate() {
        let sel = i == app.file_sel.min(files.len().saturating_sub(1));
        let bg = if sel { user_bg() } else { bg() };
        let marker = if sel { "❯ " } else { "  " };
        lines.push(Line::from(vec![
            Span::styled(format!(" {marker}"), Style::default().fg(blue()).bg(bg)),
            Span::styled("@".to_string(), Style::default().fg(dim()).bg(bg)),
            Span::styled(path.clone(), Style::default().fg(if sel { white() } else { txt() }).bg(bg)),
        ]));
    }
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(gray()))
        .title(" attach file ")
        .style(Style::default().bg(bg()));
    f.render_widget(Paragraph::new(lines).block(block).style(Style::default().bg(bg())), area);
}

/// Ctrl+. shortcuts overlay — keybindings + slash commands.
fn render_shortcuts(f: &mut ratatui::Frame, area: Rect) {
    let r = centered(60, 80, area);
    f.render_widget(Clear, r);
    let key = |k: &str, d: &str| -> Line<'static> {
        Line::from(vec![
            Span::styled(format!("  {k:<14}"), Style::default().fg(white()).bg(bg()).add_modifier(Modifier::BOLD)),
            Span::styled(d.to_string(), Style::default().fg(dim()).bg(bg())),
        ])
    };
    let hdr = |t: &str| Line::from(Span::styled(format!("  {t}"), Style::default().fg(blue()).bg(bg()).add_modifier(Modifier::BOLD)));
    let mut lines = vec![
        hdr("Keys"),
        key("Enter", "send message"),
        key("Shift+Tab", "cycle permission mode"),
        key("@", "attach a workspace file"),
        key("/", "command palette"),
        key("PgUp/PgDn", "scroll transcript"),
        key("Ctrl+N", "new session"),
        key("Ctrl+C / Ctrl+Q", "quit"),
        key("Ctrl+.", "toggle this overlay"),
        Line::from(""),
        hdr("Commands"),
    ];
    for (n, d) in COMMANDS {
        lines.push(key(n, d));
    }
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(blue()))
        .title(" shortcuts ")
        .style(Style::default().bg(bg()));
    f.render_widget(Paragraph::new(lines).block(block).style(Style::default().bg(bg())), r);
}

fn render_modal(f: &mut ratatui::Frame, area: Rect, app: &App) {
    // Approval prompts render inline in the transcript (see approval_lines); only Question
    // uses a centered overlay.
    let Some(Modal::Question { .. }) = app.modal.as_ref() else { return };
    let modal = app.modal.as_ref().unwrap();
    let r = centered(70, 50, area);
    f.render_widget(Clear, r);
    let (title, text): (&str, Vec<Line>) = match modal {
        Modal::Approval { .. } => return,
        Modal::Question { question, options, input, .. } => {
            let mut t = vec![Line::from(Span::styled(question.clone(), Style::default().fg(white()).add_modifier(Modifier::BOLD)))];
            for (i, o) in options.iter().enumerate() {
                t.push(Line::from(Span::styled(format!("  {}. {o}", i + 1), Style::default().fg(txt()))));
            }
            t.push(Line::from(""));
            t.push(Line::from(vec![
                Span::styled("answer› ", Style::default().fg(blue())),
                Span::styled(input.clone(), Style::default().fg(txt())),
            ]));
            (" question ", t)
        }
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(orange()))
        .title(title)
        .style(Style::default().bg(bg()));
    f.render_widget(Paragraph::new(text).block(block).wrap(Wrap { trim: false }).style(Style::default().bg(bg())), r);
}

fn short_path(p: &str) -> String {
    if let Some(home) = dirs::home_dir() {
        let h = home.display().to_string();
        if let Some(rest) = p.strip_prefix(&h) {
            return format!("~{rest}");
        }
    }
    p.to_string()
}

fn centered(px: u16, py: u16, r: Rect) -> Rect {
    let v = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - py) / 2),
            Constraint::Percentage(py),
            Constraint::Percentage((100 - py) / 2),
        ])
        .split(r);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - px) / 2),
            Constraint::Percentage(px),
            Constraint::Percentage((100 - px) / 2),
        ])
        .split(v[1])[1]
}

fn handle_slash(agent: &mut Agent, line: &str, emitter: &Emitter) {
    let mut parts = line.splitn(2, ' ');
    let cmd = parts.next().unwrap_or("");
    let arg = parts.next().unwrap_or("").trim();
    match cmd {
        "/help" | "/?" => emitter.info(
            "commands: /model /plan /approve /always-approve /theme /context /status /compact /copy /sessions /memory /dream /goal /mcp /inspect /new /home /quit",
        ),
        "/clear" | "/new" => {
            agent.reset();
            emitter.info("(new session)");
        }
        "/model" => {
            if arg.is_empty() {
                emitter.info(&format!("model: {}", agent.cfg.model));
            } else {
                agent.cfg.model = arg.to_string();
                agent.reset();
                emitter.info(&format!("(model → {})", agent.cfg.model));
            }
        }
        "/plan" => {
            agent.cfg.plan_mode = !agent.cfg.plan_mode;
            emitter.info(&format!("(plan mode {})", if agent.cfg.plan_mode { "ON" } else { "OFF" }));
        }
        "/approve" => {
            agent.approve_plan();
            emitter.info("(plan approved)");
        }
        "/yolo" | "/always-approve" => {
            agent.cfg.always_approve = !agent.cfg.always_approve;
            emitter.info(&format!("(auto-approve {})", if agent.cfg.always_approve { "ON" } else { "OFF" }));
        }
        "/compact" => {
            let dropped = agent.compact(10);
            if dropped == 0 {
                emitter.info("Nothing to compact yet.");
            } else {
                emitter.info(&format!("Compacted {dropped} messages from the history."));
            }
        }
        "/mcp" => {
            let servers = agent.mcp_summary();
            if servers.is_empty() {
                emitter.info("No MCP servers configured. Add them in ~/.mimo/config.toml.");
            } else {
                emitter.info("MCP servers:");
                for (name, n) in servers {
                    emitter.info(&format!("  {name} ({n} tools)"));
                }
            }
        }
        "/fork" => emitter.info("Fork from a saved session with `mimo -r <id>` (interactive forking is coming soon)."),
        "/goal" => {
            if arg.is_empty() {
                emitter.info(&crate::goal::status());
            } else if arg == "clear" {
                crate::goal::clear();
                emitter.info("(goal cleared)");
            } else {
                emitter.info(&crate::goal::set_goal(arg));
            }
        }
        "/inspect" => agent.cfg.print_inspect(),
        "/status" => {
            let cwd = std::env::current_dir().unwrap_or_default().display().to_string();
            emitter.info(&format!("Version: mimo 0.2.11"));
            emitter.info(&format!("Session ID: {}", agent.id));
            emitter.info(&format!("Working directory: {cwd}"));
            emitter.info(&format!("Model: {}", agent.cfg.model));
            emitter.info(&format!("Backend: {}", agent.cfg.base_url));
            emitter.info(&format!("Auth: {}", agent.cfg.auth_source));
        }
        other => emitter.info(&format!("unknown command: {other}")),
    }
    emitter.status(&agent.cfg.model, &agent.cfg.mode_label());
}
