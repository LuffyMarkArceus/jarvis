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

## 14. Provenance is stored, not inferred later

**Lesson.** Where a piece of data came from must be recorded when it enters
the system. Reconstructing it later from the data's shape fails as soon as
the data is copied, summarised or saved somewhere else.

**In this repo.** The egress check (`core/src/egress/mod.rs`) labels each
message by origin so file contents need an allow-list to go to a remote
model. But a deterministic `read notes.txt` is saved to history as an
*assistant* message containing the file. On the next turn it looks like
ordinary assistant text, and memory recall would send it as `memory`, which
the owner allowed. The label is therefore persisted with each message
(`label()` accepts stored labels), and the remote tier stays off until it is.

**Say it in an interview.** "Data-flow policies fail at copies. We label
data at ingestion and persist the label with it, because the moment content
is quoted into a reply or saved to history, its origin can no longer be
read off the message. This is the same idea as taint tracking for prompt
injection, so one mechanism serves privacy and security."

## 15. A test that passed first time has not been tested yet

**Lesson.** A new test that passes at once proves only that it does not
fail. Before trusting it, break the code it protects and watch it fail.
This is mutation testing done by hand, and it matters most for protective
code, where the bug you fear is "the protection silently does nothing".

**In this repo.** The redactor's nine tests passed first time; a harder
test then found that redacting twice turned `<SECRET_1>` into `<SECRET_3>`.
For the egress gateway, the check was switched off (`if false && ...`): the
five protection tests failed and the three allowed-behaviour tests still
passed - which shows each test measures what its name says. The same
mutation run against the eval cases showed that `asked` was not graded
without a model, which no passing run could have revealed.

**Say it in an interview.** "For security-relevant code we break the
protection on purpose and check the suite goes red. Green-first tests often
assert something incidental. It also told us which checks our CI mode
silently skipped."

## 16. Metadata travels beside content, not inside it

**Lesson.** Information *about* a message - who produced it, what it cost,
where it came from - must not be stored as part of the message. Mixed into
the content, it is re-read as content later.

**In this repo.** The footer (`qwen2.5:3b · local · 1.2k tokens`) is
returned as its own field, never appended to the answer: in history it
would be sent back to the model and imitated. The CLI writes it to stderr
when stdout is piped, so scripts get only the answer. The egress label
lives in `ChatMessage` as a field that is never serialized, for the same
reason in the other direction.

**Say it in an interview.** "Content and metadata get separate channels -
separate fields in the protocol, stdout versus stderr in the CLI - because
anything stored inside the content will eventually be treated as content."

## 17. Change a protocol so the old reading stays correct

**Lesson.** When a message gains a field, an old client that ignores it
must still behave safely. Make new fields optional, and define what their
absence means so it maps onto the old behaviour. Then ask what an old
client's answer would *mean* under the new rules.

**In this repo.** The confirmation prompt gained choices (`[y] send once`,
`[a] always allow <folder>`). A client that knows only yes/no replies
`approved: true` with no choice; `ConfirmationReply::selected` reads that as
the *first* choice. So the first choice must always be the narrowest yes -
"send once", never "always" - or an old client would grant more than its
user saw. A key that was not offered approves nothing.

**Say it in an interview.** "We version by additive, optional fields and
pin down the default meaning of an absent field. The design rule that fell
out was that the default yes must be the least permissive option, because
that is what every client that predates the change will send."

## 18. Measure availability apart from quality - and keep the bucket narrow

**Lesson.** An eval that counts "the server did not answer" as a wrong
answer measures the infrastructure, not the model. Separate the two. But an
"excluded" bucket is dangerous: anything put in it disappears from the
score, so define it as narrowly as possible, or it will hide real bugs.

**In this repo.** Two free OpenRouter models first scored 87% and 56.5%.
Not one answer was wrong: every failure was a fallback after the provider
did not respond. Attempts without an answer are now `unavailable`, shown as
SKIP and left out of pass rates. Its first version excluded *every* model
error, and on the next run it hid nine HTTP 400s - a real bug (tool names
with dots, #19). `is_unavailable` in `core/src/eval/mod.rs` now
accepts only 429, 5xx, network errors and timeouts; a 400 is our failure.

**Say it in an interview.** "We report quality and availability as separate
numbers. The trap is the exclusion rule: we first excluded all provider
errors and that silently dropped a request-format bug, so exclusion is an
allow-list of error kinds that mean the provider did not answer."

## 19. An adapter is only as strict as the strictest server behind it

**Lesson.** A protocol implemented against one lenient server can be wrong
in ways that only a strict one reveals. Validate against the specification,
not against the server you happen to test with.

**In this repo.** Our tool names are dotted (`filesystem.edit`). Ollama's
OpenAI-compatible endpoint accepted them for weeks; OpenAI's format allows
only `[a-zA-Z0-9_-]`, and Nvidia's server rejected every request that
offered tools. The fix lives entirely in `core/src/openai.rs` (learning
#6): names are encoded on the way out and mapped back by lookup in the
tools that were offered, never by reversing the encoding.

**Say it in an interview.** "Postel's law cuts both ways: lenient servers
let a client drift from the spec. We test the adapter's output against the
spec's rules, and the first strict server found a bug a lenient one had
masked."

## 20. When one number does two jobs, the weak case sets it for everyone

**Lesson.** A limit tuned on one population can quietly encode that
population's failure mode. Before raising or lowering it, find out what it
is actually protecting against - then protect against *that* directly, and
let the limit go back to its real job.

**In this repo.** `MAX_ITERATIONS = 4` was tuned on llama3.2:1b. The eval
records showed what it really did: every 1B attempt that reached 4 rounds
was a loop (the same broken `filesystem.write` again), while every
nemotron attempt that reached 4 was research - six *distinct* searches and
fetches, cut off mid-answer. So the limit was a loop detector for one model
and a research budget for the other. Loops are now caught as loops (an
identical call is never run twice, `call_key` in
`core/src/orchestrator.rs`), and the limit is `max_rounds` per tier,
default 8. Mutation check: without the guard, the approved write ran 8
times.

**Say it in an interview.** "Before changing a threshold I look at what
hits it. Here the same cap was catching two different behaviours; we
replaced the proxy with a direct check for the bad one, so the cap no
longer has to be set by the weakest client."

## 21. A recording captures the moment, outages included

**Lesson.** Record-replay tests freeze whatever the dependency said while
recording - including that it was down. Decide what a recording is allowed
to keep, and make refreshing it a deliberate act, not a side effect.

**In this repo.** Recording nemotron's web path saved Wikipedia's `503`
answers for every follow-up search, which would have replayed as "search
failed" forever. Recording also *replaced* existing answers, silently
changing the pages the 1B's cases were graded on. `evals/tape_mcp.py` now
only adds calls that are missing, never keeps a tool error, and leaves
refreshing a stale page to whoever deletes it.

**Say it in an interview.** "Our fixtures are append-only and only store
successful responses. A transient error in a fixture turns a flaky
dependency into a permanent test failure, and an overwrite turns
re-recording into an unreviewed change to every expectation."

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
12. The owner allowed `memory` to go to a remote model and required an
    allow-list for files. How could a file still leave as "memory", and what
    stops it?
13. A text-embedded tool call's result is sent back with the `user` role.
    Why did that break role-based labelling, and where is the label set now?
14. Your search questions leave the machine even with a local model. Why
    did the egress check not see them at first, and where are they checked
    now?
15. Why is "always allow" offered for a file but never for command output?
16. Why must the first choice in a prompt be the narrowest yes?
17. A cloud model scored 56.5% with no wrong answers. How, and what did the
    first fix for it get wrong?
18. Why are tool calls mapped back to our names by lookup, not by turning
    `__` back into `.`?
19. The 1B and nemotron both hit the 4-round limit. What was different
    about *why*, and why does that make a repeat check better than a lower
    limit?
20. A model wrote a search query after reading a file. Why is that query
    checked by the turn's labels, when the user's own first search is only
    redacted?
21. Why does the tape recorder refuse to keep a tool error, even a 404?

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
