//! Plan/apply sync of skills and instructions into harnesses.
//!
//! `plan` is side-effect free so a front-end can show (and confirm) the diff;
//! `apply` re-checks ownership/state before touching anything, so a stale plan
//! can never delete or overwrite something sk1ll doesn't manage.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

use crate::assignment::Assignment;
use crate::hash::{HashCache, Stamp, copy_hash, hash_dir, hash_payload, hash_str};
use crate::instructions::{self, Rule};
use crate::machine::{Harness, InstructionsFormat, LinkMode};
use crate::skill::{ID_FILE, Origin, Skill};
use crate::{Error, IoExt, ItemId, Repo, Result};

mod ownership;
use ownership::{
    DirLock, FileId, INDEX_DIR, Intent, Ownership, Scratch, create_anchor, index_entry, load_index, mkdir_private,
    prune_indexes, retire_anchors, same_anchor,
};
pub use ownership::{INDEX_PATH, IndexEntry, PLACEMENT_MARKER};
#[cfg(test)]
use ownership::{INDEX_VERSION, Index, save_index};

/// Names excluded from skill content at every depth.
pub const SKILL_CONTENT_EXCLUSIONS: &[&str] = &[".git"];

/// What a skill's tree leaves out, by the depth below its root (1: directly
/// inside it) and name of an entry.
pub type Exclusion = fn(usize, &std::ffi::OsStr) -> bool;

/// What every skill copy and hash leaves out: [`SKILL_CONTENT_EXCLUSIONS`],
/// and the placement marker at the root of a harness copy. All an imported
/// skill's other content is vendored, `.sk1ll-id` included.
pub fn excluded_from_skill(depth: usize, name: &std::ffi::OsStr) -> bool {
    SKILL_CONTENT_EXCLUSIONS.iter().any(|excluded| name == *excluded) || (depth == 1 && name == PLACEMENT_MARKER)
}

/// What a personal skill's copies and hashes also leave out: its Item ID
/// sidecar, which identifies the repo item and is never sent to a harness.
pub fn excluded_from_personal_skill(depth: usize, name: &std::ffi::OsStr) -> bool {
    excluded_from_skill(depth, name) || name == ID_FILE
}

fn import_source(path: &Path) -> bool {
    path.parent().and_then(Path::file_name).is_some_and(|dir| dir == crate::skill::IMPORTED_DIR)
}

/// What a copy of the repo skill at `source` leaves out.
pub(crate) fn source_exclusion(source: &Path) -> Exclusion {
    if import_source(source) { excluded_from_skill } else { excluded_from_personal_skill }
}

/// Hash of the content a copy placement would send to a harness.
pub fn deploy_hash(source: &Path) -> Result<String> {
    if import_source(source) { copy_hash(source) } else { hash_dir(source) }
}

/// How every conflict over managed content edited by hand in a harness
/// ends its reason. What to run about it is the client's to say: the CLI
/// names the command, the GUI has its buttons.
pub const EDITED: &str = "edited by hand in the harness";
const BLOCK_EDITED: &str = "managed block edited by hand in the harness";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ActionKind {
    Create,
    Update,
    Remove,
    /// Something sk1ll doesn't manage (or a hand-edited managed block) is in
    /// the way; left alone.
    Conflict,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Item {
    Skill {
        name: String,
        mode: LinkMode,
        source: Option<PathBuf>,
        #[serde(skip_serializing_if = "Option::is_none")]
        item_id: Option<ItemId>,
        #[serde(skip_serializing_if = "std::ops::Not::not")]
        metadata_only: bool,
        /// Destination content hash or link identity observed while planning (`None`: absent).
        expected: Option<String>,
        expected_index: Option<IndexEntry>,
    },
    Instructions {
        rules: Vec<String>,
        content: Option<String>,
        /// Hash of the managed block as seen when planning; `apply` refuses
        /// to write if the file changed since.
        expected: Option<String>,
    },
    /// One rule as a file in a rules directory.
    Rule {
        file: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        item_id: Option<ItemId>,
        #[serde(skip_serializing_if = "std::ops::Not::not")]
        metadata_only: bool,
        /// `None` removes the file.
        content: Option<String>,
        /// Hash of the file as seen when planning (`None`: absent).
        expected: Option<String>,
        expected_index: Option<IndexEntry>,
    },
}

/// A file a harness would receive, for previewing.
#[derive(Debug, Clone, Serialize)]
pub struct RenderedFile {
    pub path: PathBuf,
    pub content: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Action {
    pub harness: String,
    pub kind: ActionKind,
    pub dest: PathBuf,
    pub item: Item,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct PlanOptions {
    /// Overwrite hand edits to managed content (copied skills, rule files,
    /// instruction blocks). Never affects content sk1ll doesn't manage.
    pub force: bool,
}

enum Existing {
    Missing,
    ManagedLink(PathBuf),
    ManagedCopy(String),
    Unmanaged(String),
}

pub mod unmanaged;

/// Repo content, resolved once per plan and filtered per harness by assignment.
struct Selection<'c> {
    skills: Vec<Skill>,
    rules: Vec<Rule>,
    /// Content hash of each repo skill a harness was compared against so
    /// far, by skill path: see [`Selection::source_hash`].
    source_hashes: RefCell<BTreeMap<PathBuf, String>>,
    /// Where planning finds the hashes of trees unchanged since an earlier
    /// plan (see [`plan_cached`]).
    cache: Option<&'c HashCache>,
}

impl<'c> Selection<'c> {
    fn new(repo: &Repo, cache: Option<&'c HashCache>) -> Result<Self> {
        let (skills, rules) = repo.content()?;
        Ok(Self { skills, rules, source_hashes: RefCell::default(), cache })
    }

    /// One look at a skill destination, for planning.
    fn observe(&self, dest: &Path) -> Result<Observed> {
        observe_skill_with(dest, self.cache)
    }

    /// Content hash of a repo skill, taken once per plan however many
    /// harnesses receive the skill. Every harness is thus judged against the
    /// same observation of the source; a source edited later in the plan is
    /// what the next plan sees, as with one edited right after it. `apply`
    /// never relies on this hash: it hashes the source it copies.
    fn source_hash(&self, skill: &Skill) -> Result<String> {
        if let Some(hash) = self.source_hashes.borrow().get(&skill.path) {
            return Ok(hash.clone());
        }
        let hash = match self.cache {
            Some(cache) => cache.deploy_hash(&skill.path)?,
            None => deploy_hash(&skill.path)?,
        };
        self.source_hashes.borrow_mut().insert(skill.path.clone(), hash.clone());
        Ok(hash)
    }

    fn skills_for(&self, assignment: &Assignment, harness: &str) -> Vec<&Skill> {
        let a = assignment.harness(harness);
        // An item without an Item ID yet has no toggle: the default applies.
        self.skills.iter().filter(|s| s.id.as_ref().map_or(a.new_skills_default, |id| a.skill_enabled(id))).collect()
    }

    /// Assignment picks rules by Item ID; a rule's own `harnesses:` restricts further.
    fn rules_for(&self, assignment: &Assignment, harness: &str) -> Vec<&Rule> {
        let a = assignment.harness(harness);
        self.rules
            .iter()
            .filter(|f| f.applies_to(harness) && f.id.as_ref().map_or(a.new_rules_default, |id| a.rule_enabled(id)))
            .collect()
    }
}

/// What sync would do to bring `harnesses` in line with the repo and
/// `assignment`. Side-effect free.
pub fn plan(
    repo: &Repo,
    harnesses: &BTreeMap<String, Harness>,
    assignment: &Assignment,
    opts: PlanOptions,
) -> Result<Vec<Action>> {
    plan_with(repo, harnesses, assignment, opts, None)
}

/// [`plan`], taking the hashes of skill trees (repo sources and harness
/// placements) unchanged since an earlier plan from `cache`. The plan is the
/// same as [`plan`]'s, except that a change [`HashCache`] cannot see (one
/// within its racy window) waits for the next plan; `apply` checks every
/// action against the destination as it is, not as the cache has it.
pub fn plan_cached(
    repo: &Repo,
    harnesses: &BTreeMap<String, Harness>,
    assignment: &Assignment,
    opts: PlanOptions,
    cache: &HashCache,
) -> Result<Vec<Action>> {
    plan_with(repo, harnesses, assignment, opts, Some(cache))
}

fn plan_with(
    repo: &Repo,
    harnesses: &BTreeMap<String, Harness>,
    assignment: &Assignment,
    opts: PlanOptions,
    cache: Option<&HashCache>,
) -> Result<Vec<Action>> {
    let sel = Selection::new(repo, cache)?;
    let mut actions = Vec::new();
    for (hname, harness) in harnesses {
        plan_skills(repo, hname, harness, &sel, &sel.skills_for(assignment, hname), opts, &mut actions)?;
        if let Some(path) = harness.instructions_file() {
            let rules = sel.rules_for(assignment, hname);
            match harness.instructions_format {
                InstructionsFormat::Block => plan_instructions(hname, &path, &rules, opts, &mut actions)?,
                InstructionsFormat::Rules => plan_rules(repo, hname, &path, &rules, opts, &mut actions)?,
            }
        }
    }
    Ok(actions)
}

/// A cheap fingerprint of everything in `harness` that planning its sync
/// reads, from `lstat` alone (see [`Stamp`]): its skills directory's own
/// entries, the whole tree of each one its ownership index records (and of
/// a change in progress), the index and sk1ll's scratch and recovery area,
/// and its instructions file or rules directory, with the scratch and
/// recovery area beside a block instructions file (and beside its target).
/// What planning reads through a symlink (a skills directory, an
/// instructions file or rules directory that is one) is stamped through it
/// ([`Stamp::through`]). The harness's own settings (paths, mode, format) are
/// part of it too. Unmanaged content below a skills directory's own entries
/// is left out, since planning does not look into it. Equal fingerprints mean
/// planning would see the same harness; one that is not
/// [settled](Stamp::settled) proves nothing yet.
pub fn harness_fingerprint(harness: &Harness) -> Result<Stamp> {
    let mut parts = Vec::new();
    if let Some(dir) = harness.skills_dir() {
        fn own_entries(depth: usize, _: &std::ffi::OsStr) -> bool {
            depth > 1
        }
        parts.push(("skills".to_string(), Stamp::through(&dir, Some(own_entries))?));
        let index = load_index(&dir)?;
        let names: BTreeSet<&str> =
            index.entries.keys().chain(index.pending.keys()).map(String::as_str).chain([INDEX_DIR, TMP_DIR]).collect();
        for name in names {
            parts.push((format!("skills/{name}"), Stamp::of(&dir.join(name), None, false)?));
        }
    }
    if let Some(path) = harness.instructions_file() {
        parts.push(("instructions".to_string(), Stamp::through(&path, None)?));
        if harness.instructions_format == InstructionsFormat::Block {
            // Block instructions are displaced into the directory holding
            // the file, or its target if it is a symlink.
            let mut holders: Vec<PathBuf> = path.parent().map(Path::to_path_buf).into_iter().collect();
            if let Ok(target) = fs::canonicalize(&path)
                && let Some(parent) = target.parent()
            {
                holders.push(parent.to_path_buf());
            }
            holders.dedup();
            for holder in holders {
                let tmp = holder.join(TMP_DIR);
                parts.push((format!("beside {}", holder.display()), Stamp::of(&tmp, None, false)?));
            }
        }
    }
    Ok(Stamp::combine(&format!("{harness:?}"), parts))
}

/// The instruction files a harness would receive, for previewing.
pub fn render_instructions(
    repo: &Repo,
    harnesses: &BTreeMap<String, Harness>,
    assignment: &Assignment,
    harness: &str,
) -> Result<Vec<RenderedFile>> {
    let h =
        harnesses.get(harness).ok_or_else(|| Error::Invalid(format!("unknown harness `{harness}` on this machine")))?;
    let Some(path) = h.instructions_file() else {
        return Ok(Vec::new());
    };
    let sel = Selection::new(repo, None)?;
    let rules = sel.rules_for(assignment, harness);
    Ok(match h.instructions_format {
        InstructionsFormat::Block => {
            instructions::render(&rules).map(|content| RenderedFile { path, content }).into_iter().collect()
        }
        InstructionsFormat::Rules => rules
            .iter()
            .map(|f| {
                let (file, content) = instructions::render_rule(f);
                RenderedFile { path: path.join(file), content }
            })
            .collect(),
    })
}

/// Read the destination and construct the complete file sync would write.
pub fn instruction_file_text(file: &RenderedFile, format: InstructionsFormat) -> Result<(Option<String>, String)> {
    let before = instructions::read_optional(&file.path)?;
    let next = match format {
        InstructionsFormat::Rules => file.content.clone(),
        InstructionsFormat::Block => {
            let text = before.as_deref().unwrap_or_default();
            let block = instructions::find_block(text, &file.path)?;
            if block.as_ref().is_some_and(|block| block.inner == file.content) {
                text.to_string()
            } else {
                instructions::splice(text, block.as_ref(), Some(&file.content))
            }
        }
    };
    Ok((before, next))
}

/// What sk1ll placed in `harness`: each managed skill, rule file and
/// instructions block as the Remove sync would make of it were it turned
/// off, or as a Conflict when it was edited by hand. Content sk1ll doesn't
/// manage is left out. Side-effect free; harness changes list it with this.
pub fn placed(repo: &Repo, hname: &str, harness: &Harness) -> Result<Vec<Action>> {
    let sel = Selection { skills: Vec::new(), rules: Vec::new(), source_hashes: RefCell::default(), cache: None };
    let opts = PlanOptions::default();
    let mut skills = Vec::new();
    plan_skills(repo, hname, harness, &sel, &[], opts, &mut skills)?;
    let mut actions = Vec::new();
    for a in skills {
        let Item::Skill { expected, expected_index, .. } = &a.item else {
            unreachable!("skill planning produces skill actions");
        };
        if a.kind == ActionKind::Remove && expected.is_none() {
            continue;
        }
        if a.kind == ActionKind::Conflict
            && matches!(classify(repo.root(), &a.dest, None, expected_index.as_ref())?, Existing::Unmanaged(_))
        {
            continue;
        }
        actions.push(a);
    }
    if let Some(path) = harness.instructions_file() {
        match harness.instructions_format {
            InstructionsFormat::Block => plan_instructions(hname, &path, &[], opts, &mut actions)?,
            InstructionsFormat::Rules => plan_rules(repo, hname, &path, &[], opts, &mut actions)?,
        }
    }
    actions.retain(|a| !matches!(&a.item, Item::Rule { expected: None, .. }));
    Ok(actions)
}

pub fn apply(repo: &Repo, action: &Action) -> Result<()> {
    apply_reviewed(repo, action, None)
}

/// Like [`apply`], only while the content it places is still as reviewed.
/// Refuses ([`Error::PullPending`]) while a pull is pending: its tree is
/// partial, so placements wait until it is finished.
pub fn apply_reviewed(repo: &Repo, action: &Action, reviewed_digest: Option<&str>) -> Result<()> {
    repo.require_no_pending_pull()?;
    match &action.item {
        Item::Skill { .. } => apply_skill(repo, action, false, reviewed_digest),
        Item::Instructions { content, expected, .. } => {
            apply_instructions(repo, action, content.as_deref(), expected.as_deref())
        }
        Item::Rule { .. } => apply_rule(repo, action),
    }
}

fn plan_skills(
    repo: &Repo,
    hname: &str,
    harness: &Harness,
    sel: &Selection<'_>,
    skills: &[&Skill],
    opts: PlanOptions,
    actions: &mut Vec<Action>,
) -> Result<()> {
    let Some(dir) = harness.skills_dir() else {
        return Ok(());
    };
    let dir = dir.as_path();
    // `seen` is the one observation of the destination that the action was
    // chosen from; it becomes the precondition `apply` checks. Observing again
    // here would accept an edit made meanwhile without ever judging it.
    let raw = load_index(dir)?;
    let index = raw.clone().owned(dir)?;
    // A change in progress that recovery will settle as made: its prepared
    // placement record is the one that counts.
    let made = |name: &str| raw.pending.contains_key(name) && index.get(name) != raw.entries.get(name);
    let action = |name: &str, kind, source: Option<&Skill>, seen: Observed, reason: Option<String>| Action {
        harness: hname.to_string(),
        kind,
        dest: dir.join(name),
        item: Item::Skill {
            name: name.to_string(),
            mode: harness.mode,
            source: source.map(|skill| skill.path.clone()),
            item_id: source.and_then(|skill| skill.id.clone()),
            metadata_only: false,
            expected: if kind == ActionKind::Conflict { None } else { seen.state },
            expected_index: index.get(name).cloned(),
        },
        reason,
    };
    let edited = || Some((ActionKind::Conflict, Some(format!("copied skill {EDITED}"))));
    let mut seen_names = BTreeSet::new();
    for skill in skills {
        if skill.name == INDEX_DIR {
            continue;
        }
        seen_names.insert(skill.name.clone());
        let dest = dir.join(&skill.name);
        let seen = sel.observe(&dest)?;
        let recorded = index.get(&skill.name);
        if let Err(e) = refuse_source_marker(&skill.path) {
            actions.push(action(&skill.name, ActionKind::Conflict, Some(skill), seen, Some(e.to_string())));
            continue;
        }
        let kind = match classify(repo.root(), &dest, Some(&skill.path), recorded)? {
            Existing::Missing => Some((ActionKind::Create, None)),
            Existing::ManagedLink(to) => {
                let changed = recorded.is_some_and(|entry| seen.edited(entry));
                if changed && !opts.force {
                    Some((ActionKind::Conflict, Some(format!("skill link {EDITED}"))))
                } else {
                    let matches_source =
                        harness.mode == LinkMode::Symlink && to == skill.path.canonicalize().at(&skill.path)?;
                    (!matches_source || recorded.is_none() || changed).then_some((ActionKind::Update, None))
                }
            }
            Existing::ManagedCopy(rec) => {
                let changed = recorded.is_some_and(|entry| seen.edited(entry));
                #[cfg(test)]
                hook(Hook::PlanSource, &dest);
                let want = sel.source_hash(skill)?;
                if changed && !opts.force {
                    edited()
                } else if changed || harness.mode != LinkMode::Copy || rec != want {
                    Some((ActionKind::Update, None))
                } else {
                    None
                }
            }
            Existing::Unmanaged(why) => Some((ActionKind::Conflict, Some(why))),
        };
        if let Some((kind, reason)) = kind {
            actions.push(action(&skill.name, kind, Some(skill), seen, reason));
        } else if displaced::placement_needs_id(
            repo,
            displaced::Kind::Skill,
            &dest,
            &skill.path,
            skill.id.as_ref(),
            made(&skill.name),
        )? {
            let mut record = action(&skill.name, ActionKind::Update, Some(skill), seen, None);
            if let Item::Skill { metadata_only, .. } = &mut record.item {
                *metadata_only = true;
            }
            actions.push(record);
        }
    }

    if dir.is_dir() {
        for entry in fs::read_dir(dir).at(dir)? {
            let entry = entry.at(dir)?;
            let name = entry.file_name().to_string_lossy().into_owned();
            // sk1ll's own entries: the index, and scratch or leftovers
            // of an interrupted sync (see `recover`), never skills.
            if name.starts_with(".sk1ll-") || !seen_names.insert(name.clone()) {
                continue;
            }
            let recorded = index.get(&name);
            if recorded.is_none() && !entry.file_type().at(entry.path())?.is_symlink() {
                continue;
            }
            let seen = sel.observe(&entry.path())?;
            match classify(repo.root(), &entry.path(), None, recorded)? {
                Existing::ManagedCopy(_) | Existing::ManagedLink(_)
                    if !opts.force && recorded.is_some_and(|entry| seen.edited(entry)) =>
                {
                    actions.push(action(&name, ActionKind::Conflict, None, seen, Some(format!("skill {EDITED}"))));
                }
                Existing::ManagedLink(_) | Existing::ManagedCopy(_) => {
                    actions.push(action(&name, ActionKind::Remove, None, seen, None));
                }
                Existing::Unmanaged(why) if entry.file_type().at(entry.path())?.is_symlink() => {
                    actions.push(action(&name, ActionKind::Conflict, None, seen, Some(why)));
                }
                _ => {}
            }
        }
    }
    for (name, entry) in &index {
        if !name.starts_with(".sk1ll-") && !seen_names.contains(name) && !matches!(entry, IndexEntry::Rule { .. }) {
            actions.push(action(name, ActionKind::Remove, None, Observed { state: None, payload: None }, None));
        }
    }
    Ok(())
}

fn plan_instructions(
    hname: &str,
    file: &Path,
    rules: &[&Rule],
    opts: PlanOptions,
    actions: &mut Vec<Action>,
) -> Result<()> {
    let content = instructions::render(rules);
    let text = instructions::read_optional(file)?;
    let block = match &text {
        Some(text) => instructions::find_block(text, file)?,
        None => None,
    };
    let (kind, reason) = match (&block, &content) {
        (None, None) => return Ok(()),
        (Some(b), Some(c)) if b.inner == *c => return Ok(()),
        (Some(b), _) if b.edited() && !opts.force => (ActionKind::Conflict, Some(BLOCK_EDITED.into())),
        (None, Some(_)) => (ActionKind::Create, None),
        (Some(_), Some(_)) => (ActionKind::Update, None),
        (Some(_), None) => (ActionKind::Remove, None),
    };
    actions.push(Action {
        harness: hname.to_string(),
        kind,
        dest: file.to_path_buf(),
        item: Item::Instructions {
            rules: rules.iter().map(|f| f.name.clone()).collect(),
            content,
            expected: block.map(|b| hash_str(&b.inner)),
        },
        reason,
    });
    Ok(())
}

fn plan_rules(
    repo: &Repo,
    hname: &str,
    dir: &Path,
    rules: &[&Rule],
    opts: PlanOptions,
    actions: &mut Vec<Action>,
) -> Result<()> {
    let raw = load_index(dir)?;
    let index = raw.clone().owned(dir)?;
    // A change in progress that recovery will settle as made: its prepared
    // placement record is the one that counts.
    let made = |name: &str| raw.pending.contains_key(name) && index.get(name) != raw.entries.get(name);
    let action =
        |file: &str, kind, item_id, content: Option<String>, expected: Option<String>, reason: Option<String>| Action {
            harness: hname.to_string(),
            kind,
            dest: dir.join(file),
            item: Item::Rule {
                file: file.to_string(),
                item_id,
                metadata_only: false,
                content,
                expected,
                expected_index: index.get(file).cloned(),
            },
            reason,
        };
    let foreign = || Some(format!("{NOT_A_FILE} where sk1ll placed the rule; left alone"));
    // The hash the index records for `file`, if what is there is the very
    // file it records: any other, even with the same text, is someone else's.
    let recorded = |file: &str, seen: &RuleSeen| -> Result<Option<&IndexEntry>> {
        match (index.get(file), seen) {
            (Some(entry @ IndexEntry::Rule { file: placed, .. }), RuleSeen::File { file: at, .. })
                if placed == at && same_anchor(dir, entry, &dir.join(file))? =>
            {
                Ok(Some(entry))
            }
            _ => Ok(None),
        }
    };

    let mut desired = BTreeSet::new();
    for rule in rules {
        let (file, content) = instructions::render_rule(rule);
        let want = hash_str(&content);
        let seen = rule_seen(&dir.join(&file))?;
        let (kind, reason) = match &seen {
            None => (ActionKind::Create, None),
            Some(RuleSeen::Other(_)) => (ActionKind::Conflict, foreign()),
            Some(at @ RuleSeen::File { payload, .. }) => match recorded(&file, at)? {
                None => (ActionKind::Conflict, Some("existing rule file not managed by sk1ll".into())),
                Some(entry) if payload != entry.payload() && !opts.force => {
                    (ActionKind::Conflict, Some(format!("rule {EDITED}")))
                }
                Some(IndexEntry::Rule { hash, payload: recorded, .. }) if payload == recorded && hash == &want => {
                    if displaced::placement_needs_id(
                        repo,
                        displaced::Kind::Rule,
                        &dir.join(&file),
                        &rule.path,
                        rule.id.as_ref(),
                        made(&file),
                    )? {
                        let state = Some(at.state());
                        let mut record = action(&file, ActionKind::Update, rule.id.clone(), Some(content), state, None);
                        if let Item::Rule { metadata_only, .. } = &mut record.item {
                            *metadata_only = true;
                        }
                        actions.push(record);
                    }
                    desired.insert(file);
                    continue;
                }
                Some(_) => (ActionKind::Update, None),
            },
        };
        actions.push(action(&file, kind, rule.id.clone(), Some(content), seen.as_ref().map(RuleSeen::state), reason));
        desired.insert(file);
    }

    for (file, entry) in &index {
        if !matches!(entry, IndexEntry::Rule { .. }) || desired.contains(file) {
            continue;
        }
        let seen = rule_seen(&dir.join(file))?;
        let (kind, reason) = match &seen {
            None => (ActionKind::Remove, None),
            Some(RuleSeen::Other(_)) => (ActionKind::Conflict, foreign()),
            Some(at @ RuleSeen::File { payload, .. }) => match recorded(file, at)? {
                // Another file took the name: nothing of sk1ll's is there to remove.
                None => continue,
                Some(entry) if payload != entry.payload() && !opts.force => {
                    (ActionKind::Conflict, Some(format!("rule {EDITED}")))
                }
                Some(_) => (ActionKind::Remove, None),
            },
        };
        actions.push(action(file, kind, None, None, seen.as_ref().map(RuleSeen::state), reason));
    }
    Ok(())
}

/// `result` of a change to the placement at `dest`, with the Item ID it now
/// has recorded once the change stood (a displaced edit is still reported).
fn record_placed(result: Result<()>, record: impl FnOnce() -> Result<()>) -> Result<()> {
    match result {
        Ok(()) => record(),
        Err(e @ Error::Displaced { .. }) => {
            record()?;
            Err(e)
        }
        Err(e) => Err(e),
    }
}

/// `result` of a change through [`swap_in`] to the placement `name` at
/// `dest`, with the record [`displaced::prepare_placement`] prepared for it
/// settled by what the index says now: made if its entry is no longer
/// `before`. While the index still has the change's intent, the recovery
/// that settles the intent settles the record too.
fn settle_placed(
    lock: &DirLock,
    kind: displaced::Kind,
    name: &str,
    dest: &Path,
    before: Option<&IndexEntry>,
    result: Result<()>,
) -> Result<()> {
    let settled = (|| {
        let index = load_index(lock.dir)?;
        if index.pending.contains_key(name) {
            return Ok(());
        }
        displaced::settle_placement(kind, dest, index.entries.get(name) != before)
    })();
    result.and(settled)
}

fn apply_rule(repo: &Repo, action: &Action) -> Result<()> {
    let Item::Rule { file, item_id, content, expected, expected_index, metadata_only } = &action.item else {
        unreachable!("only Rule placements reach apply_rule");
    };
    let (content, expected, expected_index) = (content.as_deref(), expected.as_deref(), expected_index.as_ref());
    let source = repo.root().join(instructions::INSTRUCTIONS_DIR).join(file);
    if action.kind == ActionKind::Conflict {
        return Ok(());
    }
    let dest = &action.dest;
    let dir = dest.parent().ok_or_else(|| Error::Invalid(format!("{} has no parent", dest.display())))?;
    let lock = DirLock::acquire(dir)?;
    lock.check_planned(dest, file, expected_index)?;
    recover_dir(&lock, &action.harness, DirectoryKind::Rules)?;
    let ownership = Ownership::new(&lock, dest, file, expected_index);
    ownership.check()?;
    // The state is the file's identity as well as its text: another file
    // put there since the plan, even with the same text, fails this.
    let current = rule_state(dest)?;
    if current.as_deref() != expected {
        return Err(changed_since_plan(dest));
    }
    if current.as_deref().is_some_and(|c| c.starts_with(NOT_A_FILE)) {
        return Err(Error::Invalid(format!("refusing to touch {}: {NOT_A_FILE}", dest.display())));
    }
    if current.is_some() && !expected_index.is_some_and(|entry| matches!(entry, IndexEntry::Rule { .. })) {
        return Err(changed_since_plan(dest));
    }
    if let (Some(_), Some(entry)) = (&current, expected_index)
        && !holds(dest, entry)?
    {
        return Err(changed_since_plan(dest));
    }
    if *metadata_only {
        // Only the placement's Item ID record changes: the file must be the one sk1ll placed.
        if current.is_none() {
            return Err(changed_since_plan(dest));
        }
        return displaced::record_placement(repo, displaced::Kind::Rule, dest, Some(&source), item_id.as_ref());
    }
    let record = |placed: bool| {
        let source = placed.then_some(source.as_path());
        displaced::record_placement(repo, displaced::Kind::Rule, dest, source, item_id.as_ref())
    };
    let prepare = |placed: bool| {
        let source = placed.then_some(source.as_path());
        displaced::prepare_placement(repo, displaced::Kind::Rule, dest, source, item_id.as_ref())
    };
    let displace = Displace {
        repo,
        kind: displaced::Kind::Rule,
        harness: &action.harness,
        expected,
        ownership: Some(&ownership),
        baseline: None,
    };
    let outcome = match content {
        Some(content) => {
            let scratch = Scratch::open(dir)?;
            let stage = scratch.path().join(format!("stage-{file}"));
            crate::config_file::write_new(&stage, content.as_bytes())?;
            let result = (|| {
                let Some(RuleSeen::File { file, hash, payload }) = rule_seen(&stage)? else {
                    return Err(changed_since_plan(&stage));
                };
                let next = IndexEntry::Rule { file, hash, payload };
                create_anchor(dir, &next, &stage)?;
                crate::config_file::sync_dir(&stage)?;
                prepare(true)?;
                swap_in(Some(&stage), dest, displace, Some(Intent::Place { next, recovery: None }))
            })();
            let _ = fs::remove_file(stage);
            result
        }
        None if current.is_some() => {
            prepare(false).and_then(|()| swap_in(None, dest, displace, Some(Intent::Remove { recovery: None })))
        }
        None => return record_placed(ownership.record(None), || record(false)),
    };
    let finished = ownership.finish(outcome);
    let result = settle_placed(&lock, displaced::Kind::Rule, file, dest, expected_index, finished);
    retire_anchors(&lock)?;
    result
}

fn apply_instructions(repo: &Repo, action: &Action, content: Option<&str>, expected: Option<&str>) -> Result<()> {
    if action.kind == ActionKind::Conflict {
        return Ok(());
    }
    let dest = &action.dest;
    recover_file_siblings(&action.harness, dest)?;
    let existing = instructions::read_optional(dest)?;
    let text = existing.clone().unwrap_or_default();
    let block = instructions::find_block(&text, dest)?;
    if block.as_ref().map(|b| hash_str(&b.inner)).as_deref() != expected {
        return Err(changed_since_plan(dest));
    }
    // The whole file must still be what was spliced, or edits made outside
    // the block in the meantime would be lost too.
    let unchanged = || match instructions::read_optional(dest)?.unwrap_or_default() == text {
        true => Ok(()),
        false => Err(changed_since_plan(dest)),
    };
    let whole = hash_str(&text);
    let displace = Displace {
        repo,
        kind: displaced::Kind::Instructions,
        harness: &action.harness,
        expected: Some(&whole),
        ownership: None,
        baseline: None,
    };
    let spliced = instructions::splice(&text, block.as_ref(), content);
    // A file sync created stays sync's only while it holds exactly what
    // sync last wrote: anything else (an edit anywhere, a file replaced)
    // makes it the user's for good, and it is never removed.
    let created = displaced::created_block(dest)?;
    let owned = match &created {
        Some(created) => existing.is_some() && !is_symlink(dest) && created.is(dest)?,
        None => false,
    };
    if created.is_some() && !owned {
        displaced::record_created_block(dest, None)?;
    }
    if let (None, true, Some(created)) = (content, owned && spliced.trim().is_empty(), &created) {
        return remove_created_block(repo, &action.harness, dest, created, unchanged);
    }
    // Splicing keeps every byte outside the block. A block that is still
    // what sk1ll wrote (or none) is sk1ll's own, so the rewrite displaces
    // nothing anyone wrote and leaves no recovery entry; a forced rewrite
    // of a hand-edited block keeps the old file there for good.
    let old = match block.as_ref().is_some_and(instructions::Block::edited) {
        false => OldCopy::DropIfIntact,
        true => OldCopy::KeepMarked,
    };
    let (displaced, published) = replace_file_published(dest, &spliced, unchanged, displace, old)?;
    // Recorded after the write: a crash in between leaves no record, and
    // the file stays, as one that sync didn't create would.
    if (existing.is_none() || owned) && !is_symlink(dest) {
        let created =
            published.map(|(file, payload)| displaced::CreatedBlock { written: hash_str(&spliced), file, payload });
        displaced::record_created_block(dest, created.as_ref())?;
    }
    displaced.map_or(Ok(()), Err)
}

fn is_symlink(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink())
}

/// Takes out a block instructions file sync created once its block is the
/// last of it: under the directory's lock, only while it is still the very
/// file sync published, unchanged (identity, text, bytes and mode), and
/// into the recovery area as every removal goes, so a write that races it
/// is kept. The file is checked once more where it was moved to: if it
/// isn't sync's after all, it goes back (or, if something took its place,
/// stays in recovery for good), and nothing is removed.
fn remove_created_block(
    repo: &Repo,
    harness: &str,
    dest: &Path,
    created: &displaced::CreatedBlock,
    unchanged: impl FnOnce() -> Result<()>,
) -> Result<()> {
    let holder = dest.parent().ok_or_else(|| Error::Invalid(format!("{} has no parent", dest.display())))?;
    let _lock = DirLock::acquire(holder)?;
    unchanged()?;
    if !created.is(dest)? {
        return Err(changed_since_plan(dest));
    }
    let kind = displaced::Kind::Instructions;
    // Its backup holds only what sync wrote: pruned once unchanged and old enough.
    let payload = Some(created.payload.as_str());
    let mut held =
        displaced::Displacing::begin(Some(repo), kind, harness, dest, Some(&created.written), payload, holder)?;
    #[cfg(test)]
    hook(Hook::BeforeSwap, dest);
    if let Err(e) = held.take(dest) {
        held.abandon();
        return Err(e);
    }
    #[cfg(test)]
    hook(Hook::AfterAside, dest);
    if !created.is(&held.content())? {
        held.put_back()?;
        return Err(changed_since_plan(dest));
    }
    let displaced = held.finish(false)?;
    displaced::record_created_block(dest, None)?;
    displaced.map_or(Ok(()), Err)
}

/// What [`replace_file`] does with the recovery entry of the file it replaced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OldCopy {
    /// Keep it, pruned like any other once unchanged (if it can be).
    Keep,
    /// It held only content sk1ll owns: drop it at once if nothing wrote to
    /// it during the swap.
    DropIfIntact,
    /// It held a hand edit sk1ll was told to overwrite: keep it, marked
    /// changed, until it is adopted or discarded.
    KeepMarked,
}

fn apply_skill(repo: &Repo, action: &Action, adopted: bool, reviewed_digest: Option<&str>) -> Result<()> {
    let Item::Skill { name, mode, source, item_id, expected, expected_index, metadata_only } = &action.item else {
        unreachable!("only skill placements reach apply_skill");
    };
    let (name, mode, source, expected) = (name.as_str(), *mode, source.as_deref(), expected.as_deref());
    if action.kind == ActionKind::Conflict {
        return Ok(());
    }
    let dest = &action.dest;
    let dir = dest.parent().ok_or_else(|| Error::Invalid(format!("{} has no parent", dest.display())))?;
    let lock = DirLock::acquire(dir)?;
    lock.check_planned(dest, name, expected_index.as_ref())?;
    recover_dir(&lock, &action.harness, DirectoryKind::Skills)?;
    let ownership = Ownership::new(&lock, dest, name, expected_index.as_ref());
    ownership.check()?;
    if observe_skill(dest)?.state.as_deref() != expected {
        return Err(changed_since_plan(dest));
    }
    let existing = classify(repo.root(), dest, source, expected_index.as_ref())?;
    let managed = match &existing {
        Existing::Unmanaged(_) if adopted => true,
        Existing::Missing => false,
        Existing::ManagedLink(_) | Existing::ManagedCopy(_) => true,
        Existing::Unmanaged(why) => {
            return Err(Error::Invalid(format!("refusing to touch {}: {why}", dest.display())));
        }
    };
    if *metadata_only {
        // Only the placement's Item ID record changes: what sk1ll placed must still be there.
        if !managed || expected_index.is_none() {
            return Err(changed_since_plan(dest));
        }
        return displaced::record_placement(repo, displaced::Kind::Skill, dest, source, item_id.as_ref());
    }
    let record = |placed: bool| {
        let source = if placed { source } else { None };
        displaced::record_placement(repo, displaced::Kind::Skill, dest, source, item_id.as_ref())
    };
    let prepare = |placed: bool| {
        let source = if placed { source } else { None };
        displaced::prepare_placement(repo, displaced::Kind::Skill, dest, source, item_id.as_ref())
    };
    let settle = |result| settle_placed(&lock, displaced::Kind::Skill, name, dest, expected_index.as_ref(), result);
    let displace = Displace {
        repo,
        kind: displaced::Kind::Skill,
        harness: &action.harness,
        expected,
        ownership: Some(&ownership),
        baseline: None,
    };
    let outcome = match action.kind {
        ActionKind::Conflict => unreachable!(),
        // Nothing there: only the record goes.
        ActionKind::Remove if !managed => return record_placed(ownership.record(None), || record(false)),
        ActionKind::Remove => {
            prepare(false).and_then(|()| swap_in(None, dest, displace, Some(Intent::Remove { recovery: None })))
        }
        ActionKind::Create | ActionKind::Update => {
            let src = source.ok_or_else(|| Error::Invalid(format!("no source for {name}")))?;
            // Validate the entire source before replacing any harness content.
            let canonical = lexical_normalize(&src.canonicalize().at(src)?);
            if mode == LinkMode::Symlink && !canonical.starts_with(repo.root()) {
                return Err(Error::Invalid(format!("{} resolves outside the repo", src.display())));
            }
            refuse_source_marker(src)?;
            // An exact link nobody owns yet only needs its ownership
            // recorded. An owned one is only updated when it was edited
            // (forced): that goes through the swap, which keeps a backup.
            if mode == LinkMode::Symlink
                && expected_index.is_none()
                && matches!(&existing, Existing::ManagedLink(to) if *to == canonical)
            {
                #[cfg(test)]
                hook(Hook::BeforeSwap, dest);
                if observe_skill(dest)?.state.as_deref() != expected
                    || !matches!(classify(repo.root(), dest, source, expected_index.as_ref())?,
                        Existing::ManagedLink(to) if to == canonical)
                {
                    return Err(changed_since_plan(dest));
                }
                let hash = link_identity(&fs::read_link(dest).at(dest)?);
                let payload = hash_payload(dest)?;
                let recorded = ownership.record(Some(IndexEntry::Link { hash, target: canonical, payload }));
                return record_placed(recorded, || record(true));
            }
            // Build the replacement in the skills dir's own scratch directory
            // (same filesystem, never discovered as a skill), then swap it in.
            let scratch = Scratch::open(dir)?;
            let stage = scratch.path().join(format!("stage-{name}"));
            let built = (|| match mode {
                LinkMode::Symlink => {
                    symlink_dir(&canonical, &stage).at(&stage)?;
                    let hash = link_identity(&fs::read_link(&stage).at(&stage)?);
                    Ok(IndexEntry::Link { hash, target: canonical, payload: hash_payload(&stage)? })
                }
                LinkMode::Copy => {
                    #[cfg(test)]
                    hook(Hook::BeforeCopy, src);
                    refuse_source_marker(src)?;
                    copy_dir(src, &stage, source_exclusion(src))?;
                    refuse_source_marker(src)?;
                    let token = new_token()?;
                    let marker = stage.join(PLACEMENT_MARKER);
                    crate::config_file::replace_private(&marker, token.as_bytes())?;
                    let digest = copy_hash(&stage)?;
                    if reviewed_digest.is_some_and(|expected| expected != digest) {
                        return Err(Error::Changed(format!("{} changed since the sync preview", src.display())));
                    }
                    unmanaged::durable_tree(&stage)?;
                    let next = IndexEntry::Copy { token, hash: digest, payload: hash_payload(&stage)? };
                    create_anchor(dir, &next, &marker)?;
                    Ok(next)
                }
            })();
            let swapped = built
                .and_then(|next| {
                    crate::config_file::sync_dir(&stage)?;
                    prepare(true)?;
                    Ok(next)
                })
                .and_then(|next| swap_in(Some(&stage), dest, displace, Some(Intent::Place { next, recovery: None })));
            let outcome = settle(ownership.finish(swapped));
            if outcome.is_err() && fs::symlink_metadata(&stage).is_ok() {
                let _ = remove_entry(&stage);
            }
            retire_anchors(&lock)?;
            return outcome;
        }
    };
    let result = settle(ownership.finish(outcome));
    retire_anchors(&lock)?;
    result
}

/// What [`rule_state`] says of anything at a rule file's place that isn't
/// a plain file (a symlink, even to identical text, is someone else's).
const NOT_A_FILE: &str = "not a plain file";

/// One look at a rule file's place, never following a symlink there.
enum RuleSeen {
    /// A plain file: the hash of its text, and which file it is.
    File { hash: String, file: FileId, payload: String },
    /// Anything else.
    Other(String),
}

impl RuleSeen {
    /// The text's hash and the file's identity together, so that another
    /// file with the same text never passes for the one observed.
    fn state(&self) -> String {
        match self {
            RuleSeen::File { payload, file, .. } => format!("{payload}@{file}"),
            RuleSeen::Other(what) => format!("{NOT_A_FILE}: {what}"),
        }
    }
}

/// `None`: absent. Opened without blocking, so a FIFO there can't stall the engine.
fn rule_identity(path: &Path) -> Result<Option<FileId>> {
    use crate::config_file::{Entry, open_entry};
    match open_entry(path)? {
        Entry::File(file, meta) => Ok(Some(FileId::read(&file, &meta, path)?)),
        _ => Ok(None),
    }
}

fn rule_seen(path: &Path) -> Result<Option<RuleSeen>> {
    use crate::config_file::{Entry, open_entry};
    use std::io::Read;
    let (mut file, meta) = match open_entry(path)? {
        Entry::Missing => return Ok(None),
        Entry::Other(what) => return Ok(Some(RuleSeen::Other(what))),
        Entry::File(file, meta) => (file, meta),
    };
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).at(path)?;
    let identity = FileId::read(&file, &meta, path)?;
    let payload = hash_payload(path)?;
    Ok(Some(RuleSeen::File { hash: crate::hash::hash_bytes(&bytes), file: identity, payload }))
}

/// A rule file's [`RuleSeen::state`]; `None`: absent. Whatever isn't a
/// plain file never matches a state sk1ll recorded, so it is never
/// replaced or removed.
fn rule_state(path: &Path) -> Result<Option<String>> {
    Ok(rule_seen(path)?.as_ref().map(RuleSeen::state))
}

/// One look at a skill destination.
struct Observed {
    /// The entry's identity (link target, or a directory's content hash
    /// and placement token), checked alongside its ownership record.
    /// Recovery compares just this fingerprint.
    state: Option<String>,
    payload: Option<String>,
}

impl Observed {
    fn edited(&self, entry: &IndexEntry) -> bool {
        self.payload.as_deref() != Some(entry.payload())
    }
}

/// Snapshot the entry itself, never following a skill symlink into the repo.
fn observe_skill(dest: &Path) -> Result<Observed> {
    observe_skill_with(dest, None)
}

/// [`observe_skill`], with the payload hash taken from `cache` while the
/// destination is unchanged: only for planning.
fn observe_skill_with(dest: &Path, cache: Option<&HashCache>) -> Result<Observed> {
    let hash_payload = |path: &Path| match cache {
        Some(cache) => cache.payload_hash(path),
        None => hash_payload(path),
    };
    let meta = match fs::symlink_metadata(dest) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Observed { state: None, payload: None }),
        Err(e) => return Err(e).at(dest),
    };
    let (state, payload) = if meta.file_type().is_symlink() {
        let payload = hash_payload(dest)?;
        (format!("{}:{payload}", link_identity(&fs::read_link(dest).at(dest)?)), Some(payload))
    } else if meta.is_dir() {
        let marker = rule_identity(&dest.join(PLACEMENT_MARKER))?;
        let payload = hash_payload(dest)?;
        (format!("copy:{payload}:{marker:?}"), Some(payload))
    } else {
        ("unmanaged".to_string(), None)
    };
    Ok(Observed { state: Some(state), payload })
}

/// Read the reserved root marker without following a link.
fn placement_token(dir: &Path) -> Result<Option<String>> {
    use crate::config_file::{Entry, open_entry};
    use std::io::Read;
    let path = dir.join(PLACEMENT_MARKER);
    let Entry::File(file, _) = open_entry(&path)? else {
        return Ok(None);
    };
    let mut bytes = Vec::new();
    file.take(33).read_to_end(&mut bytes).at(&path)?;
    Ok((bytes.len() == 32 && bytes.iter().all(u8::is_ascii_hexdigit))
        .then(|| String::from_utf8(bytes).expect("ASCII hex")))
}

fn new_token() -> Result<String> {
    let mut bytes = [0; 16];
    getrandom::fill(&mut bytes).map_err(|e| Error::Invalid(format!("could not generate a placement token: {e}")))?;
    Ok(crate::hash::hex(&bytes))
}

/// Scratch directory sk1ll keeps in the directories it writes to: in a
/// skills dir, replacements are staged in it (on the destination's
/// filesystem, so they can be renamed in), and everywhere it holds the
/// recovery entries of what sync displaced from that directory (see
/// [`displaced`]). Planning ignores it (as every `.sk1ll-*` entry), and with
/// no `SKILL.md` directly inside, no harness takes it for a skill.
const TMP_DIR: &str = ".sk1ll-tmp";

/// Names older versions staged and set aside skills under, next to the
/// skills themselves; recovered like the contents of [`TMP_DIR`].
const LEGACY_STAGE_PREFIX: &str = ".sk1ll-stage-";
const LEGACY_OLD_PREFIX: &str = ".sk1ll-old-";

fn changed_since_plan(dest: &Path) -> Error {
    Error::Invalid(format!("{} changed since the plan was made; re-run sync", dest.display()))
}

/// `.<file name>.<suffix>`, next to `path`.
fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    path.with_file_name(format!(".{name}.{suffix}"))
}

/// Swaps the staged `stage` in at `dest`. The destination is checked
/// right before the swap, and again once the old entry has been moved into
/// the recovery area, putting it back if an edit slipped in between.
///
/// The old entry is never deleted: a writer with a file already open can
/// still change it after any check, so it stays in the recovery area (see
/// [`displaced`]), checked once more after the swap. If that last check finds
/// it changed, the swap still stands, and the result is the
/// [`Error::Displaced`] that reports it. The replacement only ever goes in
/// where nothing is (see [`place`]): whatever appears at `dest` meanwhile
/// stays, and the swap is called off. On error, `stage` is left for the
/// caller to remove.
fn swap_in(stage: Option<&Path>, dest: &Path, displace: Displace, intent: Option<Intent>) -> Result<Option<Error>> {
    let holder = dest.parent().unwrap_or(dest);
    let mut held = if displace.expected.is_some() {
        let placed = displace.ownership.map(Ownership::placed_payload).transpose()?.flatten();
        let payload = placed.or(displace.baseline);
        Some(displaced::Displacing::begin(
            Some(displace.repo),
            displace.kind,
            displace.harness,
            dest,
            displace.expected,
            payload,
            holder,
        )?)
    } else {
        None
    };
    let setup = || -> Result<()> {
        if let (Some(ownership), Some(intent)) = (displace.ownership, intent) {
            ownership.begin(intent.with_recovery(held.as_ref().map(|entry| entry.id().to_owned())))?;
        }
        #[cfg(test)]
        hook(Hook::BeforeSwap, dest);
        if stage.is_some() && displaced::fingerprint(displace.kind, dest)?.as_deref() != displace.expected {
            return Err(changed_since_plan(dest));
        }
        #[cfg(test)]
        hook(Hook::AfterCheck, dest);
        displace.check_ownership()
    };
    if let Err(e) = setup() {
        if let Some(held) = held {
            held.abandon();
        }
        return Err(e);
    }
    let backup = held.as_ref().map(displaced::Displacing::content);
    #[cfg(test)]
    if let Some(error) = backup.as_deref().and_then(displaced::forced_failure) {
        if let Some(held) = held {
            held.abandon();
        }
        return Err(error).at(dest);
    }
    let swapped = crate::placement::swap_managed(stage, dest, backup.as_deref(), || {
        if let Some(held) = &mut held {
            #[cfg(test)]
            hook(Hook::AfterRename, dest);
            held.mark_moved(dest)?;
            let intact = held.intact();
            #[cfg(test)]
            hook(Hook::AfterAside, dest);
            if !intact? {
                return Err(changed_since_plan(dest));
            }
        }
        displace.check_ownership()
    });
    let outcome = match swapped {
        Ok(result) => result,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Err(changed_since_plan(dest)),
        Err(e) => Err(e).at(dest),
    };
    if let Err(e) = outcome {
        if let Some(mut held) = held {
            if held.content().symlink_metadata().is_ok() {
                held.mark_moved(dest)?;
                held.put_back()?;
            } else {
                held.abandon();
            }
        }
        return Err(e);
    }
    #[cfg(test)]
    hook(Hook::AfterSwap, dest);
    held.map_or(Ok(None), |held| held.finish(displace.ownership.is_some()))
}

fn remove_displacing(dest: &Path, displace: Displace) -> Result<Option<Error>> {
    swap_in(None, dest, displace, None)
}

/// How [`replace_file`] records the content it replaces.
struct Displace<'a> {
    /// The repo whose content is placed: recovery entries record where it came from.
    repo: &'a Repo,
    kind: displaced::Kind,
    harness: &'a str,
    /// The fingerprint the replaced content should have (`None`: there
    /// should be none, so any content found is someone else's).
    expected: Option<&'a str>,
    /// The index entry of a skill or rule file, rechecked right before each
    /// change. For [`replace_file`], also where it records its intent.
    ownership: Option<&'a Ownership<'a>>,
    /// Without `ownership`: the payload fingerprint the replaced content
    /// had, so its backup is pruned once it is unchanged and old enough
    /// (`None`: it is kept until discarded).
    baseline: Option<&'a str>,
}

impl Displace<'_> {
    fn check_ownership(&self) -> Result<()> {
        self.ownership.map_or(Ok(()), Ownership::check)
    }
}

/// Like [`instructions::write_file`] (atomic, through symlinks), but the new
/// text is staged first and `unchanged()` is checked right before the rename,
/// so an edit made while staging fails the write instead of being lost.
///
/// The rename still can't stop a writer that edits the file after that check
/// (or through a descriptor it already holds), so the old file (the symlink's
/// target, for a linked file) is first hard-linked into the recovery area
/// next to it, checked there against `expected` (calling the write off if it
/// changed), and checked once more after the rename; see [`swap_in`]. With
/// nothing there to replace (a create), the new file only goes in while
/// nothing is there (see [`place`]); a file that appeared meanwhile stays.
fn replace_file(
    path: &Path,
    text: &str,
    unchanged: impl FnOnce() -> Result<()>,
    displace: Displace,
    old: OldCopy,
) -> Result<Option<Error>> {
    replace_file_published(path, text, unchanged, displace, old).map(|(displaced, _)| displaced)
}

/// The identity and payload fingerprint of a file sync published.
type Published = (FileId, String);

/// [`replace_file`], also returning the identity and payload fingerprint
/// of the file it published, read from the staged file before the rename.
fn replace_file_published(
    path: &Path,
    text: &str,
    unchanged: impl FnOnce() -> Result<()>,
    displace: Displace,
    old: OldCopy,
) -> Result<(Option<Error>, Option<Published>)> {
    let real = match fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() => fs::canonicalize(path).at(path)?,
        _ => path.to_path_buf(),
    };
    let holder = real.parent().ok_or_else(|| Error::Invalid(format!("{} has no parent", real.display())))?;
    fs::create_dir_all(holder).at(holder)?;
    let _lock = DirLock::acquire(holder)?;
    let scratch = Scratch::open(holder)?;
    let leaf = real.file_name().expect("instructions file has a name").to_string_lossy();
    let tmp = scratch.path().join(format!("stage-block-{leaf}"));
    crate::config_file::write_new(&tmp, text.as_bytes())?;
    let fail = |e: Error| {
        let _ = fs::remove_file(&tmp);
        Err(e)
    };
    let published = match (rule_identity(&tmp), hash_payload(&tmp)) {
        (Ok(Some(file)), Ok(payload)) => Some((file, payload)),
        (Err(e), _) | (_, Err(e)) => return fail(e),
        _ => None,
    };
    #[cfg(test)]
    hook(Hook::BeforeSwap, path);
    if let Err(e) = unchanged() {
        return fail(e);
    }
    #[cfg(test)]
    hook(Hook::AfterCheck, path);
    let Displace { repo, kind, harness, expected, .. } = displace;
    let mut held = match displaced::Displacing::begin(Some(repo), kind, harness, path, expected, None, holder) {
        Ok(held) => held,
        Err(e) => return fail(e),
    };
    let verdict = match held.link(&real) {
        Ok(true) => held.intact().map(Some),
        Ok(false) if kind == displaced::Kind::Rule && expected.is_some() => Ok(Some(false)),
        Ok(false) => Ok(None),
        Err(e) => Err(e),
    };
    let held = match verdict {
        Ok(Some(true)) => Some(held),
        // Nothing there (a create): nothing to displace.
        Ok(None) => {
            held.abandon();
            None
        }
        // Edited (or, for a create, made) since the check: left as it is.
        verdict => {
            held.abandon();
            return fail(verdict.err().unwrap_or_else(|| changed_since_plan(path)));
        }
    };
    #[cfg(test)]
    hook(Hook::AfterAside, path);
    let swapped = displace.check_ownership().and_then(|()| match held {
        // The old file is held in recovery under a second name: replacing it loses nothing.
        Some(_) => fs::rename(&tmp, &real).at(&real),
        None => place_at(&tmp, &real),
    });
    if let Err(e) = swapped {
        if let Some(held) = held {
            held.abandon();
        }
        return fail(e);
    }
    #[cfg(test)]
    hook(Hook::AfterSwap, path);
    let Some(held) = held else { return Ok((None, published)) };
    let displaced = match old {
        OldCopy::Keep => held.finish(false),
        OldCopy::DropIfIntact => held.finish_owned(),
        OldCopy::KeepMarked => held.finish_kept().map(|()| None),
    }?;
    Ok((displaced, published))
}

/// [`place`], with something at `to` reported as a change since the plan.
fn place_at(from: &Path, to: &Path) -> Result<()> {
    match place(from, to) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Err(changed_since_plan(to)),
        Err(e) => Err(e).at(to),
    }
}

/// Moves the file, symlink or directory `from` to `to`, atomically, unless
/// something is at `to`: then it fails with `AlreadyExists` and leaves both
/// alone. Sync puts content in place with this wherever nothing should be,
/// so that something that appears there at the last moment is never replaced.
///
/// - A file is hard-linked at `to` (`link(2)` never replaces anything), then
///   its old name is removed: still the same inode, so a writer holding it
///   open loses nothing.
/// - A symlink is made anew at `to` (`symlink(2)` never replaces either).
/// - A directory is renamed with `renameat2(RENAME_NOREPLACE)` on Linux, or
///   `renamex_np(RENAME_EXCL)` on macOS. Elsewhere, or on a filesystem that
///   refuses the flag, it is checked to be free and renamed: a directory can
///   only ever be renamed over an empty directory (rename fails on anything
///   else), so that fallback can't lose content either.
fn place(from: &Path, to: &Path) -> std::io::Result<()> {
    let meta = fs::symlink_metadata(from)?;
    if meta.file_type().is_symlink() {
        let target = fs::read_link(from)?;
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, to)?;
        #[cfg(windows)]
        if fs::metadata(from).map(|m| m.is_dir()).unwrap_or(true) {
            symlink_dir(&target, to)?;
        } else {
            std::os::windows::fs::symlink_file(&target, to)?;
        }
        let _ = fs::remove_file(from).or_else(|_| fs::remove_dir(from));
        Ok(())
    } else if meta.is_dir() {
        rename_noreplace(from, to)
    } else {
        fs::hard_link(from, to)?;
        let _ = fs::remove_file(from);
        Ok(())
    }
}

/// `rename`, failing with `AlreadyExists` instead of replacing; see [`place`].
pub(crate) fn rename_noreplace(from: &Path, to: &Path) -> std::io::Result<()> {
    #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
    {
        use std::os::unix::ffi::OsStrExt;
        let from_c = std::ffi::CString::new(from.as_os_str().as_bytes())?;
        let to_c = std::ffi::CString::new(to.as_os_str().as_bytes())?;
        // SAFETY: both arguments are valid NUL-terminated paths that outlive the call.
        #[cfg(any(target_os = "linux", target_os = "android"))]
        let r = unsafe {
            libc::syscall(
                libc::SYS_renameat2,
                libc::AT_FDCWD as libc::c_long,
                from_c.as_ptr(),
                libc::AT_FDCWD as libc::c_long,
                to_c.as_ptr(),
                libc::RENAME_NOREPLACE as libc::c_long,
            )
        };
        // SAFETY: as above.
        #[cfg(target_os = "macos")]
        let r = unsafe { libc::renamex_np(from_c.as_ptr(), to_c.as_ptr(), libc::RENAME_EXCL) };
        if r == 0 {
            return Ok(());
        }
        let e = std::io::Error::last_os_error();
        // No such call, or a filesystem without the flag: the fallback below.
        if !matches!(e.raw_os_error(), Some(libc::EINVAL | libc::ENOSYS | libc::ENOTSUP)) {
            return Err(e);
        }
    }
    // Files and symlinks must keep their no-replace guarantee even when the
    // filesystem rejects the rename flag. place uses link/symlink creation.
    if !fs::symlink_metadata(from)?.is_dir() {
        return place(from, to);
    }
    if fs::symlink_metadata(to).is_ok() {
        return Err(std::io::ErrorKind::AlreadyExists.into());
    }
    fs::rename(from, to)
}

/// Cleans up after a sync that was killed between renames, in every
/// harness: the changes it began in a managed directory are settled in its
/// index, replacements that were being staged are deleted, and old content
/// that was set aside (by older versions, next to the destination) is moved
/// into the recovery area. Recovery entries in the harnesses' own
/// directories that the state directory's index lost are listed again.
/// Returns the entries that rescued; those that differ from what sk1ll last
/// wrote are also in [`displaced::changed`]. Planning never needs this (it
/// ignores such leftovers and reads pending changes as recovery will settle
/// them); [`apply`] does it for the directory it works in.
///
/// A managed directory's lock protects its stages from another sync's
/// recovery. A block's file siblings are recovered only with a repo lock.
/// Read-only paths (`sync --dry-run`, status, conflicts) must never call it.
pub fn recover(
    lock: Option<&crate::EngineLock>,
    harnesses: &BTreeMap<String, Harness>,
) -> Result<Vec<displaced::Entry>> {
    let mut rescued = Vec::new();
    for (hname, harness) in harnesses {
        if let Some(dir) = harness.skills_dir() {
            rescued.extend(recover_managed(hname, &dir, DirectoryKind::Skills)?);
        }
        if let Some(path) = harness.instructions_file() {
            match harness.instructions_format {
                InstructionsFormat::Block if lock.is_some() => {
                    rescued.extend(recover_file_siblings(hname, &path)?);
                    if let Some(holder) = real_file(&path).as_deref().and_then(Path::parent) {
                        displaced::reindex(holder)?;
                    }
                }
                InstructionsFormat::Block => {}
                InstructionsFormat::Rules => rescued.extend(recover_managed(hname, &path, DirectoryKind::Rules)?),
            }
        }
    }
    Ok(rescued)
}

/// The file a block instructions path stands for: its target if it is a
/// symlink (`None` if that dangles), else itself.
fn real_file(path: &Path) -> Option<PathBuf> {
    match fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() => fs::canonicalize(path).ok(),
        _ => Some(path.to_path_buf()),
    }
}

fn read_dir_names(dir: &Path) -> Result<Vec<String>> {
    use std::io::ErrorKind;
    match fs::read_dir(dir) {
        Ok(entries) => {
            let mut names = Vec::new();
            for entry in entries {
                names.push(entry.at(dir)?.file_name().to_string_lossy().into_owned());
            }
            Ok(names)
        }
        Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => Ok(Vec::new()),
        Err(e) => Err(e).at(dir),
    }
}

#[derive(Clone, Copy)]
enum DirectoryKind {
    Skills,
    Rules,
}

/// [`recover_dir`] under the directory's lock, for a directory sync has
/// worked in: one without its state directory has nothing of sk1ll's to
/// recover, and gets none.
fn recover_managed(harness: &str, dir: &Path, directory: DirectoryKind) -> Result<Vec<displaced::Entry>> {
    if !dir.join(INDEX_DIR).is_dir() {
        return Ok(Vec::new());
    }
    let lock = DirLock::acquire(dir)?;
    let rescued = recover_dir(&lock, harness, directory)?;
    displaced::reindex(dir)?;
    Ok(rescued)
}

/// Settle the changes an interrupted sync began, discard staged
/// replacements and preserve any content left aside. The index records the
/// fingerprint of the last placement at its original name.
fn recover_dir(lock: &DirLock, harness: &str, directory: DirectoryKind) -> Result<Vec<displaced::Entry>> {
    settle_pending(lock, directory)?;
    retire_anchors(lock)?;
    prune_indexes(lock, displaced::GRACE)?;
    let dir = lock.dir;
    let scratch = Scratch::open(dir)?;
    for name in read_dir_names(scratch.path())? {
        if name.starts_with("stage-") || name.starts_with(".index.json.sk1ll-tmp") {
            remove_entry(&scratch.path().join(name))?;
        }
    }
    let tmp = dir.join(TMP_DIR);
    let mut leftovers = Vec::new();
    for name in read_dir_names(dir)? {
        if let Some(entry) =
            name.strip_prefix(LEGACY_OLD_PREFIX).or_else(|| name.strip_prefix(LEGACY_STAGE_PREFIX)).or_else(|| {
                name.strip_prefix('.').and_then(|name| {
                    if matches!(directory, DirectoryKind::Rules) {
                        name.strip_suffix(".sk1ll-old").or_else(|| name.strip_suffix(".sk1ll-tmp"))
                    } else {
                        None
                    }
                })
            })
            && name != TMP_DIR
        {
            leftovers.push((dir.join(&name), entry.to_string()));
        }
    }
    let legacy = match tmp.symlink_metadata() {
        Ok(meta) if meta.is_dir() => {
            let real = dir.canonicalize().at(dir)?.join(TMP_DIR);
            Some(Scratch::pin(&real)?)
        }
        Ok(_) => return Err(Error::Invalid(format!("{} is not a plain recovery directory", tmp.display()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e).at(&tmp),
    };
    if let (DirectoryKind::Skills, Some(legacy)) = (directory, &legacy) {
        for name in read_dir_names(legacy.path())? {
            if let Some(entry) = name.strip_prefix("old-").or_else(|| name.strip_prefix("stage-")) {
                leftovers.push((legacy.path().join(&name), entry.to_string()));
            }
        }
    }
    let kind = match directory {
        DirectoryKind::Skills => displaced::Kind::Skill,
        DirectoryKind::Rules => displaced::Kind::Rule,
    };
    leftovers
        .into_iter()
        .map(|(aside, name)| displaced::rescue(kind, harness, &dir.join(name), &aside, None, dir))
        .collect()
}

/// Records the outcome of each interrupted sync before cleaning its scratch.
fn settle_pending(lock: &DirLock, directory: DirectoryKind) -> Result<()> {
    let before = load_index(lock.dir)?;
    if before.pending.is_empty() {
        return Ok(());
    }
    displaced::reindex(lock.dir)?;
    let after = before.clone().settle(lock.dir)?;
    for (name, intent) in &before.pending {
        if let Some(id) = intent.recovery() {
            let dest = lock.dir.join(name);
            displaced::settle(id, &dest, intent.keeps_backup(after.entries.contains_key(name), &dest))?;
        }
    }
    let dest = lock.dir.join(INDEX_PATH);
    // The live destination and recovery directory must be durable first.
    crate::config_file::sync_dir(&lock.dir.join("index-publication"))?;
    lock.save(&before, &after, &dest)?;
    // Each change's placement record follows its settled outcome.
    let kind = match directory {
        DirectoryKind::Skills => displaced::Kind::Skill,
        DirectoryKind::Rules => displaced::Kind::Rule,
    };
    for name in before.pending.keys() {
        let made = after.entries.get(name) != before.entries.get(name);
        displaced::settle_placement(kind, &lock.dir.join(name), made)?;
    }
    Ok(())
}

/// Leftovers next to a block instructions file (its target, if linked).
fn recover_file_siblings(harness: &str, path: &Path) -> Result<Vec<displaced::Entry>> {
    let Some(real) = real_file(path) else {
        return Ok(Vec::new());
    };
    let holder = real.parent().unwrap_or(&real);
    let mut rescued = Vec::new();
    let tmp = sibling(&real, "sk1ll-tmp");
    if fs::symlink_metadata(&tmp).is_ok() {
        rescued.push(displaced::rescue(displaced::Kind::Instructions, harness, path, &tmp, None, holder)?);
    }
    let old = sibling(&real, "sk1ll-old");
    if fs::symlink_metadata(&old).is_err() {
        return Ok(rescued);
    }
    if displaced::same_file(&old, &real) {
        // Killed before the replacement went in: it is the live file.
        fs::remove_file(&old).at(&old)?;
        return Ok(rescued);
    }
    rescued.push(displaced::rescue(displaced::Kind::Instructions, harness, path, &old, None, holder)?);
    Ok(rescued)
}

/// Resolves a conflict the other way round: copies the harness's content back
/// into the repo, where it becomes a draft change. Works for copied skill
/// directories (into the skill's repo directory) and
/// `rules` files (into `instructions/<name>.md`, keeping the rule's
/// `harnesses:` key). The harness copy is then recorded as managed, so the
/// next plan has no conflict for it.
pub fn adopt(repo: &Repo, action: &Action) -> Result<()> {
    if action.kind != ActionKind::Conflict {
        return Err(Error::Invalid("only conflicts can be adopted".into()));
    }
    match &action.item {
        Item::Skill { name, .. } => adopt_skill(repo, &action.harness, &action.dest, name),
        Item::Rule { file, .. } => adopt_rule(repo, &action.harness, &action.dest, file),
        Item::Instructions { .. } => Err(Error::Invalid(
            "adopt isn't supported for block instructions; one block mixes several rules, so move the edits \
             into instructions/ by hand, or overwrite them with `sk1ll sync --force`"
                .into(),
        )),
    }
}

fn adopt_skill(repo: &Repo, harness: &str, src: &Path, name: &str) -> Result<()> {
    let dir = src.parent().ok_or_else(|| Error::Invalid(format!("{} has no parent", src.display())))?;
    let lock = DirLock::acquire(dir)?;
    recover_dir(&lock, harness, DirectoryKind::Skills)?;
    let recorded = index_entry(src)?;
    let token = match (&recorded, classify(repo.root(), src, None, recorded.as_ref())?) {
        (Some(IndexEntry::Copy { token, .. }), Existing::ManagedCopy(_)) => token.clone(),
        _ => {
            return Err(Error::Invalid(format!(
                "`{name}` is an unmanaged skill and cannot replace an existing repo skill"
            )));
        }
    };
    let ownership = Ownership::new(&lock, src, name, recorded.as_ref());
    let target = placed_target(repo, displaced::Kind::Skill, src, name)?;
    let placed_origin = displaced::skill_placement_origin(src)?.or(target.origin);
    let (hash, payload, displaced) = copy_into_repo(repo, src, &target, None, placed_origin)?;
    // Only what was copied is owned: content the copy left out (`.git`),
    // or an edit made since, keeps the copy from matching, so sync never
    // removes it without a backup it keeps.
    ownership.record(Some(IndexEntry::Copy { token, hash, payload }))?;
    displaced.map_or(Ok(()), Err)
}

/// The path and copy policy used for both Recovery preview and Adopt.
pub struct AdoptTarget {
    pub path: PathBuf,
    pub origin: Option<Origin>,
    pub placed_origin: Option<Origin>,
    existing: bool,
    skill_id: Option<ItemId>,
    rule: Option<Rule>,
}

fn adopt_target(repo: &Repo, kind: displaced::Kind, dest: &Path, item_id: Option<&ItemId>) -> Result<AdoptTarget> {
    let leaf = dest.file_name().unwrap_or_default().to_string_lossy();
    let mut target = AdoptTarget {
        path: dest.to_path_buf(),
        origin: None,
        placed_origin: None,
        existing: false,
        skill_id: None,
        rule: None,
    };
    if matches!(kind, displaced::Kind::Instructions | displaced::Kind::Draft) {
        return Ok(target);
    }
    let (skills, rules) = repo.content()?;
    let name = if kind == displaced::Kind::Rule { leaf.strip_suffix(".md").unwrap_or(&leaf) } else { &leaf };
    let named = |candidate: &str, id: Option<&ItemId>| match item_id {
        Some(item_id) => id == Some(item_id),
        None => candidate == name,
    };
    if kind == displaced::Kind::Skill {
        if let Some(skill) = skills.iter().find(|s| named(&s.name, s.id.as_ref())) {
            target.path = skill.path.clone();
            target.origin = Some(skill.origin);
            target.skill_id = skill.id.clone();
            target.existing = true;
        } else {
            if rules.iter().any(|rule| item_id.is_some() && rule.id.as_ref() == item_id) {
                return Err(recovery_target_changed(item_id));
            }
            target.path = repo.personal_dir().join(name);
            target.origin = Some(Origin::Personal);
            if repo.imported_dir().join(name).symlink_metadata().is_ok()
                || crate::Manifest::load(repo.root())?.import.contains_key(name)
            {
                return Err(recovery_target_changed(item_id));
            }
        }
    } else if let Some(rule) = rules.into_iter().find(|r| named(&r.name, r.id.as_ref())) {
        target.path = rule.path.clone();
        target.existing = true;
        target.rule = Some(rule);
    } else {
        if skills.iter().any(|skill| item_id.is_some() && skill.id.as_ref() == item_id) {
            return Err(recovery_target_changed(item_id));
        }
        target.path = repo.root().join(instructions::INSTRUCTIONS_DIR).join(format!("{name}.md"));
    }
    if !target.existing && target.path.symlink_metadata().is_ok() {
        return Err(recovery_target_changed(item_id));
    }
    let name = if kind == displaced::Kind::Rule {
        target.path.file_stem().unwrap_or_default()
    } else {
        target.path.file_name().unwrap_or_default()
    };
    crate::imports::validate_name(&name.to_string_lossy())?;
    Ok(target)
}

/// Where Adopt copies the harness content at `src` (a skill or rule file,
/// `leaf` named there): the repo item sync placed it from, found by the
/// Item ID it recorded for the placement, whatever the item is called now
/// (it may have been renamed). Refused, naming what it found, when that ID
/// leads to no item now (deleted, or given a new ID) or to more than one.
/// Only a placement recorded without an Item ID is found by its name.
fn placed_target(repo: &Repo, kind: displaced::Kind, src: &Path, leaf: &str) -> Result<AdoptTarget> {
    let what = if kind == displaced::Kind::Rule { "rule" } else { "skill" };
    let name = if kind == displaced::Kind::Rule { leaf.strip_suffix(".md").unwrap_or(leaf) } else { leaf };
    let Some(Some(id)) = displaced::placed_id(kind, src)? else {
        return adopt_target(repo, kind, Path::new(leaf), None);
    };
    let (skills, rules) = match repo.content() {
        Err(Error::DuplicateItemId { id: shared, first, second }) => {
            return Err(Error::Invalid(format!(
                "can't tell where to adopt `{name}`: Item ID `{shared}` is claimed by both {} and {}. Run `sk1ll \
                 repair-ids` first",
                first.display(),
                second.display()
            )));
        }
        content => content?,
    };
    let holders: Vec<(String, PathBuf)> = match kind {
        displaced::Kind::Rule => {
            rules.iter().filter(|r| r.id.as_ref() == Some(&id)).map(|r| (r.name.clone(), r.path.clone())).collect()
        }
        _ => skills.iter().filter(|s| s.id.as_ref() == Some(&id)).map(|s| (s.name.clone(), s.path.clone())).collect(),
    };
    match holders.as_slice() {
        [_] => adopt_target(repo, kind, Path::new(leaf), Some(&id)),
        // An Adopt of it that stopped half-way: the repo's version of the
        // item, with that ID, is in recovery, and its path is free again.
        [] if let Some(dest) = interrupted_adopt(repo, kind, &id)? => {
            let leaf = dest.file_name().unwrap_or_default().to_string_lossy().into_owned();
            adopt_target(repo, kind, Path::new(&leaf), None)
        }
        [] => {
            let same_name = match kind {
                displaced::Kind::Rule => rules.iter().any(|r| r.name == name),
                _ => skills.iter().any(|s| s.name == name),
            };
            let hint = match same_name {
                true => format!(
                    " The repo's {what} `{name}` has another Item ID, so it is another item; adopting would \
                     overwrite it."
                ),
                false => String::new(),
            };
            Err(Error::Invalid(format!(
                "can't adopt `{name}`: sync placed it as Item ID `{id}`, which no {what} in the repo has now (deleted, \
                 or given a new Item ID).{hint} Restore that {what} (`sk1ll restore`), or move {} out of the harness \
                 and add it again as a new {what}",
                src.display()
            )))
        }
        many => Err(Error::Invalid(format!(
            "can't tell where to adopt `{name}`: Item ID `{id}` belongs to {}. Run `sk1ll repair-ids` first",
            many.iter().map(|(name, path)| format!("`{name}` ({})", path.display())).collect::<Vec<_>>().join(", ")
        ))),
    }
}

/// The path of the repo item with Item ID `id` an Adopt moved into
/// recovery and didn't put a copy in place of: the entry is there, and
/// nothing is at its path.
fn interrupted_adopt(repo: &Repo, kind: displaced::Kind, id: &ItemId) -> Result<Option<PathBuf>> {
    for entry in displaced::list()? {
        if entry.harness != REPO_HOLDER || entry.kind != kind || !entry.dest.starts_with(repo.root()) {
            continue;
        }
        if fs::symlink_metadata(&entry.dest).is_ok() {
            continue;
        }
        let held = match kind {
            displaced::Kind::Rule => {
                let Ok(file) = fs::File::open(&entry.path) else { continue };
                instructions::read_explicit_id(file, &entry.path).ok().flatten()
            }
            _ => ItemId::read_skill(&entry.path).ok().flatten(),
        };
        if held.as_ref() == Some(id) {
            return Ok(Some(entry.dest));
        }
    }
    Ok(None)
}

fn recovery_target_changed(id: Option<&ItemId>) -> Error {
    let id = id.map_or_else(|| "unassigned".into(), ToString::to_string);
    Error::Invalid(format!(
        "Recovery cannot identify a safe destination for Item ID `{id}`. Pick a target for the recovered content"
    ))
}

pub fn copy_skill_for_diff(src: &Path, dest: &Path, origin: Origin) -> Result<()> {
    copy_dir(src, dest, origin_exclusion(origin))
}

fn origin_exclusion(origin: Origin) -> Exclusion {
    if origin == Origin::Imported { excluded_from_skill } else { excluded_from_personal_skill }
}

/// Replaces the repo skill `target` (a new personal one if there is none)
/// with a copy of the skill directory `src`, through the same swap as sync:
/// the repo's version goes into recovery and the copy only goes in where
/// nothing is. A personal skill keeps its Item ID (`recovered_id` if it has
/// none yet, else a new one); an imported one keeps what a copy leaves out.
/// Returns the copy's content hash, the payload fingerprint of what was
/// copied from `src` (see [`crate::hash::read_payload`]), and the
/// [`Error::Displaced`] for a repo version written to during the swap.
fn copy_into_repo(
    repo: &Repo,
    src: &Path,
    target: &AdoptTarget,
    recovered_id: Option<&ItemId>,
    placed_origin: Option<Origin>,
) -> Result<(String, String, Option<Error>)> {
    let meta = fs::symlink_metadata(src).at(src)?;
    if !meta.is_dir() {
        return Err(Error::Invalid(format!(
            "{} isn't a skill directory; only copied skills can be adopted",
            src.display()
        )));
    }
    let dest = &target.path;
    let origin = target.origin.expect("skill origin");
    let id = if origin == Origin::Personal {
        let id = match target.skill_id.as_ref().or(recovered_id) {
            Some(id) => id.clone(),
            None => repo.new_item_id()?,
        };
        repo.check_item_id(dest.strip_prefix(repo.root()).unwrap().into(), id.clone())?;
        Some(id)
    } else {
        None
    };
    // The harness may have received a different kind of skill before a Fork.
    // Never copy the placement's private files or install a vendor ID as a personal sidecar.
    let exclude = if origin == Origin::Personal {
        excluded_from_personal_skill
    } else {
        origin_exclusion(placed_origin.unwrap_or(origin))
    };
    validate_copy(src, exclude)?;
    let parent = dest.parent().ok_or_else(|| Error::Invalid(format!("{} has no parent", dest.display())))?;
    refuse_linked_parent(repo, dest)?;
    fs::create_dir_all(parent).at(parent)?;
    refuse_linked_parent(repo, dest)?;
    crate::source::exclude_stages(repo.root());
    let existing = match fs::symlink_metadata(dest) {
        Ok(meta) if meta.is_dir() => true,
        Ok(_) => return Err(Error::Invalid(format!("{} is not a skill directory", dest.display()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => return Err(e).at(dest),
    };
    if existing != target.existing {
        return Err(changed_since_plan(dest));
    }
    // An import's content the copy leaves out (`.git`) goes into the new
    // version too, so its backup holds nothing else. Otherwise that content
    // is not in the repo version's baseline, so a backup holding some is
    // never pruned.
    let preserve = existing && id.is_none();
    let baseline = match (existing, preserve) {
        (false, _) => None,
        (true, true) => Some(hash_payload(dest)?),
        (true, false) => Some(crate::hash::hash_copied_payload(dest)?),
    };
    let expected = observe_skill(dest)?.state;
    let stage = crate::source::unique(parent);
    let result = (|| {
        let copied = crate::hash::copied_exclusion(origin == Origin::Personal);
        let (payload, ()) = crate::hash::read_payload_without(src, copied, || copy_dir(src, &stage, exclude))?;
        if let Some(id) = &id {
            crate::config_file::write_new(&stage.join(ID_FILE), format!("{id}\n").as_bytes())?;
        } else if preserve {
            copy_excluded(dest, &stage, exclude)?;
        }
        let hash = if origin == Origin::Imported { copy_hash(&stage)? } else { hash_dir(&stage)? };
        unmanaged::durable_tree(&stage)?;
        let displace = Displace {
            repo,
            kind: displaced::Kind::Skill,
            harness: REPO_HOLDER,
            expected: expected.as_deref(),
            ownership: None,
            baseline: baseline.as_deref(),
        };
        let displaced = swap_in(Some(&stage), dest, displace, None)?;
        Ok((hash, payload, displaced))
    })();
    // Only ever this call's own fresh copy.
    if fs::symlink_metadata(&stage).is_ok() {
        let _ = fs::remove_dir_all(&stage);
    }
    result
}

/// Copies into `to` just what `exclude` leaves out of the skill `from`
/// (with everything below it), so that a copy of it keeps that content.
fn copy_excluded(from: &Path, to: &Path, exclude: Exclusion) -> Result<()> {
    for entry in WalkDir::new(from).min_depth(1) {
        let entry = entry.map_err(std::io::Error::from).at(from)?;
        let rel = entry.path().strip_prefix(from).unwrap_or(entry.path());
        if !rel.components().enumerate().any(|(depth, part)| exclude(depth + 1, part.as_os_str())) {
            continue;
        }
        let out = to.join(rel);
        if let Some(parent) = out.parent() {
            fs::create_dir_all(parent).at(parent)?;
        }
        let kind = entry.file_type();
        if kind.is_dir() {
            fs::create_dir_all(&out).at(&out)?;
        } else if kind.is_symlink() {
            #[cfg(unix)]
            std::os::unix::fs::symlink(fs::read_link(entry.path()).at(entry.path())?, &out).at(&out)?;
            #[cfg(not(unix))]
            return Err(Error::Invalid(format!("unsupported skill entry: {}", entry.path().display())));
        } else if kind.is_file() {
            fs::copy(entry.path(), &out).at(&out)?;
        } else {
            return Err(Error::Invalid(format!("unsupported skill entry: {}", entry.path().display())));
        }
    }
    Ok(())
}

/// The harness name recovery records for repo content that adopting replaced.
const REPO_HOLDER: &str = "repo";

/// Refuses a repo destination whose directories below the repo root are
/// reached through a symlink: what it leads to isn't the repo's to replace.
fn refuse_linked_parent(repo: &Repo, dest: &Path) -> Result<()> {
    let rel = dest
        .strip_prefix(repo.root())
        .map_err(|_| Error::Invalid(format!("refusing to adopt into {}: it is outside the repo", dest.display())))?;
    let mut at = repo.root().to_path_buf();
    for part in rel.parent().into_iter().flat_map(Path::components) {
        at.push(part);
        match fs::symlink_metadata(&at) {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => {
                return Err(Error::Invalid(format!(
                    "refusing to adopt into {}: {} is not a plain directory",
                    dest.display(),
                    at.display()
                )));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e).at(&at),
        }
    }
    Ok(())
}

fn adopt_rule(repo: &Repo, harness: &str, src: &Path, file: &str) -> Result<()> {
    let dir = src.parent().ok_or_else(|| Error::Invalid(format!("{} has no parent", src.display())))?;
    let lock = DirLock::acquire(dir)?;
    recover_dir(&lock, harness, DirectoryKind::Rules)?;
    if fs::symlink_metadata(src).is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound) {
        return Err(Error::Invalid(format!("{} no longer exists; nothing to adopt", src.display())));
    }
    // The fingerprint of the very text adopted: an edit after the read
    // keeps the file from matching what sync owns.
    let (payload, read) = crate::hash::read_payload(src, || read_rule(src))?;
    let Some((text, id)) = read else {
        return Err(Error::Invalid(format!("{} no longer exists; nothing to adopt", src.display())));
    };
    let recorded = index_entry(src)?;
    let ownership = Ownership::new(&lock, src, file, recorded.as_ref());
    let target = placed_target(repo, displaced::Kind::Rule, src, file)?;
    let displaced = rule_into_repo(repo, &target, &text, None)?;
    // The file read, whichever it is: from now on sync manages it.
    let next = IndexEntry::Rule { file: id, hash: hash_str(&text), payload };
    create_anchor(dir, &next, src)?;
    if !holds(src, &next)? {
        return Err(changed_since_plan(src));
    }
    ownership.record(Some(next))?;
    retire_anchors(&lock)?;
    displaced.map_or(Ok(()), Err)
}

/// The text of the rule file at `path` and which file it is, from one open
/// of it; `None`: missing. Anything but a plain file is refused.
fn read_rule(path: &Path) -> Result<Option<(String, FileId)>> {
    use crate::config_file::{Entry, open_entry};
    use std::io::Read;
    let (mut file, meta) = match open_entry(path)? {
        Entry::Missing => return Ok(None),
        Entry::File(file, meta) => (file, meta),
        Entry::Other(what) => {
            return Err(Error::Invalid(format!("{} is not a regular file ({what})", path.display())));
        }
    };
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).at(path)?;
    let text =
        String::from_utf8(bytes).map_err(|e| Error::Parse { path: path.to_path_buf(), message: e.to_string() })?;
    Ok(Some((text, FileId::read(&file, &meta, path)?)))
}

/// Writes the rule file `file`'s `text` into the repo's rule, keeping
/// the rule's `harnesses:` key, through the same swap as sync (see
/// [`copy_into_repo`]).
fn rule_into_repo(
    repo: &Repo,
    target: &AdoptTarget,
    text: &str,
    recovered_id: Option<&ItemId>,
) -> Result<Option<Error>> {
    let dest = &target.path;
    let dir = dest.parent().ok_or_else(|| Error::Invalid(format!("{} has no parent", dest.display())))?;
    refuse_linked_parent(repo, dest)?;
    // Observed before the rule is read, so an edit in between fails the swap.
    let expected = rule_state(dest)?;
    if expected.as_deref().is_some_and(|state| state.starts_with(NOT_A_FILE)) {
        // A linked rule's target is not the repo's to replace.
        return Err(Error::Invalid(format!("refusing to adopt into {}: {NOT_A_FILE}", dest.display())));
    }
    if expected.is_some() != target.existing {
        return Err(changed_since_plan(dest));
    }
    let baseline = expected.is_some().then(|| hash_payload(dest)).transpose()?;
    let id = match target.rule.as_ref().and_then(|rule| rule.id.as_ref()).or(recovered_id) {
        Some(id) => id.clone(),
        None => repo.new_item_id()?,
    };
    repo.check_item_id(dest.strip_prefix(repo.root()).unwrap().into(), id.clone())?;
    let content = instructions::restore_metadata(target.rule.as_ref(), text, dest, Some(&id))?;
    fs::create_dir_all(dir).at(dir)?;
    refuse_linked_parent(repo, dest)?;
    crate::source::exclude_stages(repo.root());
    let stage = crate::source::unique(dir);
    let result = crate::config_file::write_new(&stage, content.as_bytes()).and_then(|()| {
        #[cfg(unix)]
        if let Some(mode) = crate::config_file::existing_mode(dest) {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&stage, fs::Permissions::from_mode(mode)).at(&stage)?;
        }
        let displace = Displace {
            repo,
            kind: displaced::Kind::Rule,
            harness: REPO_HOLDER,
            expected: expected.as_deref(),
            ownership: None,
            baseline: baseline.as_deref(),
        };
        swap_in(Some(&stage), dest, displace, None)
    });
    if fs::symlink_metadata(&stage).is_ok() {
        let _ = fs::remove_file(&stage);
    }
    result
}

pub mod displaced {
    //! The recovery area: where sync puts content it displaces, instead of
    //! deleting or overwriting it.
    //!
    //! Every entry sync replaces or removes in a harness (a copied skill or
    //! skill link, a rule file, a block instructions file) is first moved,
    //! or for files hard-linked, into an entry of its own, next to the
    //! fingerprint sync expected it to have. The entry is always on the
    //! destination's own filesystem, in the directory that holds it:
    //! `<holder>/.sk1ll-tmp/displaced/<timestamp>-<harness>-<name>/`, where
    //! the holder is the skills dir for a skill, and the directory of the
    //! file for a rule or a block instructions file (of its target, if it is
    //! a symlink). So displacing is one rename or link, never a copy, and
    //! the entry *is* the displaced original: a writer that holds it open
    //! writes into the entry, however late.
    //!
    //! No harness reads it: the `.sk1ll-tmp` directory is hidden (Codex skips
    //! those) and has no `SKILL.md` of its own, and skills sit two levels
    //! below it, where harnesses only look one level down a skills dir. A
    //! rules directory's harness may load every `.md` file below it, so no
    //! entry holds one there: a rule entry's file is `content` (no
    //! extension), next to `entry.json`. A `.gitignore` keeps the area out of
    //! any git repo the holder is in (dotfiles).
    //!
    //! Pull keeps what it replaces in the skills repo's working tree the same
    //! way, as [`Kind::Draft`] entries whose holder is the repo's git directory.
    //! They are never pruned: they stay until adopted or discarded.
    //!
    //! The state directory ([`root`]) holds an index of where each entry is,
    //! so [`list`] finds them all; [`super::recover`] lists again any entry
    //! of a harness the index lost. Entries older versions kept in the state
    //! directory itself are still listed, adopted, discarded and pruned.
    //!
    //! A rename or a final check can't stop a writer that edits the old
    //! content right after the check, or one that holds it open, so nothing
    //! displaced is deleted on the spot:
    //!
    //! - Once the swap is done the entry is checked again. If it still
    //!   matches, it is only kept for [`GRACE`] (so writers with open files
    //!   are done with it) and then pruned, by a later sync (see [`prune`]).
    //! - If it differs, someone wrote to it during the swap. The swap stands,
    //!   the operation reports [`Error::Displaced`], and the entry stays
    //!   (never pruned) and is listed by [`changed`], until it is adopted
    //!   ([`adopt`]) or discarded ([`discard`]). A later write that makes an
    //!   entry differ keeps it the same way, since pruning checks again.
    //! - If a swap is called off but the content can't go back (something
    //!   new is at the destination), it stays in its entry, marked changed.
    //!
    //! Unchanged entries also go before their grace period is over when all
    //! entries together take more than [`max_size`], oldest first. Pruning
    //! never deletes recursively: see [`crate::hash::remove_payload`].

    use std::collections::HashSet;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use serde::Deserialize;

    use super::*;

    /// How long an unchanged entry is kept before [`prune`] removes it.
    pub const GRACE: Duration = Duration::from_secs(10 * 60);

    /// What all entries may take (see [`max_size`]) unless
    /// `SK1LL_DISPLACED_MAX` says otherwise: 1 GiB.
    pub const DEFAULT_MAX: u64 = 1 << 30;

    const META: &str = "entry.json";
    const CONTENT: &str = "content";
    const VERSION: u32 = 3;
    /// The recovery area's directory within [`TMP_DIR`].
    const AREA: &str = "displaced";
    /// Where in [`root`] the index is: one file per entry, holding its directory.
    const INDEX: &str = ".index";

    /// What kind of harness content an entry holds.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "lowercase")]
    pub enum Kind {
        /// A copied skill directory or a skill link.
        Skill,
        /// One file of a rules directory.
        Rule,
        /// A whole block instructions file (the target's content, if it is linked).
        Instructions,
        /// A file or symlink in the skills repo's working tree that pull
        /// replaced or removed (see [`crate::Repo::pull`]). Its entry is in
        /// the repo's git directory, and its harness is [`PULL`].
        Draft,
    }

    /// The harness [`Kind::Draft`] entries are recorded under.
    pub const PULL: &str = "pull";

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    struct RepoIdentity {
        #[serde(with = "stored_path")]
        root: PathBuf,
        #[cfg(unix)]
        device: u64,
        #[cfg(unix)]
        inode: u64,
    }

    impl RepoIdentity {
        fn of(repo: &Repo) -> Result<Self> {
            let root = repo.root().to_path_buf();
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                let meta = root.metadata().at(&root)?;
                Ok(Self { root, device: meta.dev(), inode: meta.ino() })
            }
            #[cfg(not(unix))]
            Ok(Self { root })
        }
    }

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    struct Provenance {
        repo: RepoIdentity,
        head: Option<String>,
        #[serde(default, with = "stored_path::optional")]
        source: Option<PathBuf>,
    }

    impl Provenance {
        fn current(repo: &Repo, source: Option<&Path>) -> Result<Self> {
            let repo_git = crate::git::RepoGit::with_env(repo.root(), &[("GIT_OPTIONAL_LOCKS".into(), "0".into())])?;
            let git = || repo_git.git();
            let head = if git().args(["rev-parse", "--verify", "--quiet", "HEAD"]).succeeds()? {
                Some(git().args(["rev-parse", "HEAD"]).text()?)
            } else {
                None
            };
            Ok(Self {
                repo: RepoIdentity::of(repo)?,
                head,
                source: source.map(|path| path.strip_prefix(repo.root()).unwrap_or(path).into()),
            })
        }

        fn origin(&self) -> Option<Origin> {
            self.source.as_ref().and_then(|path| {
                if path.starts_with(crate::skill::IMPORTED_DIR) {
                    Some(Origin::Imported)
                } else if path.starts_with(crate::skill::PERSONAL_DIR) {
                    Some(Origin::Personal)
                } else {
                    None
                }
            })
        }
    }

    #[derive(Serialize, Deserialize)]
    struct PlacementId {
        id: Option<ItemId>,
        provenance: Provenance,
    }

    #[derive(Debug, Serialize, Deserialize)]
    struct Meta {
        version: u32,
        harness: String,
        kind: Kind,
        #[serde(with = "stored_path")]
        dest: PathBuf,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        item_id: Option<ItemId>,
        provenance: Option<Provenance>,
        /// The fingerprint sync expected the content to have; empty if it
        /// expected none (so any content is someone else's).
        expected: String,
        /// Unix seconds.
        displaced_at: u64,
        /// Sync couldn't put the content back where it came from: it is
        /// kept, as changed, whatever its fingerprint.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        kept: bool,
        #[serde(default)]
        active: bool,
        #[serde(default)]
        payload: Option<String>,
    }

    mod stored_path {
        use std::path::{Path, PathBuf};

        use serde::{Deserialize, Serialize};

        #[derive(Serialize, Deserialize)]
        #[serde(untagged)]
        enum Stored<'a> {
            Text(std::borrow::Cow<'a, str>),
            Bytes { git_quoted: std::borrow::Cow<'a, str> },
        }

        pub(super) fn serialize<S: serde::Serializer>(path: &Path, serializer: S) -> Result<S::Ok, S::Error> {
            match path.to_str() {
                Some(text) => Stored::Text(text.into()),
                None => Stored::Bytes { git_quoted: crate::draft::quote_path(&crate::git::path_bytes(path)).into() },
            }
            .serialize(serializer)
        }

        pub(super) fn deserialize<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<PathBuf, D::Error> {
            match Stored::deserialize(deserializer)? {
                Stored::Text(text) => Ok(PathBuf::from(text.as_ref())),
                Stored::Bytes { git_quoted } => crate::draft::unquote_path(&git_quoted)
                    .map(|raw| crate::git::path_from_bytes(&raw))
                    .ok_or_else(|| serde::de::Error::custom("invalid quoted path")),
            }
        }

        pub(super) mod optional {
            use std::path::PathBuf;

            use serde::{Deserialize, Serialize};

            #[derive(Serialize, Deserialize)]
            struct Stored(#[serde(with = "super")] PathBuf);

            pub(in super::super) fn serialize<S: serde::Serializer>(
                path: &Option<PathBuf>,
                serializer: S,
            ) -> Result<S::Ok, S::Error> {
                path.clone().map(Stored).serialize(serializer)
            }

            pub(in super::super) fn deserialize<'de, D: serde::Deserializer<'de>>(
                deserializer: D,
            ) -> Result<Option<PathBuf>, D::Error> {
                Ok(Option::<Stored>::deserialize(deserializer)?.map(|Stored(path)| path))
            }
        }
    }

    #[derive(Serialize, Deserialize)]
    struct IndexPath(#[serde(with = "stored_path")] PathBuf);

    /// One displaced item.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize)]
    pub struct Entry {
        /// The entry's directory name; sorts by time.
        pub id: String,
        pub harness: String,
        pub kind: Kind,
        /// Identity of the item when it was placed.
        pub item_id: Option<ItemId>,
        #[serde(skip)]
        provenance: Option<Provenance>,
        /// Where in the harness the content was displaced from.
        #[serde(with = "stored_path")]
        pub dest: PathBuf,
        /// The displaced content itself (a file, directory or symlink).
        #[serde(with = "stored_path")]
        pub path: PathBuf,
        /// Unix seconds.
        pub displaced_at: u64,
        /// It differs from what sync expected to displace: someone wrote to
        /// it during (or after) the swap. Never pruned.
        pub changed: bool,
    }

    /// `displaced` in sk1ll's state directory ([`crate::paths::state_dir`],
    /// `$XDG_STATE_HOME/sk1ll` by default): the index of all entries, and
    /// entries older versions kept here.
    pub fn root() -> Result<PathBuf> {
        #[cfg(test)]
        if let Some(root) = TEST_ROOT.with_borrow(Clone::clone) {
            return Ok(root);
        }
        #[cfg(test)]
        return Ok(DEFAULT_TEST_ROOT.with(|root| root.path().to_path_buf()));
        #[cfg(not(test))]
        Ok(crate::paths::state_dir()?.join("displaced"))
    }

    fn placement_path(kind: Kind, dest: &Path) -> Result<PathBuf> {
        use sha2::{Digest, Sha256};
        let absolute = std::path::absolute(dest).at(dest)?;
        let digest = Sha256::digest(crate::git::path_bytes(&absolute));
        Ok(root()?.join(".placements").join(format!("{kind:?}-{digest:x}")))
    }

    /// Where the record a change in progress gives `dest` waits (see
    /// [`prepare_placement`]).
    fn pending_placement_path(kind: Kind, dest: &Path) -> Result<PathBuf> {
        let mut path = placement_path(kind, dest)?.into_os_string();
        path.push(".pending");
        Ok(path.into())
    }

    fn placement_record(repo: &Repo, source: Option<&Path>, id: Option<&ItemId>) -> Result<Option<PlacementId>> {
        source
            .map(|source| Ok(PlacementId { id: id.cloned(), provenance: Provenance::current(repo, Some(source))? }))
            .transpose()
    }

    fn write_placement(path: &Path, record: &Option<PlacementId>) -> Result<()> {
        make_dirs(path.parent().unwrap())?;
        let bytes = serde_json::to_vec(record).map_err(|e| Error::Invalid(e.to_string()))?;
        crate::config_file::replace_private(path, &bytes)
    }

    /// Records the Item ID and provenance of what sync placed at `dest`
    /// from `source` (`None`: nothing of the repo's is there any more).
    pub(super) fn record_placement(
        repo: &Repo,
        kind: Kind,
        dest: &Path,
        source: Option<&Path>,
        id: Option<&ItemId>,
    ) -> Result<()> {
        let path = placement_path(kind, dest)?;
        match placement_record(repo, source, id)? {
            Some(record) => write_placement(&path, &Some(record))?,
            None => remove_if_there(&path)?,
        }
        remove_if_there(&pending_placement_path(kind, dest)?)
    }

    /// The write-ahead half of [`record_placement`], for a change the
    /// ownership index records as an intent: the record takes effect when
    /// [`settle_placement`] finds the change made, by the same operation or
    /// by the recovery that settles its intent.
    pub(super) fn prepare_placement(
        repo: &Repo,
        kind: Kind,
        dest: &Path,
        source: Option<&Path>,
        id: Option<&ItemId>,
    ) -> Result<()> {
        write_placement(&pending_placement_path(kind, dest)?, &placement_record(repo, source, id)?)
    }

    /// Settles the record [`prepare_placement`] left for `dest`: it takes
    /// effect if the change was `made`, and is dropped otherwise.
    pub(super) fn settle_placement(kind: Kind, dest: &Path, made: bool) -> Result<()> {
        let pending = pending_placement_path(kind, dest)?;
        let Some(record) = read_placement::<Option<PlacementId>>(&pending)? else { return Ok(()) };
        if made {
            let path = placement_path(kind, dest)?;
            match record {
                Some(_) => fs::rename(&pending, &path).at(&path)?,
                None => remove_if_there(&path)?,
            }
        }
        remove_if_there(&pending)
    }

    /// What sync last wrote to a block instructions file it created: the
    /// hash of the whole text, and the file it published: its identity and
    /// payload fingerprint (bytes and mode).
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub(crate) struct CreatedBlock {
        pub(crate) written: String,
        pub(crate) file: FileId,
        pub(crate) payload: String,
    }

    impl CreatedBlock {
        /// Whether `path` is still the very file sync published, unchanged.
        pub(super) fn is(&self, path: &Path) -> Result<bool> {
            Ok(rule_identity(path)? == Some(self.file)
                && fingerprint(Kind::Instructions, path)?.as_deref() == Some(self.written.as_str())
                && hash_payload(path)? == self.payload)
        }
    }

    /// What sync last wrote to the block instructions file at `dest`, if
    /// sync created it (see [`record_created_block`]).
    pub(super) fn created_block(dest: &Path) -> Result<Option<CreatedBlock>> {
        read_placement::<CreatedBlock>(&placement_path(Kind::Instructions, dest)?)
    }

    /// Records that sync created the block instructions file at `dest`
    /// and last published `created` there; `None` forgets it, for good.
    pub(super) fn record_created_block(dest: &Path, created: Option<&CreatedBlock>) -> Result<()> {
        let path = placement_path(Kind::Instructions, dest)?;
        match created {
            Some(created) => {
                make_dirs(path.parent().unwrap())?;
                let bytes = serde_json::to_vec(created).map_err(|e| Error::Invalid(e.to_string()))?;
                crate::config_file::replace_private(&path, &bytes)
            }
            None => remove_if_there(&path),
        }
    }

    fn read_placement<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Option<T>> {
        crate::config_file::read_text(path, false)?
            .map(|text| {
                serde_json::from_str::<T>(&text)
                    .map_err(|error| Error::Parse { path: path.to_path_buf(), message: error.to_string() })
            })
            .transpose()
    }

    fn placed(kind: Kind, dest: &Path) -> Result<Option<PlacementId>> {
        if matches!(kind, Kind::Instructions | Kind::Draft) {
            return Ok(None);
        }
        read_placement(&placement_path(kind, dest)?)
    }

    /// Whether the placement at `dest` lacks the record of coming from
    /// `source` with the Item ID `id`. `pending`: the ownership index has a
    /// change in progress there that recovery will settle as made, so its
    /// prepared record is the one that counts.
    pub(super) fn placement_needs_id(
        repo: &Repo,
        kind: Kind,
        dest: &Path,
        source: &Path,
        id: Option<&ItemId>,
        pending: bool,
    ) -> Result<bool> {
        let prepared = match pending {
            true => read_placement::<Option<PlacementId>>(&pending_placement_path(kind, dest)?)?,
            false => None,
        };
        let placed = match prepared {
            Some(record) => record,
            None => placed(kind, dest)?,
        };
        let root = RepoIdentity::of(repo)?;
        let source = source.strip_prefix(repo.root()).unwrap_or(source);
        Ok(placed.is_none_or(|record| {
            record.id.as_ref() != id
                || record.provenance.repo != root
                || record.provenance.source.as_deref() != Some(source)
        }))
    }

    /// The Item ID sync recorded for what it placed at `dest`: `None` if it
    /// recorded nothing there (placed before Item IDs, say), `Some(None)` if
    /// the item had no Item ID then.
    pub(super) fn placed_id(kind: Kind, dest: &Path) -> Result<Option<Option<ItemId>>> {
        Ok(placed(kind, dest)?.map(|record| record.id))
    }

    pub fn skill_placement_origin(dest: &Path) -> Result<Option<Origin>> {
        Ok(placed(Kind::Skill, dest)?.and_then(|record| record.provenance.origin()))
    }

    /// Where the entries of content displaced from the directory `holder` are.
    pub fn area(holder: &Path) -> PathBuf {
        holder.join(TMP_DIR).join(AREA)
    }

    /// A cheap fingerprint of what [`list`] reads, from `lstat` and the small
    /// index files: the state directory (the index, placement records, older
    /// entries kept there) and the whole payload of every entry the index
    /// places elsewhere (beside a skills directory, an instructions file or a
    /// repo's git directory).
    pub fn stamp() -> Result<Stamp> {
        let root = root()?;
        let mut parts = vec![(String::new(), Stamp::of(&root, None, false)?)];
        for id in ids(&root)? {
            if let Some(dir) = indexed(&root, &id) {
                parts.push((id, Stamp::of(&dir, None, false)?));
            }
        }
        Ok(Stamp::combine("recovery", parts))
    }

    /// Every entry, oldest first.
    pub fn list() -> Result<Vec<Entry>> {
        let root = root()?;
        let mut entries = Vec::new();
        for id in ids(&root)? {
            if let Some(dir) = locate(&root, &id)
                && let Some((meta, path)) = load(&dir)?
            {
                entries.push(entry(id, meta, path)?);
            }
        }
        Ok(entries)
    }

    /// The entries that were written to while (or after) sync displaced
    /// them: what the user has to adopt or discard.
    pub fn changed() -> Result<Vec<Entry>> {
        Ok(list()?.into_iter().filter(|e| e.changed).collect())
    }

    pub fn get(id: &str) -> Result<Entry> {
        find(id)?.ok_or_else(|| Error::Invalid(format!("no displaced entry `{id}`")))
    }

    /// [`get`], with `None` only for an entry confirmed gone (adopted or
    /// discarded): no index record and no legacy directory, no directory
    /// where the record points, or no content in it. Anything that can't
    /// be read (the record, the directory, the content, the metadata) is
    /// an error, so a caller never takes an unreadable entry for a gone one.
    pub fn find(id: &str) -> Result<Option<Entry>> {
        let root = root()?;
        let Some(dir) = locate_checked(&root, id)? else { return Ok(None) };
        let content = dir.join(CONTENT);
        match fs::symlink_metadata(&content) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).at(&content),
            Ok(_) => {}
        }
        let meta_path = dir.join(META);
        let text = fs::read_to_string(&meta_path).at(&meta_path)?;
        let meta: Meta = serde_json::from_str(&text)
            .map_err(|e| Error::Parse { path: meta_path.clone(), message: e.to_string() })?;
        if meta.version != VERSION {
            return Err(Error::Invalid(format!("recovery entry `{id}` is of another version of sk1ll")));
        }
        entry(id.to_string(), meta, content).map(Some)
    }

    /// [`locate`] for [`find`]: `None` only where the entry is confirmed
    /// gone; a record or directory that can't be read is an error.
    fn locate_checked(root: &Path, id: &str) -> Result<Option<PathBuf>> {
        if !valid_id(id) {
            return Ok(None);
        }
        let record = root.join(INDEX).join(id);
        let text = match fs::read_to_string(&record) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let legacy = root.join(id);
                return match fs::symlink_metadata(&legacy) {
                    Ok(m) if m.is_dir() => Ok(Some(legacy)),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                    Ok(_) => Err(Error::Invalid(format!("{} is not a recovery entry", legacy.display()))),
                    Err(e) => Err(e).at(&legacy),
                };
            }
            Err(e) => return Err(e).at(&record),
        };
        let IndexPath(dir) =
            serde_json::from_str(&text).map_err(|e| Error::Parse { path: record.clone(), message: e.to_string() })?;
        match fs::symlink_metadata(&dir) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).at(&dir),
            Ok(_) => {}
        }
        indexed(root, id).map(Some).ok_or_else(|| {
            Error::Invalid(format!("the recovery index record {} is not a recovery entry", record.display()))
        })
    }

    /// Deletes an entry and its content for good, whatever it holds: the
    /// one recursive deletion, on the user's explicit request.
    pub fn discard(id: &str) -> Result<()> {
        let root = root()?;
        let entry = get(id)?;
        if entry.kind == Kind::Draft
            && let Some(git_dir) = entry.path.ancestors().nth(4)
            && crate::draft::pull_journal_exists(git_dir)?
        {
            return Err(Error::Invalid(
                "pull pending. Finish the pull before discarding Draft recovery content".into(),
            ));
        }
        let dir = locate(&root, id).expect("an entry just loaded");
        match fs::remove_dir_all(&dir) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e).at(&dir),
            _ => {}
        }
        release(&root, id, &dir).map(drop)
    }

    pub fn discard_from(repo: &Repo, id: &str) -> Result<()> {
        let entry = get(id)?;
        check_repo(repo, &entry)?;
        discard(id)
    }

    fn check_repo(repo: &Repo, entry: &Entry) -> Result<()> {
        let other = match &entry.provenance {
            Some(provenance) => (RepoIdentity::of(repo)? != provenance.repo).then(|| provenance.repo.root.clone()),
            // Pull's entries are in the git directory of the repo they came from.
            None if entry.kind == Kind::Draft => {
                let holder = entry.path.ancestors().nth(4).unwrap_or(&entry.path);
                let out = crate::git::Git::repo(repo.root())?.args(["rev-parse", "--absolute-git-dir"]).run()?;
                let git_dir = crate::git::path_from_bytes(out.strip_suffix(b"\n").unwrap_or(&out));
                let same = matches!((git_dir.canonicalize(), holder.canonicalize()), (Ok(a), Ok(b)) if a == b);
                (!same).then(|| holder.to_path_buf())
            }
            None => None,
        };
        match other {
            Some(root) => Err(Error::Invalid(format!(
                "recovery entry `{}` belongs to another repo ({})",
                entry.id,
                root.display()
            ))),
            None => Ok(()),
        }
    }

    /// Where [`adopt`] puts an entry's content. Refused for an entry of
    /// another repo, and when the Item ID it was placed with no longer leads
    /// to the item it came from.
    pub fn target(repo: &Repo, entry: &Entry) -> Result<AdoptTarget> {
        check_repo(repo, entry)?;
        if entry.kind == Kind::Draft {
            return adopt_target(repo, entry.kind, &entry.dest, None);
        }
        let provenance = entry.provenance.as_ref().ok_or_else(|| recovery_target_changed(entry.item_id.as_ref()))?;
        let mut target = adopt_target(repo, entry.kind, &entry.dest, entry.item_id.as_ref())?;
        if target.existing {
            let Some(id) = entry.item_id.as_ref() else { return Err(recovery_target_changed(None)) };
            if !history_connects(repo, provenance, id)? {
                return Err(recovery_target_changed(Some(id)));
            }
        }
        target.placed_origin = provenance.origin();
        Ok(target)
    }

    fn history_connects(repo: &Repo, placed: &Provenance, id: &ItemId) -> Result<bool> {
        let Some(start) = placed.head.as_deref() else { return Ok(true) };
        let root = repo.root();
        let repo_git = crate::git::RepoGit::with_env(root, &[("GIT_OPTIONAL_LOCKS".into(), "0".into())])?;
        let git = || repo_git.git();
        if !git().args(["merge-base", "--is-ancestor", start, "HEAD"]).succeeds()? {
            return Ok(false);
        }
        let contains = |rev| {
            crate::item_id::claims_at(root, rev, |_| true, true)
                .map(|claims| claims.values().any(|claimed| claimed.as_ref() == Some(id)))
        };
        let mut seen = contains(start)?;
        let history = git().args(["rev-list", "--ancestry-path", "--reverse", &format!("{start}..HEAD")]).text()?;
        for rev in history.lines() {
            let current = contains(rev)?;
            if seen && !current {
                return Ok(false);
            }
            seen |= current;
        }
        Ok(true)
    }

    /// Takes an entry's content back, then deletes the entry:
    ///
    /// - A copied skill becomes the repo's version of that skill, and a rule
    ///   file its rule (as [`super::adopt`] does for a conflict): a draft
    ///   change, which the next sync brings to the harness.
    /// - A block instructions file goes back into the harness (one block
    ///   mixes several rules, so it can't go into the repo), with the
    ///   current managed block in place of its own unless its own was edited
    ///   by hand; the next plan then shows that as a conflict. The file it
    ///   replaces is displaced in turn. Refused while the current managed
    ///   block is edited by hand: that conflict has to be resolved first, or
    ///   the block would lose the marker that makes it one.
    ///
    /// - A file pull displaced from the repo's working tree goes back to its
    ///   path as a Draft change (see [`crate::draft::adopt_displaced`]).
    ///
    /// The entry goes only if it holds nothing but what was taken back: a
    /// skill's `.git` (which copies leave out), or a write since, keeps it,
    /// as changed. Returns where the content went.
    pub fn adopt(repo: &Repo, id: &str, lock: &crate::EngineLock) -> Result<PathBuf> {
        let entry = get(id)?;
        let target = target(repo, &entry)?;
        let (payload, displaced) = match entry.kind {
            Kind::Skill if fs::symlink_metadata(&entry.path).at(&entry.path)?.file_type().is_symlink() => {
                return Err(Error::Invalid(format!(
                    "displaced entry `{id}` is a skill link, which holds no content of its own; discard it"
                )));
            }
            Kind::Skill => {
                let recovered = entry.item_id.as_ref();
                let (_, payload, displaced) =
                    copy_into_repo(repo, &entry.path, &target, recovered, target.placed_origin)?;
                (payload, displaced)
            }
            Kind::Rule => {
                let (payload, text) = crate::hash::read_payload(&entry.path, || read_text(&entry.path))?;
                (payload, rule_into_repo(repo, &target, &text, entry.item_id.as_ref())?)
            }
            Kind::Instructions => {
                let payload = hash_payload(&entry.path)?;
                (payload, restore_instructions(repo, &entry)?)
            }
            Kind::Draft => {
                crate::draft::adopt_displaced(repo, &entry, lock)?;
                return Ok(target.path);
            }
        };
        let root = root()?;
        let dir = locate(&root, id).ok_or_else(|| Error::Invalid(format!("no displaced entry `{id}`")))?;
        if crate::hash::remove_payload(&entry.path, &payload)? {
            release(&root, id, &dir)?;
        } else {
            keep_entry(&dir)?;
        }
        displaced.map_or(Ok(target.path), Err)
    }

    fn restore_instructions(repo: &Repo, entry: &Entry) -> Result<Option<Error>> {
        let dest = &entry.dest;
        let current = instructions::read_optional(dest)?.unwrap_or_default();
        let managed = instructions::find_block(&current, dest)?;
        if managed.as_ref().is_some_and(|b| b.edited()) {
            return Err(Error::Invalid(format!(
                "{}: the managed block is edited by hand (a conflict); move those edits into instructions/ or \
                 overwrite them with `sk1ll sync --force` first, then adopt displaced entry `{}`",
                dest.display(),
                entry.id
            )));
        }
        let mut text = read_text(&entry.path)?;
        if let Some(own) = instructions::find_block(&text, &entry.path)?
            && !own.edited()
        {
            text = instructions::splice(&text, Some(&own), managed.map(|b| b.inner).as_deref());
        }
        let unchanged = || match instructions::read_optional(dest)?.unwrap_or_default() == current {
            true => Ok(()),
            false => Err(changed_since_plan(dest)),
        };
        let expected = hash_str(&current);
        let displace = Displace {
            repo,
            kind: Kind::Instructions,
            harness: &entry.harness,
            expected: Some(&expected),
            ownership: None,
            baseline: None,
        };
        replace_file(dest, &text, unchanged, displace, OldCopy::Keep)
    }

    /// Removes the entries that still match what sync expected and are older
    /// than [`GRACE`], and the remains of entries that were never completed.
    /// Then, while all entries take more than [`max_size`], the oldest of the
    /// unchanged ones left, however young. Changed entries are never pruned.
    /// Returns the ids removed. Sync runs this whenever it displaces something new.
    pub fn prune() -> Result<Vec<String>> {
        prune_with(GRACE, max_size())
    }

    /// `SK1LL_DISPLACED_MAX`: bytes, or a number with `K`, `M`, `G` or `T`
    /// (KiB and so on); else [`DEFAULT_MAX`].
    pub fn max_size() -> u64 {
        std::env::var("SK1LL_DISPLACED_MAX").ok().and_then(|v| parse_size(&v)).unwrap_or(DEFAULT_MAX)
    }

    fn parse_size(s: &str) -> Option<u64> {
        let s = s.trim();
        let (number, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len()));
        let unit = unit.trim().to_ascii_uppercase();
        let shift = match unit.trim_end_matches("IB").trim_end_matches('B') {
            "" => 0,
            "K" => 10,
            "M" => 20,
            "G" => 30,
            "T" => 40,
            _ => return None,
        };
        number.parse::<u64>().ok()?.checked_mul(1 << shift)
    }

    #[cfg(test)]
    pub(super) fn prune_older_than(age: Duration) -> Result<Vec<String>> {
        prune_with(age, u64::MAX)
    }

    pub(crate) fn prune_with(age: Duration, max: u64) -> Result<Vec<String>> {
        let root = root()?;
        let now = now();
        let old = |since: u64| now.saturating_sub(since) >= age.as_secs();
        let modified = |path: &Path| fs::symlink_metadata(path).and_then(|m| m.modified()).ok().map(secs);
        let mut pruned = Vec::new();
        let mut inodes = HashSet::new();
        let (mut total, mut unchanged) = (0u64, Vec::new());
        for id in ids(&root)? {
            let pointer = root.join(INDEX).join(&id);
            let dir = locate(&root, &id);
            let loaded = dir.as_deref().map(load).transpose()?.flatten();
            match (dir, loaded) {
                (Some(dir), Some((meta, path))) => {
                    let size = disk_size(&path, &mut inodes);
                    if protected(&id, &meta) || differs(&meta, &path)? {
                        total += size;
                    } else if old(meta.displaced_at) {
                        #[cfg(test)]
                        hook(Hook::BeforePrune, &path);
                        let Some((meta, path)) = load(&dir)? else { continue };
                        if protected(&id, &meta) || differs(&meta, &path)? || !retire(&root, &id, &dir, &meta)? {
                            total += size;
                            continue;
                        }
                        pruned.push(id);
                    } else {
                        total += size;
                        unchanged.push((id, dir, size));
                    }
                }
                // Begun but never filled (killed meanwhile), or not ours.
                (dir, None) => {
                    // A record an older or newer engine wrote may not load.
                    // Never prune it as an abandoned stage if it has content.
                    if dir.as_ref().is_some_and(|dir| dir.join(CONTENT).symlink_metadata().is_ok()) {
                        continue;
                    }
                    // A writer may have reserved the recovery directory
                    // and be about to move the incumbent into it.
                    if dir.as_ref().is_some_and(|dir| dir.join(META).exists()) {
                        continue;
                    }
                    let since = dir.as_deref().and_then(modified).or_else(|| modified(&pointer));
                    if since.is_some_and(old) {
                        let released = match dir {
                            Some(dir) => release(&root, &id, &dir)?,
                            None => remove_if_there(&pointer).map(|()| true)?,
                        };
                        if released {
                            pruned.push(id);
                        }
                    }
                }
                (None, Some(_)) => unreachable!(),
            }
        }
        for (id, dir, size) in unchanged {
            if total <= max {
                break;
            }
            #[cfg(test)]
            hook(Hook::BeforePrune, &dir.join(CONTENT));
            let Some((meta, path)) = load(&dir)? else { continue };
            if protected(&id, &meta) || differs(&meta, &path)? || !retire(&root, &id, &dir, &meta)? {
                continue;
            }
            total -= size;
            pruned.push(id);
        }
        Ok(pruned)
    }

    /// Bytes `path` takes, not counting files already in `inodes` again.
    fn disk_size(path: &Path, inodes: &mut HashSet<(u64, u64)>) -> u64 {
        let mut size = 0;
        for entry in WalkDir::new(path).into_iter().filter_map(std::result::Result::ok) {
            let Ok(meta) = entry.metadata() else { continue };
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                if meta.is_file() && meta.nlink() > 1 && !inodes.insert((meta.dev(), meta.ino())) {
                    continue;
                }
            }
            size += meta.len();
        }
        #[cfg(not(unix))]
        let _ = inodes;
        size
    }

    /// Lists in the index every entry in `holder`'s area it lacks (the state
    /// directory was lost or moved, say).
    pub(crate) fn reindex(holder: &Path) -> Result<()> {
        let tmp = holder.join(TMP_DIR);
        let area = area(holder);
        for dir in [&tmp, &area] {
            match dir.symlink_metadata() {
                Ok(meta) if meta.is_dir() => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(e) => return Err(e).at(dir),
                Ok(_) => return Err(Error::Invalid(format!("{} is not a plain recovery directory", dir.display()))),
            }
        }
        let names = read_dir_names(&area)?;
        if names.is_empty() {
            return Ok(());
        }
        let root = root()?;
        let area = std::path::absolute(&area).at(&area)?;
        for id in names {
            let dir = area.join(&id);
            if valid_id(&id) && locate(&root, &id).is_none() && load(&dir)?.is_some() {
                make_dirs(&root.join(INDEX))?;
                claim(&root, &id, &dir)?;
            }
        }
        Ok(())
    }

    /// Every id the index or the state directory itself has.
    fn ids(root: &Path) -> Result<BTreeSet<String>> {
        let mut ids: BTreeSet<_> = read_dir_names(root)?.into_iter().filter(|id| valid_id(id)).collect();
        ids.extend(read_dir_names(&root.join(INDEX))?.into_iter().filter(|id| valid_id(id)));
        Ok(ids)
    }

    /// An entry's directory: where the index says, else in the state directory itself.
    fn locate(root: &Path, id: &str) -> Option<PathBuf> {
        if !valid_id(id) {
            return None;
        }
        if let Some(dir) = indexed(root, id) {
            return Some(dir);
        }
        let legacy = root.join(id);
        legacy.is_dir().then_some(legacy)
    }

    /// The directory the index records for `id`, if it is one an entry can
    /// be in: nothing else is ever read from or deleted through the index.
    fn indexed(root: &Path, id: &str) -> Option<PathBuf> {
        let text = fs::read_to_string(root.join(INDEX).join(id)).ok()?;
        let IndexPath(dir) = serde_json::from_str(&text).ok()?;
        let area = dir.parent()?;
        let ok = dir.is_absolute()
            && dir.file_name()? == id
            && area.file_name()? == AREA
            && area.parent()?.file_name()? == TMP_DIR;
        if ok
            && [Some(&*dir), Some(area), area.parent()]
                .into_iter()
                .flatten()
                .all(|ancestor| ancestor.symlink_metadata().is_ok_and(|m| m.is_dir()))
        {
            Some(dir)
        } else {
            None
        }
    }

    /// Records `dir` as `id`'s entry in the index, unless `id` is taken
    /// (`false`).
    fn claim(root: &Path, id: &str, dir: &Path) -> Result<bool> {
        use std::io::Write;
        let path = root.join(INDEX).join(id);
        let mut file = match fs::OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Ok(false),
            Err(e) => return Err(e).at(&path),
        };
        let body = serde_json::to_string(&IndexPath(dir.to_path_buf())).map_err(|e| Error::Invalid(e.to_string()))?;
        if let Err(e) = file.write_all(body.as_bytes()).and_then(|()| file.sync_all()) {
            let _ = fs::remove_file(&path);
            return Err(e).at(&path);
        }
        crate::config_file::sync_dir(&path)?;
        Ok(true)
    }

    /// Prunes an unchanged entry: its content goes only file by file, each
    /// while it still matches the fingerprint recorded for it (see
    /// [`crate::hash::remove_payload`]). Whatever stays keeps the entry,
    /// marked changed: `false`.
    fn retire(root: &Path, id: &str, dir: &Path, meta: &Meta) -> Result<bool> {
        let Some(payload) = &meta.payload else { return Ok(false) };
        if crate::hash::remove_payload(&dir.join(CONTENT), payload)? {
            return release(root, id, dir);
        }
        keep_entry(dir)?;
        Ok(false)
    }

    /// Deletes what is left of the entry `id` at `dir` once its content is
    /// gone, and its index record, and the area once it is empty. Never
    /// recursive: an entry still holding content, or anything else, stays
    /// (`false`).
    fn release(root: &Path, id: &str, dir: &Path) -> Result<bool> {
        if dir.join(CONTENT).symlink_metadata().is_ok() {
            return Ok(false);
        }
        remove_if_there(&dir.join(META))?;
        match fs::remove_dir(dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) if matches!(e.kind(), std::io::ErrorKind::DirectoryNotEmpty | std::io::ErrorKind::AlreadyExists) => {
                return Ok(false);
            }
            Err(e) => return Err(e).at(dir),
        }
        remove_if_there(&root.join(INDEX).join(id))?;
        // Never the state directory itself (older versions' entries).
        let named = |p: Option<&Path>, name: &str| p.and_then(Path::file_name) == Some(name.as_ref());
        let in_area = |a: &&Path| named(Some(a), AREA) && named(a.parent(), TMP_DIR);
        if let Some(area) = dir.parent().filter(in_area) {
            tidy(area);
        }
        Ok(true)
    }

    /// Marks the entry at `dir` kept: changed, never pruned.
    fn keep_entry(dir: &Path) -> Result<()> {
        let path = dir.join(META);
        let text = fs::read_to_string(&path).at(&path)?;
        let parsed = serde_json::from_str::<Meta>(&text);
        let mut meta = parsed.map_err(|e| Error::Parse { path, message: e.to_string() })?;
        meta.kept = true;
        meta.active = false;
        save_meta(dir, &meta)
    }

    /// Removes an area left with no entries, and the scratch directory around it if that is empty too.
    fn tidy(area: &Path) {
        if read_dir_names(area).is_ok_and(|names| names.iter().all(|n| n == ".gitignore")) {
            let _ = fs::remove_file(area.join(".gitignore"));
            if fs::remove_dir(area).is_ok()
                && let Some(tmp) = area.parent()
            {
                let _ = fs::remove_dir(tmp);
            }
        }
    }

    pub(super) fn remove_if_there(path: &Path) -> Result<()> {
        match fs::remove_file(path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e).at(path),
            _ => Ok(()),
        }
    }

    fn make_dirs(dir: &Path) -> Result<()> {
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        builder.create(dir).at(dir)
    }

    fn now() -> u64 {
        secs(SystemTime::now())
    }

    fn secs(t: SystemTime) -> u64 {
        t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
    }

    fn valid_id(id: &str) -> bool {
        !id.is_empty() && !id.starts_with('.') && !id.contains(['/', '\\'])
    }

    /// An entry's metadata, if it is one this version reads.
    fn read_meta(dir: &Path) -> Option<Meta> {
        let text = instructions::read_optional(&dir.join(META)).ok().flatten()?;
        serde_json::from_str::<Meta>(&text).ok().filter(|meta| meta.version == VERSION)
    }

    /// An entry's metadata and content path; `None` unless both are there.
    fn load(dir: &Path) -> Result<Option<(Meta, PathBuf)>> {
        let path = dir.join(CONTENT);
        Ok(read_meta(dir).filter(|_| fs::symlink_metadata(&path).is_ok()).map(|meta| (meta, path)))
    }

    fn entry(id: String, meta: Meta, path: PathBuf) -> Result<Entry> {
        let changed = differs(&meta, &path)?;
        let Meta { harness, kind, item_id, provenance, dest, displaced_at, .. } = meta;
        Ok(Entry { id, harness, kind, item_id, provenance, dest, path, displaced_at, changed })
    }

    fn differs(meta: &Meta, path: &Path) -> Result<bool> {
        if meta.kept
            || meta.expected.is_empty()
            || fingerprint(meta.kind, path).ok().flatten().as_deref() != Some(&meta.expected)
        {
            return Ok(true);
        }
        // Backups without a baseline are separately protected from pruning.
        Ok(meta.payload.as_ref().is_some_and(|expected| hash_payload(path).ok().as_ref() != Some(expected)))
    }

    fn protected(id: &str, meta: &Meta) -> bool {
        // Pull's Draft entries are only ever adopted or discarded.
        if meta.kind == Kind::Draft || meta.active || meta.payload.is_none() || meta.dest.parent().is_none() {
            return true;
        }
        let holder = meta.dest.parent().unwrap();
        if meta.kind == Kind::Instructions {
            return false;
        }
        load_index(holder).map_or(true, |index| index.pending.values().any(|op| op.recovery() == Some(id)))
    }

    pub(super) fn settle(id: &str, dest: &Path, keep: bool) -> Result<()> {
        let root = root()?;
        let Some(dir) = locate(&root, id) else { return Ok(()) };
        let Some((mut meta, path)) = load(&dir)? else { return Ok(()) };
        if meta.dest != dest {
            return Err(changed_since_plan(dest));
        }
        meta.active = false;
        meta.kept |= keep;
        save_meta(&dir, &meta)?;
        sync_recovery(&path, dest)
    }

    fn save_meta(dir: &Path, meta: &Meta) -> Result<()> {
        let body = serde_json::to_vec_pretty(meta).map_err(|e| Error::Invalid(e.to_string()))?;
        crate::config_file::replace_private(&dir.join(META), &body)
    }

    fn sync_recovery(path: &Path, dest: &Path) -> Result<()> {
        for at in path.ancestors().take(3) {
            crate::config_file::sync_dir(at)?;
        }
        // The directory the content left can be gone since (pull prunes the
        // directories it empties): then there is nothing to make durable there.
        match crate::config_file::sync_dir(dest) {
            Err(Error::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => Ok(()),
            result => result,
        }
    }

    fn read_text(path: &Path) -> Result<String> {
        let bytes = crate::config_file::read_regular(path, false)?
            .ok_or_else(|| Error::Invalid(format!("{} no longer exists", path.display())))?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    /// What sync compares content by: a skill as [`observe_skill`] sees it,
    /// a file by the hash of its text.
    pub(crate) fn fingerprint(kind: Kind, path: &Path) -> Result<Option<String>> {
        match kind {
            Kind::Skill => Ok(observe_skill(path)?.state),
            Kind::Rule => super::rule_state(path),
            Kind::Instructions => {
                Ok(crate::config_file::read_regular(path, false)?
                    .map(|bytes| hash_str(&String::from_utf8_lossy(&bytes))))
            }
            Kind::Draft => match fs::symlink_metadata(path) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(e).at(path),
                Ok(m) if m.file_type().is_symlink() => {
                    Ok(Some(leaf_fingerprint(None, &crate::git::path_bytes(&fs::read_link(path).at(path)?))))
                }
                Ok(m) if m.is_file() => {
                    #[cfg(unix)]
                    let mode = std::os::unix::fs::PermissionsExt::mode(&m.permissions()) & 0o7777;
                    #[cfg(not(unix))]
                    let mode = 0;
                    Ok(crate::config_file::read_regular(path, false)?.map(|bytes| leaf_fingerprint(Some(mode), &bytes)))
                }
                Ok(_) => Ok(Some("not a file".into())),
            },
        }
    }

    /// What a [`Kind::Draft`] entry is compared by: whether it is a symlink,
    /// and its exact bytes (a symlink's target).
    pub(crate) fn leaf_fingerprint(mode: Option<u32>, bytes: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        let kind = mode.map_or_else(|| "link".into(), |mode| format!("file:{mode:o}"));
        format!("{kind}:{}", crate::hash::hex(&Sha256::digest(bytes)))
    }

    pub(crate) fn same_file(a: &Path, b: &Path) -> bool {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            match (fs::metadata(a), fs::metadata(b)) {
                (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
                _ => false,
            }
        }
        #[cfg(not(unix))]
        {
            let _ = (a, b);
            false
        }
    }

    /// File-name-safe `s`, without dots: an entry's directory never ends in
    /// `.md` (a rules dir's harness may load every `.md` below it).
    fn slug(s: &str) -> String {
        let safe = |c: char| c.is_ascii_alphanumeric() || matches!(c, '-' | '_');
        s.chars().map(|c| if safe(c) { c } else { '_' }).collect()
    }

    /// Moves `aside` (left behind by an interrupted sync, in `holder`) into
    /// a new entry for `dest`, and returns it.
    pub(crate) fn rescue(
        kind: Kind,
        harness: &str,
        dest: &Path,
        aside: &Path,
        expected: Option<&str>,
        holder: &Path,
    ) -> Result<Entry> {
        let mut held = Displacing::begin(None, kind, harness, dest, expected, None, holder)?;
        if let Err(e) = held.take(aside) {
            held.abandon();
            return Err(e);
        }
        let id = held.id.clone();
        held.finish(false)?;
        get(&id)
    }

    /// How a [`Displacing`] holds the displaced content.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Held {
        /// Moved into the entry: the destination no longer has it.
        Moved,
        /// Hard-linked into the entry: still in place at the destination too.
        Linked,
    }

    /// One displacement in progress (see the module docs).
    pub(crate) struct Displacing {
        id: String,
        root: PathBuf,
        /// The entry's directory.
        dir: PathBuf,
        kind: Kind,
        dest: PathBuf,
        expected: String,
        held: Option<Held>,
    }

    impl Displacing {
        /// Creates the entry in `holder`'s area (and prunes old ones).
        /// Nothing is displaced yet.
        ///
        /// The entry records the Item ID and provenance the destination was
        /// placed with, else (given `repo`) where the content is from now.
        pub(crate) fn begin(
            repo: Option<&Repo>,
            kind: Kind,
            harness: &str,
            dest: &Path,
            expected: Option<&str>,
            payload: Option<&str>,
            holder: &Path,
        ) -> Result<Self> {
            let recorded = placed(kind, dest)?;
            let (item_id, provenance) = match recorded {
                Some(record) => (record.id, Some(record.provenance)),
                None => (None, repo.map(|repo| Provenance::current(repo, None)).transpose()?),
            };
            let root = root()?;
            make_dirs(&root.join(INDEX))?;
            // Best effort: a stale entry that can't be pruned is no reason not
            // to sync. Pull prepares its entries before it changes anything.
            if kind != Kind::Draft {
                let _ = prune();
            }
            let area = area(holder);
            mkdir_private(&holder.join(TMP_DIR))?;
            mkdir_private(&area)?;
            let ignore = area.join(".gitignore");
            if fs::symlink_metadata(&ignore).is_err() {
                fs::write(&ignore, "*\n").at(&ignore)?;
            }
            let area = std::path::absolute(&area).at(&area)?;
            let millis = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis());
            let name = dest.file_name().unwrap_or_default().to_string_lossy();
            let bounded = |s: &str| slug(s).chars().take(64).collect::<String>();
            let base = format!("{millis:013}-{}-{}", bounded(harness), bounded(&name));
            let mut ids = (0..).map(|n| if n == 0 { base.clone() } else { format!("{base}-{n}") });
            let (id, dir) = loop {
                let id = ids.next().expect("an unused id");
                let dir = area.join(&id);
                // The index is what makes an id unique, across every area.
                if root.join(&id).exists() || !claim(&root, &id, &dir)? {
                    continue;
                }
                match fs::create_dir(&dir) {
                    Ok(()) => break (id, dir),
                    Err(e) => {
                        let _ = fs::remove_file(root.join(INDEX).join(&id));
                        if e.kind() != std::io::ErrorKind::AlreadyExists {
                            return Err(e).at(&dir);
                        }
                    }
                }
            };
            let meta = Meta {
                version: VERSION,
                harness: harness.to_string(),
                kind,
                dest: dest.to_path_buf(),
                item_id,
                provenance,
                expected: expected.unwrap_or_default().to_string(),
                displaced_at: now(),
                kept: false,
                active: true,
                payload: payload.map(str::to_owned),
            };
            let expected = meta.expected.clone();
            let held = Self { id, root, dir, kind, dest: dest.to_path_buf(), expected, held: None };
            if let Err(e) = held.write_meta(&meta) {
                let _ = release(&held.root, &held.id, &held.dir);
                return Err(e);
            }
            Ok(held)
        }

        fn write_meta(&self, meta: &Meta) -> Result<()> {
            save_meta(&self.dir, meta)
        }

        /// Takes up again the [`Kind::Draft`] entry `id` an earlier pull
        /// prepared in `holder`'s area.
        #[cfg(unix)]
        pub(crate) fn draft(holder: &Path, id: &str) -> Result<Self> {
            let root = root()?;
            if !valid_id(id) {
                return Err(Error::Invalid("invalid pull recovery id".into()));
            }
            let dir = area(holder).join(id);
            let meta = read_meta(&dir)
                .filter(|meta| meta.kind == Kind::Draft)
                .ok_or_else(|| Error::Invalid(format!("cannot read pull recovery entry at {}", dir.display())))?;
            let held = fs::symlink_metadata(dir.join(CONTENT)).is_ok().then_some(Held::Moved);
            Ok(Self { id: id.into(), root, dir, kind: Kind::Draft, dest: meta.dest, expected: meta.expected, held })
        }

        #[cfg(unix)]
        pub(crate) fn expects(&self, fingerprint: &str) -> bool {
            self.expected == fingerprint
        }

        pub(crate) fn id(&self) -> &str {
            &self.id
        }

        #[cfg(unix)]
        pub(crate) fn filled(&self) -> bool {
            self.held.is_some()
        }

        /// Drops the entry `id` once its content has gone elsewhere.
        pub(crate) fn forget_empty(id: &str) -> Result<()> {
            let root = root()?;
            let dir = locate(&root, id).ok_or_else(|| Error::Invalid(format!("no displaced entry `{id}`")))?;
            if fs::symlink_metadata(dir.join(CONTENT)).is_ok() || !release(&root, id, &dir)? {
                return Err(Error::Invalid(format!("displaced entry `{id}` still holds content")));
            }
            Ok(())
        }

        /// Where the displaced content is (or goes) in the entry.
        pub(crate) fn content(&self) -> PathBuf {
            self.dir.join(CONTENT)
        }

        /// [`take`](Self::take) for `name` in the directory `parent`,
        /// never following a symlink to it; `false` if nothing is there.
        #[cfg(unix)]
        pub(crate) fn take_at(&mut self, parent: &std::os::fd::OwnedFd, name: &std::ffi::OsStr) -> Result<bool> {
            let content = self.content();
            match rustix::fs::renameat(parent, name, rustix::fs::CWD, &content) {
                Ok(()) => {}
                Err(rustix::io::Errno::NOENT) => return Ok(false),
                Err(e) => return Err(std::io::Error::from(e)).at(&self.dest),
            }
            let dest = self.dest.clone();
            self.mark_moved(&dest)?;
            Ok(true)
        }

        /// Moves `src` (the destination, or a leftover aside) into the
        /// entry. It is in the same directory tree, so this is one rename;
        /// if that fails anyway (a mount point in between), nothing moved.
        pub(crate) fn take(&mut self, src: &Path) -> Result<()> {
            let content = self.content();
            rename(src, &content).at(src)?;
            self.mark_moved(src)
        }

        pub(super) fn mark_moved(&mut self, src: &Path) -> Result<()> {
            self.held = Some(Held::Moved);
            sync_recovery(&self.content(), src)
        }

        /// Hard-links the file `real` into the entry, leaving it in place;
        /// `false` if there is no such file.
        pub(super) fn link(&mut self, real: &Path) -> Result<bool> {
            match hard_link(real, &self.content()) {
                Ok(()) => self.held = Some(Held::Linked),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
                Err(e) => return Err(e).at(real),
            }
            crate::config_file::sync_dir(&self.content())?;
            Ok(true)
        }

        /// Whether the displaced content still matches what was expected.
        pub(crate) fn intact(&self) -> Result<bool> {
            let at = if self.held.is_some() { self.content() } else { self.dest.clone() };
            Ok(!self.expected.is_empty() && fingerprint(self.kind, &at)?.unwrap_or_default() == self.expected)
        }

        /// Puts the content back at the destination (nothing was replaced
        /// yet) and drops the entry; only ever where nothing is (see
        /// [`place`]). If something new is there, it stays, and so does the
        /// content, in its entry, marked changed: [`Error::Displaced`].
        pub(super) fn put_back(self) -> Result<()> {
            if self.held != Some(Held::Moved) {
                self.abandon();
                return Ok(());
            }
            if place(&self.content(), &self.dest).is_ok() {
                self.abandon();
                return Ok(());
            }
            keep_entry(&self.dir)?;
            Err(self.displaced_error())
        }

        /// Drops the entry, unless it holds content that was moved here: a
        /// hard link can go (the original is still in place), that can't, so
        /// it is kept instead.
        pub(crate) fn abandon(self) {
            if self.held == Some(Held::Moved) && fs::symlink_metadata(self.content()).is_ok() {
                let _ = keep_entry(&self.dir);
                return;
            }
            if self.held == Some(Held::Linked) {
                let _ = fs::remove_file(self.content());
            }
            let _ = release(&self.root, &self.id, &self.dir);
        }

        /// After the swap: checks the content one last time.
        /// `Some(Error::Displaced)` if it changed; otherwise the entry is
        /// left for [`prune`].
        pub(crate) fn finish(self, pending: bool) -> Result<Option<Error>> {
            // Report writes during the swap. A previously edited payload
            // is retained but a requested forced replacement still succeeds.
            let changed = !self.intact().unwrap_or(false);
            // The intent owns this recovery entry until it settles. Keep
            // the active flag set even if an external editor removed the index.
            if !pending {
                settle(&self.id, &self.dest, false)?;
            }
            Ok(changed.then(|| self.displaced_error()))
        }

        /// [`finish`](Self::finish) for content sk1ll owned: an entry that
        /// still matches is dropped right away instead of kept for
        /// [`GRACE`]; one written to during the swap is kept, changed.
        pub(super) fn finish_owned(self) -> Result<Option<Error>> {
            if !self.intact().unwrap_or(false) {
                return self.finish(false);
            }
            if self.held == Some(Held::Linked) {
                remove_if_there(&self.content())?;
            }
            if !release(&self.root, &self.id, &self.dir)? {
                return self.finish(false);
            }
            Ok(None)
        }

        /// [`finish`](Self::finish) for a hand edit sk1ll was told to
        /// overwrite: the entry is kept, marked changed, so it is listed as
        /// a conflict until adopted or discarded.
        pub(super) fn finish_kept(self) -> Result<()> {
            settle(&self.id, &self.dest, true)
        }

        fn displaced_error(&self) -> Error {
            Error::Displaced { dest: self.dest.clone(), kept: self.content(), id: self.id.clone() }
        }
    }

    fn rename(from: &Path, to: &Path) -> std::io::Result<()> {
        #[cfg(test)]
        if let Some(e) = forced_failure(to) {
            return Err(e);
        }
        fs::rename(from, to)
    }

    fn hard_link(from: &Path, to: &Path) -> std::io::Result<()> {
        #[cfg(test)]
        if let Some(e) = forced_failure(to) {
            return Err(e);
        }
        fs::hard_link(from, to)
    }

    /// The failure a test asked for: everything across filesystems, or
    /// just what goes into the state directory.
    #[cfg(test)]
    pub(super) fn forced_failure(to: &Path) -> Option<std::io::Error> {
        let elsewhere = STATE_ON_OTHER_FS.get() && root().is_ok_and(|root| to.starts_with(root));
        (FORCE_EXDEV.get() || elsewhere).then(|| std::io::ErrorKind::CrossesDevices.into())
    }

    #[cfg(test)]
    thread_local! {
        static DEFAULT_TEST_ROOT: tempfile::TempDir = tempfile::tempdir().expect("test recovery root");
        static TEST_ROOT: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
        /// Acts as if the state directory were on another filesystem than the harnesses.
        pub(super) static STATE_ON_OTHER_FS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
        /// Acts as if every move or link into an entry crossed filesystems,
        /// which in-directory ones never should.
        pub(super) static FORCE_EXDEV: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }

    /// Puts this test thread's recovery area at `root` until the guard drops.
    #[cfg(test)]
    pub(super) fn test_root(root: &Path) -> impl Drop + use<> {
        struct Guard;
        impl Drop for Guard {
            fn drop(&mut self) {
                TEST_ROOT.with_borrow_mut(|r| *r = None);
                STATE_ON_OTHER_FS.set(false);
                FORCE_EXDEV.set(false);
            }
        }
        TEST_ROOT.with_borrow_mut(|r| *r = Some(root.to_path_buf()));
        Guard
    }

    #[cfg(test)]
    mod tests {
        use super::parse_size;

        #[test]
        fn sizes_take_binary_units() {
            assert_eq!(parse_size("1048576"), Some(1 << 20));
            assert_eq!(parse_size("512M"), Some(512 << 20));
            assert_eq!(parse_size(" 2 GiB "), Some(2 << 30));
            assert_eq!(parse_size("3kb"), Some(3 << 10));
            assert_eq!(parse_size("lots"), None);
            assert_eq!(parse_size("1X"), None);
        }
    }
}

/// Points where a test can step in, to simulate a concurrent harness edit
/// deterministically.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Hook {
    BeforeCopy,
    CopyEntry,
    /// While planning a skill, after observing the destination and before
    /// hashing the source (the slow part with a large skill), or before
    /// reusing the hash an earlier harness of the same plan took.
    PlanSource,
    /// While applying, right before the destination is replaced.
    BeforeSwap,
    /// While replacing a skill or file, after the last check of the
    /// destination and before it is moved or linked into the recovery area.
    AfterCheck,
    /// After the content was moved (or a file hard-linked) into the
    /// recovery area and checked there, before the replacement goes in.
    AfterAside,
    /// Right after the replacement went in (or the removal happened),
    /// before the displaced content is checked one last time.
    AfterSwap,
    AfterRename,
    BeforePrune,
    IndexBeforeMove,
}

#[cfg(test)]
type HookFn = Box<dyn FnMut(Hook, &Path)>;

#[cfg(test)]
thread_local! {
    static HOOK: std::cell::RefCell<Option<HookFn>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn hook(point: Hook, dest: &Path) {
    if let Some(mut f) = HOOK.with_borrow_mut(Option::take) {
        f(point, dest);
        HOOK.with_borrow_mut(|h| *h = Some(f));
    }
}

/// What sk1ll owns in one directory, each entry settled (see [`Index::owned`]).
fn link_identity(path: &Path) -> String {
    format!("link:{path:?}")
}

/// Where the symlink `dest` leads (absolute, as written), and its canonical
/// target. A dangling one (a removed repo skill, say) has the canonical
/// target `recorded` if that names the same place: resolved lexically, so
/// `..` and `.` still match.
fn link_target(dest: &Path, recorded: Option<&Path>) -> Result<(PathBuf, Option<PathBuf>)> {
    let to = fs::read_link(dest).at(dest)?;
    let abs = match dest.parent() {
        Some(parent) if to.is_relative() => parent.join(&to),
        _ => to,
    };
    let canonical = match abs.canonicalize() {
        Ok(path) => Some(path),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            recorded.filter(|p| lexical_normalize(p) == lexical_normalize(&abs)).map(Path::to_path_buf)
        }
        Err(e) => return Err(e).at(&abs),
    };
    Ok((abs, canonical))
}

/// Whether what is at `dest` is the very object `entry` records (see
/// [`IndexEntry`]), looked at without following a symlink there.
fn holds(dest: &Path, entry: &IndexEntry) -> Result<bool> {
    let meta = match fs::symlink_metadata(dest) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e).at(dest),
    };
    Ok(match entry {
        IndexEntry::Link { target, .. } => {
            meta.file_type().is_symlink() && link_target(dest, Some(target))?.1.as_ref() == Some(target)
        }
        IndexEntry::Copy { token, .. } => {
            meta.is_dir()
                && placement_token(dest)?.as_ref() == Some(token)
                && same_anchor(dest.parent().unwrap_or(dest), entry, &dest.join(PLACEMENT_MARKER))?
        }
        IndexEntry::Rule { file, .. } if meta.is_file() => {
            rule_identity(dest)?.as_ref() == Some(file) && same_anchor(dest.parent().unwrap_or(dest), entry, dest)?
        }
        IndexEntry::Rule { .. } => false,
    })
}

fn classify(
    repo_root: &Path,
    dest: &Path,
    assigned_source: Option<&Path>,
    recorded: Option<&IndexEntry>,
) -> Result<Existing> {
    let meta = match fs::symlink_metadata(dest) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Existing::Missing),
        Err(e) => return Err(e).at(dest),
    };
    if meta.file_type().is_symlink() {
        let target = match recorded {
            Some(IndexEntry::Link { target, .. }) => Some(target.as_path()),
            _ => None,
        };
        let (abs, canonical) = link_target(dest, target)?;
        if let Some(canonical) = canonical {
            let owned = target == Some(canonical.as_path());
            // A link the index doesn't own is taken over only if it is
            // exactly the link sync would make for the assigned skill.
            let exact =
                !owned && assigned_source.map(|p| p.canonicalize().at(p)).transpose()?.as_ref() == Some(&canonical);
            if canonical.starts_with(repo_root) && (owned || exact) {
                return Ok(Existing::ManagedLink(canonical));
            }
        }
        return Ok(Existing::Unmanaged(format!("symlink to {} not managed by sk1ll", abs.display())));
    }
    if meta.is_dir() {
        return Ok(match recorded {
            Some(entry @ IndexEntry::Copy { hash, .. }) if holds(dest, entry)? => Existing::ManagedCopy(hash.clone()),
            _ => Existing::Unmanaged("existing directory not managed by sk1ll".into()),
        });
    }
    Ok(Existing::Unmanaged("existing file".into()))
}

/// Normalize an absolute link target without requiring its source to exist.
fn lexical_normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            component => normalized.push(component.as_os_str()),
        }
    }
    normalized
}

fn remove_entry(path: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(path).at(path)?;
    if meta.file_type().is_symlink() {
        // Directory symlinks on Windows need remove_dir.
        fs::remove_file(path).or_else(|_| fs::remove_dir(path)).at(path)
    } else if meta.is_dir() {
        fs::remove_dir_all(path).at(path)
    } else {
        fs::remove_file(path).at(path)
    }
}

fn refuse_source_marker(source: &Path) -> Result<()> {
    let marker = source.join(PLACEMENT_MARKER);
    match marker.symlink_metadata() {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).at(marker),
        Ok(_) => {
            Err(Error::Invalid(format!("{} contains the reserved root file {PLACEMENT_MARKER}", source.display())))
        }
    }
}

/// Reject unsupported entries before creating or replacing any output.
fn validate_copy(src: &Path, exclude: Exclusion) -> Result<()> {
    if !fs::symlink_metadata(src).at(src)?.is_dir() {
        return Err(Error::Invalid(format!("{} is not a skill directory", src.display())));
    }
    let walker = WalkDir::new(src).into_iter().filter_entry(|e| e.depth() == 0 || !exclude(e.depth(), e.file_name()));
    for entry in walker {
        let entry = entry.map_err(std::io::Error::from).at(src)?;
        let ft = entry.file_type();
        if !ft.is_dir() && !ft.is_file() && !(cfg!(unix) && ft.is_symlink()) {
            return Err(Error::Invalid(format!("unsupported skill entry: {}", entry.path().display())));
        }
    }
    Ok(())
}

fn copy_dir(src: &Path, dest: &Path, exclude: Exclusion) -> Result<()> {
    validate_copy(src, exclude)?;
    let walker = WalkDir::new(src).into_iter().filter_entry(|e| e.depth() == 0 || !exclude(e.depth(), e.file_name()));
    for entry in walker {
        let entry = entry.map_err(std::io::Error::from).at(src)?;
        let rel = entry.path().strip_prefix(src).unwrap_or(entry.path());
        let out = dest.join(rel);
        #[cfg(test)]
        hook(Hook::CopyEntry, entry.path());
        if entry.file_type().is_dir() {
            fs::create_dir_all(&out).at(&out)?;
        } else if entry.file_type().is_symlink() {
            #[cfg(unix)]
            std::os::unix::fs::symlink(fs::read_link(entry.path()).at(entry.path())?, &out).at(&out)?;
        } else {
            fs::copy(entry.path(), &out).at(&out)?;
        }
    }
    Ok(())
}

#[cfg(unix)]
fn symlink_dir(src: &Path, dest: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(src, dest)
}

#[cfg(windows)]
fn symlink_dir(src: &Path, dest: &Path) -> std::io::Result<()> {
    std::os::windows::fs::symlink_dir(src, dest)
}

/// The entry sync records for the copy at `dest` as it is now: its
/// placement marker's token and its content hash.
#[cfg(test)]
fn copy_entry(dest: &Path) -> IndexEntry {
    let token = placement_token(dest).unwrap().expect("a placement marker");
    IndexEntry::Copy { token, hash: copy_hash(dest).unwrap(), payload: hash_payload(dest).unwrap() }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn skill_setup(mode: LinkMode) -> (tempfile::TempDir, Repo, BTreeMap<String, Harness>) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("repo");
        write(&root.join("sk1ll.toml"), "");
        write(&root.join("personal/pdf/SKILL.md"), "PDFs.\n");
        let repo = Repo::open(&root).unwrap();
        repo.assign_missing_ids().unwrap();
        let harness = Harness {
            skills: Some(tmp.path().join("skills").to_string_lossy().into_owned()),
            mode,
            ..Harness::default()
        };
        (tmp, repo, BTreeMap::from([("h".to_string(), harness)]))
    }

    #[test]
    fn ownership_index_round_trips_each_kind() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("repo/personal/pdf");
        let file = FileId { dev: 1, ino: 2 };
        let copy = IndexEntry::Copy { token: "t1".into(), hash: "sha256:1".into(), payload: "payload".into() };
        let mut index = Index::default();
        index.set(
            "link",
            Some(IndexEntry::Link { hash: link_identity(&target), target, payload: "payload".into() }),
            None,
        );
        index.set("copy", Some(copy.clone()), Some(Intent::Place { next: copy.clone(), recovery: None }));
        index.set(
            "rule.md",
            Some(IndexEntry::Rule { file, hash: "sha256:2".into(), payload: "payload".into() }),
            Some(Intent::Remove { recovery: None }),
        );
        assert!(load_index(tmp.path()).unwrap().is_empty());
        save_index(tmp.path(), &index).unwrap();
        assert_eq!(load_index(tmp.path()).unwrap(), index);
        let bytes = fs::read(tmp.path().join(INDEX_PATH)).unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["version"], INDEX_VERSION);
        let entries = &json["entries"];
        assert_eq!(entries["link"]["kind"], "link");
        assert_eq!(
            entries["copy"],
            serde_json::json!({"kind": "copy", "token": "t1", "hash": "sha256:1", "payload": "payload"})
        );
        assert_eq!(
            entries["rule.md"],
            serde_json::json!({"kind": "rule", "file": {"dev": 1, "ino": 2}, "hash": "sha256:2", "payload": "payload"})
        );
        assert_eq!(json["pending"]["copy"]["op"], "place");
        assert_eq!(json["pending"]["copy"]["next"], entries["copy"]);
        assert_eq!(json["pending"]["rule.md"], serde_json::json!({"op": "remove", "recovery": null}));

        // Another format is refused, never read as this one.
        let mut other = json.clone();
        other["version"] = serde_json::json!(INDEX_VERSION + 1);
        write(&tmp.path().join(INDEX_PATH), &other.to_string());
        assert!(matches!(load_index(tmp.path()), Err(Error::Parse { .. })));
        write(&tmp.path().join(INDEX_PATH), r#"{"entries": {}}"#);
        assert!(matches!(load_index(tmp.path()), Err(Error::Parse { .. })));
    }

    #[cfg(unix)]
    #[test]
    fn bookkeeping_writes_never_follow_a_planted_temp_name() {
        let tmp = tempfile::tempdir().unwrap();
        let victim = tmp.path().join("victim");
        write(&victim, "victim\n");
        let path = tmp.path().join(INDEX_PATH);
        fs::create_dir(path.parent().unwrap()).unwrap();
        let planted = sibling(&path, "sk1ll-tmp");
        std::os::unix::fs::symlink(&victim, &planted).unwrap();
        let mut index = Index::default();
        let file = FileId { dev: 1, ino: 2 };
        index.set("git.md", Some(IndexEntry::Rule { file, hash: "sha256:1".into(), payload: "payload".into() }), None);
        save_index(tmp.path(), &index).unwrap();
        assert_eq!(load_index(tmp.path()).unwrap(), index);
        assert!(fs::symlink_metadata(path).unwrap().file_type().is_file());
        assert_eq!(fs::read_to_string(&victim).unwrap(), "victim\n");
        assert!(fs::symlink_metadata(&planted).unwrap().file_type().is_symlink());
    }

    /// A cached plan reads no unchanged tree, and sees a change to any. A
    /// stale cache entry can only make a plan stale: `apply` checks the
    /// destination itself and keeps an edit the plan missed.
    #[test]
    fn a_cached_plan_rereads_only_changed_trees_and_apply_checks_the_real_one() {
        let (tmp, repo, harnesses) = skill_setup(LinkMode::Copy);
        let mut assignment = Assignment::default();
        apply(&repo, &plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap()[0]).unwrap();
        let cache = HashCache::with_racy_window(Duration::ZERO);
        let cached = |assignment: &Assignment| {
            plan_cached(&repo, &harnesses, assignment, PlanOptions::default(), &cache).unwrap()
        };
        assert!(cached(&assignment).is_empty());
        let read = cache.hashed();
        assert_eq!(read, 2, "the source and the placement");
        assert!(cached(&assignment).is_empty());
        assert_eq!(cache.hashed(), read, "nothing changed: nothing read");

        let dest = tmp.path().join("skills/pdf");
        let placed = hash_payload(&dest).unwrap();
        std::thread::sleep(Duration::from_millis(30));
        write(&dest.join("SKILL.md"), "Edited in the harness.\n");
        let actions = cached(&assignment);
        assert_eq!(cache.hashed(), read + 1);
        assert_eq!(actions.iter().map(|a| a.kind).collect::<Vec<_>>(), [ActionKind::Conflict]);

        // A stale entry says the edited copy is still as placed: turned
        // off, the plan removes it, and `apply` refuses.
        cache.remember_payload(&dest, &placed);
        let id = repo.skills().unwrap()[0].id.clone().unwrap();
        assignment.set_skill("h", &id, false);
        let actions = cached(&assignment);
        assert_eq!(actions.iter().map(|a| a.kind).collect::<Vec<_>>(), [ActionKind::Remove]);
        assert!(apply(&repo, &actions[0]).is_err());
        assert_eq!(fs::read_to_string(dest.join("SKILL.md")).unwrap(), "Edited in the harness.\n");
        let fresh = plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap();
        assert_eq!(fresh.iter().map(|a| a.kind).collect::<Vec<_>>(), [ActionKind::Conflict]);
    }

    /// The harness fingerprint changes with what planning reads in it, and
    /// not with unmanaged content below the skills directory's own entries.
    #[test]
    fn the_harness_fingerprint_follows_what_planning_reads() {
        let (tmp, repo, harnesses) = skill_setup(LinkMode::Copy);
        let harness = &harnesses["h"];
        let before = harness_fingerprint(harness).unwrap();
        assert_eq!(harness_fingerprint(harness).unwrap(), before);
        apply(&repo, &plan(&repo, &harnesses, &Assignment::default(), PlanOptions::default()).unwrap()[0]).unwrap();
        let placed = harness_fingerprint(harness).unwrap();
        assert_ne!(placed, before);
        write(&tmp.path().join("skills/mine/SKILL.md"), "Mine.\n");
        let unmanaged = harness_fingerprint(harness).unwrap();
        assert_ne!(unmanaged, placed, "a new entry of its own");
        write(&tmp.path().join("skills/mine/SKILL.md"), "Edited, unmanaged.\n");
        assert_eq!(harness_fingerprint(harness).unwrap(), unmanaged, "not looked into");
        std::thread::sleep(Duration::from_millis(30));
        write(&tmp.path().join("skills/pdf/SKILL.md"), "Edited, managed.\n");
        assert_ne!(harness_fingerprint(harness).unwrap(), unmanaged);
    }

    /// What planning reads through a symlink changes the fingerprint: a
    /// skills directory that is a link (its target's own entries), an
    /// instructions file that is one (its target), and so does the recovery
    /// area beside a block instructions file and its target, and the
    /// harness's own settings. A fresh change leaves it unsettled.
    #[cfg(unix)]
    #[test]
    fn the_harness_fingerprint_reads_through_links_and_covers_settings_and_recovery() {
        use std::os::unix::fs::symlink;
        let (tmp, _repo, harnesses) = skill_setup(LinkMode::Copy);
        let dotfiles = tmp.path().join("dotfiles");
        fs::create_dir_all(dotfiles.join("skills")).unwrap();
        symlink(dotfiles.join("skills"), tmp.path().join("skills")).unwrap();
        write(&dotfiles.join("AGENTS.md"), "Mine.\n");
        let codex = tmp.path().join("codex");
        fs::create_dir_all(&codex).unwrap();
        symlink(dotfiles.join("AGENTS.md"), codex.join("AGENTS.md")).unwrap();
        let mut harness = harnesses["h"].clone();
        harness.instructions = Some(codex.join("AGENTS.md").to_string_lossy().into_owned());
        let mut last = harness_fingerprint(&harness).unwrap();
        type Change<'a> = Box<dyn Fn(&mut Harness) + 'a>;
        let changes: [(&str, Change); 5] = [
            ("an entry of the linked skills directory", Box::new(|_| write(&dotfiles.join("skills/mine/x"), "x"))),
            ("the linked instructions file's target", Box::new(|_| write(&dotfiles.join("AGENTS.md"), "Edited.\n"))),
            ("recovery beside the link", Box::new(|_| write(&codex.join(".sk1ll-tmp/displaced/a/content"), "a"))),
            ("recovery beside the target", Box::new(|_| write(&dotfiles.join(".sk1ll-tmp/displaced/b/content"), "b"))),
            ("the placement mode", Box::new(|h| h.mode = LinkMode::Symlink)),
        ];
        for (what, change) in changes {
            std::thread::sleep(Duration::from_millis(30));
            change(&mut harness);
            let now = harness_fingerprint(&harness).unwrap();
            assert_ne!(now, last, "{what}");
            assert!(!now.settled(std::time::SystemTime::now(), Duration::from_secs(3600)), "{what}: just changed");
            last = now;
        }
        assert!(last.settled(std::time::SystemTime::now(), Duration::ZERO));
        assert_eq!(harness_fingerprint(&harness).unwrap(), last);
    }

    /// The recovery area's stamp covers each entry's payload where it is,
    /// beside the instructions file it came from, not only the index.
    #[test]
    fn the_recovery_stamp_covers_payloads_beside_instructions() {
        let d = Displacement::new("block update");
        let _root = displaced::test_root(&d.tmp.path().join("state/displaced"));
        // Only a block edited by hand leaves an entry when it is rewritten (forced).
        let text = fs::read_to_string(&d.dest).unwrap();
        write(&d.dest, &text.replace("Use CC.", "Use CC by hand."));
        let forced = plan(&d.repo, &d.harnesses, &d.assignment, PlanOptions { force: true }).unwrap();
        apply(&d.repo, &forced[0]).unwrap();
        let entry = d.entry();
        assert!(entry.path.starts_with(d.area()));
        let before = displaced::stamp().unwrap();
        assert_eq!(displaced::stamp().unwrap(), before);
        std::thread::sleep(Duration::from_millis(30));
        fs::write(d.kept_file(&entry), "Written to the recovery copy.\n").unwrap();
        assert_ne!(displaced::stamp().unwrap(), before);
    }

    #[test]
    fn a_hand_edit_conflicts_even_when_it_matches_the_source() {
        let (tmp, repo, harnesses) = skill_setup(LinkMode::Copy);
        let assignment = Assignment::default();
        apply(&repo, &plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap()[0]).unwrap();
        let dest = tmp.path().join("skills/pdf/SKILL.md");
        write(&dest, "Edited in the harness.\n");
        for source in ["PDFs.\n", "Edited in the harness.\n"] {
            write(&repo.personal_dir().join("pdf/SKILL.md"), source);
            let actions = plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap();
            assert_eq!(actions.len(), 1);
            assert_eq!(actions[0].kind, ActionKind::Conflict);
            apply(&repo, &actions[0]).unwrap();
            assert_eq!(fs::read_to_string(&dest).unwrap(), "Edited in the harness.\n");
        }
    }

    #[test]
    fn an_unindexed_copy_is_unmanaged_and_never_removed() {
        let (tmp, repo, harnesses) = skill_setup(LinkMode::Copy);
        let mut assignment = Assignment::default();
        write(&repo.personal_dir().join("notes/SKILL.md"), "Notes.\n");
        for action in plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap() {
            apply(&repo, &action).unwrap();
        }
        let dir = tmp.path().join("skills");
        let mut index = load_index(&dir).unwrap();
        assert!(index.entries.remove("pdf").is_some());
        assert!(index.entries.contains_key("notes"));
        save_index(&dir, &index).unwrap();
        assert!(unmanaged::discover(&repo, &harnesses).unwrap().is_empty());
        let blocked = unmanaged::select(&repo, &harnesses, "h", "pdf").unwrap_err();
        assert!(blocked.to_string().contains(PLACEMENT_MARKER));
        let placements = placed(&repo, "h", &harnesses["h"]).unwrap();
        assert_eq!(placements.len(), 1);
        assert_eq!(placements[0].dest, dir.join("notes"));
        let opts = PlanOptions { force: true };
        let actions = plan(&repo, &harnesses, &assignment, opts).unwrap();
        assert_eq!(actions[0].kind, ActionKind::Conflict);
        assignment.set_skill("h", &skill_id(&repo, "pdf"), false);
        assert!(plan(&repo, &harnesses, &assignment, opts).unwrap().is_empty());
        assert_eq!(fs::read_to_string(dir.join("pdf/SKILL.md")).unwrap(), "PDFs.\n");
    }

    #[cfg(unix)]
    #[test]
    fn an_unindexed_link_is_unmanaged_until_sync_records_it() {
        let (tmp, repo, harnesses) = skill_setup(LinkMode::Symlink);
        let dir = tmp.path().join("skills");
        let dest = dir.join("pdf");
        fs::create_dir(&dir).unwrap();
        std::os::unix::fs::symlink(repo.personal_dir().join("pdf"), &dest).unwrap();
        assert_eq!(unmanaged::discover(&repo, &harnesses).unwrap()[0].name, "pdf");
        assert!(placed(&repo, "h", &harnesses["h"]).unwrap().is_empty());
        let actions = plan(&repo, &harnesses, &Assignment::default(), PlanOptions::default()).unwrap();
        assert_eq!(actions[0].kind, ActionKind::Update);
        apply(&repo, &actions[0]).unwrap();
        assert!(unmanaged::discover(&repo, &harnesses).unwrap().is_empty());
        assert!(matches!(index_entry(&dest).unwrap(), Some(IndexEntry::Link { .. })));
    }

    #[test]
    fn stale_index_entries_refuse_skill_and_rule_updates() {
        for case in ["skill update", "skill removal", "rule update", "rule removal"] {
            for point in [None, Some(Hook::BeforeSwap), Some(Hook::AfterAside)] {
                let d = Displacement::new(case);
                let _root = displaced::test_root(&d.tmp.path().join("recovery"));
                let action = d.action();
                let before = fs::read(&d.file).unwrap();
                let holder = d.holder.clone();
                let name = d.dest.file_name().unwrap().to_string_lossy().into_owned();
                let mut index = load_index(&holder).unwrap();
                assert!(index.entries.remove(&name).is_some());
                let edit = move |_: &Path| save_index(&holder, &index).unwrap();
                let result = match point {
                    Some(point) => with_hook(point, edit, || apply(&d.repo, &action)),
                    None => {
                        edit(&d.dest);
                        apply(&d.repo, &action)
                    }
                };
                assert_eq!(
                    result.unwrap_err().to_string(),
                    changed_since_plan(&d.dest).to_string(),
                    "{case} {point:?}"
                );
                assert_eq!(fs::read(&d.file).unwrap(), before);
                assert_eq!(index_entry(&d.dest).unwrap(), None);
                assert!(displaced::list().unwrap().is_empty());
            }
        }
    }

    #[test]
    fn a_partially_written_index_invalidates_the_plan() {
        let d = Displacement::new("skill update");
        let action = d.action();
        write(&d.holder.join(INDEX_PATH), "{");
        assert_eq!(apply(&d.repo, &action).unwrap_err().to_string(), changed_since_plan(&d.dest).to_string());
        assert_eq!(fs::read_to_string(&d.file).unwrap(), "PDFs.\n");
        assert_eq!(fs::read_to_string(d.holder.join(INDEX_PATH)).unwrap(), "{");
    }

    /// Stop after the selected hook, before any caller can settle the intent.
    fn interrupt(repo: &Repo, action: &Action, point: Hook) {
        let failure = std::panic::catch_unwind(|| {
            with_hook(point, |_| std::panic::panic_any("interrupted sync"), || apply(repo, action))
        })
        .expect_err("sync reached the interruption point");
        assert_eq!(failure.downcast_ref::<&str>(), Some(&"interrupted sync"));
    }

    #[cfg(unix)]
    #[test]
    fn restart_finishes_the_first_copy_an_update_and_a_copy_to_link() {
        let (tmp, repo, mut harnesses) = skill_setup(LinkMode::Copy);
        let _root = displaced::test_root(&tmp.path().join("recovery"));
        let dir = tmp.path().join("skills");
        let src = repo.personal_dir().join("pdf");
        let dest = dir.join("pdf");
        let assignment = Assignment::default();
        for (kind, mode, text) in [
            (ActionKind::Create, LinkMode::Copy, "First.\n"),
            (ActionKind::Update, LinkMode::Copy, "Updated.\n"),
            (ActionKind::Update, LinkMode::Symlink, "Linked.\n"),
        ] {
            harnesses.get_mut("h").unwrap().mode = mode;
            write(&src.join("SKILL.md"), text);
            let plan_now = || plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap();
            let actions = plan_now();
            assert_eq!(actions.len(), 1);
            assert_eq!(actions[0].kind, kind);
            interrupt(&repo, &actions[0], Hook::AfterSwap);
            assert!(load_index(&dir).unwrap().pending.contains_key("pdf"));
            assert_eq!(fs::read_to_string(dest.join("SKILL.md")).unwrap(), text);
            let before = fs::read(dir.join(INDEX_PATH)).unwrap();
            assert!(plan_now().is_empty());
            assert!(unmanaged::discover(&repo, &harnesses).unwrap().is_empty());
            assert_eq!(fs::read(dir.join(INDEX_PATH)).unwrap(), before, "preview is read-only");
            recover(None, &harnesses).unwrap();
            let index = load_index(&dir).unwrap();
            assert!(index.pending.is_empty());
            match mode {
                LinkMode::Copy => assert_eq!(index.entries["pdf"], copy_entry(&dest)),
                LinkMode::Symlink => {
                    assert_eq!(fs::read_link(&dest).unwrap(), src);
                    assert!(matches!(&index.entries["pdf"], IndexEntry::Link { target, .. } if target == &src));
                }
            }
            assert!(plan_now().is_empty());
        }
    }

    #[test]
    fn restart_resolves_a_rule_placement_before_cleaning_stages() {
        let (_tmp, repo, harnesses, dir) = instructions_setup(InstructionsFormat::Rules);
        let assignment = Assignment::default();
        let plan_now = || plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap();
        for point in [Hook::AfterSwap, Hook::BeforeSwap, Hook::AfterSwap] {
            if point == Hook::BeforeSwap {
                write(&repo.root().join("instructions/git.md"), "New rule.\n");
            }
            interrupt(&repo, &plan_now()[0], point);
            let index = load_index(&dir).unwrap();
            assert!(index.pending.contains_key("git.md"));
            let expected = if point == Hook::BeforeSwap {
                assert!(dir.join(".sk1ll-state/staging/stage-git.md").exists());
                assert_eq!(plan_now()[0].kind, ActionKind::Update);
                index.entries["git.md"].clone()
            } else {
                assert!(plan_now().is_empty());
                let Intent::Place { next, .. } = &index.pending["git.md"] else {
                    panic!("expected a placement intent")
                };
                next.clone()
            };
            recover(None, &harnesses).unwrap();
            let index = load_index(&dir).unwrap();
            assert!(index.pending.is_empty());
            assert_eq!(index.entries["git.md"], expected);
            assert!(!dir.join(".sk1ll-state/staging/stage-git.md").exists());
        }
    }

    #[test]
    fn a_copy_replaced_between_syncs_or_after_an_interrupted_removal_stays_unmanaged() {
        for interrupted in [false, true] {
            for newcomer in ["PDFs.\n", "My skill.\n"] {
                let (tmp, repo, harnesses) = skill_setup(LinkMode::Copy);
                let _root = displaced::test_root(&tmp.path().join("recovery"));
                let mut assignment = Assignment::default();
                let initial = plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap();
                apply(&repo, &initial[0]).unwrap();
                let dest = &initial[0].dest;
                let dir = dest.parent().unwrap();
                assignment.set_skill("h", &skill_id(&repo, "pdf"), false);
                let removal = plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap();
                assert_eq!(removal[0].kind, ActionKind::Remove);
                if interrupted {
                    interrupt(&repo, &removal[0], Hook::AfterSwap);
                    assert!(matches!(
                        load_index(dir).unwrap().pending.get("pdf"),
                        Some(Intent::Remove { recovery: Some(_) })
                    ));
                } else {
                    fs::rename(dest, tmp.path().join("moved")).unwrap();
                }
                write(&dest.join("SKILL.md"), newcomer);
                assert!(apply(&repo, &removal[0]).is_err(), "the old action is stale");
                let off = plan(&repo, &harnesses, &assignment, PlanOptions { force: true }).unwrap();
                assert!(off.is_empty(), "{off:?}");
                assert_eq!(unmanaged::discover(&repo, &harnesses).unwrap()[0].name, "pdf");
                recover(None, &harnesses).unwrap();
                if interrupted {
                    assert!(load_index(dir).unwrap().is_empty());
                    assert_eq!(displaced::list().unwrap().len(), 1, "only the original was displaced");
                }
                assignment.set_skill("h", &skill_id(&repo, "pdf"), true);
                for force in [false, true] {
                    let actions = plan(&repo, &harnesses, &assignment, PlanOptions { force }).unwrap();
                    assert_eq!(actions.len(), 1);
                    assert_eq!(actions[0].kind, ActionKind::Conflict);
                    apply(&repo, &actions[0]).unwrap();
                }
                displaced::prune_older_than(Duration::ZERO).unwrap();
                assert_eq!(fs::read_to_string(dest.join("SKILL.md")).unwrap(), newcomer);
                assert!(!dest.join(PLACEMENT_MARKER).exists());
            }
        }
    }

    #[test]
    fn a_rule_replaced_by_another_file_with_identical_text_is_unmanaged() {
        let (_tmp, repo, harnesses, dir) = instructions_setup(InstructionsFormat::Rules);
        let mut assignment = Assignment::default();
        let initial = plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap();
        apply(&repo, &initial[0]).unwrap();
        let dest = &initial[0].dest;
        let replacement = dir.join("new");
        fs::copy(dest, &replacement).unwrap();
        fs::rename(&replacement, dest).unwrap();
        let opts = PlanOptions { force: true };
        let conflict = plan(&repo, &harnesses, &assignment, opts).unwrap();
        assert_eq!(conflict[0].kind, ActionKind::Conflict);
        assignment.set_rule("h", &rule_id(&repo, "git"), false);
        assert!(plan(&repo, &harnesses, &assignment, opts).unwrap().is_empty());
        assert_eq!(fs::read_to_string(dest).unwrap(), "Use CC.\n");
    }

    #[test]
    fn a_skill_copies_nested_state_and_detects_edits_to_it() {
        let (_tmp, repo, harnesses) = skill_setup(LinkMode::Copy);
        let assignment = Assignment::default();
        let path = "x/.sk1ll-state/f";
        write(&repo.personal_dir().join("pdf").join(path), "User content.\n");
        let initial = plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap();
        apply(&repo, &initial[0]).unwrap();
        let file = initial[0].dest.join(path);
        assert_eq!(fs::read_to_string(&file).unwrap(), "User content.\n");
        write(&file, "Edited by hand.\n");
        let actions = plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap();
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].kind, ActionKind::Conflict);
    }

    #[test]
    fn source_placement_marker_is_a_conflict_even_with_force() {
        let (_tmp, repo, harnesses) = skill_setup(LinkMode::Copy);
        let marker = repo.personal_dir().join("pdf").join(PLACEMENT_MARKER);
        let assignment = Assignment::default();
        let opts = PlanOptions { force: true };
        let planned = plan(&repo, &harnesses, &assignment, opts).unwrap();
        write(&marker, "user content\n");
        let error = apply(&repo, &planned[0]).unwrap_err();
        assert!(error.to_string().contains(PLACEMENT_MARKER));
        let conflicts = plan(&repo, &harnesses, &assignment, opts).unwrap();
        assert_eq!(conflicts[0].kind, ActionKind::Conflict);
        assert!(conflicts[0].reason.as_ref().unwrap().contains(PLACEMENT_MARKER));
        assert!(!conflicts[0].dest.exists());
        assert_eq!(fs::read_to_string(marker).unwrap(), "user content\n");
    }

    #[test]
    fn a_clone_with_the_same_marker_bytes_stays_unmanaged() {
        for interrupted in [false, true] {
            let (tmp, repo, harnesses) = skill_setup(LinkMode::Copy);
            let mut assignment = Assignment::default();
            let initial = plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap();
            if interrupted {
                interrupt(&repo, &initial[0], Hook::AfterSwap);
            } else {
                apply(&repo, &initial[0]).unwrap();
            }
            let dest = &initial[0].dest;
            let original = tmp.path().join("original");
            fs::rename(dest, &original).unwrap();
            copy_dir(&original, dest, excluded_from_personal_skill).unwrap();
            fs::copy(original.join(PLACEMENT_MARKER), dest.join(PLACEMENT_MARKER)).unwrap();
            assert_eq!(
                fs::read(original.join(PLACEMENT_MARKER)).unwrap(),
                fs::read(dest.join(PLACEMENT_MARKER)).unwrap()
            );
            assert_eq!(hash_dir(&original).unwrap(), hash_dir(dest).unwrap());
            let view =
                |assignment: &Assignment, force| plan(&repo, &harnesses, assignment, PlanOptions { force }).unwrap();
            for force in [false, true] {
                assert_eq!(view(&assignment, force)[0].kind, ActionKind::Conflict);
            }
            assignment.set_skill("h", &skill_id(&repo, "pdf"), false);
            assert!(view(&assignment, true).is_empty());
            recover(None, &harnesses).unwrap();
            assert!(view(&assignment, true).is_empty());
            let error = unmanaged::select(&repo, &harnesses, "h", "pdf").unwrap_err();
            assert!(error.to_string().contains(PLACEMENT_MARKER), "{error}");
            assert!(dest.join("SKILL.md").is_file());
        }
    }

    #[test]
    fn rule_anchor_keeps_the_old_inode_alive_after_the_live_name_is_removed() {
        let (_tmp, repo, harnesses, dir) = instructions_setup(InstructionsFormat::Rules);
        let assignment = Assignment::default();
        let actions = plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap();
        let dest = &actions[0].dest;
        apply(&repo, &actions[0]).unwrap();
        let identity = rule_identity(dest).unwrap().unwrap();
        let anchor = dir.join(INDEX_DIR).join("anchors").join(format!("rule-{}-{}", identity.dev, identity.ino));
        fs::remove_file(dest).unwrap();
        assert_eq!(rule_identity(&anchor).unwrap(), Some(identity));
        write(dest, "Use CC.\n");
        assert_ne!(rule_identity(dest).unwrap(), Some(identity));
        assert_eq!(
            plan(&repo, &harnesses, &assignment, PlanOptions { force: true }).unwrap()[0].kind,
            ActionKind::Conflict
        );
    }

    #[test]
    fn an_atomic_rule_save_after_detach_is_not_overwritten() {
        let save = |dest: &Path| {
            let edit = dest.with_extension("editor");
            write(&edit, "Editor save.\n");
            fs::rename(&edit, dest).unwrap();
        };
        for point in [Hook::AfterCheck, Hook::AfterAside] {
            let d = Displacement::new("rule update");
            let _root = displaced::test_root(&d.tmp.path().join("recovery"));
            let action = d.action();
            let result = with_hook(point, save, || apply(&d.repo, &action));
            assert_eq!(fs::read_to_string(&d.dest).unwrap(), "Editor save.\n");
            if point == Hook::AfterAside {
                let entry = reported(result);
                assert_eq!(fs::read_to_string(entry.path).unwrap(), "Use CC.\n");
            } else {
                assert_eq!(result.unwrap_err().to_string(), changed_since_plan(&d.dest).to_string());
                assert!(displaced::list().unwrap().is_empty());
            }
        }
    }

    #[test]
    fn pruning_rechecks_queued_entries_and_includes_git_and_modes() {
        use std::io::Write;
        for case in ["queued write", "git metadata", "empty directory", "mode"] {
            let d = Displacement::new("skill update");
            let _root = displaced::test_root(&d.tmp.path().join("recovery"));
            let mut open = fs::OpenOptions::new().append(true).open(&d.file).unwrap();
            apply(&d.repo, &d.action()).unwrap();
            let entry = d.entry();
            assert!(!entry.changed);
            match case {
                "queued write" => {
                    let removed = with_hook(
                        Hook::BeforePrune,
                        move |_| open.write_all(b"Late edit.\n").unwrap(),
                        || displaced::prune_with(displaced::GRACE, 0),
                    )
                    .unwrap();
                    assert!(removed.is_empty());
                    assert!(fs::read_to_string(entry.path.join("SKILL.md")).unwrap().ends_with("Late edit.\n"));
                }
                "git metadata" => write(&entry.path.join(".git/notes"), "Git metadata"),
                "empty directory" => fs::create_dir(entry.path.join("empty")).unwrap(),
                "mode" => {
                    let path = entry.path.join("SKILL.md");
                    let mut permissions = path.metadata().unwrap().permissions();
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        permissions.set_mode(permissions.mode() ^ 0o100);
                    }
                    #[cfg(not(unix))]
                    permissions.set_readonly(true);
                    fs::set_permissions(path, permissions).unwrap();
                }
                _ => unreachable!(),
            }
            assert!(displaced::get(&entry.id).unwrap().changed, "{case}");
            assert!(displaced::prune_older_than(Duration::ZERO).unwrap().is_empty(), "{case}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn edits_since_placement_conflict_and_forced_swaps_keep_the_backup() {
        use std::os::unix::fs::PermissionsExt;
        for case in ["git init", "mode", "empty directory"] {
            let d = Displacement::new("skill update");
            let _root = displaced::test_root(&d.tmp.path().join("recovery"));
            match case {
                "git init" => {
                    crate::git::Git::new(&d.dest).args(["init", "-q"]).run().unwrap();
                }
                "mode" => {
                    let mode = fs::metadata(&d.file).unwrap().permissions().mode();
                    fs::set_permissions(&d.file, fs::Permissions::from_mode(mode ^ 0o100)).unwrap();
                }
                "empty directory" => fs::create_dir(d.dest.join("empty")).unwrap(),
                _ => unreachable!(),
            }
            let edited = hash_payload(&d.dest).unwrap();
            let normal = plan(&d.repo, &d.harnesses, &d.assignment, PlanOptions::default()).unwrap();
            assert_eq!(normal[0].kind, ActionKind::Conflict, "{case}");
            let forced = plan(&d.repo, &d.harnesses, &d.assignment, PlanOptions { force: true }).unwrap();
            apply(&d.repo, &forced[0]).unwrap();
            let entry = d.entry();
            assert!(entry.changed, "{case}");
            assert_eq!(hash_payload(&entry.path).unwrap(), edited);
            assert!(displaced::prune_older_than(Duration::ZERO).unwrap().is_empty(), "{case}");
        }
    }

    #[test]
    fn a_write_to_an_early_file_during_the_final_prune_scan_keeps_the_entry() {
        use std::io::Write;
        let (tmp, repo, harnesses) = skill_setup(LinkMode::Copy);
        let _root = displaced::test_root(&tmp.path().join("recovery"));
        let source = repo.personal_dir().join("pdf");
        write(&source.join("a-first"), "old");
        fs::write(source.join("z-large"), vec![b'x'; 1 << 20]).unwrap();
        let assignment = Assignment::default();
        let actions = || plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap();
        apply(&repo, &actions()[0]).unwrap();
        let dest = tmp.path().join("skills/pdf");
        let open = fs::OpenOptions::new().write(true).open(dest.join("a-first")).unwrap();
        write(&source.join("SKILL.md"), "Updated.\n");
        apply(&repo, &actions()[0]).unwrap();
        let entry = displaced::list().unwrap().remove(0);
        assert!(!entry.changed);
        let later = entry.path.join("z-large");
        let checking = std::rc::Rc::new(std::cell::Cell::new(false));
        let at_deletion_check = checking.clone();
        let mut open = Some(open);
        let removed = crate::hash::tests::with_read_hook(
            move |path| {
                if checking.get()
                    && path == later
                    && let Some(mut file) = open.take()
                {
                    file.write_all(b"new").unwrap();
                }
            },
            || {
                with_hook(
                    Hook::BeforePrune,
                    move |_| at_deletion_check.set(true),
                    || displaced::prune_with(displaced::GRACE, 0),
                )
            },
        )
        .unwrap();
        assert!(removed.is_empty());
        assert_eq!(fs::read_to_string(entry.path.join("a-first")).unwrap(), "new");
        assert!(displaced::get(&entry.id).unwrap().changed);
    }

    #[test]
    fn a_fresh_process_crash_after_the_backup_rename_preserves_both_saves() {
        const CHILD: &str = "SK1LL_BACKUP_RENAME_CRASH_TEST";
        let harnesses = |dir: &Path| {
            BTreeMap::from([(
                "h".into(),
                Harness {
                    instructions: Some(dir.join("rules").to_string_lossy().into_owned()),
                    instructions_format: InstructionsFormat::Rules,
                    ..Harness::default()
                },
            )])
        };
        if let Some(dir) = std::env::var_os(CHILD).map(PathBuf::from) {
            let _root = displaced::test_root(&dir.join("recovery"));
            let repo = Repo::open(dir.join("repo")).unwrap();
            let actions = plan(&repo, &harnesses(&dir), &Assignment::default(), PlanOptions::default()).unwrap();
            with_hook(Hook::AfterRename, |_| std::process::exit(78), || apply(&repo, &actions[0])).unwrap();
            panic!("the child reached the end of the swap");
        }

        let (tmp, repo, harnesses, dir) = instructions_setup(InstructionsFormat::Rules);
        let _root = displaced::test_root(&tmp.path().join("recovery"));
        let assignment = Assignment::default();
        apply(&repo, &plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap()[0]).unwrap();
        write(&repo.root().join("instructions/git.md"), "Updated.\n");
        #[cfg(target_os = "linux")]
        let executable = PathBuf::from("/proc/self/exe");
        #[cfg(not(target_os = "linux"))]
        let executable = std::env::current_exe().unwrap();
        let status = std::process::Command::new(executable)
            .args(["--exact", "sync::tests::a_fresh_process_crash_after_the_backup_rename_preserves_both_saves"])
            .env(CHILD, tmp.path())
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(78));
        let live = dir.join("git.md");
        assert_eq!(live.symlink_metadata().unwrap_err().kind(), std::io::ErrorKind::NotFound);
        let backup = displaced::list().unwrap().remove(0);
        write(&live, "Editor saved to live name.\n");
        write(&backup.path, "Editor saved to recovery.\n");
        assert!(displaced::prune_older_than(Duration::ZERO).unwrap().is_empty());
        recover(None, &harnesses).unwrap();
        assert!(displaced::get(&backup.id).unwrap().changed);
        assert!(displaced::prune_older_than(Duration::ZERO).unwrap().is_empty());
        assert_eq!(fs::read_to_string(&live).unwrap(), "Editor saved to live name.\n");
        assert_eq!(fs::read_to_string(&backup.path).unwrap(), "Editor saved to recovery.\n");
    }

    #[cfg(unix)]
    #[test]
    fn recovery_refuses_a_symlinked_scratch_root() {
        let (tmp, repo, harnesses) = skill_setup(LinkMode::Copy);
        let assignment = Assignment::default();
        let actions = plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap();
        apply(&repo, &actions[0]).unwrap();
        let dir = actions[0].dest.parent().unwrap();
        let outside = tmp.path().join("outside");
        let victim = outside.join("stage-pdf/data");
        write(&victim, "Keep this.\n");
        for scratch in [dir.join(TMP_DIR), dir.join(INDEX_DIR).join("staging")] {
            if scratch.is_dir() {
                fs::remove_dir(&scratch).unwrap();
            }
            std::os::unix::fs::symlink(&outside, &scratch).unwrap();
            assert!(recover(None, &harnesses).is_err());
            assert_eq!(fs::read_to_string(&victim).unwrap(), "Keep this.\n");
            fs::remove_file(scratch).unwrap();
        }
    }

    #[test]
    fn a_forced_swap_interrupted_after_detach_preserves_the_edited_backup() {
        let d = Displacement::new("skill update");
        let _root = displaced::test_root(&d.tmp.path().join("recovery"));
        write(&d.file, "Hand-edited original.\n");
        let actions = plan(&d.repo, &d.harnesses, &d.assignment, PlanOptions { force: true }).unwrap();
        let stopped = std::panic::catch_unwind(|| {
            with_hook(
                Hook::AfterAside,
                |dest| {
                    write(&dest.join("SKILL.md"), "Newcomer.\n");
                    std::panic::panic_any("interrupted sync");
                },
                || apply(&d.repo, &actions[0]),
            )
        })
        .expect_err("interrupted before put_back");
        assert_eq!(stopped.downcast_ref::<&str>(), Some(&"interrupted sync"));
        let entry = d.entry();
        assert!(displaced::prune_older_than(Duration::ZERO).unwrap().is_empty());
        recover(None, &d.harnesses).unwrap();
        assert!(displaced::get(&entry.id).unwrap().changed);
        assert!(displaced::prune_older_than(Duration::ZERO).unwrap().is_empty());
        assert_eq!(fs::read_to_string(entry.path.join("SKILL.md")).unwrap(), "Hand-edited original.\n");
        assert_eq!(fs::read_to_string(&d.file).unwrap(), "Newcomer.\n");
    }

    #[test]
    fn writers_from_independent_repos_serialize_in_one_harness_directory() {
        use std::sync::mpsc::{self, RecvTimeoutError};
        let (tmp, first, harnesses) = skill_setup(LinkMode::Copy);
        let other = tmp.path().join("other-repo");
        write(&other.join("sk1ll.toml"), "");
        write(&other.join("personal/notes/SKILL.md"), "Notes.\n");
        let second = Repo::open(&other).unwrap();
        let assignment = Assignment::default();
        let place = |repo| plan(repo, &harnesses, &assignment, PlanOptions::default()).unwrap().remove(0);
        let one = place(&first);
        let two = place(&second);
        std::thread::scope(|threads| {
            let (staged, ready) = mpsc::channel();
            let (resume, wait) = mpsc::channel();
            let first = threads.spawn(|| {
                with_hook(
                    Hook::BeforeSwap,
                    move |_| {
                        staged.send(()).unwrap();
                        wait.recv().unwrap();
                    },
                    || apply(&first, &one),
                )
            });
            ready.recv_timeout(Duration::from_secs(5)).unwrap();
            let (started, started_rx) = mpsc::channel();
            let (finished, result) = mpsc::channel();
            let two = &two;
            let second = threads.spawn(move || {
                started.send(()).unwrap();
                finished.send(apply(&second, two)).unwrap();
            });
            started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            let early = result.recv_timeout(Duration::from_millis(100));
            let stage_intact = tmp.path().join("skills/.sk1ll-state/staging/stage-pdf/SKILL.md").is_file();
            resume.send(()).unwrap();
            first.join().unwrap().unwrap();
            second.join().unwrap();
            assert!(matches!(early, Err(RecvTimeoutError::Timeout)), "{early:?}");
            assert!(stage_intact, "the second writer left the first writer's stage alone");
            result.recv().unwrap().unwrap();
        });
        assert_eq!(index_entry(&one.dest).unwrap(), Some(copy_entry(&one.dest)));
        assert_eq!(index_entry(&two.dest).unwrap(), Some(copy_entry(&two.dest)));
    }

    fn adopt_test_entry(repo: &Repo, id: &str) -> Result<PathBuf> {
        crate::git::Git::new(repo.root()).args(["init", "-q"]).run()?;
        displaced::adopt(repo, id, &crate::EngineLock::acquire(repo.root())?)
    }

    #[test]
    fn publishing_does_not_restore_an_index_entry_removed_during_the_swap() {
        let (_tmp, repo, harnesses) = skill_setup(LinkMode::Copy);
        let assignment = Assignment::default();
        let initial = plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap();
        let dest = &initial[0].dest;
        let result = with_hook(
            Hook::AfterSwap,
            |dest| save_index(dest.parent().unwrap(), &Index::default()).unwrap(),
            || apply(&repo, &initial[0]),
        );
        assert_eq!(result.unwrap_err().to_string(), changed_since_plan(dest).to_string());
        assert_eq!(index_entry(dest).unwrap(), None);
        assert!(dest.join("SKILL.md").is_file());
    }

    /// The Item ID of the repo's skill `name`.
    fn skill_id(repo: &Repo, name: &str) -> ItemId {
        item_id_of(repo, &crate::ItemRef::Skill(name.into()))
    }

    /// The Item ID of the repo's rule `name`.
    fn rule_id(repo: &Repo, name: &str) -> ItemId {
        item_id_of(repo, &crate::ItemRef::Rule(name.into()))
    }

    fn item_id_of(repo: &Repo, item: &crate::ItemRef) -> ItemId {
        let ids = Repo::open(repo.root()).unwrap().item_ids().unwrap();
        ids.into_iter().find(|(found, _)| found == item).and_then(|(_, id)| id).expect("an item with an Item ID")
    }

    #[cfg(unix)]
    #[test]
    fn recording_an_exact_link_preserves_an_intervening_index_edit() {
        let (tmp, repo, harnesses) = skill_setup(LinkMode::Symlink);
        let dir = tmp.path().join("skills");
        fs::create_dir(&dir).unwrap();
        symlink_dir(&repo.personal_dir().join("pdf"), &dir.join("pdf")).unwrap();
        let actions = plan(&repo, &harnesses, &Assignment::default(), PlanOptions::default()).unwrap();
        let mut changed = Index::default();
        changed.set(
            "pdf",
            Some(IndexEntry::Copy { hash: "other".into(), token: "t1".into(), payload: "payload".into() }),
            None,
        );
        let edit = changed.clone();
        let result = with_hook(
            Hook::BeforeSwap,
            move |dest| save_index(dest.parent().unwrap(), &edit).unwrap(),
            || apply(&repo, &actions[0]),
        );
        assert_eq!(result.unwrap_err().to_string(), changed_since_plan(&actions[0].dest).to_string());
        assert_eq!(load_index(&dir).unwrap(), changed);
    }

    /// Toggles follow Item IDs (ADR 0006): a rename keeps them, and an item
    /// re-created under the same name doesn't inherit them, whether it was
    /// made through sk1ll or by hand (sk1ll writes it a new Item ID).
    #[test]
    fn toggles_follow_item_ids_through_renames_and_recreation() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repo::init(&tmp.path().join("repo")).unwrap();
        repo.create_item("skill", "made", "").unwrap();
        repo.create_item_with("rule", "made", "Made.\n").unwrap();
        write(&repo.personal_dir().join("hand/SKILL.md"), "Hand-made.\n");
        repo.assign_missing_ids().unwrap();
        let harness = Harness {
            skills: Some(tmp.path().join("skills").to_string_lossy().into_owned()),
            instructions: Some(tmp.path().join("AGENTS.md").to_string_lossy().into_owned()),
            ..Harness::default()
        };
        let harnesses = BTreeMap::from([("h".to_string(), harness)]);
        let mut assignment = Assignment::default();
        for (item, id) in Repo::open(repo.root()).unwrap().item_ids().unwrap() {
            let id = id.expect("every item has an Item ID");
            match item {
                crate::ItemRef::Skill(_) => assignment.set_skill("h", &id, false),
                _ => assignment.set_rule("h", &id, false),
            }
        }
        let planned = |repo: &Repo| -> Vec<String> {
            let repo = Repo::open(repo.root()).unwrap();
            let actions = plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap();
            let mut names: Vec<String> = actions
                .iter()
                .flat_map(|a| match &a.item {
                    Item::Skill { name, .. } => vec![name.clone()],
                    Item::Instructions { rules, .. } => rules.clone(),
                    Item::Rule { file, .. } => vec![file.clone()],
                })
                .collect();
            names.sort();
            names
        };
        assert!(planned(&repo).is_empty());

        repo.rename_item("skill", "made", "renamed").unwrap();
        repo.rename_item("rule", "made", "renamed").unwrap();
        repo.rename_item("skill", "hand", "handier").unwrap();
        assert!(planned(&repo).is_empty(), "renamed items keep their toggles");

        repo.delete_item("skill", "renamed").unwrap();
        repo.delete_item("rule", "renamed").unwrap();
        repo.create_item("skill", "renamed", "").unwrap();
        repo.create_item_with("rule", "renamed", "Made again.\n").unwrap();
        write(&repo.personal_dir().join("hand/SKILL.md"), "Hand-made again.\n");
        assert_eq!(planned(&repo), ["hand", "renamed", "renamed"], "an item without an ID has the defaults");
        repo.assign_missing_ids().unwrap();
        assert_eq!(planned(&repo), ["hand", "renamed", "renamed"], "new ids start from the defaults");
    }

    #[test]
    #[ignore = "sync planning benchmark"]
    fn sync_plan_no_change() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repo::init(&tmp.path().join("repo")).unwrap();
        for i in 0..200 {
            let skill = repo.personal_dir().join(format!("skill-{i}"));
            write(&skill.join("SKILL.md"), "A skill.\n");
            write(&skill.join("references/guide.md"), "Reference material.\n");
            write(&skill.join("examples/example.md"), "An example.\n");
        }
        let harnesses = (0..4)
            .map(|i| {
                let name = format!("harness-{i}");
                let harness = Harness {
                    skills: Some(tmp.path().join(&name).join("skills").to_string_lossy().into_owned()),
                    mode: LinkMode::Copy,
                    ..Harness::default()
                };
                (name, harness)
            })
            .collect();
        let assignment = Assignment::default();
        let actions = plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap();
        assert_eq!(actions.len(), 800);
        for action in &actions {
            apply(&repo, action).unwrap();
        }
        let start = std::time::Instant::now();
        let actions = plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap();
        let elapsed = start.elapsed();
        assert!(actions.is_empty(), "{actions:?}");
        println!("No-change sync plan for 200 skills across 4 copy-mode harnesses: {elapsed:?}");
    }

    #[test]
    fn skill_id_is_ignored_in_hashes_and_copies_but_visible_through_links() {
        for mode in [LinkMode::Copy, LinkMode::Symlink] {
            let (_tmp, repo, harnesses) = skill_setup(mode);
            let src = repo.personal_dir().join("pdf");
            let hash = hash_dir(&src).unwrap();
            ItemId::generate().write_skill(&src).unwrap();
            assert_eq!(hash_dir(&src).unwrap(), hash);
            let plan_now = || plan(&repo, &harnesses, &Assignment::default(), PlanOptions::default()).unwrap();
            let actions = plan_now();
            apply(&repo, &actions[0]).unwrap();
            assert_eq!(actions[0].dest.join(ID_FILE).is_file(), mode == LinkMode::Symlink);
            ItemId::generate().write_skill(&src).unwrap();
            let repair = plan_now().remove(0);
            assert!(matches!(repair.item, Item::Skill { metadata_only: true, .. }));
            apply(&repo, &repair).unwrap();
            assert!(plan_now().is_empty());
            assert!(displaced::list().unwrap().iter().all(|entry| entry.dest != repair.dest));
        }
    }

    #[test]
    fn reviewed_copies_reject_source_edits_before_and_during_staging() {
        for point in [Hook::BeforeCopy, Hook::CopyEntry] {
            let (_tmp, repo, harnesses) = skill_setup(LinkMode::Copy);
            let src = repo.personal_dir().join("pdf");
            let file = src.join("SKILL.md");
            let actions = plan(&repo, &harnesses, &Assignment::default(), PlanOptions::default()).unwrap();
            let action = &actions[0];
            let digest = hash_dir(&src).unwrap();
            let error = with_hooks(
                move |at, path| {
                    if at == point && path == if point == Hook::BeforeCopy { &src } else { &file } {
                        write(&file, "Edited since review.\n");
                    }
                },
                || apply_reviewed(&repo, action, Some(&digest)),
            )
            .unwrap_err();
            assert!(matches!(error, Error::Changed(_)), "{point:?}: {error}");
            assert!(!action.dest.exists());
            apply(&repo, action).unwrap();
            assert_eq!(index_entry(&action.dest).unwrap(), Some(copy_entry(&action.dest)));
            let mut names: Vec<_> = fs::read_dir(&action.dest).unwrap().map(|e| e.unwrap().file_name()).collect();
            names.sort();
            assert_eq!(names, [PLACEMENT_MARKER, "SKILL.md"]);
        }
    }

    #[test]
    fn stale_skill_plans_preserve_harness_edits() {
        for remove in [false, true] {
            for force in [false, true] {
                let (tmp, repo, harnesses) = skill_setup(LinkMode::Copy);
                let mut assignment = Assignment::default();
                let actions = plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap();
                apply(&repo, &actions[0]).unwrap();
                write(&repo.personal_dir().join("pdf/SKILL.md"), "Updated source.\n");
                if remove {
                    assignment.set_skill("h", &skill_id(&repo, "pdf"), false);
                }
                let actions = plan(&repo, &harnesses, &assignment, PlanOptions { force }).unwrap();
                assert_eq!(actions[0].kind, if remove { ActionKind::Remove } else { ActionKind::Update });
                let dest = tmp.path().join("skills/pdf");
                write(&dest.join("SKILL.md"), "Late harness edit.\n");
                let before = hash_dir(&dest).unwrap();
                let path = dest.parent().unwrap().join(INDEX_PATH);
                let index = fs::read(&path).unwrap();
                assert!(apply(&repo, &actions[0]).is_err());
                assert_eq!(hash_dir(&dest).unwrap(), before);
                assert_eq!(fs::read(path).unwrap(), index);
            }
        }
    }

    /// Runs `f` with `edit` hooked in at `point`, once.
    pub(super) fn with_hook<T>(point: Hook, edit: impl FnOnce(&Path) + 'static, f: impl FnOnce() -> T) -> T {
        let mut edit = Some(edit);
        HOOK.with_borrow_mut(|h| {
            *h = Some(Box::new(move |at, dest| {
                if at == point
                    && let Some(edit) = edit.take()
                {
                    edit(dest)
                }
            }))
        });
        let out = f();
        HOOK.with_borrow_mut(|h| *h = None);
        out
    }

    #[test]
    fn a_plan_hashes_each_source_skill_once() {
        let (tmp, repo, mut harnesses) = skill_setup(LinkMode::Copy);
        let second = Harness {
            skills: Some(tmp.path().join("skills2").to_string_lossy().into_owned()),
            mode: LinkMode::Copy,
            ..Harness::default()
        };
        harnesses.insert("i".to_string(), second);
        let assignment = Assignment::default();
        for action in plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap() {
            apply(&repo, &action).unwrap();
        }
        // The source changes after the first harness was compared with it:
        // the second is judged against the same observation, not a newer one.
        let source = repo.personal_dir().join("pdf/SKILL.md");
        let mut calls = 0;
        HOOK.with_borrow_mut(|h| {
            *h = Some(Box::new(move |at, _| {
                calls += usize::from(at == Hook::PlanSource);
                if calls == 2 {
                    write(&source, "Updated source.\n");
                }
            }))
        });
        let actions = plan(&repo, &harnesses, &assignment, PlanOptions::default());
        HOOK.with_borrow_mut(|h| *h = None);
        assert!(actions.unwrap().is_empty());
        // The next plan sees the edit, for both harnesses.
        let actions = plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap();
        assert_eq!(
            actions.iter().map(|a| (a.harness.as_str(), a.kind)).collect::<Vec<_>>(),
            [("h", ActionKind::Update), ("i", ActionKind::Update)]
        );
        // A harness edit made while the reused hash is looked up is still
        // judged by `apply` against what the plan observed.
        let dest = tmp.path().join("skills2/pdf");
        let mut calls = 0;
        HOOK.with_borrow_mut(|h| {
            *h = Some(Box::new(move |at, dest| {
                calls += usize::from(at == Hook::PlanSource);
                if calls == 2 {
                    write(&dest.join("SKILL.md"), "Late harness edit.\n");
                }
            }))
        });
        let actions = plan(&repo, &harnesses, &assignment, PlanOptions::default());
        HOOK.with_borrow_mut(|h| *h = None);
        let actions = actions.unwrap();
        assert_eq!(actions.len(), 2);
        apply(&repo, &actions[0]).unwrap();
        assert!(apply(&repo, &actions[1]).is_err());
        assert_eq!(fs::read_to_string(dest.join("SKILL.md")).unwrap(), "Late harness edit.\n");
    }

    #[test]
    fn harness_edit_while_planning_a_refresh_is_never_overwritten() {
        for force in [false, true] {
            let (tmp, repo, harnesses) = skill_setup(LinkMode::Copy);
            let assignment = Assignment::default();
            apply(&repo, &plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap()[0]).unwrap();
            write(&repo.personal_dir().join("pdf/SKILL.md"), "Updated source.\n");
            let dest = tmp.path().join("skills/pdf");
            let edit = |dest: &Path| write(&dest.join("SKILL.md"), "Edited while the source was hashed.\n");
            let actions = with_hook(Hook::PlanSource, edit, || {
                plan(&repo, &harnesses, &assignment, PlanOptions { force }).unwrap()
            });
            // Whatever the plan says, applying it must not lose the edit:
            // either it's a Conflict, or its precondition predates the edit.
            let _ = apply(&repo, &actions[0]);
            assert_eq!(
                fs::read_to_string(dest.join("SKILL.md")).unwrap(),
                "Edited while the source was hashed.\n",
                "force={force}: {actions:?}"
            );
        }
    }

    #[test]
    fn harness_edit_while_applying_is_never_overwritten() {
        let cases = [
            (Hook::BeforeSwap, LinkMode::Copy, false),
            (Hook::BeforeSwap, LinkMode::Copy, true),
            (Hook::BeforeSwap, LinkMode::Symlink, false),
            (Hook::AfterCheck, LinkMode::Copy, false),
            (Hook::AfterCheck, LinkMode::Symlink, false),
        ];
        for (point, mode, remove) in cases {
            let (tmp, repo, mut harnesses) = skill_setup(LinkMode::Copy);
            let mut assignment = Assignment::default();
            apply(&repo, &plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap()[0]).unwrap();
            let dest = tmp.path().join("skills/pdf");
            // Copy: refresh from a changed source. Symlink: replace the copy with a link.
            write(&repo.personal_dir().join("pdf/SKILL.md"), "Updated source.\n");
            harnesses.get_mut("h").unwrap().mode = mode;
            assignment.set_skill("h", &skill_id(&repo, "pdf"), !remove);
            let actions = plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap();
            assert!(matches!(actions[0].kind, ActionKind::Update | ActionKind::Remove), "{actions:?}");
            let edit = |dest: &Path| write(&dest.join("SKILL.md"), "Late edit.\n");
            let result = with_hook(point, edit, || apply(&repo, &actions[0]));
            assert!(result.is_err(), "{point:?} {mode:?} remove={remove}: {result:?}");
            assert_eq!(fs::read_to_string(dest.join("SKILL.md")).unwrap(), "Late edit.\n");
            let leftovers: Vec<_> = fs::read_dir(tmp.path().join("skills"))
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .filter(|n| n.starts_with(".sk1ll-") && n != INDEX_DIR)
                .collect();
            assert!(leftovers.is_empty(), "{leftovers:?}");
        }
    }

    fn edit_rule_body(path: &Path, body: &str) {
        let before = fs::read_to_string(path).unwrap();
        let (metadata, _) = crate::skill::split_frontmatter(&before);
        let after = metadata.map_or_else(|| body.to_string(), |metadata| format!("---\n{metadata}---\n{body}"));
        write(path, &after);
    }

    fn instructions_setup(format: InstructionsFormat) -> (tempfile::TempDir, Repo, BTreeMap<String, Harness>, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("repo");
        write(&root.join("sk1ll.toml"), "");
        write(&root.join("instructions/git.md"), "Use CC.\n");
        let repo = Repo::open(&root).unwrap();
        repo.assign_missing_ids().unwrap();
        let dest = tmp.path().join(if format == InstructionsFormat::Rules { "rules" } else { "AGENTS.md" });
        let harness = Harness {
            instructions: Some(dest.to_string_lossy().into_owned()),
            instructions_format: format,
            ..Harness::default()
        };
        (tmp, repo, BTreeMap::from([("h".to_string(), harness)]), dest)
    }

    #[test]
    fn instruction_edit_while_applying_is_never_overwritten() {
        for format in [InstructionsFormat::Rules, InstructionsFormat::Block] {
            let (_tmp, repo, harnesses, dest) = instructions_setup(format);
            let assignment = Assignment::default();
            apply(&repo, &plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap()[0]).unwrap();
            edit_rule_body(&repo.root().join("instructions/git.md"), "Use CC always.\n");
            let actions = plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap();
            assert_eq!(actions[0].kind, ActionKind::Update, "{actions:?}");
            let file = if format == InstructionsFormat::Rules { dest.join("git.md") } else { dest.clone() };
            let late = {
                let file = file.clone();
                move |_: &Path| {
                    let text = fs::read_to_string(&file).unwrap();
                    write(&file, &format!("{text}Late edit.\n"));
                }
            };
            let result = with_hook(Hook::BeforeSwap, late, || apply(&repo, &actions[0]));
            assert!(result.is_err(), "{format:?}: {result:?}");
            assert!(fs::read_to_string(&file).unwrap().ends_with("Late edit.\n"), "{format:?}");
            let stray: Vec<_> = fs::read_dir(file.parent().unwrap())
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .filter(|n| n.contains("sk1ll-tmp") || n.starts_with(".sk1ll-stage"))
                .collect();
            assert!(stray.is_empty(), "{stray:?}");
        }
    }

    /// Rewrites of a block instructions file that only replace what sk1ll
    /// wrote (a rule toggled off and on, a rule changed) keep every byte
    /// outside the block and leave no recovery entry. A forced rewrite of a block
    /// edited by hand keeps the old file for good, marked changed.
    #[test]
    fn rewriting_an_owned_block_leaves_no_recovery_entry() {
        let (tmp, repo, harnesses, dest) = instructions_setup(InstructionsFormat::Block);
        let _root = displaced::test_root(&tmp.path().join("recovery"));
        let mut assignment = Assignment::default();
        let sync = |assignment: &Assignment, force| {
            for action in plan(&repo, &harnesses, assignment, PlanOptions { force }).unwrap() {
                apply(&repo, &action).unwrap();
            }
        };
        sync(&assignment, false);
        assignment.set_rule("h", &rule_id(&repo, "git"), false);
        sync(&assignment, false);
        assert!(!dest.exists(), "sync created it, and its block was all of it");
        let entries = displaced::list().unwrap();
        assert!(entries.len() == 1 && !entries[0].changed, "taken out into recovery: {entries:?}");
        assert_eq!(displaced::prune_older_than(Duration::ZERO).unwrap().len(), 1);
        assignment.set_rule("h", &rule_id(&repo, "git"), true);
        sync(&assignment, false);
        write(&dest, &format!("# Mine\n\n{}", fs::read_to_string(&dest).unwrap()));
        edit_rule_body(&repo.root().join("instructions/git.md"), "Use CC always.\n");
        sync(&assignment, false);
        let text = fs::read_to_string(&dest).unwrap();
        assert!(text.starts_with("# Mine\n") && text.contains("Use CC always."), "{text}");
        assert!(displaced::list().unwrap().is_empty(), "{:?}", displaced::list().unwrap());

        let edited = text.replace("Use CC always.", "Use CC by hand.");
        write(&dest, &edited);
        assert_eq!(plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap()[0].kind, ActionKind::Conflict);
        sync(&assignment, true);
        let entries = displaced::list().unwrap();
        assert!(entries.len() == 1 && entries[0].changed, "{entries:?}");
        assert_eq!(fs::read_to_string(&entries[0].path).unwrap(), edited);
        assert!(displaced::prune_older_than(Duration::ZERO).unwrap().is_empty());
    }

    /// Turning every rule off for a block file: one sync created and that
    /// holds nothing but its block is taken out; any other stays, emptied
    /// of the block.
    #[test]
    fn an_emptied_block_file_goes_only_if_sync_created_it_and_nobody_touched_it() {
        let toggle = |repo: &Repo, harnesses: &BTreeMap<String, Harness>, on: bool| {
            let mut assignment = Assignment::default();
            assignment.set_rule("h", &rule_id(repo, "git"), on);
            for action in plan(repo, harnesses, &assignment, PlanOptions::default()).unwrap() {
                apply(repo, &action).unwrap();
            }
        };
        // Created, then emptied: removed, into recovery.
        let (tmp, repo, harnesses, dest) = instructions_setup(InstructionsFormat::Block);
        let _root = displaced::test_root(&tmp.path().join("recovery"));
        toggle(&repo, &harnesses, true);
        assert!(fs::read_to_string(&dest).unwrap().contains("Use CC."));
        toggle(&repo, &harnesses, false);
        assert!(!dest.exists());
        let entries = displaced::list().unwrap();
        assert!(entries.len() == 1 && entries[0].dest == dest && !entries[0].changed, "{entries:?}");
        assert!(displaced::created_block(&dest).unwrap().is_none(), "the record goes with it");

        // There before sync wrote to it (empty): kept.
        let (tmp, repo, harnesses, dest) = instructions_setup(InstructionsFormat::Block);
        let _root = displaced::test_root(&tmp.path().join("recovery"));
        write(&dest, "");
        toggle(&repo, &harnesses, true);
        toggle(&repo, &harnesses, false);
        assert_eq!(fs::read_to_string(&dest).unwrap(), "");

        // Edited by the user after sync created it (whitespace only): kept for good.
        let (tmp, repo, harnesses, dest) = instructions_setup(InstructionsFormat::Block);
        let _root = displaced::test_root(&tmp.path().join("recovery"));
        toggle(&repo, &harnesses, true);
        write(&dest, &format!("\n{}", fs::read_to_string(&dest).unwrap()));
        toggle(&repo, &harnesses, false);
        assert!(dest.exists() && fs::read_to_string(&dest).unwrap().trim().is_empty(), "kept, emptied of the block");
        toggle(&repo, &harnesses, true);
        toggle(&repo, &harnesses, false);
        assert!(dest.exists(), "once the user's, always the user's");

        // Text added outside the block: kept, with that text.
        let (tmp, repo, harnesses, dest) = instructions_setup(InstructionsFormat::Block);
        let _root = displaced::test_root(&tmp.path().join("recovery"));
        toggle(&repo, &harnesses, true);
        write(&dest, &format!("# Mine\n\n{}", fs::read_to_string(&dest).unwrap()));
        toggle(&repo, &harnesses, false);
        assert_eq!(fs::read_to_string(&dest).unwrap().trim(), "# Mine");
        assert!(displaced::list().unwrap().is_empty());
    }

    /// The record is of the very file sync published: a file of the user's
    /// put in its place with the same text, or the same file given another
    /// mode, is the user's, and only its block is taken out.
    #[cfg(unix)]
    #[test]
    fn an_emptied_block_file_replaced_or_rechmodded_by_the_user_stays() {
        use std::os::unix::fs::PermissionsExt;
        let toggle = |repo: &Repo, harnesses: &BTreeMap<String, Harness>, on: bool| {
            let mut assignment = Assignment::default();
            assignment.set_rule("h", &rule_id(repo, "git"), on);
            for action in plan(repo, harnesses, &assignment, PlanOptions::default()).unwrap() {
                apply(repo, &action).unwrap();
            }
        };
        for case in ["replaced", "chmod"] {
            let (tmp, repo, harnesses, dest) = instructions_setup(InstructionsFormat::Block);
            let _root = displaced::test_root(&tmp.path().join("recovery"));
            toggle(&repo, &harnesses, true);
            let text = fs::read_to_string(&dest).unwrap();
            match case {
                "replaced" => {
                    let mine = tmp.path().join("mine.md");
                    write(&mine, &text);
                    fs::rename(&mine, &dest).unwrap();
                }
                _ => fs::set_permissions(&dest, fs::Permissions::from_mode(0o600)).unwrap(),
            }
            assert_eq!(fs::read_to_string(&dest).unwrap(), text, "{case}: same text");
            toggle(&repo, &harnesses, false);
            assert!(dest.exists(), "{case}: the user's file stays");
            assert!(displaced::created_block(&dest).unwrap().is_none(), "{case}: and is the user's for good");
        }
    }

    /// The removal checks the file it moved aside too: one that isn't what
    /// sync published goes back, and nothing is removed.
    #[cfg(unix)]
    #[test]
    fn a_created_block_file_swapped_during_its_removal_goes_back() {
        let (tmp, repo, harnesses, dest) = instructions_setup(InstructionsFormat::Block);
        let _root = displaced::test_root(&tmp.path().join("recovery"));
        let mut assignment = Assignment::default();
        apply(&repo, &plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap()[0]).unwrap();
        let text = fs::read_to_string(&dest).unwrap();
        assignment.set_rule("h", &rule_id(&repo, "git"), false);
        let actions = plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap();
        let swap = {
            let (dest, mine) = (dest.clone(), tmp.path().join("mine.md"));
            move |_: &Path| {
                write(&mine, &text);
                fs::rename(&mine, &dest).unwrap();
            }
        };
        assert!(with_hook(Hook::BeforeSwap, swap, || apply(&repo, &actions[0])).is_err());
        assert!(dest.exists(), "put back");
        assert!(displaced::list().unwrap().is_empty(), "{:?}", displaced::list().unwrap());
    }

    /// A write that lands while sync takes the file out is never lost: the
    /// file is put back, or kept in recovery, changed.
    #[test]
    fn a_write_racing_the_removal_of_a_created_block_file_is_kept() {
        let (tmp, repo, harnesses, dest) = instructions_setup(InstructionsFormat::Block);
        let _root = displaced::test_root(&tmp.path().join("recovery"));
        let mut assignment = Assignment::default();
        apply(&repo, &plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap()[0]).unwrap();
        assignment.set_rule("h", &rule_id(&repo, "git"), false);
        let actions = plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap();
        assert_eq!(actions[0].kind, ActionKind::Remove);
        let late = {
            let dest = dest.clone();
            move |_: &Path| write(&dest, "Late edit.\n")
        };
        assert!(with_hook(Hook::BeforeSwap, late, || apply(&repo, &actions[0])).is_err());
        assert_eq!(fs::read_to_string(&dest).unwrap(), "Late edit.\n", "left where it was");
        assert!(displaced::created_block(&dest).unwrap().is_some(), "not forgotten: nothing was removed");
    }

    #[test]
    fn absent_rule_is_omitted_from_placements_but_removed_from_index() {
        let (_tmp, repo, harnesses, dir) = instructions_setup(InstructionsFormat::Rules);
        let mut assignment = Assignment::default();
        let initial = plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap();
        apply(&repo, &initial[0]).unwrap();
        fs::remove_file(dir.join("git.md")).unwrap();
        assert!(placed(&repo, "h", &harnesses["h"]).unwrap().is_empty());
        assignment.set_rule("h", &rule_id(&repo, "git"), false);
        let cleanup = plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap();
        assert_eq!(cleanup[0].kind, ActionKind::Remove);
        apply(&repo, &cleanup[0]).unwrap();
        assert!(load_index(&dir).unwrap().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn bookkeeping_honors_umask_and_preserves_replacements() {
        use std::os::unix::fs::PermissionsExt;
        use std::os::unix::process::CommandExt;
        const CHILD: &str = "SK1LL_BOOKKEEPING_UMASK_CHILD";
        if std::env::var_os(CHILD).is_none() {
            // The shared target directory can be rebuilt while this binary runs.
            #[cfg(target_os = "linux")]
            let executable = PathBuf::from("/proc/self/exe");
            #[cfg(not(target_os = "linux"))]
            let executable = std::env::current_exe().unwrap();
            let mut cmd = std::process::Command::new(executable);
            cmd.args(["--exact", "sync::tests::bookkeeping_honors_umask_and_preserves_replacements", "--nocapture"])
                .env(CHILD, "1");
            unsafe {
                cmd.pre_exec(|| {
                    libc::umask(0o077);
                    Ok(())
                });
            }
            assert!(cmd.status().unwrap().success());
            return;
        }
        let (tmp, repo, _, rules_dir) = instructions_setup(InstructionsFormat::Rules);
        let assignment_path = tmp.path().join("assignment.json");
        let links_dir = tmp.path().join("links");
        let incoming = rules_dir.join("new.md");
        let adopted = repo.root().join("instructions/new.md");
        write(&incoming, "A local rule.\n");
        let target = repo.personal_dir().join("pdf");
        let mut links = Index::default();
        links.set(
            "pdf",
            Some(IndexEntry::Link { hash: link_identity(&target), target, payload: "payload".into() }),
            None,
        );
        let save = || {
            Assignment::default().save(&assignment_path).unwrap();
            adopt_rule(&repo, "h", &incoming, "new.md").unwrap();
            save_index(&links_dir, &links).unwrap();
        };
        save();
        let files = [assignment_path.clone(), adopted, links_dir.join(INDEX_PATH), rules_dir.join(INDEX_PATH)];
        for file in &files {
            assert_eq!(fs::metadata(file).unwrap().permissions().mode() & 0o777, 0o600, "{file:?}");
            fs::set_permissions(file, fs::Permissions::from_mode(0o640)).unwrap();
        }
        save();
        for file in files {
            assert_eq!(fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o640, "{file:?}");
        }
    }

    /// Whatever holds the staging name next to a rule file is never written
    /// through, nor published as the rule.
    #[cfg(unix)]
    #[test]
    fn a_planted_staging_name_is_never_followed() {
        let (tmp, repo, harnesses, dest) = instructions_setup(InstructionsFormat::Rules);
        let assignment = Assignment::default();
        let victim = tmp.path().join("victim");
        write(&victim, "victim\n");
        fs::create_dir_all(&dest).unwrap();
        std::os::unix::fs::symlink(&victim, dest.join(".git.md.sk1ll-tmp")).unwrap();
        apply(&repo, &plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap()[0]).unwrap();
        assert!(fs::symlink_metadata(dest.join("git.md")).unwrap().file_type().is_file());
        assert_eq!(fs::read_to_string(dest.join("git.md")).unwrap(), "Use CC.\n");
        assert_eq!(fs::read_to_string(&victim).unwrap(), "victim\n");
    }

    /// A rule file replaced by a symlink (even to the same text) or a FIFO
    /// is someone else's: a conflict, never followed, read or replaced.
    #[cfg(unix)]
    #[test]
    fn a_rule_file_that_isnt_a_plain_file_is_a_conflict() {
        let (tmp, repo, harnesses, dest) = instructions_setup(InstructionsFormat::Rules);
        let assignment = Assignment::default();
        apply(&repo, &plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap()[0]).unwrap();
        let rule = dest.join("git.md");
        let text = fs::read_to_string(&rule).unwrap();
        let mine = tmp.path().join("dotfiles/git.md");
        write(&mine, &text);
        fs::remove_file(&rule).unwrap();
        std::os::unix::fs::symlink(&mine, &rule).unwrap();
        edit_rule_body(&repo.root().join("instructions/git.md"), "Use CC always.\n");
        let actions = plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap();
        assert_eq!(actions.iter().map(|a| a.kind).collect::<Vec<_>>(), [ActionKind::Conflict], "{actions:?}");
        assert!(fs::symlink_metadata(&rule).unwrap().file_type().is_symlink());
        assert_eq!(fs::read_to_string(&mine).unwrap(), text);

        fs::remove_file(&rule).unwrap();
        let c = std::ffi::CString::new(rule.to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        let actions = plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap();
        assert_eq!(actions.iter().map(|a| a.kind).collect::<Vec<_>>(), [ActionKind::Conflict], "{actions:?}");
        assert!(actions[0].reason.as_deref().is_some_and(|r| r.contains(NOT_A_FILE)), "{actions:?}");
    }

    #[cfg(unix)]
    #[test]
    fn stale_skill_plan_preserves_retargeted_link() {
        let (tmp, repo, harnesses) = skill_setup(LinkMode::Symlink);
        let mut assignment = Assignment::default();
        let actions = plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap();
        apply(&repo, &actions[0]).unwrap();
        let dir = tmp.path().join("skills");
        let index = fs::read(dir.join(INDEX_PATH)).unwrap();
        assignment.set_skill("h", &skill_id(&repo, "pdf"), false);
        let actions = plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap();
        fs::remove_file(dir.join("pdf")).unwrap();
        // Same canonical target and ownership, but a different observed link.
        std::os::unix::fs::symlink("../repo/personal/pdf", dir.join("pdf")).unwrap();
        assert!(apply(&repo, &actions[0]).is_err());
        assert_eq!(fs::read_link(dir.join("pdf")).unwrap(), Path::new("../repo/personal/pdf"));
        assert_eq!(fs::read(dir.join(INDEX_PATH)).unwrap(), index);
    }

    #[cfg(unix)]
    #[test]
    fn unsupported_copy_entries_leave_existing_content_untouched() {
        let (tmp, repo, harnesses) = skill_setup(LinkMode::Copy);
        let assignment = Assignment::default();
        let actions = plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap();
        apply(&repo, &actions[0]).unwrap();
        write(&repo.personal_dir().join("pdf/SKILL.md"), "Changed.\n");
        let actions = plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap();
        let socket = std::os::unix::net::UnixListener::bind(repo.personal_dir().join("pdf/socket")).unwrap();
        let dest = tmp.path().join("skills/pdf");
        let before = hash_dir(&dest).unwrap();
        assert!(apply(&repo, &actions[0]).is_err());
        assert_eq!(hash_dir(&dest).unwrap(), before);
        assert!(!dest.join("socket").exists());
        drop(socket);
    }

    #[cfg(unix)]
    #[test]
    fn indexed_links_are_removed_when_repo_skill_is_deleted() {
        let (tmp, repo, harnesses) = skill_setup(LinkMode::Symlink);
        let assignment = Assignment::default();
        let actions = plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap();
        apply(&repo, &actions[0]).unwrap();
        assert!(plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap().is_empty());
        fs::remove_dir_all(repo.personal_dir().join("pdf")).unwrap();
        let actions = plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap();
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].kind, ActionKind::Remove);
        apply(&repo, &actions[0]).unwrap();
        assert!(!tmp.path().join("skills/pdf").is_symlink());
        assert!(!tmp.path().join("skills").join(INDEX_PATH).exists());
    }

    #[cfg(unix)]
    #[test]
    fn migrated_relative_links_are_removed_after_source_deletion_but_unindexed_links_remain() {
        let (tmp, repo, harnesses) = skill_setup(LinkMode::Symlink);
        let assignment = Assignment::default();
        let dir = tmp.path().join("skills");
        fs::create_dir(&dir).unwrap();
        let target = Path::new("../repo/personal/../personal/./pdf");
        std::os::unix::fs::symlink(target, dir.join("pdf")).unwrap();
        let actions = plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap();
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].kind, ActionKind::Update);
        apply(&repo, &actions[0]).unwrap();
        assert_eq!(fs::read_link(dir.join("pdf")).unwrap(), target);
        assert!(dir.join(INDEX_PATH).is_file());
        assert!(plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap().is_empty());

        fs::remove_dir_all(repo.personal_dir().join("pdf")).unwrap();
        std::os::unix::fs::symlink(target, dir.join("manual")).unwrap();
        let actions = plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap();
        let managed = actions.iter().find(|a| a.dest == dir.join("pdf")).unwrap();
        assert_eq!(managed.kind, ActionKind::Remove);
        let manual = actions.iter().find(|a| a.dest == dir.join("manual")).unwrap();
        assert_eq!(manual.kind, ActionKind::Conflict);
        for action in &actions {
            apply(&repo, action).unwrap();
        }
        assert!(!dir.join("pdf").is_symlink());
        assert!(!dir.join(INDEX_PATH).exists());
        assert_eq!(fs::read_link(dir.join("manual")).unwrap(), target);
    }

    #[cfg(unix)]
    #[test]
    fn copied_and_adopted_skills_preserve_symlinks() {
        let (tmp, repo, harnesses) = skill_setup(LinkMode::Copy);
        let src = repo.personal_dir().join("pdf");
        write(&src.join("data"), "data\n");
        fs::create_dir(src.join("nested")).unwrap();
        for (name, target) in [("link", "data"), ("dir-link", "nested"), ("dangling", "missing")] {
            std::os::unix::fs::symlink(target, src.join(name)).unwrap();
        }
        let assignment = Assignment::default();
        let actions = plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap();
        apply(&repo, &actions[0]).unwrap();
        let dest = tmp.path().join("skills/pdf");
        assert_eq!(fs::read_link(dest.join("link")).unwrap(), Path::new("data"));
        assert_eq!(hash_dir(&src).unwrap(), hash_dir(&dest).unwrap());
        assert!(plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap().is_empty());
        write(&dest.join("SKILL.md"), "Adopted.\n");
        let actions = plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap();
        adopt(&repo, &actions[0]).unwrap();
        for (name, target) in [("link", "data"), ("dir-link", "nested"), ("dangling", "missing")] {
            assert_eq!(fs::read_link(src.join(name)).unwrap(), Path::new(target));
        }
        assert_eq!(fs::read_to_string(src.join("data")).unwrap(), "data\n");
        assert!(plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn handmade_repo_links_are_unmanaged_unless_exactly_assigned() {
        let (tmp, repo, harnesses) = skill_setup(LinkMode::Symlink);
        let dir = tmp.path().join("skills");
        fs::create_dir(&dir).unwrap();
        std::os::unix::fs::symlink(repo.personal_dir().join("pdf"), dir.join("manual")).unwrap();
        let mut assignment = Assignment::default();
        assignment.set_skill("h", &skill_id(&repo, "pdf"), false);
        let actions = plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap();
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].kind, ActionKind::Conflict);
        apply(&repo, &actions[0]).unwrap();
        assert!(dir.join("manual").is_symlink());

        // Legacy links are adopted only for the exact, currently assigned skill.
        std::os::unix::fs::symlink("../repo/personal/./pdf", dir.join("pdf")).unwrap();
        assignment.set_skill("h", &skill_id(&repo, "pdf"), true);
        let actions = plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap();
        let action = actions.iter().find(|a| a.dest == dir.join("pdf")).unwrap();
        assert_eq!(action.kind, ActionKind::Update);
        apply(&repo, action).unwrap();
        assignment.set_skill("h", &skill_id(&repo, "pdf"), false);
        let actions = plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap();
        let action = actions.iter().find(|a| a.dest == dir.join("pdf")).unwrap();
        assert_eq!(action.kind, ActionKind::Remove);
        apply(&repo, action).unwrap();
        assert!(!dir.join("pdf").is_symlink());
        assert!(dir.join("manual").is_symlink());
    }

    #[cfg(unix)]
    #[test]
    fn links_to_another_repo_directory_conflict_even_with_force() {
        for indexed in [false, true] {
            let (tmp, repo, harnesses) = skill_setup(LinkMode::Symlink);
            let assignment = Assignment::default();
            let dir = tmp.path().join("skills");
            if indexed {
                let actions = plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap();
                apply(&repo, &actions[0]).unwrap();
                fs::remove_file(dir.join("pdf")).unwrap();
            } else {
                fs::create_dir(&dir).unwrap();
            }
            std::os::unix::fs::symlink(repo.personal_dir(), dir.join("pdf")).unwrap();
            for force in [false, true] {
                let actions = plan(&repo, &harnesses, &assignment, PlanOptions { force }).unwrap();
                assert_eq!(actions[0].kind, ActionKind::Conflict);
                apply(&repo, &actions[0]).unwrap();
                assert_eq!(fs::read_link(dir.join("pdf")).unwrap(), repo.personal_dir());
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn repo_link_with_parent_escape_is_unmanaged() {
        let (tmp, repo, harnesses) = skill_setup(LinkMode::Symlink);
        let dir = tmp.path().join("skills");
        fs::create_dir(&dir).unwrap();
        write(&tmp.path().join("outside/SKILL.md"), "Outside.\n");
        std::os::unix::fs::symlink(repo.root().join("../outside"), dir.join("pdf")).unwrap();
        let actions = plan(&repo, &harnesses, &Assignment::default(), PlanOptions::default()).unwrap();
        assert_eq!(actions[0].kind, ActionKind::Conflict);
        apply(&repo, &actions[0]).unwrap();
        assert_eq!(fs::read_to_string(dir.join("pdf/SKILL.md")).unwrap(), "Outside.\n");
    }

    #[test]
    fn missing_ids_use_defaults_and_reconcile_without_replacing_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        let recovery = tmp.path().join("recovery");
        let _root = displaced::test_root(&recovery);
        let repo = Repo::init_committed(&tmp.path().join("repo")).unwrap();
        write(&repo.personal_dir().join("pdf/SKILL.md"), "PDFs.\n");
        write(&repo.root().join("instructions/git.md"), "Use CC.\n");
        let harness = Harness {
            skills: Some(tmp.path().join("skills").to_string_lossy().into_owned()),
            instructions: Some(tmp.path().join("rules").to_string_lossy().into_owned()),
            instructions_format: InstructionsFormat::Rules,
            mode: LinkMode::Copy,
        };
        let harnesses = BTreeMap::from([("h".into(), harness)]);
        // Toggles are keyed by Item ID: an item without one has none, so the
        // defaults decide, even with every known item off.
        let mut assignment = Assignment::default();
        assignment.set_skill("h", &ItemId::from_text("other-skill").unwrap(), false);
        assignment.set_rule("h", &ItemId::from_text("other-rule").unwrap(), false);
        let planned = plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap();
        assert_eq!(planned.len(), 2, "read-only planning uses enabled defaults");
        assert!(repo.item_ids().unwrap().iter().all(|(_, id)| id.is_none()));
        for action in planned {
            apply(&repo, &action).unwrap();
        }
        repo.assign_missing_ids().unwrap();
        let plan_now = || plan(&repo, &harnesses, &Assignment::default(), PlanOptions::default()).unwrap();
        let repairs = plan_now();
        assert_eq!(repairs.len(), 2);
        let receipts = recovery.join(".placements");
        fs::rename(&receipts, recovery.join("previous-receipts")).unwrap();
        write(&receipts, "state temporarily unavailable");
        assert!(apply(&repo, &repairs[0]).is_err());
        fs::remove_file(&receipts).unwrap();
        assert_eq!(plan_now().len(), 2, "an unwritten receipt stays pending");
        for action in repairs {
            assert!(matches!(
                action.item,
                Item::Skill { metadata_only: true, .. } | Item::Rule { metadata_only: true, .. }
            ));
            let before = fs::metadata(&action.dest).unwrap().modified().unwrap();
            apply(&repo, &action).unwrap();
            assert_eq!(fs::metadata(&action.dest).unwrap().modified().unwrap(), before);
        }
        assert!(plan_now().is_empty());
        assert!(displaced::list().unwrap().iter().all(|e| !e.dest.starts_with(tmp.path())));
    }

    #[test]
    fn assignment_selects_skills_and_rules() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("repo");
        write(&root.join("sk1ll.toml"), "");
        write(&root.join("personal/pdf/SKILL.md"), "---\nname: pdf\n---\n");
        write(&root.join("personal/git/SKILL.md"), "---\nname: git\n---\n");
        write(&root.join("instructions/10-only-other.md"), "---\nharnesses: [other]\n---\nNot here.\n");
        write(&root.join("instructions/20-legacy.md"), "---\ntargets: [h]\n---\nLegacy key.\n");
        write(&root.join("instructions/30-off.md"), "Disabled.\n");
        let repo = Repo::open(&root).unwrap();
        repo.assign_missing_ids().unwrap();

        let out = tmp.path().join("harness");
        let harness = Harness {
            skills: Some(out.join("skills").to_string_lossy().into_owned()),
            instructions: Some(out.join("AGENTS.md").to_string_lossy().into_owned()),
            ..Harness::default()
        };
        let harnesses = BTreeMap::from([("h".to_string(), harness)]);
        let mut assignment = Assignment::default();
        assignment.set_skill("h", &skill_id(&repo, "pdf"), false);
        assignment.set_rule("h", &rule_id(&repo, "30-off"), false);

        let actions = plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap();
        let created: Vec<&str> = actions
            .iter()
            .filter(|a| a.kind == ActionKind::Create)
            .filter_map(|a| match &a.item {
                Item::Skill { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(created, ["git"]);
        let rules = actions.iter().find_map(|a| match &a.item {
            Item::Instructions { rules, .. } => Some(rules.clone()),
            _ => None,
        });
        assert_eq!(rules, Some(vec!["20-legacy".to_string()]));
        assert!(actions.iter().all(|a| a.harness == "h"));

        // Turning an applied skill off removes it.
        for a in &actions {
            apply(&repo, a).unwrap();
        }
        assignment.set_skill("h", &skill_id(&repo, "git"), false);
        let actions = plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap();
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].kind, ActionKind::Remove);
        assert_eq!(actions[0].dest, out.join("skills/git"));
    }

    #[test]
    fn adopt_turns_harness_edits_into_draft() {
        use crate::draft::{Change, ItemRef};
        use crate::git::Git;

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("repo");
        write(&root.join("sk1ll.toml"), "");
        write(&root.join("personal/pdf/SKILL.md"), "---\nname: pdf\n---\n");
        write(&root.join("personal/pdf/ref.md"), "ref\n");
        let id = ItemId::generate();
        id.write_skill(&root.join("personal/pdf")).unwrap();
        write(
            &root.join("instructions/git.md"),
            "---\nharnesses: [h, b]\nid: git-rule\ndescription: Git\n---\nUse CC.\n",
        );
        for args in [
            &["init", "-q"][..],
            &["add", "-A"],
            &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qm", "seed"],
        ] {
            Git::new(&root).args(args).run().unwrap();
        }
        let repo = Repo::open(&root).unwrap();

        let out = tmp.path().join("harness");
        let h = Harness {
            skills: Some(out.join("skills").to_string_lossy().into_owned()),
            instructions: Some(out.join("rules").to_string_lossy().into_owned()),
            instructions_format: InstructionsFormat::Rules,
            mode: LinkMode::Copy,
        };
        let b =
            Harness { instructions: Some(out.join("AGENTS.md").to_string_lossy().into_owned()), ..Harness::default() };
        let harnesses = BTreeMap::from([("h".to_string(), h), ("b".to_string(), b)]);
        let assignment = Assignment::default();
        let plan_now = |force| plan(&repo, &harnesses, &assignment, PlanOptions { force }).unwrap();
        for a in plan_now(false) {
            apply(&repo, &a).unwrap();
        }
        assert!(plan_now(false).is_empty());
        for path in ["rules/git.md", "AGENTS.md"] {
            assert!(!fs::read_to_string(out.join(path)).unwrap().contains("id:"));
        }

        write(&out.join("skills/pdf/SKILL.md"), "---\nname: pdf\n---\nEdited.\n");
        write(&out.join("skills/pdf/extra.md"), "extra\n");
        write(&out.join("rules/git.md"), "---\ndescription: Git\n---\n\nUse CC always.\n");
        let agents = fs::read_to_string(out.join("AGENTS.md")).unwrap();
        write(&out.join("AGENTS.md"), &agents.replace("Use CC.", "Use CC!"));

        let conflicts: Vec<Action> = plan_now(false).into_iter().filter(|a| a.kind == ActionKind::Conflict).collect();
        assert_eq!(conflicts.len(), 3, "{conflicts:?}");
        let forced = plan_now(true);
        assert!(forced.iter().all(|a| a.kind == ActionKind::Update), "{forced:?}");

        for a in &conflicts {
            match &a.item {
                Item::Instructions { .. } => {
                    assert!(matches!(adopt(&repo, a), Err(Error::Invalid(_))))
                }
                _ => adopt(&repo, a).unwrap(),
            }
        }
        assert!(matches!(adopt(&repo, &forced[0]), Err(Error::Invalid(_))));
        assert_eq!(fs::read_to_string(root.join("personal/pdf/SKILL.md")).unwrap(), "---\nname: pdf\n---\nEdited.\n");
        assert_eq!(ItemId::read_skill(&root.join("personal/pdf")).unwrap(), Some(id));
        assert!(root.join("personal/pdf/extra.md").is_file() && !root.join("personal/pdf").join(INDEX_DIR).exists());
        assert_eq!(
            fs::read_to_string(root.join("instructions/git.md")).unwrap(),
            "---\nharnesses: [h, b]\nid: git-rule\ndescription: Git\n---\nUse CC always.\n"
        );
        let draft: Vec<(ItemRef, Change)> = repo.draft().unwrap().into_iter().map(|d| (d.item, d.change)).collect();
        assert_eq!(
            draft,
            [(ItemRef::Skill("pdf".into()), Change::Modified), (ItemRef::Rule("git".into()), Change::Modified)]
        );

        // Only the block harness is still out of line (and its edit conflicts).
        let rest = plan_now(false);
        assert!(rest.iter().all(|a| a.harness == "b" && a.kind == ActionKind::Conflict), "{rest:?}");
    }

    #[test]
    fn rule_adopt_keeps_the_repo_id_and_routing_with_flow_frontmatter() {
        let (tmp, repo, mut harnesses) = skill_setup(LinkMode::Copy);
        let rules = tmp.path().join("rules");
        let config = harnesses.get_mut("h").unwrap();
        config.instructions = Some(rules.display().to_string());
        config.instructions_format = InstructionsFormat::Rules;
        let source = repo.root().join("instructions/git.md");
        write(&source, "---\nid: repo-id\nharnesses: [h]\n---\nOriginal.\n");
        let plan_now = || plan(&repo, &harnesses, &Assignment::default(), PlanOptions::default()).unwrap();
        for action in plan_now() {
            apply(&repo, &action).unwrap();
        }
        let dest = rules.join("git.md");
        let conflict = || plan_now().into_iter().find(|action| action.dest == dest).unwrap();
        let invalid = "---\nid: 42\n---\nInvalid edit.\n";
        write(&dest, invalid);
        assert!(adopt(&repo, &conflict()).is_err());
        assert!(fs::read_to_string(&source).unwrap().contains("Original."));
        assert_eq!(fs::read_to_string(&dest).unwrap(), invalid);
        assert_eq!(conflict().kind, ActionKind::Conflict);

        write(&dest, "---\n{paths: [src/**], id: other}\n---\nHarness edit.\n");
        adopt(&repo, &conflict()).unwrap();
        let rule = repo.rules().unwrap().remove(0);
        assert_eq!(rule.id.as_ref().unwrap().to_string(), "repo-id");
        assert_eq!(rule.harnesses.as_deref().unwrap(), ["h"]);
        let rendered = instructions::render_rule(&rule).1;
        assert!(rendered.contains("src/**") && rendered.contains("Harness edit."));
        assert!(!rendered.contains("other") && !rendered.contains("harnesses"));
    }

    #[test]
    fn adopting_a_filtered_import_copy_keeps_upstream_only_files() {
        let (tmp, repo, harnesses) = skill_setup(LinkMode::Copy);
        let src = repo.imported_dir().join("pdf");
        fs::create_dir_all(src.parent().unwrap()).unwrap();
        fs::rename(repo.personal_dir().join("pdf"), &src).unwrap();
        write(&src.join(ID_FILE), "vendored\n");
        write(&src.join("refs/.git/HEAD"), "upstream\n");
        write(&repo.root().join("sk1ll.toml"), "[import.pdf]\nsource = 'local'\n");
        repo.assign_missing_ids().unwrap();
        let plan_now = || plan(&repo, &harnesses, &Assignment::default(), PlanOptions::default()).unwrap();
        for action in plan_now() {
            apply(&repo, &action).unwrap();
        }
        let copy = tmp.path().join("skills/pdf");
        assert_eq!(fs::read_to_string(copy.join(ID_FILE)).unwrap(), "vendored\n");
        assert!(!copy.join("refs/.git/HEAD").exists());
        write(&copy.join("SKILL.md"), "Harness edit.\n");
        fs::remove_file(copy.join(ID_FILE)).unwrap();
        adopt(&repo, &plan_now()[0]).unwrap();
        assert_eq!(fs::read_to_string(src.join("SKILL.md")).unwrap(), "Harness edit.\n");
        assert!(!src.join(ID_FILE).exists());
        assert_eq!(fs::read_to_string(src.join("refs/.git/HEAD")).unwrap(), "upstream\n");
    }

    #[test]
    fn recovery_follows_a_rename_and_refuses_a_foreign_repo_or_reused_id() {
        let (tmp, _repo, harnesses) = skill_setup(LinkMode::Copy);
        let repo = Repo::init_committed(&tmp.path().join("tracked")).unwrap();
        write(&repo.personal_dir().join("pdf/SKILL.md"), "PDFs.\n");
        let pdf = crate::ItemRef::Skill("pdf".into());
        let docs = crate::ItemRef::Skill("docs".into());
        repo.commit(std::slice::from_ref(&pdf), "PDF", None).unwrap();
        let id = repo.skills().unwrap()[0].id.clone().unwrap();
        let plan_now = || plan(&repo, &harnesses, &Assignment::default(), PlanOptions::default()).unwrap();
        apply(&repo, &plan_now()[0]).unwrap();
        let mut open = fs::OpenOptions::new().append(true).open(tmp.path().join("skills/pdf/SKILL.md")).unwrap();
        repo.rename_item("skill", "pdf", "docs").unwrap();
        for action in plan_now() {
            apply(&repo, &action).unwrap();
        }
        use std::io::Write;
        open.write_all(b"late edit\n").unwrap();
        let entry = displaced::changed()
            .unwrap()
            .into_iter()
            .find(|entry| entry.dest == tmp.path().join("skills/pdf"))
            .unwrap();
        let foreign = Repo::init_committed(&tmp.path().join("foreign")).unwrap();
        assert!(displaced::target(&foreign, &entry).err().unwrap().to_string().contains("another repo"));
        assert_eq!(displaced::target(&repo, &entry).unwrap().path, repo.personal_dir().join("docs"));

        repo.commit(&[pdf, docs.clone()], "Rename", None).unwrap();
        repo.delete_item("skill", "docs").unwrap();
        repo.commit(&[docs], "Delete", None).unwrap();
        let replacement = repo.personal_dir().join("replacement");
        write(&replacement.join("SKILL.md"), "Different item.\n");
        id.write_skill(&replacement).unwrap();
        assert!(adopt_test_entry(&repo, &entry.id).unwrap_err().to_string().contains("Pick a target"));
        assert_eq!(fs::read_to_string(replacement.join("SKILL.md")).unwrap(), "Different item.\n");
        assert!(displaced::get(&entry.id).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn adopting_refuses_symlinked_repo_content_parents() {
        for origin in ["imported", "instructions"] {
            let (tmp, repo, mut harnesses) = skill_setup(LinkMode::Copy);
            let external = tmp.path().join("external");
            let source = if origin == "imported" {
                write(&external.join("vendored/SKILL.md"), "original\n");
                external.join("vendored/SKILL.md")
            } else {
                write(&external.join("git.md"), "original\n");
                let h = harnesses.get_mut("h").unwrap();
                h.instructions = Some(tmp.path().join("rules").to_string_lossy().into_owned());
                h.instructions_format = InstructionsFormat::Rules;
                external.join("git.md")
            };
            std::os::unix::fs::symlink(&external, repo.root().join(origin)).unwrap();
            let dest = if origin == "imported" {
                tmp.path().join("skills/vendored/SKILL.md")
            } else {
                tmp.path().join("rules/git.md")
            };
            let plan_now = || plan(&repo, &harnesses, &Assignment::default(), PlanOptions::default()).unwrap();
            for action in plan_now() {
                apply(&repo, &action).unwrap();
            }
            write(&dest, "harness edit\n");
            let conflict = plan_now().into_iter().find(|action| action.kind == ActionKind::Conflict).unwrap();
            assert!(adopt(&repo, &conflict).is_err(), "{origin}");
            assert_eq!(fs::read_to_string(source).unwrap(), "original\n");
        }
    }

    #[test]
    fn recovery_recreates_a_deleted_rule_with_its_placed_id() {
        let (tmp, repo, mut harnesses) = skill_setup(LinkMode::Copy);
        let _root = displaced::test_root(&tmp.path().join("state/displaced"));
        let rules = tmp.path().join("rules");
        let config = harnesses.get_mut("h").unwrap();
        config.instructions = Some(rules.display().to_string());
        config.instructions_format = InstructionsFormat::Rules;
        let source = repo.root().join("instructions/git.md");
        write(&source, "---\nid: original-rule\n---\nOriginal.\n");
        let plan_now = |force| plan(&repo, &harnesses, &Assignment::default(), PlanOptions { force }).unwrap();
        for action in plan_now(false) {
            apply(&repo, &action).unwrap();
        }
        let dest = rules.join("git.md");
        assert_eq!(fs::read_to_string(&dest).unwrap(), "Original.\n");
        write(&dest, "---\nid: harness-edit\n---\nEdited.\n");
        repo.delete_item("rule", "git").unwrap();
        for action in plan_now(true) {
            apply(&repo, &action).unwrap();
        }
        let entry = displaced::list().unwrap().into_iter().find(|entry| entry.dest == dest).unwrap();
        assert_eq!(entry.item_id.unwrap().to_string(), "original-rule");
        adopt_test_entry(&repo, &entry.id).unwrap();
        let restored = repo.rules().unwrap().remove(0);
        assert_eq!(restored.id.unwrap().to_string(), "original-rule");
        assert_eq!(restored.body, "Edited.");
    }

    /// The recovery entry a displacing apply reported.
    fn reported(result: Result<()>) -> displaced::Entry {
        match result {
            Err(Error::Displaced { kept, id, .. }) => {
                let entry = displaced::get(&id).unwrap();
                assert_eq!(entry.path, kept);
                assert!(entry.changed, "{entry:?}");
                assert_eq!(displaced::changed().unwrap(), std::slice::from_ref(&entry));
                entry
            }
            other => panic!("expected the edit to be reported as displaced, got {other:?}"),
        }
    }

    /// What sk1ll left in `dir` besides its recovery area.
    fn sk1ll_leftovers(dir: &Path) -> Vec<String> {
        let names = |dir: &Path| -> Vec<String> {
            let Ok(entries) = fs::read_dir(dir) else { return Vec::new() };
            entries.map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect()
        };
        let mut found: Vec<_> = names(dir)
            .into_iter()
            .filter(|n| n != TMP_DIR && ["sk1ll-tmp", "sk1ll-old", "sk1ll-stage"].iter().any(|s| n.contains(s)))
            .collect();
        let scratch = names(&dir.join(TMP_DIR)).into_iter().filter(|n| n != "displaced");
        found.extend(scratch.map(|n| format!("{TMP_DIR}/{n}")));
        found
    }

    #[cfg(unix)]
    #[test]
    fn writes_to_instruction_files_during_the_swap_are_never_lost() {
        use InstructionsFormat::{Block, Rules};
        use std::io::Write;
        let formats = [(Rules, false), (Block, false), (Block, true)];
        for (format, linked) in formats {
            for point in [Hook::AfterAside, Hook::AfterSwap] {
                for fallback in [false, true] {
                    let case = format!("{format:?} linked={linked} {point:?} state elsewhere={fallback}");
                    let (tmp, repo, harnesses, dest) = instructions_setup(format);
                    let _root = displaced::test_root(&tmp.path().join("state/displaced"));
                    if linked {
                        // A dotfiles-managed instructions file: a link to the real one.
                        write(&tmp.path().join("dotfiles/AGENTS.md"), "# Mine\n");
                        std::os::unix::fs::symlink(tmp.path().join("dotfiles/AGENTS.md"), &dest).unwrap();
                    }
                    let assignment = Assignment::default();
                    apply(&repo, &plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap()[0]).unwrap();
                    edit_rule_body(&repo.root().join("instructions/git.md"), "Use CC always.\n");
                    let actions = plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap();
                    assert_eq!(actions[0].kind, ActionKind::Update, "{case}: {actions:?}");
                    let file = if format == InstructionsFormat::Rules { dest.join("git.md") } else { dest.clone() };
                    let real = fs::canonicalize(&file).unwrap();
                    displaced::STATE_ON_OTHER_FS.set(fallback);
                    let result = if point == Hook::AfterAside && format == Block {
                        // A block swap still has the original at its live name.
                        let late = {
                            let file = file.clone();
                            move |_: &Path| {
                                let text = fs::read_to_string(&file).unwrap();
                                write(&file, &format!("{text}Late note.\n"));
                            }
                        };
                        with_hook(point, late, || apply(&repo, &actions[0]))
                    } else {
                        // A writer that opened the file before the swap writes after it.
                        let mut open = fs::OpenOptions::new().append(true).open(&file).unwrap();
                        let late = move |_: &Path| open.write_all(b"Late note.\n").unwrap();
                        with_hook(point, late, || apply(&repo, &actions[0]))
                    };
                    let entry = reported(result);
                    // The note is in the recovery area; the harness has the new content.
                    let kept = fs::read_to_string(&entry.path).unwrap();
                    assert!(kept.ends_with("Late note.\n") && kept.contains("Use CC.\n"), "{case}: {kept}");
                    let now = fs::read_to_string(&file).unwrap();
                    assert!(now.contains("Use CC always.") && !now.contains("Late note."), "{case}: {now}");
                    assert_eq!(entry.dest, file, "{case}");
                    // Next to the file it displaced (the target, for a link), on its filesystem.
                    assert!(entry.path.starts_with(displaced::area(real.parent().unwrap())), "{case}");
                    assert!(sk1ll_leftovers(real.parent().unwrap()).is_empty(), "{case}");
                    if linked {
                        assert!(fs::symlink_metadata(&dest).unwrap().file_type().is_symlink(), "{case}");
                        assert!(kept.starts_with("# Mine\n"), "{case}: the target's prior content: {kept}");
                    }
                    // However old, a changed entry is never pruned.
                    assert!(!displaced::prune_older_than(Duration::ZERO).unwrap().contains(&entry.id), "{case}");

                    adopt_test_entry(&repo, &entry.id).unwrap();
                    assert!(displaced::list().unwrap().iter().all(|e| e.id != entry.id), "{case}");
                    if format == InstructionsFormat::Rules {
                        let rule = fs::read_to_string(repo.root().join("instructions/git.md")).unwrap();
                        assert!(rule.ends_with("Late note.\n"), "{case}: {rule}");
                    } else {
                        let now = fs::read_to_string(&file).unwrap();
                        assert!(now.contains("Use CC always.") && now.ends_with("Late note.\n"), "{case}: {now}");
                        assert!(plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap().is_empty());
                    }
                }
            }
        }
    }

    #[test]
    fn writers_with_open_skill_files_never_lose_edits() {
        use std::io::Write;
        for remove in [false, true] {
            for point in [Hook::AfterAside, Hook::AfterSwap] {
                for fallback in [false, true] {
                    let case = format!("remove={remove} {point:?} state elsewhere={fallback}");
                    let (tmp, repo, harnesses) = skill_setup(LinkMode::Copy);
                    let _root = displaced::test_root(&tmp.path().join("state/displaced"));
                    let assignment = Assignment::default();
                    repo.assign_missing_ids().unwrap();
                    let id = repo.skills().unwrap()[0].id.clone();
                    apply(&repo, &plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap()[0]).unwrap();
                    let dest = tmp.path().join("skills/pdf");
                    assert!(!dest.join(ID_FILE).exists());
                    write(&dest.join(ID_FILE), "harness-added-id\n");
                    if remove {
                        repo.delete_item("skill", "pdf").unwrap();
                    } else {
                        write(&repo.personal_dir().join("pdf/SKILL.md"), "Updated source.\n");
                    }
                    let actions = plan(&repo, &harnesses, &assignment, PlanOptions { force: true }).unwrap();
                    // Opened before sync starts; written after every check.
                    let mut open = fs::OpenOptions::new().append(true).open(dest.join("SKILL.md")).unwrap();
                    displaced::STATE_ON_OTHER_FS.set(fallback);
                    let late = move |_: &Path| open.write_all(b"Written through an open file.\n").unwrap();
                    let entry = reported(with_hook(point, late, || apply(&repo, &actions[0])));
                    let kept = fs::read_to_string(entry.path.join("SKILL.md")).unwrap();
                    assert_eq!(kept, "PDFs.\nWritten through an open file.\n", "{case}");
                    assert_eq!(entry.kind, displaced::Kind::Skill);
                    match remove {
                        false => {
                            assert_eq!(fs::read_to_string(dest.join("SKILL.md")).unwrap(), "Updated source.\n");
                            assert_eq!(index_entry(&dest).unwrap(), Some(copy_entry(&dest)));
                        }
                        true => {
                            assert!(fs::symlink_metadata(&dest).is_err(), "{case}");
                            assert_eq!(index_entry(&dest).unwrap(), None);
                        }
                    }
                    assert!(sk1ll_leftovers(&tmp.path().join("skills")).is_empty(), "{case}");

                    assert_eq!(entry.item_id, id);
                    let name = if remove {
                        "pdf"
                    } else {
                        repo.rename_item("skill", "pdf", "docs").unwrap();
                        "docs"
                    };
                    adopt_test_entry(&repo, &entry.id).unwrap();
                    let personal = repo.personal_dir().join(name);
                    assert_eq!(fs::read_to_string(personal.join("SKILL.md")).unwrap(), kept, "{case}");
                    assert_eq!(ItemId::read_skill(&personal).unwrap(), id);
                    assert!(!personal.join(INDEX_DIR).exists());
                    let left = displaced::list().unwrap();
                    // The sidecar the harness copy gained is not taken back, so its entry stays.
                    let own = left.iter().find(|e| e.id == entry.id).expect("the adopted entry");
                    assert!(own.changed && own.path.join(ID_FILE).is_file(), "{case}");
                    // The repo version it replaced is the other entry left.
                    let backups: Vec<_> = left.iter().filter(|e| e.harness == "repo").collect();
                    assert_eq!(left.len(), 1 + backups.len(), "{case}");
                    if remove {
                        assert!(backups.is_empty(), "{case}");
                    } else {
                        assert_eq!((backups.len(), &backups[0].dest), (1, &personal), "{case}");
                        assert!(!backups[0].changed, "{case}");
                        assert_eq!(fs::read_to_string(backups[0].path.join("SKILL.md")).unwrap(), "Updated source.\n");
                    }
                }
            }
        }
    }

    #[test]
    fn displaced_content_is_pruned_after_the_grace_period_unless_it_changed() {
        let (tmp, repo, harnesses) = skill_setup(LinkMode::Copy);
        let _root = displaced::test_root(&tmp.path().join("state/displaced"));
        let assignment = Assignment::default();
        apply(&repo, &plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap()[0]).unwrap();
        assert!(displaced::list().unwrap().is_empty(), "a create displaces nothing");
        for version in ["v2\n", "v3\n"] {
            write(&repo.personal_dir().join("pdf/SKILL.md"), version);
            for a in plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap() {
                apply(&repo, &a).unwrap();
            }
        }
        let entries = displaced::list().unwrap();
        assert_eq!(entries.len(), 2, "{entries:?}");
        assert!(entries.iter().all(|e| !e.changed && e.harness == "h" && e.dest == tmp.path().join("skills/pdf")));
        assert!(displaced::changed().unwrap().is_empty());
        // Too young to prune: a writer may still hold one open.
        assert!(displaced::prune().unwrap().is_empty());

        // A late write to the older entry keeps it, however old.
        write(&entries[0].path.join("SKILL.md"), "Late.\n");
        assert_eq!(displaced::prune_older_than(Duration::ZERO).unwrap(), [entries[1].id.clone()]);
        let left = displaced::list().unwrap();
        assert_eq!(left.len(), 1);
        assert!(left[0].changed && left[0].id == entries[0].id);
        displaced::discard(&left[0].id).unwrap();
        assert!(displaced::list().unwrap().is_empty());
        assert!(displaced::discard("../escape").is_err());
    }

    #[test]
    fn crash_leftovers_are_ignored_by_planning_and_recovered() {
        for explicit in [true, false] {
            let (tmp, repo, mut harnesses) = skill_setup(LinkMode::Copy);
            let _root = displaced::test_root(&tmp.path().join("state/displaced"));
            let rules = tmp.path().join("rules");
            harnesses.get_mut("h").unwrap().instructions = Some(rules.to_string_lossy().into_owned());
            harnesses.get_mut("h").unwrap().instructions_format = InstructionsFormat::Rules;
            write(&repo.root().join("instructions/git.md"), "Use CC.\n");
            let assignment = Assignment::default();
            for a in plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap() {
                apply(&repo, &a).unwrap();
            }
            write(&repo.personal_dir().join("pdf/SKILL.md"), "Updated source.\n");

            // Killed between renames, by an older version (names next to the
            // skills) and by this one (in the scratch dir): the destination
            // was moved aside and the replacement never went in.
            let skills = tmp.path().join("skills");
            fs::rename(skills.join("pdf"), skills.join(".sk1ll-old-pdf")).unwrap();
            copy_dir(&repo.personal_dir().join("pdf"), &skills.join(".sk1ll-stage-pdf"), excluded_from_personal_skill)
                .unwrap();
            copy_dir(
                &repo.personal_dir().join("pdf"),
                &skills.join(".sk1ll-tmp/stage-pdf"),
                excluded_from_personal_skill,
            )
            .unwrap();
            copy_dir(&skills.join(".sk1ll-old-pdf"), &skills.join(".sk1ll-tmp/old-gone"), excluded_from_personal_skill)
                .unwrap();
            let mut index = load_index(&skills).unwrap();
            index.entries.insert("gone".into(), index.entries["pdf"].clone());
            save_index(&skills, &index).unwrap();
            write(&skills.join(".sk1ll-tmp/old-gone/SKILL.md"), "Edited before the crash.\n");
            fs::rename(rules.join("git.md"), rules.join(".sk1ll-old-git.md")).unwrap();
            write(&rules.join(".git.md.sk1ll-tmp"), "staged\n");
            let index_stage = skills.join(".sk1ll-state/staging/.index.json.sk1ll-tmp-1");
            write(&index_stage, "interrupted index write\n");

            let actions = plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap();
            let summary: Vec<_> = actions.iter().map(|a| (a.kind, a.dest.clone())).collect();
            assert_eq!(
                summary,
                [
                    (ActionKind::Create, skills.join("pdf")),
                    (ActionKind::Remove, skills.join("gone")),
                    (ActionKind::Create, rules.join("git.md")),
                ],
                "explicit={explicit}"
            );
            if explicit {
                crate::git::Git::new(repo.root()).args(["init", "--quiet"]).run().unwrap();
                let lock = crate::EngineLock::acquire(repo.root()).unwrap();
                let rescued = recover(Some(&lock), &harnesses).unwrap();
                assert_eq!(rescued.len(), 6, "{rescued:?}");
                assert!(rescued.iter().all(|entry| entry.changed), "{rescued:?}");
            }
            for a in &actions {
                apply(&repo, a).unwrap();
            }
            assert!(plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap().is_empty());
            assert_eq!(fs::read_to_string(skills.join("pdf/SKILL.md")).unwrap(), "Updated source.\n");
            let mut names: Vec<_> = fs::read_dir(&skills).unwrap().map(|e| e.unwrap().file_name()).collect();
            names.sort();
            assert_eq!(names, [INDEX_DIR, TMP_DIR, "pdf"], "explicit={explicit}");
            assert!(sk1ll_leftovers(&skills).is_empty(), "only recovery entries are kept");
            assert!(sk1ll_leftovers(&rules).is_empty());
            assert!(!index_stage.exists());
            let changed = displaced::changed().unwrap();
            assert_eq!(changed.len(), 6, "{changed:?}");
            let gone = changed.iter().find(|entry| entry.dest == skills.join("gone")).unwrap();
            assert_eq!(fs::read_to_string(gone.path.join("SKILL.md")).unwrap(), "Edited before the crash.\n");
            assert!(
                changed.iter().any(|entry| entry.dest == rules.join("git.md")
                    && fs::read_to_string(&entry.path).unwrap() == "staged\n")
            );
            assert!(displaced::prune_older_than(Duration::ZERO).unwrap().is_empty());
        }
    }

    /// What one displacement test works on: a harness set up and synced
    /// once, then a plan that displaces `dest`.
    struct Displacement {
        tmp: tempfile::TempDir,
        repo: Repo,
        harnesses: BTreeMap<String, Harness>,
        assignment: Assignment,
        /// The planned destination.
        dest: PathBuf,
        /// A file of the displaced content, that writers may hold open.
        file: PathBuf,
        /// The directory whose `.sk1ll-tmp/displaced` must hold the entry.
        holder: PathBuf,
    }

    impl Displacement {
        /// `case`: "skill update", "skill removal", "link removal", "rule
        /// update", "rule removal" or "block update".
        fn new(case: &str) -> Self {
            let (kind, change) = case.split_once(' ').unwrap();
            let remove = change == "removal";
            let mut assignment = Assignment::default();
            let synced = |repo: &Repo, harnesses: &BTreeMap<String, Harness>, assignment: &Assignment| {
                for a in plan(repo, harnesses, assignment, PlanOptions::default()).unwrap() {
                    apply(repo, &a).unwrap();
                }
            };
            match kind {
                "skill" | "link" => {
                    let mode = if kind == "link" { LinkMode::Symlink } else { LinkMode::Copy };
                    let (tmp, repo, harnesses) = skill_setup(mode);
                    synced(&repo, &harnesses, &assignment);
                    write(&repo.personal_dir().join("pdf/SKILL.md"), "Updated source.\n");
                    assignment.set_skill("h", &skill_id(&repo, "pdf"), !remove);
                    let dest = tmp.path().join("skills/pdf");
                    let (file, holder) = (dest.join("SKILL.md"), tmp.path().join("skills"));
                    Self { tmp, repo, harnesses, assignment, dest, file, holder }
                }
                _ => {
                    let format = if kind == "rule" { InstructionsFormat::Rules } else { InstructionsFormat::Block };
                    let (tmp, repo, harnesses, path) = instructions_setup(format);
                    synced(&repo, &harnesses, &assignment);
                    edit_rule_body(&repo.root().join("instructions/git.md"), "Use CC always.\n");
                    assignment.set_rule("h", &rule_id(&repo, "git"), !remove);
                    let dest = if kind == "rule" { path.join("git.md") } else { path.clone() };
                    let holder = dest.parent().unwrap().to_path_buf();
                    Self { tmp, repo, harnesses, assignment, file: dest.clone(), dest, holder }
                }
            }
        }

        fn action(&self) -> Action {
            let actions = plan(&self.repo, &self.harnesses, &self.assignment, PlanOptions::default()).unwrap();
            assert_eq!(actions.len(), 1, "{actions:?}");
            assert!(matches!(actions[0].kind, ActionKind::Update | ActionKind::Remove), "{actions:?}");
            actions.into_iter().next().unwrap()
        }

        fn area(&self) -> PathBuf {
            self.holder.join(TMP_DIR).join("displaced")
        }

        /// The one recovery entry for `dest`.
        fn entry(&self) -> displaced::Entry {
            let entries: Vec<_> = displaced::list().unwrap().into_iter().filter(|e| e.dest == self.dest).collect();
            assert_eq!(entries.len(), 1, "{entries:?}");
            entries.into_iter().next().unwrap()
        }

        /// The displaced copy of [`Self::file`] in `entry`.
        fn kept_file(&self, entry: &displaced::Entry) -> PathBuf {
            match self.file.strip_prefix(&self.dest) {
                Ok(rel) if !rel.as_os_str().is_empty() => entry.path.join(rel),
                _ => entry.path.clone(),
            }
        }
    }

    /// The recovery copy is the displaced original itself (renamed, or for
    /// files hard-linked, never copied), so a writer that held the old file
    /// open keeps writing into it after sync is done, wherever the state
    /// directory is. An owned block rewrite keeps no copy (see
    /// `rewriting_an_owned_block_leaves_no_recovery_entry`).
    #[cfg(unix)]
    #[test]
    fn late_writes_after_sync_reach_the_recovery_copy_wherever_the_state_dir_is() {
        use std::io::Write;
        for case in ["skill update", "skill removal", "rule update", "rule removal"] {
            for state_elsewhere in [false, true] {
                let d = Displacement::new(case);
                let _root = displaced::test_root(&d.tmp.path().join("state/displaced"));
                let action = d.action();
                let mut open = fs::OpenOptions::new().append(true).open(&d.file).unwrap();
                displaced::STATE_ON_OTHER_FS.set(state_elsewhere);
                apply(&d.repo, &action).unwrap();
                open.write_all(b"Written after sync returned.\n").unwrap();
                drop(open);
                let entry = d.entry();
                let kept = fs::read_to_string(d.kept_file(&entry)).unwrap();
                assert!(kept.ends_with("Written after sync returned.\n"), "{case} {state_elsewhere}: {kept}");
                assert!(entry.changed, "{case} {state_elsewhere}");
                assert!(entry.path.starts_with(d.area()), "{case} {state_elsewhere}: {}", entry.path.display());
            }
        }
    }

    /// A create never replaces what appeared at the destination after the
    /// last check: it is a conflict, the newcomer stays and ours stays out.
    #[test]
    fn a_create_never_replaces_what_appeared_after_the_last_check() {
        for case in ["rule", "block", "link"] {
            let (tmp, repo, harnesses, dest) = match case {
                "link" => {
                    let (tmp, repo, harnesses) = skill_setup(LinkMode::Symlink);
                    let dest = tmp.path().join("skills/pdf");
                    (tmp, repo, harnesses, dest)
                }
                _ => {
                    let format = if case == "rule" { InstructionsFormat::Rules } else { InstructionsFormat::Block };
                    let (tmp, repo, harnesses, path) = instructions_setup(format);
                    let dest = if case == "rule" { path.join("git.md") } else { path };
                    (tmp, repo, harnesses, dest)
                }
            };
            let _root = displaced::test_root(&tmp.path().join("state/displaced"));
            let assignment = Assignment::default();
            let actions = plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap();
            assert_eq!(actions[0].kind, ActionKind::Create, "{case}: {actions:?}");
            let newcomer = |dest: &Path| write(dest, "Newcomer.\n");
            let result = with_hook(Hook::AfterCheck, newcomer, || apply(&repo, &actions[0]));
            assert!(result.is_err(), "{case}: {result:?}");
            assert!(fs::symlink_metadata(&dest).unwrap().is_file(), "{case}");
            assert_eq!(fs::read_to_string(&dest).unwrap(), "Newcomer.\n", "{case}");
            let parent = dest.parent().unwrap();
            assert!(sk1ll_leftovers(parent).is_empty(), "{case}: {:?}", sk1ll_leftovers(parent));
            if case != "block" {
                let actions = plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap();
                assert_eq!(actions[0].kind, ActionKind::Conflict, "{case}: {actions:?}");
            }
        }
    }

    /// Runs `f` with `edit` hooked in at every point.
    fn with_hooks<T>(edit: impl FnMut(Hook, &Path) + 'static, f: impl FnOnce() -> T) -> T {
        HOOK.with_borrow_mut(|h| *h = Some(Box::new(edit)));
        let out = f();
        HOOK.with_borrow_mut(|h| *h = None);
        out
    }

    /// When a swap is called off, the displaced content only goes back if
    /// the destination is still free. Otherwise it stays in recovery, marked
    /// changed (never pruned), and whatever appeared is left alone.
    #[cfg(unix)]
    #[test]
    fn displaced_content_never_goes_back_over_what_appeared_meanwhile() {
        use std::io::Write;
        for case in ["rule removal", "skill removal", "link removal", "skill update"] {
            let d = Displacement::new(case);
            let _root = displaced::test_root(&d.tmp.path().join("state/displaced"));
            let action = d.action();
            // A removal is called off by an edit made right before the content
            // is moved into recovery; the update fails on the newcomer itself.
            let mut open = fs::OpenOptions::new().append(true).open(&d.file).ok();
            let hooks = move |point: Hook, dest: &Path| match point {
                Hook::BeforeSwap if case.starts_with("link") => {
                    fs::remove_file(dest).unwrap();
                    std::os::unix::fs::symlink("/elsewhere", dest).unwrap();
                }
                Hook::BeforeSwap if case.ends_with("removal") => {
                    open.take().unwrap().write_all(b"Late edit.\n").unwrap()
                }
                Hook::AfterAside => write(dest, "Newcomer.\n"),
                _ => {}
            };
            let entry = reported(with_hooks(hooks, || apply(&d.repo, &action)));
            assert_eq!(fs::read_to_string(&d.dest).unwrap(), "Newcomer.\n", "{case}");
            assert!(entry.path.starts_with(d.area()), "{case}");
            match case {
                "rule removal" => assert_eq!(fs::read_to_string(&entry.path).unwrap(), "Use CC.\nLate edit.\n"),
                "link removal" => assert_eq!(fs::read_link(&entry.path).unwrap(), Path::new("/elsewhere")),
                "skill removal" => assert_eq!(fs::read_to_string(d.kept_file(&entry)).unwrap(), "PDFs.\nLate edit.\n"),
                _ => assert_eq!(fs::read_to_string(d.kept_file(&entry)).unwrap(), "PDFs.\n", "intact, yet kept"),
            }
            // Kept for good, however old.
            assert!(!displaced::prune_older_than(Duration::ZERO).unwrap().contains(&entry.id), "{case}");
            assert!(displaced::get(&entry.id).unwrap().changed, "{case}");
            assert!(!d.holder.join(TMP_DIR).join("stage-pdf").exists(), "{case}");
        }
    }

    /// Adopting a block file entry would recompute the current managed
    /// block's marker, so a hand edit inside it would stop being a conflict
    /// and be overwritten by the next sync: refused instead.
    #[cfg(unix)]
    #[test]
    fn adopting_a_block_entry_is_refused_while_the_managed_block_is_edited_by_hand() {
        use std::io::Write;
        let d = Displacement::new("block update");
        let _root = displaced::test_root(&d.tmp.path().join("state/displaced"));
        let action = d.action();
        let mut open = fs::OpenOptions::new().append(true).open(&d.dest).unwrap();
        let late = move |_: &Path| open.write_all(b"Outside note.\n").unwrap();
        let entry = reported(with_hook(Hook::AfterSwap, late, || apply(&d.repo, &action)));
        let synced = fs::read_to_string(&d.dest).unwrap();
        let edited = synced.replace("Use CC always.", "Use CC always, edited by hand.");
        assert_ne!(edited, synced);
        write(&d.dest, &edited);
        let conflict = |d: &Displacement| {
            let actions = plan(&d.repo, &d.harnesses, &d.assignment, PlanOptions::default()).unwrap();
            actions.len() == 1 && actions[0].kind == ActionKind::Conflict
        };
        assert!(conflict(&d));

        let err = adopt_test_entry(&d.repo, &entry.id).unwrap_err();
        assert!(matches!(&err, Error::Invalid(m) if m.contains("edited by hand")), "{err}");
        assert_eq!(fs::read_to_string(&d.dest).unwrap(), edited);
        assert!(displaced::get(&entry.id).unwrap().changed);
        assert!(conflict(&d), "the hand edit is still a conflict");
    }

    /// Unchanged entries go after the grace period, or before it while all
    /// entries take more than the cap, oldest first; changed ones never.
    #[test]
    fn displaced_content_over_the_size_cap_goes_oldest_unchanged_first() {
        let (tmp, repo, harnesses) = skill_setup(LinkMode::Copy);
        let _root = displaced::test_root(&tmp.path().join("state/displaced"));
        let assignment = Assignment::default();
        let asset = "x".repeat(64 << 10);
        write(&repo.personal_dir().join("pdf/asset.bin"), &asset);
        let sync = || {
            for a in plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap() {
                apply(&repo, &a).unwrap();
            }
        };
        sync();
        for version in ["v2\n", "v3\n", "v4\n"] {
            write(&repo.personal_dir().join("pdf/SKILL.md"), version);
            sync();
        }
        let entries = displaced::list().unwrap();
        assert_eq!(entries.len(), 3, "{entries:?}");
        write(&entries[0].path.join("SKILL.md"), "Late.\n");
        // All young: nothing goes while under the cap.
        assert!(displaced::prune_with(displaced::GRACE, 1 << 30).unwrap().is_empty());
        // Room for two of three: the oldest unchanged one goes, not the changed one.
        let cap = 3 * (64 << 10) - (32 << 10);
        assert_eq!(displaced::prune_with(displaced::GRACE, cap).unwrap(), [entries[1].id.clone()]);
        // No room at all: every unchanged one goes; the changed one stays.
        assert_eq!(displaced::prune_with(displaced::GRACE, 0).unwrap(), [entries[2].id.clone()]);
        let left = displaced::list().unwrap();
        assert_eq!(left.len(), 1);
        assert!(left[0].changed && left[0].id == entries[0].id);
    }

    /// Displacing is a rename or link within the destination's directory. If
    /// that fails anyway (a mount point in between), the swap is called off
    /// with the original untouched in place: there is no copy to fall back
    /// on, and nothing is ever deleted.
    #[cfg(unix)]
    #[test]
    fn displacing_fails_closed_if_the_move_crosses_filesystems() {
        use std::os::unix::fs::MetadataExt;
        for case in ["skill update", "skill removal", "rule update", "rule removal", "block update"] {
            let d = Displacement::new(case);
            let _root = displaced::test_root(&d.tmp.path().join("state/displaced"));
            let action = d.action();
            let (before, inode) = (fs::read(&d.file).unwrap(), fs::metadata(&d.file).unwrap().ino());
            displaced::FORCE_EXDEV.set(true);
            let result = apply(&d.repo, &action);
            assert!(result.is_err(), "{case}: {result:?}");
            assert_eq!(fs::read(&d.file).unwrap(), before, "{case}");
            assert_eq!(fs::metadata(&d.file).unwrap().ino(), inode, "{case}: the original itself, in place");
            assert!(displaced::list().unwrap().is_empty(), "{case}");
            assert!(sk1ll_leftovers(&d.holder).is_empty(), "{case}: {:?}", sk1ll_leftovers(&d.holder));
            assert!(!d.holder.join(TMP_DIR).exists(), "{case}: tidied away");
        }
    }

    /// Entries are found through the state directory's index wherever they
    /// are, the index is rebuilt by recovery if it is lost, entries older
    /// versions kept in the state directory are still found, and the index
    /// can't point anywhere but into a recovery area.
    #[test]
    fn recovery_entries_are_found_wherever_they_are() {
        let d = Displacement::new("rule update");
        let state = d.tmp.path().join("state/displaced");
        let _root = displaced::test_root(&state);
        apply(&d.repo, &d.action()).unwrap();
        let entry = d.entry();
        assert!(entry.path.starts_with(d.area()) && !entry.changed);
        // A harness that loads every `.md` below its rules dir finds none here.
        let md: Vec<_> = WalkDir::new(d.holder.join(TMP_DIR))
            .into_iter()
            .map(|e| e.unwrap().into_path())
            .filter(|p| p.extension().is_some_and(|x| x == "md"))
            .collect();
        assert!(md.is_empty(), "{md:?}");
        assert_eq!(fs::read_to_string(d.area().join(".gitignore")).unwrap(), "*\n");
        // The rules dir's own planning ignores it.
        assert!(plan(&d.repo, &d.harnesses, &d.assignment, PlanOptions::default()).unwrap().is_empty());

        // An entry an older version kept in the state directory itself.
        let legacy = state.join("0000000000001-h-old.md");
        write(&legacy.join("content"), "Old rule.\n");
        let meta = format!(
            r#"{{"version":3,"harness":"h","kind":"rule","dest":{:?},"expected":"","displaced_at":0}}"#,
            d.holder.join("old.md")
        );
        write(&legacy.join("entry.json"), &meta);
        let ids = |list: Vec<displaced::Entry>| list.into_iter().map(|e| e.id).collect::<Vec<_>>();
        assert_eq!(ids(displaced::list().unwrap()), ["0000000000001-h-old.md", entry.id.as_str()]);

        // The index is lost: recovery lists the harness's entries again.
        fs::remove_dir_all(state.join(".index")).unwrap();
        assert_eq!(ids(displaced::list().unwrap()), ["0000000000001-h-old.md"]);
        crate::git::Git::new(d.repo.root()).args(["init", "--quiet"]).run().unwrap();
        let lock = crate::EngineLock::acquire(d.repo.root()).unwrap();
        recover(Some(&lock), &d.harnesses).unwrap();
        assert_eq!(displaced::list().unwrap(), [displaced::get("0000000000001-h-old.md").unwrap(), entry.clone()]);

        // A record pointing anywhere else is never followed.
        let victim = d.tmp.path().join("victim");
        write(&victim.join("content"), "Not sk1ll's.\n");
        write(&victim.join("entry.json"), &meta);
        write(&state.join(".index/victim"), &serde_json::to_string(&victim).unwrap());
        assert!(displaced::get("victim").is_err() && displaced::discard("victim").is_err());
        assert!(victim.join("content").exists());

        displaced::discard(&entry.id).unwrap();
        displaced::discard("0000000000001-h-old.md").unwrap();
        assert!(!d.holder.join(TMP_DIR).exists(), "the empty area is tidied away");
        assert!(!state.join(".index").join(&entry.id).exists());
    }

    /// A copied skill edited in the harness, and the conflict to adopt it.
    fn adoptable(tmp: &tempfile::TempDir, repo: &Repo, harnesses: &BTreeMap<String, Harness>) -> (PathBuf, Action) {
        let assignment = Assignment::default();
        apply(repo, &plan(repo, harnesses, &assignment, PlanOptions::default()).unwrap()[0]).unwrap();
        let dest = tmp.path().join("skills/pdf");
        write(&dest.join("SKILL.md"), "Edited in the harness.\n");
        let actions = plan(repo, harnesses, &assignment, PlanOptions::default()).unwrap();
        assert_eq!(actions[0].kind, ActionKind::Conflict);
        (dest, actions.into_iter().next().unwrap())
    }

    /// Adopt follows the Item ID a placement was recorded with: an edit to
    /// the harness copy of a skill renamed since goes into the renamed
    /// skill, never into a new one of the old name. An ID that leads to no
    /// item, or to two, is refused, naming what it found.
    #[test]
    fn adopt_finds_a_renamed_target_by_its_item_id() {
        let (tmp, repo, harnesses) = skill_setup(LinkMode::Copy);
        let _root = displaced::test_root(&tmp.path().join("recovery"));
        let assignment = Assignment::default();
        apply(&repo, &plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap()[0]).unwrap();
        let personal = repo.personal_dir();
        fs::rename(personal.join("pdf"), personal.join("docs")).unwrap();
        write(&tmp.path().join("skills/pdf/SKILL.md"), "Edited in the harness.\n");
        let conflict = || {
            let actions = plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap();
            actions.into_iter().find(|a| a.kind == ActionKind::Conflict && a.dest.ends_with("pdf")).unwrap()
        };
        let pending = conflict();

        // Another item of the old name, and later a copy that shares the ID.
        write(&personal.join("pdf/SKILL.md"), "Another pdf.\n");
        repo.assign_missing_ids().unwrap();
        fs::rename(personal.join("docs"), tmp.path().join("docs-aside")).unwrap();
        let error = adopt(&repo, &pending).unwrap_err().to_string();
        assert!(error.contains("no skill in the repo has now") && error.contains("another Item ID"), "{error}");
        fs::rename(tmp.path().join("docs-aside"), personal.join("docs")).unwrap();
        fs::create_dir(personal.join("copy")).unwrap();
        fs::copy(personal.join("docs/SKILL.md"), personal.join("copy/SKILL.md")).unwrap();
        fs::copy(personal.join("docs/.sk1ll-id"), personal.join("copy/.sk1ll-id")).unwrap();
        let error = adopt(&repo, &pending).unwrap_err().to_string();
        assert!(error.contains("personal/copy") && error.contains("personal/docs"), "{error}");
        fs::remove_dir_all(personal.join("copy")).unwrap();

        adopt(&repo, &conflict()).unwrap();
        assert_eq!(fs::read_to_string(personal.join("docs/SKILL.md")).unwrap(), "Edited in the harness.\n");
        assert_eq!(fs::read_to_string(personal.join("pdf/SKILL.md")).unwrap(), "Another pdf.\n");
    }

    /// Adopting replaces the repo version through sync's swap: an edit to
    /// it before the swap calls the adoption off, one during the swap is
    /// reported and kept in recovery, and an ordinary replacement is kept
    /// there until it is pruned.
    #[cfg(unix)]
    #[test]
    fn adopting_swaps_the_repo_version_into_recovery() {
        use std::io::Write;
        for point in [None, Some(Hook::BeforeSwap), Some(Hook::AfterAside)] {
            let (tmp, repo, harnesses) = skill_setup(LinkMode::Copy);
            let _root = displaced::test_root(&tmp.path().join("recovery"));
            let (dest, conflict) = adoptable(&tmp, &repo, &harnesses);
            let source = repo.personal_dir().join("pdf/SKILL.md");
            let mut open = fs::OpenOptions::new().append(true).open(&source).unwrap();
            let edit = move |_: &Path| open.write_all(b"Repo edit.\n").unwrap();
            let result = match point {
                Some(point) => with_hook(point, edit, || adopt(&repo, &conflict)),
                None => adopt(&repo, &conflict),
            };
            let stages: Vec<_> = read_dir_names(&repo.personal_dir())
                .unwrap()
                .into_iter()
                .filter(|n| n.starts_with(crate::source::STAGE_PREFIX))
                .collect();
            assert!(stages.is_empty(), "{point:?}: {stages:?}");
            match point {
                Some(Hook::BeforeSwap) => {
                    let expected = changed_since_plan(&repo.personal_dir().join("pdf"));
                    assert_eq!(result.unwrap_err().to_string(), expected.to_string());
                    assert_eq!(fs::read_to_string(&source).unwrap(), "PDFs.\nRepo edit.\n");
                    assert!(displaced::list().unwrap().is_empty());
                    let again = plan(&repo, &harnesses, &Assignment::default(), PlanOptions::default()).unwrap();
                    assert_eq!(again[0].kind, ActionKind::Conflict);
                }
                Some(_) => {
                    let entry = reported(result);
                    assert_eq!((entry.harness.as_str(), &entry.dest), (REPO_HOLDER, &repo.personal_dir().join("pdf")));
                    assert_eq!(fs::read_to_string(entry.path.join("SKILL.md")).unwrap(), "PDFs.\nRepo edit.\n");
                    assert_eq!(fs::read_to_string(&source).unwrap(), "Edited in the harness.\n");
                    assert!(displaced::prune_older_than(Duration::ZERO).unwrap().is_empty());
                }
                None => {
                    result.unwrap();
                    assert_eq!(fs::read_to_string(&source).unwrap(), "Edited in the harness.\n");
                    let entry = displaced::list().unwrap().remove(0);
                    assert!(!entry.changed && entry.harness == REPO_HOLDER);
                    assert_eq!(fs::read_to_string(entry.path.join("SKILL.md")).unwrap(), "PDFs.\n");
                    assert_eq!(displaced::prune_older_than(Duration::ZERO).unwrap(), [entry.id]);
                    assert!(!repo.personal_dir().join(TMP_DIR).exists());
                }
            }
            if point != Some(Hook::BeforeSwap) {
                assert!(plan(&repo, &harnesses, &Assignment::default(), PlanOptions::default()).unwrap().is_empty());
                assert_eq!(fs::read_to_string(dest.join("SKILL.md")).unwrap(), "Edited in the harness.\n");
            }
        }
    }

    /// A crash after the repo version was moved aside leaves it in recovery,
    /// listed and never pruned, and the adoption can simply be retried.
    #[test]
    fn an_adoption_interrupted_mid_swap_keeps_the_repo_version_in_recovery() {
        let (tmp, repo, harnesses) = skill_setup(LinkMode::Copy);
        let _root = displaced::test_root(&tmp.path().join("recovery"));
        let (dest, conflict) = adoptable(&tmp, &repo, &harnesses);
        let failure = std::panic::catch_unwind(|| {
            with_hook(Hook::AfterRename, |_| std::panic::panic_any("interrupted adopt"), || adopt(&repo, &conflict))
        })
        .expect_err("adopt reached the interruption point");
        assert_eq!(failure.downcast_ref::<&str>(), Some(&"interrupted adopt"));
        let source = repo.personal_dir().join("pdf");
        assert!(fs::symlink_metadata(&source).is_err());
        let entry = displaced::list().unwrap().remove(0);
        assert_eq!((entry.harness.as_str(), &entry.dest), (REPO_HOLDER, &source));
        assert_eq!(fs::read_to_string(entry.path.join("SKILL.md")).unwrap(), "PDFs.\n");
        assert!(displaced::prune_older_than(Duration::ZERO).unwrap().is_empty());
        adopt(&repo, &conflict).unwrap();
        assert_eq!(fs::read_to_string(source.join("SKILL.md")).unwrap(), "Edited in the harness.\n");
        assert_eq!(fs::read_to_string(dest.join("SKILL.md")).unwrap(), "Edited in the harness.\n");
        assert_eq!(displaced::get(&entry.id).unwrap().path, entry.path);
    }

    /// Adopting a rule file replaces the repo rule through the same swap.
    #[test]
    fn adopting_a_rule_swaps_the_repo_rule_into_recovery() {
        for edit_repo in [false, true] {
            let (tmp, repo, harnesses, path) = instructions_setup(InstructionsFormat::Rules);
            let _root = displaced::test_root(&tmp.path().join("recovery"));
            let assignment = Assignment::default();
            for a in plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap() {
                apply(&repo, &a).unwrap();
            }
            write(&path.join("git.md"), "Edited in the harness.\n");
            let conflict = plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap().remove(0);
            assert_eq!(conflict.kind, ActionKind::Conflict);
            let rule = repo.root().join("instructions/git.md");
            let before = fs::read_to_string(&rule).unwrap();
            let result = if edit_repo {
                let at = rule.clone();
                with_hook(Hook::BeforeSwap, move |_| write(&at, "Repo edit.\n"), || adopt(&repo, &conflict))
            } else {
                adopt(&repo, &conflict)
            };
            if edit_repo {
                assert_eq!(result.unwrap_err().to_string(), changed_since_plan(&rule).to_string());
                assert_eq!(fs::read_to_string(&rule).unwrap(), "Repo edit.\n");
                assert!(displaced::list().unwrap().is_empty());
            } else {
                result.unwrap();
                assert!(fs::read_to_string(&rule).unwrap().ends_with("Edited in the harness.\n"));
                let entry = displaced::list().unwrap().remove(0);
                assert_eq!((entry.harness.as_str(), entry.kind), (REPO_HOLDER, displaced::Kind::Rule));
                assert_eq!(fs::read_to_string(&entry.path).unwrap(), before);
            }
            let leftovers: Vec<_> = read_dir_names(&rule.with_file_name(""))
                .unwrap()
                .into_iter()
                .filter(|n| n.starts_with(crate::source::STAGE_PREFIX))
                .collect();
            assert!(leftovers.is_empty(), "{leftovers:?}");
        }
    }

    /// Adopt owns only what it copied. The harness copy's `.git` is left
    /// out of the copy and of the recorded fingerprint, so the copy stays a
    /// conflict and a forced removal keeps it in recovery for good. The
    /// repo version's `.git` is kept in recovery for good too. An edit
    /// while the copy is made calls the adoption off.
    #[test]
    fn adopt_owns_only_what_it_copied_and_keeps_what_it_left_out() {
        let (tmp, repo, harnesses) = skill_setup(LinkMode::Copy);
        let _root = displaced::test_root(&tmp.path().join("recovery"));
        let (dest, conflict) = adoptable(&tmp, &repo, &harnesses);
        write(&dest.join(".git/HEAD"), "harness git\n");
        let source = repo.personal_dir().join("pdf");
        write(&source.join(".git/HEAD"), "repo git\n");

        let at = dest.clone();
        let during =
            with_hook(Hook::CopyEntry, move |_| write(&at.join("late.md"), "Late.\n"), || adopt(&repo, &conflict));
        assert!(matches!(during, Err(Error::Changed(_))), "{during:?}");
        assert_eq!(fs::read_to_string(source.join("SKILL.md")).unwrap(), "PDFs.\n");
        assert!(displaced::list().unwrap().is_empty());
        fs::remove_file(dest.join("late.md")).unwrap();

        adopt(&repo, &conflict).unwrap();
        assert_eq!(fs::read_to_string(source.join("SKILL.md")).unwrap(), "Edited in the harness.\n");
        assert!(!source.join(".git").exists());
        assert_eq!(fs::read_to_string(dest.join(".git/HEAD")).unwrap(), "harness git\n");
        let repo_entry = displaced::list().unwrap().remove(0);
        assert!(repo_entry.changed && repo_entry.harness == REPO_HOLDER, "{repo_entry:?}");
        assert_eq!(fs::read_to_string(repo_entry.path.join(".git/HEAD")).unwrap(), "repo git\n");

        let mut assignment = Assignment::default();
        let normal = plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap();
        assert_eq!(normal[0].kind, ActionKind::Conflict, "{normal:?}");
        assignment.set_skill("h", &skill_id(&repo, "pdf"), false);
        let removal = plan(&repo, &harnesses, &assignment, PlanOptions { force: true }).unwrap();
        assert_eq!(removal[0].kind, ActionKind::Remove);
        apply(&repo, &removal[0]).unwrap();
        assert!(fs::symlink_metadata(&dest).is_err());
        let entry = displaced::list().unwrap().into_iter().find(|e| e.dest == dest).unwrap();
        assert!(entry.changed);
        assert!(displaced::prune_older_than(Duration::ZERO).unwrap().is_empty());
        assert_eq!(fs::read_to_string(entry.path.join(".git/HEAD")).unwrap(), "harness git\n");
        assert_eq!(fs::read_to_string(repo_entry.path.join(".git/HEAD")).unwrap(), "repo git\n");
    }

    /// `recover adopt` deletes the entry only if it held nothing but what
    /// went back into the repo.
    #[test]
    fn recover_adopt_keeps_an_entry_whose_copy_left_something_out() {
        // Set first: the state directory also holds the records of what was placed.
        let state = tempfile::tempdir().unwrap();
        let _root = displaced::test_root(state.path());
        let d = Displacement::new("skill update");
        write(&d.dest.join(".git/HEAD"), "git\n");
        let forced = plan(&d.repo, &d.harnesses, &d.assignment, PlanOptions { force: true }).unwrap();
        apply(&d.repo, &forced[0]).unwrap();
        let entry = d.entry();
        assert!(entry.changed);
        adopt_test_entry(&d.repo, &entry.id).unwrap();
        assert_eq!(fs::read_to_string(d.repo.personal_dir().join("pdf/SKILL.md")).unwrap(), "PDFs.\n");
        let kept = displaced::get(&entry.id).unwrap();
        assert!(kept.changed);
        assert_eq!(fs::read_to_string(kept.path.join(".git/HEAD")).unwrap(), "git\n");
        assert_eq!(fs::read_to_string(kept.path.join("SKILL.md")).unwrap(), "PDFs.\n");
        assert!(!displaced::prune_older_than(Duration::ZERO).unwrap().contains(&entry.id));
        assert!(kept.path.join(".git/HEAD").exists());
    }

    /// Pruning unlinks only files that still match their fingerprint and
    /// removes directories only while empty: whatever appears or changes
    /// after the last check stays, and keeps its entry, marked changed.
    #[test]
    fn pruning_never_deletes_recursively() {
        for case in ["new file", "new directory", "edited file", "renamed away"] {
            let d = Displacement::new("skill update");
            let _root = displaced::test_root(&d.tmp.path().join("recovery"));
            apply(&d.repo, &d.action()).unwrap();
            let entry = d.entry();
            assert!(!entry.changed);
            let content = entry.path.clone();
            let outside = d.tmp.path().join("moved.md");
            let target = outside.clone();
            let pruned = crate::hash::tests::with_removal_hook(
                move |path| match case {
                    "new file" => write(&path.join("new.md"), "New.\n"),
                    "new directory" => write(&path.join("new/deeper.md"), "New.\n"),
                    "edited file" => write(&path.join("SKILL.md"), "Edited.\n"),
                    "renamed away" => fs::rename(path.join("SKILL.md"), &target).unwrap(),
                    _ => unreachable!(),
                },
                || displaced::prune_older_than(Duration::ZERO),
            )
            .unwrap();
            match case {
                "renamed away" => {
                    assert_eq!(pruned, std::slice::from_ref(&entry.id), "{case}");
                    assert!(displaced::list().unwrap().is_empty(), "{case}");
                    assert_eq!(fs::read_to_string(&outside).unwrap(), "PDFs.\n");
                }
                _ => {
                    assert!(!pruned.contains(&entry.id), "{case}");
                    assert!(displaced::get(&entry.id).unwrap().changed, "{case}");
                    let kept = match case {
                        "new file" => ("new.md", "New.\n"),
                        "new directory" => ("new/deeper.md", "New.\n"),
                        _ => ("SKILL.md", "Edited.\n"),
                    };
                    assert_eq!(fs::read_to_string(content.join(kept.0)).unwrap(), kept.1, "{case}");
                }
            }
        }
    }

    /// A forced update of a hand-edited skill link keeps the edited link.
    #[cfg(unix)]
    #[test]
    fn a_forced_update_of_an_edited_link_keeps_a_backup() {
        let (tmp, repo, harnesses) = skill_setup(LinkMode::Symlink);
        let _root = displaced::test_root(&tmp.path().join("recovery"));
        let assignment = Assignment::default();
        apply(&repo, &plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap()[0]).unwrap();
        let dest = tmp.path().join("skills/pdf");
        let placed = fs::read_link(&dest).unwrap();
        fs::remove_file(&dest).unwrap();
        std::os::unix::fs::symlink("../repo/personal/pdf", &dest).unwrap();
        let normal = plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap();
        assert_eq!(normal[0].kind, ActionKind::Conflict, "{normal:?}");
        let forced = plan(&repo, &harnesses, &assignment, PlanOptions { force: true }).unwrap();
        assert_eq!(forced[0].kind, ActionKind::Update);
        apply(&repo, &forced[0]).unwrap();
        assert_eq!(fs::read_link(&dest).unwrap(), placed);
        let entry = displaced::list().unwrap().remove(0);
        assert!(entry.changed && entry.dest == dest, "{entry:?}");
        assert_eq!(fs::read_link(&entry.path).unwrap(), Path::new("../repo/personal/pdf"));
        assert!(plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap().is_empty());
    }
}
