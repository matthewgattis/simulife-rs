use std::{
    path::{Path, PathBuf},
    sync::atomic::Ordering,
};

use anyhow::{Context, Result};
use protocol::Chunk;
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
    pub chunks: Vec<Chunk>,
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
        chunks: state.world.lock().expect("sim lock poisoned").clone(),
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
    let mut chunks = world::build_world(world_width, world_height);
    world::place_showcase(&mut chunks, world_width);
    Ok(WorldSnapshot {
        chunks_x: world_width,
        chunks_y: world_height,
        next_plant_id: 1,
        chunks,
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
