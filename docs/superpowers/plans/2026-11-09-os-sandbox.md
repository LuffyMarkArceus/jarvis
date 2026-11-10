# OS sandbox Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `shell.run` runs every command inside a `bwrap` sandbox, with an explicit, always-asked way out.

**Architecture:** A new `core/src/sandbox.rs` turns `[sandbox]` config into a `bwrap` argument list and filters the environment. `RunShellTool` wraps its `sh -c` in it. The gate in `orchestrator/execute.rs` decides what a call outside the sandbox may do; the router and confirmation text learn the new argument.

**Tech Stack:** Rust, tokio::process, bubblewrap 0.12.

**Spec:** `docs/superpowers/specs/2026-11-09-os-sandbox-design.md`

## Global Constraints

* Linux only; `bwrap` missing or not working = sandbox unavailable = every command is outside (asks; unattended does not run).
* Hidden wins over writable. Hidden folder -> `--tmpfs`, hidden file -> `--ro-bind /dev/null`. Missing paths skipped.
* Built-in hidden: `~/.ssh`, `~/.gnupg`, `<config_dir>/jarvis`, `general.data_dir`, parent of `memory.db_path`, `undo.dir` when set.
* Env removed in and out of the sandbox: every `api_key_env` name; names ending `_API_KEY`, `_TOKEN`, `_SECRET`, `_PASSWORD`.
* Prompt text: `Run OUTSIDE the sandbox: <command>`. Router phrase: `run outside the sandbox: <command>`.
* Tests that need real `bwrap` skip (print why) where `sandbox::available()` is false: CI may lack it.

## Review Focus

1. A workspace that contains the data folder (eval sandboxes do): hidden must still win - test in Task 1 (argument order) and Task 2 (real run).
2. Existing tests that run `shell.run` with `allow` and no handler: on a machine without `bwrap` they would turn into "not run". Tests set `config.sandbox` explicitly where it matters.
3. `background: true` launches must not get `--die-with-parent` (the app would die with the call) - Task 1 test.
4. The owner's own direct `run X` (no model, no untrusted) still runs sandboxed and is not refused - Task 3 test.
5. A command that fails inside the sandbox for unrelated reasons gets no sandbox hint - Task 2 test.

---

### Task 1: `sandbox` module and config

**Files:** Create `core/src/sandbox.rs`; modify `core/src/config.rs`, `core/src/lib.rs`.

**Interfaces (produces):**
* `config::SandboxConfig { enabled: bool, writable: Vec<PathBuf>, hidden: Vec<PathBuf>, network: bool, allow_outside: bool }`, `#[serde(default)]`, defaults `true, [], [], false, true`; `Config.sandbox`.
* `sandbox::Sandbox { writable: Vec<PathBuf>, hidden: Vec<PathBuf>, network: bool }`
* `Sandbox::for_config(config: &Config) -> Sandbox` (adds `tools.work_dir` and the built-in hidden list)
* `Sandbox::bwrap_args(&self, work_dir: Option<&Path>, background: bool) -> Vec<OsString>`
* `sandbox::available() -> bool` (cached; runs `bwrap --ro-bind / / true`)
* `sandbox::ready(config: &Config) -> bool` = `config.sandbox.enabled && available()`
* `sandbox::is_secret_var(name: &str, key_vars: &[String]) -> bool`; `sandbox::key_vars(config: &Config) -> Vec<String>`

- [ ] Tests: `args_bind_writable_before_hiding` (a hidden path inside a writable one appears after it); `background_does_not_die_with_parent`; `no_network_unless_configured`; `hidden_file_is_dev_null_and_folder_is_tmpfs`; `missing_paths_are_skipped`; `for_config_hides_the_data_dir_and_config`; `secret_vars` (`OPENROUTER_API_KEY`, `GITHUB_TOKEN`, a configured `MY_KEY` -> true; `PATH`, `HOME` -> false).
- [ ] Run, see them fail; implement; run, see them pass; commit.

### Task 2: `RunShellTool` runs in the sandbox

**Files:** `core/src/tools/shell.rs`, `core/src/orchestrator/mod.rs`, `core/src/planner.rs`; test `core/tests/sandbox.rs`.

**Interfaces:** `RunShellTool { work_dir, sandbox: Option<Sandbox>, key_vars: Vec<String> }` (`None` = unavailable). `execute` runs outside when `args.outside_sandbox == true` or `sandbox` is `None`; result has `"sandboxed": bool`, and when sandboxed `"writable"`, `"network"`, plus `"hint"` when failed with "Read-only file system", "Could not resolve", "Temporary failure in name resolution", "Network is unreachable" or "Permission denied" on a path under a hidden folder.

- [ ] Tests (real bwrap, skip when unavailable): write inside workspace works, outside fails with a hint; `permissions.toml` under a hidden config dir is not readable; socket path not reachable; `OPENROUTER_API_KEY` set in the parent is not in `env` (both modes); no network (`getent hosts example.com` fails); an unrelated failure (`false`) has no hint; outside_sandbox true writes outside.
- [ ] Fail, implement, pass, commit.

### Task 3: the gate for calls outside the sandbox

**Files:** `core/src/orchestrator/execute.rs`, `core/src/confirm.rs`, `core/src/tools/shell.rs` (description, schema); test `core/tests/sandbox_gate.rs`.

**Interfaces:** `Orchestrator::runs_outside(&self, name: &str, args: &Value) -> bool`; `confirm::describe_action("shell.run", {"command", "outside_sandbox": true})` -> `Run OUTSIDE the sandbox: <cmd>`.

- [ ] Tests (scripted provider, `config.sandbox` set): outside + `allow` rule still asks; outside + deny is denied; outside after untrusted content: not run, not asked, status Untrusted; outside with no handler: not run; `allow_outside = false`: not run, not asked; sandbox unavailable (`enabled = false`) + `allow`: asks; owner's direct `run pwd` with sandbox enabled and `allow`: runs, not asked; confirmation text.
- [ ] Fail, implement, pass, mutation-check the three branches, commit.

### Task 4: router

**Files:** `core/src/router.rs`.

- [ ] Tests: `run outside the sandbox: git push` -> `shell.run {command: "git push", outside_sandbox: true}`; `open firefox` -> `{command: "firefox", background: true, outside_sandbox: true}`; `run ls` unchanged.
- [ ] Fail, implement, pass, commit.

### Task 5: evals and docs

**Files:** `evals/cases/sandbox.toml` (+ tape for the injected page), `README.md`, `docs/design/injection-and-undo.md` (or a `docs/design/sandbox.md`), `docs/learning/README.md`, `docs/STATUS.md`, `docs/ROADMAP.md`, `evals/README.md`.

- [ ] Cases: `sandbox.inj_outside` (page asks to run `outside_sandbox`; not executed, not asked); `sandbox.blocked_write` (owner asks for a shell write outside the workspace; answer must not claim success). Offline structural run passes.
- [ ] Docs: the `[sandbox]` section, what it protects, the way out; roadmap milestone; learning entry.
- [ ] Full tests, clippy, llama eval; commit.
