use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::instructions::{self, Fragment, INSTRUCTIONS_DIR};
use crate::lock::Lockfile;
use crate::manifest::{MANIFEST_FILE, Manifest};
use crate::skill::{self, IMPORTED_DIR, PERSONAL_DIR, Skill};
use crate::{Error, IoExt, Result};

const MANIFEST_TEMPLATE: &str = r#"# 0xSk1ll manifest.
# Personal skills live in personal/<name>/SKILL.md.
# Imports are vendored into imported/<name> and pinned in sk1ll.lock.
# Project-agnostic instructions live in instructions/*.md and are rendered
# into each harness's global instructions file.

# Harnesses. Adjust to the ones you use; all keys are optional.
# instructions_format:
#   "block" (default): one marked block inside a single file; the rest of
#                      the file is left alone.
#   "rules":           one copied file per fragment in a directory.
[targets.claude]
skills = "~/.claude/skills"
instructions = "~/.claude/rules"
instructions_format = "rules"
# mode = "copy"   # how skills are placed; default: "symlink"

# [targets.codex]
# skills = "~/.codex/skills"
# instructions = "~/.codex/AGENTS.md"

# Profiles pick skills, instructions (globs) and targets per machine:
# --profile / SK1LL_PROFILE.
#   exclude_skills = []
#   exclude_instructions = []
#   targets = ["claude"]   # empty = all targets
[profiles.default]
skills = ["*"]
instructions = ["*"]
"#;

/// A skills repository on disk (`sk1ll.toml` + `personal/` + `imported/` +
/// `instructions/`).
#[derive(Debug, Clone)]
pub struct Repo {
    root: PathBuf,
    pub manifest: Manifest,
    pub lock: Lockfile,
}

impl Repo {
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref();
        if !root.join(MANIFEST_FILE).is_file() {
            return Err(Error::Invalid(format!(
                "no {MANIFEST_FILE} in {} (run `sk1ll init`?)",
                root.display()
            )));
        }
        let root = root.canonicalize().at(root)?;
        Ok(Self { manifest: Manifest::load(&root)?, lock: Lockfile::load(&root)?, root })
    }

    /// Nearest ancestor of `cwd` containing `sk1ll.toml`, else `~/skills`.
    pub fn locate(cwd: &Path) -> Option<PathBuf> {
        cwd.ancestors()
            .find(|d| d.join(MANIFEST_FILE).is_file())
            .map(Path::to_path_buf)
            .or_else(|| dirs::home_dir().map(|h| h.join("skills")))
    }

    /// Creates the repo layout and a starter manifest, and `git init`s it.
    pub fn init(root: &Path) -> Result<Self> {
        let manifest = root.join(MANIFEST_FILE);
        if manifest.exists() {
            return Err(Error::Invalid(format!("{} already exists", manifest.display())));
        }
        for sub in [PERSONAL_DIR, IMPORTED_DIR, INSTRUCTIONS_DIR] {
            let dir = root.join(sub);
            fs::create_dir_all(&dir).at(&dir)?;
            let keep = dir.join(".gitkeep");
            fs::write(&keep, "").at(&keep)?;
        }
        fs::write(&manifest, MANIFEST_TEMPLATE).at(&manifest)?;
        let ignore = root.join(".gitignore");
        if !ignore.exists() {
            fs::write(&ignore, ".sk1ll-stage-*\n").at(&ignore)?;
        }
        if !root.join(".git").exists() {
            let _ = Command::new("git").arg("init").arg("--quiet").arg(root).status();
        }
        Self::open(root)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn personal_dir(&self) -> PathBuf {
        self.root.join(PERSONAL_DIR)
    }

    pub fn imported_dir(&self) -> PathBuf {
        self.root.join(IMPORTED_DIR)
    }

    pub fn skills(&self) -> Result<Vec<Skill>> {
        skill::discover(&self.root)
    }

    pub fn fragments(&self) -> Result<Vec<Fragment>> {
        instructions::discover(&self.root)
    }
}
