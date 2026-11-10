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
