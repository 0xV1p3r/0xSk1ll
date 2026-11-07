//! Fetching imports from git (via the `git` binary, see [`crate::git`]).

use std::fs;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::git::Git;
use crate::hash::hex;
use crate::manifest::Import;
use crate::{Error, IoExt, Result};

/// Expands `github:owner/repo` shorthand; anything else is passed to git as-is.
pub fn resolve_url(source: &str) -> String {
    match source.strip_prefix("github:") {
        Some(rest) => format!("https://github.com/{}.git", rest.trim_end_matches(".git")),
        None => source.to_string(),
    }
}

/// Fetches `import` and replaces `dest` with the skill directory at the
/// resolved commit. Returns the commit SHA.
pub fn fetch_into(import: &Import, dest: &Path) -> Result<String> {
    let url = resolve_url(&import.source);
    let cache = cache_dir_for(&url);
    if cache.join(".git").is_dir() {
        git(&cache, &["fetch", "--quiet", "--tags", "--force", "--prune", "origin"], &[])?;
    } else {
        let parent = cache.parent().expect("cache dir has a parent");
        fs::create_dir_all(parent).at(parent)?;
        let cache_str = cache.to_string_lossy();
        git(parent, &["clone", "--quiet", "--no-checkout", &url, &cache_str], &[])?;
    }
    let commit = resolve_commit(&cache, &import.git_ref)?;

    let parent = dest.parent().ok_or_else(|| Error::Invalid("import destination has no parent".into()))?;
    fs::create_dir_all(parent).at(parent)?;
    let stem = dest.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let stage = parent.join(format!(".sk1ll-stage-{stem}"));
    let index = parent.join(format!(".sk1ll-stage-{stem}.index"));

    let result = (|| {
        if stage.exists() {
            fs::remove_dir_all(&stage).at(&stage)?;
        }
        fs::create_dir_all(&stage).at(&stage)?;
        let sub = import.path.trim_matches('/');
        let pathspec = if sub.is_empty() { "." } else { sub };
        // A throwaway index + work tree so the cache repo itself is never checked out.
        git(
            &cache,
            &["checkout", "--quiet", &commit, "--", pathspec],
            &[("GIT_WORK_TREE", stage.as_path()), ("GIT_INDEX_FILE", index.as_path())],
        )?;
        let src = if sub.is_empty() { stage.clone() } else { stage.join(sub) };
        if !src.join("SKILL.md").is_file() {
            return Err(Error::Invalid(format!(
                "no SKILL.md at `{pathspec}` in {} @ {}",
                import.source, import.git_ref
            )));
        }
        if dest.exists() {
            fs::remove_dir_all(dest).at(dest)?;
        }
        fs::rename(&src, dest).at(dest)
    })();

    let _ = fs::remove_dir_all(&stage);
    let _ = fs::remove_file(&index);
    result.map(|()| commit)
}

fn resolve_commit(repo: &Path, git_ref: &str) -> Result<String> {
    // Prefer the remote-tracking branch so `main` means the fetched `origin/main`.
    for candidate in [format!("origin/{git_ref}"), git_ref.to_string()] {
        let spec = format!("{candidate}^{{commit}}");
        if let Ok(sha) = git(repo, &["rev-parse", "--verify", "--quiet", &spec], &[]) {
            return Ok(sha);
        }
    }
    Err(Error::Invalid(format!("ref `{git_ref}` not found")))
}

fn cache_dir_for(url: &str) -> PathBuf {
    let digest = hex(&Sha256::digest(url.as_bytes()));
    dirs::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("sk1ll")
        .join("git")
        .join(&digest[..16])
}

fn git(dir: &Path, args: &[&str], env: &[(&str, &Path)]) -> Result<String> {
    env.iter().fold(Git::new(dir).args(args), |g, (k, v)| g.env(k, v)).text()
}
