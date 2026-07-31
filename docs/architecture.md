∂# Architecture Overview

Simulife-rs is a distributed plant-evolution cellular automaton with a Rust server engine and networked wgpu/egui viewer clients connected via QUIC.

## System Architecture

```plantuml
@startuml system-architecture
!theme plain
skinparam backgroundColor #f5f5f5
skinparam roundCornerRadius 10
skinparam fontSize 11

package "Simulife System" {
  actor User as user
  
  package "Client (Viewer)" {
    component "wgpu/winit" as wgpu
    component "egui UI" as egui
    component "QUIC Client" as quic_client
    component "Decoder\n(msgpack/zstd)" as decoder
  }
  
  package "Network" {
    card "QUIC\nUDP:4433" as quic_net
  }
  
  package "Server" {
    component "Simulation Engine" as sim
    component "Persistence\n(zstd/msgpack)" as persist
    component "QUIC Listener" as quic_server
  }
  
  database "World Snapshot" as world_db
  
  user --> wgpu: input
  wgpu --> egui
  egui --> quic_client: ClientMessage
  quic_client --> quic_net
  quic_net --> quic_server
  quic_server --> sim: command
  sim --> quic_server: Welcome/Tick updates
  quic_server --> quic_net
  quic_net --> quic_client
  quic_client --> decoder
  decoder --> wgpu: Chunk updates
  wgpu --> user: render
  
  sim --> persist
  persist --> world_db
  world_db --> sim: load
}

note right of quic_net
  Any number of viewers
  can connect simultaneously.
  Sim controls are server-authoritative.
end note

note right of world_db
  Optional --world-file path.
  Auto-saves every N seconds.
  Format: zstd + msgpack.
end note
@enduml
```

## Simulation Pipeline

Each tick runs **10 phases in sequence** to update world state deterministically:

```plantuml
@startuml simulation-pipeline
!theme plain
skinparam backgroundColor #f5f5f5
skinparam fontSize 10

start
:1️⃣ Photosynthesis;
note right
  Sunlit Leaves gain energy
  (leaf_photosynthesis)
end note

:2️⃣ Soil Regulation;
note right
  Soil energy drifts toward rest
  (soil_energy_regulation per tick)
end note

:3️⃣ Soil Pulls;
note right
  Roots pull organic (3×3 kernel)
  Antennas pull soil energy
  Kernel scaled by root/antenna scales
end note

:4️⃣ Upkeep;
note right
  Every occupant pays fixed
  energy cost (per type)
end note

:5️⃣ Prune;
note right
  Stems drop child bits
  if neighbor is Empty or foreign
  Clear parent ref if parent died
end note

:6️⃣ Drainage;
note right
  Productive stems pull energy
  from dead-end Stem children
end note

:7️⃣ Push;
note right
  Leaves/Roots push to parent
  Stems split between children
  Dead-ends push up
end note

:8️⃣ Germination;
note right
  Seeds → Sprouts when:
  energy ≥ threshold, or
  parent stem dies
end note

:9️⃣ Growth;
note right
  Sprouts execute genome gene
  Spawn slot products (front/left/right)
  Genome mutates on every copy
  Can eat foreign cells
end note

:🔟 Death;
note right
  Zero energy, stranded, or poisoned
  Death deposits organic + energy
  (3×3 kernel, scaled)
end note

stop
@enduml
```

## Data Model: Chunks and Cells

The world is divided into **chunks** (32×32 cells each, in row-major order). Each cell contains:

```plantuml
@startuml data-model
!theme plain
skinparam backgroundColor #f5f5f5
skinparam fontSize 10

class World {
  chunk_x: u32
  chunk_y: u32
  chunks: Vec<Chunk>
  rng: ChaCha12Rng (seeded)
  sim_params: SimParams
  world_gen_params: WorldGenParams
}

class Chunk {
  coord: ChunkCoord (x, y)
  cells: Vec<Cell>
}

class Cell {
  organic: u16
  soil_energy: u16
  sunlit: bool
  lineage_mutation_rate: f32
  occupant: Occupant enum
}

enum Occupant {
  Empty
  Sprout { plant, clan, energy, facing, genome, current_gene, parent }
  Seed { energy, genome, parent }
  Leaf { plant, clan, energy, facing, parent }
  Root { plant, clan, energy, parent }
  Antenna { plant, clan, energy, parent }
  Stem { plant, clan, energy, connections, parent, children }
}

class Genome {
  genes: Vec<Gene>
  mutation_rate: u32 (fixed-point, scaled by RATE_SCALE=10000)
}

class Gene {
  front: GeneSlot
  left: GeneSlot
  right: GeneSlot
}

World "1" *-- "chunk_x*chunk_y" Chunk
Chunk "1" *-- "1024" Cell
Cell "1" *-- "1" Occupant
Occupant <|.. Sprout: contains
Occupant <|.. Seed: contains
Sprout "1" *-- "1" Genome
Genome "1" *-- "32..128" Gene

note right of Cell
  CHUNK_AREA = 32×32 = 1024 cells
  Linear index = cy*32 + cx
end note

note right of Occupant
  Each plant cell has:
  • plant ID (u32, global)
  • clan ID (u32, box index)
  • energy (u16)
  Sprout/Seed also have genome
end note

note right of Genome
  mutation_rate is u32 fixed-point.
  Storage: actual_rate * RATE_SCALE
  Min: 0.01 (100), Max: 0.2 (2000)
  Prevents f32 non-determinism
end note
@enduml
```

## Determinism Architecture

Determinism is guaranteed by:

```plantuml
@startuml determinism-arch
!theme plain
skinparam backgroundColor #f5f5f5
skinparam fontSize 10

package "Determinism Guarantees" {
  component "Seeded ChaCha12Rng" as rng
  note right of rng
    All randomness from seeded RNG
    Same seed → identical sequence
  end note

  component "Fixed-Point Arithmetic" as fixedpt
  note right of fixedpt
    mutation_rate: u32 (not f32)
    Avoids floating-point rounding
    Conversion to f32 only for display
  end note

  component "BTreeMap/BTreeSet\n(not HashMap/HashSet)" as btree
  note right of btree
    Ordered iteration in growth-bid resolution
    HashMap randomizes iteration order
    Caused divergence in earlier bug
  end note

  component "Immutable WorldGenParams\nafter world-gen" as immutable
  note right of immutable
    world_wrap can't change at runtime
    Prevents accidental topology shifts
    Part of world snapshot
  end note

  component "Determinism Test CLI\n(--determinism-test seed:ticks)" as test_cli
  note right of test_cli
    Rebuilds world, runs N ticks
    Compares state hashes across runs
    Easy verification tool
  end note
}

note bottom
  Each change can affect state hash but not break determinism.
  Same seed → identical hash at every tick across restarts.
  Different seeds → different outcomes, still deterministic.
end note
@enduml
```

## Network Protocol

Client and server exchange **binary** messages (msgpack + optional zstd compression):

```plantuml
@startuml network-protocol
!theme plain
skinparam backgroundColor #f5f5f5
skinparam fontSize 10

package "ClientMessage" {
  [Pause]
  [Resume]
  [Tick]
  [SetSimParams: parameter, value]
  [RegenerateWorld: seed, WorldGenParams]
}

package "ServerMessage" {
  [Welcome: tick, chunks_x/y, sim_params, world_gen_params, chunks, seed, rng]
  [TickUpdate: tick, chunk_updates]
}

note right
  **Welcome**: Sent on connect and after Regenerate.
  Establishes synchronized state. Allows viewer to mirror sim controls.

  **TickUpdate**: Sent every tick (if running).
  Only modified chunks included. Msgpack + optional zstd compress.
end note
@enduml
```

## Codebase Structure

```
crates/
├── protocol/          # Wire types + codecs (no async, no GPU)
│   ├── Cell, Chunk, Occupant, Genome
│   ├── SimParams, WorldGenParams
│   ├── ClientMessage, ServerMessage
│   └── msgpack/zstd roundtrip tests
│
├── server/            # Simulation + QUIC listener + persistence
│   ├── main.rs        # Entry point, CLI args, QUIC setup
│   ├── sim.rs         # 10 phases, mutate_world(), BTreeMap for determinism
│   ├── world.rs       # World generation, sprout placement
│   ├── persist.rs     # Save/load snapshots (zstd + msgpack)
│   └── tests/         # Unit tests for each phase + determinism
│
└── viewer/            # wgpu/winit/egui client
    ├── main.rs        # Entry point, window setup
    ├── app.rs         # State management, camera, dialogs
    ├── render.rs      # egui UI, chunk upload, frame render
    ├── net.rs         # QUIC connect/read/send
    └── tests/         # Camera math, label formatting
```

## Key Design Decisions

### 1. **Determinism-First Simulation**
- Fixed-point mutation rates (u32, not f32) to avoid floating-point non-determinism
- ChaCha12Rng (seeded) for all randomness
- BTreeMap for ordered iteration in growth-bid resolution
- Immutable WorldGenParams after world generation
- Verified with `--determinism-test` CLI flag

### 2. **Chunked World**
- 32×32 cell chunks for efficient storage and network transfer
- Row-major layout (chunk_idx = cy * chunks_x + cx)
- Only modified chunks sent to viewers each tick
- Scales to large worlds (default: 36×24 chunks = 864 chunks = 884,736 cells)

### 3. **Live Tuning vs. Immutable Settings**
- **SimParams** (live): Energy flows, costs, scales — take effect next tick
- **WorldGenParams** (immutable): World size, topology (world_wrap), spacing — require regeneration
- Both persisted in snapshot and mirrored in viewer UI

### 4. **Persistence**
- zstd-compressed msgpack snapshots with version field
- .bak rotation keeps previous good copy
- Auto-save every N seconds (configurable)
- Forward-compatible via `#[serde(default)]`

### 5. **Multi-Client Architecture**
- Server-authoritative: all sim controls flow through server
- Viewers mirror UI state via Welcome broadcasts
- Any viewer can pause/resume, change params, or regenerate world
- All viewers see the same world state (eventually consistent via tick updates)

### 6. **Platform Support**
- Desktop: wgpu/winit + egui (macOS, Linux, Windows)
- Android: wgpu/ndk + egui compiled as cdylib
- Server: x86_64 Linux + Docker support

## Performance Characteristics

- **Per-tick cost**: O(world_size) — linear scan of all cells for each phase
- **Network bandwidth**: Only modified chunks (msgpack + optional zstd)
- **Memory**: ~75 MB Docker image (multi-stage build), ~500 MB world state for default size
- **Simulation rate**: Tunable via `--tick-hz` (default: 10 Hz)

## Profiling & Observability

- Chrome-trace JSON output (`--trace-chrome <path>`)
- Per-phase events with occupant counts
- Per-tick viewer metrics (network decode, upload, total µs)
- Graceful shutdown after N seconds (`--profile-duration-secs <n>`)
- Trace summarization: `scripts/analyze-trace.sh <trace.json>`
