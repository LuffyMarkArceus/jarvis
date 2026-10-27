# Evals

Unit tests check that each piece does what its author meant. The eval suite
checks whether Jarvis answers real requests correctly with a given model, and
whether a change made answers better or worse.

```sh
jarvis eval                      # every case, 3 attempts each, configured model
jarvis eval --model qwen2.5:3b   # same suite, another model
jarvis eval --filter web         # cases whose id contains "web" or tagged "web"
jarvis eval --offline            # scripted model: grade only what code decides
jarvis eval --record             # re-record web tapes from the real servers
jarvis eval diff                 # what changed since the previous run of this model
```

Run it from the repository root. Each attempt prints PASS or FAIL with the
failing checks. The summary gives two numbers:

* **mean pass rate**: the average over cases of attempts passed. Use it to
  compare models.
* **cases passing every attempt**: stricter. A case that passes 2 of 3 times
  is not reliable yet.

Every attempt is also written as one JSON line to `evals/results/`, with the
model, git SHA, route, tool calls, latency and tokens. Results are not
committed.

## Two models: one for development, one for verification

* **While developing**, run the suite on `llama3.2:1b` and compare with
  `jarvis eval diff`. Its score tops out around 85% for reasons that are the
  model's, so the absolute number says little; a case that *changed* is what
  a code change caused. `diff` exits 1 when any case got worse.
* **When a feature or phase is complete**, run the full suite on
  `qwen2.5:3b` (`--model qwen2.5:3b`) and add a line to
  [BASELINES.md](BASELINES.md).

`jarvis eval diff` with no arguments compares the newest run with the one
before it that used the same model and grading. Either run can be named by
a path or by part of its id (`jarvis eval diff ddffd5e qwen`). Pass rates and
latency are computed over the cases both runs contain, so a run filtered with
`--filter` is still compared fairly. A case that moved by a single attempt is
marked as possible noise: with three attempts that happens by chance.

## How an attempt runs

* Fixture files are created in a fresh temporary directory, `{sandbox}`.
* Memory, the audit log and the permission policy live in that directory, so
  your own history and policy do not change a result and nothing is left
  behind.
* The policy is `evals/permissions.toml`, the same on every machine.
* Permission prompts are answered by a scripted user: no, unless the case
  sets `approve = true`. An approved tool really runs, so approved cases must
  be harmless.
* Web cases (`web = true`) get `search` and `fetch` replayed from
  `evals/tapes/<case id>/` by `tape_mcp.py`. A call that is not on the tape
  fails as a tool error rather than going to the network.

CI runs the whole suite with `--offline` grading (`core/tests/eval_suite.rs`).
That checks routes, deterministic tool calls, prompts and files. It cannot
check the model's answers, so run the real suite before and after any change
to routing, planning, prompts or the model.

## Writing a case

Cases live in `evals/cases/*.toml`:

```toml
[[case]]
id = "fs.read"                          # area.what; results are tracked by it
input = "read {sandbox}/notes.txt"
tags = ["filesystem"]
note = "Why this case exists."          # shown to whoever reads a failure
files = { "notes.txt" = "buy oat milk" } # created in the sandbox first
setup = ["my name is Ada"]              # earlier turns, not graded
approve = false                          # answer to permission prompts
web = false                              # replay search/fetch from tapes

[case.expect]                            # every field is optional
route = "Tool(filesystem.read)"          # Direct | Model | Tool(name)
tools = ["filesystem.read"]              # exact ordered calls; [] = none
tools_include = ["filesystem.read"]      # must be among the calls
tools_exclude = ["shell.run"]            # must not be called
args = { "filesystem.read" = { path = "{sandbox}/notes.txt" } }
asked = []                               # exact ordered permission prompts
contains = ["oat milk"]                  # all, case-insensitive
contains_any = ["4", "four"]             # at least one
excludes = ["I created"]                 # none
files = { "out.txt" = "hello" }          # sandbox after; any other change fails
```

Unknown keys are an error, so a misspelt expectation cannot pass by being
ignored.

**Check for the right answer, not only a plausible one.** The first web cases
passed with `excludes = ["could not retrieve a source"]` while the model
invented a Postgres release history and answered a Rust question about a film.
A case is only as strict as its expectations: when the answer has a correct
value, require it.

A web case needs its tapes. Add the case, then record it once:

```sh
jarvis eval --record --filter web.my_case
```

Read the recorded tape before committing it. It is the ground truth the case
is graded against.
