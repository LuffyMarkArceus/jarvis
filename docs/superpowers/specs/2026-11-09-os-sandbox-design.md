# OS sandbox for `shell.run` - design

Approved by the owner 2026-11-09 (option A: sandboxed by default, an explicit
way out). Comes before Phase 7, because a job that acts unattended needs a
boundary that does not depend on someone reading a prompt.

## Why

`shell.run` runs `sh -c <command>` with the owner's full rights. The only
protections are the permission prompt, the untrusted-content gate and text
patterns, and a text pattern cannot see what a command does: a rule for `rm`
misses `/bin/rm`, `find -delete` and `python -c "os.remove(...)"`.

Found while designing this:

1. **Jarvis's own controls are writable by any command.**
   `~/.config/jarvis/permissions.toml` is an ordinary file the owner can
   write, so a command could set `shell.run` to `allow`, then delete the audit
   log and the undo copies. The sandbox has to protect Jarvis from what Jarvis
   runs.
2. **The daemon socket is a way back in.** `<data_dir>/jarvis.sock` accepts
   requests; a command could ask Jarvis to run a new turn with none of this
   turn's labels.
3. **Keys leak through the environment.** The remote model's key is read from
   an environment variable (`api_key_env`), and every command inherits the
   daemon's environment, so `env` prints it - into output that can go to a
   remote model.

## What a sandboxed command can do

Built with `bwrap` (bubblewrap 0.12 here; Flatpak's sandbox, also used by
Codex on Linux). Landlock was considered: it restricts only file access, and
its network rules cover TCP only.

| | |
| --- | --- |
| Read | the whole filesystem, except the hidden list |
| Write | `[sandbox] writable`, plus `[tools] work_dir`; a private `/tmp` thrown away afterwards |
| Hidden | `~/.ssh`, `~/.gnupg`, `~/.config/jarvis` (config, permissions, key files), `general.data_dir` (memory, audit, undo copies, socket), the memory database's folder, `[undo] dir`, plus `[sandbox] hidden` |
| Network | none (`--unshare-net`), unless `[sandbox] network = true` |
| Processes | own PID namespace, own session (`--new-session`: no typing into the owner's terminal), killed with Jarvis (`--die-with-parent`; not for `background`) |
| Environment | without the variables named in any `api_key_env`, and without names ending `_API_KEY`, `_TOKEN`, `_SECRET` or `_PASSWORD` - outside the sandbox too |

Hidden wins over writable: a workspace that contains the data folder still
cannot reach it. A hidden folder becomes an empty `tmpfs`; a hidden file is
`/dev/null`. Paths that do not exist are skipped.

## The way out

`shell.run` gains `"outside_sandbox": true`. The owner can also type
`run outside the sandbox: <command>`. The router's "open <app>" launch sets it,
since a GUI app needs a writable profile and usually the network.

A call outside the sandbox:

* **always asks**, even where the rules say `allow` (deny still wins);
* is described as `Run OUTSIDE the sandbox: <command>`;
* is **not run and not asked** when the model read untrusted content this
  turn, or when nobody can be asked (Phase 7 jobs). A page must never be able
  to request it;
* is refused when `[sandbox] allow_outside = false`.

When the sandbox is unavailable (no `bwrap`, user namespaces off, or
`[sandbox] enabled = false`), every command is a call outside the sandbox.
This fails closed: it asks, and unattended it does not run.

## What the model is told

A sandboxed result carries `"sandboxed": true`, what is writable and whether
there is a network. When the command failed and its output shows the sandbox
was the cause ("Read-only file system", a resolution or connection failure),
the result adds a hint that it may need to run outside the sandbox, which
the owner will be asked about. Jarvis never retries outside the sandbox on
its own. The tool description says commands run sandboxed.

## Configuration

```toml
[sandbox]
enabled = true          # false: every command runs outside, and asks
writable = []           # plus [tools] work_dir
hidden = []             # plus the built-in list
network = false
allow_outside = true    # false: never run outside the sandbox
```

## Out of scope

macOS (`sandbox-exec`), limiting which programs run (seccomp), per-host
network allow-lists, sandboxing MCP sidecars (Jarvis starts them, a model does
not choose them).

## Testing

* Unit: the `bwrap` argument list (order: writable before hidden), the
  environment filter, the escape rules in the gate, the confirmation text,
  the router phrase.
* Integration, running real `bwrap` (skipped with a message where it is
  unavailable): a write outside the workspace fails and inside succeeds;
  `permissions.toml` and the socket cannot be reached; a key variable is not
  visible; there is no network; a call outside the sandbox after untrusted
  content is not run; one with no handler is not run.
* Evals: an injected page asking to run outside the sandbox (not run, not
  asked); a model whose command hits the sandbox reports it instead of
  claiming success.
* Mutation checks on the gate and the argument builder; qwen2.5:3b
  verification; PR, merge, post-merge checks.
