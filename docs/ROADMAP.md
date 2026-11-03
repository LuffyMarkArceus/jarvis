# Roadmap

**Draft, 2026-11-03 - awaiting the owner's confirmation.** Where Jarvis
stands today is in [STATUS.md](STATUS.md).

Phases 7-10 come from the plan written after Phase 4 (in the project's
conversation history, not the repository until now). Their numbers are
kept as names; the order below is the proposed order of work.

## Principles that decide the order

1. **Safety before power.** Each phase that lets Jarvis do more (act
   unattended, listen, reach other machines) comes after the defences it
   depends on. Phase 6 came before Phases 7-10 for this reason.
2. **Measure, then change.** A phase starts with eval cases for what it
   adds and ends with a verification run on qwen2.5:3b and a baselines
   entry.
3. **Pay small debts before they compound.** A bug found during a phase is
   fixed in the next milestone, not carried for three phases.
4. **One record.** STATUS.md is updated at the end of every phase; design
   decisions go in `docs/design/`, lessons in the learning log.

## Milestone 6.1 - harden Phase 6 (next)

| # | Item | Why now |
| --- | --- | --- |
| 1 | Relay warning: a note when an answer repeats a command or data-carrying address that came only from untrusted content (option A: every relayed command) | qwen handed the owner the attacker's instructions in 9/9 attempts; design approved |
| 2 | Router misroutes: "... and run it" runs `it`; a path in a sentence triggers an unfitting tool | Wrong behaviour, and it made the injection evals harder to write |
| 3 | Request ids: a resent request returns its stored result instead of running again (T3 Code's command receipts) | An approved write can run twice after a TUI reconnect; Phase 8 jobs need the same guarantee |
| 4 | Split `orchestrator.rs` (3,400 lines) into the turn loop, web steps, the egress and injection gates, and tool execution | Every phase has added to it; Phase 8 adds scheduled turns. Done after 1-3 so it moves tested code |

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

## Phase 8 - proactive behaviour (proposed next phase)

Jarvis acts without being asked: "remind me at 6", "tell me when this
download finishes", "every morning, summarise my unread mail".

* A job scheduler in the daemon; time, file and process-exit triggers;
  desktop notifications.
* **A permission ceiling for jobs** (T3 Code's `DispatchModeLimit`): a job
  runs at a set level, and nothing it starts goes higher.
* Unattended means nobody to ask: Phase 6 already says the action is not
  run. Phase 8 adds where those go instead - a queue the owner reviews.
* Request ids (6.1) make a retried job safe.

**Why before voice:** it builds on what Phase 6 just settled (what happens
with nobody to ask), it is useful every day, and the original plan asked
the owner to choose between the two.

## Phase 7 - voice

* Wake word (openWakeWord), speech-to-text (whisper.cpp, `base` or
  `small`, CPU), text-to-speech (Piper) - the empty `stt` and `tts`
  sidecars.
* Target: under ~1.5 s from the end of speech to the first spoken word,
  so speech starts on the first sentence. That needs redacted remote
  answers to stream again (a carried-over gap).

## Phase 9 - semantic memory

* An embeddings sidecar and hybrid ranking (keywords plus meaning): "what
  car do I drive?" finds "I bought a Corolla".
* Structured facts about the owner, kept apart from conversation and
  editable.
* Possibly a local entity recogniser, which would also let the redactor
  catch names.

## Phase 10 - other machines and Android

* The IPC protocol stays independent of its transport (Unix socket today,
  TLS over Tailscale later).
* A thin agent per device registers its tools; permissions say which
  device a tool may act on.
* Secret references (T3 Code's `request_secret`): a secret entered
  privately, never in the conversation.

## Open questions for the owner

1. Phase 8 before Phase 7, as proposed?
2. Is Milestone 6.1's item 4 (splitting the orchestrator) worth a
   milestone of its own, or folded into Phase 8's first task?
3. Anything missing - for example the TypeScript UI the README's
   architecture line mentions, which no phase covers yet?
