# Roadmap

Confirmed by the owner 2026-11-03 and 2026-11-05.
Where Jarvis stands today is in [STATUS.md](STATUS.md).

Phases 7-11 grew out of the plan written after Phase 4 (voice, proactive
behaviour, semantic memory, other machines). On 2026-11-03 they were
renumbered to the order of work, and the TypeScript UI got a phase of its
own. Older commits and conversations use the old numbers: old 7 (voice) is
now 9, old 8 (proactive) 7, old 9 (semantic memory) 8, old 10 (other
machines) 11.

## Principles that decide the order

1. **Safety before power.** A phase that lets Jarvis do more - act
   unattended, listen, reach other machines - comes after the defences it
   depends on.
2. **Text first.** Everything works in the CLI and TUI before it gets a
   new way in: voice (Phase 9) and the TypeScript UI (Phase 10) are
   transports over a core that already works. Voice places three
   requirements on that core, noted under Phase 9, so they are planned for
   rather than retrofitted.
3. **Measure, then change.** A phase starts with eval cases for what it
   adds and ends with a verification run on qwen2.5:3b and a baselines
   entry.
4. **Pay small debts before they compound.** A bug found during a phase is
   fixed in the next milestone, not carried for three phases.
5. **One record.** STATUS.md is updated at the end of every phase; designs
   go in `docs/design/`, lessons in the learning log.
6. **Clients stay interchangeable.** The daemon protocol does not depend on
   which client (CLI, TUI, a future UI, another machine) or which transport
   (Unix socket today) is on the other end.

## Milestone 6.1 - harden Phase 6 (done, 2026-11-05, PR #2)

1. Relay warning: a note when an answer repeats a command or address that came
   only from untrusted content. qwen handed the owner the attacker's
   instructions in 9/9 attempts.
2. Router: "run" is a command only at the start of a request and only with a
   command after it; a path is a direct read or listing only when that is all
   that was asked.
3. A request is never run twice: `jarvis ask` re-ran a request in direct mode if
   the daemon died mid-request.
4. `orchestrator.rs` (3,500 lines) split by what each part does.

## Milestone 6.2 - small items (done, 2026-11-09, PR #3)

* Marking for earlier turns (tags explained only to a turn that has some).
* `read` returns the start of a file over 10 MB; `edit` refuses one.
* A test for the TUI's undo handler.
* `filesystem.delete`: one file at a time, asks first, kept for undo.
* Evals: relative paths and commands run in the sandbox (`[tools] work_dir`);
  a turn that never finishes says so; a raw tool log is not an answer.
* The two memory cases failing on small models are not a Jarvis bug: the
  history reaches the model and nemotron passes them.

## Milestone 6.4 - measure, and the obvious fixes (2026-11-17)

Real use (`docs/real-use.md`): the shell was denied, "run outside the
sandbox: <sentence>" became a command, and an 88-second turn could not be
explained.

* `shell.run` is `ask` for the owner (was a leftover `deny`).
* Per-turn timing from the provider and our own clock; the owner's time at
  prompts kept apart. `/timing`.
* `turns.jsonl`: one line per turn with friction signals; `/bad`, `/good`;
  `jarvis report`.
* A sentence after "run outside the sandbox:" goes to the model.

## Milestone 6.5a - one conversation per window (done, 2026-11-19)

Real use: `/new` in one window moved every window, and a question went into a
conversation its window did not show.

* A `Conversation` holds one conversation's state; the orchestrator holds
  none, and every caller passes one.
* Every conversation request names its `session`; none is a default.
* Each conversation keeps its model and tier across restarts; each answer
  records its model; new conversations start with the model used last.
* A new window starts a new conversation (`--continue` for the last one).
* Fixed on the way: the search-index triggers made deleting a conversation
  with messages, or updating any message, fail.
* [Design](superpowers/specs/2026-11-18-conversation-isolation-design.md).

## Milestone 6.5b - turns in parallel (done, 2026-11-20)

Two conversations at once: a quick question must not wait for a long one.

* No lock held for a turn: a window at a prompt no longer freezes the others.
  One turn at a time per conversation.
* The local lane: `[model] slots` local calls at once, first come first
  served, saying it waits and cancellable while waiting.
* A busy GPU spills a turn to the remote tier after `spill_after` seconds
  (`spill = auto | ask | never`), with a notice and the egress check.
* New conversations start local; a model choice per tier.
* [Spec](superpowers/specs/2026-11-19-parallel-turns-design.md).

## Milestone 6.6 - modes (next)

Autonomy (Plan / Interactive / Autopilot), approval (Manual / Assisted /
Allow all) and the sandbox, per conversation. Assisted approves what can be
undone or is contained, and asks the rest. Fixed whatever the mode: nothing
runs outside the sandbox after untrusted content, deny rules win, Jarvis's
files stay hidden, everything is audited. A Phase 7 job runs at a mode as its
ceiling. Spec first.

## Milestone 6.3 - OS sandbox (done, 2026-11-09)

Before Phase 7, because an unattended job needs a boundary that does not
depend on someone reading a prompt (owner, 2026-11-09).

* `shell.run` runs in bubblewrap: writes only to the workspace, no network,
  Jarvis's config, permissions, keys, memory, audit, undo and socket hidden.
* Keys removed from every command's environment.
* Outside the sandbox (`outside_sandbox`, "run outside the sandbox: X", app
  launches) always asks; never after untrusted content or unattended.
* [docs/design/sandbox.md](design/sandbox.md).

## Later, when wanted: Claude Code / Codex beside Jarvis

Driving the owner's own installed agent CLIs from Jarvis. Revisit after the UI
and/or other-machines phases, if at all (owner, 2026-11-09). Their tools would
run outside Jarvis's guardrails, so it is a separate trust boundary, not a
model provider; check each product's terms first.

## Later, when wanted: batch operations

Renaming or moving many files at once, with a dry-run preview first ("about to
rename 340 files"). Infrequent but very useful (owner, 2026-11-08). Jarvis has no
batch operation yet - each change is its own call and prompt - so the preview is
designed with the operation, not before it.

## A week of real use (before Phase 7)

Use Jarvis for ordinary work for a week and log every failure: wrong
route, wrong answer, a prompt that was noise, something it could not do.
The log reorders the rest of this roadmap. So far the order has been
decided by engineering judgement; from here, use should decide it.

## Phase 7 - proactive behaviour

Jarvis acts without being asked: "remind me at 6", "tell me when this
download finishes", "every morning, summarise my unread mail".

* A job scheduler in the daemon; time, file and process-exit triggers;
  desktop notifications.
* **A permission ceiling for jobs** (T3 Code's `DispatchModeLimit`): a job
  runs at a set level, and nothing it starts goes higher.
* Unattended means nobody to ask: Phase 6 already says the action is not
  run. Phase 7 adds where those go instead - a queue the owner reviews.
* Stored results for request ids (T3 Code's command receipts): a retried job
  returns the first run's result instead of running again. No client retries
  today, so this is built with the first thing that does.

## Phase 8 - semantic memory

* An embeddings sidecar and hybrid ranking (keywords plus meaning): "what
  car do I drive?" finds "I bought a Corolla".
* Structured facts about the owner, kept apart from conversation and
  editable.
* Possibly a local entity recogniser, which would also let the redactor
  catch names.

## Phase 9 - voice

* Wake word (openWakeWord), speech-to-text (whisper.cpp, `base` or
  `small`, CPU), text-to-speech (Piper) - the empty `stt` and `tts`
  sidecars.
* Target: under ~1.5 s from the end of speech to the first spoken word.

What voice requires of the core, to plan for before this phase:

1. **Streaming everywhere.** Speech starts on the first sentence, so
   answers must stream - including redacted remote answers, which today
   are shown whole.
2. **Confirmations that can say "use the screen".** A spoken "[y] run
   once" is slow, and audio nearby - or a page read aloud - could answer
   for the owner. The confirmation protocol must let a prompt require a
   screen.
3. **An always-on microphone is a privacy boundary** of the same weight as
   "what may leave the machine" (Phase 5b): decided explicitly, not by
   default.

## Phase 10 - TypeScript UI

A desktop or web client over the same daemon protocol, once everything
works in the CLI and TUI. The TUI already proves a second client works;
this is a third. Every client multiplies the surfaces a feature must
reach (T3 Code's "hit every surface"), which is a reason to add it late.

## Phase 11 - other machines and Android

* The daemon protocol stays independent of its transport (Unix socket
  today, TLS over Tailscale later).
* A thin agent per device registers its tools; permissions say which
  device a tool may act on.
* Secret references (T3 Code's `request_secret`): a secret entered
  privately, never in the conversation.
