// Full-screen ratatui TUI (the default UX). Mirrors the real mimo terminal UI: a scrollable
// transcript viewport, a bottom input box, a status line, live streaming, styled tool-activity
// blocks, a todo panel, and an approval modal. The agent runs in its own task and communicates
// with the render loop over channels (see event.rs).

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Terminal;
use std::io::Stdout;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

use crate::agent::Agent;
use crate::config::Config;
use crate::event::{Emitter, UiEvent};
use crate::session::{self, Resume};

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

enum Blk {
    User(String),
    Assistant(String),
    Tool(String),
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
    scroll_from_bottom: u16,
    modal: Option<Modal>,
    quit: bool,
    model: String,
    plan_mode: bool,
}

impl App {
    fn new(model: String, plan_mode: bool) -> Self {
        App {
            blocks: vec![],
            input: String::new(),
            streaming: None,
            busy: false,
            spinner: 0,
            scroll_from_bottom: 0,
            modal: None,
            quit: false,
            model,
            plan_mode,
        }
    }

    fn flush_stream(&mut self) {
        if let Some(s) = self.streaming.take() {
            let s = s.trim_end().to_string();
            if !s.is_empty() {
                self.blocks.push(Blk::Assistant(s));
            }
        }
    }

    fn apply(&mut self, ev: UiEvent) {
        match ev {
            UiEvent::AssistantDelta(d) => {
                self.streaming.get_or_insert_with(String::new).push_str(&d);
            }
            UiEvent::ToolStart(s) => {
                self.flush_stream();
                self.blocks.push(Blk::Tool(s));
            }
            UiEvent::ToolDone => {}
            UiEvent::Todos(items) => {
                self.flush_stream();
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
            UiEvent::Status { model, plan_mode } => {
                self.model = model;
                self.plan_mode = plan_mode;
            }
            UiEvent::TurnDone => {
                self.flush_stream();
                self.busy = false;
            }
        }
    }

    /// Render the transcript as wrapped lines.
    fn transcript_lines(&self, width: u16) -> Vec<Line<'static>> {
        let w = width.max(10) as usize;
        let mut lines: Vec<Line> = vec![];
        let wrap = |text: &str, w: usize| -> Vec<String> {
            let mut out = vec![];
            for raw in text.split('\n') {
                if raw.is_empty() {
                    out.push(String::new());
                } else {
                    for piece in textwrap::wrap(raw, w) {
                        out.push(piece.to_string());
                    }
                }
            }
            out
        };
        for blk in &self.blocks {
            match blk {
                Blk::User(t) => {
                    for (i, l) in wrap(t, w.saturating_sub(2)).into_iter().enumerate() {
                        let prefix = if i == 0 { "› " } else { "  " };
                        lines.push(Line::from(Span::styled(
                            format!("{prefix}{l}"),
                            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                        )));
                    }
                    lines.push(Line::from(""));
                }
                Blk::Assistant(t) => {
                    for l in wrap(t, w) {
                        lines.push(Line::from(l));
                    }
                    lines.push(Line::from(""));
                }
                Blk::Tool(t) => {
                    lines.push(Line::from(Span::styled(
                        format!("• {t}"),
                        Style::default().fg(Color::DarkGray),
                    )));
                }
                Blk::Todos(items) => {
                    for (content, status) in items {
                        let (mark, color) = match status.as_str() {
                            "completed" => ("✔", Color::Green),
                            "in_progress" => ("▸", Color::Yellow),
                            _ => ("○", Color::DarkGray),
                        };
                        lines.push(Line::from(vec![
                            Span::styled(format!("  {mark} "), Style::default().fg(color)),
                            Span::styled(content.clone(), Style::default().fg(Color::Gray)),
                        ]));
                    }
                }
                Blk::Info(t) => lines.push(Line::from(Span::styled(
                    t.clone(),
                    Style::default().fg(Color::Magenta).add_modifier(Modifier::DIM),
                ))),
                Blk::Error(t) => lines.push(Line::from(Span::styled(
                    t.clone(),
                    Style::default().fg(Color::Red),
                ))),
            }
        }
        // Live streaming buffer.
        if let Some(s) = &self.streaming {
            for l in wrap(s, w) {
                lines.push(Line::from(l));
            }
        }
        lines
    }
}

pub async fn run(cfg: Config, resume: Option<Resume>) -> Result<()> {
    let (ev_tx, mut ev_rx) = mpsc::unbounded_channel::<UiEvent>();
    let (in_tx, mut in_rx) = mpsc::unbounded_channel::<String>();
    let emitter = Emitter::Channel(ev_tx.clone());

    let model = cfg.model.clone();
    let plan_mode = cfg.plan_mode;
    let auth_source = cfg.auth_source.clone();

    // Agent runs in its own task and owns the Agent across turns.
    let agent_emitter = emitter.clone();
    tokio::spawn(async move {
        let mut agent = Agent::new_with(cfg, agent_emitter.clone());
        if let Some(r) = &resume {
            session::apply(&mut agent, r);
        }
        agent_emitter.status(&agent.cfg.model, agent.cfg.plan_mode);
        while let Some(line) = in_rx.recv().await {
            if line.starts_with('/') {
                handle_slash(&mut agent, &line, &agent_emitter);
            } else {
                let _ = agent.run_turn(&line).await;
                session::save(&agent);
            }
            ev_tx.send(UiEvent::TurnDone).ok();
        }
    });

    // Terminal setup.
    enable_raw_mode()?;
    let mut out = std::io::stdout();
    crossterm::execute!(out, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(out);
    let mut terminal: Terminal<CrosstermBackend<Stdout>> = Terminal::new(backend)?;

    let mut app = App::new(model, plan_mode);
    app.blocks.push(Blk::Info(format!(
        "Mimo Build (mimo-rs) · model {} · {} · /help for commands",
        app.model, auth_source
    )));

    let res = event_loop(&mut terminal, &mut app, &in_tx, &mut ev_rx).await;

    // Teardown.
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
    loop {
        terminal.draw(|f| render(f, app))?;

        if event::poll(Duration::from_millis(80))? {
            if let Event::Key(k) = event::read()? {
                if k.kind != KeyEventKind::Press {
                    // ignore key repeats/releases
                } else if app.modal.is_some() {
                    handle_modal_key(app, k.code);
                } else {
                    handle_key(app, k.code, k.modifiers, in_tx);
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
    match code {
        KeyCode::Char('c') if mods.contains(KeyModifiers::CONTROL) => app.quit = true,
        KeyCode::Char(c) => app.input.push(c),
        KeyCode::Backspace => {
            app.input.pop();
        }
        KeyCode::Enter => {
            let line = app.input.trim().to_string();
            if line.is_empty() {
                return;
            }
            app.input.clear();
            app.scroll_from_bottom = 0;
            if matches!(line.as_str(), "/quit" | "/exit" | "/q") {
                app.quit = true;
                return;
            }
            if !line.starts_with('/') {
                app.flush_stream();
                app.blocks.push(Blk::User(line.clone()));
            }
            app.busy = true;
            in_tx.send(line).ok();
        }
        KeyCode::PageUp => app.scroll_from_bottom = app.scroll_from_bottom.saturating_add(5),
        KeyCode::Up => app.scroll_from_bottom = app.scroll_from_bottom.saturating_add(1),
        KeyCode::PageDown => app.scroll_from_bottom = app.scroll_from_bottom.saturating_sub(5),
        KeyCode::Down => app.scroll_from_bottom = app.scroll_from_bottom.saturating_sub(1),
        _ => {}
    }
}

fn handle_modal_key(app: &mut App, code: KeyCode) {
    match app.modal.as_mut() {
        Some(Modal::Approval { .. }) => {
            let decision = match code {
                KeyCode::Char('y') | KeyCode::Char('Y') => Some(true),
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => Some(false),
                _ => None,
            };
            if let (Some(d), Some(Modal::Approval { summary, reply, .. })) = (decision, app.modal.take().as_mut()) {
                if let Some(r) = reply.take() {
                    r.send(d).ok();
                }
                app.blocks.push(Blk::Info(format!("{} {summary}", if d { "✓ approved" } else { "✗ denied" })));
            }
        }
        Some(Modal::Question { options, input, .. }) => match code {
            KeyCode::Char(c) => input.push(c),
            KeyCode::Backspace => {
                input.pop();
            }
            KeyCode::Esc => {
                finish_question(app, String::new());
            }
            KeyCode::Enter => {
                // A bare number selects an option; otherwise the typed text is the answer.
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

fn render(f: &mut ratatui::Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(3), Constraint::Length(1)])
        .split(f.area());

    // Transcript.
    let inner_w = chunks[0].width.saturating_sub(2);
    let lines = app.transcript_lines(inner_w);
    let total = lines.len() as u16;
    let view_h = chunks[0].height.saturating_sub(2);
    let max_scroll = total.saturating_sub(view_h);
    let scroll = max_scroll.saturating_sub(app.scroll_from_bottom);
    let transcript = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0))
        .block(Block::default().borders(Borders::ALL).title(" mimo "));
    f.render_widget(transcript, chunks[0]);

    // Input.
    let input = Paragraph::new(Line::from(vec![
        Span::styled("› ", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        Span::raw(&app.input),
    ]))
    .block(Block::default().borders(Borders::ALL));
    f.render_widget(input, chunks[1]);
    // Cursor in the input box.
    f.set_cursor_position((chunks[1].x + 4 + app.input.chars().count() as u16, chunks[1].y + 1));

    // Status line.
    let spin = if app.busy { format!("{} working  ", SPINNER[app.spinner]) } else { String::new() };
    let status = format!(
        " {spin}model: {}  ·  plan: {}  ·  ^C quit · PgUp/PgDn scroll",
        app.model,
        if app.plan_mode { "on" } else { "off" }
    );
    f.render_widget(
        Paragraph::new(status).style(Style::default().fg(Color::DarkGray)),
        chunks[2],
    );

    // Modal overlay (approval or question).
    if let Some(modal) = &app.modal {
        let area = centered(70, 50, f.area());
        f.render_widget(Clear, area);
        let (title, text): (&str, Vec<Line>) = match modal {
            Modal::Approval { summary, plan, .. } => {
                let mut text = vec![];
                if let Some(plan) = plan {
                    text.push(Line::from(Span::styled("Proposed plan:", Style::default().add_modifier(Modifier::BOLD))));
                    for l in plan.lines() {
                        text.push(Line::from(l.to_string()));
                    }
                } else {
                    text.push(Line::from(Span::styled("Approve tool call?", Style::default().add_modifier(Modifier::BOLD))));
                    text.push(Line::from(summary.clone()));
                }
                text.push(Line::from(""));
                text.push(Line::from(Span::styled("[y] approve    [n] deny", Style::default().fg(Color::Yellow))));
                (" approval ", text)
            }
            Modal::Question { question, options, input, .. } => {
                let mut text = vec![Line::from(Span::styled(question.clone(), Style::default().add_modifier(Modifier::BOLD)))];
                for (i, o) in options.iter().enumerate() {
                    text.push(Line::from(format!("  {}. {o}", i + 1)));
                }
                text.push(Line::from(""));
                text.push(Line::from(vec![
                    Span::styled("answer› ", Style::default().fg(Color::Cyan)),
                    Span::raw(input.clone()),
                ]));
                text.push(Line::from(Span::styled("(type a number or free text, Enter to submit)", Style::default().add_modifier(Modifier::DIM))));
                (" question ", text)
            }
        };
        let popup = Paragraph::new(text)
            .wrap(Wrap { trim: false })
            .alignment(Alignment::Left)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(title)
                    .style(Style::default().fg(Color::Yellow)),
            );
        f.render_widget(popup, area);
    }
}

fn centered(pct_x: u16, pct_y: u16, r: Rect) -> Rect {
    let v = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - pct_y) / 2),
            Constraint::Percentage(pct_y),
            Constraint::Percentage((100 - pct_y) / 2),
        ])
        .split(r);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - pct_x) / 2),
            Constraint::Percentage(pct_x),
            Constraint::Percentage((100 - pct_x) / 2),
        ])
        .split(v[1])[1]
}

fn handle_slash(agent: &mut Agent, line: &str, emitter: &Emitter) {
    let mut parts = line.splitn(2, ' ');
    let cmd = parts.next().unwrap_or("");
    let arg = parts.next().unwrap_or("").trim();
    match cmd {
        "/help" | "/?" => emitter.info(
            "/model [id] · /plan · /yolo · /clear · /quit   (PgUp/PgDn scroll, ^C quit)",
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
        "/yolo" => {
            agent.cfg.always_approve = !agent.cfg.always_approve;
            emitter.info(&format!("(auto-approve {})", if agent.cfg.always_approve { "ON" } else { "OFF" }));
        }
        other => emitter.info(&format!("unknown command: {other}")),
    }
    emitter.status(&agent.cfg.model, agent.cfg.plan_mode);
}
