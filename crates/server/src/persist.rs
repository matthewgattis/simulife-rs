use std::{
    path::{Path, PathBuf},
    sync::atomic::Ordering,
};

use anyhow::{Context, Result};
use protocol::Chunk;
use rand_chacha::ChaCha12Rng;
use serde::{Deserialize, Serialize};
use tracing::info;

use crate::sim::SimState;
use crate::world;

const ZSTD_MAGIC: [u8; 4] = [0x28, 0xB5, 0x2F, 0xFD];
const ZSTD_LEVEL: i32 = 3;

#[derive(Serialize, Deserialize)]
pub struct WorldSnapshot {
    pub chunks_x: u32,
    pub chunks_y: u32,
    pub next_plant_id: u32,
    #[serde(default)]
    pub current_tick: u64,
    pub chunks: Vec<Chunk>,
    /// Seed used to initialize the RNG for this run. `None` on snapshots
    /// saved before seed plumbing existed.
    #[serde(default)]
    pub seed: Option<u64>,
    /// Full RNG state at save time. `None` on pre-seed snapshots; main
    /// rebuilds a fresh RNG from `seed` in that case.
    #[serde(default)]
    pub rng: Option<ChaCha12Rng>,
}

pub fn load_world(path: &Path) -> Result<WorldSnapshot> {
    let raw = std::fs::read(path).with_context(|| format!("read {path:?}"))?;
    let payload = if raw.starts_with(&ZSTD_MAGIC) {
        zstd::decode_all(&raw[..]).context("zstd decode")?
    } else {
        raw.clone()
    };
    let snapshot: WorldSnapshot = rmp_serde::from_slice(&payload)?;
    info!(
        path = %path.display(),
        on_disk = raw.len(),
        decoded = payload.len(),
        chunks = snapshot.chunks.len(),
        chunks_x = snapshot.chunks_x,
        chunks_y = snapshot.chunks_y,
        next_plant_id = snapshot.next_plant_id,
        "world loaded",
    );
    Ok(snapshot)
}

pub fn save_world(path: &Path, state: &SimState) -> Result<()> {
    let snapshot = WorldSnapshot {
        chunks_x: state.chunks_x,
        chunks_y: state.chunks_y,
        next_plant_id: state.next_plant_id.load(Ordering::Relaxed),
        current_tick: state.current_tick.load(Ordering::Relaxed),
        chunks: state.world.lock().expect("sim lock poisoned").clone(),
        seed: Some(state.seed.load(Ordering::Relaxed)),
        rng: Some(state.rng.lock().expect("rng lock poisoned").clone()),
    };
    let raw = rmp_serde::to_vec(&snapshot)?;
    let compressed = zstd::encode_all(&raw[..], ZSTD_LEVEL).context("zstd encode")?;
    atomic_save(path, &compressed)?;
    let ratio = raw.len() as f64 / compressed.len().max(1) as f64;
    info!(
        path = %path.display(),
        on_disk = compressed.len(),
        uncompressed = raw.len(),
        ratio = format!("{ratio:.1}x"),
        "world saved",
    );
    Ok(())
}

pub fn load_or_build(
    world_file: Option<&Path>,
    world_width: u32,
    world_height: u32,
) -> Result<WorldSnapshot> {
    if let Some(path) = world_file {
        if path.exists() {
            return load_world(path);
        }
    }
    let chunks = world::build_world(world_width, world_height);
    Ok(WorldSnapshot {
        chunks_x: world_width,
        chunks_y: world_height,
        next_plant_id: 1,
        current_tick: 0,
        chunks,
        seed: None,
        rng: None,
    })
}

/// Crash-safe save: write `<path>.tmp`, fsync, rotate `<path>` → `<path>.bak`,
/// then atomically rename tmp → live. The live path is always either the old
/// version or the new one, never partial.
fn atomic_save(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;

    let tmp = with_extension_suffix(path, "tmp");
    let bak = with_extension_suffix(path, "bak");

    {
        let mut file = std::fs::File::create(&tmp)
            .with_context(|| format!("create {tmp:?}"))?;
        file.write_all(bytes)
            .with_context(|| format!("write {tmp:?}"))?;
        file.sync_all()
            .with_context(|| format!("fsync {tmp:?}"))?;
    }

    if path.exists() {
        std::fs::rename(path, &bak)
            .with_context(|| format!("rotate {path:?} -> {bak:?}"))?;
    }
    std::fs::rename(&tmp, path)
        .with_context(|| format!("rename {tmp:?} -> {path:?}"))?;
    Ok(())
}

fn with_extension_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".");
    s.push(suffix);
    PathBuf::from(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sim::{SimControl, SimState};
    use protocol::{CHUNK_AREA, Cell, ChunkCoord, Direction, Genome, Occupant};
    use rand::SeedableRng;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU32, AtomicU64};

    fn unique_tmp(name: &str) -> PathBuf {
        use std::sync::atomic::AtomicU32;
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let pid = std::process::id();
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("ca_persist_{}_{}_{}", name, pid, n))
    }

    fn cleanup(path: &Path) {
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(with_extension_suffix(path, "tmp"));
        let _ = std::fs::remove_file(with_extension_suffix(path, "bak"));
    }

    fn empty_chunk(cx: i32, cy: i32) -> Chunk {
        let cells = (0..CHUNK_AREA)
            .map(|_| Cell {
                organic: 0,
                soil_energy: 0,
                sunlit: false,
                lineage_mutation_rate: 0,
                occupant: Occupant::Empty,
            })
            .collect();
        Chunk {
            coord: ChunkCoord { x: cx, y: cy },
            cells,
        }
    }

    fn make_snapshot() -> WorldSnapshot {
        let mut chunks = vec![empty_chunk(0, 0)];
        chunks[0].cells[0].organic = 123;
        chunks[0].cells[5].occupant = Occupant::Sprout {
            plant: 7,
            clan: 0,
            energy: 250,
            facing: Direction::East,
            genome: Box::new(Genome::default_vine()),
            parent: Some(Direction::West),
            current_gene: 4,
        };
        WorldSnapshot {
            chunks_x: 1,
            chunks_y: 1,
            next_plant_id: 42,
            current_tick: 99,
            chunks,
            seed: Some(0xABCD_1234),
            rng: Some(rand_chacha::ChaCha12Rng::seed_from_u64(0xABCD_1234)),
        }
    }

    fn build_state(snap: &WorldSnapshot) -> SimState {
        let (tx, _rx) = tokio::sync::broadcast::channel(8);
        let seed = snap.seed.unwrap_or(0);
        SimState {
            chunks_x: snap.chunks_x,
            chunks_y: snap.chunks_y,
            world: Mutex::new(snap.chunks.clone()),
            tick_tx: tx,
            next_plant_id: AtomicU32::new(snap.next_plant_id),
            current_tick: AtomicU64::new(snap.current_tick),
            control: Mutex::new(SimControl {
                paused: true,
                tick_hz: 10,
                tick_rate_limited: false,
                step_pending: 0,
            }),
            seed: AtomicU64::new(seed),
            rng: Mutex::new(
                snap.rng
                    .clone()
                    .unwrap_or_else(|| rand_chacha::ChaCha12Rng::seed_from_u64(seed)),
            ),
        }
    }

    #[test]
    fn snapshot_round_trips_through_save_and_load() {
        let path = unique_tmp("roundtrip");
        cleanup(&path);

        let snap = make_snapshot();
        let state = build_state(&snap);
        save_world(&path, &state).expect("save");

        let loaded = load_world(&path).expect("load");
        assert_eq!(loaded.chunks_x, snap.chunks_x);
        assert_eq!(loaded.chunks_y, snap.chunks_y);
        assert_eq!(loaded.next_plant_id, snap.next_plant_id);
        assert_eq!(loaded.current_tick, snap.current_tick);
        assert_eq!(loaded.chunks.len(), 1);
        assert_eq!(loaded.chunks[0].cells[0].organic, 123);
        match &loaded.chunks[0].cells[5].occupant {
            Occupant::Sprout {
                plant,
                energy,
                current_gene,
                genome,
                ..
            } => {
                assert_eq!(*plant, 7);
                assert_eq!(*energy, 250);
                assert_eq!(*current_gene, 4);
                assert_eq!(genome.genes.len(), protocol::GENOME_LEN);
            }
            other => panic!("expected sprout, got {other:?}"),
        }

        cleanup(&path);
    }

    #[test]
    fn save_writes_zstd_magic_header() {
        let path = unique_tmp("magic");
        cleanup(&path);

        let snap = make_snapshot();
        let state = build_state(&snap);
        save_world(&path, &state).expect("save");

        let raw = std::fs::read(&path).expect("read");
        assert_eq!(&raw[..4], &ZSTD_MAGIC, "save output should be zstd-framed");
        cleanup(&path);
    }

    #[test]
    fn save_rotates_existing_world_into_bak() {
        let path = unique_tmp("rotate");
        cleanup(&path);

        let snap = make_snapshot();
        let state = build_state(&snap);
        save_world(&path, &state).expect("first save");
        // Mutate tick to make the second save distinguishable.
        state.current_tick.store(101, Ordering::Relaxed);
        save_world(&path, &state).expect("second save");

        let bak = with_extension_suffix(&path, "bak");
        assert!(bak.exists(), "expected backup at {bak:?}");
        let live = load_world(&path).expect("load live");
        assert_eq!(live.current_tick, 101);
        let backup = load_world(&bak).expect("load bak");
        assert_eq!(backup.current_tick, 99);

        cleanup(&path);
    }

    #[test]
    fn load_handles_uncompressed_msgpack() {
        let path = unique_tmp("uncompressed");
        cleanup(&path);

        let snap = make_snapshot();
        let raw = rmp_serde::to_vec(&snap).expect("encode");
        std::fs::write(&path, &raw).expect("write");
        let loaded = load_world(&path).expect("load");
        assert_eq!(loaded.chunks_x, snap.chunks_x);
        assert_eq!(loaded.next_plant_id, snap.next_plant_id);

        cleanup(&path);
    }

    #[test]
    fn load_or_build_returns_default_when_path_missing() {
        // No path provided → build from defaults.
        let snap = load_or_build(None, 2, 2).expect("build default");
        assert_eq!(snap.chunks_x, 2);
        assert_eq!(snap.chunks_y, 2);
        assert_eq!(snap.next_plant_id, 1);
        assert_eq!(snap.current_tick, 0);

        // Path that doesn't exist → also build, not error.
        let nowhere = unique_tmp("doesnotexist");
        cleanup(&nowhere);
        let snap = load_or_build(Some(&nowhere), 1, 1).expect("missing path");
        assert_eq!(snap.chunks_x, 1);
        assert_eq!(snap.next_plant_id, 1);
    }

    #[test]
    fn with_extension_suffix_appends_dot_and_label() {
        let p = Path::new("/tmp/world.bin");
        assert_eq!(
            with_extension_suffix(p, "tmp"),
            PathBuf::from("/tmp/world.bin.tmp")
        );
        assert_eq!(
            with_extension_suffix(p, "bak"),
            PathBuf::from("/tmp/world.bin.bak")
        );
    }
}
