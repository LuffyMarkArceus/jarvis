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
