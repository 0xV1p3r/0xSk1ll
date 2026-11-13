//! `draft_discarded` messages not yet delivered to the server, kept in a small
//! private file next to the state file so a restart doesn't lose them. An
//! entry is removed only after the server confirms it was applied; sending one twice is harmless
//! because the server handles them idempotently.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use sk1ll_proto::ItemRef;

use crate::state::write_private;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Entry {
    pub(crate) id: u64,
    pub(crate) items: Vec<ItemRef>,
}

#[derive(Debug)]
pub(crate) struct Outbox {
    path: PathBuf,
    entries: Vec<Entry>,
}

impl Outbox {
    /// Loads `path`; a missing file is an empty outbox, an unreadable one is
    /// logged and replaced.
    pub(crate) fn load(path: PathBuf) -> Self {
        let entries = match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
                tracing::warn!("ignoring unreadable {}: {e}", path.display());
                Vec::new()
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => {
                tracing::warn!("reading {}: {e}", path.display());
                Vec::new()
            }
        };
        Self { path, entries }
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
        write_private(&self.path, serde_json::to_string_pretty(&self.entries)?.as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn survives_a_reload_until_delivered() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("outbox.json");
        let mut o = Outbox::load(path.clone());
        o.push(vec![ItemRef::Skill("a".into())]).unwrap();
        o.push(vec![ItemRef::Rule("b".into())]).unwrap();
        let mut o = Outbox::load(path.clone());
        assert_eq!(o.len(), 2);
        let first = o.front().unwrap();
        assert_eq!(first.items, [ItemRef::Skill("a".into())]);
        o.remove(first.id).unwrap();
        let mut o = Outbox::load(path.clone());
        assert_eq!(o.front().unwrap().items, [ItemRef::Rule("b".into())]);
        let id = o.front().unwrap().id;
        o.remove(id).unwrap();
        assert!(!path.exists());
    }
}
