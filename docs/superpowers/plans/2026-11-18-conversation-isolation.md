# One Conversation per Window (Milestone 6.5a) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Each window (and each one-off CLI call) is its own conversation - its own history, model, tier, trust and `/bad` - and nothing a window does changes another.

**Architecture:** A `Conversation` holds one conversation's state; the orchestrator holds only shared services and every turn/setting method takes a `&Conversation`. A `Conversations` registry in the daemon finds, creates and persists them in `memory.db`. Every conversation request on the wire names its `session`.

**Tech Stack:** Rust, tokio, rusqlite (additive migrations), ratatui TUI, Unix-socket IPC.

**Spec:** `docs/superpowers/specs/2026-11-18-conversation-isolation-design.md`

## Global Constraints

* No default conversation anywhere: the orchestrator holds none; a conversation request without `session` is an error.
* Every caller passes a conversation - tests, each eval attempt and each direct-mode `jarvis ask` make their own.
* Conversation requests (require `session`): `ask`, `model.list`, `model.set`, `tier.set`, `feedback`, `trust.list`, `trust.clear`, `session.show`.
* Daemon requests (no `session`): `session.new`, `session.last`, `session.list`, `session.delete`, `session.prune`, `status`, `tools.list`.
* Removed: `session.current`, `session.switch`.
* Migrations are additive and run twice without harm: `sessions.model TEXT`, `sessions.remote INTEGER`, `messages.model TEXT`, `settings(key TEXT PRIMARY KEY, value TEXT)`.
* New conversations start with the most recently used model (setting `last_model`, with its tier).
* Tests never touch the owner's daemon, data or GPU: throw-away data folders, scripted providers.

## Review Focus

1. A daemon restart while a window is open: the window's next request names a conversation the new daemon has not loaded - it must load it from `memory.db`, not refuse it (Task 3).
2. A conversation deleted in one window while another shows it: the other's next request gets an error naming it, and no conversation is silently recreated (Task 3).
3. Messages stored before `messages.model` existed: shown with no model, not an error (Task 2).
4. `jarvis model` on a fresh install, with no conversation used yet: a clear message, not a panic (Task 5).
5. Two windows deliberately on one conversation (`/switch`): a model switch in one is what the other's next turn uses (Task 3).

---

### Task 1: `Conversation`, and every caller passes one

Behaviour unchanged: the daemon still has one conversation (it becomes the field `current: Arc<Conversation>`), so the whole suite passes with only call-site edits.

**Files:**
- Create: `core/src/conversation.rs`
- Modify: `core/src/lib.rs`, `core/src/orchestrator/{mod,outcome,prompt,execute,turn,turnlog,gateway}.rs`, `core/src/daemon.rs`, `core/src/eval/mod.rs`, `cli/src/main.rs`, every test calling `run_turn`

**Interfaces (produces):**
- `pub struct Conversation { pub id: String, remote: AtomicBool, chosen: Mutex<[Option<String>; 2]>, pub(crate) last_turn: Mutex<Option<LastTurn>> }` with `Conversation::new(id: String, remote: bool) -> Arc<Conversation>`, `fn remote(&self) -> bool`, `fn chosen(&self) -> [Option<String>; 2]`.
- `LastTurn` moves from `turnlog.rs` to `conversation.rs` (`pub(crate)`).
- `Orchestrator::new_conversation(&self) -> Arc<Conversation>` - fresh uuid, its `sessions` row, remote per `[remote] default`.
- `run_turn(&self, conv: &Conversation, input, confirm, cancel, sink)`, `run_turn_observed(&self, conv, ...)`, `run_with_footer(&self, conv, ...)`, `set_remote(&self, conv, on)`, `uses_remote(&self, conv)`, `set_model(&self, conv, query)`, `models(&self, conv)`, `model_info(&self, conv)`, `destination(&self, conv)`, `warm_up(&self, conv)`, `feedback(&self, conv, kind, note)`, `trusted(&self, conv)`, `clear_trust(&self, conv)`.
- `TurnRecorder::new(conversation: Arc<Conversation>)`; internal code reads `rec.conversation.id` where it read `self.session_id`.
- Removed from `Orchestrator`: `session_id`, `use_remote`, `chosen`, `last_turn`, `session_id()`, `set_session()`.

- [ ] Step 1: unit test in `conversation.rs` - `a_new_conversation_starts_on_the_tier_it_was_given`: `Conversation::new("a".into(), true).remote() == true`, `chosen() == [None, None]`.
- [ ] Step 2: run it - fails (no module).
- [ ] Step 3: implement `conversation.rs`, then move the four fields and thread `&Conversation` through; the compiler lists every call site. Tests get a helper `fn conv(j: &Orchestrator) -> Arc<Conversation>` in `core/tests/common/mod.rs` returning `j.new_conversation()`.
- [ ] Step 4: `cargo test --workspace` - same count as before plus the new test, 0 failures; `cargo clippy` 0.
- [ ] Step 5: commit "conversation: a turn belongs to a Conversation the caller passes".

### Task 2: persistence - model, tier, each answer's model, the last-used model

**Files:**
- Modify: `core/src/memory.rs` (migrations; `Message.model: Option<String>`; `add_answer(session, content, label, model)`; `session_settings(id) -> Result<Option<(Option<String>, bool)>>`; `save_session_settings(id, model: Option<&str>, remote: bool)`; `setting(key) -> Result<Option<String>>`; `set_setting(key, value)`), `core/src/conversation.rs` (`Conversations`), `core/src/orchestrator/prompt.rs` (`persist_assistant` records the answering model)
- Test: `core/tests/conversations.rs`

**Interfaces (produces):**
- `pub struct Conversations` with `Conversations::new(memory: MemoryManager, config: &Config) -> Self`, `get(&self, id: &str) -> Result<Arc<Conversation>>` (cache, else load from `memory.db`, else `Err` naming the id), `create(&self, orch: &Orchestrator) -> Arc<Conversation>` (starts with `last_model`), `saved(&self, conv: &Conversation)` (persist model/tier and set `last_model`), `last(&self) -> Option<String>` (last-used id).
- `Conversation::restore(id, remote, chosen)` for loading.

- [ ] Step 1: failing tests in `core/tests/conversations.rs`:
  - `migrations_run_twice_and_old_messages_have_no_model` - an old-schema db opened twice; an old message reads back with `model == None`.
  - `a_conversations_model_and_tier_survive_a_restart` - set qwen and remote on one, save, new `Conversations` over the same db, `get(id)` has them.
  - `a_new_conversation_starts_with_the_model_used_last` - save one on `cohere/north-mini-code:free` (remote), `create` gives that model and tier.
  - `model_hopping_records_each_answers_model` - scripted local+remote providers; turns on llama, switch to qwen, back to llama, then a remote model; `get_recent_messages` assistant rows carry `llama3.2:1b`, `qwen2.5:3b`, `llama3.2:1b`, the remote name, in order; the history sent on the 4th turn holds all earlier answers.
  - `switching_to_remote_still_asks_before_sending_local_files` - read a file outside the allow-list on local, switch to remote, ask: the confirmer is asked `egress.send`.
  - `an_unknown_conversation_is_an_error` - `get("nope")` errs, message contains "nope".
- [ ] Step 2: run them - fail.
- [ ] Step 3: implement migrations (the `label` column's pattern), the memory functions, `Conversations`.
- [ ] Step 4: `cargo test --workspace` passes.
- [ ] Step 5: commit "conversations: each keeps its model and tier; each answer its model; new ones start with the last used".

### Task 3: the daemon and the protocol name the conversation

**Files:**
- Modify: `core/src/daemon.rs` (holds `Conversations`, no current one; `Daemon::with_orchestrator(config: Config, orch: Orchestrator) -> Result<Daemon>` for tests; conversation methods take `session: &str`), `core/src/ipc/server.rs` (required `session` per the constraints; `session.new` returns `{session_id, model, tier}`; `session.show` messages carry `model`; `session.last`; remove `session.current`, `session.switch`; `last_session` saved on every `ask`), `common/src/ipc.rs` if helpers help
- Test: `core/tests/two_windows.rs` - a throw-away daemon (`with_orchestrator`, scripted provider) and `IpcServer` on a temp socket, two real `UnixStream` clients

- [ ] Step 1: failing tests:
  - `a_window_asks_in_its_own_conversation` - A and B `session.new`; B asks; B's question is in B's session, A's has none.
  - `a_model_switch_stays_in_its_conversation` - A `model.set qwen`; B `model.list` current is still llama.
  - `trust_and_feedback_stay_in_their_conversation`.
  - `a_conversation_request_without_a_session_is_refused` - `ask` without `session` -> error containing "session".
  - `a_conversation_survives_a_daemon_restart` (Review Focus 1) - new daemon over the same data; `ask` with the old id works and sees its history.
  - `a_deleted_conversation_is_not_recreated` (Review Focus 2).
  - `two_windows_on_one_conversation_share_its_model` (Review Focus 5).
- [ ] Step 2: run - fail.
- [ ] Step 3: implement.
- [ ] Step 4: `cargo test --workspace` passes.
- [ ] Step 5: commit "daemon: every conversation request names its session".

### Task 4: the TUI - a window owns its conversation

**Files:**
- Modify: `tui/src/app.rs`, `tui/src/main.rs` (`--continue`), `tui/src/ui.rs` (header model from the conversation)

- [ ] Step 1: failing tests in `app.rs`:
  - `a_new_window_starts_a_new_conversation` - `on_connected` with no session -> `session.new`; with `continue` -> `session.last`.
  - `a_reconnecting_window_keeps_its_conversation` - `on_connected` with `session_id = Some("abc")` -> `session.show` for "abc".
  - `every_conversation_request_carries_the_windows_session` - `ask`, `model.set`, `tier.set`, `feedback`, `trust.list` params contain `"session": "abc"`.
  - `switch_is_the_windows_choice` - `/switch xyz` sends `session.show` for xyz and sets `session_id`; no `session.switch`.
  - `the_header_shows_the_conversations_model` - a `session.new` reply with `model` sets `app.model`.
- [ ] Step 2: run - fail. Step 3: implement. Step 4: `cargo test -p jarvis-tui` passes. Step 5: commit "tui: each window is its own conversation".

### Task 5: the CLI, eval harness and `jarvis up --continue`

**Files:**
- Modify: `cli/src/main.rs` (`ask`: `session.new` then `ask`; direct mode: `new_conversation()`; `model`/`tier`: `session.last`, saying which conversation), `cli/src/up.rs` (`--continue` passed to `jarvis-tui`), `core/src/eval/mod.rs` (one conversation per attempt; multi-turn cases reuse it)

- [ ] Step 1: failing tests: `jarvis_model_with_no_conversation_says_so` (Review Focus 4, the message formatter); eval offline suite unchanged (60/60); `up` passes `--continue` (argument builder test).
- [ ] Step 2-4: run, implement, pass (`cargo test --workspace`, offline eval 60/60).
- [ ] Step 5: commit "cli: one-off asks get their own conversation; up --continue".

### Task 6: docs and the live check

**Files:** `README.md` (windows and conversations, `--continue`, model hopping), `docs/ROADMAP.md` (6.5a done), `docs/real-use.md`.

- [ ] Step 1: live: two `jarvis-tui` windows against a throw-away daemon (temp `XDG_*`), `/new` in one, ask in the other: stored where its header says.
- [ ] Step 2: full suite, clippy, fmt, offline eval; commit; PR; merge only after checks pass.
