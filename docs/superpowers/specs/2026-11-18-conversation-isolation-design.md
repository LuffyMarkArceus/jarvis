# Milestone 6.5a - one conversation per window - design

Approved in conversation with the owner, 2026-11-18. Correctness first: 6.5a
makes windows separate; 6.5b lets their turns run at the same time.

## The bug

The daemon has one orchestrator holding one current session. Every window
talks to it, so all of this is shared between windows:

* which conversation is current - `/new` in one window moves every window,
  whose questions then go into (and draw context from) a conversation its
  header does not show. Reproduced with two real windows, 2026-11-18.
* the model and tier - `/model` in one window switches all of them;
* what `/bad` points at, and rephrase detection;
* the saved "active session" a new window resumes.

The cause: four fields that describe a conversation (`session_id`,
`use_remote`, `chosen`, `last_turn`) live on the object that serves all of
them.

## Principles

1. **Identity apart from services.** The orchestrator keeps what is shared -
   config, providers, tools, memory, undo, permissions, MCP. A `Conversation`
   holds what is one conversation's - its id, its model and tier, its last
   turn. A turn is "this conversation, using the shared services". 6.5b runs
   two `Conversation`s against one orchestrator at once, so the split is paid
   for once.
2. **Every conversation request names its conversation.** `{"method":
   "ask", "session": "08ff9130", ...}`. Not remembered per connection, which
   breaks on reconnect, for one-off CLI calls and for a future web UI.
3. **No default.** A conversation request without `session` is an error. A
   default would be the same hidden global under another name. One user,
   one machine, client and daemon shipped together: no compatibility shim.
4. **Refactor, then change behaviour.** A move-only step first, tests
   unchanged; then the behaviour change.

## Conversation and model

* A conversation's model and tier are its **current** ones: any turn may be
  preceded by a switch (`/model`, `/remote`, `/local`), freely and any number
  of times, affecting only that conversation.
* History is plain text messages, so it carries across models unchanged.
* **Each answer records the model that wrote it**: transcripts show
  `nemotron · remote` under one answer and `qwen · local` under the next.
* A switch from local to remote does not loosen privacy: every remote
  request is judged by the labels of everything in it, history included.
* **New conversations start with the model used most recently** (a stored
  preference, updated on every switch). Open conversations are unaffected.
* Model, tier and the last-used preference survive a daemon restart.

## Windows and sessions

* **A new window starts a new conversation** (owner, option A). Ctrl+S /
  `/sessions` resumes one; `jarvis up --continue` opens the last one used.
* A window keeps its conversation across reconnects and daemon restarts.
* Two windows may deliberately show the same conversation (`/switch`).
* A one-off `jarvis ask` gets a new conversation each time.

## Protocol

| Method | Session |
| --- | --- |
| `ask`, `model.list`, `model.set`, `tier.set`, `feedback`, `trust.list`, `trust.clear` | required (`session`) |
| `session.new` | creates one; returns it with its model and tier |
| `session.show` | required (the one to show); messages now carry their model |
| `session.current` | removed; `session.last` returns the last-used one for `--continue` |
| `session.list`, `session.delete`, `session.prune`, `status`, `tools.list` | none - about the daemon |

`session.switch` is removed: switching is the client's choice of which id it
sends, not daemon state.

Unknown session id: an error naming it.

## Components

* `core/src/conversation.rs` - `Conversation { id, remote, chosen, last_turn }`
  and `Conversations`, the daemon's registry: get or load by id (model and
  tier from `memory.db`), create, persist a switch, the last-used preference.
* `Orchestrator` - loses `session_id`, `use_remote`, `chosen`, `last_turn`.
  Turn and setting methods take a `&Conversation`; `TurnRecorder` carries it,
  so internal code reads `rec.conversation` instead of `self`. It keeps one
  conversation of its own for **single-conversation embeddings** - tests,
  each eval attempt, `jarvis ask` in direct mode - used by the existing
  `run_turn`; the daemon never uses it.
* `memory.db` - additive migrations, tested by starting twice: `sessions.model`,
  `sessions.remote`, `messages.model`, and a `settings` table for the
  last-used model.
* TUI - sends its `session` with every conversation request; `session.new` on
  first connect (or `session.last` with `--continue`); on reconnect, its own
  id; the header shows the conversation's own model.
* CLI - `jarvis ask` creates a session first; `jarvis model` and `jarvis tier`
  act on the last-used conversation, and say which.

## Out of scope

Running turns in parallel and the GPU queue (6.5b). Modes (6.6). Showing two
windows' activity on one conversation live.

## Tests

* **The test of record**: an IPC test with two real clients on a real socket
  against a throw-away daemon - window A `session.new`, window B asks; B's
  question is stored in B's session.
* `/model` in one conversation leaves another's model alone; a new
  conversation starts with the last used.
* Model hopping A -> B -> A -> C in one conversation: each answer records its
  model; the history is intact.
* Local history, then a switch to remote: the privacy rules still ask.
* Trust and `/bad` stay in their conversation.
* A conversation request without `session` is refused.
* Migrations run twice without harm; model and tier survive a restart.
* The move-only step: the full suite unchanged.
