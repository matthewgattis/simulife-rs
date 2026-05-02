# AGENTS.md

Guidance for AI agents (Claude Code etc.) working in this repo. Skim before doing non-trivial work; the README covers what the project *is*, this file covers how to work in it without breaking conventions.

## Workspace

Three crates:
- `protocol` — wire types and msgpack+zstd codecs. No tokio, no GPU, no rand. Both other crates depend on it.
- `server` — sim, QUIC listener, persistence. Holds authoritative state.
- `viewer` — wgpu/winit/egui client. Reads server state via the wire format, renders, sends control commands. Holds no authoritative state.

The `server` and `viewer` binaries are usually run together. Server defaults to paused so viewers can attach first.

## Commands you'll run

| task | command |
|---|---|
| typecheck workspace | `cargo check --workspace` |
| run all tests | `cargo test --workspace` |
| build release | `cargo build --release -p server -p viewer` |
| profiling capture | see README profiling section |

When iterating on sim rules:
1. Edit + add/update tests.
2. `cargo test --workspace`.
3. `cargo build --release -p server -p viewer`.
4. `pkill -f "target/release/server"; pkill -f "target/release/viewer"`, restart in background, verify behavior.

The user prefers binaries restart automatically after changes (the workflow above).

## Conventions

### Code style
- Edition 2024. Watch for `gen` being a reserved keyword — use `r#gen` for `rand::Rng::gen`.
- No emojis in code or commit messages unless the user asked for them.
- No multi-paragraph docstrings or multi-line comment blocks. One short line if at all. Comment *why* (non-obvious constraints, gotchas), never *what* — well-named identifiers cover that.
- Avoid `#[allow(dead_code)]` — delete dead code instead.
- No `unwrap()` on user-input or network paths; use `?` and propagate.

### Tests
- Unit tests live in `#[cfg(test)] mod tests` at the bottom of the file under test.
- Phase-style sim tests build a tiny world (often 1×1 chunk), call `mutate_world` once, assert exact post-tick numbers. Hand-trace energy through all 10 phases (photo → soil regulation → soil pulls → upkeep → prune → drainage → push → germination → growth → death) when writing them — close-to-threshold values often produce surprising results.
- The `det_rng()` helper in `sim::tests` gives a seeded `ChaCha12Rng` for reproducibility.
- Tests should derive expected numbers from the test-module mirror constants (`UPKEEP_DEFAULT`, `COST_SEED`, etc.) instead of hard-coding, so retuning `SimParams::default()` only requires touching the mirrors. The test module's `test_params()` returns `SimParams::default()` with `world_wrap: false` so existing edge tests keep working.
- Write tests *before* committing new sim rules; future-you will thank present-you.

### Commits
- One logical change per commit. The user reviews diffs frequently.
- Commit messages: imperative summary line, blank line, prose body explaining *why* and any non-obvious tradeoffs. Include measured numbers when relevant.
- Commit footer:
  ```
  Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
  ```
- Don't commit until the user asks. Don't push unless asked.

### Files you should not create
- Project-level docs (`*.md`) unless the user asks. README and this file are exceptions.
- Plan/decision/analysis docs as intermediate artifacts.

## Architecture invariants

### Server is authoritative for sim state
The viewer's `sim_paused` / `sim_tick_hz` / `sim_tick` / `seed` / `sim_params` / `world_gen_params` are *mirrors* of server state. UI buttons (and the params sliders) are pure requests — they send a `ClientMessage` and wait for the server's `Welcome` broadcast to update the local mirror. Don't re-add optimistic local toggles on these unless the user explicitly asks.

### Wire vs in-memory types
- `Occupant` / `Cell` / `Chunk` (in `protocol`): full types with `Box<Genome>` for sprouts/seeds. Used for persistence and in the server's in-memory world.
- `WireOccupant` / `WireCell` / `WireChunk`: lightweight wire types with no genomes. Used in `ServerMessage::ChunkBatch`. The viewer never sees genomes via the tick stream.

If you need genomes on the viewer (e.g., to display in a cell-details panel), add a request/response round-trip; don't put genomes back into tick batches.

### Live-tunable params
`SimParams` (live; takes effect next tick) and `WorldGenParams` (regen-time only) live in `protocol` and are mirrored on `SimState`. The sim loop snapshots `SimParams` at the top of each tick so `mutate_world` works against a stable copy. When adding a new tunable scalar:
1. Add the field on the struct + a default in the protocol crate (forward-compat handled automatically for `Welcome` / snapshot via `#[serde(default)]` — bump `SNAPSHOT_VERSION` only on non-additive changes).
2. Plumb a slider into the viewer's `sim_params_ui` (or `draw_regen_dialog` for world-gen).
3. In the sim path, read from the `params: &SimParams` arg threaded into `mutate_world` and helpers — never from a `const`.

### Sim phase order matters
Phases run in a fixed order each tick (see README — 10 phases). When adding a new rule, decide *which phase* it logically belongs in:
- Soil regulation runs **before** soil pulls so antennae/roots deplete a freshened soil each tick.
- Drainage runs **between prune and push** so any bit-drop happens before push reads `children`. This is also what prevents parent↔dead-end mutual energy flow.
- Seed germination runs **between push and growth** so a same-tick germinated sprout can immediately try to execute gene 0.
- Toxicity death is folded into the death phase alongside zero-energy and orphan checks.
- Death deposits read the cell's `occupant_energy` *at death time*; if you reorder phases, watch that energy is captured before any earlier phase zeroes it (drainage is one such phase).

### Encode runs on its own task
The sim loop publishes `(tick, Vec<WireChunk>)` into `SimState::latest_snapshot` (a `Mutex<Option<T>> + Notify` slot with drop-old semantics). A separate `run_encode_loop` tokio task takes from the slot, runs msgpack+zstd, and broadcasts via `tick_tx`. Sim never blocks on encode; if encode falls behind, intermediate snapshots are dropped. The user's tick-rate limiter (`SimAction::TickAfter`) gates *how often a tick starts*, independent of encode throughput.

### Tracing spans
Hot paths are instrumented with `tracing::info_span!`. Capture with `--trace-chrome <path> --profile-duration-secs <n>` on either binary; analyze with `scripts/analyze-trace.sh`. Don't enter spans across `await`s — use `.instrument()` for async, `.entered()` for sync (and scope so the guard drops before any `.await`). The viewer's `--tick-metrics` flag toggles the simpler per-tick `info!` lines independently. Per-phase events go on `target = "phase"` so they're recorded in the chrome trace but suppressed on stdout by default.

### Determinism
The simulation is deterministic given the seed. Don't introduce `thread_rng()` calls in the sim path — use the threaded `&mut impl Rng` argument. `mutate_genome` clamps the resulting `mutation_rate` to `[MUTATION_RATE_MIN, MUTATION_RATE_MAX]` (both in `protocol`); the floor prevents an absorbing zero state.

## Risky operations

These all require explicit user authorization (don't do them on your own):
- `git push` (especially force-push), `git reset --hard`, deleting branches.
- Deleting files outside scratch paths.
- Modifying `Cargo.lock` directly.
- Bumping major dependency versions.

## Things to flag, not do

If you notice any of these while working on something else, tell the user — don't silently fix:
- Cross-plant energy bleed (e.g., a stem pushing energy across plant ids).
- Wire format incompatibilities between server and a running viewer (manifest as `invalid type: sequence, expected variant identifier` errors).
- Tests that pass but assert weak conditions (e.g., just "not equal to None" when an exact value was expected).

## Memory system

There is a per-project memory system at `~/.claude/projects/-Users-ryan-projects-cellular-automata/memory/` that persists across conversations. Add to it when you learn something durable about the project that *isn't* derivable from the current code (architectural intent, user preferences, ongoing goals). See `MEMORY.md` for the index. Don't duplicate things this file already says.
