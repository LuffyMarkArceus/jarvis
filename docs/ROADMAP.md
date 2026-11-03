# Roadmap

Confirmed by the owner 2026-11-03, except where an item says "proposed".
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

## Milestone 6.1 - harden Phase 6 (next)

| # | Item | Why now |
| --- | --- | --- |
| 1 | Relay warning: a note when an answer repeats a command or data-carrying address that came only from untrusted content (option A: every relayed command) | qwen handed the owner the attacker's instructions in 9/9 attempts; design approved |
| 2 | Router misroutes: "... and run it" runs `it`; a path in a sentence triggers an unfitting tool | Wrong behaviour, and it made the injection evals harder to write |
| 3 | Request ids: a resent request returns its stored result instead of running again (T3 Code's command receipts) | An approved write can run twice after a TUI reconnect; Phase 7 jobs need the same guarantee |
| 4 | Split `orchestrator.rs` (3,400 lines) into the turn loop, web steps, the egress and injection gates, and tool execution | Every phase has added to it; Phase 7 adds scheduled turns. After 1-3, so it moves tested code |

## Milestone 6.2 - small items (after 6.1)

* Marking for earlier turns: untrusted text from history is wrapped like
  text read in the current request.
* The edit tool reads a whole file into memory: refuse or stream past the
  undo size limit.
* A test for the TUI's undo handler.
* From the original Phase 6 plan, not yet built: a `filesystem.delete` tool
  that keeps a copy (today deletes go through `shell.run` and cannot be
  undone), and a dry-run preview for batch operations ("about to rename
  340 files").
* The two memory cases that fail on every qwen run
  (`memory.correction`, `memory.followup_file`).
* Answer checks that match text in the raw tool log.

## Proposed: a week of real use (before Phase 7)

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
* Request ids (6.1) make a retried job safe.

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

## Open questions for the owner

1. The real-use week before Phase 7 - yes or no?
2. Phase 10 (UI) before Phase 11 (other machines), as listed - or the
   other way round? Proposed: UI first, because it completes the
   single-machine product; other machines multiply it.
