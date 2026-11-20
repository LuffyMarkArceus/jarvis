# Turns in Parallel and the GPU Queue (Milestone 6.5b) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Windows' turns run at the same time. Local model calls queue
visibly for `slots` places, and a call that waits too long spills to the
remote model with a notice.

**Architecture:**
* Every request is already spawned as its own task. The two turn-long
  locks go: `Arc<Daemon>` and `Arc<Orchestrator>`, with no `Mutex` around
  either.
* A conversation answers one turn at a time, through a busy marker.
* A `LocalLane`, a FIFO `tokio::sync::Semaphore` wrapper, admits each local
  model call.
* A spill is decided in `ask_model`. It uses a remote version of the system
  prompt that was prepared when the turn's prompt was assembled.

**Tech Stack:** Rust, tokio (`Semaphore`, `time::pause`), rusqlite (additive
migrations), Unix-socket IPC tests.

**Spec:** `docs/superpowers/specs/2026-11-19-parallel-turns-design.md`.
Read its "Settled after the builder check" section: those five calls are
requirements.

## Global Constraints

* **Defaults:** `[model] slots = 1`, `[remote] spill = "auto"`,
  `[remote] spill_after = 5` (seconds). `slots = 0` is rejected at config
  load with a message naming `slots`.
* **Copy, verbatim:**
  * Refused second ask: "This conversation is answering another question.
    Wait for it, or open another with /new."
  * Refused delete: "Conversation {id} is answering a question; delete it
    after."
  * Waiting progress: "waiting for the GPU: another window is using
    {model}".
  * Spill progress: "GPU busy: answering with {short model} (remote)".
  * The footer of a spilled answer ends with " (GPU busy)".
* **Signals:** `"queued"` (waited longer than `spill_after`), `"spilled"`
  and `"spill_withheld"`.
* **Spill confirmation:** `spill = "ask"` uses the tool name `egress.spill`.
  A no, or no confirmer, means wait.
* **Spills and the conversation's tier.** A spill never changes the
  conversation's tier. Every spilled request goes through the egress check
  and redaction, as for a remote conversation.
* **The lane follows the destination.** It admits calls whose
  `destination_of(cfg)` is local. Spilling is possible only when the remote
  tier's destination is remote.
* **No test touches the owner's daemon, data, GPU or `~`.** Use throw-away
  temp dirs, fake providers, and paused tokio time where timing matters.
* **Before every commit:** `cargo fmt`, and `cargo clippy --workspace
  --all-targets -- -D warnings` must be clean.

## Review Focus

1. **Lost permits.** A turn that errors or is cancelled while holding a
   slot, or while waiting for one, must release the slot. Afterwards
   `available()` is `slots` again (Task 1 and Task 3).
2. **The busy marker on every exit.** It is released when a turn ends by
   error, cancellation or a dropped connection. The window's next `ask`
   works (Task 2).
3. **The window's own cancel.** A user pressing Esc on a turn waiting for
   the GPU gets a cancelled turn at once, not after the other turn's model
   call (Task 3).
4. **Remote is down when a spill happens.** The spilled call's error
   surfaces as for a remote conversation, and does not hang. There is no
   silent retry on local (Task 4).
5. **Old rows after the per-tier migration.** A database where
   `sessions.model` holds a remote model's name (6.5a wrote the active
   tier's model) still loads. The name goes to the remote slot when
   `remote = 1` (Task 5).

---

### Task 1: `LocalLane` and its configuration

**Files:**
- Create: `core/src/lane.rs` (and register it in `core/src/lib.rs`)
- Modify: `core/src/config.rs`:
  * `ModelConfig.slots: usize`, with serde default 1;
  * `RemoteConfig.spill: Spill` and `spill_after: u64`, defaults `Auto` and
    5;
  * validation that rejects 0 slots.

**Interfaces (produces):**
- `pub enum Spill { Auto, Ask, Never }`, serde lowercase.
- `pub struct LocalLane`:
  * `LocalLane::new(slots: usize) -> Arc<LocalLane>`
  * `fn available(&self) -> usize`
  * `fn running(&self) -> Option<String>`: the model of some current
    holder.
  * `async fn acquire(self: &Arc<Self>, model: &str, deadline:
    Option<Duration>, cancel: Option<&Cancellation>) -> Admit`
- `pub enum Admit`:
  * `Slot(LanePermit)`
  * `Busy(Waiter)`
  * `Cancelled`
- `pub struct Waiter`. It keeps its place in line:
  * `async fn wait(self, cancel: Option<&Cancellation>) -> Option<LanePermit>`
    returns `None` if cancelled.
  * `fn waited(&self) -> Duration`
  * `fn running(&self) -> Option<String>`
- `pub struct LanePermit`. Dropping it releases the slot and clears its
  model from `running`.
- `Admit::Busy` is returned only when `deadline` elapses.
- An immediate grant is `Slot`. A call that had to wait reports the wait
  through `LanePermit::waited() -> Duration`.

How it is built: `Arc<Semaphore>`. A `Waiter` owns the pinned
`acquire_owned` future, which keeps its queue position, so turning
`Busy` into `wait()` does not re-queue. `running` is a
`std::sync::Mutex<Vec<(u64, String)>>` keyed by permit id.

- [ ] **Step 1: Write failing unit tests in `lane.rs`**
  (`#[tokio::test(start_paused = true)]`):
  - `a_free_slot_is_granted_at_once`: `slots = 1`. `acquire("qwen", None,
    None)` is `Slot`, and `available() == 0`. Dropping it gives
    `available() == 1`.
  - `waiters_are_served_first_come_first_served`: hold the slot, then
    spawn waiters B, C, D in that order, each pushing its name to a shared
    `Vec` when granted. Release one at a time. The order is
    `["B", "C", "D"]`.
  - `a_deadline_returns_busy_and_keeps_the_place`: hold the slot. B's
    `acquire(.., Some(5s), ..)` is `Busy` after 5 s, with `waited() >= 5s`
    and `running() == Some("qwen")`. C then queues with no deadline.
    Releasing the holder grants B's `wait()` before C.
  - `cancelling_a_waiter_frees_nothing_and_loses_nothing`: hold the slot,
    and B waits with a `Cancellation`. Cancelling it gives
    `Admit::Cancelled`. Drop the holder: `available() == 1`.
  - `two_slots_run_two_calls`: `slots = 2`. Two `Slot`s, and the third is
    `Busy` at its deadline.
  - in `config.rs` tests, `zero_slots_is_rejected`: the error contains
    `"slots"`. Also `spill_defaults`: `Spill::Auto`, 5 and 1.
- [ ] **Step 2:** Run `cargo test -p jarvis-core lane`. Expected: FAIL (no
  module).
- [ ] **Step 3:** Implement `lane.rs` and the config fields.
- [ ] **Step 4:** Run `cargo test -p jarvis-core lane config`. Expected:
  PASS.
- [ ] **Step 5:** Commit: "lane: a FIFO queue for local model calls, sized
  by [model] slots".

### Task 2: No turn-long locks; one turn per conversation

**Files:**
- Modify:
  * `core/src/ipc/server.rs`: `Arc<Daemon>` everywhere; drop
    `daemon.lock()`.
  * `core/src/daemon.rs`: `orchestrator: Arc<Orchestrator>`; drop
    `.lock().await`.
  * `cli/src/main.rs`, where the daemon is constructed.
  * `core/src/conversation.rs`: the busy marker.
  * `core/src/orchestrator/turnlog.rs`: the log mutex.
  * `core/src/orchestrator/gateway.rs`: a mutex around writing the allow
    file.
  * `core/src/permissions.rs`: a mutex around `set_rule`'s
    read-modify-write.
- Test: `core/tests/two_windows.rs`. `start()` builds
  `IpcServer::new(Arc::new(daemon), ..)`.

**Interfaces (produces):**
- `IpcServer::new(daemon: Arc<Daemon>, socket: PathBuf)`.
- `Conversation::begin_turn(self: &Arc<Self>) -> Option<Answering>`.
  `Answering` is a guard that clears the marker on drop. The result is
  `None` if the conversation is already answering.
  `Conversation::answering(&self) -> bool`.
- `Daemon::ask_with_footer` takes the guard first and refuses with the
  Global Constraints copy. `delete_session` refuses an answering
  conversation. `prune_empty_sessions` already skips loaded ones.
- `Orchestrator` is `Send + Sync`. Add the compile-time check
  `const _: fn() = || { fn f<T: Send + Sync>() {} f::<Orchestrator>(); };`.

- [ ] **Step 1: Write failing tests** in `two_windows.rs`:
  - `a_window_at_a_prompt_does_not_block_another`:
    * A asks to write a file (the scripted provider calls
      `filesystem.write`). Its client reads until it gets
      `ConfirmationRequest`, and does not reply.
    * Meanwhile B's `model.list`, `status` and `ask` each answer within 2
      s (wrap each in `tokio::time::timeout`).
    * A then replies `no`, and A's `ask` response arrives.

    The `Window` helper gets `send(method, params) -> Uuid` and
    `next_message() -> Message`, so a test can stop at the prompt.
  - `a_second_question_in_an_answering_conversation_is_refused`:
    * A asks a write and stops at the prompt.
    * Window B with `session = A's` asks. The error is exactly the refusal
      copy.
    * A answers the prompt. B's `ask` now works.
  - `an_answering_conversation_cannot_be_deleted`: while A is at the
    prompt, `session.delete` of A's id errors with the delete copy.
  - `the_marker_is_released_when_a_turn_fails`: in `conversations.rs`, a
    provider that returns `Err`. The next `begin_turn()` is `Some`.
  - `concurrent_turns_keep_every_log_line_whole`: in `turn_log.rs`, 8
    turns on 8 conversations of one `Arc<Orchestrator>`, joined. Every
    line of `turns.jsonl` parses, and there are 8 of them.
- [ ] **Step 2:** Run `cargo test -p jarvis-core --test two_windows --test
  conversations --test turn_log`. Expected: FAIL. The prompt test times out
  on B, because today the lock holds it.
- [ ] **Step 3:** Implement. Where the compiler finds a part that is not
  `Sync`, guard that part.
- [ ] **Step 4:** Run `cargo test --workspace`. Expected: all pass.
- [ ] **Step 5:** Commit: "daemon: turns run in parallel; a conversation
  answers one at a time".

### Task 3: Turns pin their settings; local calls go through the lane

**Files:**
- Modify:
  * `core/src/orchestrator/outcome.rs`: `TurnRecorder` gains
    `pinned: (bool, [Option<String>; 2])`, taken from the conversation in
    `new`. It also gains `spilled: AtomicBool` and `queue_ms` in
    `TurnTiming`.
  * `core/src/orchestrator/mod.rs`: the `lane: Arc<LocalLane>` field, built
    from `config.model.slots`. Also `turn_tier(&self, rec) -> (ModelConfig,
    Arc<dyn Provider>)`, which uses `rec.pinned` and `rec.spilled`. Turn
    code (gateway, `turn.rs` max_rounds, `prompt.rs` remote recall) calls
    `turn_tier(rec)`. Non-turn code (`model_info`, `models`, `warm_up`)
    keeps `tier(conv)`.
  * `core/src/orchestrator/gateway.rs`: `send` takes a slot when the
    destination is local.
  * `core/src/orchestrator/turnlog.rs`: the `"queued"` signal.
  * `TurnTiming::jarvis_ms` excludes `queue_ms`.
- Test: `core/tests/gpu_queue.rs`. Add a `Gated` provider to
  `core/tests/common/mod.rs`. It replies with a script, and each call
  awaits a release from a `tokio::sync::mpsc` the test holds. It records
  `(model, order)`.

**Interfaces (consumes):** `LocalLane`, `Admit`, `Waiter`, `LanePermit`
from Task 1, and `Arc<Orchestrator>` from Task 2.
**Interfaces (produces):**
- `Orchestrator::lane(&self) -> &Arc<LocalLane>`.
- `TurnTiming.queue_ms: u64`, serialized.
- `TurnRecorder::turn_tier` semantics: pinned at the start; remote once
  `spilled`.

In `send`, for a local destination: `lane.acquire(model, deadline, cancel)`.
The deadline is `spill_after` if a spill is possible, otherwise `None`.
- On `Busy` with no spill possible: report the waiting progress line, then
  `wait()`.
- Report the waiting line once, as soon as the call has to wait. Check
  `available() == 0` before acquiring.
- Add the permit's waited time to `queue_ms`.
- Hold the permit across the provider call; drop it after.
- `warm_up` returns early when `lane.available() == 0`.

- [ ] **Step 1: Write failing tests** in `gpu_queue.rs`, with two
  conversations on one `Arc<Orchestrator>` and the `Gated` local provider:
  - `a_second_local_call_waits_and_says_so`:
    * A's turn holds the gate.
    * B's `run_turn_observed` progress receives "waiting for the GPU:
      another window is using llama3.2:1b".
    * Release A, then B. Both answer, A's first.
    * B's `timing.queue_ms > 0`, and `jarvis_ms()` excludes it.
  - `a_waiting_turn_can_be_cancelled_at_once`: B waits; cancel B. B
    returns `Err(Cancelled)` before A is released.
    `orch.lane().available() == 0` while A holds the slot, and 1 after.
  - `a_failed_call_releases_its_slot`: the provider errors on A's call.
    Then `available() == 1`.
  - `settings_are_pinned_for_the_turn`: A's turn holds the gate. During it,
    call `set_model(&conv, "qwen")` on the same conversation. A's turn
    answers with llama3.2:1b (the recorded model), and the next turn uses
    qwen2.5:3b.
  - `a_remote_conversation_does_not_queue`: A holds the local slot. B, on
    remote, answers without waiting.
  - `slots_two_runs_two_local_calls_together`: config `slots = 2`. A and B
    both reach the provider before either is released.
- [ ] **Step 2:** Run `cargo test -p jarvis-core --test gpu_queue`.
  Expected: FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run `cargo test --workspace`. Expected: all pass.
- [ ] **Step 5:** Commit: "gateway: local model calls queue in the lane
  and say they wait".

### Task 4: Spill to remote

**Files:**
- Modify:
  * `core/src/orchestrator/prompt.rs`: `with_recall` returns `Recalled`.
  * `core/src/orchestrator/turn.rs`: the three `ask_model` sites and
    `converse` pass a `SystemPrompt`.
  * `core/src/orchestrator/gateway.rs`: the spill decision.
  * `core/src/orchestrator/outcome.rs` and `render.rs`: the footer and
    signals.
- Test: `core/tests/gpu_queue.rs`.

**Interfaces (produces):**
- `pub(super) struct Recalled { pub prompt: String, pub label: Label,
  pub spill: Option<(String, Label)> }`. `spill` is the same prompt with the
  recall block a remote tier would get (the filter in `recall_block` keyed
  on a `remote: bool` argument). It is computed only when
  `self.can_spill(rec)`.
- `pub(super) struct SystemPrompt { pub text: String, pub label: Label,
  pub spill: Option<(String, Label)> }`, built after `context::assemble`.
  The remote text is the spill prompt followed by
  `assembled.system[recalled.prompt.len()..]`, the history sections
  `assemble` appends. Its label is `assembled.system_label` absorbing the
  spill recall label.
- `ask_model(&self, system: &SystemPrompt, messages, tools, confirm,
  cancel, sink, rec)` replaces the `(system, system_label)` pair.
- `fn can_spill(&self, rec: &TurnRecorder<'_>) -> bool` is true when all
  of these hold:
  * the turn's pinned tier is local;
  * `self.remote` is `Some`;
  * the remote destination is remote;
  * `spill != Never`;
  * the turn is not already `spilled` and not `spill_withheld`.
- `TurnReport.spilled: bool`. `footer()` appends " (GPU busy)" when it is
  set.

Flow on `Admit::Busy(waiter)` in a local call where `can_spill`:
1. With `Ask`, `confirm.confirm("egress.spill", {model, waited_s})`. A no,
   or no confirmer, means `waiter.wait()` and run locally.
2. Otherwise, or on a yes: `rec.spilled = true`, report the spill progress
   line, and run `ask_model`'s remote path (egress check, approval,
   redaction) with `system.spill`. The `waiter` stays alive until that path
   decides.
3. If the remote path would be withheld (policy says never, or the user
   says no): clear `spilled`, set `spill_withheld`, `waiter.wait()` and run
   locally. The withheld path's "answer locally?" prompt is never shown for
   a spill.

- [ ] **Step 1: Write failing tests** (`start_paused = true`; A holds the
  local gate throughout):
  - `a_busy_gpu_spills_to_remote_after_five_seconds`:
    * B answers from the remote recording provider.
    * B's progress includes "GPU busy: answering with nemotron (remote)".
    * B's footer ends with " (GPU busy)".
    * B's signals contain `"spilled"`.
    * `conv_b.remote() == false`.
    * B's stored answer's model is the remote model.
    * Release A. B's next turn goes to local.
  - `a_spill_still_asks_before_sending_a_local_file`:
    * B's first turn reads a file outside the allow-list; release it.
    * In its second turn B spills. The confirmer is asked `egress.send`
      and says no.
    * The remote provider received nothing.
    * After A is released, B answers locally, with signal
      `"spill_withheld"`, and the footer has no "(GPU busy)".
  - `a_spill_leaves_out_recall_the_policy_would_block`:
    `recall_enabled = true`, and a stored message labelled with a file
    outside the allow-list matches B's question. The spilled request's
    system text does not contain it, and no `egress.send` is asked.
  - `spill_never_waits_for_the_gpu`: B waits past 5 s; nothing reaches
    remote.
  - `spill_ask_with_a_no_keeps_its_place`: `Spill::Ask`, and the confirmer
    says no to `egress.spill`. C queues after B's prompt. Releasing A runs
    B before C.
  - `a_spill_to_a_failing_remote_reports_the_error`: the remote provider
    errors. B's turn ends with the model error, and does not hang (finishes
    within paused 10 s after the spill).
  - `a_local_remote_tier_is_not_a_spill`: `[remote] base_url` is
    `http://127.0.0.1:11434`. B waits; there is no spill.
- [ ] **Step 2:** Run `cargo test -p jarvis-core --test gpu_queue`.
  Expected: the new tests FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run `cargo test --workspace`, then `cargo run -p
  jarvis-cli -- eval --offline`. Expected: all pass, with the offline eval
  unchanged.
- [ ] **Step 5:** Commit: "gateway: a busy GPU spills to remote, with a
  notice and the egress gate".

### Task 5: New conversations start local; a model choice per tier

**Files:**
- Modify:
  * `core/src/memory.rs`: the migration `sessions.remote_model TEXT`.
    `session_settings` returns `ConversationSettings { local: Option<String>,
    remote_model: Option<String>, remote: bool }`, and
    `save_session_settings(id, &ConversationSettings)`.
  * `core/src/conversation.rs`: `get`, `create`, `saved` and `preferred`
    use both slots, with the settings `last_local_model` and
    `last_remote_model`. `last_model` and `last_remote` are only read, for
    migration.
  * `core/src/orchestrator/mod.rs`: `new_conversation` is unchanged (local
    unless `[remote] default`).

Old rows: when `remote = 1` and `remote_model` is NULL, the old
`sessions.model` is the remote choice. The old `last_model` setting counts
as `last_local_model` only when it is one of `[model]`'s models.

- [ ] **Step 1: Write failing tests** in `conversations.rs`:
  - `a_new_conversation_starts_local_after_a_remote_one`: save a
    conversation on remote, then `create`. `remote() == false`, and its
    local model is the last local one used.
  - `a_remote_default_starts_remote`: `[remote] default = true`, so
    `create` is remote.
  - `the_local_choice_survives_a_trip_to_remote_and_a_restart`: qwen, then
    remote, then save, then a new registry, then `get`, then `/local`. The
    model is qwen2.5:3b.
  - `an_old_row_with_a_remote_model_loads`: an old-schema row with
    `model = 'nvidia/nemotron-3-ultra-550b-a55b:free'` and `remote = 1`
    loads with that as the remote choice and no local choice.
  - Update `a_new_conversation_starts_with_the_model_used_last` to the
    per-tier meaning.
- [ ] **Step 2:** Run `cargo test -p jarvis-core --test conversations`.
  Expected: FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run `cargo test --workspace`. Expected: all pass.
- [ ] **Step 5:** Commit: "conversations: start local; keep a model choice
  per tier".

### Task 6: Report, docs and the live check

**Files:**
- Modify:
  * `core/src/turn_report.rs`: plain words for `queued`, `spilled` and
    `spill_withheld` in `jarvis report`.
  * `README.md`: "Two windows at once", plus the `slots` and `spill`
    settings and the advice to raise `slots` with `OLLAMA_NUM_PARALLEL` on
    a bigger machine.
  * `config/` example config: the new keys, commented.
  * `docs/ROADMAP.md`: 6.5b done, 6.6 next.
  * `docs/real-use.md`.

- [ ] **Step 1: Write the failing test** in `turn_report.rs`:
  `new_signals_have_plain_words`. Each of the three maps to a non-empty
  phrase, not the raw key.
- [ ] **Step 2: Implement it**, then run `cargo test --workspace`. Expected:
  all pass.
- [ ] **Step 3: The live check.**
  * Run a throw-away daemon: temp `XDG_DATA_HOME`, `XDG_CONFIG_HOME` and
    `XDG_RUNTIME_DIR`, the release build, and the owner's Ollama, only when
    the owner has said the GPU is free.
  * Two `jarvis-tui` windows. In A, ask something that prompts, and leave
    the prompt open. In B, ask a question: it answers.
  * With A mid-local-call (a long qwen answer), B's local question shows
    the waiting line, then spills with the notice.

  Record the results in the ledger.
- [ ] **Step 4:** Run the full suite, clippy, fmt and the offline eval;
  commit "docs: Milestone 6.5b - turns in parallel"; open the PR; merge
  only after `gh pr checks <n>` exits 0.
