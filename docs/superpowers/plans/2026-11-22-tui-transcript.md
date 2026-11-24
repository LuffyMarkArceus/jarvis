# The Transcript (Milestone 6.7b) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Steps fold into one row per turn; a turn's file changes have a diff
view with revert; approvals are cards in the conversation with a diff and a
Tab-started decline reason; answers render Markdown; modes sit in the picker.

**Architecture:**
* **Protocol.** New protocol types go in `common/src/ipc.rs`.
* **Daemon.** The daemon emits `Step` events beside `Progress`. It builds
  the approval `Preview` and the turn's `changes` in `core`, with diffs
  from the `similar` crate, and passes decline reasons through a new
  `ConfirmationHandler::answer`.
* **TUI.**
  * A work-log entry per turn.
  * The approval card in the transcript instead of the pop-up.
  * An `Overlay::Changes` diff view.
  * `tui/src/markdown.rs`, built on `pulldown-cmark`.

**Tech Stack:** Rust, ratatui 0.28, tokio, `pulldown-cmark` (tui), `similar`
(core).

**Spec:** `docs/superpowers/specs/2026-11-21-tui-transcript-design.md`. Its
"Settled after the builder check" calls are requirements.

## Global Constraints

* **Step states:** `running`, `done`, `failed`, `declined`, `denied`,
  `deferred`, `withheld`, `untrusted`.
* **State marks:** `◐` running, `●` done, `✗` failed/denied/untrusted, `⊘`
  declined/withheld, `⋯` deferred.
* **The folded row:**
  `▸ {n} steps · {secs:.1} s` + ` · {k} files changed` (when k > 0) +
  ` · {d} declined` (when d > 0). When expanded, `▾` replaces `▸`.
* **Approval card key line:** `y yes · n no · Tab reason` (plus choice
  keys, as today).
* **Decline with a reason:** the tool result reads
  `Tool {name} was declined by the user: {reason}`.
* **Limits:**
  * diff context: 3 lines;
  * per-file diff in `changes`: cut at 2000 lines;
  * card preview: cut at 400 lines;
  * delete preview: cut at 200 lines;
  * a cut says `… (N more lines)`.
* **Revert prompt:** `Revert {n} file(s)? y/n`.
* **Dependencies:** `pulldown-cmark` (default features off; only what is
  needed) in `tui`; `similar` in `core`. Nothing else is new.
* **Tests:** none touches the owner's daemon, data, GPU or `~`.
* **Before every commit:** `cargo fmt`, then
  `cargo clippy --workspace --all-targets -- -D warnings` must be clean.

## Review Focus

1. **The card's preview must be what runs.** An `edit` whose `old_string`
   appears 3 times previews all 3 replacements, because the tool uses
   `str::replace` (Task 4).
2. **A turn cancelled after one write** still lists that change, and `r`
   reverts it (Task 3, Task 7).
3. **A Markdown answer with an unclosed code fence or raw HTML** shows its
   text; none is lost (Task 8).
4. **A card arrives while the input holds a half-typed steer:** the draft
   comes back after the card is answered (Task 6).
5. **A file Jarvis created this turn (new file)** diffs from nothing, and
   `r` deletes it, which is undo's existing behaviour for a new file
   (Task 3, Task 7).

---

### Task 1: Protocol types

**Files:** Modify `common/src/ipc.rs`.

**Interfaces (produces):**
- `pub struct Step { pub id: Uuid, pub step: u32, pub kind: String, pub name: String, pub summary: String, pub state: String, #[serde(default)] pub detail: Option<String>, #[serde(default)] pub ms: Option<u64> }`
- `Message::Step(Step)`, tagged `"step"`.
- `pub struct Preview { pub kind: String, pub text: String }`. `kind` is
  `"diff"` or `"command"`.
- `ConfirmationRequest.preview: Option<Preview>`, with
  `#[serde(default, skip_serializing_if = "Option::is_none")]`.
- `ConfirmationReply.reason: Option<String>`, same serde attributes.
- `pub struct FileChange { pub undo_ids: Vec<String>, pub path: String, pub tool: String, pub added: usize, pub removed: usize, #[serde(default)] pub diff: Option<String> }`
  (`None`: no copy kept).

- [ ] **Step 1: Write failing tests** in `ipc.rs`:
  `step_round_trips`, `confirmation_without_preview_or_reason_still_parses`
  (old JSON), `file_change_round_trips`.
- [ ] **Step 2:** Run `cargo test -p jarvis-common`. Expected: FAIL.
- [ ] **Step 3:** Implement. Fix every `ConfirmationRequest {…}` and
  `ConfirmationReply {…}` literal in the workspace by adding
  `preview: None` and `reason: None`.
- [ ] **Step 4:** Run `cargo test --workspace`. Expected: all pass.
- [ ] **Step 5:** Commit: "protocol: step events, previews, reasons, file
  changes".

### Task 2: The daemon emits step events

**Files:**
- Modify:
  * `core/src/orchestrator/outcome.rs`: `TurnRecorder.steps`, plus
    `step_start`/`step_end`.
  * `core/src/orchestrator/gateway.rs`: model calls.
  * `core/src/orchestrator/execute.rs`: tool calls in `execute_call`.
  * `core/src/orchestrator/mod.rs`: `run_turn_observed` and
    `run_with_footer` gain
    `steps: Option<&UnboundedSender<jarvis_common::ipc::StepEvent>>`.
  * `core/src/daemon.rs`: `ask_steered` and `ask_with_footer` gain
    `steps`.
  * `core/src/ipc/server.rs`: the chunk task forwards steps.
- Test: `core/tests/steps.rs` (new) and `core/tests/two_windows.rs`.

**Interfaces:**
- **Consumes:** `Step` (Task 1).
- **Produces:**
  - `pub struct StepEvent { pub step: u32, pub kind: &'static str, pub name: String, pub summary: String, pub state: &'static str, pub detail: Option<String>, pub ms: Option<u64> }`
    in `common/src/ipc.rs`: a `Step` without the request id, which the
    server adds.
  - `TurnRecorder::step_start(kind, name, summary) -> u32` and
    `step_end(n, state, detail, ms)`.
  - **Model steps:** summary "asking {short model}", detail "{tokens}
    tokens" when known.
  - **Tool steps:** summary `describe_step(name, args)`. State from the
    outcome status: `Succeeded`→done, `Failed`→failed, `Denied`→denied,
    `Declined`→declined, `NeedsConfirmation`→declined, `Withheld`→withheld,
    `Untrusted`→untrusted, `Deferred`→deferred. Detail is the first line of
    `for_model(outcome)`, cut at 60 chars.
  - All existing callers pass `None`.

- [ ] **Step 1: Write failing tests** in `steps.rs` (scripted provider, a
  `list_dir` call, then an answer):
  - `a_tool_turn_emits_running_and_done_pairs`: the events are
    `[1 model running, 1 model done, 2 tool running, 2 tool done "N entries…",
    3 model running, 3 model done]`, with the same step numbers in each
    pair.
  - `a_declined_call_ends_declined` (rule ask, confirmer no).

  In `two_windows.rs`: `steps_reach_the_window`. An `ask` that writes
  (`ByPrompt`) shows a `Message::Step` with state `declined` after the
  window says no.
- [ ] **Step 2:** Run them. Expected: FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run `cargo test --workspace`. Expected: all pass.
- [ ] **Step 5:** Commit: "steps: the daemon reports each step as it
  starts and ends".

### Task 3: A turn's file changes, with diffs

**Files:**
- Modify:
  * `core/Cargo.toml`: `similar`.
  * `core/src/undo.rs`: `pub fn before(&self, id: &str) -> Result<Option<Vec<u8>>>`.
  * create `core/src/changes.rs`: the diff and grouping.
  * `core/src/daemon.rs` and `core/src/ipc/server.rs`: `changes` on the
    answer and cancel replies.
- Test: `core/tests/changes.rs`.

**Interfaces (produces):**
- `pub fn unified(before: &str, after: &str, path: &str) -> (String, usize, usize)`:
  a diff with context 3, plus the lines added and removed. Cut at 2000
  lines.
- `pub fn turn_changes(store: &UndoStore, session: &str, since: DateTime<Utc>) -> Vec<FileChange>`:
  - one entry per path;
  - `undo_ids` newest first;
  - the diff is from the oldest `before` this turn (or empty, if the file
    did not exist) to the file now (or empty, if it is gone);
  - non-UTF-8 content gives `diff: Some("binary file")`;
  - a change with no copy kept gives `diff: None`.
- `Daemon::ask_steered` notes `since = Utc::now()` at the start and
  returns the `changes` with its outcome. The server adds `"changes"` to
  the answer and cancel replies.

- [ ] **Step 1: Write failing tests** in `changes.rs` (a temp undo folder,
  `before_change`/`after_change` as `undo.rs` tests do):
  - `one_change_diffs_before_to_now`;
  - `a_file_changed_twice_is_one_entry_from_its_oldest_copy`;
  - `a_new_file_diffs_from_nothing`;
  - `a_deleted_file_diffs_to_nothing`;
  - `another_conversations_changes_are_not_listed`;
  - `changes_before_the_turn_are_not_listed`;
  - `a_change_with_no_copy_has_no_diff`.

  In `two_windows.rs`: `an_answer_and_a_cancel_carry_changes`.
- [ ] **Step 2:** Run them. Expected: FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run `cargo test --workspace`. Expected: all pass.
- [ ] **Step 5:** Commit: "changes: a turn reports the files it changed,
  with diffs".

### Task 4: Approval previews and decline reasons

**Files:**
- Create: `core/src/preview.rs`.
- Modify:
  * `core/src/orchestrator/mod.rs`: `Answer` and
    `ConfirmationHandler::answer`.
  * `core/src/orchestrator/execute.rs`: the Ask path and
    `ask_after_untrusted` call `answer`.
  * `core/src/ipc/server.rs`: `SocketConfirmer` implements `answer`, and
    sets `preview` from `crate::preview::preview`.
- Test: unit tests in `preview.rs`; `core/tests/modes_gate.rs`-style
  `core/tests/reasons.rs`.

**Interfaces (produces):**
- `pub fn preview(tool: &str, args: &Value) -> Option<Preview>`:
  - `filesystem.write`: `unified(current-or-empty, content)`;
  - `filesystem.edit`: `unified(current, current.replace(old, new))`,
    which is all occurrences, as the tool does;
  - `filesystem.delete`: every line as removed, cut at 200;
  - `shell.run`: kind `command`, text
    `"$ {command}\n(in {work_dir or .}{, OUTSIDE the sandbox})"`;
  - anything else: `None`.
  - Cut at 400 lines.
- `pub struct Answer { pub approved: bool, pub choice: Option<char>, pub reason: Option<String> }`
- `async fn answer(&self, tool, args, choices) -> Answer` on
  `ConfirmationHandler`. Its default calls `choose` and uses no reason.
- `TimedConfirm` and `NoPrompts` forward or implement it.
- **The decline message:** with a reason, the outcome output is
  `Tool {name} was declined by the user: {reason}`, and the audit outcome
  is `declined_by_user`, with the reason in the message.

- [ ] **Step 1: Write failing tests:**
  - `preview.rs`:
    - `a_write_previews_the_diff_of_the_new_content`;
    - `an_edit_previews_every_replacement`: `old_string` three times,
      three `-`/`+` pairs (Review Focus 1);
    - `a_delete_previews_removed_lines_cut_at_200`;
    - `shell_previews_the_command_and_where`.
  - `reasons.rs` (scripted provider, a confirmer whose `answer` returns
    `reason: Some("use notes/ instead")`):
    - `a_reason_reaches_the_models_next_request`: the Script's `sent`
      contains the declined message with the reason;
    - `a_plain_no_reads_as_before`.
- [ ] **Step 2:** Run them. Expected: FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run `cargo test --workspace`. Expected: all pass.
- [ ] **Step 5:** Commit: "approvals: a diff preview, and a reason with a
  no".

### Task 5: The work log in the TUI

**Files:**
- Modify:
  * `tui/src/client.rs`: decode `Message::Step` into `ClientEvent::Step`.
  * `tui/src/app.rs`: `Role::Work`, the turn's work entry, and Ctrl+O.
  * `tui/src/ui.rs`: render live and folded steps; mouse click on a row.

**Interfaces (produces):**
- `Entry.work: Option<WorkLog>`, where
  `pub struct WorkLog { pub request: Uuid, pub steps: Vec<Step>, pub done: bool, pub ms: u64, pub changes: Vec<FileChange>, pub open: bool, pub failed_turn: bool }`.
- **Creating the entry.** When a turn starts (an `ask` is sent), push an
  `Entry { role: Role::Work, work: Some(..) }` after the user entry.
  - Steps update it by `(request, step)`.
  - The ask reply sets `done`, `ms` (from `timing.total_ms`) and `changes`.
  - A failed turn sets `failed_turn`, and its row says
    `changes: see /undo`.
- `App.show_steps: bool`, toggled by Ctrl+O. An entry shows its steps when
  `!done || open || show_steps`.
- `fn work_row(log: &WorkLog) -> String` builds the folded row (Global
  Constraints).
- **Mouse:** a left click on a row in the transcript area toggles that
  entry's `open`. `ui.rs` records each work entry's row range in
  `App.work_rows: Vec<(u16, usize)>` (screen row, entry index) while
  drawing.

- [ ] **Step 1: Write failing tests:**
  - `steps_show_live_then_fold`;
  - `the_row_counts_steps_files_and_declines`:
    `"▸ 3 steps · 12.4 s · 2 files changed · 1 declined"`;
  - `ctrl_o_opens_every_turn`;
  - `a_click_on_a_row_opens_that_turn_only`;
  - `a_failed_turn_points_to_undo`;
  - render: `the_folded_row_draws`.
- [ ] **Step 2:** Run `cargo test -p jarvis-tui`. Expected: FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run `cargo test -p jarvis-tui`. Expected: PASS.
- [ ] **Step 5:** Commit: "tui: steps live, then folded into one row".

### Task 6: The approval card

**Files:**
- Modify:
  * `tui/src/app.rs`: the card state replaces `Overlay::Confirm`, with
    reason mode and draft parking.
  * `tui/src/ui.rs`: draw the card above the input, replacing the
    `draw_confirm` pop-up.

**Interfaces (produces):**
- `App.card: Option<Card>`, where
  `pub struct Card { pub request: ConfirmationRequest, pub scroll: u16, pub reasoning: bool, pub parked: String }`.
- **On a confirmation:** park the input text in `parked` and clear the
  input.
- **Card keys:**
  * `y` / choice keys: `Action::Confirm`, with `reason: None`;
  * `n` / Esc (not reasoning): decline;
  * Tab: `reasoning = true`;
  * while reasoning, keys edit the input; Enter sends the decline with
    `reason: Some(input)` (empty means a plain no); Esc leaves reasoning;
  * PgUp/PgDn scroll the preview;
  * Ctrl+C: cancel the turn, as today.
- **Closing the card** restores `parked` to the input.
- `Action::Confirm` gains `reason: Option<String>`. `main` puts it in
  `ConfirmationReply`.

- [ ] **Step 1: Write failing tests:**
  - `a_card_parks_the_draft_and_gives_it_back` (Review Focus 4);
  - `y_and_n_answer_at_once`;
  - `typing_t_never_starts_a_reason`: `t` on an untrusted card is the trust
    choice, never text;
  - `tab_then_text_then_enter_declines_with_the_reason`;
  - `esc_in_reason_mode_leaves_it_without_answering`;
  - `pgdn_scrolls_the_preview`;
  - render: `the_card_shows_the_diff_in_colour`.
- [ ] **Step 2:** Run `cargo test -p jarvis-tui`. Expected: FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run `cargo test -p jarvis-tui`. Expected: PASS.
- [ ] **Step 5:** Commit: "tui: approvals are cards in the conversation".

### Task 7: The diff view

**Files:** Modify `tui/src/app.rs` (`Overlay::Changes`, Ctrl+D,
`/changes`, the revert prompt), `tui/src/ui.rs` (draw it),
`tui/src/commands.rs` (`/changes`).

**Interfaces:**
- **Consumes:** `WorkLog.changes` (Task 5); undo actions (`UndoApply(id)`,
  as `/undo` uses).
- **Produces:**
  - `Overlay::Changes { files: Vec<FileChange>, selected: usize, scroll: u16, confirm_revert: bool }`;
  - `r` sets `confirm_revert` and shows `Revert {n} file(s)? y/n`;
  - `y` emits `Action::UndoApply(id)` for each `undo_id` of every file,
    newest first, in one `Vec<Action>`;
  - Ctrl+D and `/changes` open the last turn with changes. With none, the
    note "The last turn changed no files."

- [ ] **Step 1: Write failing tests:**
  - `ctrl_d_opens_the_last_turns_changes`;
  - `up_down_choose_a_file_and_pgdn_scrolls`;
  - `r_then_y_reverts_exactly_the_turns_ids_newest_first` (Review Focus 2
    and 5: a cancelled turn's changes and a new file);
  - `r_then_n_does_nothing`;
  - render: `the_diff_view_colours_plus_and_minus`.
- [ ] **Step 2:** Run `cargo test -p jarvis-tui`. Expected: FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run `cargo test -p jarvis-tui`. Expected: PASS.
- [ ] **Step 5:** Commit: "tui: a diff view of a turn's changes, with
  revert".

### Task 8: Markdown in answers

**Files:**
- Create: `tui/src/markdown.rs`.
- Modify: `tui/Cargo.toml` (`pulldown-cmark`), `tui/src/ui.rs`
  (assistant entries).

**Interfaces (produces):**
- `pub fn render(text: &str, width: usize) -> Vec<Line<'static>>`.
  Styling:
  - headings: bold, cyan;
  - bold, italic;
  - inline code: yellow, on dark grey;
  - code fence: each line indented 2, green, cut at the width with `…`, a
    dim language label above it;
  - lists: `• ` and `1. `, nested 2 spaces;
  - quotes: `│ ` in grey;
  - links: their text, then ` (url)` dim;
  - a rule: `─` across;
  - tables: cells padded to the column width, ` │ ` between.
- **Wrapping** keeps styles. It splits on spaces by display width, with
  `unicode-width`.
- **Raw HTML and unknown events** emit their text unstyled.
- **Assistant entries** use `render`, except an entry still streaming,
  which stays plain.

- [ ] **Step 1: Write failing tests** in `markdown.rs`, each asserting the
  line texts and spot styles:
  - `headings_are_bold`;
  - `bold_italic_code_inline`;
  - `a_fence_keeps_its_lines_and_is_not_rewrapped`;
  - `nested_lists`;
  - `quotes_have_a_bar`;
  - `links_show_text_then_url`;
  - `a_table_aligns`;
  - `an_unclosed_fence_keeps_every_line` (Review Focus 3);
  - `raw_html_stays_as_text`;
  - `wrapping_keeps_bold_across_lines`;
  - in `ui.rs`: `a_streaming_answer_is_plain`.
- [ ] **Step 2:** Run `cargo test -p jarvis-tui`. Expected: FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run `cargo test -p jarvis-tui`. Expected: PASS.
- [ ] **Step 5:** Commit: "tui: markdown in answers".

### Task 9: Modes in the picker; docs; the live check

**Files:**
- Modify: `tui/src/app.rs` (`Overlay::Models` gains the mode rows),
  `tui/src/ui.rs`.
- Docs: `README.md`, `docs/ROADMAP.md` (6.7b done, then Phase 7 design),
  `docs/real-use.md`.

**Interfaces:**
- **The mode rows** come after the models:
  `Autonomy: {..}`, `Approval: {..}`, `Sandbox: {on|off}`.
- Enter on a mode row sends `mode.set` with the next value.
  * Autonomy: interactive → autopilot → plan → interactive.
  * Approval: manual → assisted → allow_all → manual.
  * Sandbox: toggles.
  * The picker stays open, and updates from the reply.

- [ ] **Step 1: Write failing tests:**
  - `the_picker_lists_mode_rows_after_models`;
  - `enter_on_a_mode_row_sends_the_next_value`.
- [ ] **Step 2:** Run `cargo test -p jarvis-tui`. Expected: FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run `cargo test -p jarvis-tui`. Expected: PASS.
- [ ] **Step 5: The live check.** Use a throw-away daemon (temp XDG dirs)
  on the free remote model.
  * **Over the socket:** a write turn shows step events and a confirmation
    carrying a diff preview. A decline with a reason reaches the model,
    whose answer mentions it. A turn that writes returns `changes` with a
    diff.

  Record the results in the ledger.
- [ ] **Step 6:** Full suite, clippy, fmt, offline eval; commit "docs:
  Milestone 6.7b - the transcript"; PR; merge only after `gh pr checks <n>`
  exits 0.
