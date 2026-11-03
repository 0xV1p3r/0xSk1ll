//! Core logic for 0xSk1ll: manifest, lockfile, imports, and syncing skills and
//! global instructions into harnesses. Front-ends (CLI, GUI, daemon) only call into this crate;
//! nothing here prints, and public types are `Serialize` so they can cross an
//! IPC boundary (e.g. Tauri commands) unchanged.

pub mod hash;
pub mod imports;
pub mod instructions;
pub mod lock;
pub mod manifest;
pub mod repo;
pub mod skill;
pub mod source;
pub mod sync;

use std::path::{Path, PathBuf};

pub use imports::{ImportState, ImportStatus, UpdateReport};
pub use instructions::Fragment;
pub use lock::{LockEntry, Lockfile};
pub use manifest::{Import, InstructionsFormat, LinkMode, Manifest, Profile, Target};
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
