# Baselines

One line per reference run of the full suite (`jarvis eval --repeat 3`).
Add a line when the model, the suite or the scoring changes, so a score can
be read against the one before it.

| Date | Commit | Model | Cases | Mean pass rate | Pass every attempt | p50 / p95 latency |
| --- | --- | --- | --- | --- | --- | --- |
| 2026-10-26 | 20dbd01 + Phase 5 | llama3.2:1b (Ollama) | 20 x 3 | 80.0% | 16/20 | 11.0 s / 58.3 s |

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
