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
