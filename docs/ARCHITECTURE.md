# Architecture

mimo is a single Rust binary. The agent loop, tools, and UI are decoupled through a small
event abstraction so the same core drives both headless output and the full-screen TUI.

## Flow

```
main.rs  ──parse flags / load config──▶  Agent::run_turn
                                              │
                 ┌────────────────────────────┼───────────────────────────┐
                 ▼                            ▼                            ▼
            api::stream_chat            tools::assemble              event::Emitter
        (OpenAI Chat Completions,   (tool schemas for this        (Stdout | Channel)
         SSE streaming, tool calls)  config: builtins + MCP +      → stdout (headless)
                 │                    subagents + features)        → UiEvent channel (TUI)
                 ▼
        assistant text + tool_calls ──▶ dispatch each tool ──▶ tool result back into messages
                 ▲                                                        │
                 └──────────────────── loop until no tool calls ─────────┘
```

A turn streams a completion, executes any tool calls, feeds the results back as `tool`
messages, and repeats until the model stops calling tools or `--max-turns` is hit. A
doom-loop guard breaks identical repeated calls.

## Modules

| Module | Responsibility |
|---|---|
| `main.rs` | clap CLI, config assembly, subcommand/flag dispatch |
| `config.rs` | `~/.mimo` config + provider/auth resolution (env → `mimo-rs.toml` → OIDC) |
| `api.rs` | OpenAI-compatible streaming client; message/tool/assistant types |
| `agent.rs` | the turn loop, tool dispatch, plan-mode + approval gating, diff emission |
| `tools.rs` | built-in tool schemas + local execution + allow/deny filtering + sandbox hook |
| `event.rs` | `Emitter` (stdout vs channel) and `UiEvent`; the seam between agent and UI |
| `ui.rs` | ratatui full-screen TUI (header, transcript, diffs, palette, modals) |
| `tui.rs` | inline line-REPL fallback (`--no-alt-screen`) |
| `subagent.rs` | `spawn_subagent` + agent definitions (built-in and `.mimo/agents/*.md`) |
| `mcp.rs` | MCP stdio JSON-RPC client; servers from `.mimo/mcp.json` |
| `bgtask.rs` | background command registry (spawn / poll / wait / kill) |
| `memory.rs` | cross-session memory store, `/flush`, `/dream`, recall injection |
| `goal.rs` | goal state machine + LLM completion classifier |
| `scheduler.rs` | scheduler CRUD + bounded `monitor` |
| `bestofn.rs` | best-of-N via git worktrees + LLM judge |
| `image.rs` | image/video tools against an OpenAI-style images endpoint |
| `sandbox.rs` | macOS `sandbox-exec` SBPL profiles for shell commands |
| `acp.rs` | Agent Client Protocol server over stdio (editor integration) |
| `personalities.rs` | alternate system-prompt personalities |
| `session.rs` | session persistence + `--continue`/`--resume` |
| `auth.rs` | OAuth2 OIDC device-code login |

## Design notes

- **Event seam.** The agent never prints directly; it calls `Emitter` methods. `Emitter::Stdout`
  writes ANSI text for headless/REPL use; `Emitter::Channel` forwards `UiEvent`s to the ratatui
  render loop. This is why the identical loop powers `-p` one-shots and the TUI.
- **TUI concurrency.** The agent runs in its own task and owns the `Agent` across turns; the render
  loop owns the terminal. They communicate over channels, with a `oneshot` round-trip for approval
  and question modals.
- **Stateless feature modules.** Memory, goals, and schedulers persist to files under `~/.mimo`, so
  their tool handlers are simple functions rather than agent-held state.
- **Provider-agnostic.** Everything speaks OpenAI Chat Completions, so any compatible endpoint works
  by editing `~/.mimo/mimo-rs.toml`.

## Provenance

This is an independent, clean-room-style reimplementation built by observing the externally visible
behavior of a terminal coding agent (CLI surface, streaming protocol, tool semantics, UI layout). It
contains original code and an original system prompt; no proprietary source, prompts, or assets are
included.
