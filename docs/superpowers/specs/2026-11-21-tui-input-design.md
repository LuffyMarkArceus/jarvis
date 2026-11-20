# The input box (Milestone 6.7a) - Design

Status: scope and direction approved by the owner (2026-11-21). Part one of
6.7; 6.7b is the transcript: work log, approval cards, diffs, markdown.

## Intent

The owner finds T3 Code much better to use than Claude Code, and wants
Jarvis's TUI to follow T3's ways wherever they are better: "use the best
from T3 code, not claude code". The most pressing pain is editing: "I should
be able to do shortcuts like Ctrl + arrow key to move to next word ... to
move to end of line, I have to go through each character".

6.7a makes the input box work like a modern text field (T3's composer is
one) and adds three things T3 does well:
* a `/` commands menu;
* **Queue**: a message typed while Jarvis is busy waits and is sent after;
* **Steer**: a message typed while Jarvis is busy goes into the running
  turn.

## How it works today

* **`tui/src/app.rs` `Input`** is a `Vec<char>` plus a cursor. It supports:
  * a character left or right;
  * Home and End, over the whole message;
  * insert, Backspace and Delete;
  * Alt+Enter for a newline.

  There is no word movement or word deletion.
* **Up/Down** walk history when the box is empty or history is being
  walked, and scroll otherwise.
* **Ctrl bindings in use:** C (cancel or quit), Q (quit), H (help, with
  F1), N (new), S (sessions), P (model picker), L (clear), U (clear the
  input), R (reconnect).
* **Text sent while a turn is pending** is refused: "Still waiting for the
  daemon…". The text is put back in the box.
* **The owner's terminal is Konsole 26.08.** Without the kitty keyboard
  protocol, a terminal sends Ctrl+Backspace as Ctrl+H and Shift+Enter as
  Enter. crossterm 0.28 can ask for the protocol
  (`PushKeyboardEnhancementFlags`) and can check whether the terminal
  supports it (`supports_keyboard_enhancement`).
* **The turn loop** (`core/src/orchestrator/turn.rs`) calls the model once
  per round: model call, tools, model call. A direct answer (`converse`) is
  a single call.

## Goals

### 1. Editing keys

These follow T3's standard text-field behaviour, plus terminal habits that
do not clash with it.

| Keys | Does |
|---|---|
| Ctrl+← / Ctrl+→, Alt+B / Alt+F | jump a word back / forward |
| Home / End, Ctrl+A / Ctrl+E | start / end of the current line |
| Ctrl+Home / Ctrl+End | start / end of the whole message |
| Ctrl+Backspace, Ctrl+W, Alt+Backspace | delete the word before the cursor |
| Ctrl+Delete, Alt+D | delete the word after the cursor |
| Ctrl+K | delete to the end of the line |
| Ctrl+U | clear the input (unchanged) |
| Up / Down | move between lines of a multi-line message; history at the first or last line |
| Shift+Enter, Alt+Enter | new line |

* **A word** is a run of letters and digits. Anything else separates words,
  and a jump skips the separators first. So Ctrl+→ steps through
  `/mnt/storage/CS` one folder at a time.
* **Ctrl+H is no longer help** (F1 still is): without the keyboard
  protocol it is Ctrl+Backspace, so Ctrl+H deletes a word.
* **The keyboard protocol.** At start the TUI asks for it
  (`DISAMBIGUATE_ESCAPE_CODES`) when `supports_keyboard_enhancement()` says
  yes, and pops it on exit, panic included.
* **Without the protocol:** Shift+Enter cannot be told from Enter and
  sends. Alt+Enter always makes a new line, and the input's bottom hint
  says which keys work here.

### 2. The `/` commands menu

* **When it opens.** When the input starts with `/` and has no space yet,
  a menu opens above the input. It lists the commands that start with what
  was typed, then those that contain it, each with a one-line description.
* **Keys while it is open:**
  * Up/Down select;
  * Tab completes the selected command and adds a space;
  * Enter runs the typed command if it matches one exactly, and otherwise
    completes the selection;
  * Esc closes the menu and keeps the text.
* **The command list is defined once**, as one table of name, argument
  hint and description. Both `/help` and the menu are built from it.

### 3. Queue

* **Sending while busy.** While a turn is running, Enter on a message (not
  a command) **queues** it, by default. It shows at the end of the
  transcript as a dim, dashed entry: `⋯ queued: <text>`.
* **When it is sent.** When the turn ends normally, the first queued
  message is sent as the next question, and so on through the queue.
* **When the queue is held.** If the turn was cancelled by the owner, or
  the daemon connection dropped, the queue is held, as T3 does. A line
  says: "Queue held: /resume sends it, Alt+Up edits the last one."
* **Editing.** Alt+Up, with an empty input, takes the newest queued
  message back into the input, removing it from the queue, to edit or
  delete it.
* **Lifetime.** The queue lives in the window. It survives a daemon
  reconnect, but not closing the window.

### 4. Steer

A message that goes into the running turn: the model sees it at its next
step, the way T3's Steer does.

* **How to steer:**
  * `/steer <text>` always;
  * Ctrl+Enter, where the keyboard protocol tells it from Enter, flips the
    default (queue or steer) for that message;
  * `[tui] follow_up = "queue" | "steer"` sets the default
    (`"queue"` unless set).
* **Protocol:** `steer {session, text}`.
  * The daemon adds the text to the conversation's steer inbox and replies
    at once with `{status: "steering"}`.
  * If the conversation is not answering, it replies
    `{status: "not_running"}` and the TUI queues the message instead.
* **The turn loop.** Before each model call after the first, it takes
  every steer in the inbox, in order. Each one:
  * is added to the request as a user message, labelled `Origin::User`;
  * is stored in memory as a user message;
  * reports progress: "steered: <first 40 chars>".
* **When a steer is not used.** A steer that arrives when no model call is
  left (a one-call direct answer, or the last round) is returned in the
  `ask` response as `"unused_steers": [..]`. The TUI puts those at the
  front of its queue, so nothing typed is lost.
* **What a steer does not change.** It does not change the turn's pinned
  mode or model, and it is not a new turn: no new busy marker, and the
  deferred actions are not dropped.

### 5. The input box shows its state

* The bottom hint shows the keys that apply now, for example
  "Enter queue · Ctrl+Enter steer · Ctrl+C cancel turn" while busy.
* It shows "queued: 2" when the queue is not empty.
* Ctrl+C cancels a turn, as today. Esc does not; it only closes the menu
  or a picker.

## Settled after the builder check (designer's calls)

1. **No steer is lost, however the turn ends.** Each steer carries an id
   the TUI chooses, and the TUI keeps each one it sent as "in flight".
   * **A normal answer** returns `"unused_steers"` (their ids and text).
   * **A cancelled answer** (`{cancelled: true}`) returns
     `"used_steers"`: the ids applied before the cancel.
   * **A failed turn or a dropped connection:** every in-flight steer goes
     back to the front of the queue.

   In every case except a normal answer, the queue is **held**, not sent.
   The owner sees, for example, "queued: use Pictures" and can drop one
   that had in fact been applied. Nothing typed is lost; a duplicate is
   visible and removable.
2. **Only a turn that can take a steer accepts one.** A conversation is
   steerable only while `model_with_tools` runs its rounds. A direct
   answer (`converse`), a tool route, `/approve` or undo is not steerable:
   `steer` replies `not_running` and the TUI queues the message.
   * **Steers waiting when the model gives its final answer:** if rounds
     remain, the loop does not stop there. It adds the steers and asks
     again, so a late steer still changes the answer instead of becoming
     the next question.
   * **Steers waiting when no rounds remain** are returned unused.
3. **The queue belongs to the conversation it was typed in.** The TUI
   keeps one queue per session id. Switching conversations (`/new`,
   Ctrl+S) leaves a queue with its conversation, held. Coming back shows
   it, and `/resume` sends it. Re-queued steers go to their own
   conversation's queue.
4. **Esc does not cancel a turn.** Ctrl+C does, as today; the earlier hint
   wording was a slip.
5. **A steer is stored as a user message** (the owner's own words,
   `Origin::User`) when it is applied. A later prompt may then hold two
   user messages in a row, which providers accept. Auto-titling no longer
   needs exactly two messages: it titles a conversation that has no title
   once its first answer exists.
6. **Commands while busy.** These stay refused as today: "Still waiting
   for the daemon". Only these work during a turn:
   * `/steer`;
   * `/resume`;
   * Alt+Up (edit the newest queued message);
   * `/queue` (lists the queue).

   A steer's reply is tracked apart from the turn (`steer_pending`), so
   the single pending slot still means the turn.

## Non-goals

* Selecting text with Shift+arrows, an undo history in the input box, or
  styling markdown while typing. Selection is left to the terminal
  (Shift+drag): Shift+selection plus Ctrl+C would clash with cancelling a
  turn.
* Saving the queue across closing the window.
* T3's draft stash, mentions and dictation.
* Everything in 6.7b: the work log, approval cards, diffs, markdown in
  answers, the mode section in the picker.

## Anti-goals

* A keystroke that silently does something else on a terminal without the
  protocol. Ctrl+H must delete a word there, never open help mid-sentence.
* A queued or steered message lost without a trace: a turn that ends, is
  cancelled, or drops the connection.
* A steer that changes the turn's mode, model or permissions, or slips
  past the gate.
* The menu or queue stealing keys the owner already uses: Up/Down history,
  Ctrl+C cancel, Esc.

## Approaches considered

* **Editing:**
  1. **Extend our own `Input` (chosen).** It is small, pure and easy to
     test, and its key bindings are ours to keep consistent.
  2. **The `tui-textarea` crate.** It is a full editor widget with
     selection and undo. But it brings its own emacs-style bindings that
     clash with ours, a ratatui version to match (we use 0.28), and a
     larger dependency. Rejected for now; it can be revisited if selection
     or undo is wanted later.
* **Steer:**
  1. **A per-conversation inbox read by the turn loop before each model
     call (chosen).** It is small, and it uses the turn's existing rounds.
  2. **Cancel the turn and resend it with the new text.** It is simple,
     but it throws away the work done so far and repeats tools. Rejected.
  3. **Queue only, no steer.** It would miss what makes T3 better on long
     Autopilot runs. Rejected.

## Testing

* **`Input`, as tables:** each key from a given text and cursor gives the
  expected text and cursor. Multi-line Up/Down and history at the edges.
* **App:**
  * Ctrl+H deletes a word and does not open help;
  * the menu filters, completes and runs, and Esc keeps the text;
  * Enter while busy queues, and the queue is sent in order when the turn
    ends;
  * the queue is held after a cancel, and `/resume` sends it;
  * Alt+Up edits the newest queued message;
  * `/steer` sends `steer`, `not_running` queues, and `unused_steers` are
    re-queued at the front.
* **Daemon** (a gated model, as in 6.5b), with a turn of two model calls:
  * a steer sent during the first call is in the second call's request,
    and in memory;
  * a steer to a one-call turn comes back in `unused_steers`;
  * a steer to a conversation that is not answering gets `not_running`.
* **Terminal:** the keyboard protocol is pushed and popped, checked by a
  unit test of the setup function's choices, not a real terminal.
