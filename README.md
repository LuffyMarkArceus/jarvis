# Jarvis - Personal AI Assistant

Local-first personal AI assistant for Linux. Hybrid architecture: Rust core + Python sidecars + TypeScript UI.

## Status

Phase 1 - local core: daemon, CLI and terminal UI.

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
