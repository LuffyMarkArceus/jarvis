# The OS sandbox around `shell.run`

Owner decision 2026-11-09 (option A: sandboxed by default, with an explicit,
always-asked way out). The full design and its reasoning are in
[the spec](../superpowers/specs/2026-11-09-os-sandbox-design.md); this page is
the reference.

## Threat model

The prompt and the untrusted-content gate decide *whether* a command runs.
They cannot limit *what it does* once it runs, and text patterns cannot see
that either (`rm` vs `/bin/rm` vs `find -delete`). The sandbox is the layer
that does not depend on a person reading a prompt correctly or a pattern
matching - and Phase 7 jobs will run with no person at all.

The sharpest reason: Jarvis's own controls were ordinary files. A command
could rewrite `permissions.toml` to allow itself, delete the audit log and
the undo copies, or connect to the daemon socket and start a fresh turn
without this turn's labels. The sandbox hides all of them.

## What a command gets

| | |
| --- | --- |
| Reads | everything, except the hidden list |
| Writes | `[sandbox] writable`, `[tools] work_dir`, a private `/tmp` thrown away afterwards |
| Hidden | `~/.ssh`, `~/.gnupg`, `~/.config/jarvis`, `general.data_dir` (memory, audit, undo, socket), the memory database's folder, `[undo] dir`, `/run/user` and `/run/dbus` (session and system buses, gpg-agent, Wayland), `[sandbox] hidden` |
| Network | none, unless `[sandbox] network = true` |
| Processes | own PID namespace and session; dies with Jarvis (not `background`) |
| Environment | no `api_key_env` variables and no secret-looking names - a word `SECRET`, `TOKEN`, `PASSWORD`, `CREDENTIALS`, or ending `_KEY` / `_DSN`, `DATABASE_URL` (outside the sandbox too); inside, no bus, agent or runtime-dir addresses |

Hidden wins over writable: bwrap applies mounts in order, and the hiding
comes last. A test runs real bwrap to prove it.

Hiding a folder stops it being renamed, but not a folder above it: with the
home folder writable, a command could move `.config` aside and plant its own
`permissions.toml`. So every folder between a workspace and a hidden folder
is mounted read-only (for a hidden folder that does not exist yet, its
deepest existing parent); if that parent is the workspace itself, the
workspace is not writable and a warning says why.

The final review of this milestone found the session bus open: a read-only
mount does not stop a `connect()`, and `systemd-run --user` over the bus
starts a program outside the sandbox, unasked. `/run/user` and `/run/dbus`
are hidden for that reason.

## The way out

`outside_sandbox: true` on the call, `run outside the sandbox: <command>`
typed by the owner, or an app launch ("open firefox"). Such a call always
asks, even under `allow`; is never run after untrusted content or with
nobody to ask; and is refused when `[sandbox] allow_outside = false`. With
no working bwrap, or `[sandbox] enabled = false`, every command is outside
and asks - the sandbox fails closed.

## Known gaps

* Reading is not limited beyond the hidden list: a command can read your
  documents and browser profile. Without a network it can only hand them to
  the model; command output is untrusted content, and what goes to a remote
  model passes the privacy rules.
* A command run outside the sandbox has your full rights. That is what the
  prompt is for.
* The typed file tools (`filesystem.write`, `edit`, `delete`) are not
  sandboxed: they ask, and keep undo copies.
* The private `/tmp` hides the real one: a file another tool wrote to `/tmp`
  is not there for a command.
* MCP sidecars are not sandboxed; Jarvis starts them, a model does not
  choose them.
* A sandboxed command can change files in the workspace that later run
  with full rights: a git hook, a `Makefile`, `.envrc`. Approving
  `run outside the sandbox: git push` runs a planted `pre-push` hook too, and
  the prompt shows only `git push`.
* With `[sandbox] network = true`, abstract Unix sockets (X11) are reachable.
* Linux only (bubblewrap). macOS would need `sandbox-exec`.

## Tests

Integration tests run real bubblewrap (`core/tests/sandbox.rs`,
`sandbox_gate.rs`). Where it does not run they skip with a message; CI
installs it and sets `JARVIS_REQUIRE_SANDBOX`, so there a skip is a failure.
