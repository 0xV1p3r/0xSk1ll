//! [`Repo::pull_keeping`]: "keep mine" for a Blocked pull in one step.
//! Each chosen item's Draft version goes into recovery as Restore keeps it,
//! the pull takes the new Baseline, and the kept files go back on top as
//! Draft changes, each the whole file: the other Machine's change to that
//! item is overwritten in the Draft.
//!
//! Only the run itself puts kept files back, once its pull went through
//! (or, when the pull stays Blocked on an item it wasn't told to keep, back
//! where they were), with the guards a run has against an edit made while
//! it runs: a kept file goes back only where the path holds exactly what
//! the run left there. A run that is interrupted (a crash, a stop, or an
//! error once a file moved) puts nothing back, ever: whatever the user did
//! since can't be guessed. Its journal ([`FILE`] in the worktree's git
//! directory), written before the first file moves, names each recovery
//! entry before its file goes in, and the index entries the pull's
//! un-commit staged at kept paths. The next pull or reconcile, once no pull
//! is pending ([`Repo::finish_keeping`]), turns it into a report: each kept
//! version's recovery entry, for `sk1ll recover adopt`. Beyond that it only
//! clears the run's own staging, and resets an index entry the un-commit
//! staged where it is still exactly that; one changed since is reported and
//! left as it is.
//!
//! Staging is the user's or the run's own: a kept path the user staged
//! before the run is refused before anything changes, so every other kept
//! path's index entry is `HEAD`'s, or what the un-commit gave it.

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::{ItemRef, PullOutcome, pull, quote_path, rev, unquote_path};
use crate::git::{path_bytes, path_from_bytes};
use crate::sync::displaced;
use crate::{Error, IoExt, Repo, Result};

/// The journal's file name in the worktree's git directory.
const FILE: &str = "sk1ll-keep.json";

/// What an interrupted run left, kept beside the journal until the user
/// has acted on it (see [`report`]).
const REPORT: &str = "sk1ll-keep-report.json";

/// Which clashing items keep the Draft's version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Keep {
    /// Every item the pull is Blocked on.
    All,
    /// These; the pull stays Blocked if it clashes with any other.
    Items(Vec<ItemRef>),
}

/// What [`Repo::pull_keeping`] did. `kept` is empty unless the pull went
/// through with the Draft's version of those items on top.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct KeptPull {
    pub outcome: PullOutcome,
    pub kept: Vec<ItemRef>,
}

/// A kept version that isn't back in the Draft: it is in recovery as
/// `entry` (none for a file the Draft deleted, or a note), and `error`
/// says why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Unkept {
    pub item: ItemRef,
    pub entry: Option<String>,
    pub error: String,
}

/// One kept file, as journaled: `entry` holds the Draft's version of
/// `dest`; no entry means the Draft deleted it.
#[derive(Debug, Clone)]
pub(super) struct Kept {
    pub(super) item: ItemRef,
    pub(super) dest: PathBuf,
    pub(super) entry: Option<String>,
}

/// An index entry: its mode and object id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct IndexEntry(pub(super) String, pub(super) String);

/// How far a run got, for its report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Phase {
    Restoring,
    Pulling,
    Settling,
}

#[derive(Serialize, Deserialize)]
struct Stored {
    phase: Phase,
    kept: Vec<StoredKept>,
    /// Kept paths the pull's un-commit staged, and what it staged there.
    staged: Vec<StoredStaged>,
}

#[derive(Serialize, Deserialize)]
struct StoredKept {
    item: ItemRef,
    /// Git-quoted, so any bytes round-trip.
    dest: String,
    entry: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct StoredStaged {
    item: ItemRef,
    dest: String,
    entry: Option<IndexEntry>,
}

/// What [`Repo::restore_recording`] tells the journal as it goes.
pub(super) enum Event {
    /// The pull's un-commit staged `entry` at this kept path.
    Staged(ItemRef, PathBuf, Option<IndexEntry>),
    /// This file is about to move into recovery.
    Kept(Kept),
    /// This file is in recovery now.
    Moved(PathBuf),
    /// A Baseline file is staged, about to go in.
    Placing,
    /// The Baseline's file is in at this path.
    Published(PathBuf),
}

struct Journal {
    path: PathBuf,
    phase: Phase,
    kept: Vec<Kept>,
    staged: Vec<(ItemRef, PathBuf, Option<IndexEntry>)>,
}

impl Journal {
    fn path(repo: &Repo) -> Result<PathBuf> {
        let out = crate::git::Git::repo(repo.root())?.args(["rev-parse", "--absolute-git-dir"]).run()?;
        Ok(path_from_bytes(out.strip_suffix(b"\n").unwrap_or(&out)).join(FILE))
    }

    fn new(repo: &Repo) -> Result<Self> {
        Ok(Self { path: Self::path(repo)?, phase: Phase::Restoring, kept: Vec::new(), staged: Vec::new() })
    }

    fn load(repo: &Repo) -> Result<Option<Self>> {
        let path = Self::path(repo)?;
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).at(&path),
        };
        let parse = |message: String| Error::Parse { path: path.clone(), message };
        let stored: Stored = serde_json::from_str(&text).map_err(|e| parse(e.to_string()))?;
        let dest = |quoted: &str| {
            unquote_path(quoted).map(|raw| path_from_bytes(&raw)).ok_or_else(|| parse(format!("invalid path {quoted}")))
        };
        let kept = stored
            .kept
            .into_iter()
            .map(|k| Ok(Kept { dest: dest(&k.dest)?, item: k.item, entry: k.entry }))
            .collect::<Result<_>>()?;
        let staged = stored.staged.into_iter().map(|s| Ok((s.item, dest(&s.dest)?, s.entry))).collect::<Result<_>>()?;
        Ok(Some(Self { path, phase: stored.phase, kept, staged }))
    }

    fn save(&self) -> Result<()> {
        let quoted = |path: &PathBuf| quote_path(&path_bytes(path));
        let stored = Stored {
            phase: self.phase,
            kept: self
                .kept
                .iter()
                .map(|k| StoredKept { item: k.item.clone(), dest: quoted(&k.dest), entry: k.entry.clone() })
                .collect(),
            staged: self
                .staged
                .iter()
                .map(|(item, dest, entry)| StoredStaged {
                    item: item.clone(),
                    dest: quoted(dest),
                    entry: entry.clone(),
                })
                .collect(),
        };
        let body = serde_json::to_vec_pretty(&stored).map_err(|e| Error::Invalid(e.to_string()))?;
        crate::config_file::replace_private(&self.path, &body)
    }

    /// Clears the run's staging, then removes the journal.
    fn remove(&self) -> Result<()> {
        // What a restore that stopped left staged: only ever a copy of a
        // Baseline file, in a directory no one else writes to.
        if let Some(git_dir) = self.path.parent() {
            pull::clear_keep_stage(git_dir)?;
        }
        match fs::remove_file(&self.path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e).at(&self.path),
            _ => crate::config_file::sync_dir(&self.path),
        }
    }
}

/// See [`Repo::pull_keeping`].
///
/// A Blocked pull names only the clashes it found first: Draft changes,
/// then (once those are out of the way) unpushed commits it turns back into
/// Draft. So each Blocked outcome whose items `keep` covers is restored in
/// turn, into the same journal, and the pull tried again; one with an item
/// `keep` doesn't cover puts everything back and stays Blocked on all of them.
pub(super) fn run(repo: &Repo, lock: &crate::EngineLock, keep: &Keep) -> Result<KeptPull> {
    // An earlier run's journal becomes its report, which stays until acted on.
    finish(repo)?;
    // What can clash: the Draft, and what unpushed commits change (a pull
    // turns those back into Draft once it finds them in the way). Checked
    // before anything changes, so a name that can't clash moves nothing.
    if let Keep::Items(items) = keep {
        let draft = repo.draft()?;
        let unpushed = repo.unpushed_items()?;
        if let Some(other) =
            items.iter().find(|item| !draft.iter().any(|d| d.item == **item) && !unpushed.contains(*item))
        {
            return Err(Error::Invalid(format!(
                "`{other}` is neither in the draft nor in an unpushed commit, so it can't clash (see `sk1ll draft`)"
            )));
        }
    }
    // The user's staging, before the run or its pull changes the index.
    let user_staged = repo.user_staged_paths()?;
    let named = |item: &ItemRef| match keep {
        Keep::All => true,
        Keep::Items(items) => items.contains(item),
    };
    let mut chosen: Vec<ItemRef> = Vec::new();
    let mut journal: Option<Journal> = None;
    let outcome = loop {
        let before = rev(repo.git()?, "HEAD")?;
        if let Some(journal) = &mut journal {
            journal.phase = Phase::Pulling;
            journal.save()?;
            crate::failpoint!(KeepPull, repo.root());
        }
        let pulled = repo.pull_fetched(lock);
        let pending = repo.pull_pending()?;
        match pulled {
            Ok(outcome @ PullOutcome::Updated { .. }) if !pending => break outcome,
            Err(Error::Displaced { .. }) if !pending => {
                let to = rev(repo.git()?, "HEAD")?.unwrap_or_default();
                break PullOutcome::Updated { from: before.unwrap_or_default(), to };
            }
            Ok(PullOutcome::Blocked { baseline, items, uncommitted }) if !pending => {
                let fresh = !items.is_empty() && items.iter().all(|item| !chosen.contains(item));
                if fresh && items.iter().all(named) {
                    let journal = match &mut journal {
                        Some(journal) => journal,
                        None => {
                            let started = Journal::new(repo)?;
                            started.save()?;
                            crate::failpoint!(KeepRestore, repo.root());
                            journal.insert(started)
                        }
                    };
                    journal.phase = Phase::Restoring;
                    journal.save()?;
                    let set: BTreeSet<ItemRef> = items.iter().cloned().collect();
                    let mut moved = false;
                    let restored = repo.restore_recording(&items, &set, Some(&user_staged), &mut |event| match event {
                        Event::Staged(item, dest, entry) => {
                            journal.staged.push((item, dest.clone(), entry));
                            journal.save()?;
                            crate::failpoint!(KeepOwned, &dest);
                            Ok(())
                        }
                        Event::Kept(kept) => {
                            moved = true;
                            let dest = kept.dest.clone();
                            journal.kept.push(kept);
                            journal.save()?;
                            crate::failpoint!(KeepRecorded, &dest);
                            Ok(())
                        }
                        Event::Moved(path) => {
                            crate::failpoint!(KeepMoved, &path);
                            Ok(())
                        }
                        Event::Placing => {
                            crate::failpoint!(KeepStaged, repo.root());
                            Ok(())
                        }
                        Event::Published(path) => {
                            crate::failpoint!(KeepPublished, &path);
                            Ok(())
                        }
                    });
                    match restored {
                        Ok(_) => {}
                        // Refused before this round moved anything: the run puts back what earlier rounds kept.
                        Err(e) if !moved => {
                            return Err(rolled_back(repo, &format!("can't keep {}: {e}", names(&items))));
                        }
                        Err(e) => return Err(stop(repo, &format!("can't keep {}: {e}", names(&items)))),
                    }
                    chosen.extend(items);
                    continue;
                }
                // Items not named keep Blocked: nothing moves until every clash is handled.
                if let Some(journal) = &journal {
                    let unkept = settle(repo, journal)?;
                    if !unkept.is_empty() {
                        return Err(unkept_error("the pull is still blocked", &unkept));
                    }
                }
                let mut all: Vec<ItemRef> = chosen.into_iter().chain(items).collect();
                all.sort();
                all.dedup();
                let outcome = PullOutcome::Blocked { baseline, items: all, uncommitted };
                return Ok(KeptPull { outcome, kept: Vec::new() });
            }
            result if journal.is_none() => return result.map(|outcome| KeptPull { outcome, kept: Vec::new() }),
            result if pending => {
                let why = match result {
                    Err(e) => e.to_string(),
                    Ok(_) => "a Draft change appeared in its way".into(),
                };
                return Err(Error::Invalid(format!(
                    "the pull stopped half-way ({why}). Your version of {} is kept in recovery: `sk1ll pull` \
                     finishes the pull and lists each entry, for `sk1ll recover adopt <id>`",
                    names(&chosen)
                )));
            }
            Ok(_) => return Err(stop(repo, "the baseline moved back meanwhile")),
            Err(e) => return Err(stop(repo, &format!("can't pull: {e}"))),
        }
    };
    let Some(mut journal) = journal else { return Ok(KeptPull { outcome, kept: Vec::new() }) };
    journal.phase = Phase::Settling;
    journal.save()?;
    crate::failpoint!(KeepSettle, repo.root());
    let unkept = settle(repo, &journal)?;
    if !unkept.is_empty() {
        return Err(unkept_error("pulled onto the new baseline", &unkept));
    }
    chosen.sort();
    Ok(KeptPull { outcome, kept: chosen })
}

/// The run's own settling, of the journal it holds: puts each kept file
/// back (see [`put_back`]), then removes the journal. Returns what
/// couldn't go back, each still in recovery.
fn settle(repo: &Repo, journal: &Journal) -> Result<Vec<Unkept>> {
    let mut unkept = Vec::new();
    for kept in &journal.kept {
        if let Err(e) = put_back(repo, kept) {
            unkept.push(Unkept { item: kept.item.clone(), entry: kept.entry.clone(), error: e.to_string() });
        }
    }
    journal.remove()?;
    Ok(unkept)
}

/// Puts one kept file back, as the run that kept it: only where the path
/// holds exactly what the run left there (`HEAD`'s version, through
/// adopt's checks, or nothing where `HEAD` has no such file). One that
/// is vacant while `HEAD` has a file there was emptied by someone else.
fn put_back(repo: &Repo, kept: &Kept) -> Result<()> {
    let Some(id) = &kept.entry else { return pull::take_out(repo, &kept.dest) };
    let entry = displaced::get(id)?;
    let vacant = matches!(fs::symlink_metadata(&kept.dest), Err(e) if e.kind() == std::io::ErrorKind::NotFound);
    if vacant && in_head(repo, &kept.dest)? {
        return Err(Error::Invalid(format!(
            "{} was deleted while the keep ran, so it is left as it is",
            kept.dest.display()
        )));
    }
    pull::adopt(repo, &entry)
}

/// What an interrupted run left: `None` if there is no journal, or a pull
/// is pending (it waits for that); else the report of each kept version
/// still in recovery, saved (see [`report`]) before the journal goes.
/// Nothing is adopted: only the run's own staging is cleared, and an
/// index entry the journal names as the operation's own is reset where it
/// is still exactly that. Staging at a kept path the journal doesn't name
/// is left as it is, and reported.
pub(super) fn finish(repo: &Repo) -> Result<Option<Vec<Unkept>>> {
    let Some(journal) = Journal::load(repo)? else { return Ok(None) };
    if repo.pull_pending()? {
        return Ok(None);
    }
    let mut report = Vec::new();
    let note = |item: &ItemRef, error: String| Unkept { item: item.clone(), entry: None, error };
    for (item, dest, entry) in &journal.staged {
        let Ok(rel) = dest.strip_prefix(repo.root()) else { continue };
        let current = repo.index_entry(rel)?;
        if current == *entry {
            repo.git()?.literal().args(["reset", "--quiet", "--"]).arg(rel).run()?;
        } else if current != repo.head_entry(rel)? {
            let why =
                format!("{} was staged since the keep stopped, so its index entry is left as it is", rel.display());
            report.push(note(item, why));
        }
    }
    let when = match journal.phase {
        Phase::Restoring => "the keep stopped while restoring",
        Phase::Pulling => "the keep stopped before its pull went through",
        Phase::Settling => "the keep stopped after its pull",
    };
    for kept in &journal.kept {
        let Ok(rel) = kept.dest.strip_prefix(repo.root()) else { continue };
        let owned = journal.staged.iter().any(|(_, dest, _)| *dest == kept.dest);
        if !owned && repo.index_entry(rel)? != repo.head_entry(rel)? {
            let why = format!(
                "{} has staged content the keep didn't record as its own, so it is left as it is",
                rel.display()
            );
            report.push(note(&kept.item, why));
        }
        let error = match &kept.entry {
            Some(id) => match displaced::find(id) {
                Ok(Some(_)) => format!("{when}; your version is in recovery"),
                // Journaled, then stopped before the file moved: it is where
                // it was, and its empty entry goes as any abandoned one.
                Ok(None) => continue,
                Err(e) => format!("{when}; your version should be in recovery, but reading it failed ({e})"),
            },
            None => format!("{when}; your draft deleted {}", kept.dest.display()),
        };
        report.push(Unkept { item: kept.item.clone(), entry: kept.entry.clone(), error });
    }
    save_report(&journal.path, &report)?;
    journal.remove()?;
    Ok(Some(report))
}

fn report_path(journal: &std::path::Path) -> PathBuf {
    journal.with_file_name(REPORT)
}

/// Adds `report` to the saved one.
fn save_report(journal: &std::path::Path, report: &[Unkept]) -> Result<()> {
    if report.is_empty() {
        return Ok(());
    }
    let path = report_path(journal);
    let mut all = load_report(&path)?;
    all.extend(report.iter().filter(|u| !all.contains(u)).cloned().collect::<Vec<_>>());
    write_report(&path, &all)
}

fn load_report(path: &std::path::Path) -> Result<Vec<Unkept>> {
    match fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).map_err(|e| Error::Parse { path: path.into(), message: e.to_string() }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e).at(path),
    }
}

fn write_report(path: &std::path::Path, report: &[Unkept]) -> Result<()> {
    if report.is_empty() {
        return match fs::remove_file(path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e).at(path),
            _ => Ok(()),
        };
    }
    let body = serde_json::to_vec_pretty(report).map_err(|e| Error::Invalid(e.to_string()))?;
    crate::config_file::replace_private(path, &body)
}

/// The saved report of interrupted runs, as it stands: each kept version
/// still in recovery, and notes. A version adopted or discarded since drops
/// out; one whose recovery entry can't be read stays, to check again. Notes
/// (no recovery entry: staging left as it is, a deletion) stay until the
/// user dismisses them ([`dismiss_notes`]). Once nothing is left, it goes.
pub(super) fn report(repo: &Repo) -> Result<Vec<Unkept>> {
    report_with(repo, true)
}

/// [`report`], writing nothing: for a preview, which changes nothing on disk.
pub(super) fn peek_report(repo: &Repo) -> Result<Vec<Unkept>> {
    report_with(repo, false)
}

fn report_with(repo: &Repo, prune: bool) -> Result<Vec<Unkept>> {
    let path = report_path(&Journal::path(repo)?);
    let saved = load_report(&path)?;
    if saved.is_empty() {
        return Ok(saved);
    }
    let mut left = Vec::new();
    for unkept in &saved {
        let keep = match &unkept.entry {
            Some(id) => match displaced::find(id) {
                Ok(found) => found.is_some(),
                Err(_) => true,
            },
            None => true,
        };
        if keep {
            left.push(unkept.clone());
        }
    }
    if prune && left != saved {
        write_report(&path, &left)?;
    }
    Ok(left)
}

/// Dismisses the report's notes, the entries with no recovery entry, as the
/// user's explicit choice; versions in recovery stay until adopted or
/// discarded. Returns how many were dismissed.
pub(super) fn dismiss_notes(repo: &Repo) -> Result<usize> {
    let path = report_path(&Journal::path(repo)?);
    let saved = load_report(&path)?;
    let left: Vec<Unkept> = saved.iter().filter(|u| u.entry.is_some()).cloned().collect();
    write_report(&path, &left)?;
    Ok(saved.len() - left.len())
}

/// Whether `HEAD` has a file at `dest`, a path in `repo`.
fn in_head(repo: &Repo, dest: &std::path::Path) -> Result<bool> {
    let Ok(rel) = dest.strip_prefix(repo.root()) else { return Ok(false) };
    let spec = format!("HEAD:./{}", rel.to_string_lossy());
    repo.git()?.args(["cat-file", "-e", &spec]).succeeds()
}

/// A run stopped by an error once a file moved: reports what it kept, as
/// an interrupted run's journal is reported, and adopts nothing.
fn stop(repo: &Repo, why: &str) -> Error {
    match finish(repo) {
        Ok(Some(report)) if !report.is_empty() => stopped_error(why, &report),
        Ok(_) => Error::Invalid(format!("{why}. Nothing was changed")),
        Err(e) => Error::Invalid(format!("{why}, and reading its journal failed too ({e}); see `sk1ll recover list`")),
    }
}

/// A run refused before its current round moved anything: puts back what
/// earlier rounds kept, as the run.
fn rolled_back(repo: &Repo, why: &str) -> Error {
    let journal = match Journal::load(repo) {
        Ok(Some(journal)) => journal,
        Ok(None) => return Error::Invalid(format!("{why}. Nothing was changed")),
        Err(e) => return Error::Invalid(format!("{why}, and reading its journal failed too ({e})")),
    };
    match settle(repo, &journal) {
        Ok(unkept) if unkept.is_empty() => Error::Invalid(format!("{why}. Nothing was changed")),
        Ok(unkept) => unkept_error(why, &unkept),
        Err(e) => {
            Error::Invalid(format!("{why}, and putting your version back failed too ({e}); see `sk1ll recover list`"))
        }
    }
}

/// What an interrupted run's report says, for an error.
pub fn report_text(report: &[Unkept]) -> String {
    report
        .iter()
        .map(|u| match &u.entry {
            Some(id) => format!("{}: {} as {id}, `sk1ll recover adopt {id}`", u.item, u.error),
            None => format!("{}: {}", u.item, u.error),
        })
        .collect::<Vec<_>>()
        .join("; ")
}

fn stopped_error(why: &str, report: &[Unkept]) -> Error {
    Error::Invalid(format!("{why}. An interrupted keep left your versions in recovery: {}", report_text(report)))
}

fn unkept_error(what: &str, unkept: &[Unkept]) -> Error {
    let each: Vec<String> = unkept
        .iter()
        .map(|u| match &u.entry {
            Some(id) => format!(
                "your version of {} is kept in recovery as {id} ({}); finish with `sk1ll recover adopt {id}`",
                u.item, u.error
            ),
            None => format!("{} still has a file your version deletes ({}); `sk1ll diff {}`", u.item, u.error, u.item),
        })
        .collect();
    Error::Invalid(format!("{what}, but couldn't put everything back: {}", each.join("; ")))
}

fn names(items: &[ItemRef]) -> String {
    items.iter().map(ToString::to_string).collect::<Vec<_>>().join(", ")
}
