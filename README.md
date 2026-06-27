# mimo

**mimo** is an open-source, terminal-native AI coding agent written in Rust. It talks to any
OpenAI-compatible chat backend, drives a full agentic loop (multi-step tool calling with live
streaming), and ships a polished full-screen TUI — plan mode, subagents, MCP, cross-session memory,
goals, schedulers, an OS sandbox, best-of-N with git worktrees, and an ACP server for editor
integration.

It is an independent reimplementation written from a black-box study of how a terminal coding agent
behaves; it contains no proprietary code or prompts.

## Install

```bash
curl -fsSL https://raw.githubusercontent.com/Lulzx/mimo-build/main/install.sh | bash
```

This builds from source (needs a Rust toolchain) and installs `mimo` onto your `PATH`.
Or build manually:

```bash
git clone https://github.com/Lulzx/mimo-build && cd mimo-build
cargo build --release          # binary at target/release/mimo
```

## Configure a backend

mimo speaks the OpenAI Chat Completions protocol. Point it at any provider via
`~/.mimo/mimo-rs.toml`:

```toml
[provider]
base_url = "https://api.openai.com/v1"   # or api.x.ai/v1, a local server, etc.
api_key  = "sk-..."
model    = "gpt-4o"
```

…or with env vars: `MIMO_BASE_URL`, `MIMO_API_KEY`, `MIMO_MODEL`. Check what got resolved with
`mimo inspect`, and list models with `mimo models`.

## Usage

```bash
mimo                                  # interactive full-screen TUI
mimo -p "fix the failing test"        # one-shot headless (streams to stdout)
mimo -m gpt-4o -p "..."               # pick a model
mimo --best-of-n 3 -p "..."           # 3 candidates in git worktrees, judged, best applied
mimo --persona codex                  # alternate prompt personality
mimo --sandbox read-only -p "..."     # confine shell commands (macOS)
mimo acp                              # Agent Client Protocol server over stdio (editors)
```

Slash commands in the TUI (type `/` for the command palette):
`/model /plan /approve /always-approve /theme /context /status /compact /copy /fork /sessions`
`/memory /dream /goal /mcp /inspect /new /home /help /quit`.

Switch themes live with `/theme <name>` — `groknight` (default), `grokday`, `tokyonight`,
`rosepine-moon`, `nord`, `oscura-midnight`. The choice is saved to `~/.mimo/config.toml`
(`[ui] theme`) and restored on the next launch.

## What's inside

- **Streaming agent loop** with multi-turn tool calling, a doom-loop guard, and `--max-turns`.
- **Tools**: `read_file`, `write`, `search_replace`, `run_terminal_command` (incl. background),
  `grep`, `list_dir`, `web_fetch`, `todo_write`, `enter/exit_plan_mode`, `ask_user_question`,
  `spawn_subagent`, background-task management, `update_goal`, schedulers, image/video.
- **Plan mode + approval gating**; per-tool approval unless `--always-approve`.
- **Subagents** with their own context windows (`explore`/`plan`/`general-purpose`, or your own
  `.mimo/agents/*.md`).
- **MCP** stdio client (`.mimo/mcp.json`), exposing server tools as `server__tool`.
- **Cross-session memory** (`/flush`, `/dream`), a **goal** state machine with an LLM completion
  classifier, **schedulers**, and an **OS sandbox** (`sandbox-exec` on macOS).
- **Best-of-N** with git-worktree isolation and an LLM judge.
- **ACP server** for editor integration.
- A **ratatui** TUI: rounded input box, `◆` activity bullets, inline diffs, todo panel, approval
  modals, slash-command palette, and a runtime **theme switcher** (`/theme`) with six built-in
  palettes (*groknight* default, *grokday*, *tokyonight*, *rosepine-moon*, *nord*, *oscura-midnight*).
- **Animations** (30fps while active): an 80ms braille spinner, a **shimmer** sweeping across the
  status label, a **breathing** pulse on the spinner and the active `❙` tool marker, a top-to-bottom
  **wave reveal** as diff hunks land, and an animated window title.

## Layout

```
src/main.rs       CLI parsing + dispatch          src/ui.rs        full-screen ratatui TUI
src/agent.rs      agent loop + tool dispatch       src/tui.rs       inline line REPL (--no-alt-screen)
src/api.rs        OpenAI-compatible client         src/event.rs     output/event abstraction
src/tools.rs      tool schemas + execution         src/subagent.rs  subagents + agent defs
src/config.rs     ~/.mimo config + auth            src/mcp.rs       MCP stdio client
src/session.rs    session persistence              src/memory.rs    cross-session memory
src/goal.rs       goal state machine               src/scheduler.rs schedulers + monitor
src/bestofn.rs    best-of-N worktrees              src/image.rs     image/video tools
src/sandbox.rs    OS sandbox profiles              src/acp.rs       ACP stdio server
src/personalities.rs  prompt personalities         src/auth.rs      OAuth2 device login
```

## License

MIT. Independent project; not affiliated with or endorsed by any AI provider.
