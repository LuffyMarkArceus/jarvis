# Modes (Milestone 6.6) - Design

Status: approach and shape approved by the owner (2026-11-20). Follows 6.5b.

## Intent

The owner uses Copilot, VS Code and Claude at work, and wants the same
three controls in Jarvis. Each is set per conversation:

* **Autonomy** decides who drives:
  * **Plan:** look and propose; change nothing.
  * **Interactive:** check in along the way. In the owner's words: "the one
    where it will keep on asking me: push the branch? raise a PR?"
  * **Autopilot:** "I have given full control over and it will just
    proceed, without any prompt to me". For example, it implements a
    feature and files the PR, which then waits for review.
* **Approval** decides which actions need a yes: **Manual**, **Assisted**
  or **Allow all**.
* **Sandbox** is a toggle: on or off.

Protections that never change with the mode (owner, 2026-11-11, restated
in 6.6):

* After untrusted content, nothing runs outside the sandbox.
* Deny rules always win.
* Jarvis's files stay hidden.
* Everything is audited.
* A Phase 7 job runs with a mode as its ceiling.

**Autopilot with an action its approval does not cover** (owner, option A):
it skips that action, keeps going, and ends with a report of what is
waiting. `/approve` runs those actions. Option B was acceptable to the
owner and may come later: approve once up front for the actions it expects
to need, and fall back to A for anything it did not foresee.

## How it works today

* **One global rule set.** `permissions.toml` holds allow / ask / deny per
  tool and pattern ("most restrictive wins"). The owner's file allows reads
  and asks for `filesystem.write`, `filesystem.edit` and `shell.run`.
  * Evaluation: `PermissionManager::check_permission(tool, args)`.
  * The gate: `Orchestrator::gate_and_run` in
    `core/src/orchestrator/execute.rs`.
* **Outside the sandbox.** A `shell.run` with `outside_sandbox`, or any
  shell when the sandbox is not ready (`runs_outside`), is handled as
  follows:
  * It always asks.
  * It is refused after untrusted content.
  * It is refused when `[sandbox] allow_outside = false`.
  * App launches run outside too.
  * "run outside the sandbox: X" is a router phrase (`router.rs`).
* **After untrusted content**, every action asks
  (`ask_after_untrusted`), and deny still wins.
* **Undo.** `filesystem.write`, `filesystem.edit` and `filesystem.delete`
  keep an undo copy when the file is under `[undo] max_file_bytes`.
  `undo_note(name, args)` already says `kept`, `too_large` or
  `unavailable`.
* **The sandbox (bubblewrap)** mounts `/` read-only, binds
  `[sandbox] writable` plus `[tools] work_dir` writable, hides Jarvis's
  files and keys, and has no network unless `[sandbox] network = true`
  (`core/src/sandbox.rs`, `Sandbox::bwrap_args`).
* **No modes.** Autonomy is effectively Interactive, with up to
  `max_rounds` (default 8) model rounds per turn and no check-ins.

## The finding that shapes Assisted

**Contained is not reversible.** The sandbox protects the network, keys and
the rest of the system, but its writable folders are writable for real,
with no undo copies. A sandboxed `rm -rf /mnt/storage/CS/jarvis` must not be
approved automatically.

So a command that is **approved automatically** runs in a **read-only
sandbox**: every writable folder is mounted read-only, and there is no
network. A command that needs to write asks, and then runs with the normal
writable sandbox. The OS enforces this, not a model. The same read-only
sandbox is Plan mode's shell.

## Goals

1. **A mode per conversation.** It is stored with the conversation
   (`sessions` columns) and survives a restart.
   * New conversations take `[modes]` defaults from config: Interactive ·
     Manual · Sandbox on, which is today's behaviour.
   * A turn pins the mode it started with, like the tier in 6.5b.
2. **One pure decision function.**
   `decide(mode, call, context) -> Decision`, where `Decision` is one of
   `Run`, `RunReadOnly`, `Ask`, `Defer(reason)` or `Refuse(reason)`. It
   replaces the branching in `gate_and_run`. `context` carries:
   * the rule verdict (allow / ask / deny, with its pattern);
   * whether the turn read untrusted content;
   * whether the call runs outside the sandbox;
   * whether an undo copy can be kept;
   * whether the call writes at all (a read tool, or a shell command).

   `decide` does no I/O. It can be tested as a table.
3. **The protections first, then the mode.** `decide` checks, in this
   order:
   1. **Deny:** `Refuse`.
   2. **Outside the sandbox after untrusted content:** `Refuse`.
   3. **Outside the sandbox while `[sandbox] allow_outside = false`:**
      `Refuse`.
   4. **Plan mode, and the call would change anything:** `Refuse`, with
      "Plan mode changes nothing; /go to carry out the plan".
   5. **The approval mode transforms the verdict:**
      * **Manual:** rule verdicts as written.
      * **Assisted:** an `ask` becomes `Run` for a file change whose undo
        copy is `kept`, and `RunReadOnly` for a sandboxed `shell.run`.
        Outside the sandbox, too large to undo, and after untrusted
        content: still `Ask`.
      * **Allow all:** every `ask` becomes `Run`, except the cases rules 1
        to 3 already decided.
   6. **After untrusted content, inside the sandbox:** an action still asks
      whatever the approval mode. That is today's `ask_after_untrusted`,
      kept: Allow all does not mean a page can act.
   7. **Autopilot:** any `Ask` left becomes `Defer`.
4. **Running read-only.** `RunReadOnly` runs `shell.run` in the sandbox
   with every writable folder mounted read-only (`Sandbox::read_only()` or
   a flag to `bwrap_args`).
   * If such a command fails because it tried to write (read-only file
     system), the outcome tells the model the command was run read-only
     and that writing needs approval.
   * Manual and Allow all keep the normal writable sandbox.
5. **The sandbox toggle.** `/sandbox off` makes this conversation's
   `shell.run` run outside the sandbox. `/sandbox on` restores it.
   * Outside, the approval mode decides as for any outside call: Manual
     and Assisted ask; Allow all runs. Protections 2 and 3 still apply.
   * The router phrase "run outside the sandbox: X" stays for a literal
     one-off command.
6. **Plan mode.**
   * Only tools that do not change anything are offered: the filesystem
     read, list, search and stat tools, `process.list`, `system.info`, the
     web tools, and `shell.run` read-only.
   * The prompt asks for a numbered plan of what it would do.
   * `/go` switches the conversation to Interactive and asks it to carry
     out the plan above. `/go auto` does the same in Autopilot.
7. **Autopilot.**
   * Its step budget is `[modes] autopilot_rounds` (default 24) instead of
     `max_rounds`.
   * Its prompt says to pick sensible defaults instead of asking
     clarifying questions, and to state what it assumed.
   * It never prompts:
     * an action that would ask is deferred;
     * an egress prompt (sending a local file to the remote model) counts
       as "not sent", and is reported;
     * a spill with `spill = "ask"` counts as a no.
   * The turn ends with a report: what it did, what it assumed, and what
     is waiting.
8. **Deferred actions and `/approve`.**
   * Deferred calls are kept per conversation, with name, args, why they
     were deferred, and the turn they came from.
   * They are shown in the turn's answer and on request (`/approve` with
     no argument lists them).
   * `/approve` runs them in order, as confirmed by the owner, through the
     gate with the protections re-checked. `/approve 2` runs one;
     `/approve none` drops them.
   * They are dropped when the conversation's next turn starts. A pending
     list is not carried into an unrelated request.
9. **Interactive** is today's behaviour, plus one prompt line: when a
   request is ambiguous, ask instead of guessing. Approval prompts follow
   the approval mode.
10. **Visible state.**
    * The TUI header shows `Plan|Interactive|Autopilot · Manual|Assisted|Allow all · Sandbox on|off`.
    * Commands: `/mode` (a picker, like `/model`), `/mode autopilot`,
      `/approval assisted`, `/sandbox off`.
    * A key cycles autonomy: Shift+Tab, as in Claude Code.
    * `session.show`, `session.new` and `status` return the mode.
    * CLI: `jarvis mode [autonomy|approval|sandbox] [value]` acts on the
      last conversation and says which, like `jarvis model`.
11. **Audit.** Every decision is audited with its mode:
    `auto_approved_undoable`, `auto_approved_read_only`, `deferred`,
    `refused_plan`, plus the existing outcomes.
12. **Signals.** `deferred` and `plan_refused` are added to the turn log
    and `jarvis report`.

## Settled after the builder check (designer's calls)

1. **How a sandboxed command that writes reaches a prompt in Assisted.**
   `shell.run` gains an optional argument `writes: bool`, and the tool
   description tells the model to set it when the command changes files.
   * `writes` not set: `RunReadOnly`.
   * `writes: true`: `Ask` (`Defer` in Autopilot), then it runs in the
     normal writable sandbox.
   * A read-only run that fails on a read-only file system returns: "This
     ran read-only and needs to write. Call it again with writes: true to
     ask the owner."
   * Running it twice is safe: a read-only run leaves nothing behind except
     in its private `/tmp`.
   * The model does not need to be right about `writes`: the OS enforces
     read-only, and a wrong `true` only costs a prompt.
   * Manual and Allow all ignore `writes` and use their verdict with the
     writable sandbox.
2. **Modes apply to every call, the owner's typed commands included**
   (`Route::Tool`: "delete x.txt", "run outside the sandbox: X", app
   launches). This is predictable, and it keeps "Plan changes nothing" and
   "Autopilot never prompts" true without exceptions.
   * A typed `shell.run` counts as `writes: true`, since there is no model
     to say otherwise.
   * In Plan mode the owner gets "Plan mode changes nothing; switch with
     /mode, or /go".
   * In Autopilot the command is deferred, and `/approve` runs it.
3. **What `/approve` checks.**
   * `/approve` runs deferred items as owner-confirmed: no prompt, audited
     `approved_deferred`.
   * Before each one it re-checks protections 1 to 3 and Plan mode, under
     the conversation's mode at the time of `/approve`.
   * Each item keeps the untrusted sources of the turn it came from, for
     the audit and for protection 2.
   * `/approve` is not a model turn, but it takes the conversation's busy
     marker. It removes the items it ran; the list is dropped when the
     next model turn starts.
4. **No bubblewrap** (`sandbox::ready` false).
   * There is no read-only sandbox, so Assisted's `RunReadOnly` becomes
     `Ask`, outside, as today.
   * Plan does not offer `shell.run`.
   * `/sandbox on` reports that the sandbox is unavailable.
   * CI without bwrap tests these rows directly.
5. **Undoable means "a copy kept at the time, which will survive the run".**
   * Pruning (by total size or age) never removes a copy made in the last
     24 hours.
   * If keeping a new copy would need that, the change is not undoable:
     `Ask` in Assisted, `Defer` in Autopilot.
   * If keeping the copy fails at the moment of the write (today a
     `warn!`, and the write goes ahead), an auto-approved write is not
     made, and the outcome says why.
6. **The ceiling (`Mode::within`) caps approval and sandbox only.**
   * Order: Manual < Assisted < Allow all, and sandbox on < off.
   * A Phase 7 job is unattended, so it is always Autopilot: it never
     prompts and defers what its approval does not cover.
   * `within` never turns Autopilot into Interactive.

## Non-goals

* Option B (approve up front). It may come later and builds on `Defer`.
* A model deciding what is safe (rejected: unreliable on 1B-3B models, and
  a target for injection).
* Rule profiles or files per mode.
* Phase 7 jobs themselves. 6.6 provides the ceiling they will use:
  `Mode::within(ceiling)` clamps a mode to the ceiling, and is tested now.
* Diffs in approval prompts and showing the plan richly (6.7).

## Anti-goals

These would make it a failure even if it worked:

* Any mode reaching a protection: a page-triggered command outside the
  sandbox, a deny rule bypassed, Jarvis's files shown.
* Assisted approving a write that cannot be undone.
* Autopilot prompting.
* A deferred action run without the owner's `/approve`.
* A mode silently changed by another window's turn (modes are per
  conversation, and pinned per turn).

## Configuration

```toml
[modes]
autonomy = "interactive"   # plan | interactive | autopilot
approval = "manual"        # manual | assisted | allow_all
sandbox = true
autopilot_rounds = 24
```

## Approaches considered

1. **Modes reshape the rule verdict in one pure decision function
   (chosen).** `permissions.toml` stays the baseline; the protections are
   checked before the mode, so no mode can reach them; it is testable as
   an exhaustive table.
2. **A rule file per mode.** Rejected. Rules can name tools and patterns,
   but cannot say "if an undo copy can be kept" or "if the command only
   reads", which is what Assisted is. It also means three files to keep in
   step.
3. **A model classifying safety** (Claude's auto mode). Rejected, as above.

## Testing

* **`decide`** as a table: every autonomy × approval × sandbox × {read,
  undoable write, write too large, sandboxed shell, outside shell, app
  launch} × {trusted, untrusted} × {allow, ask, deny} rule. The
  protections hold in every row.
* **Read-only sandbox** (where bwrap runs): a write to a writable folder
  fails read-only; a read works.
* **Integration** with scripted providers:
  * Assisted writes an undoable file without asking.
  * Assisted asks for a command that writes.
  * Autopilot defers `git push`, and `/approve` runs it.
  * Plan refuses a write, and `/go` carries the plan out.
  * Allow all with sandbox off runs outside without asking, but never
    after untrusted content.
* **Persistence:** a mode survives a restart. A turn pins its mode.
* **The TUI:** header text, `/mode`, `/approval`, `/sandbox`, `/approve`,
  and the Shift+Tab cycle.
