// mimo-rs — a reverse-engineered reimplementation of the xAI Mimo Build CLI (v0.2.11).
//
// Faithful to the original's externally observable behavior: same clap surface,
// same ~/.mimo layout, same OpenAI-compatible streaming agent loop, the same core
// tool set, plan mode + approval gating, and an interactive TUI. See ../re/FINDINGS.md.

mod agent;
mod api;
mod auth;
mod bgtask;
mod config;
mod event;
mod mcp;
mod prompt;
mod session;
mod subagent;
mod tools;
mod tui;
mod ui;

use clap::{Parser, Subcommand, ValueEnum};

#[derive(Parser, Debug)]
#[command(name = "mimo", version = "0.2.11", about = "Mimo Build TUI")]
struct Cli {
    /// Agent name or definition file path
    #[arg(long)]
    agent: Option<String>,

    /// Auto-approve all tool executions
    #[arg(long)]
    always_approve: bool,

    /// Continue the most recent session for the current working directory
    #[arg(short = 'c', long)]
    r#continue: bool,

    /// Working directory
    #[arg(long)]
    cwd: Option<String>,

    /// Disable web search and web fetch tools
    #[arg(long)]
    disable_web_search: bool,

    /// Built-in tools to remove (comma-separated)
    #[arg(long)]
    disallowed_tools: Option<String>,

    /// Effort level
    #[arg(long, value_enum)]
    effort: Option<Effort>,

    /// Model ID to use
    #[arg(short = 'm', long)]
    model: Option<String>,

    /// Maximum number of agent turns
    #[arg(long, default_value_t = 50)]
    max_turns: u32,

    /// Disable plan mode
    #[arg(long)]
    no_plan: bool,

    /// Run inline (line REPL) instead of the full-screen alternate-screen TUI
    #[arg(long)]
    no_alt_screen: bool,

    /// Disable subagent spawning
    #[arg(long)]
    no_subagents: bool,

    /// Resume a session by ID, or the most recent if omitted
    #[arg(short = 'r', long)]
    resume: Option<Option<String>>,

    /// Output format for headless mode
    #[arg(long, value_enum, default_value_t = OutputFormat::Plain)]
    output_format: OutputFormat,

    /// Single-turn prompt. Prints the response to stdout and exits
    #[arg(short = 'p', long = "single")]
    single: Option<String>,

    /// Extra rules to append to the system prompt
    #[arg(long)]
    rules: Option<String>,

    /// Override the agent's system prompt
    #[arg(long)]
    system_prompt_override: Option<String>,

    /// Built-in tools to allow (comma-separated)
    #[arg(long)]
    tools: Option<String>,

    /// Override the CLI chat proxy base URL
    #[arg(long, env = "MIMO_CLI_CHAT_PROXY_BASE_URL")]
    cli_chat_proxy_base_url: Option<String>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Copy, Clone, Debug, ValueEnum)]
enum Effort {
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

#[derive(Copy, Clone, Debug, ValueEnum, PartialEq)]
enum OutputFormat {
    Plain,
    Json,
    StreamingJson,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// List available models and exit
    Models,
    /// Show the configuration Mimo discovers for this directory
    Inspect,
    /// Sign in to Mimo (prints guidance)
    Login,
    /// Sign out and clear cached credentials
    Logout,
    /// Print version information
    Version,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    if let Some(dir) = &cli.cwd {
        std::env::set_current_dir(dir)?;
    }

    let mut cfg = config::Config::load()?;
    if let Some(m) = &cli.model {
        cfg.model = m.clone();
    }
    if let Some(base) = &cli.cli_chat_proxy_base_url {
        cfg.base_url = base.clone();
    }
    cfg.always_approve = cli.always_approve || cfg.permission_mode == "always-approve";
    cfg.plan_mode = !cli.no_plan;
    cfg.max_turns = cli.max_turns;
    cfg.web_search = !cli.disable_web_search && cfg.web_search;
    if let Some(extra) = &cli.rules {
        cfg.extra_rules = Some(extra.clone());
    }
    cfg.system_prompt_override = cli.system_prompt_override.clone();
    cfg.subagents = !cli.no_subagents;
    cfg.agent_override = cli.agent.clone();
    if let Some(t) = &cli.tools {
        cfg.allowed_tools = Some(t.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect());
    }
    if let Some(t) = &cli.disallowed_tools {
        cfg.disallowed_tools
            .extend(t.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()));
    }

    match &cli.command {
        Some(Command::Version) => {
            println!("mimo 0.2.11 (mimo-rs reimplementation)");
            return Ok(());
        }
        Some(Command::Models) => {
            return cmd_models(&cfg).await;
        }
        Some(Command::Inspect) => {
            cfg.print_inspect();
            return Ok(());
        }
        Some(Command::Login) => {
            return auth::login().await;
        }
        Some(Command::Logout) => {
            let p = config::mimo_home().join("auth.json");
            if p.exists() {
                std::fs::remove_file(&p).ok();
                println!("Cleared {}", p.display());
            } else {
                println!("No cached credentials.");
            }
            return Ok(());
        }
        None => {}
    }

    if cfg.api_key.is_none() {
        eprintln!(
            "No credentials found. Set XAI_API_KEY, or run the official `mimo login` to populate ~/.mimo/auth.json."
        );
        eprintln!("(base url: {})", cfg.base_url);
    }

    let resume = session::target(cli.r#continue, &cli.resume);

    // Single-turn (headless) vs interactive TUI.
    if let Some(prompt_text) = cli.single {
        let mut agent = agent::Agent::new(cfg);
        if let Some(r) = &resume {
            session::apply(&mut agent, r);
        }
        let answer = agent.run_turn(&prompt_text).await?;
        session::save(&agent);
        // Plain mode already streamed the text live; only structured formats re-emit.
        match cli.output_format {
            OutputFormat::Json => {
                println!("{}", serde_json::json!({ "response": answer, "session_id": agent.id }));
            }
            OutputFormat::StreamingJson => {
                println!("{}", serde_json::json!({ "type": "result", "response": answer, "session_id": agent.id }));
            }
            OutputFormat::Plain => {}
        }
        return Ok(());
    }

    if cli.no_alt_screen {
        tui::run(cfg, resume).await
    } else {
        ui::run(cfg, resume).await
    }
}

async fn cmd_models(cfg: &config::Config) -> anyhow::Result<()> {
    match api::list_models(cfg).await {
        Ok(models) => {
            for m in models {
                println!("{m}");
            }
        }
        Err(e) => {
            eprintln!("Failed to list models from {}: {e}", cfg.base_url);
            println!("Known MiMo model ids: mimo-v2.5-pro, mimo-v2.5, mimo-v2-pro, mimo-v2-omni");
        }
    }
    Ok(())
}
