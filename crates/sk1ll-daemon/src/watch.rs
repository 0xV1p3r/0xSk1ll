//! Watches the repo's working tree and every harness's skills and
//! instructions paths, and re-syncs once changes settle.
//!
//! Inside `.git` only `HEAD`, `packed-refs` and `refs/heads/` count, so
//! commits, restores and checkouts are noticed but git's own bookkeeping (and
//! our fetches) isn't. Reads never count, so a sync that changes nothing
//! doesn't trigger another one; one that does write converges after one
//! more pass.
//!
//! Events only set a coalesced dirty flag. A sync runs after a quiet period
//! (`debounce`), but a steady stream of events postpones it at most
//! `debounce_max`. Independently of events and of the server connection, a
//! periodic reconciliation (`reconcile_every`) re-syncs too.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher, event::ModifyKind};
use tokio::sync::Notify;
use tokio::time::Instant;

use crate::daemon::Daemon;

/// A path to watch. A directory is watched recursively if `recursive`; a
/// file through its directory (editors replace files by renaming); a path
/// that doesn't exist yet through its nearest existing ancestor, so that
/// creating it is noticed.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct WatchRoot {
    path: PathBuf,
    recursive: bool,
}

impl WatchRoot {
    pub(crate) fn new(path: PathBuf, recursive: bool) -> Self {
        Self { path, recursive }
    }

    fn target(&self) -> Option<(PathBuf, RecursiveMode)> {
        if self.path.is_dir() {
            let mode = if self.recursive { RecursiveMode::Recursive } else { RecursiveMode::NonRecursive };
            return Some((self.path.clone(), mode));
        }
        let dir = self.path.ancestors().skip(1).find(|p| p.is_dir())?;
        Some((dir.to_path_buf(), RecursiveMode::NonRecursive))
    }
}

type Target = (PathBuf, RecursiveMode);

/// Paths alone do not identify a watch: a replacement directory has a new
/// inode while the backend may still be watching the old one.
#[derive(Debug, Clone, PartialEq, Eq)]
struct WatchedTarget {
    target: Target,
    identity: Option<(u64, u64)>,
}

fn watched_targets(roots: &[WatchRoot]) -> Vec<WatchedTarget> {
    use std::os::unix::fs::MetadataExt;
    targets(roots)
        .into_iter()
        .map(|target| {
            let identity = std::fs::metadata(&target.0).ok().map(|m| (m.dev(), m.ino()));
            WatchedTarget { target, identity }
        })
        .collect()
}

/// What to actually watch for `roots` right now. It changes when a missing
/// directory appears even if the roots don't.
fn targets(roots: &[WatchRoot]) -> Vec<Target> {
    let mut targets: Vec<Target> = roots.iter().filter_map(WatchRoot::target).collect();
    // Recursive first, so it wins over a non-recursive watch of the same dir.
    targets.sort_by(|a, b| {
        let flat = |m: &RecursiveMode| *m == RecursiveMode::NonRecursive;
        a.0.cmp(&b.0).then(flat(&a.1).cmp(&flat(&b.1)))
    });
    targets.dedup_by(|a, b| a.0 == b.0);
    targets
}

/// Whether a change at `path` matters, given the repo's `.git` directory.
fn relevant(path: &Path, git_dir: &Path) -> bool {
    // Imports being staged and installed (see `sk1ll_core::source::STAGE_PREFIX`).
    if sk1ll_core::source::is_staging_path(path) {
        return false;
    }
    let Ok(rel) = path.strip_prefix(git_dir) else {
        return true;
    };
    if rel.extension().is_some_and(|e| e == "lock") {
        return false;
    }
    rel == Path::new("HEAD") || rel == Path::new("packed-refs") || rel.starts_with("refs/heads")
}

/// The coalesced "something changed" flag shared with the watcher thread.
#[derive(Default)]
struct Dirty {
    flag: AtomicBool,
    rearm: AtomicBool,
    notify: Notify,
}

impl Dirty {
    fn mark(&self) {
        self.flag.store(true, Ordering::SeqCst);
        self.notify.notify_one();
    }

    fn take(&self) -> bool {
        self.flag.swap(false, Ordering::SeqCst)
    }
}

fn build(
    targets: &[WatchedTarget],
    roots: &[WatchRoot],
    git_dir: PathBuf,
    dirty: Arc<Dirty>,
) -> Option<RecommendedWatcher> {
    if targets.is_empty() {
        return None;
    }
    let roots = roots.to_vec();
    let handler = move |res: notify::Result<notify::Event>| {
        let Ok(ev) = res else {
            // Overflowed or failed: we don't know what changed, so re-sync.
            dirty.rearm.store(true, Ordering::SeqCst);
            dirty.mark();
            return;
        };
        if matches!(ev.kind, EventKind::Access(_)) {
            return;
        }
        if matches!(ev.kind, EventKind::Remove(_) | EventKind::Modify(ModifyKind::Name(_)))
            && (ev.paths.is_empty() || ev.paths.iter().any(|p| roots.iter().any(|r| r.path.starts_with(p))))
        {
            dirty.rearm.store(true, Ordering::SeqCst);
        }
        if ev.paths.is_empty() || ev.paths.iter().any(|p| relevant(p, &git_dir)) {
            dirty.mark();
        }
    };
    let mut watcher = match notify::recommended_watcher(handler) {
        Ok(w) => w,
        Err(e) => {
            tracing::error!("cannot watch files: {e}");
            return None;
        }
    };
    for WatchedTarget { target: (path, mode), .. } in targets {
        if let Err(e) = watcher.watch(path, *mode) {
            tracing::warn!("cannot watch {}: {e}", path.display());
        }
    }
    tracing::debug!("watching {} path(s)", targets.len());
    Some(watcher)
}

/// Waits until changes settle: `debounce` without a new event, or
/// `debounce_max` after the first one, whichever comes first.
async fn settle(dirty: &Dirty, d: &Daemon) {
    let deadline = Instant::now() + d.cfg.debounce_max;
    loop {
        let now = Instant::now();
        if now >= deadline {
            return;
        }
        let wait = d.cfg.debounce.min(deadline - now);
        if tokio::time::timeout(wait, dirty.notify.notified()).await.is_err() {
            return;
        }
    }
}

/// Keeps the watcher in line with the roots the daemon publishes (and with
/// directories appearing), and runs [`Daemon::on_files_changed`] once changes
/// settle or the reconciliation interval passes. Registers the initial watches
/// before returning the future, so the local API cannot acknowledge readiness
/// while file changes can still be missed.
pub(crate) fn run(d: Arc<Daemon>) -> impl Future<Output = ()> {
    let mut roots_rx = d.roots.subscribe();
    let dirty = Arc::new(Dirty::default());
    // The repo root comes first in the daemon's list; its `.git` filters.
    let git_dir = d.view().repo.canonicalize().unwrap_or_else(|_| d.view().repo).join(".git");
    // Startup published the roots before we subscribed: start from them.
    let mut roots = roots_rx.borrow_and_update().clone();
    let mut current = watched_targets(&roots);
    let mut _watcher = build(&current, &roots, git_dir.clone(), dirty.clone());
    // Changes between startup's refresh and installing the watches have no
    // event to replay. Reconcile once with the watches already installed.
    dirty.mark();
    async move {
        let mut reconcile = tokio::time::interval(d.cfg.reconcile_every);
        reconcile.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        reconcile.tick().await;
        loop {
            tokio::select! {
                changed = roots_rx.changed() => {
                    if changed.is_err() {
                        return;
                    }
                    roots = roots_rx.borrow_and_update().clone();
                }
                () = dirty.notify.notified() => settle(&dirty, &d).await,
                _ = reconcile.tick() => dirty.flag.store(true, Ordering::SeqCst),
            }
            if d.is_revoked() {
                continue;
            }
            // Recheck identities on every wake, including reconciliation ticks.
            let now = watched_targets(&roots);
            let rearm = dirty.rearm.swap(false, Ordering::SeqCst);
            if now != current || rearm {
                current = now;
                _watcher = build(&current, &roots, git_dir.clone(), dirty.clone());
                // Whatever appeared hasn't been seen yet.
                dirty.flag.store(true, Ordering::SeqCst);
            }
            if dirty.take()
                && let Err(e) = d.on_files_changed().await
            {
                d.set_error(format!("sync after file change: {e}"));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_ref_changes_count_inside_git() {
        let git = Path::new("/r/.git");
        assert!(relevant(Path::new("/r/personal/pdf/SKILL.md"), git));
        assert!(relevant(Path::new("/r/.git/HEAD"), git));
        assert!(relevant(Path::new("/r/.git/refs/heads/main"), git));
        assert!(relevant(Path::new("/r/.git/packed-refs"), git));
        assert!(!relevant(Path::new("/r/.git/refs/heads/main.lock"), git));
        assert!(!relevant(Path::new("/r/.git/index"), git));
        assert!(!relevant(Path::new("/r/.git/objects/ab/cdef"), git));
        assert!(!relevant(Path::new("/r/.git/refs/remotes/origin/main"), git));
        assert!(!relevant(Path::new("/r/.git/FETCH_HEAD"), git));
        assert!(!relevant(Path::new("/r/imported/.sk1ll-stage-1-2-3/tree/SKILL.md"), git));
        assert!(!relevant(Path::new("/r/imported/.sk1ll-stage-1-2-3-old"), git));
        assert!(relevant(Path::new("/r/imported/pdf/SKILL.md"), git));
    }

    #[test]
    fn targets_change_when_a_missing_dir_appears() {
        let dir = tempfile::tempdir().unwrap();
        let roots = [WatchRoot::new(dir.path().join("agent/skills"), true)];
        assert_eq!(targets(&roots), [(dir.path().to_path_buf(), RecursiveMode::NonRecursive)]);
        std::fs::create_dir_all(dir.path().join("agent/skills")).unwrap();
        assert_eq!(targets(&roots), [(dir.path().join("agent/skills"), RecursiveMode::Recursive)]);
    }

    #[test]
    fn missing_paths_are_watched_through_an_ancestor() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("AGENTS.md");
        std::fs::write(&file, "").unwrap();
        let t = WatchRoot::new(file, true).target().unwrap();
        assert_eq!(t, (dir.path().to_path_buf(), RecursiveMode::NonRecursive));
        let t = WatchRoot::new(dir.path().join("a/b/skills"), true).target().unwrap();
        assert_eq!(t, (dir.path().to_path_buf(), RecursiveMode::NonRecursive));
        let t = WatchRoot::new(dir.path().to_path_buf(), true).target().unwrap();
        assert_eq!(t, (dir.path().to_path_buf(), RecursiveMode::Recursive));
    }
}
