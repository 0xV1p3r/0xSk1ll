//! Core logic for 0xSk1ll: manifest, lockfile, imports, machine-local harness
//! config, assignment, and syncing skills and global instructions into
//! harnesses. Front-ends (CLI, GUI, daemon) only call into this crate; nothing
//! here prints, and public types are `Serialize` so they can cross an
//! IPC boundary (e.g. Tauri commands) unchanged.

pub mod assignment;
pub mod baseline_check;
pub mod bundled;
pub mod config_file;
pub mod content;
pub mod draft;
pub mod engine;
#[cfg(any(test, feature = "failpoints"))]
pub mod failpoints;
pub mod git;
pub mod harness_change;
pub mod harness_rename;
pub mod hash;
pub mod imports;
pub mod instructions;
pub mod item_id;
pub mod lock;
pub mod machine;
pub mod manifest;
pub mod paths;
pub mod placement;
pub mod real_path;
pub mod redact;
pub mod repo;
pub mod skill;
pub mod source;
pub mod sync;

use std::path::{Path, PathBuf};

/// Reaches a [`failpoints::Point`] (by its variant name) about `path`. A
/// statement; compiles to nothing without the `failpoints` feature.
#[cfg(any(test, feature = "failpoints"))]
#[doc(hidden)]
#[macro_export]
macro_rules! failpoint {
    ($point:ident, $path:expr) => {{
        let _ = $crate::failpoints::hit($crate::failpoints::Point::$point, $path);
    }};
}

#[cfg(not(any(test, feature = "failpoints")))]
#[doc(hidden)]
#[macro_export]
macro_rules! failpoint {
    ($point:ident, $path:expr) => {{
        if false {
            let _ = $path;
        }
    }};
}

/// Reaches a [`failpoints::Point`] that [can fail](failpoints::Point::can_fail):
/// the `std::io::Error` a test injects there, if any. Always `None` without
/// the `failpoints` feature.
#[cfg(any(test, feature = "failpoints"))]
#[doc(hidden)]
#[macro_export]
macro_rules! failpoint_error {
    ($point:ident, $path:expr) => {
        $crate::failpoints::hit($crate::failpoints::Point::$point, $path)
    };
}

#[cfg(not(any(test, feature = "failpoints")))]
#[doc(hidden)]
#[macro_export]
macro_rules! failpoint_error {
    ($point:ident, $path:expr) => {{
        if false {
            let _ = $path;
        }
        None::<::std::io::Error>
    }};
}

/// This build's version and commit, e.g. `0.1.0 (1a2b3c4d5e)`.
pub const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), " (", env!("SK1LL_COMMIT"), ")");

pub use assignment::{Assignment, HarnessAssignment};
pub use draft::{Change, DraftItem, FLEET_BRANCH, ItemRef, PullOutcome, default_commit_message, machine_author};
pub use engine::EngineLock;
pub use imports::{
    FetchedLink, FetchedUpdates, ImportState, ImportStatus, ResolvedSource, SkillCandidate, UpdateReport, UpdateTargets,
};
pub use instructions::Rule;
pub use item_id::ItemId;
pub use lock::{LockEntry, Lockfile};
pub use machine::{
    EffectiveHarness, Harness, HarnessCandidate, HarnessChange, HarnessChoice, HarnessEntry, HarnessOrigin,
    InstructionsFormat, LinkMode, MachineConfig,
};
pub use manifest::{Import, Manifest};
pub use repo::Repo;
pub use skill::{Origin, Skill};
pub use sync::{Action, ActionKind, Item, PlanOptions, RenderedFile};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse {}: {message}", path.display())]
    Parse { path: PathBuf, message: String },
    #[error("`git {args}` failed: {stderr}")]
    Git { args: String, stderr: String },
    #[error("duplicate Item ID `{id}` at {} and {}", first.display(), second.display())]
    DuplicateItemId { id: ItemId, first: PathBuf, second: PathBuf },
    #[error("interrupted rename left an item staged at {}. Move it to its intended name before retrying", path.display())]
    StrandedRename { path: PathBuf },
    #[error("{0}")]
    Invalid(String),
    /// A pull stopped half-way (its journal is still there): its tree is
    /// partial, so nothing that writes Item IDs, syncs or pushes may run
    /// until the pull is finished.
    #[error(
        "a pull stopped half-way and has to finish first: restore what blocks it (copy edits you want to keep aside \
         first), then run `sk1ll pull`"
    )]
    PullPending,
    /// The Baseline a pull would move onto (`baseline`) writes `paths`
    /// through a git filter driver, which the skills repo doesn't support
    /// (the server has no LFS endpoint). The pull changed nothing; `items`:
    /// those `paths` belong to.
    #[error(
        "the skills repo uses a git filter driver (e.g. Git LFS) on {}; sk1ll doesn't support filter drivers in the \
         skills repo — remove `filter=` from .gitattributes",
        paths.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", ")
    )]
    FilterDriver { baseline: String, paths: Vec<PathBuf>, items: Vec<ItemRef> },
    /// What an operation was planned from changed before it was made, so
    /// nothing was done: look again (re-read, preview again) and retry.
    #[error("{0}")]
    Changed(String),
    /// Sync (or pull, in the skills repo) replaced or removed `dest`, but the
    /// content it displaced was written to while that happened. Nothing is
    /// lost: that content is kept, never pruned, in the recovery area at
    /// `kept` (see [`sync::displaced`]) until it is adopted or discarded by `id`.
    #[error(
        "{} was written to while sk1ll replaced it; that version is kept at {} (adopt or discard displaced \
         entry `{id}`)",
        dest.display(),
        kept.display()
    )]
    Displaced { dest: PathBuf, kept: PathBuf, id: String },
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

pub(crate) trait IoExt<T> {
    fn at(self, path: impl AsRef<Path>) -> Result<T>;
}

impl<T> IoExt<T> for std::io::Result<T> {
    fn at(self, path: impl AsRef<Path>) -> Result<T> {
        self.map_err(|source| Error::Io { path: path.as_ref().to_path_buf(), source })
    }
}

pub(crate) fn expand_tilde(path: &str) -> PathBuf {
    match (path.strip_prefix("~/"), dirs::home_dir()) {
        // `~//x` is `x` under the home directory, never the absolute `/x`.
        (Some(rest), Some(home)) => home.join(rest.trim_start_matches('/')),
        _ if path == "~" => dirs::home_dir().unwrap_or_else(|| path.into()),
        _ => PathBuf::from(path),
    }
}
