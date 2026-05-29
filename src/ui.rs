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
use crate::event::{Emitter, UiEvent};
use crate::session::{self, Resume};

// ---- Tokyo Night palette (captured from the real binary) ----
const BG: Color = Color::Rgb(20, 20, 20);
const USER_BG: Color = Color::Rgb(28, 28, 28);
const DIM: Color = Color::Rgb(108, 108, 108);
const GRAY: Color = Color::Rgb(88, 88, 88);
const TXT: Color = Color::Rgb(200, 200, 200);
const WHITE: Color = Color::Rgb(224, 224, 224);
const BLUE: Color = Color::Rgb(122, 162, 247);
const GREEN: Color = Color::Rgb(158, 206, 106);
const RED: Color = Color::Rgb(247, 118, 142);
const PURPLE: Color = Color::Rgb(187, 154, 247);
const CYAN: Color = Color::Rgb(137, 221, 255);
const ORANGE: Color = Color::Rgb(224, 175, 104);

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Slash-command palette entries (name, description).
const COMMANDS: &[(&str, &str)] = &[
    ("/help", "Show available commands"),
    ("/model", "Show or switch the model"),
    ("/plan", "Toggle plan mode"),
    ("/approve", "Approve the plan (enable edits)"),
    ("/yolo", "Toggle auto-approval of all tools"),
    ("/goal", "Set or show the current goal"),
    ("/flush", "Save this session to memory"),
    ("/dream", "Consolidate stored memories"),
    ("/clear", "Start a new session"),
    ("/status", "Show session status"),
    ("/inspect", "Show resolved configuration"),
    ("/quit", "Quit the application"),
];

enum Blk {
    User { text: String, ts: String },
    Tool { kind: String, summary: String, active: bool },
    Diff { start: usize, old: String, new: String },
    Assistant { text: String, ts: String },
    Todos(Vec<(String, String)>),
    Info(String),
    Error(String),
}

enum Modal {
    Approval { summary: String, plan: Option<String>, reply: Option<oneshot::Sender<bool>> },
    Question { question: String, options: Vec<String>, input: String, reply: Option<oneshot::Sender<String>> },
}

struct App {
    blocks: Vec<Blk>,
    input: String,
    streaming: Option<String>,
    busy: bool,
    spinner: usize,
    turn_start: Option<Instant>,
    last_event: Instant,
    used_tokens: usize,
    scroll_from_bottom: u16,
    modal: Option<Modal>,
    palette_sel: usize,
    quit: bool,
    responding: bool,
    title: String,
    todos_total: usize,
    todos_done: usize,
    cwd: String,
    model: String,
    mode: String,
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
            spinner: 0,
            turn_start: None,
            last_event: Instant::now(),
            used_tokens: BASE_CONTEXT_TOKENS,
            scroll_from_bottom: 0,
            modal: None,
            palette_sel: 0,
            quit: false,
            responding: false,
            title: "mimo".to_string(),
            todos_total: 0,
            todos_done: 0,
            cwd,
            model,
            mode,
        }
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
                self.responding = true;
                self.last_event = Instant::now();
                self.streaming.get_or_insert_with(String::new).push_str(&d);
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
                self.blocks.push(Blk::Tool { kind, summary: s, active: true });
            }
            UiEvent::ToolDone => {}
            UiEvent::Diff { start_line, old, new } => {
                self.blocks.push(Blk::Diff { start: start_line, old, new });
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
                self.modal = Some(Modal::Approval { summary, plan, reply: Some(reply) });
            }
            UiEvent::Question { question, options, reply } => {
                self.modal = Some(Modal::Question { question, options, input: String::new(), reply: Some(reply) });
            }
            UiEvent::Status { model, mode } => {
                self.model = model;
                self.mode = mode;
            }
            UiEvent::TurnDone => {
                self.flush_stream();
                if let Some(t) = self.turn_start.take() {
                    let secs = t.elapsed().as_secs_f64();
                    self.blocks.push(Blk::Info(format!("Turn completed in {secs:.1}s.")));
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
}

/// Light shell-command highlighting for `Run` lines: flags orange, paths blue, operators gray.
fn highlight_cmd(cmd: &str) -> Vec<Span<'static>> {
    let mut spans = vec![];
    for (i, tok) in cmd.split(' ').enumerate() {
        if i > 0 {
            spans.push(Span::styled(" ".to_string(), Style::default().bg(BG)));
        }
        let style = if tok.starts_with('-') {
            Style::default().fg(ORANGE).bg(BG)
        } else if matches!(tok, "|" | "||" | "&&" | ";" | ">" | ">>" | "<" | "2>/dev/null") {
            Style::default().fg(GRAY).bg(BG)
        } else if tok.contains('/') {
            Style::default().fg(BLUE).bg(BG)
        } else {
            Style::default().fg(DIM).bg(BG)
        };
        spans.push(Span::styled(tok.to_string(), style));
    }
    spans
}

fn diamond_color(kind: &str) -> Color {
    match kind {
        "Read" => RED,
        "Run" => GREEN,
        "Edit" | "Write" => BLUE,
        "Grep" | "List" | "Glob" | "Fetch" => CYAN,
        "Subagent" | "Task" => PURPLE,
        "Ask" => ORANGE,
        _ => DIM,
    }
}

pub async fn run(cfg: Config, resume: Option<Resume>) -> Result<()> {
    let (ev_tx, mut ev_rx) = mpsc::unbounded_channel::<UiEvent>();
    let (in_tx, mut in_rx) = mpsc::unbounded_channel::<String>();
    let emitter = Emitter::Channel(ev_tx.clone());

    let cwd = std::env::current_dir().unwrap_or_default().display().to_string();
    let model = cfg.model.clone();
    let mode = cfg.mode_label();

    let agent_emitter = emitter.clone();
    tokio::spawn(async move {
        let mut agent = Agent::new_with(cfg, agent_emitter.clone());
        if let Some(r) = &resume {
            session::apply(&mut agent, r);
        }
        agent_emitter.status(&agent.cfg.model, &agent.cfg.mode_label());
        while let Some(line) = in_rx.recv().await {
            match line.as_str() {
                "/flush" => {
                    let s = agent.flush_memory().await;
                    agent_emitter.info(&s);
                }
                "/dream" => {
                    let s = agent.dream_memory().await;
                    agent_emitter.info(&s);
                }
                _ if line.starts_with('/') => handle_slash(&mut agent, &line, &agent_emitter),
                _ => {
                    let _ = agent.run_turn(&line).await;
                    session::save(&agent);
                }
            }
            ev_tx.send(UiEvent::TurnDone).ok();
        }
    });

    enable_raw_mode()?;
    let mut out = std::io::stdout();
    crossterm::execute!(out, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(out);
    let mut terminal: Terminal<CrosstermBackend<Stdout>> = Terminal::new(backend)?;

    let mut app = App::new(cwd, model, mode);
    let res = event_loop(&mut terminal, &mut app, &in_tx, &mut ev_rx).await;

    disable_raw_mode()?;
    crossterm::execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    res
}

async fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut App,
    in_tx: &mpsc::UnboundedSender<String>,
    ev_rx: &mut mpsc::UnboundedReceiver<UiEvent>,
) -> Result<()> {
    let mut last_title = String::new();
    loop {
        // Dynamic window title with state, like the real CLI.
        let want = if app.busy {
            format!("{} — {} - mimo", if app.responding { "Responding" } else { "Thinking" }, app.title)
        } else {
            format!("{} - mimo", app.title)
        };
        if want != last_title {
            crossterm::execute!(std::io::stdout(), crossterm::terminal::SetTitle(&want)).ok();
            last_title = want;
        }
        terminal.draw(|f| render(f, app))?;
        if event::poll(Duration::from_millis(80))? {
            if let Event::Key(k) = event::read()? {
                if k.kind == KeyEventKind::Press {
                    if app.modal.is_some() {
                        handle_modal_key(app, k.code);
                    } else {
                        handle_key(app, k.code, k.modifiers, in_tx);
                    }
                }
            }
        }
        while let Ok(ev) = ev_rx.try_recv() {
            app.apply(ev);
        }
        if app.busy {
            app.spinner = (app.spinner + 1) % SPINNER.len();
        }
        if app.quit {
            break;
        }
    }
    Ok(())
}

fn handle_key(app: &mut App, code: KeyCode, mods: KeyModifiers, in_tx: &mpsc::UnboundedSender<String>) {
    let has_palette = !app.palette_matches().is_empty();
    match code {
        KeyCode::Char('c') if mods.contains(KeyModifiers::CONTROL) => app.quit = true,
        KeyCode::Char(c) => {
            app.input.push(c);
            app.palette_sel = 0;
        }
        KeyCode::Backspace => {
            app.input.pop();
            app.palette_sel = 0;
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
        KeyCode::Enter => {
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

fn handle_modal_key(app: &mut App, code: KeyCode) {
    match app.modal.as_mut() {
        Some(Modal::Approval { .. }) => {
            let d = match code {
                KeyCode::Char('y') | KeyCode::Char('Y') => Some(true),
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => Some(false),
                _ => None,
            };
            if let Some(d) = d {
                if let Some(Modal::Approval { summary, reply, .. }) = app.modal.as_mut() {
                    if let Some(r) = reply.take() {
                        r.send(d).ok();
                    }
                    let s = summary.clone();
                    app.modal = None;
                    app.blocks.push(Blk::Info(format!("{} {s}", if d { "✓ approved" } else { "✗ denied" })));
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

fn now_label() -> String {
    chrono::Local::now().format("%-I:%M %p").to_string()
}

// ---- rendering ----

fn render(f: &mut ratatui::Frame, app: &App) {
    let area = f.area();
    // Base background.
    f.render_widget(Block::default().style(Style::default().bg(BG)), area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // top margin (blank row above header, like the real CLI)
            Constraint::Length(1), // header
            Constraint::Min(1),    // transcript
            Constraint::Length(1), // working line / gap above box
            Constraint::Length(3), // input box (3 rows: top, prompt, bottom)
            Constraint::Length(1), // spacer between box and footer
            Constraint::Length(1), // footer
        ])
        .split(area);

    render_header(f, chunks[1], app);
    render_transcript(f, chunks[2], app);
    render_working(f, chunks[3], app);
    render_input(f, chunks[4], app);
    render_footer(f, chunks[6], app);

    if app.input.starts_with('/') && !app.palette_matches().is_empty() {
        render_palette(f, chunks[2], app);
    }
    if app.modal.is_some() {
        render_modal(f, area, app);
    }
}

fn render_header(f: &mut ratatui::Frame, area: Rect, app: &App) {
    let used = if app.used_tokens >= 1000 {
        format!("{}K", app.used_tokens / 1000)
    } else {
        format!("{}", app.used_tokens)
    };
    let todos = if app.todos_total > 0 {
        format!("│ {}/{} ✓ ", app.todos_done, app.todos_total)
    } else {
        String::new()
    };
    let right = format!("│ {used} / 512K {todos}");
    let w = area.width as usize;
    let left = format!("  {}", short_path(&app.cwd));
    let pad = w.saturating_sub(left.chars().count() + right.chars().count());
    let line = Line::from(vec![
        Span::styled(left, Style::default().fg(GRAY).bg(BG)),
        Span::styled(" ".repeat(pad), Style::default().bg(BG)),
        Span::styled(right, Style::default().fg(DIM).bg(BG)),
    ]);
    f.render_widget(Paragraph::new(line).style(Style::default().bg(BG)), area);
}

fn render_transcript(f: &mut ratatui::Frame, area: Rect, app: &App) {
    let inner_w = area.width.saturating_sub(4) as usize;
    let lines = transcript_lines(app, inner_w);
    let total = lines.len() as u16;
    let view_h = area.height;
    let max_scroll = total.saturating_sub(view_h);
    let scroll = max_scroll.saturating_sub(app.scroll_from_bottom);
    f.render_widget(
        Paragraph::new(lines).style(Style::default().bg(BG).fg(TXT)).scroll((scroll, 0)),
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
    let indent = "    ";
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

    if app.blocks.is_empty() && app.streaming.is_none() {
        // Minimal empty state with breathing room below the header.
        out.push(Line::from(""));
        out.push(Line::from(""));
        out.push(Line::from(Span::styled(
            format!("    {} · {} · type / for commands", app.model, app.mode),
            Style::default().fg(DIM).bg(BG),
        )));
        return out;
    }

    for blk in &app.blocks {
        match blk {
            Blk::User { text, ts } => {
                // Shaded full-width row: "❯ text" + right timestamp.
                let body = format!("❯ {text}");
                let pad = w.saturating_sub(body.chars().count() + ts.chars().count() + 1);
                out.push(Line::from(vec![
                    Span::styled("  ".to_string(), Style::default().bg(USER_BG)),
                    Span::styled("❯ ".to_string(), Style::default().fg(WHITE).bg(USER_BG).add_modifier(Modifier::BOLD)),
                    Span::styled(text.clone(), Style::default().fg(WHITE).bg(USER_BG)),
                    Span::styled(" ".repeat(pad), Style::default().bg(USER_BG)),
                    Span::styled(format!("{ts} "), Style::default().fg(DIM).bg(USER_BG)),
                ]));
                out.push(Line::from(""));
            }
            Blk::Tool { kind, summary, active } => {
                let dcol = diamond_color(kind);
                // `❙` marks the currently-running item (left margin); colored bar for Run/Edit; else blank.
                let gutter = if *active {
                    Span::styled("  ❙ ".to_string(), Style::default().fg(GREEN).add_modifier(Modifier::BOLD))
                } else if matches!(kind.as_str(), "Run" | "Edit" | "Write") {
                    Span::styled("  │ ".to_string(), Style::default().fg(dcol))
                } else {
                    Span::styled("    ".to_string(), Style::default().bg(BG))
                };
                let (verb, rest) = match summary.split_once(' ') {
                    Some((v, r)) => (v.to_string(), r.to_string()),
                    None => (summary.clone(), String::new()),
                };
                let mut spans = vec![
                    gutter,
                    Span::styled("◆ ".to_string(), Style::default().fg(dcol)),
                    Span::styled(format!("{verb} "), Style::default().fg(WHITE)),
                ];
                if kind == "Run" {
                    spans.extend(highlight_cmd(&rest));
                } else if rest.contains('/') {
                    spans.push(Span::styled(rest, Style::default().fg(BLUE)));
                } else {
                    spans.push(Span::styled(rest, Style::default().fg(DIM)));
                }
                out.push(Line::from(spans));
            }
            Blk::Diff { start, old, new } => {
                let mut n = *start;
                for l in old.split('\n') {
                    out.push(diff_row(n, '-', l, RED));
                    n += 1;
                }
                let mut n2 = *start;
                for l in new.split('\n') {
                    out.push(diff_row(n2, '+', l, GREEN));
                    n2 += 1;
                }
                out.push(Line::from(""));
            }
            Blk::Assistant { text, ts } => {
                out.extend(render_assistant(text, ts, w));
            }
            Blk::Todos(items) => {
                for (content, status) in items {
                    let (mark, c) = match status.as_str() {
                        "completed" => ("✔", GREEN),
                        "in_progress" => ("▸", ORANGE),
                        _ => ("○", DIM),
                    };
                    out.push(Line::from(vec![
                        Span::styled(format!("{indent}{mark} "), Style::default().fg(c)),
                        Span::styled(content.clone(), Style::default().fg(TXT)),
                    ]));
                }
            }
            Blk::Info(t) => out.push(Line::from(Span::styled(
                format!("{indent}{t}"),
                Style::default().fg(DIM).bg(BG),
            ))),
            Blk::Error(t) => out.push(Line::from(Span::styled(
                format!("{indent}{t}"),
                Style::default().fg(RED).bg(BG),
            ))),
        }
    }
    if let Some(s) = &app.streaming {
        for l in wrap(s, w) {
            out.push(render_md_line(&l, indent));
        }
    }
    out
}

fn diff_row(n: usize, marker: char, code: &str, color: Color) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("    {n:>4} {marker} "), Style::default().fg(DIM)),
        Span::styled(code.to_string(), Style::default().fg(color)),
    ])
}

/// Light markdown: `code`→cyan, **bold**→bold white, [text](url)→blue-underlined text + dim (url),
/// leading `- `/`* ` bullets → `·`.
fn render_md_line(l: &str, indent: &str) -> Line<'static> {
    let mut spans = vec![Span::styled(indent.to_string(), Style::default().bg(BG))];
    // Normalize leading bullets to the real CLI's middot.
    let mut rest = l.to_string();
    let trimmed = rest.trim_start();
    if let Some(b) = trimmed.strip_prefix("- ").or_else(|| trimmed.strip_prefix("* ")) {
        let lead = &rest[..rest.len() - trimmed.len()];
        spans.push(Span::styled(format!("{lead}· "), Style::default().fg(DIM).bg(BG)));
        rest = b.to_string();
    }
    // Tokenize links first, then style the inter-link text.
    let link = regex::Regex::new(r"\[([^\]]+)\]\(([^)]+)\)").unwrap();
    let mut last = 0;
    for cap in link.captures_iter(&rest) {
        let m = cap.get(0).unwrap();
        spans.extend(style_inline(&rest[last..m.start()]));
        spans.push(Span::styled(cap[1].to_string(), Style::default().fg(BLUE).bg(BG).add_modifier(Modifier::UNDERLINED)));
        spans.push(Span::styled(format!(" ({})", &cap[2]), Style::default().fg(DIM).bg(BG)));
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
                spans.push(Span::styled(seg.to_string(), Style::default().fg(CYAN).bg(BG)));
            }
        } else {
            // handle **bold** within non-code text
            for (j, part) in seg.split("**").enumerate() {
                if part.is_empty() {
                    continue;
                }
                let st = if j % 2 == 1 {
                    Style::default().fg(WHITE).bg(BG).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(TXT).bg(BG)
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
fn render_assistant(text: &str, ts: &str, w: usize) -> Vec<Line<'static>> {
    let indent = "    ";
    let lines: Vec<&str> = text.split('\n').collect();
    let mut out: Vec<Line> = vec![];
    let mut first = true;
    let mut i = 0;
    while i < lines.len() {
        if is_table_row(lines[i]) && i + 1 < lines.len() && is_separator_row(lines[i + 1]) {
            let mut block = vec![];
            while i < lines.len() && is_table_row(lines[i]) {
                block.push(lines[i]);
                i += 1;
            }
            out.extend(render_table(&block, indent));
            first = false;
            continue;
        }
        let raw = lines[i];
        let pieces = if raw.is_empty() { vec![String::new()] } else { textwrap::wrap(raw, w.max(8)).iter().map(|s| s.to_string()).collect() };
        for piece in pieces {
            let mut ln = render_md_line(&piece, indent);
            if first {
                ln = with_right_ts(ln, ts, w);
                first = false;
            }
            out.push(ln);
        }
        i += 1;
    }
    out.push(Line::from(""));
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
    let body: Vec<Vec<String>> = block[2..].iter().map(|r| split_cells(r)).collect();
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
    let border = Style::default().fg(GRAY).bg(BG);
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
                Style::default().fg(WHITE).bg(BG).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(TXT).bg(BG)
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
    spans.push(Span::styled(" ".repeat(pad), Style::default().bg(BG)));
    spans.push(Span::styled(format!("{ts} "), Style::default().fg(DIM).bg(BG)));
    Line::from(spans)
}

fn render_working(f: &mut ratatui::Frame, area: Rect, app: &App) {
    if !app.busy {
        f.render_widget(Block::default().style(Style::default().bg(BG)), area);
        return;
    }
    let turn = app.turn_start.map(|t| t.elapsed().as_secs_f64()).unwrap_or(0.0);
    let local = app.last_event.elapsed().as_secs_f64();
    let phase = if app.responding { "Responding…" } else { "Waiting…" };
    let left = format!("  {} {phase} {:.1}s", SPINNER[app.spinner], local);
    let right = format!("{:.0}s ⇣{:.1}k [✗] ", turn, app.used_tokens as f64 / 1000.0);
    let w = area.width as usize;
    let pad = w.saturating_sub(left.chars().count() + right.chars().count());
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(left, Style::default().fg(PURPLE).bg(BG)),
            Span::styled(" ".repeat(pad), Style::default().bg(BG)),
            Span::styled(right, Style::default().fg(DIM).bg(BG)),
        ]))
        .style(Style::default().bg(BG)),
        area,
    );
}

fn render_input(f: &mut ratatui::Frame, area: Rect, app: &App) {
    let title = Line::from(vec![
        Span::styled(" Mimo Build ", Style::default().fg(DIM)),
        Span::styled("· ", Style::default().fg(GRAY)),
        Span::styled(format!("{} ", app.mode), Style::default().fg(DIM)),
    ])
    .right_aligned();
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(GRAY))
        .title_bottom(title)
        .style(Style::default().bg(BG));
    let prompt_line = if app.input.is_empty() {
        Line::from(vec![
            Span::styled(" ❯ ", Style::default().fg(BLUE)),
            Span::styled("Build anything", Style::default().fg(GRAY)),
        ])
    } else {
        Line::from(vec![
            Span::styled(" ❯ ", Style::default().fg(BLUE)),
            Span::styled(app.input.clone(), Style::default().fg(TXT)),
        ])
    };
    // Inset the box 2 columns each side (measured from the real CLI: box width = term-4).
    let r = Rect {
        x: area.x + 2,
        y: area.y,
        width: area.width.saturating_sub(4),
        height: area.height,
    };
    f.render_widget(Paragraph::new(prompt_line).block(block).style(Style::default().bg(BG)), r);
    // Single interior row; cursor at first text column: left border + " ❯ " (3 cells) = +4.
    f.set_cursor_position((r.x + 4 + app.input.chars().count() as u16, r.y + 1));
}

fn render_footer(f: &mut ratatui::Frame, area: Rect, app: &App) {
    let pairs: &[(&str, &str)] = if app.input.starts_with('/') {
        &[("Enter", "run"), ("Tab", "complete"), ("↑↓", "select"), ("Ctrl+.", "shortcuts")]
    } else if app.busy {
        &[("Shift+Tab", "mode"), ("Ctrl+C", "cancel"), ("Ctrl+Enter", "interject"), ("Ctrl+.", "shortcuts")]
    } else {
        &[("Enter", "send"), ("Shift+Tab", "mode"), ("Ctrl+.", "shortcuts")]
    };
    let mut spans = vec![Span::styled("  ", Style::default().bg(BG))];
    for (i, (k, v)) in pairs.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled("  │  ", Style::default().fg(GRAY).bg(BG)));
        }
        spans.push(Span::styled(k.to_string(), Style::default().fg(TXT).bg(BG).add_modifier(Modifier::BOLD)));
        spans.push(Span::styled(format!(":{v}"), Style::default().fg(DIM).bg(BG)));
    }
    f.render_widget(Paragraph::new(Line::from(spans)).style(Style::default().bg(BG)), area);
}

/// Slash-command palette, drawn just above the input box.
fn render_palette(f: &mut ratatui::Frame, transcript_area: Rect, app: &App) {
    let matches = app.palette_matches();
    let rows = (matches.len() as u16).min(8);
    let h = rows + 2;
    let area = Rect {
        x: transcript_area.x + 1,
        y: transcript_area.y + transcript_area.height.saturating_sub(h),
        width: transcript_area.width.saturating_sub(2),
        height: h,
    };
    f.render_widget(Clear, area);
    let mut lines = vec![];
    for (i, (name, desc)) in matches.iter().take(8).enumerate() {
        let sel = i == app.palette_sel.min(matches.len().saturating_sub(1));
        let bg = if sel { USER_BG } else { BG };
        let marker = if sel { "❯ " } else { "  " };
        let namew = 14usize;
        let pad = namew.saturating_sub(name.chars().count());
        lines.push(Line::from(vec![
            Span::styled(format!(" {marker}"), Style::default().fg(BLUE).bg(bg)),
            Span::styled(name.to_string(), Style::default().fg(if sel { WHITE } else { TXT }).bg(bg).add_modifier(if sel { Modifier::BOLD } else { Modifier::empty() })),
            Span::styled(" ".repeat(pad + 2), Style::default().bg(bg)),
            Span::styled(desc.to_string(), Style::default().fg(DIM).bg(bg)),
        ]));
    }
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(GRAY))
        .style(Style::default().bg(BG));
    f.render_widget(Paragraph::new(lines).block(block).style(Style::default().bg(BG)), area);
}

fn render_modal(f: &mut ratatui::Frame, area: Rect, app: &App) {
    let modal = app.modal.as_ref().unwrap();
    let r = centered(70, 50, area);
    f.render_widget(Clear, r);
    let (title, text): (&str, Vec<Line>) = match modal {
        Modal::Approval { summary, plan, .. } => {
            let mut t = vec![];
            if let Some(plan) = plan {
                t.push(Line::from(Span::styled("Proposed plan:", Style::default().fg(WHITE).add_modifier(Modifier::BOLD))));
                for l in plan.lines() {
                    t.push(Line::from(Span::styled(l.to_string(), Style::default().fg(TXT))));
                }
            } else {
                t.push(Line::from(Span::styled("Approve tool call?", Style::default().fg(WHITE).add_modifier(Modifier::BOLD))));
                t.push(Line::from(Span::styled(summary.clone(), Style::default().fg(TXT))));
            }
            t.push(Line::from(""));
            t.push(Line::from(Span::styled("[y] approve    [n] deny", Style::default().fg(ORANGE))));
            (" approval ", t)
        }
        Modal::Question { question, options, input, .. } => {
            let mut t = vec![Line::from(Span::styled(question.clone(), Style::default().fg(WHITE).add_modifier(Modifier::BOLD)))];
            for (i, o) in options.iter().enumerate() {
                t.push(Line::from(Span::styled(format!("  {}. {o}", i + 1), Style::default().fg(TXT))));
            }
            t.push(Line::from(""));
            t.push(Line::from(vec![
                Span::styled("answer› ", Style::default().fg(BLUE)),
                Span::styled(input.clone(), Style::default().fg(TXT)),
            ]));
            (" question ", t)
        }
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(ORANGE))
        .title(title)
        .style(Style::default().bg(BG));
    f.render_widget(Paragraph::new(text).block(block).wrap(Wrap { trim: false }).style(Style::default().bg(BG)), r);
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
        "/help" | "/?" => emitter.info("commands: /model /plan /approve /yolo /goal /flush /dream /clear /inspect /quit"),
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
        "/yolo" => {
            agent.cfg.always_approve = !agent.cfg.always_approve;
            emitter.info(&format!("(auto-approve {})", if agent.cfg.always_approve { "ON" } else { "OFF" }));
        }
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
