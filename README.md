# Jarvis - Personal AI Assistant

Local-first personal AI assistant for Linux. Hybrid architecture: Rust core +
Python sidecars + TypeScript UI.

## Status

Phase 1.5 - local core with deterministic tool planning and MCP tooling.

## Build

```sh
cargo build --workspace
```

## Usage

```sh
jarvis daemon start      # background daemon (Unix socket)
jarvis ask "hello"       # one turn; falls back to direct mode if the daemon is down
jarvis-tui               # full terminal UI
```

The TUI resumes the session you last used. Inside it, `F1` lists the keys and
`/help` lists the session and daemon commands.

## How a turn is decided

Two modules own the decision, and they cannot disagree:

* `core/src/router.rs` answers **what kind of request is this** - a
  deterministic tool call, a model turn, or plain conversation. It is exact,
  testable and free of model latency. It also recognises when a request needs
  an external source (`needs_web`, `needs_source`).
* `core/src/planner.rs` answers **which tools may serve it**. It returns one
  `TurnPlan` holding both the route and the exact tool set. The provider
  request and the system prompt are rendered from that same list, so the model
  is never told about a tool it cannot call. A model-requested call outside the
  plan is refused rather than executed.

Tool selection is deliberately narrow. For a `Route::Model` turn the planner
offers only the tools the detected intent needs (files, shell, processes,
system, web). A model turn with nothing to offer degrades to conversation
instead of offering `system.info` for every unmatched question.

MCP servers are offered when the request names the server or tool, when the
server sets `always_offer = true`, or when the request needs the web and the
server is a web server.

## MCP sidecars

MCP servers are started over stdio and their tools are namespaced as
`mcp.<server>.<tool>`. Every call goes through the permission system exactly
like a built-in tool.

```toml
[mcp]
[[mcp.servers]]
name = "fetch"
command = "python3"
args = ["sidecars/fetch/server.py"]
enabled = true
```

Arguments beginning with `sidecars/` (or `${SIDECAR_DIR}`) are resolved to the
sidecars shipped with the checkout, so no absolute path is required. See
`config/mcp.example.toml` for a full example.

The built-in `filesystem.*` tools cover listing, reading, searching, writing
and editing. The MCP filesystem server duplicates them and is therefore
disabled by default; enabling both doubles the tool surface for no new
capability.

## Tests

```sh
cargo test --workspace
python3 -m unittest discover -s sidecars/fetch -p 'test_*.py'
```

The Rust suite includes golden routing cases (`core/tests/routing_golden.rs`)
and MCP client integration tests against a real stdio subprocess
(`core/tests/mcp_stdio.rs`). The Python suite covers HTML extraction and the
JSON-RPC protocol without network access.
