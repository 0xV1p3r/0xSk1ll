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
                crate::failpoint!(PlanSource, &dest);
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
        adopting: None,
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
        adopting: None,
    };
    let spliced = instructions::splice(&text, block.as_ref(), content);
    // A file sync created stays sync's only while it holds exactly what
    // sync last wrote: anything else (an edit anywhere, a file replaced)
    // makes it the user's for good, and it is never removed. The one
    // exception is a forced write over an edit made only inside the block
    // (`block_edited` below).
    let created = displaced::created_block(dest)?;
    let owned = match &created {
        Some(created) => existing.is_some() && !is_symlink(dest) && created.is(dest)?,
        None => false,
    };
    // Still the very file sync created, with nothing at all outside its
    // block, whose block was edited by hand: forcing the block over that
    // edit (kept in recovery) leaves nothing of anyone else's in the file,
    // so it stays sync's, and a forced take-out removes the file.
    let block_edited = match &created {
        Some(created) => {
            !owned
                && existing.is_some()
                && !is_symlink(dest)
                && block.as_ref().is_some_and(|b| b.edited() && b.is_whole(&text))
                && created.same_file(dest)?
        }
        None => false,
    };
    if let (None, Some(created)) = (content, &created) {
        if owned && spliced.trim().is_empty() {
            return remove_created_block(repo, &action.harness, dest, created, None, unchanged);
        }
        if block_edited {
            return remove_created_block(repo, &action.harness, dest, created, Some(&whole), unchanged);
        }
    }
    if created.is_some() && !owned {
        displaced::record_created_block(dest, None)?;
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
    if (existing.is_none() || owned || block_edited) && !is_symlink(dest) {
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
///
/// `edited`: the hash of the whole text, for a forced take-out of the file
/// sync created whose block was edited by hand: it must still be that very
/// file with that text, and its recovery entry keeps the edit for good.
fn remove_created_block(
    repo: &Repo,
    harness: &str,
    dest: &Path,
    created: &displaced::CreatedBlock,
    edited: Option<&str>,
    unchanged: impl FnOnce() -> Result<()>,
) -> Result<()> {
    let holder = dest.parent().ok_or_else(|| Error::Invalid(format!("{} has no parent", dest.display())))?;
    let _lock = DirLock::acquire(holder)?;
    unchanged()?;
    let kind = displaced::Kind::Instructions;
    let ours = |path: &Path| match edited {
        None => created.is(path),
        Some(whole) => Ok(created.same_file(path)? && displaced::fingerprint(kind, path)?.as_deref() == Some(whole)),
    };
    if !ours(dest)? {
        return Err(changed_since_plan(dest));
    }
    // An unedited backup holds only what sync wrote: pruned once unchanged and old enough.
    let (expected, payload) = match edited {
        None => (created.written.as_str(), Some(created.payload.as_str())),
        Some(whole) => (whole, None),
    };
    let mut held = displaced::Displacing::begin(Some(repo), kind, harness, dest, Some(expected), payload, holder)?;
    crate::failpoint!(BeforeSwap, dest);
    if let Err(e) = held.take(dest) {
        held.abandon();
        return Err(e);
    }
    crate::failpoint!(AfterAside, dest);
    if !ours(&held.content())? {
        held.put_back()?;
        return Err(changed_since_plan(dest));
    }
    let displaced = match edited {
        None => held.finish(false)?,
        Some(_) => held.finish_kept().map(|()| None)?,
    };
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
        adopting: match &existing {
            Existing::Unmanaged(_) => source.map(|source| (source, item_id.as_ref(), holds_uncopied(dest))),
            _ => None,
        },
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
                crate::failpoint!(BeforeSwap, dest);
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
                    crate::failpoint!(BeforeCopy, src);
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
    let recorded = match (&held, displace.adopting) {
        (Some(entry), Some((source, id, uncopied))) => entry.record_source(displace.repo, source, id, uncopied),
        _ => Ok(()),
    };
    if let Err(e) = recorded {
        if let Some(held) = held {
            held.abandon();
        }
        return Err(e);
    }
    let setup = || -> Result<()> {
        if let (Some(ownership), Some(intent)) = (displace.ownership, intent) {
            ownership.begin(intent.with_recovery(held.as_ref().map(|entry| entry.id().to_owned())))?;
        }
        crate::failpoint!(BeforeSwap, dest);
        if stage.is_some() && displaced::fingerprint(displace.kind, dest)?.as_deref() != displace.expected {
            return Err(changed_since_plan(dest));
        }
        crate::failpoint!(AfterCheck, dest);
        displace.check_ownership()
    };
    if let Err(e) = setup() {
        if let Some(held) = held {
            held.abandon();
        }
        return Err(e);
    }
    let backup = held.as_ref().map(displaced::Displacing::content);
    if let Some(error) = backup.as_deref().and_then(|backup| crate::failpoint_error!(RecoveryRename, backup)) {
        if let Some(held) = held {
            held.abandon();
        }
        return Err(error).at(dest);
    }
    let swapped = crate::placement::swap_managed(stage, dest, backup.as_deref(), || {
        if let Some(held) = &mut held {
            crate::failpoint!(AfterRename, dest);
            held.mark_moved(dest)?;
            let intact = held.intact();
            crate::failpoint!(AfterAside, dest);
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
    crate::failpoint!(AfterSwap, dest);
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
    /// Adopt replaces an unmanaged skill with the repo item it copied it
    /// into (its source and Item ID): sync placed nothing there, so the
    /// recovery entry records that item, which its Adopt goes back into.
    /// `true`: the skill holds what the copy left out (see [`holds_uncopied`]).
    adopting: Option<(&'a Path, Option<&'a ItemId>, bool)>,
}

/// Whether the skill directory at `path` holds anything a copy of it into
/// the repo leaves out (a `.git`, with branches and stashes that may be
/// nowhere else), so the original is the only full copy. An Item ID sidecar
/// only names an item. A link holds nothing: what it points to stays.
fn holds_uncopied(path: &Path) -> bool {
    if fs::symlink_metadata(path).map_or(true, |m| m.file_type().is_symlink()) {
        return false;
    }
    WalkDir::new(path)
        .min_depth(1)
        .into_iter()
        .any(|entry| entry.map_or(true, |e| excluded_from_skill(e.depth(), e.file_name())))
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
    crate::failpoint!(BeforeSwap, path);
    if let Err(e) = unchanged() {
        return fail(e);
    }
    crate::failpoint!(AfterCheck, path);
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
    crate::failpoint!(AfterAside, path);
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
    crate::failpoint!(AfterSwap, path);
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
/// - A directory is renamed with `renameat2(RENAME_NOREPLACE)` on Linux.
///   Elsewhere, or on a filesystem that refuses the flag, it is checked to be
///   free and renamed: a directory can only ever be renamed over an empty
///   directory (rename fails on anything else), so that fallback can't lose
///   content either.
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
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        use std::os::unix::ffi::OsStrExt;
        let from_c = std::ffi::CString::new(from.as_os_str().as_bytes())?;
        let to_c = std::ffi::CString::new(to.as_os_str().as_bytes())?;
        // SAFETY: both arguments are valid NUL-terminated paths that outlive the call.
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
            adopting: None,
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
            adopting: None,
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
    //! One that holds just the committed version of a path the pull replaced
    //! or removed is pruned as a routine copy is, once the pull is finished
    //! (see [`Displacing::finish_committed`]). Every other one (someone's
    //! version, which a Restore or a keep set aside, or a save during the
    //! swap) stays until adopted or discarded.
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
        /// A [`Kind::Draft`] entry holding just the committed version of a
        /// path a pull replaced or removed (`payload` is what it held), so
        /// it is pruned as a routine copy is, once the pull is finished.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        committed: bool,
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
        Ok(crate::paths::state_dir()?.join("displaced"))
    }

    fn placement_path(kind: Kind, dest: &Path) -> Result<PathBuf> {
        use sha2::{Digest, Sha256};
        let absolute = std::path::absolute(dest).at(dest)?;
        let digest = Sha256::digest(crate::git::path_bytes(&absolute));
        Ok(root()?.join(".placements").join(format!("{kind:?}-{}", crate::hash::hex(&digest))))
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
        /// Whether `path` is still the very file sync published (its
        /// identity), whatever it holds now.
        pub(super) fn same_file(&self, path: &Path) -> Result<bool> {
            Ok(rule_identity(path)? == Some(self.file))
        }

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
        resolve(repo, entry, true)
    }

    /// Whether [`adopt`] can tell where `entry`'s content goes: [`target`]
    /// finds a destination for it, and it is not a skill link (which holds
    /// no content of its own). Only the walk through the history since its
    /// placement is left to Adopt, which refuses an Item ID the history
    /// lost in between.
    pub fn adoptable(repo: &Repo, entry: &Entry) -> bool {
        let link = || fs::symlink_metadata(&entry.path).is_ok_and(|m| m.file_type().is_symlink());
        !(entry.kind == Kind::Skill && link()) && resolve(repo, entry, false).is_ok()
    }

    /// [`target`]; `history`: also check that the Item ID it was placed
    /// with leads to the same item through every commit since.
    fn resolve(repo: &Repo, entry: &Entry, history: bool) -> Result<AdoptTarget> {
        check_repo(repo, entry)?;
        if entry.kind == Kind::Draft {
            return adopt_target(repo, entry.kind, &entry.dest, None);
        }
        let provenance = entry.provenance.as_ref().ok_or_else(|| recovery_target_changed(entry.item_id.as_ref()))?;
        let mut target = adopt_target(repo, entry.kind, &entry.dest, entry.item_id.as_ref())?;
        if target.existing {
            let Some(id) = entry.item_id.as_ref() else { return Err(recovery_target_changed(None)) };
            if history && !history_connects(repo, provenance, id)? {
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
            adopting: None,
        };
        replace_file(dest, &text, unchanged, displace, OldCopy::Keep)
    }

    /// Removes the entries that still match what sync expected and are older
    /// than [`GRACE`], and the remains of entries that were never completed.
    /// Then, while all entries take more than [`max_size`], the oldest of the
    /// unchanged ones left, however young. Changed entries are never pruned.
    /// Returns the ids removed. Sync runs this whenever it displaces something new.
    pub fn prune() -> Result<Vec<String>> {
        prune_retaining(GRACE, max_size())
    }

    /// [`prune`] with its retention overridden: entries are old enough at
    /// `age`, and unchanged ones go while all take more than `max` bytes.
    /// Only for tests (the `failpoints` feature), which can't wait out [`GRACE`].
    #[cfg(any(test, feature = "failpoints"))]
    pub fn prune_with(age: Duration, max: u64) -> Result<Vec<String>> {
        prune_retaining(age, max)
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

    /// Prunes recovery entries with an explicit retention age and byte cap.
    /// Entries with user changes and entries still in use are always kept.
    fn prune_retaining(age: Duration, max: u64) -> Result<Vec<String>> {
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
                    if protected(&id, &meta, &dir) || differs(&meta, &path)? {
                        total += size;
                    } else if old(meta.displaced_at) {
                        crate::failpoint!(BeforePrune, &path);
                        let Some((meta, path)) = load(&dir)? else { continue };
                        if protected(&id, &meta, &dir) || differs(&meta, &path)? || !retire(&root, &id, &dir, &meta)? {
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
            crate::failpoint!(BeforePrune, &dir.join(CONTENT));
            let Some((meta, path)) = load(&dir)? else { continue };
            if protected(&id, &meta, &dir) || differs(&meta, &path)? || !retire(&root, &id, &dir, &meta)? {
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

    /// `dir`: the entry's directory.
    fn protected(id: &str, meta: &Meta, dir: &Path) -> bool {
        if meta.active || meta.payload.is_none() || meta.dest.parent().is_none() {
            return true;
        }
        // A Draft entry holds someone's version, except a committed one a
        // pull replaced: that one waits only for its pull to be finished,
        // whose resume reads it. The holder is the repo's git directory.
        if meta.kind == Kind::Draft {
            return !meta.committed
                || dir
                    .ancestors()
                    .nth(3)
                    .is_none_or(|git_dir| crate::draft::pull_journal_exists(git_dir).unwrap_or(true));
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
                committed: false,
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

        /// Records that what this entry is about to hold is the content of
        /// the repo item at `source` (Item ID `id`), placed or not: what an
        /// Adopt of an unmanaged skill copied into the repo. Before
        /// anything is displaced. `uncopied`: it also holds what the copy
        /// left out, so it is kept, changed, as the only full copy.
        pub(super) fn record_source(
            &self,
            repo: &Repo,
            source: &Path,
            id: Option<&ItemId>,
            uncopied: bool,
        ) -> Result<()> {
            let mut meta = read_meta(&self.dir)
                .ok_or_else(|| Error::Invalid(format!("cannot read recovery entry at {}", self.dir.display())))?;
            meta.item_id = id.cloned();
            meta.provenance = Some(Provenance::current(repo, Some(source))?);
            meta.kept |= uncopied;
            self.write_meta(&meta)
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

        /// [`finish`](Self::finish) for what a pull replaced or removed
        /// once it checked that the path held the committed version: the
        /// entry records what it holds, and is pruned like any routine copy
        /// once it is old enough, while it still holds just that (a write
        /// through a descriptor opened before the pull keeps it, changed).
        /// Extended attributes Git can't restore keep it for good (see
        /// [`crate::hash::has_local_xattrs`]). Run again by a resumed pull.
        #[cfg(unix)]
        pub(crate) fn finish_committed(self) -> Result<Option<Error>> {
            let content = self.content();
            // Fingerprinted before the check, so a write in between fails the
            // check. What can't be fingerprinted stays, as any Draft entry.
            if let Ok(payload) = hash_payload(&content)
                && self.intact().unwrap_or(false)
                && let Some(mut meta) = read_meta(&self.dir)
            {
                match crate::hash::has_local_xattrs(&content) {
                    Ok(false) => {
                        meta.payload = Some(payload);
                        meta.committed = true;
                    }
                    Ok(true) | Err(_) => meta.kept = true,
                }
                self.write_meta(&meta)?;
            }
            self.finish(false)
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
        if let Some(e) = crate::failpoint_error!(RecoveryRename, to) {
            return Err(e);
        }
        fs::rename(from, to)
    }

    fn hard_link(from: &Path, to: &Path) -> std::io::Result<()> {
        if let Some(e) = crate::failpoint_error!(RecoveryLink, to) {
            return Err(e);
        }
        fs::hard_link(from, to)
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
        crate::failpoint!(CopyEntry, entry.path());
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
