//! Persistence for the servers the viewer has connected to.
//!
//! Stored as TOML, most-recent-first:
//!
//! ```toml
//! [[server]]
//! addr = "192.168.0.10:4433"
//!
//! [[server]]
//! addr = "iapetusservers.net:4433"
//! ```
//!
//! An array of tables rather than a bare array of strings so per-entry fields
//! (a nickname, a last-connected timestamp) can be added later without
//! invalidating existing files.
//!
//! Only addresses that produced a successful connection are recorded, so the
//! list never fills up with typos.

use std::{fs, path::PathBuf};

use serde::{Deserialize, Serialize};
use tracing::warn;

/// Prefilled in the connect dialog when nothing has been saved yet. This is
/// only a suggestion — it is never auto-connected to, so a fresh install
/// still shows the dialog and waits for the user.
pub const DEFAULT_SERVER_ADDR: &str = "iapetusservers.net:4433";

/// Upper bound on remembered entries.
const MAX_HISTORY: usize = 8;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ConfigFile {
    #[serde(default, rename = "server")]
    servers: Vec<ServerEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ServerEntry {
    addr: String,
}

#[derive(Debug, Clone, Default)]
pub struct ServerHistory {
    /// `None` when the platform gave us nowhere to write; the history then
    /// works for the current session but isn't persisted.
    path: Option<PathBuf>,
    entries: Vec<String>,
}

impl ServerHistory {
    /// Reads the history, treating any failure (missing file, bad TOML,
    /// unreadable path) as "no history". A corrupt file costs the user their
    /// list, not their ability to start the viewer.
    pub fn load(path: Option<PathBuf>) -> Self {
        let entries = path
            .as_deref()
            .and_then(|p| match fs::read_to_string(p) {
                Ok(text) => Some(text),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(e) => {
                    warn!("could not read server history from {}: {e}", p.display());
                    None
                }
            })
            .and_then(|text| match toml::from_str::<ConfigFile>(&text) {
                Ok(cfg) => Some(cfg),
                Err(e) => {
                    warn!("ignoring malformed server history: {e}");
                    None
                }
            })
            .map(|cfg| {
                cfg.servers
                    .into_iter()
                    .map(|s| s.addr)
                    .filter(|a| !a.trim().is_empty())
                    .take(MAX_HISTORY)
                    .collect()
            })
            .unwrap_or_default();
        Self { path, entries }
    }

    pub fn entries(&self) -> &[String] {
        &self.entries
    }

    /// Most recently connected address, if any.
    pub fn last(&self) -> Option<&str> {
        self.entries.first().map(String::as_str)
    }

    /// Move `addr` to the front and persist. Call this only after a
    /// connection actually succeeds.
    pub fn record(&mut self, addr: &str) {
        let addr = addr.trim();
        if addr.is_empty() {
            return;
        }
        if self.last() == Some(addr) {
            // Already the most recent entry — reconnects to the same server
            // would otherwise rewrite the file on every resume.
            return;
        }
        self.entries.retain(|e| e != addr);
        self.entries.insert(0, addr.to_string());
        self.entries.truncate(MAX_HISTORY);
        self.save();
    }

    fn save(&self) {
        let Some(path) = self.path.as_deref() else {
            return;
        };
        let cfg = ConfigFile {
            servers: self
                .entries
                .iter()
                .map(|addr| ServerEntry { addr: addr.clone() })
                .collect(),
        };
        let text = match toml::to_string_pretty(&cfg) {
            Ok(t) => t,
            Err(e) => {
                warn!("could not serialize server history: {e}");
                return;
            }
        };
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        // Best effort: a viewer that can't write its history should still run.
        if let Err(e) = fs::write(path, text) {
            warn!("could not write server history to {}: {e}", path.display());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{MAX_HISTORY, ServerHistory};

    /// Path `None` exercises the ordering logic without touching disk.
    fn in_memory() -> ServerHistory {
        ServerHistory::load(None)
    }

    #[test]
    fn record_puts_newest_first() {
        let mut h = in_memory();
        h.record("a:1");
        h.record("b:2");
        assert_eq!(h.last(), Some("b:2"));
        assert_eq!(h.entries(), ["b:2", "a:1"]);
    }

    #[test]
    fn record_moves_existing_entry_to_front_without_duplicating() {
        let mut h = in_memory();
        h.record("a:1");
        h.record("b:2");
        h.record("a:1");
        assert_eq!(h.entries(), ["a:1", "b:2"]);
    }

    #[test]
    fn record_is_capped() {
        let mut h = in_memory();
        for i in 0..(MAX_HISTORY + 4) {
            h.record(&format!("host{i}:1"));
        }
        assert_eq!(h.entries().len(), MAX_HISTORY);
        // Newest survives, oldest is evicted.
        assert_eq!(
            h.last(),
            Some(format!("host{}:1", MAX_HISTORY + 3).as_str())
        );
        assert!(!h.entries().iter().any(|e| e == "host0:1"));
    }

    #[test]
    fn record_ignores_empty_and_repeated_current() {
        let mut h = in_memory();
        h.record("a:1");
        h.record("");
        h.record("   ");
        h.record("a:1");
        assert_eq!(h.entries(), ["a:1"]);
    }

    #[test]
    fn round_trips_through_a_toml_file() {
        let path = std::env::temp_dir().join(format!(
            "simulife-history-{}-{:?}.toml",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_file(&path);

        let mut h = ServerHistory::load(Some(path.clone()));
        h.record("first:4433");
        h.record("second:4433");

        let reloaded = ServerHistory::load(Some(path.clone()));
        assert_eq!(reloaded.entries(), ["second:4433", "first:4433"]);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn malformed_file_is_ignored_rather_than_fatal() {
        let path = std::env::temp_dir().join(format!(
            "simulife-history-bad-{}-{:?}.toml",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::write(&path, "this is not valid toml {{{").unwrap();

        let h = ServerHistory::load(Some(path.clone()));
        assert!(h.entries().is_empty());

        let _ = std::fs::remove_file(&path);
    }
}
