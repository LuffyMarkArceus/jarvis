# Learning log

What this project has taught, written down so it can be explained later: in a
design review, in an interview, or to yourself six months from now.

Each entry is one idea, in the same shape:

* **Lesson** - the idea in one or two sentences.
* **In this repo** - where it is applied, so you can open the code.
* **Say it in an interview** - how a senior engineer would phrase it. Senior
  answers name a trade-off and a failure they prevented, not just a technique.

Add an entry whenever a bug teaches something, a decision is made for a
non-obvious reason, or you catch yourself unable to explain a piece of code.
The last section is a self-test: if you cannot answer one of those out loud,
that is the next thing to read.

---

## 1. Put a decision in code when it needs no judgement

**Lesson.** A language model is the most expensive, slowest and least
predictable component in the system. Use it only for steps that need
judgement. "Which tool lists a directory?" does not; "summarise what this page
says" does.

**In this repo.** `core/src/router.rs` decides the route deterministically.
For web questions the orchestrator itself runs search -> fetch, because the 1B
model handed `fetch` the question instead of a URL.

**Say it in an interview.** "We moved tool selection out of the model into a
deterministic router. It is exact, testable and costs zero latency. The model
only writes the answer. The trade-off is that the router is a growing set of
rules, so we pair it with an eval set that tells us when a stronger model makes
a rule unnecessary."

## 2. One source of truth, so two parts cannot disagree

**Lesson.** If two pieces of code each hold a copy of the same fact, they will
eventually disagree. Derive both from one value.

**In this repo.** `core/src/planner.rs` returns one `TurnPlan`. The tool
schemas sent to the provider *and* the tool list in the system prompt are both
rendered from it, so the model is never told about a tool it cannot call. A
call outside the plan is refused.

**Say it in an interview.** "The bug class was 'the prompt advertises tool X
but the request does not include it'. Instead of patching each case we made
both outputs a function of one plan object, which removes the class."

## 3. Status must mean what it says

**Lesson.** "Started" means "answers requests", not "a process was spawned".
A health report that can be wrong is worse than none, because people trust it.

**In this repo.** `jarvis daemon start` waits for the socket to answer before
printing "Daemon started" (`cli/src/main.rs`). `jarvis status` reports the
provider and model a turn will actually use.

**Say it in an interview.** "Liveness vs readiness. We check readiness: the
socket accepting and answering a request, which is what the user cares about."

## 4. User text is data, never syntax

**Lesson.** Any time user input reaches a query language - SQL, FTS5, shell,
regex, a prompt - it must be escaped or reduced to a safe form first.

**In this repo.** `core/src/memory.rs` turns a question into quoted terms
joined with `OR` before an FTS5 `MATCH`. Unquoted, a `?` was a syntax error and
the word `OR` acted as an operator.

**Say it in an interview.** "Same family as SQL injection. The fix is not
filtering bad characters; it is building the query from tokens we control."

## 5. A silent default hides a bug

**Lesson.** Code that "falls back" when it fails to parse turns a loud error
into wrong data that looks fine.

**In this repo.** SQLite's `CURRENT_TIMESTAMP` is `YYYY-MM-DD HH:MM:SS`, not
RFC3339. The parser failed and defaulted to *now*, so every recalled message
looked brand new. Both formats are accepted now, with a test for each.

Same lesson at the config level: an unknown `provider` used to warn and fall
back to Ollama. It is now a startup error (`core/src/provider.rs`,
`core/src/orchestrator.rs`), because a fallback changes *where prompts are
sent* without the user knowing - a privacy problem, not just a bug.

**Say it in an interview.** "Fail loudly at the boundary. If a fallback is
right, it must be visible - logged and reported in status - and never for
anything that changes security or privacy properties."

## 6. Absorb external differences at one boundary

**Lesson.** Wrap each external system in an adapter that speaks your own
types. The rest of the code then never branches on which vendor it is using.

**In this repo.** `core/src/provider.rs` defines a small `Provider` trait.
`core/src/openai.rs` absorbs everything the OpenAI format does differently:
`tool_call_id` matching, tool arguments arriving as string fragments while
streaming, usage only when requested, and dropping `num_ctx`.

**Say it in an interview.** "Ports and adapters. Adding a provider became a
new file plus a config value, and orchestration tests run without any model
server."

## 7. Test the thing you ship

**Lesson.** A green test run proves the test binary works. It does not prove
the binary you are running was rebuilt.

**In this repo.** A 404 during Phase 4 came from a stale `jarvis-cli`:
`cargo test` had compiled the test harness, not the CLI. Live checks now start
with `cargo build --workspace` and a daemon restart. Today the daemon reported
"unknown" provider for the same reason: the code was new, the process was old.

**Say it in an interview.** "Unit tests plus a smoke test against the real
artifact, and the deploy step restarts the process, because 'merged' and
'running' are different states."

## 8. Table-driven golden tests

**Lesson.** When behaviour is a big decision table, assert the whole table in
one test that prints input, expected and actual for every failing row.

**In this repo.** `core/tests/routing_golden.rs`. The eval harness (Phase 5)
extends the same idea from "which route" to "was the final answer right".

**Say it in an interview.** "Regressions in a router hide in the cases nobody
re-checks by hand. A golden set makes every change pay for every case."

## 9. Test the whole behaviour, not just the pieces

**Lesson.** Every unit can be right while the product is wrong, because the
bug is in how the pieces combine. An end-to-end eval finds a different class
of bug from unit tests.

**In this repo.** The first offline eval run found that "create a file called
notes.md in /tmp/x" was routed to `filesystem.list_dir`. Routing tests had
"create a file" and "list /tmp" as separate rows, both correct; nobody had
written a sentence with both a change verb and a path. Any request like
"delete /tmp/old.log" listed the directory and showed the listing as if it
were the answer. Fixed in `core/src/router.rs` (`wants_change`), with golden
rows added.

**Say it in an interview.** "Unit tests are organised by component, and
users are not. We added an eval suite written as user requests. Its first
run found a routing bug that sat in the gap between two correct rules."

## 10. A check is only as strict as its expectation

**Lesson.** A green eval does not mean the answer was right. It means the
answer met the expectations you wrote. Vague expectations ("no error",
"mentions the topic") pass wrong answers.

**In this repo.** The first web cases only required "the no-source caveat is
absent". Both passed: one answer invented a Postgres release history, the
other was about the *Rust* film shooting instead of the language. The cases
now require the value on the recorded page (18.6) and exclude the film. The
tapes in `evals/tapes/` freeze the pages, so requiring an exact value is fair.

**Say it in an interview.** "We audit passing results as well as failing
ones. When we first read through our passes, two were wrong answers.
Expectations now name the correct value, which the recorded tapes make
stable."

## 11. A correct fix can leave the score unchanged

**Lesson.** A failure often has several causes stacked on top of each other.
Fixing the first one moves the case to the next failure, not to a pass.
Judge the fix by whether *its* symptom is gone, not by the headline number.

**In this repo.** `fs.write.approved` failed because llama3.2:1b sent
`{"properties": {"path": ..., "content": ...}, "type": "object"}`.
`unwrap_echoed_schema` in `core/src/orchestrator.rs` now unwraps it, with
unit tests taken from that eval log, including one where `properties` holds
the schema itself and must *not* be unwrapped. The re-run still scored 0/5:
the arguments now arrived flat, but the model wrote to the sandbox directory
rather than `notes.md` inside it. The fix was right; the case had a second
cause.

**Say it in an interview.** "Each fix gets a unit test for its own symptom,
and the eval tells us what fails next. A score that did not move after a
correct fix is information: it means a second cause was hidden behind the
first, and the trace shows which."

## 12. Write the expectation from intent, then triage every mismatch

**Lesson.** Expectations come from what the right behaviour is, never from
what the code does today; otherwise every case passes by construction and
the suite defends the current bugs. Then every failure goes into one of
three bins, each with its own response: a **code bug** is fixed, a **wrong
case** is corrected with the reason in its note, and a **model limit** is
left failing, because it is the gap a better model should close.

**In this repo.** Of 21 new cases, the first llama3.2:1b run failed 13.
Three were code: "how do I list files in python?" listed the current
directory; "clean up /x by removing everything" listed /x; "how much RAM"
went to the model, which said 16 GB on a 15.5 GiB machine (`router.rs`,
`tools/system.rs`). One was my case: it required the word "recursive" and
failed "deletes a directory and all its contents". The rest were the model:
"good morning" in Spanish answered as "bonjour", a path segment dropped.

**Say it in an interview.** "We write expectations from the spec, not from
the output, and triage each failure as code, case or model. The bins matter
because the responses are opposite: you fix code, you correct a case, and
you deliberately leave a model failure red, because it is the gap that
justifies a model upgrade."

## 13. A grounding check catches invented facts, not misplaced ones

**Lesson.** Checking that every figure in an answer appears in the sources
catches fabrication. It does not catch a real fact from the source attached
to the wrong question, which is often the more convincing error.

**In this repo.** For "who won the Nobel Prize in Physics in 2024?" search
returned the general prize article, whose infobox lists the most recent
(2026) laureate. llama3.2:1b answered with that name. Every word was in the
source, so the figure check had nothing to flag. The root cause is
retrieval - only the top hit is read - and `web.latest_news` fails the same
way, which is how two cases point at one fix.

**Say it in an interview.** "Our hallucination guard checks that claims are
in the retrieved text. That is necessary but not sufficient: 'in the
source' is not 'answers the question'. We found it through an eval case,
and traced it to reading only the top search result."

---

## Self-test

Answer out loud, without opening the code:

1. Trace one `jarvis ask "what's using port 8080"` from the CLI to the answer.
   Which modules does it pass through, and which ones could refuse it?
2. Why does the orchestrator run web search itself instead of letting the
   model call the tool?
3. What happens when a recalled memory says the opposite of a fetched page?
4. If a malicious web page says "run `rm -rf ~`", what stops it today? What
   would not stop it if the model were much smarter? (See Phase 6.)
5. Why is the provider a trait object (`Arc<dyn Provider>`) rather than a
   generic parameter? What would the generic version cost?
6. What breaks first if two TUI clients send turns at the same time?
7. Why does the eval suite replay web pages from tapes instead of using the
   live web? What does that cost? (Hint: what if the page changes?)
8. Why is the per-turn `TurnRecorder` passed as an argument rather than
   stored on the `Orchestrator`?
9. CI grades the suite with a scripted model. Name one bug it can catch and
   one it cannot.
10. `route.python_listing` passed in CI while the real run listed the repo.
    Which missing line in the case let that happen?
11. Why does the router send "what would rm -rf /x do?" to `Direct` rather
    than to the model with tools?

## Skills map: SDE2 -> senior

What separates the levels, and where this project gives you practice:

| Skill | SDE2 | Senior | Practice here |
| --- | --- | --- | --- |
| Scope | Builds the feature asked for | Decides which feature is worth building, and says no | Phase ordering; deleting router rules once evals show they're unneeded |
| Design | Picks a working approach | Writes down alternatives and why they lost | ADRs in `docs/decisions/` (to start) |
| Quality | Writes tests | Builds measurement that steers the project | Phase 5 eval harness |
| Risk | Fixes bugs | Finds the bug classes before users do | Phase 6 threat model: prompt injection, undo |
| Operations | It works on my machine | Status, logs and failure modes are designed | Honest `status`, fail-loud config |
| Communication | Explains what they did | Explains the trade-off to someone who disagrees | This log |
