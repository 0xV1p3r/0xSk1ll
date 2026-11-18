//! `draft_discarded` messages not yet delivered to the server, kept in a small
//! private file next to the state file so a restart doesn't lose them. An
//! entry is removed only after the server confirms it was applied; sending one twice is harmless
//! because the server handles them idempotently.
//!
//! The file names the registration that queued it. Restores apply to the
//! toggles of that registration only, so a queue left by an earlier join is
//! set aside (renamed, not deleted) instead of being replayed under new
//! credentials.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use sk1ll_proto::ItemRef;

use crate::state::{DaemonState, write_private};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Entry {
    pub(crate) id: u64,
    pub(crate) items: Vec<ItemRef>,
}

#[derive(Serialize, Deserialize)]
struct File {
    registration: String,
    entries: Vec<Entry>,
}

/// The file as written before it named its registration.
#[derive(Deserialize)]
#[serde(untagged)]
enum Stored {
    Bound(File),
    Legacy(Vec<Entry>),
}

#[derive(Debug)]
pub(crate) struct Outbox {
    path: PathBuf,
    registration: String,
    entries: Vec<Entry>,
}

impl Outbox {
    pub(crate) fn empty(path: PathBuf) -> Self {
        Self { path, registration: String::new(), entries: Vec::new() }
    }

    /// Loads `path` for `state`'s registration; a missing file is an empty
    /// outbox, an unreadable one is logged and replaced, and one queued by
    /// another registration is moved aside.
    pub(crate) fn load(path: PathBuf, state: &DaemonState) -> Self {
        let registration = registration(state);
        let entries = match std::fs::read_to_string(&path) {
            Ok(text) => match serde_json::from_str(&text) {
                Ok(Stored::Bound(f)) if f.registration == registration => f.entries,
                Ok(Stored::Bound(f)) => {
                    set_aside(&path, &f.registration);
                    Vec::new()
                }
                // Older daemons didn't record the registration; assume it is this one.
                Ok(Stored::Legacy(entries)) => entries,
                Err(e) => {
                    tracing::warn!("ignoring unreadable {}: {e}", path.display());
                    Vec::new()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => {
                tracing::warn!("reading {}: {e}", path.display());
                Vec::new()
            }
        };
        Self { path, registration, entries }
    }

    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    /// Queues `items` and persists the queue.
    pub(crate) fn push(&mut self, items: Vec<ItemRef>) -> anyhow::Result<()> {
        let id = self.entries.iter().map(|e| e.id).max().map_or(1, |m| m + 1);
        self.entries.push(Entry { id, items });
        self.save()
    }

    pub(crate) fn unsent(&self, sent: &std::collections::HashSet<u64>) -> Option<Entry> {
        self.entries.iter().find(|e| !sent.contains(&e.id)).cloned()
    }

    #[cfg(test)]
    pub(crate) fn front(&self) -> Option<Entry> {
        self.entries.first().cloned()
    }

    /// Drops a delivered entry and persists the queue.
    pub(crate) fn remove(&mut self, id: u64) -> anyhow::Result<()> {
        self.entries.retain(|e| e.id != id);
        self.save()
    }

    fn save(&self) -> anyhow::Result<()> {
        if self.entries.is_empty() {
            match std::fs::remove_file(&self.path) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
                _ => return Ok(()),
            }
        }
        let file = File { registration: self.registration.clone(), entries: self.entries.clone() };
        write_private(&self.path, serde_json::to_string_pretty(&file)?.as_bytes())
    }
}

/// Identifies a join without storing its token: FNV-1a over the server, the
/// machine ID and the token. Only compared for equality, and stable across
/// builds, unlike `std`'s hasher.
fn registration(state: &DaemonState) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for part in [&state.server_url, &state.machine_id, &state.token] {
        for b in part.bytes().chain([0]) {
            hash = (hash ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3);
        }
    }
    format!("{hash:016x}")
}

fn set_aside(path: &std::path::Path, registration: &str) {
    let aside = path.with_extension(format!("{registration}.stale.json"));
    match std::fs::rename(path, &aside) {
        Ok(()) => tracing::warn!(
            "{} was queued by an earlier registration; moved it to {} without sending it",
            path.display(),
            aside.display()
        ),
        Err(e) => tracing::warn!("setting aside {}: {e}", path.display()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn joined(token: &str) -> DaemonState {
        DaemonState {
            machine_id: "m".into(),
            server_url: "https://server.example".into(),
            token: token.into(),
            repo: "/repo".into(),
            revoked: false,
        }
    }

    #[test]
    fn survives_a_reload_until_delivered() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("outbox.json");
        let state = joined("t");
        let mut o = Outbox::load(path.clone(), &state);
        o.push(vec![ItemRef::Skill("a".into())]).unwrap();
        o.push(vec![ItemRef::Rule("b".into())]).unwrap();
        let mut o = Outbox::load(path.clone(), &state);
        assert_eq!(o.len(), 2);
        let first = o.front().unwrap();
        assert_eq!(first.items, [ItemRef::Skill("a".into())]);
        o.remove(first.id).unwrap();
        let mut o = Outbox::load(path.clone(), &state);
        assert_eq!(o.front().unwrap().items, [ItemRef::Rule("b".into())]);
        let id = o.front().unwrap().id;
        o.remove(id).unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn another_registrations_queue_is_set_aside() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("outbox.json");
        Outbox::load(path.clone(), &joined("old")).push(vec![ItemRef::Skill("a".into())]).unwrap();
        let o = Outbox::load(path.clone(), &joined("new"));
        assert_eq!(o.len(), 0);
        assert!(!path.exists());
        // Kept for inspection, without the token.
        let aside: Vec<_> = std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().path()).collect();
        assert_eq!(aside.len(), 1);
        assert!(!std::fs::read_to_string(&aside[0]).unwrap().contains("old"));
    }

    #[test]
    fn a_legacy_queue_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("outbox.json");
        std::fs::write(&path, r#"[{"id": 3, "items": ["skill:a"]}]"#).unwrap();
        let o = Outbox::load(path, &joined("t"));
        assert_eq!(o.front().unwrap().id, 3);
    }
}
