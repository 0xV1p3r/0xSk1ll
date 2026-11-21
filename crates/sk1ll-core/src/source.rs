//! Fetching imports from git (via the `git` binary, see [`crate::git`]), and
//! the links an import can be added from.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::git::Git;
use crate::hash::hex;
use crate::lock::Lockfile;
use crate::manifest::Import;
use crate::{Error, IoExt, Result};

/// A pasted link or source, before it is resolved against the remote.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Link {
    /// What the import records: `github:owner/repo` for GitHub links, else
    /// the git URL or local path as given.
    pub source: String,
    /// The (percent-decoded) segments after `tree/` or `blob/`: a ref, then a
    /// directory. Refs may contain slashes, so only the remote can tell where
    /// one ends (see [`split_ref`]). Empty: the link names neither.
    pub rest: Vec<String>,
    /// A `blob/` link: `rest` names the directory of its `SKILL.md`, which
    /// must exist at the resolved commit.
    pub blob: bool,
}

/// Parses a GitHub link (`https://github.com/owner/repo[.git]`, with
/// `/tree/<ref>/<path>` or `/blob/<ref>/<path>/SKILL.md`), the
/// `github:owner/repo[/tree/<ref>/<path>]` shorthand, or anything else as a
/// plain git URL or local path. Links are percent-decoded segment by segment;
/// the shorthand is taken literally.
pub fn parse_link(link: &str) -> Result<Link> {
    let link = link.trim();
    if link.chars().any(char::is_control) {
        return Err(Error::Invalid(format!("invalid source {link:?}: control characters")));
    }
    let prefixes = ["https://github.com/", "http://github.com/", "https://www.github.com/", "github.com/", "github:"];
    let Some((prefix, rest)) = prefixes.iter().find_map(|p| Some((*p, link.strip_prefix(p)?))) else {
        if link.is_empty() || link.starts_with('-') {
            return Err(Error::Invalid(format!("invalid source `{link}`")));
        }
        return Ok(Link { source: link.to_string(), rest: Vec::new(), blob: false });
    };
    let bad = || Error::Invalid(format!("`{link}` is not a link to a GitHub repo, folder or file"));
    let rest = rest.split(['?', '#']).next().unwrap_or_default();
    // One trailing slash is fine; any other empty segment is refused below.
    let rest = rest.strip_suffix('/').unwrap_or(rest);
    let segments: Vec<&str> = rest.split('/').collect();
    if segments.contains(&"") {
        return Err(bad());
    }
    let (owner, repo) = match segments.as_slice() {
        [owner, repo, ..] => (*owner, repo.trim_end_matches(".git")),
        _ => return Err(bad()),
    };
    let name_ok = |s: &str| {
        !matches!(s, "" | "." | "..") && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    if !name_ok(owner) || !name_ok(repo) {
        return Err(bad());
    }
    let blob = match segments.get(2) {
        None | Some(&"tree") => false,
        Some(&"blob") => true,
        _ => return Err(bad()),
    };
    let mut decoded = Vec::new();
    for segment in segments.iter().skip(3) {
        let segment = if prefix == "github:" { Some(segment.to_string()) } else { percent_decode(segment) };
        let segment = segment.ok_or_else(|| Error::Invalid(format!("bad percent-encoding in `{link}`")))?;
        // An encoded `/` separates segments like a plain one.
        for part in segment.split('/') {
            if !segment_ok(part) {
                return Err(Error::Invalid(format!("unsafe path segment `{part}` in `{link}`")));
            }
            decoded.push(part.to_string());
        }
    }
    if blob {
        if decoded.len() < 2 || decoded.last().map(String::as_str) != Some("SKILL.md") {
            return Err(Error::Invalid(format!("blob links must point at a SKILL.md: `{link}`")));
        }
        decoded.pop();
    }
    Ok(Link { source: format!("github:{owner}/{repo}"), rest: decoded, blob })
}

/// `%XX` escapes to bytes; `None` for a malformed escape or non-UTF-8 result.
fn percent_decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// One segment of a ref or repo path: not empty, `.` or `..`, and free of
/// control characters (NUL included).
fn segment_ok(s: &str) -> bool {
    !matches!(s, "" | "." | "..") && !s.chars().any(char::is_control)
}

/// A repo-relative directory given by the user: `/`-trimmed, each segment
/// [`segment_ok`]. `""` is the repo root.
pub(crate) fn check_path(path: &str) -> Result<String> {
    let path = path.trim_matches('/');
    if !path.is_empty() && !path.split('/').all(segment_ok) {
        return Err(Error::Invalid(format!("unsafe path `{path}`")));
    }
    Ok(path.to_string())
}

/// Refs reach git as arguments: never an option, a range or a control character.
pub(crate) fn check_ref(git_ref: &str) -> Result<()> {
    if git_ref.is_empty() || git_ref.starts_with('-') || git_ref.contains("..") || git_ref.chars().any(char::is_control)
    {
        return Err(Error::Invalid(format!("invalid ref `{git_ref}`")));
    }
    Ok(())
}

/// Splits `link.rest` into a ref and a path, against `cache` (fetched): the
/// longest leading segments that name a branch (`origin/<ref>`), tag or
/// commit are the ref, the remainder is the path. No ref in the link means
/// `HEAD`.
pub(crate) fn split_ref(cache: &Path, link: &Link) -> Result<(String, String)> {
    if link.rest.is_empty() {
        return Ok(("HEAD".into(), String::new()));
    }
    for n in (1..=link.rest.len()).rev() {
        let git_ref = link.rest[..n].join("/");
        if resolve_commit(cache, &git_ref).is_ok() {
            return Ok((git_ref, link.rest[n..].join("/")));
        }
    }
    Err(Error::Invalid(format!("no branch, tag or commit of {} matches `{}`", link.source, link.rest.join("/"))))
}

/// The ref and path to import from `link`, with the user's explicit
/// `git_ref` and `path` taking precedence. An explicit ref must agree with
/// the ref in the link (or the link must name none) unless `path` is
/// explicit too, in which case the link's ref and path are both ignored.
pub(crate) fn locate(cache: &Path, link: &Link, git_ref: Option<&str>, path: Option<&str>) -> Result<(String, String)> {
    let path = path.map(check_path).transpose()?;
    let Some(git_ref) = git_ref else {
        let (git_ref, link_path) = split_ref(cache, link)?;
        return Ok((git_ref, path.unwrap_or(link_path)));
    };
    check_ref(git_ref)?;
    if let Some(path) = path {
        return Ok((git_ref.to_string(), path));
    }
    if link.rest.is_empty() {
        return Ok((git_ref.to_string(), String::new()));
    }
    // The link's own ref is what the remote says it is, not a textual prefix.
    let (link_ref, link_path) = split_ref(cache, link)?;
    if link_ref != git_ref {
        return Err(Error::Invalid(format!(
            "--ref `{git_ref}` conflicts with the ref in the link (`{link_ref}`); pass --path too to override both"
        )));
    }
    Ok((link_ref, link_path))
}

/// The ref, path and commit to import from `link` (see [`locate`]): the
/// ref's commit, or `commit` if given (it must be in `cache`). A `blob/`
/// link's `SKILL.md` must be a file at that commit.
pub(crate) fn resolve_link(
    cache: &Path,
    link: &Link,
    git_ref: Option<&str>,
    path: Option<&str>,
    commit: Option<&str>,
) -> Result<(String, String, String)> {
    let (git_ref, path) = locate(cache, link, git_ref, path)?;
    let commit = match commit {
        Some(c) => verify_commit(cache, &link.source, c)?,
        None => resolve_commit(cache, &git_ref)?,
    };
    let file = if path.is_empty() { "SKILL.md".into() } else { format!("{path}/SKILL.md") };
    let is_file = || git(cache, &["cat-file", "-t", &format!("{commit}:{file}")], &[]).is_ok_and(|t| t == "blob");
    if link.blob && !is_file() {
        return Err(Error::Invalid(format!("no `{file}` in {} @ {git_ref}", link.source)));
    }
    Ok((git_ref, path, commit))
}

/// A commit as the user gives it: a full or abbreviated hex SHA.
pub(crate) fn check_commit(commit: &str) -> Result<()> {
    if !(4..=64).contains(&commit.len()) || !commit.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(Error::Invalid(format!("commit `{commit}` is not a SHA")));
    }
    Ok(())
}

/// `commit` (full or abbreviated SHA) as a full SHA, if `cache` has it.
pub(crate) fn verify_commit(cache: &Path, source: &str, commit: &str) -> Result<String> {
    check_commit(commit)?;
    git(cache, &["rev-parse", "--verify", "--quiet", &format!("{commit}^{{commit}}")], &[])
        .map_err(|_| Error::Invalid(format!("commit `{commit}` is not in {source}")))
}

/// The name an import gets unless one is given: the last segment of `path`,
/// or the repo's name for the repo root.
pub fn default_name(source: &str, path: &str) -> String {
    let base = path.trim_matches('/').rsplit('/').next().filter(|s| !s.is_empty());
    let base = base.unwrap_or_else(|| source.trim_end_matches('/').rsplit(['/', ':']).next().unwrap_or(source));
    base.trim_end_matches(".git").to_string()
}

/// Expands `github:owner/repo` shorthand; anything else is passed to git as-is.
pub fn resolve_url(source: &str) -> String {
    match source.strip_prefix("github:") {
        Some(rest) => format!("https://github.com/{}.git", rest.trim_end_matches(".git")),
        None => source.to_string(),
    }
}

/// Whether git would read `source` as a local path rather than a URL
/// (`scheme://`, scp-like `host:path`, or `github:` shorthand).
pub fn is_local_path(source: &str) -> bool {
    let scp_like = source.split_once(':').is_some_and(|(host, _)| !host.contains('/'));
    !source.contains("://") && !scp_like
}

/// Clones or fetches `source` into its cache repo (never checked out), and
/// returns the cache's path. A relative local source is relative to `base`.
///
/// Fetches into one cache repo take turns ([`lock_cache`]); different
/// sources fetch in parallel. Auto-gc is off, so a fetch never repacks or
/// prunes objects another caller is reading at a pinned commit.
pub(crate) fn fetch(source: &str, base: &Path) -> Result<PathBuf> {
    let url = resolve_url(source);
    let url = if is_local_path(&url) && Path::new(&url).is_relative() {
        base.join(&url).to_string_lossy().into_owned()
    } else {
        url
    };
    let cache = cache_dir_for(&url);
    let parent = cache.parent().expect("cache dir has a parent");
    fs::create_dir_all(parent).at(parent)?;
    let _turn = lock_cache(&cache)?;
    let fetched = if cache.join(".git").is_dir() {
        git(
            &cache,
            &["fetch", "--quiet", "--no-auto-gc", "--tags", "--force", "--prune", "origin"],
            &[("GIT_ALLOW_PROTOCOL", Path::new("https:http:ssh:git:file"))],
        )
    } else {
        let cache_str = cache.to_string_lossy();
        git(
            parent,
            &["clone", "--quiet", "--no-checkout", "--", &url, &cache_str],
            &[("GIT_ALLOW_PROTOCOL", Path::new("https:http:ssh:git:file"))],
        )
    };
    fetched.map_err(|e| explain_github_failure(source, e))?;
    Ok(cache)
}

/// GitHub answers a missing repo, and a private one without credentials, by
/// asking for a username, which git reports as "could not read Username".
fn explain_github_failure(source: &str, e: Error) -> Error {
    match (source.strip_prefix("github:"), e) {
        (Some(repo), Error::Git { args, stderr })
            if stderr.contains("could not read Username") || stderr.contains("Repository not found") =>
        {
            let stderr = format!(
                "GitHub repo `{repo}` does not exist, or it is private and this machine's git has no \
                 credentials for it (git said: {})",
                stderr.trim()
            );
            Error::Git { args, stderr }
        }
        (_, e) => e,
    }
}

/// Waits for, then holds, the lock on `cache` (an OS file lock on a file
/// next to it, so it also covers other processes and a cache not cloned
/// yet). Released when dropped.
pub(crate) fn lock_cache(cache: &Path) -> Result<CacheLock> {
    let mut path = cache.as_os_str().to_owned();
    path.push(".lock");
    let path = PathBuf::from(path);
    let file = fs::OpenOptions::new().create(true).truncate(false).write(true).open(&path).at(&path)?;
    file.lock().at(&path)?;
    Ok(CacheLock(file))
}

/// A held [`lock_cache`] lock.
pub(crate) struct CacheLock(fs::File);

impl Drop for CacheLock {
    fn drop(&mut self) {
        // As for the engine lock: a child forked meanwhile shares the lock
        // until it reaches exec, so it is released explicitly.
        let _ = self.0.unlock();
    }
}

/// Prefix of staging directories. Draft, skill discovery and the daemon's
/// watcher ignore every path with a component starting with it (see
/// [`is_staging_path`]), and git is told to ([`exclude_stages`]), whatever the
/// repo's own `.gitignore` says.
pub const STAGE_PREFIX: &str = ".sk1ll-stage-";

/// Whether `path` is (inside) a staging directory.
pub fn is_staging_path(path: &Path) -> bool {
    path.components().any(|c| c.as_os_str().to_string_lossy().starts_with(STAGE_PREFIX))
}

/// `import`'s skill directory at `commit` of `cache`, checked out into a
/// fresh staging directory in `into` (the directory the destination will be
/// in, resolved, so installing is a rename on one file system). This is
/// the step that may still reach the network (smudge filters such as Git
/// LFS run at checkout), so it belongs with the fetch, not with the install.
/// `root` is the repo, whose git is told to ignore staging directories.
pub(crate) fn stage(cache: &Path, commit: &str, import: &Import, root: &Path, into: &Path) -> Result<Staged> {
    fs::create_dir_all(into).at(into)?;
    let parent = into.canonicalize().at(into)?;
    exclude_stages(root);
    let staged = Staged { dir: unique(&parent), sub: import.path.trim_matches('/').to_string() };
    let tree = staged.dir.join("tree");
    let index = staged.dir.join("index");
    fs::create_dir_all(&tree).at(&tree)?;
    let pathspec = if staged.sub.is_empty() { "." } else { &staged.sub };
    // A throwaway index + work tree so the cache repo itself is never checked out.
    git(
        cache,
        &["checkout", "--quiet", commit, "--", pathspec],
        &[("GIT_WORK_TREE", tree.as_path()), ("GIT_INDEX_FILE", index.as_path())],
    )?;
    if !staged.content().join("SKILL.md").is_file() {
        return Err(Error::Invalid(format!("no SKILL.md at `{pathspec}` in {} @ {}", import.source, import.git_ref)));
    }
    Ok(staged)
}

/// A fresh staging path in `dir` (nothing is created).
pub(crate) fn unique(dir: &Path) -> PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    dir.join(format!("{STAGE_PREFIX}{}-{nanos}-{n}", std::process::id()))
}

/// Adds the staging pattern to `info/exclude` in the repo's common git dir
/// (shared by its worktrees), once. Best effort: draft and discovery ignore
/// staging paths themselves too.
pub(crate) fn exclude_stages(root: &Path) {
    let Ok(common) = Git::new(root).args(["rev-parse", "--path-format=absolute", "--git-common-dir"]).text() else {
        return;
    };
    let info = Path::new(&common).join("info");
    if fs::create_dir_all(&info).is_err() {
        return;
    }
    // Concurrent stages take turns, so the line is only ever added once.
    let Ok(lock) =
        fs::OpenOptions::new().create(true).truncate(false).write(true).open(info.join("exclude.sk1ll-lock"))
    else {
        return;
    };
    if lock.lock().is_err() {
        return;
    }
    let file = info.join("exclude");
    let text = match fs::read(&file) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        // Unreadable: leave it alone rather than risk rewriting it.
        Err(_) => return,
    };
    let pattern = format!("{STAGE_PREFIX}*");
    if text.split(|b| *b == b'\n').any(|l| l.trim_ascii() == pattern.as_bytes()) {
        return;
    }
    // Append only: whatever is there already is never rewritten.
    let sep = if text.is_empty() || text.ends_with(b"\n") { "" } else { "\n" };
    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(&file) {
        let _ = std::io::Write::write_all(&mut f, format!("{sep}{pattern}\n").as_bytes());
    }
}

/// Where [`Staged::replace`] keeps the previous version of `dest` while it
/// installs the new one: a fixed name next to it, so an interrupted
/// replacement can be found and undone ([`recover_replaced`]).
fn backup_of(dest: &Path) -> Result<PathBuf> {
    let (Some(parent), Some(name)) = (dest.parent(), dest.file_name()) else {
        return Err(Error::Invalid(format!("bad import destination {}", dest.display())));
    };
    Ok(parent.join(format!("{STAGE_PREFIX}old-{}", name.to_string_lossy())))
}

/// Finishes replacements in the `imported/` directory of the repo at `root`
/// that were interrupted (see [`settle`]), with `sk1ll.lock` saying which
/// version is the recorded one (read only if there is anything to settle).
/// Returns the names restored. Anything ambiguous is left alone and
/// reported as an error, after the rest is settled, so a sync stops instead
/// of removing harness entries. Only run it holding the engine lock.
pub fn recover_replaced(root: &Path) -> Result<Vec<String>> {
    let dir = root.join(crate::skill::IMPORTED_DIR);
    let dir = dir.as_path();
    let mut lock: Option<Lockfile> = None;
    let mut restored = Vec::new();
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(restored),
        Err(e) => return Err(e).at(dir),
    };
    let prefix = format!("{STAGE_PREFIX}old-");
    let mut unsettled = Vec::new();
    for entry in entries {
        let entry = entry.at(dir)?;
        let file_name = entry.file_name().to_string_lossy().into_owned();
        let Some(name) = file_name.strip_prefix(&prefix) else { continue };
        let lock = match &mut lock {
            Some(l) => l,
            None => lock.insert(Lockfile::load(root)?),
        };
        let locked = lock.skills.get(name).map(|e| e.hash.as_str());
        match settle(&dir.join(name), &entry.path(), locked) {
            Ok(true) => restored.push(name.to_string()),
            Ok(false) => {}
            Err(e) => unsettled.push(e.to_string()),
        }
    }
    if !unsettled.is_empty() {
        return Err(Error::Invalid(unsettled.join("; ")));
    }
    Ok(restored)
}

/// Settles an interrupted replacement of `dest` that kept the previous
/// version at `backup`; `locked` is `dest`'s hash in the lockfile. The
/// backup is never deleted on doubt:
///
/// - Nothing at `dest`: the backup is put back (`Ok(true)`).
/// - `dest` is a directory whose hash is the recorded one: the replacement
///   completed and was recorded, so the backup goes (`Ok(false)`).
/// - Anything else (no lock entry, a different hash, an empty or partial
///   directory, a file): both are left alone and the error names them.
fn settle(dest: &Path, backup: &Path, locked: Option<&str>) -> Result<bool> {
    let meta = match fs::symlink_metadata(dest) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            fs::rename(backup, dest).at(dest)?;
            return Ok(true);
        }
        Err(e) => return Err(e).at(dest),
        Ok(meta) => meta,
    };
    let hash = |dir: &Path| crate::hash::vendor_hash(dir).ok();
    let recorded = |dir: &Path| locked.is_some() && hash(dir).as_deref() == locked;
    if meta.is_dir() && recorded(dest) {
        fs::remove_dir_all(backup).at(backup)?;
        return Ok(false);
    }
    let backup_recorded = recorded(backup);
    let (dest, backup) = (dest.display(), backup.display());
    Err(Error::Invalid(if backup_recorded {
        format!(
            "an update of {dest} was interrupted before it was recorded: {backup} is the recorded version and \
             {dest} the unrecorded new one. sk1ll keeps both: remove {dest} to go back to the recorded version, \
             or remove {backup} and update again to keep the new one; then sync again"
        )
    } else {
        format!(
            "an interrupted update left {backup}, and {dest} is not what sk1ll.lock records for it. sk1ll keeps \
             both: remove or move one of them, then sync again"
        )
    }))
}

/// A skill directory checked out by [`stage`], waiting to be installed.
/// Dropping it removes whatever is left of the staging directory.
#[derive(Debug)]
pub(crate) struct Staged {
    dir: PathBuf,
    sub: String,
}

#[cfg(test)]
thread_local! {
    /// Makes this test thread's next install rename fail, like EXDEV would.
    pub(crate) static FAIL_INSTALL: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Makes this test thread's next restore after a failed install fail too.
    pub(crate) static FAIL_RESTORE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

impl Staged {
    fn content(&self) -> PathBuf {
        let tree = self.dir.join("tree");
        if self.sub.is_empty() { tree } else { tree.join(&self.sub) }
    }

    /// Moves the staged content to `dest`, never replacing anything there.
    fn place(&self, dest: &Path) -> Result<()> {
        #[cfg(test)]
        if FAIL_INSTALL.replace(false) {
            return Err(std::io::Error::from(std::io::ErrorKind::CrossesDevices)).at(dest);
        }
        crate::sync::rename_noreplace(&self.content(), dest).at(dest)
    }

    /// Installs a new import at `dest`, where nothing may be: something that
    /// appeared there (even at the last moment) is refused, never replaced.
    pub(crate) fn install_new(self, dest: &Path) -> Result<()> {
        self.place(dest).map_err(|e| match e {
            Error::Io { source, .. } if source.kind() == std::io::ErrorKind::AlreadyExists => Error::Invalid(format!(
                "{} appeared while the import was being added; nothing was changed",
                dest.display()
            )),
            e => e,
        })
    }

    /// Replaces the import at `dest`, whose lockfile hash is `locked`: the
    /// old directory is moved aside (in its own directory, so that can't
    /// cross file systems), the new one is moved in, and the old one is
    /// restored if that fails. Only then is the old one deleted.
    pub(crate) fn replace(self, dest: &Path, locked: Option<&str>) -> Result<()> {
        let aside = backup_of(dest)?;
        // A replacement interrupted before: finish it first, or refuse.
        if fs::symlink_metadata(&aside).is_ok() {
            settle(dest, &aside, locked)?;
        }
        fs::rename(dest, &aside).at(dest)?;
        if let Err(e) = self.place(dest) {
            #[cfg(test)]
            let restored = if FAIL_RESTORE.replace(false) {
                Err(std::io::Error::other("injected restore failure"))
            } else {
                fs::rename(&aside, dest)
            };
            #[cfg(not(test))]
            let restored = fs::rename(&aside, dest);
            if let Err(r) = restored {
                return Err(Error::Invalid(format!(
                    "installing {} failed ({e}), and so did putting the previous version back ({r}); it is kept \
                     at {} and sk1ll restores it before its next sync",
                    dest.display(),
                    aside.display()
                )));
            }
            return Err(e);
        }
        let _ = fs::remove_dir_all(&aside);
        Ok(())
    }
}

impl Drop for Staged {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// The directories (repo-relative, `""` for the root) holding a `SKILL.md`
/// at `commit` of `cache`, under `path` (or anywhere), sorted. Reads the
/// tree only; nothing is checked out.
pub(crate) fn skill_dirs(cache: &Path, commit: &str, path: &str) -> Result<Vec<String>> {
    let mut cmd = Git::new(cache).literal().args(["ls-tree", "-r", "-z", "--name-only", commit]);
    if !path.is_empty() {
        cmd = cmd.args(["--", path]);
    }
    let out = cmd.run()?;
    let mut dirs: Vec<String> = out
        .split(|b| *b == 0)
        .map(String::from_utf8_lossy)
        .filter_map(|f| match f.rsplit_once('/') {
            Some((dir, "SKILL.md")) => Some(dir.to_string()),
            None if f == "SKILL.md" => Some(String::new()),
            _ => None,
        })
        .collect();
    dirs.sort();
    Ok(dirs)
}

pub(crate) fn file_warnings(cache: &Path, commit: &str, path: &str) -> Result<(u64, Vec<crate::imports::FileWarning>)> {
    use crate::imports::{FileWarning, WarningKind};
    let mut cmd = Git::new(cache).literal().args(["ls-tree", "-rz", commit]);
    if !path.is_empty() {
        cmd = cmd.args(["--", path]);
    }
    let rows = cmd.run()?;
    let mut total = 0;
    let mut warnings = Vec::new();
    for row in rows.split(|b| *b == 0).filter(|row| !row.is_empty()) {
        let row = String::from_utf8_lossy(row);
        let Some((meta, file)) = row.split_once('\t') else { continue };
        let mut fields = meta.split_whitespace();
        let (Some(mode), Some("blob"), Some(oid)) = (fields.next(), fields.next(), fields.next()) else {
            continue;
        };
        let content = Git::new(cache).args(["cat-file", "blob", oid]).run()?;
        total += content.len() as u64;
        let mut kinds = Vec::new();
        if mode == "100755" {
            kinds.push(WarningKind::Executable);
        }
        let file = file.strip_prefix(path).and_then(|p| p.strip_prefix('/')).unwrap_or(file);
        let extension = Path::new(file).extension().and_then(|s| s.to_str()).unwrap_or_default().to_ascii_lowercase();
        if content.starts_with(b"#!")
            || matches!(extension.as_str(), "sh" | "py" | "js" | "ts" | "ps1" | "rb" | "pl" | "bat" | "cmd")
        {
            kinds.push(WarningKind::Script);
        }
        if std::str::from_utf8(&content).is_err() {
            kinds.push(WarningKind::Binary);
        }
        if content.len() > 1024 * 1024 {
            kinds.push(WarningKind::Large);
        }
        if !kinds.is_empty() {
            warnings.push(FileWarning { path: file.to_string(), kinds });
        }
    }
    Ok((total, warnings))
}

pub(crate) fn changes(cache: &Path, locked: &str, upstream: &str, path: &str) -> Result<crate::imports::ImportChanges> {
    use crate::imports::{ImportChanges, ImportCommit};
    const LIMIT: usize = 50;
    let range = format!("{locked}..{upstream}");
    let log = Git::new(cache)
        .args(["log", "--no-notes", "--no-show-signature", "--max-count=51", "--format=%h%x00%cs%x00%s", &range, "--"])
        .text()?;
    let mut commits: Vec<_> = log
        .lines()
        .filter_map(|line| {
            let mut parts = line.splitn(3, '\0');
            Some(ImportCommit { sha: parts.next()?.into(), date: parts.next()?.into(), subject: parts.next()?.into() })
        })
        .collect();
    let truncated = commits.len() > LIMIT;
    commits.truncate(LIMIT);
    let mut diff =
        Git::new(cache).literal().args(["diff", "--name-only", "--no-renames", "-z", locked, upstream, "--"]);
    if !path.is_empty() {
        diff = diff.arg(path);
    }
    let files = crate::git::split_paths(&diff.run()?).map(|file| file.to_string_lossy().into_owned()).collect();
    Ok(ImportChanges { locked: locked.into(), upstream: upstream.into(), commits, truncated, files })
}

/// Explains why `path` at `commit` holds no skills when it isn't a folder:
/// a `tree/` link can point at a file, or at something that isn't there.
pub(crate) fn check_folder(cache: &Path, commit: &str, git_ref: &str, path: &str) -> Result<()> {
    match git(cache, &["cat-file", "-t", &format!("{commit}:{path}")], &[]).as_deref() {
        Ok("tree") => Ok(()),
        Ok(_) => Err(Error::Invalid(format!(
            "`{path}` is a file, not a folder: link to the folder that holds SKILL.md, or to SKILL.md itself"
        ))),
        Err(_) => Err(Error::Invalid(format!("there is no `{path}` at `{git_ref}`"))),
    }
}

/// A file's text at `commit` of `cache`, if it is there.
pub(crate) fn show(cache: &Path, commit: &str, file: &str) -> Option<String> {
    let out = Git::new(cache).args(["show", &format!("{commit}:{file}")]).run().ok()?;
    Some(String::from_utf8_lossy(&out).into_owned())
}

pub(crate) fn resolve_commit(repo: &Path, git_ref: &str) -> Result<String> {
    check_ref(git_ref)?;
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
    #[cfg(test)]
    if let Some(root) = TEST_CACHE.with_borrow(Clone::clone) {
        return root.join(&digest[..16]);
    }
    dirs::cache_dir().unwrap_or_else(std::env::temp_dir).join("sk1ll").join("git").join(&digest[..16])
}

fn git(dir: &Path, args: &[&str], env: &[(&str, &Path)]) -> Result<String> {
    env.iter().fold(Git::new(dir).args(args), |g, (k, v)| g.env(k, v)).text()
}

#[cfg(test)]
thread_local! {
    static TEST_CACHE: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

/// Keeps this test thread's import cache in `root` instead of the user's
/// cache directory, until the guard is dropped.
#[cfg(test)]
pub(crate) fn test_cache(root: &Path) -> TestCache {
    TEST_CACHE.with_borrow_mut(|c| *c = Some(root.to_path_buf()));
    TestCache
}

#[cfg(test)]
pub(crate) struct TestCache;

#[cfg(test)]
impl Drop for TestCache {
    fn drop(&mut self) {
        TEST_CACHE.with_borrow_mut(|c| *c = None);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_github_username_prompt_means_missing_or_private() {
        let failed = |stderr: &str| Error::Git { args: "clone".into(), stderr: stderr.into() };
        let prompt = "fatal: could not read Username for 'https://github.com': terminal prompts disabled\n";
        let explained = explain_github_failure("github:o/nope", failed(prompt)).to_string();
        assert!(explained.contains("GitHub repo `o/nope` does not exist, or it is private"), "{explained}");
        assert!(explained.contains("could not read Username"), "keeps git's message: {explained}");
        let other = explain_github_failure("github:o/r", failed("could not resolve host")).to_string();
        assert_eq!(other, "`git clone` failed: could not resolve host");
        let plain = explain_github_failure("https://example.com/r.git", failed(prompt)).to_string();
        assert!(!plain.contains("GitHub repo"), "{plain}");
    }

    use super::*;

    fn link(source: &str, rest: &[&str], blob: bool) -> Link {
        Link { source: source.into(), rest: rest.iter().map(|s| s.to_string()).collect(), blob }
    }

    #[test]
    fn parses_github_links() {
        let parse = |s: &str| parse_link(s).unwrap();
        for root in [
            "https://github.com/o/r",
            "https://github.com/o/r/",
            "https://github.com/o/r.git",
            "github.com/o/r",
            "github:o/r",
            "github:o/r.git",
            " https://github.com/o/r?tab=readme#top ",
        ] {
            assert_eq!(parse(root), link("github:o/r", &[], false), "{root}");
        }
        assert_eq!(
            parse("https://github.com/o/r/tree/feature/x/skills/pdf"),
            link("github:o/r", &["feature", "x", "skills", "pdf"], false)
        );
        assert_eq!(parse("github:o/r/tree/main/skills/pdf/"), link("github:o/r", &["main", "skills", "pdf"], false));
        // A blob link names its SKILL.md; the directory is what's kept.
        assert_eq!(
            parse("https://github.com/o/r/blob/v1.2/skills/pdf/SKILL.md"),
            link("github:o/r", &["v1.2", "skills", "pdf"], true)
        );
        for bad in [
            "https://github.com/o",
            "https://github.com/o/r/issues/3",
            "https://github.com/o/r/blob/main",
            "https://github.com/o/r/blob/main/SKILL.md/x",
            "https://github.com/o/r/blob/main/skills/pdf/README.md",
            "https://github.com/o/r/blob/main/skills/pdf/skill.md",
            "https://github.com/../r",
            "https://github.com/o/r%2Fx",
            "--upload-pack=touch /tmp/x",
            "",
            "https://github.com/o/r/tree/main/skills//pdf",
            "https://github.com/o/r/tree/main/skills/pdf//",
            "https://github.com//o/r",
            "/srv/git/r\n--upload-pack=x",
            "https://example.com/r.git\u{7}",
        ] {
            assert!(parse_link(bad).is_err(), "{bad}");
        }
        let err = parse_link("https://github.com/o/r/blob/main/skills/pdf/README.md").unwrap_err();
        assert!(err.to_string().contains("blob links must point at a SKILL.md"), "{err}");
        // Anything else is a git URL or local path, unchanged.
        for plain in ["https://gitlab.com/o/r.git", "git@github.com:o/r.git", "../skills-repo", "/srv/git/r"] {
            assert_eq!(parse(plain), link(plain, &[], false), "{plain}");
        }
    }

    #[test]
    fn decodes_link_segments_and_rejects_unsafe_ones() {
        let parse = |s: &str| parse_link(s).unwrap();
        // An encoded slash splits like a plain one, so the remote still decides.
        assert_eq!(
            parse("https://github.com/o/r/tree/feature%2Fx/skills/pdf"),
            link("github:o/r", &["feature", "x", "skills", "pdf"], false)
        );
        let tree = |rest: &str| parse(&format!("https://github.com/o/r/tree/main/{rest}"));
        assert_eq!(tree("my%20skills"), link("github:o/r", &["main", "my skills"], false));
        assert_eq!(tree("caf%C3%A9/%E6%97%A5"), link("github:o/r", &["main", "café", "日"], false));
        // The shorthand is literal.
        assert_eq!(parse("github:o/r/tree/main/a%20b"), link("github:o/r", &["main", "a%20b"], false));
        for unsafe_link in [
            "https://github.com/o/r/tree/main/%2E%2E/x",
            "https://github.com/o/r/tree/main/a%2F..%2Fb",
            "https://github.com/o/r/tree/main/%2E",
            "https://github.com/o/r/tree/main/a%00b",
            "https://github.com/o/r/tree/main/a%0Ab",
            "https://github.com/o/r/tree/main/a%2F%2Fb",
            "https://github.com/o/r/tree/main/%zz",
            "https://github.com/o/r/tree/main/%FF",
            "github:o/r/tree/main/../x",
        ] {
            assert!(parse_link(unsafe_link).is_err(), "{unsafe_link}");
        }
        assert!(check_path("skills/../x").is_err() && check_path("a\nb").is_err());
        assert_eq!(check_path("/skills/pdf/").unwrap(), "skills/pdf");
        for bad in ["-x", "a..b", "main\n", ""] {
            assert!(check_ref(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn default_names() {
        assert_eq!(default_name("github:o/r", "skills/pdf"), "pdf");
        assert_eq!(default_name("github:o/r", ""), "r");
        assert_eq!(default_name("https://example.com/x/skills.git", "/"), "skills");
        assert_eq!(default_name("/srv/git/tools/", ""), "tools");
    }

    /// A local remote with `x/skills/pdf/SKILL.md` on `main`, a slashed
    /// branch `feature/x`, tags `v1` and `feature`, and a directory named
    /// `x/skills/fake/SKILL.md` (holding a skill of its own); returns
    /// (source, cache, HEAD sha).
    fn remote(tmp: &Path) -> (String, PathBuf, String) {
        let origin = tmp.join("origin");
        let run = |args: &[&str]| Git::new(&origin).args(args).text().unwrap();
        fs::create_dir_all(origin.join("x/skills/pdf")).unwrap();
        fs::write(origin.join("x/skills/pdf/SKILL.md"), "PDFs.").unwrap();
        fs::create_dir_all(origin.join("x/skills/fake/SKILL.md/nested")).unwrap();
        fs::write(origin.join("x/skills/fake/SKILL.md/nested/SKILL.md"), "Nested.").unwrap();
        run(&["init", "--quiet", "-b", "main"]);
        run(&["add", "-A"]);
        run(&["-c", "user.name=T", "-c", "user.email=t@example.com", "commit", "--quiet", "-m", "seed"]);
        // `feature` and `feature/x` can't both be branches; `x` is a path here.
        run(&["branch", "feature/x"]);
        run(&["tag", "v1"]);
        // A tag that is a textual prefix of the branch `feature/x`.
        run(&["tag", "feature"]);
        let sha = run(&["rev-parse", "HEAD"]);
        let source = origin.to_string_lossy().into_owned();
        let cache = fetch(&source, Path::new("/")).unwrap();
        (source, cache, sha)
    }

    /// Branch names with slashes, tags and commits, against a local remote.
    #[test]
    fn splits_the_longest_ref_that_resolves() {
        let tmp = tempfile::tempdir().unwrap();
        let _cache = test_cache(&tmp.path().join("cache"));
        let (source, cache, sha) = remote(tmp.path());
        let split = |rest: &[&str]| split_ref(&cache, &link(&source, rest, false)).unwrap();
        let pair = |r: &str, p: &str| (r.to_string(), p.to_string());

        assert_eq!(split(&["feature", "x", "skills", "pdf"]), pair("feature/x", "skills/pdf"));
        assert_eq!(split(&["main", "x", "skills", "pdf"]), pair("main", "x/skills/pdf"));
        assert_eq!(split(&["v1", "x", "skills", "pdf"]), pair("v1", "x/skills/pdf"));
        assert_eq!(split(&[&sha, "x", "skills", "pdf"]), pair(&sha, "x/skills/pdf"));
        assert_eq!(split(&["feature", "x"]), pair("feature/x", ""));
        assert_eq!(split(&[]), pair("HEAD", ""));
        let err = split_ref(&cache, &link(&source, &["nope", "skills"], false)).unwrap_err();
        assert!(err.to_string().contains("no branch, tag or commit"), "{err}");

        // A blob link's SKILL.md must exist at the resolved commit.
        let blob = |rest: &[&str]| resolve_link(&cache, &link(&source, rest, true), None, None, None);
        assert_eq!(blob(&["v1", "x", "skills", "pdf"]).unwrap(), ("v1".into(), "x/skills/pdf".into(), sha.clone()));
        let err = blob(&["main", "x", "skills"]).unwrap_err();
        assert!(err.to_string().contains("no `x/skills/SKILL.md`"), "{err}");
        // A directory named SKILL.md is not a skill's SKILL.md.
        let err = blob(&["main", "x", "skills", "fake"]).unwrap_err();
        assert!(err.to_string().contains("no `x/skills/fake/SKILL.md`"), "{err}");
    }

    /// Explicit `--ref`/`--path` against what the link names.
    #[test]
    fn explicit_ref_and_path_override_the_link() {
        let tmp = tempfile::tempdir().unwrap();
        let _cache = test_cache(&tmp.path().join("cache"));
        let (source, cache, _) = remote(tmp.path());
        let tree = link(&source, &["feature", "x", "skills", "pdf"], false);
        let plain = link(&source, &[], false);
        let at = |l: &Link, r: Option<&str>, p: Option<&str>| locate(&cache, l, r, p);
        let pair = |r: &str, p: &str| Ok((r.to_string(), p.to_string()));

        assert_eq!(at(&tree, None, Some("x")).map_err(drop), pair("feature/x", "x"));
        // An explicit ref that agrees with the link keeps the link's path.
        assert_eq!(at(&tree, Some("feature/x"), None).map_err(drop), pair("feature/x", "skills/pdf"));
        let err = at(&tree, Some("main"), None).unwrap_err().to_string();
        assert!(err.contains("conflicts with the ref in the link") && err.contains("--path"), "{err}");
        // The link means `feature/x` (the longest ref); the tag `feature` is
        // only a textual prefix of it, so it conflicts too.
        let err = at(&tree, Some("feature"), None).unwrap_err().to_string();
        assert!(err.contains("(`feature/x`)"), "{err}");
        // With both explicit, the link's ref and path are ignored (and need not resolve).
        let bogus = link(&source, &["no-such-ref", "skills"], false);
        assert_eq!(at(&bogus, Some("HEAD"), Some("x/skills/pdf")).map_err(drop), pair("HEAD", "x/skills/pdf"));
        assert_eq!(at(&plain, Some("HEAD"), None).map_err(drop), pair("HEAD", ""));
        assert_eq!(at(&plain, Some("v1"), Some("/x/")).map_err(drop), pair("v1", "x"));
        assert!(at(&plain, Some("--output=x"), None).is_err());
        assert!(at(&plain, Some("main..v1"), None).is_err());
        assert!(at(&plain, None, Some("../x")).is_err());
    }

    /// Fetches of one source take turns: one waits while another holds the
    /// cache's lock, and many at once (into a cache not cloned yet) all
    /// succeed instead of racing to clone into the same directory.
    #[test]
    fn fetches_into_one_cache_take_turns() {
        use std::sync::mpsc;
        use std::time::Duration;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("cache");
        let _cache = test_cache(&root);
        let (source, cache, _) = remote(tmp.path());

        let held = lock_cache(&cache).unwrap();
        let (tx, rx) = mpsc::channel();
        let waiter = {
            let (root, source) = (root.clone(), source.clone());
            std::thread::spawn(move || {
                let _cache = test_cache(&root);
                tx.send(fetch(&source, Path::new("/")).map(drop)).unwrap();
            })
        };
        assert!(rx.recv_timeout(Duration::from_millis(300)).is_err(), "fetched while another held the lock");
        drop(held);
        rx.recv_timeout(Duration::from_secs(30)).unwrap().unwrap();
        waiter.join().unwrap();

        fs::remove_dir_all(&cache).unwrap();
        let racers: Vec<_> = (0..6)
            .map(|_| {
                let (root, source) = (root.clone(), source.clone());
                std::thread::spawn(move || {
                    let _cache = test_cache(&root);
                    fetch(&source, Path::new("/")).map(drop)
                })
            })
            .collect();
        for r in racers {
            r.join().unwrap().unwrap();
        }
        assert!(resolve_commit(&cache, "feature/x").is_ok());
    }
}

#[cfg(test)]
mod transport_tests {
    #[test]
    fn ext_import_cannot_execute() {
        let tmp = tempfile::tempdir().unwrap();
        let marker = tmp.path().join("executed");
        let source = format!("ext::sh -c 'touch {}'", marker.display());
        assert!(super::fetch(&source, tmp.path()).is_err());
        assert!(!marker.exists());
    }
}
