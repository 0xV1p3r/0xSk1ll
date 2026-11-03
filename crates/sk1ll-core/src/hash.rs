use std::fs;
use std::path::Path;

use sha2::{Digest, Sha256};
use walkdir::WalkDir;

use crate::sync::MARKER;
use crate::{IoExt, Result};

/// Stable content hash of a skill directory (file paths + contents), ignoring
/// `.git` and sk1ll's own marker file.
pub fn hash_dir(dir: &Path) -> Result<String> {
    let mut hasher = Sha256::new();
    let walker = WalkDir::new(dir)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|e| e.depth() == 0 || (e.file_name() != ".git" && e.file_name() != MARKER));
    for entry in walker {
        let entry = entry.map_err(std::io::Error::from).at(dir)?;
        let rel = entry.path().strip_prefix(dir).unwrap_or(entry.path());
        let rel = rel.to_string_lossy().replace('\\', "/");
        let ft = entry.file_type();
        let (tag, bytes) = if ft.is_symlink() {
            let to = fs::read_link(entry.path()).at(entry.path())?;
            (b'l', to.to_string_lossy().into_owned().into_bytes())
        } else if ft.is_file() {
            (b'f', fs::read(entry.path()).at(entry.path())?)
        } else {
            continue;
        };
        hasher.update([tag]);
        hasher.update(rel.as_bytes());
        hasher.update([0]);
        hasher.update((bytes.len() as u64).to_le_bytes());
        hasher.update(&bytes);
    }
    Ok(format!("sha256:{}", hex(&hasher.finalize())))
}

pub fn hash_str(text: &str) -> String {
    format!("sha256:{}", hex(&Sha256::digest(text.as_bytes())))
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
