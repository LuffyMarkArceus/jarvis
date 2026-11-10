# Status

Where Jarvis is, what is open, and where to read more. Updated at the end of
every phase, as part of finishing it. The plan ahead is in
[ROADMAP.md](ROADMAP.md).

Last updated: 2026-11-09, after Milestone 6.2 (main `f91104c`).

## At a glance

| | |
| --- | --- |
| Code | Rust workspace (`core`, `cli`, `tui`, `common`) + Python MCP sidecars; ~25k lines of Rust |
| Tests | 469 (`cargo test --workspace`), CI on every push |
| Evals | 53 cases (`jarvis eval`); qwen2.5:3b 90.6%, llama3.2:1b 82.4% ([BASELINES](../evals/BASELINES.md)) |
| Models | local: Ollama (llama3.2:1b for development, qwen2.5:3b for verification); remote: OpenRouter free tier (nemotron-3-ultra) |
| Lessons | 29 entries in the [learning log](learning/README.md) |

## Phases

Phases 1-4 are reconstructed from the git history; their plan was made in
earlier sessions and is not in the repository.

| Phase | What it gave Jarvis | Done | Read |
| --- | --- | --- | --- |
| 0 | Scaffolding: Rust daemon, CLI, TUI skeleton, Ollama | 2026-10-21 | - |
| 1 | Memory (SQLite), daemon + Unix-socket IPC, sessions | 2026-10-21 | README "Memory" |
| 2 | Permissions and audit log, filesystem/shell/system tools, agent loop, deterministic router | 2026-10-22 | README "How a turn is decided" |
| 3 | Terminal UI, streaming and cancel, cross-session recall (FTS5), MCP client | 2026-10-24 | README |
| 4 | Web search and fetch sidecars, answer grounding, recall in the prompt, provider boundary | 2026-10-26 | README "Web questions" |
| 5a | Eval harness: `jarvis eval`, taped web, `eval diff`, baselines | 2026-10-27 | [evals/README](../evals/README.md) |
| 5b | Local and remote model tiers, privacy rules (labels, redaction, allow-lists), footer | 2026-10-28 | [model-tiers](design/model-tiers.md) |
| 5b+ | Follow-ups: round limit per tier, repeat guard, closing answer, web-call egress check, frozen web pages | 2026-10-28 | learning #20-22 |
| 6 | Prompt-injection gate, exfiltration rule, undo, marking, injection evals | 2026-11-02 | [injection-and-undo](design/injection-and-undo.md) |
| 6.1 | Relay note, router fixes, a request never runs twice, orchestrator split | 2026-11-05 | learning #27-28 |
| 6.2 | Earlier-turn marking, `filesystem.delete` with undo, large-file limits, eval sandbox (`work_dir`) and honesty fixes | 2026-11-09 | [ROADMAP](ROADMAP.md) |
| 6.3 | OS sandbox for commands (bubblewrap), keys out of command environments, an always-asked way out | 2026-11-09 | [sandbox](design/sandbox.md) |

## Open items

The order they will be done in is in the roadmap.

**Phase 6 follow-ups**

* Relayed instructions: tool-turn answers get a note. A plain chat turn
  that relays from history does not yet (its answer streams). A link a page
  mentions gets the note too (`ctrl.research`, qwen 3/3): watch for noise in use.
* Batch operations with a dry-run preview: later, with the operation itself.
* Deletes through `shell.run` (`rm`) cannot be undone; `filesystem.delete` can.
  In the sandbox, `rm` reaches only the workspace.

**Sandbox (6.3)**

* Reading is limited only by the hidden list; a command outside the sandbox has
  full rights (it always asks). Typed file tools and MCP sidecars are not
  sandboxed. Linux only. See [sandbox](design/sandbox.md), known gaps.

**Carried over**

* Names and free-text personal details are not redacted.
* A redacted remote answer is not streamed.
* Marking does not cover memory recalled into the system prompt.

**Not bugs, to remember**

* `memory.correction` and `memory.followup_file` fail on qwen and llama and pass
  on nemotron: the prompt carries the history, so they measure the small models.

**For the owner**

* Cap the OpenRouter key's spend at about $1.
* `/home/shrey/~/.config/ums-deploy.env` (2026-10-06) is not Jarvis's; check it.

## Where things are

| To learn about | Read |
| --- | --- |
| Using Jarvis | [README](../README.md) |
| Why a design is the way it is | [docs/design/](design/) |
| What each phase taught, in interview form | [docs/learning/README.md](learning/README.md) |
| Model scores over time | [evals/BASELINES.md](../evals/BASELINES.md) |
| Writing and running evals | [evals/README.md](../evals/README.md) |
| How Phase 6 was built, task by task | [docs/superpowers/plans/](superpowers/plans/) |
