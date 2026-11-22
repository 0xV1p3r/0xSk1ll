//! The Snapshot: what the local API's readers of reconciled state (the
//! Draft, the inventory, the conflicts, the recovery area) answer from. Each
//! reconcile builds a new one and publishes it whole, before the events
//! that tell clients to ask again; a reader takes the current `Arc` and
//! never waits for the engine mutex, so a long reconcile or mutation delays
//! no read: readers get the Snapshot of the last reconcile meanwhile.

use std::sync::Arc;

use crate::client::{ConflictView, DraftEntry, ItemView};
use crate::error::{Error, Result};

/// One part of a Snapshot, or why the reconcile could not read it.
pub(crate) type Part<T> = std::result::Result<T, Error>;

pub(crate) struct Snapshot {
    /// How many reconciles published a Snapshot before this one.
    pub(crate) generation: u64,
    pub(crate) draft: Part<Vec<DraftEntry>>,
    pub(crate) items: Part<Vec<ItemView>>,
    /// The plan's conflicts and the recovery entries written to since.
    pub(crate) conflicts: Part<Vec<ConflictView>>,
    /// Every recovery entry.
    pub(crate) recovered: Part<Vec<ConflictView>>,
}

impl Snapshot {
    /// What readers see before the first reconcile.
    pub(super) fn initial() -> Arc<Self> {
        fn pending<T>() -> Part<T> {
            Err(Error::Unavailable("the daemon has not read the repo yet; try again".into()))
        }
        Arc::new(Self { generation: 0, draft: pending(), items: pending(), conflicts: pending(), recovered: pending() })
    }

    /// What readers see while a standalone daemon's repo is missing (moved
    /// or removed): `error` for every part, as for every other request
    /// about the repo, never the items of the repo that was there.
    pub(super) fn without_repo(generation: u64, error: fn() -> Error) -> Arc<Self> {
        Arc::new(Self {
            generation,
            draft: Err(error()),
            items: Err(error()),
            conflicts: Err(error()),
            recovered: Err(error()),
        })
    }

    pub(crate) fn draft(&self) -> Result<Vec<DraftEntry>> {
        read(&self.draft)
    }

    pub(crate) fn items(&self) -> Result<Vec<ItemView>> {
        read(&self.items)
    }

    pub(crate) fn conflicts(&self) -> Result<Vec<ConflictView>> {
        read(&self.conflicts)
    }

    pub(crate) fn recovered(&self) -> Result<Vec<ConflictView>> {
        read(&self.recovered)
    }
}

fn read<T: Clone>(part: &Part<T>) -> Result<T> {
    match part {
        Ok(value) => Ok(value.clone()),
        Err(e) => Err(e.duplicate()),
    }
}
