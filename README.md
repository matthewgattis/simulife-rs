# cellular-automata

A plant-evolution cellular automaton. The server runs the simulation; the viewer connects over QUIC and renders the world live with wgpu/egui.

Inspired by [this cellular-automata video](https://youtu.be/q2uuMY37JuA): each cell can be Empty, Leaf, Root, Stem, Antenna, Sprout, or Seed. Plants grow from sprouts whose genome encodes what to spawn at front/left/right slots. Mutation on every copy site means lineages drift; eating, soil toxicity, energy/organic flows, and germination create selective pressure.

## Workspace

```
crates/
  protocol/   wire types + msgpack/zstd codecs (no_async, no GPU)
  server/     simulation + QUIC listener + persistence
  viewer/     wgpu/winit/egui client
scripts/
  analyze-trace.sh   summarises a tracing-chrome JSON trace
```

## Build & run

```sh
cargo build --release -p server -p viewer
./target/release/server
./target/release/viewer
```

The server starts paused so you can attach a viewer first; press the **Resume** button (or `Space`) to start ticking. The viewer auto-connects to `127.0.0.1:4433`.

### Useful server flags

| flag | default | purpose |
|---|---|---|
| `--world-width` | 36 | world size in chunks along X (each chunk = 32×32 cells) |
| `--world-height` | 24 | world size in chunks along Y |
| `--tick-hz` | 10 | simulation rate (only honored when "Limit tick rate" is on) |
| `--seed` | random | u64 world seed; loaded snapshots override |
| `--world-file` | (none) | load/save snapshot path; auto-saves while running |
| `--autosave-secs` | 30 | seconds between auto-saves; 0 disables |
| `--start-running` | off | skip the default paused state |
| `--trace-chrome <path>` | (none) | record a Chrome-trace JSON profile |
| `--profile-duration-secs <n>` | (none) | graceful exit after `n` seconds (flushes trace) |

### Useful viewer flags

| flag | purpose |
|---|---|
| `--server-addr` | server endpoint (default `127.0.0.1:4433`) |
| `--tick-metrics` | log per-tick decode/upload timing |
| `--trace-chrome <path>` | record a Chrome-trace JSON profile |
| `--profile-duration-secs <n>` | graceful exit after `n` seconds |

### Viewer controls

- **Drag**: pan camera
- **Scroll**: zoom
- **Right-click**: cell context menu (spawn sprout, etc.)
- **Space**: toggle pause
- **`.`**: step one tick (also pauses)
- **Status panel**: pause/resume, tick rate slider, layer toggles, regenerate-world dialog

## Simulation rules (high level)

Each tick runs phases in order:

1. **Photosynthesis** — sunlit Leaves gain energy.
2. **Soil regulation** — `soil_energy` drifts toward 100 by 1/tick.
3. **Soil pulls** — Roots pull organic, Antennas pull soil energy from a 3×3 kernel.
4. **Upkeep** — every occupant pays its fixed energy cost.
5. **Prune** — Stems drop child bits whose neighbor isn't a valid sink.
6. **Push** — Leaves/Antennas/Roots push surplus to parent; Stems split between children. Sprouts and Seeds are terminal sinks.
7. **Seed germination** — Seeds become Sprouts when energy reaches threshold or the parent stem dies. The new Sprout enters the same tick's growth phase.
8. **Growth** — Sprouts execute their current gene, spawning slot products at front/left/right. Genome is mutated on every copy. Sprouts can eat foreign Leaf/Antenna/Sprout/Seed cells (Roots and Stems are inviolate). Frustrated sprouts die in place.
9. **Death** — zero-energy, orphaned, or poisoned cells die. Death deposits both organic (kernel weights) and the cell's own energy (distributed across the kernel, integer-divided to avoid manufacturing energy).

**Soil toxicity**: when soil organic > 300, only Roots survive in that cell. When soil_energy > 200, only Antennas survive. Both poisons → nobody.

**Multi-client**: any number of viewers can connect. Sim controls (pause, tick rate, regenerate) are server-authoritative — a control change from one viewer broadcasts a fresh `Welcome` so every viewer's UI reflects the same state.

## Persistence

`--world-file <path>` enables periodic auto-saves and a final shutdown save. The format is zstd-compressed msgpack of a `WorldSnapshot` with `seed`, full RNG state, current tick, plant id counter, and chunks. A `.bak` rotation keeps the previous good copy.

## Profiling

Both binaries can record a Chrome-trace JSON. Each captured span / event becomes a timeline bar at `chrome://tracing` or [ui.perfetto.dev](https://ui.perfetto.dev).

### Coverage

**Server** (per-tick, in `mutate_world`): `tick` → `mutate_world` → `phase_photo`, `phase_soil_regulation`, `phase_soil_pulls`, `phase_upkeep`, `phase_prune`, `phase_drainage`, `phase_push`, `phase_germination`, `phase_growth`, `phase_death`. Each phase ends with a per-tick event on `target="phase"` carrying counts (e.g. `phase_prune_done { prune_changes }`, `phase_death_done { deaths }`, `tick_census { occupants, leaves, ... }`).

The console fmt layer uses `EnvFilter::new("info,phase=off,quinn=warn")` by default, so the phase events stay out of stdout. The chrome layer overrides to `phase=info` so they're recorded in the trace file. Bottom line: phase events show up in the trace but not on stdout — exactly when you want them.

**Viewer**: `render_frame`, `tick_apply { tick }`, `upload_chunks`, `read` (network), `decode { bytes }`. `--tick-metrics` adds `info!` log lines per applied tick (assign / upload / total µs) — independent of chrome trace.

### Capture

```sh
mkdir -p traces
./target/release/server --start-running \
  --trace-chrome traces/server.json --profile-duration-secs 10 &
./target/release/viewer \
  --trace-chrome traces/viewer.json --profile-duration-secs 8 \
  --tick-metrics
wait
```

`--profile-duration-secs <n>` is what makes this hands-off — both binaries gracefully exit after `n` seconds, which flushes the chrome layer's buffered events to disk. Without it the trace JSON would be truncated.

Open `traces/server.json` (or `viewer.json`) in `chrome://tracing` / Perfetto. Or summarize from the CLI:

```sh
scripts/analyze-trace.sh traces/server.json
```

That prints per-span `count / total / avg / p50 / p95 / max` in microseconds, sorted by total — the fastest way to spot which phase is dominating.

### Tips

- The viewer's `--profile-duration-secs` should be a few seconds *less* than the server's so the viewer shuts down first; otherwise the viewer keeps trying to read from a closed connection during shutdown and the last second of trace is mostly idle.
- Phase events don't fire while the sim is paused, so `--start-running` is essential for server profiling.
- `EnvFilter` honors `RUST_LOG`. To see phase events live on stdout (e.g. `RUST_LOG="info,phase=info,quinn=warn"`), set it before launching.

## Tests

```sh
cargo test --workspace
```

Unit tests live alongside the code (`#[cfg(test)] mod tests`) for sim rules, world generation, persistence, the wire format, and viewer pure helpers (Camera math, label formatting).
