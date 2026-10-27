# Design: local and hosted model tiers (Phase 5b)

Status: **built** (2026-10-27): steps 1-5. Open items under "Known gaps".
Decisions are the owner's; the rules and mechanisms are how they are kept.

## Problem

Jarvis runs on a local model by default. On this laptop the best one that
fits (qwen2.5:3b) passes most of the eval suite, but small models fail at
multi-step work. Some users will want a hosted frontier model for harder
requests. Others never will. Both must be first-class.

The provider part is already done: any OpenAI-compatible server works with a
URL, a model name and an API key variable (`core/src/openai.rs`). What is
missing is the **policy**: which turn may go to which model, and what context
may leave the machine with it.

## Goals

1. Local is the default and is never blocked. Hosted is opt-in, per user.
2. The trade-off is a number, not a disclaimer: the eval suite runs against
   both and reports the gap.
3. Nothing sensitive leaves the machine without an explicit rule allowing
   it, and the user is told when a rule stopped or changed something.
4. Nothing is silent: no quiet fallback, no quiet redaction (learning log #5).

Non-goals for now: automatic per-request routing by difficulty, spending
caps, remote machines (Phase 10).

## Decisions

| # | Question | Owner's decision | Rule that implements it |
| --- | --- | --- | --- |
| 1 | What may go remote by default | `user`, `web` and `memory`, but never secrets or PII, even if the user pasted them | Allowed origins pass through the redactor first |
| 2 | File contents | Only from paths the user allow-lists (a project folder) | Allow-list starts empty; allowed files are still redacted; `never_paths` cannot be allowed |
| 3 | On violation | Ask each time | Scoped answers: once / always for this path / answer locally. Unattended turns answer locally |
| 4 | Providers | Free and paid both allowed; free-tier data use shown explicitly | Each provider declares `data_policy`; shown at selection and in `status` |
| 5 | Visibility | Every answer is marked | Footer on every turn, local included |

## Origins, and why they propagate

A turn's prompt is assembled from several sources. Each message gets the
origin of what it contains:

| Origin | Comes from | Default remote |
| --- | --- | --- |
| `user` | what the user typed | yes, redacted |
| `memory` | recalled messages from other sessions | yes, redacted |
| `web` | `mcp.search`, `mcp.fetch` | yes, redacted |
| `file` | `filesystem.read` / `search` output | only allow-listed paths, redacted |
| `shell` | `shell.run` output | no (asks) |
| `system` | `process.list`, `system.info` | no (asks) |

**Origins propagate to the assistant.** After a file is read, the
assistant's next message may quote it ("your DB password is hunter2"). Judged
by role alone, that message is `assistant` and would leave. So within a turn,
every assistant message carries the most restrictive origin seen before it.
This is the taint tracking Phase 6 needs for prompt injection ("text that
came from `web` must not trigger `shell` without asking"): one mechanism
serves privacy and security.

**Origins must be stored, not inferred.** A deterministic `read notes.txt`
is saved to history as an *assistant* message containing the file, with no
tool message beside it. On the next turn that file content looks like plain
assistant text. Worse, memory recall would bring it back as `memory`, which
may go remote - so "memory is allowed" would quietly override "files need
an allow-list". Each persisted message therefore stores its label, and
recall carries the stored label instead of `memory`. `label()` takes those
stored labels; until persistence lands (step 3), the remote tier must not be
enabled.

The check sits in one place: where a request is handed to a provider. A
local provider accepts everything. A remote provider gets the request only
after the check passes.

```toml
[egress]
file_allow = []                     # e.g. ["~/projects/jarvis"]
never_paths = ["~/.ssh", "~/.gnupg", "~/.aws", "~/.config/jarvis", "**/.env"]
```

## Redaction

Allowed text still passes through a redactor before it leaves.

**Detected in v1**, because a pattern finds them reliably:

* Secrets: private-key blocks, provider key formats (`sk-`, `ghp_`, `AKIA`,
  `xox[bp]-`, ...), `Bearer` tokens, JWTs, and the value in `NAME=value` or
  `name: value` when the name says KEY, TOKEN, SECRET or PASSWORD.
* Structured PII: email addresses, phone numbers, card numbers (Luhn
  checked, so order numbers are not hit), IP addresses.

**Not detected in v1:** names, street addresses, health or other free-text
personal details. Finding those needs a model that recognises entities, and
the local 1B cannot be trusted with it. This limit is stated here and in the
README rather than implied away: "redacts PII" would be a promise the code
does not keep. It is also why file contents need an allow-list on top of
redaction - each layer catches what the other misses.

**Placeholders are reversible, locally.** `shrey@example.com` becomes
`<EMAIL_1>` in the request; the same value gets the same placeholder
throughout a turn. If the answer uses `<EMAIL_1>`, Jarvis puts the real value
back before showing it. The value never leaves; the answer is still usable.
Secrets are not restored into answers: a secret in an answer is a leak on the
screen even if it never left the machine.

## Asking

When a remote request would carry something the policy does not allow:

```
This turn would send file contents (/home/u/notes/todo.txt) to openrouter.
  [o] send once   [a] always allow ~/notes   [l] answer locally
```

* "Always" writes the allow-list, so the next turn does not ask.
* A turn with nobody to ask (scheduled jobs, Phase 8; the eval harness)
  answers locally. Choosing that default is the reason this exists before
  Phase 8.
* `never_paths` are not offered: there is nothing to approve.

## Providers and their data policy

```toml
[[providers]]
name = "openrouter"
kind = "openai"
base_url = "https://openrouter.ai/api/v1"
api_key_env = "OPENROUTER_API_KEY"
data_policy = "may_train"     # no_training | may_train | unknown
```

`unknown` is treated as `may_train`. A provider that may train shows a
notice when it is selected and in `jarvis status`: "this provider may log or
train on what you send". It is not repeated on every turn, where it would
become wallpaper. Which providers train is not hard-coded: policies change,
and a stale list would be worse than the user's own declaration.

## The footer

Every answer ends with one line, local turns included:

```
qwen2.5:3b · local · 1.2k tokens
claude-x · remote: openrouter · 3.4k tokens · 2 redacted
```

Marking only remote turns would make "local" the absence of a mark, and an
absence is not noticed. Cost is shown when the provider reports it
(OpenRouter does); otherwise tokens.

## Choosing the tier

* `model.tier = "local"` (default) or `"remote"`; `/local` and `/remote`
  switch the session in the TUI.
* If the remote tier is unreachable the turn fails visibly with "retry
  locally". It never falls back silently: that changes where the prompt goes.

Automatic routing ("easy turns local, hard ones remote") waits for evidence
from tagged eval runs about which requests the local model fails.

## Testing

* Redactor: unit tests per pattern, plus near-misses that must not match.
* Egress check: every origin against every policy, and propagation.
* Eval: a recording "remote" provider stores what it was sent; a new
  expectation `sent_excludes` checks it. Example: read `{sandbox}/.env`,
  then ask a remote question; the key must not appear in what was sent.
* `jarvis eval --model <remote>` against the local baselines.

## Build order

1. Redactor (`core/src/egress/redact.rs`). **Done.**
2. Origins, propagation and the policy check (`core/src/egress/mod.rs`).
   **Done.**
3. **Done.** Labels travel inside `ChatMessage` (never serialized) and are
   stored with each message; recall carries them. `model.locality` and
   `model.data_policy` (a misspelt value stops startup; `jarvis status`
   shows the notice). Every model request goes through
   `Orchestrator::ask_model`: check, ask, redact, send, restore. The footer
   is returned beside the answer and shown by CLI and TUI.
4. **Done.** Two tiers: `[model]` (local) and `[remote]`, switched with
   `jarvis tier local|remote` or `/local` `/remote`, daemon-wide until
   switched back. The prompt offers `[y] send once`, `[a] always allow
   <folders>` (files only) and `[n]`; the protocol carries choices
   backwards-compatibly (an old client's yes is the first choice). "Always"
   writes `egress-allow.toml` in the data directory; `jarvis egress
   allow|remove|list` manage it. When a request is not sent and `[model]` is
   local, the owner's rule applies: ask each time whether the local model
   should answer (`egress.local`). The footer names the model that answered.
5. **Done.** `remote = true` eval cases with `sent_contains` /
   `sent_excludes` (`evals/cases/egress.toml`), graded with or without a
   model.

## Known gaps

* **Web search queries leave the machine on any tier.** The search sidecar
  sends the query to its backend, and this check covers model requests
  only, so a query is neither checked nor redacted. It needs its own rule
  (at least the redactor) at the MCP call.
* **Names and free-text personal details** are not redacted (see
  Redaction).
* **A redacted remote answer is not streamed**: a placeholder split across
  chunks would be shown before it is restored. A small look-behind buffer
  would allow streaming again.
* **Symlinks** are not resolved when matching paths: a link inside an
  allowed folder that points into `~/.ssh` is judged by the link's path.
