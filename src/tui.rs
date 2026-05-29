// Interactive REPL. Streams assistant output live, supports the core slash commands.

use anyhow::Result;
use rustyline::error::ReadlineError;
use rustyline::DefaultEditor;

use crate::agent::Agent;
use crate::config::Config;
use crate::session::{self, Resume};

pub async fn run(cfg: Config, resume: Option<Resume>) -> Result<()> {
    banner(&cfg);
    let mut agent = Agent::new(cfg);
    if let Some(r) = &resume {
        session::apply(&mut agent, r);
    }
    let mut rl = DefaultEditor::new()?;

    loop {
        let prompt = "\x1b[1;36mmimo ›\x1b[0m ";
        match rl.readline(prompt) {
            Ok(line) => {
                let line = line.trim().to_string();
                if line.is_empty() {
                    continue;
                }
                rl.add_history_entry(&line).ok();

                if line.starts_with('/') {
                    if handle_slash(&line, &mut agent) {
                        break;
                    }
                    continue;
                }

                if let Err(e) = agent.run_turn(&line).await {
                    eprintln!("\x1b[31merror:\x1b[0m {e}");
                }
                session::save(&agent);
                println!();
            }
            Err(ReadlineError::Interrupted) => {
                println!("(^C — type /quit to exit)");
            }
            Err(ReadlineError::Eof) => break,
            Err(e) => {
                eprintln!("input error: {e}");
                break;
            }
        }
    }
    Ok(())
}

/// Returns true if the REPL should exit.
fn handle_slash(line: &str, agent: &mut Agent) -> bool {
    let mut parts = line.splitn(2, ' ');
    let cmd = parts.next().unwrap_or("");
    let arg = parts.next().unwrap_or("").trim();

    match cmd {
        "/quit" | "/exit" | "/q" => return true,
        "/help" | "/?" => help(),
        "/clear" | "/new" => {
            agent.reset();
            println!("\x1b[2m(new session)\x1b[0m");
        }
        "/model" => {
            if arg.is_empty() {
                println!("model: {}", agent.cfg.model);
            } else {
                agent.cfg.model = arg.to_string();
                agent.reset();
                println!("\x1b[2m(model → {})\x1b[0m", agent.cfg.model);
            }
        }
        "/approve" => {
            agent.approve_plan();
            println!("\x1b[32m(plan approved — mutations enabled)\x1b[0m");
        }
        "/plan" => {
            agent.cfg.plan_mode = !agent.cfg.plan_mode;
            println!("\x1b[2m(plan mode {})\x1b[0m", if agent.cfg.plan_mode { "ON" } else { "OFF" });
        }
        "/yolo" => {
            agent.cfg.always_approve = !agent.cfg.always_approve;
            println!(
                "\x1b[2m(auto-approve {})\x1b[0m",
                if agent.cfg.always_approve { "ON" } else { "OFF" }
            );
        }
        "/inspect" => agent.cfg.print_inspect(),
        other => println!("unknown command: {other} (try /help)"),
    }
    false
}

fn banner(cfg: &Config) {
    println!("\x1b[1;35m▌ Mimo Build\x1b[0m \x1b[2m(mimo-rs reimplementation · model {} · {})\x1b[0m", cfg.model, cfg.auth_source);
    if cfg.plan_mode {
        println!("\x1b[2m  plan mode ON — /approve to enable edits, /help for commands\x1b[0m");
    } else {
        println!("\x1b[2m  /help for commands\x1b[0m");
    }
}

fn help() {
    println!("Commands:");
    println!("  /help            show this help");
    println!("  /model [id]      show or switch model");
    println!("  /plan            toggle plan mode");
    println!("  /approve         approve the plan (enable mutating tools)");
    println!("  /yolo            toggle auto-approve of all tool calls");
    println!("  /clear, /new     start a fresh session");
    println!("  /inspect         show resolved configuration");
    println!("  /quit            exit");
}
