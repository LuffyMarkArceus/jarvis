# Real use

What using Jarvis day to day turned up. The week of real use before Phase 7
(see the roadmap) adds to this; the list decides what comes next.

Each finding says whose problem it is - **ours** (Jarvis's code) or the
**model's** - and what happened to it.

## 2026-11-10, first sessions (session fe3f7d69)

| Finding | Whose | Done |
| --- | --- | --- |
| The mouse wheel recalled old prompts instead of scrolling: the terminal turns the wheel into arrow keys | ours | Fixed: the TUI captures the mouse; Shift+drag selects text |
| "What is using space on this machine right now? ... find it out for me" searched files for "it out": a `?` ending a word counted as a wildcard | ours | Fixed; eval case `route.disk_space` |
| The same question offered only web tools ("right now" means current events) and no shell; llama searched Wikipedia, then invented a breakdown with "Windows Files" | ours, then the model's | Fixed: questions about this machine need no web source; disk space offers the shell |
| llama sent `shell.run` its own schema instead of a command, and the owner was asked to approve nothing | the model's, made worse by ours | Fixed: a call missing its required arguments goes back to the model |
| A looping llama answer ran 9 minutes and ~3,900 tokens and held the GPU; everything else waited | ours | Fixed: `max_answer_tokens` (local 1024, remote 8192 in this config) |
| `/model remote : ...` in a TUI without `/model` was sent to llama as a question | ours | Fixed: an unknown `/word` says so and sends nothing |
| Even with the shell, llama cannot make the call; qwen proposes a real `du` | the model's | Use `/model qwen` (or a remote model) for machine tasks |
| P vs NP answer is wrong (it defines P vs NP backwards) and does not look up "OpenAI's progress" | the model's | A 1B is not a source for this; `/model` to a larger one |
| "Have some personality, who is your favourite character?" - llama refuses to have a view | ours, then the model's | Owner: witty in chat, factual for facts and actions. The prompt now says so; qwen picks Tony Stark and says why, llama still disclaims first, then picks Nebula |
| Refuses to continue song lyrics | not a bug | Full lyrics are copyrighted; any model should decline |
| The TUI resumed a session from 2026-10-28 | by design | `/new` (Ctrl+N) starts a fresh one |

## 2026-11-10, later

| Finding | Whose | Done |
| --- | --- | --- |
| `/model qwen <message>` failed: everything after `/model` was taken as the model's name, so nothing matched and nothing was sent. The owner wrote it this way twice | ours | `/model <name> <message>` switches, then sends the message; if the switch fails, the message stays in the box |

## 2026-11-17, a week of light use

| Finding | Whose | Done |
| --- | --- | --- |
| Both disk-space questions were "blocked by the permission policy" and never reached the owner: `permissions.toml` still had `shell.run = deny` from early development, so the sandbox never ran | ours (config) | Owner: `ask` |
| "run outside the sandbox: what is taking so much space?" ran the whole sentence as a shell command | ours | A sentence after the phrase goes to the model; a literal command still runs (Milestone 6.4) |
| One turn took 88 s (2,375 prompt tokens, 30 written) and nothing said why | ours | Per-turn timing, `/timing`, `jarvis report` (Milestone 6.4) |
| Two conversations at once: the quick one waits for the long one | ours | Milestone 6.5 |

### First measurements (2026-11-17, Milestone 6.4)

| Turn | Total | Loading | Reading | Writing |
| --- | --- | --- | --- | --- |
| llama3.2:1b, cold | 29.9 s | 25.8 s | 336 tok, 1.9 s | 31 tok, 2.1 s |
| llama3.2:1b, warm | 3.4 s | 0.1 s | 332 tok, 0.7 s | 38 tok, 2.6 s |
| qwen2.5:3b, cold | 49.5 s | 42.4 s | 300 tok, 5.3 s | 13 tok, 1.8 s |
| qwen2.5:3b, warm, tool turn | 14.7 s | 0.0 s | 817 tok, 9.7 s | 31 tok, 4.9 s |

70% of Jarvis's time was loading models. Ollama unloads one after 5 idle
minutes, the 940MX holds one at a time (picking qwen evicted llama), and the
models load at about 50 MB/s from the NTFS drive. Warm, llama answers in 3.4 s.
Second cost: long tool-turn prompts on qwen (817 tokens, 9.7 s).

After keeping the model loaded (`keep_alive`, default 30 minutes) and loading
it in the background on daemon start, TUI connect and `/model` - with the same
context size as real requests, or Ollama loads it again:

| | Before | After |
| --- | --- | --- |
| llama3.2:1b, first question | 29.9 s (25.8 s loading) | 3.6 s (0.0 s loading) |
| qwen2.5:3b, first question after `/model` | 49.5 s (42.4 s loading) | 7.8 s (0.0 s loading) |

What is left is the GPU reading the prompt (qwen: about 60 tokens a second):
shorter prompts are the next lever.

## 2026-11-18, "it does not respond" (session 4c35cbf3)

| Finding | Whose | Done |
| --- | --- | --- |
| After opening Firefox, a question "did not respond". The daemon was healthy throughout: the next question was a remote turn of five model rounds, 50.6 s and 36k tokens, and the TUI showed only a spinner - working and stuck looked the same | ours | Live progress in the header: what it is doing, for how long, "Ctrl+C cancels" after 20 s |
| A request that never got an answer would leave the window "still waiting" forever | ours | Non-turn requests are given up after 30 s; a turn is only ever cancelled by the owner |
| Nothing on the TUI's side recorded what it sent, so the incident could not be reconstructed | ours | `tui.log` per window |
| `jarvis daemon start` left the daemon in the starting terminal's process group with the terminal as input; SIGHUP (closing that terminal) stopped it; SIGTERM left the socket and PID file behind | ours | Own process group, no input, SIGHUP ignored, clean SIGTERM/SIGINT; tested with the real binary |

## 2026-11-18, three /bad (session 08ff9130; the window showed 313a8d0c)

The window's header said 313a8d0c, but a test of two windows run against the
live daemon had moved every window to 08ff9130 - the shared-session bug that
Milestone 6.5a fixes. The feedback was recorded, against the session the
turns were really in.

| Finding | Whose | Done |
| --- | --- | --- |
| "open the episode 001 of BleaCH on my DATA drive" ran `episode 001` and said "Started in the background" | ours | "open X" launches X only if it is an installed app (from the `.desktop` files); otherwise the model finds and opens it, with files and the shell |
| "open vscode" ran `vscode` and said it had started; the program is `code` | ours | Resolved through the app list: VS Code's keyword is vscode, its Exec is `code` |
| A background launch of a missing program reported success (`setsid X &` always does) | ours | The program is checked first; missing, it fails and says so |
| "watch these episodes on my DATA drive in mpv" was offered no tools; nemotron made up a `find` result inside a copy of our `<untrusted>` marker | ours, then the model's | Media and drive words offer files and the shell; the marker is scrubbed from answers, and a copy with no tool run is called out |
| The model did not know "DATA drive" is /mnt/storage and guessed /mnt/DATA | ours | Drive labels and their folders go to the model when it can act on files |

Re-run on nemotron: both requests now search /mnt/storage, then ask before
sending its listing to the remote model ("always allow" stops that).

