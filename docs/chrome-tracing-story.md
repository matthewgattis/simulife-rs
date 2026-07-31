# How Chrome Tracing Drove Performance Over the Project's Life

A reconstruction of the profiling-driven optimization arc of simulife-rs, built from
commit messages (which recorded measured before/after numbers), the README's Profiling
section, and the development chat history. Written as source material for a job-interview
success story.

## Setup (Apr 27–29, 2026): "let's find where the slowdown actually is. Empirically."

The project had a vague "the client is struggling to keep up" feeling. **Direction from
Ryan, verbatim from the chat history (Apr 28):** *"Yes please, let's try to find where
the slow down actually is. Emperically."* — an explicit refusal to optimize on guesswork.

That produced the profiling infrastructure (`237496d`, building on the earlier
`println!` → `tracing` migration in `2ce3ade`):

- `--trace-chrome <path>` on both server and viewer via `tracing-chrome`, emitting
  Chrome-trace JSON viewable in `chrome://tracing` / Perfetto.
- `--profile-duration-secs` for unattended, hands-off captures (graceful exit flushes
  the trace buffer — a real gotcha the README now documents).
- `scripts/analyze-trace.sh` — a pure jq+awk summarizer printing
  count/total/avg/p50/p95/max per span, so bottlenecks could be ranked from the CLI
  without opening a UI.

**The first capture immediately overturned the working theory.** The viewer's
decode/upload pipeline — the suspected problem — was nowhere near the bottleneck. The
trace showed `render_frame` eating ~94% of viewer wall time at 115 fps, and the server
tick dominated by `encode_msg` (~9 ms of a 12.8 ms tick).

## Win 1 — Viewer: ~190× fewer render frames (Apr 29)

**Direction from Ryan:** *"egui updating at 115 FPS seems like that really should not be
so demanding… We are not uploading chunk data every frame. Are we? We only need to do
decode and upload when we receive. Right?"* The intuition that the workload didn't
justify the cost was exactly right.

Root cause: `RedrawRequested` events were being fed back into `egui_winit`, which
returned `repaint=true` for them — a self-sustaining tight render loop. Fix (`bbfa58f`):
switch to reactive rendering (`ControlFlow::Wait`, honor egui's `repaint_delay`).
Measured in the trace: **9,637 → 73 render frames per 8 s; render CPU time
7.6 s → 40 ms**, with dragging still smooth because input events trigger redraws
normally.

## Win 2 — Simulation: 42× faster prune, tick rate doubled (Apr 29–30)

**Direction from Ryan:** *"Let's do 1000 generation tracing. Add any metrics/additional
tracing that needs to be added… I think it makes sense to make sure we are tracing with
release build? Yes?"* That added per-phase spans inside `mutate_world` plus a per-tick
`tick_census` event (`a9595ee`), with a two-filter setup so phase events land in the
trace but never spam stdout.

The per-phase breakdown fingered `phase_prune` — a fixpoint loop re-scanning the whole
world with a ~1 MB allocation per pass — at **60% of the phase budget (~59 ms/tick)**.

**The architectural call, and the most interview-worthy moment:** *"We should be doing
no tree traversal… when we process a cell, we do not effect anything other than the 3x3
cell area around ourself. Speed of light is 1 cell/tick."* Ryan rejected the
traversal-based design and imposed a strict cellular-automaton locality principle.
Result (`f9fdea9`): **`phase_prune` 59,327 µs → 1,410 µs (42×); `mutate_world`
73.7 ms → 19.9 ms; tick rate roughly doubled (300 → 570 ticks/30 s)**.

He then generalized it: *"We only write to our own pixel… very fragment shader like…
this moves us closer to a multithreading performance optimization."* That drove the
fair-share soil-pulls rewrite (`e3b5c37`) and pull-pattern growth (`1d4ffb1`). Notably,
tracing was used honestly in both directions here — those commits record *accepted
regressions* (e.g. soil pulls 1.1 → 3.2 ms) as a measured, documented price for
order-independence and future parallelism.

## Win 3 — Server pipeline: +60% throughput from reading thread IDs (May 2)

After decoupling encode/broadcast onto its own task (`e77646b`), **Ryan personally read
the trace and caught the flaw:** *"Looking at the server trace, it looks to me like
`tick` and `encode_tick` are mostly happening on the same thread. Is this right?"*

It was. Tokio's cooperative scheduler had coalesced the sim and encoder — neither had
enough `.await` points, so they serialized on one worker despite being "separate tasks."
Fix (`527bdf6`): move encode to `spawn_blocking`. The trace then showed `tick` and
`encode_tick` on disjoint thread IDs, and **sim throughput jumped 35.5 → 56.8 tps
(+60%)**. This is a textbook example of Chrome tracing's timeline view (specifically the
per-`tid` lanes) catching something aggregate timers never would.

Follow-on wins, each with trace-measured numbers:

- Multithreaded zstd (`772497c`): encode_zstd 8.2 → 2.8 ms; encoder drop rate
  22% → 0.4%.
- Per-chunk delta stream (`0a83af2`): encode_msg 19.5 → 11.1 ms, drop rate 0% —
  informed by an earlier span-split (`b976d14`) proving msgpack, not zstd, was the
  bigger cost.
- Skip encode with no viewers (`93eff34`): 68.6 tps idle, verified by the *absence* of
  encode spans in the trace.

## The measurement culture stuck (May onward)

- Trace-derived metrics were productized: live TPS (`f3ce0a4`) and wire-bytes/s
  (`71d06e9`) readouts in the viewer UI.
- The workflow was institutionalized in the README's Profiling section (`ae3b946`),
  including operational gotchas learned the hard way.
- For the Android port, **Ryan refused to publish unprofiled claims:** *"Until we
  profile, I am not comfortable being specific beyond this."* The README correction
  cycle ended with "bandwidth is the leading hypothesis… nothing has been profiled yet."
  The discipline applied even when it meant saying "we don't know yet."

## Interview-ready summary (STAR)

- **Situation:** Client-server evolutionary cellular automaton in Rust (QUIC + wgpu);
  vague "client can't keep up" performance complaints.
- **Task:** Find and fix the real bottlenecks, empirically.
- **Action:** Built Chrome-trace instrumentation into both binaries plus a CLI trace
  summarizer; iterated capture → analyze → fix, adding finer-grained spans (per-phase,
  msgpack-vs-zstd) as each layer of the onion peeled.
- **Result:** Viewer render work cut ~190×; sim tick 73.7 → ~17.6 ms; server throughput
  35.5 → 68.6 tps; encoder drop rate 22% → 0%; and a repeatable profiling workflow
  documented in the repo.

**Five highlightable personal contributions:**

1. Insisting on empirical measurement before any optimization.
2. The "115 fps for an idle UI is wrong" instinct behind the 190× viewer win.
3. The 3×3-locality / write-to-self / speed-of-light CA architecture behind the 42×
   prune win.
4. Reading the trace timeline personally and spotting the same-thread coalescing behind
   the +60% throughput win.
5. Refusing unprofiled performance claims in public docs.

**Honesty note for interview framing:** the commits are co-authored with Claude, so the
strongest truthful angle is the one the evidence actually supports — Ryan set the
methodology, made the architectural calls, and did trace analysis himself, using an AI
pair-programmer for implementation.
