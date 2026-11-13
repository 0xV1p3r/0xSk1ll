//! Core logic for 0xSk1ll: manifest, lockfile, imports, machine-local harness
//! config, assignment, and syncing skills and global instructions into
//! harnesses. Front-ends (CLI, GUI, daemon) only call into this crate; nothing
//! here prints, and public types are `Serialize` so they can cross an
//! IPC boundary (e.g. Tauri commands) unchanged.

pub mod assignment;
pub mod content;
pub mod draft;
pub mod engine;
pub mod git;
pub mod hash;
pub mod imports;
pub mod instructions;
pub mod lock;
pub mod machine;
pub mod manifest;
pub mod placement;
pub mod repo;
pub mod skill;
pub mod source;
pub mod sync;

use std::path::{Path, PathBuf};

pub use assignment::{Assignment, HarnessAssignment};
pub use draft::{Change, DraftItem, FLEET_BRANCH, ItemRef, PullOutcome, default_commit_message, machine_author};
pub use engine::EngineLock;
pub use imports::{
    FetchedLink, FetchedUpdates, ImportState, ImportStatus, ResolvedSource, SkillCandidate, UpdateReport, UpdateTargets,
};
pub use instructions::Rule;
pub use lock::{LockEntry, Lockfile};
pub use machine::{
    EffectiveHarness, Harness, HarnessEntry, HarnessOrigin, InstructionsFormat, LinkMode, MachineConfig,
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
    #[error("{0}")]
    Invalid(String),
    /// Sync replaced or removed `dest`, but the content it displaced was
    /// written to while that happened. Nothing is lost: that content is kept,
    /// never pruned, in the recovery area at `kept` (see [`sync::displaced`])
    /// until it is adopted or discarded by `id`.
    #[error(
        "{} was written to while sync replaced it; that version is kept at {} (adopt or discard displaced \
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
        (Some(rest), Some(home)) => home.join(rest),
        _ if path == "~" => dirs::home_dir().unwrap_or_else(|| path.into()),
        _ => PathBuf::from(path),
    }
}
