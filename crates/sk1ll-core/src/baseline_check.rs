//! What a tree must pass to become the Baseline: every item (personal
//! skill, Rule, Import) has an Item ID of its own, and no file goes through
//! a git filter driver (the skills repo supports none, see
//! [`crate::git::filtered_paths`]). The server's pre-receive check refuses a
//! push by these rules (ADR 0006), and a commit checks its tree by them
//! first, so what the server would refuse for good is never committed.

use std::collections::BTreeSet;
use std::path::Path;

use crate::git::{RepoGit, filtered_paths, split_paths};
use crate::item_id::IdProblems;
use crate::{Error, Result};

/// Why a tree can't be the Baseline, as the server and a commit say it:
/// `ids`, from [`crate::item_id::ItemInventory::id_problems`], and the
/// files a filter driver would write (`filtered`). `None`: it can.
pub fn refusal(ids: Option<&IdProblems>, filtered: &[String]) -> Option<String> {
    let mut reasons = Vec::new();
    if let Some(ids) = ids {
        reasons.push(ids.to_string());
    }
    if !filtered.is_empty() {
        let mut named = filtered.iter().take(5).cloned().collect::<Vec<_>>().join(", ");
        if filtered.len() > 5 {
            named.push_str(&format!(" and {} more", filtered.len() - 5));
        }
        reasons.push(format!(
            "the skills repo uses a git filter driver (e.g. Git LFS) on {named}; sk1ll doesn't support filter drivers \
             in the skills repo — remove `filter=` from .gitattributes"
        ));
    }
    (!reasons.is_empty()).then(|| reasons.join("; "))
}

/// [`refusal`] for the tree (or commit) `tree` of the repo at `root`, read
/// as the server reads a pushed commit: its own `.gitattributes`, no
/// driver configured. Items without an Item ID count only where `missing`
/// says so, given the item and where its claim is (see
/// [`IdProblems::retain_missing`]): a commit answers for what it commits,
/// and the Baseline under it has its IDs (the server took it).
pub fn check_tree(
    root: &Path,
    tree: &str,
    missing: impl Fn(&crate::ItemRef, Option<&str>) -> bool,
) -> Result<Option<String>> {
    let git = RepoGit::with_env(root, &[("GIT_OPTIONAL_LOCKS".into(), "0".into())])?;
    // The skills repo may be a subdirectory of the worktree `tree` is of.
    let skills = git.git().args(["rev-parse", "--verify", "--end-of-options", &format!("{tree}:./")]).text()?;
    let listed = git.git().args(["ls-tree", "-r", "-z", "--name-only", &skills]).run()?;
    let paths: Vec<String> = split_paths(&listed).map(|p| p.to_string_lossy().into_owned()).collect();
    let ids = crate::item_id::committed_inventory(root, &skills, &paths);
    let whole = git.git().args(["ls-tree", "-r", "-z", "--name-only", "--full-tree", tree]).run()?;
    let filtered = if whole.is_empty() { Vec::new() } else { tree_filtered(&git, tree, whole)? };
    let ids = ids.id_problems().and_then(|problems| problems.retain_missing(missing));
    Ok(refusal(ids.as_ref(), &filtered))
}

/// The files of `tree` (listed `-z` in `paths`) its own `.gitattributes`
/// give a filter driver, read as the server reads them: in a bare repo of
/// its own that borrows this one's objects, so neither `info/attributes`
/// nor any configuration of this repo or user counts.
fn tree_filtered(git: &RepoGit, tree: &str, paths: Vec<u8>) -> Result<Vec<String>> {
    let objects = git.git().args(["rev-parse", "--path-format=absolute", "--git-path", "objects"]).text()?;
    let tree = git.git().args(["rev-parse", "--verify", "--end-of-options", &format!("{tree}^{{tree}}")]).text()?;
    let bare = tempfile::tempdir().map_err(|e| Error::Invalid(format!("creating a scratch repo: {e}")))?;
    let isolated = |args: &[&str]| {
        crate::git::Git::new(bare.path())
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_ATTR_NOSYSTEM", "1")
            .env("GIT_ALTERNATE_OBJECT_DIRECTORIES", &objects)
            .args(args)
    };
    isolated(&["init", "--quiet", "--bare", "--template="]).run()?;
    let source = format!("--source={tree}");
    let out = isolated(&["check-attr", &source, "-z", "--stdin", "filter"]).stdin(paths).run()?;
    Ok(filtered_paths(&out, &BTreeSet::new()).iter().map(|p| p.to_string_lossy().into_owned()).collect())
}
