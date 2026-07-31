# Tokio: How This Server Uses It (and How It Once Bit Us)

Notes on the async runtime underpinning the server — what Tokio is, the mental
model for futures and tasks (written for someone coming from C++), how this
codebase maps onto it, and a case study of the cooperative-scheduling pitfall
that once serialized the sim and encoder onto one thread.

## What Tokio even is

Rust the language ships only two things for async: the `Future` trait and the
`async`/`await` syntax. There is deliberately **no built-in runtime** — nothing
in the standard library knows how to actually *run* a future. Tokio fills that
gap. It bundles:

1. **A reactor** — an event loop over epoll/kqueue that the OS tells "socket 7
   is readable now" (the layer you'd hand-roll in C++ around `epoll_wait`, or
   get from `boost::asio::io_context`).
2. **A scheduler** — a pool of worker threads (one per core by default) that
   run tasks.
3. **Async flavors of everything** — sockets, timers (`tokio::time::sleep`),
   channels, mutexes, `select!` — that suspend instead of block.

The `#[tokio::main]` attribute on `main.rs` is a macro that builds this runtime
and hands `async fn main` to it as the first task.

## The core concept: a future is an inert state machine

The biggest mental shift. Given:

```rust
async fn run_encode_loop(...) {
    loop {
        let snap = state.latest_snapshot.take().await;   // suspension point
        ...
    }
}
```

the compiler transforms the whole function body into a **state machine
struct** — conceptually the same transformation C++20 coroutines do, except the
"frame" is an ordinary value you can move around, with no per-frame heap
allocation. Each `.await` is a numbered state. "Running" the future means
calling `poll()` on it: it executes forward until it either finishes or hits an
`.await` whose result isn't ready, at which point poll returns `Pending` and
the stack unwinds *completely* — nothing is blocked, no stack is parked, the
state machine just sits there as a struct remembering it's at state 3.

Two consequences that surprise C++ people:

- **Futures are lazy.** Unlike `std::async`, creating a future does nothing.
  It only makes progress when something polls it.
- **There are no hidden threads.** `async` doesn't mean "runs in the
  background." It means "can be suspended and resumed."

## Tasks: green threads over a worker pool

`tokio::spawn(async move { ... })` wraps a future into a **task** and hands it
to the scheduler. A task is the async analog of a thread — cheap enough to have
thousands (basically one allocation), which is why `net.rs` can casually spawn
one per QUIC connection and per stream.

The lifecycle loop:

```
worker thread:  pop task → poll it → runs until Pending (an .await blocked)
                → task registers a "waker" with whatever it's waiting on
                → worker pops the next task
...later...
reactor: "socket readable" → fires the waker → task goes back on a run queue
                → some worker (work-stealing) polls it again, resuming at the
                  exact .await it stopped at
```

So `.await` really means: *"if not ready, put me to sleep and give this OS
thread to somebody else; wake me when the event fires."* That's the whole
trick — a handful of OS threads multiplexing an arbitrary number of suspended
state machines, with the reactor as the wake-up service.

Crucially, the scheduler is **cooperative, with zero preemption**. The only
place a task can give the thread back is an `.await`. CPU work between awaits
monopolizes that worker thread — the scheduler isn't "unfair," it's simply
never given a chance to run.

## This server, mapped onto Tokio

| Tokio piece | Where | Role |
|---|---|---|
| `#[tokio::main]` | `main.rs` | builds the runtime, runs `main` as the root task |
| `tokio::spawn` | `main.rs`, `net.rs` | long-lived tasks: sim loop, encode loop, accept loop, one per connection/stream |
| `tokio::time::sleep` | `sim.rs` (tick limiter) | suspends the sim task without blocking a thread |
| `broadcast::channel` | `main.rs` (`tick_tx`) | fan-out of encoded bytes to every connected viewer's task |
| `Notify` (in `LatestSnapshot`) | `sim.rs` | single-slot handoff: encode task sleeps until sim publishes |
| `tokio::select!` | `main.rs` | "wait on shutdown signal *or* server error, whichever fires first" |
| `yield_now` | `sim.rs` (end of tick loop) | manual cooperation when a loop has no natural `.await` |
| `spawn_blocking` | `sim.rs` (`run_encode_loop`) | escape hatch: CPU-heavy work onto a pool of real, dedicated threads |

Note the pattern: everything network- and coordination-shaped fits async
beautifully, and the two places that needed special handling (`yield_now`,
`spawn_blocking`) are exactly the two places doing raw CPU work.

## Case study: `tick` and `encode_tick` on the same thread

The canonical async-Rust trap, caught here via Chrome tracing (see
`docs/chrome-tracing-story.md` for the timeline; commit `527bdf6` for the fix).

### The wrong mental model

The original setup looked like it should parallelize: two `tokio::spawn` tasks
— sim loop and encode loop — on a multi-threaded runtime with a worker per
core. The C++ intuition says "two threads, two cores, they overlap."

### What actually happened

Both loops were CPU-heavy with almost no yield points. One sim iteration was
~15 ms of pure computation (`mutate_world` holds a mutex and never awaits)
followed by essentially one await. One encode iteration was ~11 ms of
diff + msgpack + zstd, also with one await (`latest_snapshot.take()`).

The coupling between them made it worse than random. The handoff works via a
notify: sim finishes a tick and calls `publish()`, which wakes the encode task.
When task A wakes task B, tokio places B in the *current worker's* LIFO slot —
a deliberate cache-locality optimization for message-passing ping-pong
patterns — and tasks in that slot aren't eligible for work-stealing by idle
workers. So the wake-up itself glued the encoder to the sim loop's thread. The
resulting schedule on that one worker:

```
worker thread 2: [ sim tick 15ms ][ encode 11ms ][ sim tick 15ms ][ encode 11ms ] ...
worker threads 1,3,4: idle
```

Two "concurrent" tasks, perfectly serialized, ~26 ms per tick — cooperative
multitasking behaving exactly as designed, just not as assumed.

### Why the Chrome trace was the right instrument

`tracing-chrome` records the thread id (`tid`) for every span, and Perfetto
draws one lane per tid. Aggregate timers would have said "sim takes 15 ms,
encode takes 11 ms" — both true, neither alarming. Only the timeline view
showed the damning fact: `tick` and `encode_tick` bars **alternating in the
same lane with zero overlap**.

### The fix

`spawn_blocking` in `run_encode_loop` moves the heavy closure to tokio's
*blocking* pool — dedicated OS threads that are expected to hog the CPU,
separate from the async workers. The async encode task shrinks to almost
nothing: await a snapshot, `spawn_blocking` the diff+msgpack+zstd, await the
`JoinHandle`, broadcast the bytes. After the change the trace showed `tick`
hopping across async-worker tids and `encode_tick` on its own blocking-pool
tid, genuinely overlapped. Sim throughput went 35.5 → 56.8 tps (+60%).

One Rust detail worth noticing: the `std::mem::take` dance on
`prev_wire_chunks`. The closure crosses threads, so it must own everything it
touches (`'static + Send` — no borrowing the encoder's local state). The diff
baseline is *moved out* of the loop variable into the closure, used, and
*returned* in the result tuple to be moved back in. In C++ terms: pass-by-move
in, move back out through the future's return value, because the borrow checker
won't let you share a reference across an unscoped thread boundary.

## The rule of thumb

Async is a tool for **waiting on many things cheaply** — thousands of
connections, timers, channels, where tasks spend 99% of their life suspended.
It is *not* a tool for computing in parallel; for that you want real threads
(`spawn_blocking`, rayon, `std::thread`), because CPU work doesn't await and
therefore doesn't cooperate. Tokio's own guidance is that async tasks should
yield every few hundred microseconds at most; a task doing 10+ ms of straight
computation between awaits is in the wrong execution domain.

This failure class appears twice in `sim.rs`, with two remedies:

- The encoder's CPU burst → **relocate it** (`spawn_blocking`).
- The sim loop with rate-limiting off has *no* await per iteration, which
  would starve the Ctrl-C handler and QUIC accept loop → **yield manually**
  (`tokio::task::yield_now().await` once per tick).

Same disease (cooperative scheduler, uncooperative task): yield if the work
must stay async, relocate it if it's genuinely blocking.

To go one level deeper, study how `Waker` works — the mechanism connecting
reactor to scheduler that makes suspend/resume click. Tokio's tutorial at
[tokio.rs](https://tokio.rs/tokio/tutorial) builds a mini-runtime from scratch
to show it.
