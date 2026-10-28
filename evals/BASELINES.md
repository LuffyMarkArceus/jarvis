# Baselines

One line per reference run of the full suite (`jarvis eval --repeat 3`).
Add a line when the model, the suite or the scoring changes, so a score can
be read against the one before it.

| Date | Commit | Model | Cases | Mean pass rate | Pass every attempt | p50 / p95 latency |
| --- | --- | --- | --- | --- | --- | --- |
| 2026-10-26 | 20dbd01 + Phase 5 | llama3.2:1b (Ollama) | 20 x 3 | 80.0% | 16/20 | 11.0 s / 58.3 s |
| 2026-10-27 | ddffd5e | llama3.2:1b (Ollama, GPU) | 20 x 3 | 85.0% | 17/20 | 6.7 s / 43.9 s |
| 2026-10-27 | ddffd5e | qwen2.5:3b (Ollama, GPU) | 20 x 3 | 95.0% | 19/20 | 6.9 s / 62.8 s |
| 2026-10-27 | 4c69a2b | llama3.2:1b (Ollama, GPU) | 41 x 3 | 74.0% | 30/41 | 4.1 s / 51.4 s |
| 2026-10-27 | 319613c | qwen2.5:3b (Ollama, GPU) | 41 x 3 | 90.2% | 37/41 | 16.3 s* / 57.2 s* |
| 2026-10-28 | e231cdc | nemotron-3-ultra-550b-a55b:free (OpenRouter) | 46 x 1 | 93.5% | 43/46 | 3.3 s / 18.1 s |
| 2026-10-28 | e231cdc+docs | laguna-xs-2.1:free (OpenRouter) | 34 of 46 answered | 97.1% | 33/34 | 2.6 s / 9.7 s |
| 2026-10-28 | e82b25b | qwen2.5:3b (Ollama, GPU) | 46 x 3 | 91.3% | 42/46 | 10.4 s / 48.4 s |

## 2026-10-26, llama3.2:1b

The run before `chat.greeting` was relaxed; that case failed 0/3 on a wording
check, not on a wrong answer, so the comparable score is 85.0% and 17/20.

Failed every attempt:

* `fs.write.approved`: with write approved, the model sent its arguments
  wrapped in a JSON-schema shape (`{"properties": {...}, "type": "object"}`),
  wrote to the directory path, then "edited" the word away. The schema
  wrapping is something `sanitize_args` could unwrap; the rest is the model.
* `web.current_version`: the taped page says 18.6; the model invented a
  release history. The grounding check flagged every made-up figure, but the
  answer is still wrong.
* `web.latest_news`: search ranked the Rust film shooting first, and the
  answer followed it. A search-quality problem as much as a model one.

Deterministic routes (filesystem, shell, system, fetch-a-URL) passed every
attempt, as expected: the model is not involved in them.

## 2026-10-27, llama3.2:1b vs qwen2.5:3b at ddffd5e

Same commit, so the difference is the model. Both ran 100% on a GeForce 940MX
(4 GB); qwen2.5:3b takes 2.4 GB at 8k context. The 26th's 11.0 s p50 was
measured under different load; compare latency only within one day.

* `fs.write.approved`: the `properties` unwrap (ddffd5e) works - llama3.2:1b's
  arguments now reach the tool flat - but it still fails 0/3, writing to the
  sandbox directory instead of `notes.md` inside it. qwen2.5:3b passes 3/3.
* `web.current_version`: llama3.2:1b still invents versions; qwen2.5:3b
  answers 18.6 from the page, 3/3.
* `web.latest_news`: both fail. The search returns the Rust shooting article
  and qwen2.5:3b summarises it faithfully, so the remaining failure is search
  ranking, not the model.

## 2026-10-27, 41 cases: Phase 5a verification on qwen2.5:3b

The suite grew from 20 to 41 cases. On the 20 shared cases qwen2.5:3b is
unchanged (95.0%), so the router and renderer changes regressed nothing.

\* Latency is not comparable: a build ran during part of this run. Pass
rates are unaffected.

qwen2.5:3b passes every routing, safety, permission, honesty, shell, system
and process case. It fails four, none of them code:

* `memory.correction`: "what should you call me?" answered as "what should I
  call you" ("You can call me Jarvis"). Model.
* `memory.followup_file`: "buy oat milk" is "12 words" - the character count
  (llama3.2:1b says 2). Models count tokens, not words; counting belongs in a
  tool, not the model.
* `web.latest_news`, `web.nobel_physics`: retrieval reads only the top search
  hit, which is the wrong page for both. For Nobel, qwen2.5:3b honestly says
  the page does not cover 2024 but concludes the prize was "not awarded",
  which is false.

llama3.2:1b at 4c69a2b additionally fails translation, unit conversion, the
two approved edits/writes (path handling) and Postgres grounding; the
`system.ram` failure in that run was fixed in 319613c.

## 2026-10-28, free OpenRouter models (`jarvis eval --remote`)

One attempt per case (free models are rate-limited per day); $0 spent.
The sandbox is allowed to go remote for ordinary cases, so these grade the
model; the `egress.*` cases grade the privacy rules and pass on both.

* Before e231cdc the scores were 87% and 56.5% with **no wrong answer**:
  every failure was the provider not answering, and nine were HTTP 400s from
  our dotted tool names (fixed in e231cdc). Read "unavailable" counts before
  comparing scores.
* nemotron-3-ultra (served by Nvidia, not the shared free pool) answered
  every request. It fails only the three web cases, and not for the reason
  small models do: it searched again with better queries, which are not on
  the tapes (recorded with llama3.2:1b), so they failed, and four
  iterations ran out. The tapes encode the small model's path; re-recording
  them with a stronger model is needed before web cases can grade one.
* laguna-xs-2.1 got no answer for 12 of 46 cases even with retries (HTTP
  429 from the shared free pool). Of the 34 it answered, 33 passed.

## 2026-10-28, Phase 5b verification on qwen2.5:3b at e82b25b

The privacy work (labels, redaction, allow-lists, tiers, footer) against
the 5a baseline, 319613c:

* The 41 shared cases are unchanged: 90.2%, the same four failures
  (`memory.correction`, `memory.followup_file`, `web.latest_news`,
  `web.nobel_physics`). Nothing in 5b moved an answer.
* The five new `egress.*` cases pass 3/3, as do every privacy, permission
  and safety case.
* p50 latency of model turns fell from 14.5 s to 12.7 s on shared cases.
