# The transcript (Milestone 6.7b) - Design

Status: scope agreed with the owner (2026-11-21), approach 1 chosen. Part two of
6.7, after the input box (6.7a).

## Intent

The owner finds T3 Code far better to use than Claude Code and wants Jarvis's
TUI to follow T3's ways. This milestone brings in five of them:

- **Steps** are shown live while a turn runs, then fold into one row once it
  ends.
- **A diff view** shows the files a turn changed, with a revert.
- **Approvals** appear as a card in the conversation, showing a diff. Typing
  a reply declines the request and gives the model a reason.
- **Markdown** in answers is rendered.
- **Modes** are switched in the model picker.

## How it works today

- **Progress.** The daemon sends `Message::Progress { id, text }`
  ("asking qwen (step 1)", "writing notes.md"). The text comes from
  `TurnRecorder::step`, using `describe_step(tool, args)` and the
  model-call lines in `gateway.rs`. The TUI shows the latest text in the
  header. Steps leave no trace once the turn ends.
- **Approvals.**
  - The daemon sends
    `ConfirmationRequest { id, tool, args, prompt, choices }`, where `prompt`
    is one rendered line.
  - The TUI shows it in a fixed 70×9 pop-up (`draw_confirm`).
  - The reply is `ConfirmationReply { id, approved, choice }`.
  - `SocketConfirmer` answers `ConfirmationHandler::confirm` and `choose`.
  - A "no" reaches the model as "Tool X was declined by the user".
- **Undo.** Each write, edit or delete keeps a `before` copy with metadata
  (path, tool, session, time, id), under the undo folder. `UndoStore`
  plans and applies undo by id. The TUI has `/undo` with a preview.
- **Answers** are plain wrapped text (`ui.rs` `transcript_lines`).
- **The Ctrl+P picker** lists models (`Overlay::Models`).
- **Dependencies:** there is no Markdown or diff crate yet.

## Goals

### 1. Step events (protocol)
- **The message.** A new
  `Message::Step(Step { id, step, kind, name, summary, state, detail, ms })`:
  - `id` is the ask request's id;
  - `step` counts from 1 within the turn;
  - `kind` is `model` or `tool`;
  - `name` is the model or the tool;
  - `summary` is one line of what it is ("list_dir ~/Pictures",
    "asking qwen2.5:3b");
  - `state` is `running`, `done`, `failed`, `declined`, `denied`,
    `deferred`, `withheld` or `untrusted`;
  - `detail` is one short line of the result ("34 files", "exit 1",
    "412 tokens");
  - `ms` is the time the step took.
- **When it is sent.** One event when a step starts (`running`), and one
  when it ends, with the same `step` number.
- **Progress stays** for the header line.

### 2. The work log in the TUI
- **While the turn runs,** its steps show above the coming answer, one line
  each: a state mark, the summary, then the detail (dim).
  - `◐` running, `●` done, `✗` failed, denied or untrusted, `⊘` declined
    or withheld, `⋯` deferred.
- **When the turn ends,** they fold into one row:
  `▸ 5 steps · 12.4 s · 2 files changed`.
  - "files changed" appears only if the turn changed any.
  - A step that did not succeed adds to the row, for example
    "· 1 declined".
- **Opening it.** Ctrl+O opens or closes every turn's steps. A click on a
  `▸` row opens or closes that turn's steps only.
- **Kept per window only,** as the owner chose. Reopened conversations show
  answers without steps.

### 3. Files a turn changed, and the diff view
- **The `ask` reply** gains
  `changes: [{ undo_ids, path, tool, added, removed, diff }]`, one per file
  this turn changed (see the settled calls below).
  - `diff` is a unified diff of the `before` copy against the file now,
    computed with `similar`, with 3 lines of context.
  - A deleted file diffs against nothing. A new file diffs from nothing.
  - A change with no copy (over the size limit) is listed without a diff:
    "no copy kept".
  - Diffs are cut at 2,000 lines per file, saying so.
- **The diff view.** Ctrl+D opens a full-window view of the last turn's
  changes, and `/changes` does the same.
  - A file list on the left, the selected file's diff on the right.
  - Lines starting with `+` are green, `-` red, `@@` cyan.
  - Up/Down choose a file; PgUp/PgDn scroll the diff.
  - `r` reverts this turn's files, asking "Revert 2 files? y/n". It applies
    undo to those `undo_id`s, newest first, through the existing undo path,
    and reports each one.
  - Esc closes the view.

### 4. Approval cards
- **The request** gains `preview: Option<Preview>`, where
  `Preview { kind: "diff" | "command", text }`:
  - `filesystem.write`: the diff of the file now against the new content;
  - `filesystem.edit`: the diff the edit would make;
  - `filesystem.delete`: the whole file as removed lines, cut at 200 lines;
  - `shell.run`: the command, with its working folder and whether it runs
    outside the sandbox.
  - The daemon builds the preview, because only it can read the file. It
    is cut at 400 lines, saying so.
- **The card replaces the pop-up.** It sits at the bottom of the
  transcript, above the input:
  - a title line ("Jarvis wants to edit notes.md");
  - the preview, scrollable with PgUp/PgDn while the card is open;
  - a key line: "y yes · n no · Tab reason".
- **Keys while a card is open:**
  - `y` or a choice key approves;
  - `n` or Esc declines;
  - Ctrl+C cancels the turn, as today.
- **Declining with a reason.** Tab starts a reason (see the settled calls
  below); Enter then declines with that text as the reason.
- **The reply** gains `reason: Option<String>`. The model's tool result for
  a decline becomes "Tool X was declined by the user: <reason>". The reason
  is audited.
- **The protocol change** behind this:
  - `ConfirmationHandler` gains
    `async fn answer(&self, tool, args, choices) -> Answer`, where
    `Answer { approved, choice, reason }`;
  - its default comes from `choose`, and `confirm` and `choose` remain;
  - `SocketConfirmer` implements `answer`;
  - the gate's Ask path calls `answer`, and so does `ask_after_untrusted`
    when it applies.

### 5. Markdown in answers
- **Assistant entries are rendered from Markdown with `pulldown-cmark`:**
  - headings in bold, in colour;
  - `**bold**`, `*italic*`, `` `code` ``, the latter on a dim background;
  - fenced code blocks indented, in a code colour, not re-wrapped (long
    lines are cut at the width, with `…`), and with the language if given;
  - bullet and numbered lists, nested;
  - `>` quotes with a `│` bar;
  - links as their text with the URL dim after it;
  - `---` as a rule;
  - tables as aligned plain rows.
- **Other entries stay plain:** your messages, system lines and errors.
- **Wrapping** follows the styled text.
- **Streaming.** A message still streaming is rendered plain, and as
  Markdown once complete. Half a Markdown document renders badly.

### 6. Modes in the picker
- The Ctrl+P picker lists the models, then a "Mode" section with three
  rows: `Autonomy: Interactive`, `Approval: Manual`, `Sandbox: on`.
- Enter on a mode row moves it to the next value, sending `mode.set`.
- The header updates from the reply, as in 6.6.

## Settled after the builder check (designer's calls)

1. **A draft in the input when a card opens is parked, not used.** A
   half-typed steer or queued message is taken out of the input when a card
   opens, and put back when the card closes. The card's reason uses the
   input on its own. A draft never becomes a decline reason.
2. **A reason starts with Tab (safety).** With the card open, single keys
   answer: `y`, `n`, Esc, and choice keys such as `a` (always allow) or `t`
   (trust these). Typed letters do not start a reason, so "try X instead"
   can never press `t` and widen trust.
   - **Tab** starts a reason: keys then type into the input.
   - Enter declines with the reason; Esc leaves reason mode.
   - The key line says "y yes · n no · Tab reason".

   Choice keys that widen permissions (`a`, `t`) stay single keys, as
   today.
3. **One entry per file, not per undo entry.** A turn that changes a file
   twice lists it once, diffed from its oldest `before` copy this turn to
   the file now. The undo ids are kept as a list, and `r` reverts them all,
   newest first. "N files changed" counts paths.
4. **Which replies carry `changes`.** The ask reply carries them both on an
   answer and on a cancel (`{cancelled: true, changes}`), so Ctrl+D and `r`
   work after Ctrl+C. A failed turn (`Response::err`) has no payload: its
   writes are reachable with `/undo`, and the row says "changes: see
   /undo". Accepted.
5. **Where a reason is accepted.** Only the gate's Ask prompt and the
   untrusted-content prompt take one. The web-egress prompt
   (`check_web_call`) does not.
6. **Mode rows in the picker do not ask for confirmation,** as
   `/approval allow-all` does not today.

## Non-goals

- Storing steps or changes in memory.db. They are kept per window, as the
  owner chose.
- Diffs of changes made by shell commands. They have no undo copies, so the
  turn's row can only say "ran 3 commands".
- Syntax colouring inside code blocks.
- Editing a proposed change before approving it.
- Mouse selection of text. Shift+drag stays.

## Anti-goals

- A diff in a card that differs from what will run. The preview must be
  computed from the same arguments the gate will use, after
  `sanitize_args`.
- A revert that touches a file the turn did not change. It works only
  through the turn's own `undo_ids`, with undo's existing checks: it
  refuses if the file changed since.
- An answer garbled by the renderer. Any Markdown it cannot render shows as
  its plain text, never lost.
- A decline reason lost, or treated as a new message to the model. It goes
  only into the declined tool result.

## Approaches considered

1. **Structured events plus `pulldown-cmark` and `similar` (chosen by the
   owner).** Both crates are standard, pure Rust, and well tested on the
   edge cases.
2. **The same, but our own Markdown and diff code.** Rejected: two classic
   sources of subtle bugs, where a wrong diff means approving the wrong
   thing.
3. **No protocol change: the TUI parses the progress text.** Rejected: it
   is fragile, and it cannot give diffs.

## Testing

- **Protocol:** `Step`, `Preview` and `reason` round-trip through serde,
  and old messages without the new fields still parse.
- **Daemon** (scripted provider):
  - a two-step turn emits `running`/`done` pairs with the same `step`
    number, and a declined call emits `declined`;
  - the `changes` in the reply list the turn's undo entries, with diffs;
  - a write's confirmation carries a diff preview equal to the change that
    then runs;
  - a decline with a reason reaches the model's next request.
- **TUI:**
  - live steps, then folded into one row, with Ctrl+O to open;
  - the row's counts;
  - the diff view's keys and its revert, which sends the undo for exactly
    the turn's ids;
  - the card's keys: `y`, `n`, typed reason plus Enter, Esc;
  - the Markdown renderer as table tests (each construct gives the expected
    styled lines; unknown input stays as text);
  - streaming stays plain until complete;
  - the picker's mode rows send `mode.set`;
  - render tests with `TestBackend` for the folded row, the card and the
    diff view.
