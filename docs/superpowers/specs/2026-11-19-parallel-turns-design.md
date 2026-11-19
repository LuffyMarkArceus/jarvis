# Turns in Parallel and the GPU Queue (Milestone 6.5b) - Design

Status: approved approach and defaults (owner, 2026-11-19). Follows 6.5a
(one conversation per window, merged in #14).

## Intent

Two windows can work at once. A quick question in one window must not wait
for a long turn in another, and a window waiting for the GPU says so instead
of looking hung.

Owner's direction (2026-11-19). Jarvis should feel as responsive as a real
assistant. Remote models are fast and capable, so they can be "the brain",
while Jarvis remains a system for using local and remote models as the user
wishes. Sending a call to the remote model because the local GPU is busy is
acceptable, as long as the user is told every time. The owner's laptop
(GTX 940MX, 4 GB) runs llama3.2:1b and qwen2.5:3b one at a time, but a newer
machine can run several models together. Capacity must therefore be a
setting, not a constant.

Measured from the owner's `turns.jsonl` (median, the owner's time excluded):

| Tier / model | Median |
|---|---|
| local llama3.2:1b | 3.6 s |
| local qwen2.5:3b | 14.7 s |
| remote nemotron | 6.8 s |

## How it works today

* **The daemon lock.** The IPC server holds one `tokio::sync::Mutex<Daemon>`
  for every request. For `ask` it is held for the whole turn, including while
  a confirmation waits for the owner (`core/src/ipc/server.rs`,
  `handle_ask`).
* **The orchestrator lock.** Inside, `Daemon` holds
  `Arc<Mutex<Orchestrator>>` and also locks it for the whole turn
  (`daemon.rs`, `ask_with_footer`).
* **The result.** One window at an "Allow? y/n" prompt blocks every other
  window: questions, `model.list`, `status`, everything.
* **No exclusive methods.** No method on `Orchestrator` or `Daemon` takes
  `&mut self`. Their parts lock themselves: `MemoryManager` (a std Mutex
  around its Connection), `PermissionManager` (the audit connection),
  `SessionTrust`, `McpClient` (calls are matched to answers by id, so
  several can be in flight) and `Conversations`.
* **Local model calls.** Every local call goes through
  `Orchestrator::send` in `core/src/orchestrator/gateway.rs`, by way of
  `ask_model`, or the local fallback after a withheld remote request.
* **Warm-up.** `Orchestrator::warm_up` spawns a load of the conversation's
  local model.

## Goals

1. **No turn-long locks.** The daemon and the orchestrator are shared
   without a turn-long lock, and locks are held only around state. While one
   window's turn runs, or waits on a prompt, any other window's requests are
   answered at once.
2. **One turn at a time per conversation.** A second `ask` in a conversation
   that is answering is refused at once with: "This conversation is
   answering another question. Wait for it, or open another with /new." No
   history interleaves.
3. **The local lane.** A `LocalLane` with `slots` places, first come first
   served, admits every model call to a *local* destination.
   * A call that must wait reports progress at once: "waiting for the GPU:
     another window is using qwen2.5:3b". The name is the model of a call
     holding a slot.
   * Cancelling a turn while it waits ends the wait.
   * Waiting for a slot is not model time, and the timing reports it
     separately.
4. **Spill to remote.** A local call that has waited `spill_after` seconds
   (default 5) for a slot, with a remote tier configured and
   `spill = "auto"`, goes to the remote tier instead.
   * The progress line says "GPU busy: answering with nemotron (remote)".
   * The answer's footer ends with "(GPU busy)".
   * The rest of *that turn* stays remote. The conversation's tier does not
     change, and its next turn tries local again.
   * The remote path runs exactly as for a remote conversation: the egress
     check, prompts for local files and untrusted content, redaction. If
     nothing may be sent, the turn does not fall back to local through the
     withheld path's "answer locally?" prompt, because local is busy. It
     keeps waiting for a slot instead.
5. **The spill setting.** `spill = "ask"` asks first (confirmation tool
   `egress.spill`, args: the model and the wait so far). A no keeps waiting
   for the slot. `"never"` keeps waiting. With nobody to ask (no confirmer),
   `"ask"` means wait.
6. **New conversations start local.** A new conversation starts on the
   local tier, with the model last used on the local tier, unless
   `[remote] default = true`. This replaces 6.5a's "last used tier" (owner,
   2026-11-19).
7. **Per-tier model choice.** A conversation saves its chosen model for
   *each* tier, not just the active one. This was the 6.5a review's Minor 1.
   Hopping tiers, and spilling, must not lose the local choice.

## Non-goals

* **Queuing a second message in the same conversation.** That is 6.7.
  6.5b refuses it clearly.
* **Priorities**, such as interactive questions ahead of background jobs.
  That is Phase 7: `LocalLane` is the seam where a priority queue replaces
  the FIFO.
* **Asking Ollama what is loaded (`/api/ps`)**, or scheduling by model
  residency.
* **Parallelism inside one turn**, such as tool calls run concurrently.
* **Changing Ollama's own settings**, `OLLAMA_NUM_PARALLEL` and the like.

## Anti-goals

These would make it a failure even if it worked:

* A spill that skips the egress gate, or that silently changes a
  conversation's tier.
* A wait the user cannot see, or cannot cancel.
* A deadlock or lost answer under two concurrent turns. Tests must cover
  this with deterministic fake providers, not with sleeps and luck.
* Any test touching the owner's daemon, data or GPU.

## Configuration

```toml
[model]
slots = 1            # local model calls at once; raise on a machine that
                     # can hold several models (and set OLLAMA_NUM_PARALLEL)

[remote]
spill = "auto"       # "auto" | "ask" | "never"
spill_after = 5      # seconds a local call waits before spilling
```

* **Defaults:** `slots = 1`, `spill = "auto"`, `spill_after = 5`.
* **No remote configured:** `spill` has no effect.
* **`slots = 0`:** invalid. Config loading rejects it with a clear message.

## Approaches considered

1. **A `LocalLane` object with a FIFO semaphore (chosen).** It wraps
   `tokio::sync::Semaphore` (fair, FIFO) with `slots` permits, plus a record
   of what each holder is running.
   * **Interface:**
     `acquire(model, deadline: Option<Duration>, cancel) -> Admit`, where
     `Admit` is one of:
     * `Slot(permit)` - the call may run;
     * `Busy { running: String, waited }` - the deadline passed;
     * `Cancelled`.
     The permit is released on drop.
   * **Why:** it is small and testable, and nothing outside knows how it
     queues. Phase 7 can swap the inside for a priority queue without
     changing callers.
2. **A GPU worker task owning a job queue.** It would give positions and
   priorities, but errors and cancellation would cross channels. Rejected
   for now: the priority need is Phase 7's, and approach 1 leaves the seam
   for it.
3. **Scheduling on Ollama's loaded models (`/api/ps`).** It would be the
   cleverest, but it couples us to Ollama's internals and is hard to test.
   Rejected.

## Design

### Sharing without turn-long locks

* **The server** holds `Arc<Daemon>`, not `Arc<Mutex<Daemon>>`.
* **The daemon** holds `Arc<Orchestrator>`, not `Arc<Mutex<Orchestrator>>`.
  If the compiler shows a part is not `Sync`, guard that part, not the
  whole.
* **Appends to `turns.jsonl`**, including its rotation, go under one
  `std::sync::Mutex` in the orchestrator, so two turns cannot interleave or
  race a rotation.
* **Writes to the permissions file and the egress allow file** (the "always
  allow" choices) go under a mutex, so concurrent read-modify-write cannot
  lose a rule.

### One turn per conversation

* **The marker.** `Conversation` gains a busy marker, for example
  `answering: AtomicBool` or a `tokio::sync::Mutex<()>` taken with
  `try_lock`.
* **Take it or refuse.** The daemon's `ask` takes the marker or returns the
  error from Goal 2 without running anything.
* **Release.** A guard releases the marker when the turn ends for any
  reason: answer, error, cancellation, or a panic unwinding.

### The lane and spill in the gateway

* **One lane.** The orchestrator owns one `LocalLane`. In `send`, a request
  to a local destination first calls
  `lane.acquire(model, deadline, cancel)`. The deadline is `spill_after`
  when a spill is possible, and `None` otherwise.
* **A spill is possible when:**
  * the remote tier is configured;
  * `spill != "never"`;
  * the call is not already the local fallback of a withheld remote
    request.
* **On `Busy`, with `spill = "ask"`:** ask; a no means wait with no
  deadline, keeping the place in line (see the calls settled below).
* **On `Busy`, otherwise:**
  * set `rec.spilled = true`;
  * report the progress line;
  * re-enter `ask_model`'s remote path for this request.
* **Choosing the tier.** `tier()` for a recorder with `spilled` returns the
  remote tier, so later calls in the turn go remote without queuing.
* **The model used.** A spilled call uses the remote tier's chosen model
  for that conversation, or else the remote `default_model`.
* **The permit** is held across the provider call, streaming included, and
  dropped right after.
* **Warm-up** takes no slot. It is skipped while every slot is held, since
  loading another model would evict the one in use.

### Settled after the builder check (designer's calls)

1. **A waiter keeps its place in line while it is asked something.** This
   covers `spill = "ask"` and an egress prompt on a spilled call. `LocalLane`
   hands back a pending waiter rather than leaving the queue on `Busy`, so
   `Admit::Busy` carries the waiter. The waiter stays queued during the
   prompt.
   * If a slot becomes free while the prompt is open, the waiter holds it
     until the answer.
   * "Send to remote" releases the slot and goes remote.
   * "No" runs locally at once on the held slot. Waiting with no deadline
     keeps the original place.
   * The GPU may sit idle for as long as a human decides. That is bounded by
     the confirmation timeout, and a prompt cannot be withdrawn until 6.7.
2. **A spilled call's request is rebuilt as a remote conversation would
   build it.** Recalled memory that the egress policy would block is left
   out, exactly as `prompt.rs` does for a remote tier. Otherwise most spills
   would prompt about recall, and some could never be sent. So the spill
   decision sits where the request is assembled, in the turn loop, before
   each model call: wait for a slot, or on `Busy` mark the turn spilled and
   build the remote request. It does not sit inside `send`. Rule: a spilled
   turn behaves exactly like a remote conversation for the rest of the turn.
3. **The settings a turn runs with are fixed when the turn starts.** The
   turn recorder copies the conversation's tier and chosen models at the
   start. `/model` and `/remote` from another window on the same
   conversation are accepted, and they apply from that conversation's next
   turn.
   * `session.delete` of a conversation that is answering is refused:
     "...is answering a question; delete it after."
   * `/prune` skips it.
   * Nothing forgets a conversation that is mid-turn, so two `Conversation`
     values for one id can never exist.
4. **The lane follows the real destination, not the tier's name.** A call
   is queued when `destination_of(cfg)` is local, whichever tier it is. A
   spill is possible only when the remote tier's destination is actually
   remote. A `[remote]` that points at a local Ollama does not count.
5. **A spilled call whose content may not be sent ends the spill.** The
   user said no, or the policy says never. The turn clears `spilled`, waits
   for its slot and runs locally for the rest of the turn, so it is not
   asked again on every step. Its footer is the local model's. It gets the
   signal `"spill_withheld"` instead of `"spilled"`.

### Reporting

* **Timing.** `TurnTiming` gains `queue_ms`, the time spent waiting for a
  slot. `jarvis_ms()` excludes it, so "slow" still means Jarvis was slow.
* **Signals.** A turn that waited over `spill_after` gets the signal
  `"queued"`. A spilled turn gets `"spilled"`.
* **The footer** of a spilled answer reads `<model> · remote (GPU busy)`.
* **Each stored answer** still records its real model.
* **`jarvis report`** counts the new signals.

## Testing

All tests are deterministic and do not touch the owner's daemon, data or
GPU.

* **The gated provider.** A `Gated` fake provider blocks each call until
  the test releases it (`tokio::sync::Notify` or a oneshot), and records the
  order of calls.
* **Time.** Paused tokio time (`start_paused = true`) for `spill_after`.
* **Real sockets.** Real-socket tests in the style of `two_windows.rs`.

The cases:

1. **Window A is at a confirmation prompt:** B's `model.list`, `status` and
   `ask` are answered.
2. **A's local call holds the only slot:** B's call waits, and B gets the
   "waiting for the GPU" progress naming A's model. When A is released, B
   runs. The order is FIFO across three waiters.
3. **With `spill = "auto"`:** B spills after `spill_after`. Its answer
   comes from the remote provider, the footer says "(GPU busy)", B's
   conversation is still local, and B's next turn uses local.
4. **Spill and the egress gate:** B read a local file outside the allow
   list, and B spills. The confirmer is asked `egress.send`. If it says no,
   nothing reaches the remote provider and B waits for the slot.
5. **`spill = "never"` and `"ask"` with a no:** B waits and runs locally.
6. **Cancelling B while it waits** ends the wait promptly. The slot count is
   intact afterwards.
7. **A second `ask` in a conversation that is answering** is refused at
   once. After the first turn ends, a new `ask` works.
8. **`slots = 2`:** two local calls run together, and a third waits.
9. **New conversations start local** after a remote one was used last. A
   conversation's local choice survives a trip to remote and back, and a
   restart.
10. **Two concurrent turns appending to the turn log** leave every line
    intact.

## Left to the builder

* The exact module (`core/src/lane.rs` or similar) and the type names.
* How `Admit` carries the permit.
* Whether the busy marker is an atomic or a mutex.
* How `spill_after` deadlines interact with paused time in tests.
* The precise wording of progress lines, within the meaning above.
* Whether `queue_ms` is per call or per turn. Per turn is enough for the
  report.
