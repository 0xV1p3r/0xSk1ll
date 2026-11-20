use std::fmt;
use std::io::Read;
use std::path::Path;

use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest, Sha256};

use crate::Result;
use crate::skill::ID_FILE;

/// The stable identity of a personal skill, Rule or Import.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct ItemId(String);

impl ItemId {
    pub(crate) fn generate() -> Self {
        Self(ulid::Ulid::generate().to_string())
    }

    pub(crate) fn derived(kind: &str, name: &str) -> Self {
        let mut hash = Sha256::new();
        hash.update(kind);
        hash.update(b"\0");
        hash.update(name);
        Self(format!("d:{:x}", hash.finalize()))
    }

    pub(crate) fn from_text(text: &str) -> Option<Self> {
        let id = text.trim();
        (!id.is_empty() && !id.contains(['\r', '\n', '\0'])).then(|| Self(id.into()))
    }

    pub(crate) fn read_skill(dir: &Path) -> Option<Self> {
        // Treat only a regular one-line file as explicit metadata.
        let crate::config_file::Entry::File(file, _) = crate::config_file::open_entry(&dir.join(ID_FILE)).ok()? else {
            return None;
        };
        let mut text = String::new();
        let limit = crate::content::MAX_FILE_BYTES;
        file.take(limit + 1).read_to_string(&mut text).ok()?;
        (text.len() as u64 <= limit).then(|| Self::from_text(&text)).flatten()
    }

    pub(crate) fn write_skill(&self, dir: &Path) -> Result<()> {
        let path = dir.join(ID_FILE);
        crate::config_file::Staged::write(&path, format!("{self}\n").as_bytes(), None)?.replace()
    }
}

impl<'de> Deserialize<'de> for ItemId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Self::from_text(&text).ok_or_else(|| serde::de::Error::custom("id must be a nonempty, single-line string"))
    }
}

impl fmt::Display for ItemId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}
