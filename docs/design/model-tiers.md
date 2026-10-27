# Design draft: local and hosted model tiers (Phase 5b)

Status: **draft for review**. Nothing here is built. The open questions at
the end are decisions for the owner, not the implementer.

## Problem

Jarvis runs on a local model by default. On this laptop the best one that
fits (qwen2.5:3b) passes 95% of today's eval suite, but the suite is easy and
small models fail at multi-step work. Some users will want a hosted frontier
model for harder requests. Others never will. Both must be first-class.

The provider part is already done: any OpenAI-compatible server works with a
URL, a model name and an API key variable (`core/src/openai.rs`). What is
missing is the **policy**: which turn may go to which model, and what context
may leave the machine with it.

## Goals

1. Local is the default and is never blocked. Hosted is opt-in, per user.
2. The user can see the trade-off as a number, not a disclaimer: the eval
   suite runs against both and reports the gap.
3. Nothing sensitive leaves the machine without an explicit rule allowing
   it, and the user is told when a rule stopped something.
4. A violation is never silent: no quiet fallback, no quiet redaction
   (learning log #5).

Non-goals for now: automatic per-request routing by difficulty, spending
caps across several providers, and remote machines (Phase 10).

## The central idea: every piece of context knows where it came from

A turn's prompt is assembled from several sources. Tag each message part
with its origin:

| Origin | Example | Sensitivity |
| --- | --- | --- |
| `user` | what the user typed | the user chose to say it |
| `memory` | recalled earlier messages | may repeat anything ever said |
| `file` | `filesystem.read` output | anything on disk |
| `shell` | `shell.run` output | environment, paths, secrets in output |
| `system` | `process.list`, `system.info` | machine fingerprint |
| `web` | `fetch` and `search` output | public, but **untrusted** |

One **egress check** sits in the single place where a request is handed to a
provider. A local provider accepts everything. A hosted provider accepts the
origins the policy allows. Proposed default:

```toml
[egress.remote]
allow = ["user", "web"]            # everything else stays local
never_paths = ["~/.ssh", "~/.gnupg", "~/.config/jarvis", "**/.env"]
on_violation = "ask"               # ask | local | refuse
```

`never_paths` is absolute: no rule or prompt overrides it. The same tags are
what Phase 6 needs for prompt-injection safety ("this text came from `web`,
so it must not trigger a `shell` call without asking"). One mechanism serves
privacy and security, so it should be designed once.

## Choosing the tier

Start with the simplest rule that the user controls:

* `model.tier = "local"` (default) or `"remote"` in config.
* `/local` and `/remote` switch the current session in the TUI.
* If the remote tier is unreachable, the turn fails visibly with a one-key
  "retry locally". It does not fall back silently: that changes where the
  prompt goes (learning log #5).

Automatic routing ("easy turns local, hard ones remote") is deliberately
later. It needs evidence about which requests the local model fails, and the
eval suite with tags is how that evidence is collected.

## How it is tested

* Unit tests for the egress check: every origin against every policy.
* A new eval expectation, `remote_excludes`, checked against what a
  recording provider was *sent*: for example, a case that reads
  `{sandbox}/.env` and then asks a remote-tier question must not send the
  file's contents.
* `jarvis eval --model <remote>` gives the score gap against the local
  baselines in `evals/BASELINES.md`.

## Open questions for the owner

1. **Default allow-list.** Is `["user", "web"]` right? Should `memory` be
   allowed, given it may repeat anything said in an earlier session?
2. **File contents.** Never remote, or remote for paths the user allows (a
   project folder)? A per-path allow-list is more useful and more to get
   wrong.
3. **On violation:** ask each time, answer locally, or refuse? Asking is the
   most transparent but trains people to click yes.
4. **Which hosted provider.** Free tiers often log or train on prompts, which
   conflicts with goal 3. Is a paid API key acceptable, or is the hosted tier
   only for providers with a no-training policy?
5. **Visibility.** Should every remote turn be marked in the answer (for
   example a small "sent to <provider>" line), or only in the audit log?
