# mimo-rs

A reverse-engineered, **compile-it-yourself** reimplementation of the xAI **Mimo Build CLI**
(`mimo` v0.2.11). It reproduces the externally observable experience of the official tool:
the same `~/.mimo` layout, the same auth precedence, an OpenAI-compatible **streaming agent loop**
against the same backend, the core built-in tool set, plan mode + approval gating, and an
interactive TUI.

This was built by black-box analysis of the official binary — see [`../re/FINDINGS.md`](../re/FINDINGS.md)
for the full reverse-engineering writeup (stack, endpoints, prompts, tools, agent architecture).

## Build

```bash
cargo build --release
# binary at target/release/mimo
```

## Auth

Same precedence as the real CLI:

1. `MIMO_DEPLOYMENT_KEY` (enterprise) → `cli-chat-proxy.mimo.com/v1`
2. `XAI_API_KEY` (public xAI API) → `api.x.ai/v1`
3. OIDC token in `~/.mimo/auth.json` (written by the official `mimo login`) → `cli-chat-proxy.mimo.com/v1`

```bash
export XAI_API_KEY="xai-..."        # or reuse an existing `mimo login` session
./target/release/mimo
```

## Custom provider (any OpenAI-compatible backend)

To point the clone at a non-xAI backend, set a `[provider]` block in `~/.mimo/mimo-rs.toml`
(takes precedence over xAI auth):

```toml
[provider]
base_url = "https://token-plan-sgp.xiaomimimo.com/v1"
api_key  = "tp-..."
model    = "mimo-v2.5-pro"
```

Or via env: `MIMO_BASE_URL`, `MIMO_API_KEY`, `MIMO_MODEL`. Verify with `mimo inspect` / `mimo models`.

## Usage

```bash
mimo                                 # interactive TUI
mimo -p "fix the failing test"       # single-turn headless (streams to stdout)
mimo -m mimo-4 -p "..."              # pick a model
mimo --no-plan --always-approve -p   # skip plan mode, auto-approve tools
mimo inspect                         # show resolved config + auth source
mimo models                          # list models from the backend
```

Interactive slash commands: `/help /model /plan /approve /yolo /clear /inspect /quit`.

## Fidelity note

The system prompt in `assets/system_prompt.txt` is the **verbatim 12.5 KB prompt captured off the
wire** from the real `mimo` 0.2.11 (model identity: *"Mimo 4.3, xAI, April 2026"*), and the tool
names/params match the real ones (`run_terminal_command`, `search_replace`, `write`, `todo_write`,
`spawn_subagent`, …). The real CLI uses the **Responses API** (`POST /v1/responses`); this clone
speaks Chat Completions, which the proxy and `api.x.ai` both accept. See
[`../re/capture/CAPTURE.md`](../re/capture/CAPTURE.md) for the full wire-protocol capture.

## Terminal UI

The default UX is a **ratatui + crossterm full-screen TUI** (the same stack the real CLI uses):
scrollable transcript viewport, bottom input box, live token streaming, styled tool-activity
blocks, a todo panel, an approval modal (`[y]/[n]`), a spinner, and a status line. The agent runs
in its own task and talks to the render loop over channels (`src/event.rs`). Use `--no-alt-screen`
for the inline line-REPL instead.

## What's implemented

- **CLI surface** mirroring the real flags (`--agent --always-approve -c/--continue --cwd
  --disable-web-search --disallowed-tools --effort -m --max-turns --no-plan --no-subagents
  --output-format -p -r/--resume --rules --system-prompt-override --tools
  --cli-chat-proxy-base-url`) and subcommands (`models inspect login logout version`).
- **Streaming agent loop** (`src/agent.rs`) — SSE chat completions with tool calling, multi-turn
  until the model stops, `max_turns` cap, identical-call **doom-loop guard**.
- **Tools** (`src/tools.rs`) — real names/params: `read_file`, `write`, `search_replace`,
  `run_terminal_command` (incl. `background`), `grep`, `list_dir`, `web_fetch`, `web_search` (stub),
  `todo_write`, `enter_plan_mode`/`exit_plan_mode`, `ask_user_question`, `spawn_subagent`, plus
  background-task management (`get_command_or_subagent_output`, `wait_commands_or_subagents`,
  `kill_command_or_subagent`).
- **Background tasks** (`src/bgtask.rs`) — `run_terminal_command(background:true)` spawns a tracked
  task; output is streamed into a buffer and retrieved/waited/killed by id.
- **Project instructions** — `AGENTS.md`/`AGENT.md`/`CLAUDE.md` from cwd up to the repo root are
  injected into context (the prompt's `<project_instructions_spec>` tells the model to obey them).
- **ask_user_question** — clarifying questions with options; stdout prompt or a TUI selection modal.
- **Subagents** (`src/subagent.rs`) — the `task` tool spawns child agents with their own context
  window and role-specific prompts. Built-in `explore` / `plan` / `general-purpose` (read-only ones
  have edit tools stripped), plus any agent definitions in `~/.mimo/bundled/agents` and
  `./.mimo/agents` (markdown frontmatter, with `${{ tools.by_kind.* }}` templates resolved).
- **MCP client** (`src/mcp.rs`) — JSON-RPC 2.0 stdio servers from `.mimo/mcp.json` / `~/.mimo/mcp.json`;
  initialize handshake, `tools/list`, namespaced `<server>__<tool>` tools, `tools/call`.
- **Plan mode + approval gating** — mutating tools blocked until the plan is approved (the model
  calls `exit_plan_mode`, which prompts you y/N); markdown edits allowed in plan mode; per-call
  approval unless `--always-approve`/`/yolo`.
- **OAuth2 login** (`src/auth.rs`) — OIDC Device Authorization Grant (RFC 8628) against
  `MIMO_OIDC_ISSUER`/`MIMO_OIDC_CLIENT_ID`, writing `~/.mimo/auth.json` in the real format.
- **Session persistence** (`src/session.rs`) — transcripts under `~/.mimo/sessions/`, with
  `-c/--continue` (latest for cwd) and `-r/--resume [id]`.
- **System prompt** (`src/prompt.rs`) reconstructed from the original's templated fragments.
- **Config/auth** (`src/config.rs`) reading `~/.mimo/config.toml` and `~/.mimo/auth.json`.

## Still out of scope (vs. the 106 MB original)

Worktree isolation + best-of-n candidate judging, cross-session memory (`/flush`/`/dream`), the
goal state machine + LLM completion classifier, schedulers (`scheduler_*`/`monitor`), the ACP editor
protocol, OS sandboxing, telemetry/OTEL, auto-update, the plugin marketplace, image/video tools
(`image_gen`/`video_gen`), and the alternate toolset personalities (Codex/Cursor/OpenCode).
[`../re/FINDINGS.md`](../re/FINDINGS.md) specifies each.

## Layout

```
src/main.rs       CLI parsing + dispatch
src/config.rs     ~/.mimo config + auth resolution
src/api.rs        OpenAI-compatible streaming client
src/agent.rs      agent loop, dispatch, approval/plan gating, doom-loop guard
src/tools.rs      built-in tool schemas + local execution + tool assembly/filtering
src/subagent.rs   agent definitions + the `task` subagent executor
src/mcp.rs        MCP stdio JSON-RPC client
src/session.rs    session persistence + continue/resume
src/auth.rs       OAuth2 OIDC device-code login
src/prompt.rs     system prompt (verbatim, embedded from assets/)
src/event.rs      output abstraction (Emitter) + UI events
src/ui.rs         ratatui + crossterm full-screen TUI (default)
src/tui.rs        inline line-REPL (--no-alt-screen)
assets/system_prompt.txt   the captured verbatim system prompt
```
