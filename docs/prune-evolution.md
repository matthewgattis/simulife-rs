# The Evolution of `phase_prune`

The history of the sim's prune phase — three implementations in three days —
is the clearest single example of the project's trace-driven, locality-first
design philosophy. Reconstructed from git history (`43500aa`, `a9595ee`,
`f9fdea9`) and the development chat log. See `docs/chrome-tracing-story.md`
for the broader profiling arc.

## What prune is for (unchanged across all versions)

A `Stem` carries a `children` bitmask (N/E/S/W) meaning "I push energy toward
these neighbors." A bit is valid only if the neighbor it points at is a sink
(Sprout/Seed) or a Stem that itself still has children. When a branch tip
dies, the stem behind it must drop its bit; that makes *that* stem a dead-end
(`children == 0`), which invalidates *its* parent's bit, and so on — an
unwinding cascade back down the chain:

```
tick N:   Stem→Stem→Stem→Sprout(dies)
          Stem→Stem→Stem(dead-end)      ← last stem's bit now invalid
          Stem→Stem(dead-end)           ← and so on, link by link
          Stem(dead-end)                → dies via phase_death
```

The *local rule* never changed. What changed — twice — was how far the
cascade was pushed within a single tick.

## v1: single pass, and the "stem loops" artifact

Originally prune ran once per tick, so a cascade unwound one link per tick.
Mid-cascade there's a moment where a parent (which hasn't dropped its bit
yet) pushes energy to a child that has already pruned its own bits — and the
child, now a dead-end, pushes the energy straight back. With long chains this
produced a lingering wave of high-energy "stem loops" visible in the viewer.

## v2: the fixpoint (`43500aa`, Apr 29 2026, 12:34)

To kill the artifact, prune was wrapped in a run-until-nothing-changes loop:
each pass allocated a fresh world-sized `Vec<Option<u8>>` (~1 MB), swept
*every cell in the world* applying the local rule, and repeated if any bit
dropped anywhere. Monotonic (bits only drop), so convergence was guaranteed;
the pass count equaled the length of the longest chain dying that tick.

Two things worth noting from that commit's message:

- It explicitly weighed and rejected a tree-traversal alternative: *"much
  cheaper than a global BFS that walks every connected cell whether anything
  changed there or not."* So an explicit tree-walk was on the table that day;
  the fixpoint was chosen as the lesser evil.
- **"Resolves in one tick" referred to the children-bits only.** After one
  tick the whole chain's bits were cleared and the head orphan-died, but the
  remaining stems became dead-ends that still died over *subsequent* ticks
  via `phase_death`. Branches never visually vanished in a single tick.

The commit predicted the extra passes would be cheap ("a typical disturbance
only touches the affected chain"). The flaw: each pass rescanned the *entire
world*, not just the cascade frontier. Worst case O(longest-chain ×
world-size) per tick.

## The trace makes the cost undeniable (Apr 29, evening)

Ryan: *"Let's do 1000 generation tracing. Add any metrics/additional tracing
that needs to be added… make sure we are tracing with release build."* The
per-phase spans landed (`a9595ee`, 21:41), and the Chrome trace showed
`phase_prune` at **~59 ms/tick — 60% of the entire phase budget** — in dense
worlds where something long was always dying.

## The veto (Apr 29, 22:35)

Ryan, verbatim: *"Oh no, this looks completely wrong. We should be doing no
tree traversal. And we should not be iterating over the world more than
once. IMO. It should be perfectly acceptable for this back propagation to
happen over many generations. Ideally, when we process a cell, we do not
effect anything other than the 3x3 cell area around ourself. Speed of light
is 1 cell/tick."*

Both halves of the objection name one of the two competing designs: "no tree
traversal" → the BFS-style walk that had been under discussion; "not
iterating over the world more than once" → the fixpoint that was actually
committed. (No committed version ever moved energy to sinks via traversal —
energy always flowed one hop per tick — but the fixpoint existed precisely
to suppress an energy-flow artifact, and the traversal idea died in
discussion without reaching git.)

The key reframe: the mid-cascade energy bouncing wasn't a bug to stamp out
within the tick — it was **legitimate transient behavior of a proper
cellular automaton**. The requirement "cascades must finish instantly" was
the thing to delete, not optimize.

## v3: single-pass local rule (`f9fdea9`, Apr 30 2026, 11:53)

Back to one pass per tick, but in compute-then-apply form
(`phase_prune_pull`): each stem reads its 3×3 snapshot, decides its own new
mask, writes only to itself in the apply pass. Cascades unwind one link per
tick; the few ticks of energy bouncing are accepted (drainage, added later
in `3f7b3db`/`4207315`, further tamed the dead-end↔parent flow).

Measured in the trace: **`phase_prune` 59,327 µs → 1,410 µs (42×);
`mutate_world` 73,715 µs → 19,871 µs; effective tick rate roughly doubled
(300 → 570 ticks per 30 s).**

The test was renamed to match the philosophy:
`phase_prune_cascades_dead_end_chain_in_one_tick` →
`phase_prune_cascades_one_link_per_tick`.

## The lesson

The fix wasn't optimizing the loop — it was deleting a requirement the rest
of the CA never had. Once every phase obeyed "read your 3×3, write only
yourself, effects travel ≤1 cell/tick," prune stopped being special, its
cost became flat and tiny, and the whole sim moved closer to a
multithreadable gather pattern.
