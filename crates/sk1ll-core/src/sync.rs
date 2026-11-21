//! Plan/apply sync of skills and instructions into harnesses.
//!
//! `plan` is side-effect free so a front-end can show (and confirm) the diff;
//! `apply` re-checks ownership/state before touching anything, so a stale plan
//! can never delete or overwrite something sk1ll doesn't manage.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;
use walkdir::WalkDir;

use crate::assignment::Assignment;
use crate::hash::{copy_hash, hash_dir, hash_str};
use crate::instructions::{self, Rule};
use crate::machine::{Harness, InstructionsFormat, LinkMode};
use crate::skill::{ID_FILE, Origin, Skill};
use crate::{Error, IoExt, ItemId, Repo, Result};

/// Written inside copied skill directories (holds the content hash) and in
/// rules directories (holds the index of managed rule files).
pub const MARKER: &str = ".sk1ll-managed";
pub(crate) const COPY_SKIP: &[&str] = &[".git", MARKER, ID_FILE];
pub(crate) const IMPORT_COPY_SKIP: &[&str] = &[".git", MARKER];

fn import_source(path: &Path) -> bool {
    path.parent().and_then(Path::file_name).is_some_and(|dir| dir == crate::skill::IMPORTED_DIR)
}

/// Hash of the content a copy placement would send to a harness.
pub fn deploy_hash(source: &Path) -> Result<String> {
    if import_source(source) { copy_hash(source) } else { hash_dir(source) }
}

/// Ownership of skill symlinks, stored next to the skill directories.
const LINK_INDEX: &str = ".sk1ll-managed-links";

const EDITED: &str = "edited by hand in the harness; `sk1ll adopt` it into the repo or use --force";
const BLOCK_EDITED: &str =
    "managed block edited by hand in the harness; move the edits into instructions/ or use --force";

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
        /// Destination hash or link identity observed while planning (`None`: absent).
        expected: Option<String>,
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
struct Selection {
    skills: Vec<Skill>,
    rules: Vec<Rule>,
    /// Content hash of each repo skill a harness was compared against so
    /// far, by skill path: see [`Selection::source_hash`].
    source_hashes: RefCell<BTreeMap<PathBuf, String>>,
}

impl Selection {
    fn new(repo: &Repo) -> Result<Self> {
        let (skills, rules) = repo.content()?;
        Ok(Self { skills, rules, source_hashes: RefCell::default() })
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
        let hash = deploy_hash(&skill.path)?;
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
    let sel = Selection::new(repo)?;
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
    let sel = Selection::new(repo)?;
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
    let sel = Selection { skills: Vec::new(), rules: Vec::new(), source_hashes: RefCell::default() };
    let opts = PlanOptions::default();
    let mut skills = Vec::new();
    plan_skills(repo, hname, harness, &sel, &[], opts, &mut skills)?;
    let mut actions = Vec::new();
    for a in skills {
        // A symlink sk1ll doesn't own is a sync conflict, but was never sk1ll's.
        if a.kind != ActionKind::Conflict || !matches!(classify(repo.root(), &a.dest, None)?, Existing::Unmanaged(_)) {
            actions.push(a);
        }
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

pub fn apply_reviewed(repo: &Repo, action: &Action, reviewed_digest: Option<&str>) -> Result<()> {
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
    sel: &Selection,
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
        },
        reason,
    };
    let links = load_links_index(dir)?;

    let edited = || Some((ActionKind::Conflict, Some(format!("copied skill {EDITED}"))));
    let mut desired = BTreeSet::new();
    for skill in skills {
        desired.insert(skill.name.as_str());
        let dest = dir.join(&skill.name);
        let seen = observe_skill(&dest)?;
        let kind = match classify(repo.root(), &dest, Some(&skill.path))? {
            Existing::Missing => Some((ActionKind::Create, None)),
            Existing::ManagedLink(to)
                if harness.mode == LinkMode::Symlink && to == skill.path.canonicalize().at(&skill.path)? =>
            {
                // Persist ownership when migrating an exact legacy link.
                (links.get(&skill.name) != Some(&to)).then_some((ActionKind::Update, None))
            }
            Existing::ManagedCopy(rec) => {
                // Changed from a copy since observed: never matches `rec`.
                let current = seen.copy_hash.clone().unwrap_or_default();
                #[cfg(test)]
                hook(Hook::PlanSource, &dest);
                let want = sel.source_hash(skill)?;
                let copy = harness.mode == LinkMode::Copy;
                if copy && current == rec && rec == want {
                    None
                } else if current != rec && !opts.force && !(copy && current == want) {
                    edited()
                } else {
                    Some((ActionKind::Update, None))
                }
            }
            Existing::ManagedLink(_) => Some((ActionKind::Update, None)),
            Existing::Unmanaged(why) => Some((ActionKind::Conflict, Some(why))),
        };
        if let Some((kind, reason)) = kind {
            actions.push(action(&skill.name, kind, Some(skill), seen, reason));
        } else if displaced::placement_needs_id(repo, displaced::Kind::Skill, &dest, &skill.path, skill.id.as_ref())? {
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
            // sk1ll's own entries: the links index, and scratch or leftovers
            // of an interrupted sync (see `recover`), never skills.
            if name.starts_with(".sk1ll-") || desired.contains(name.as_str()) {
                continue;
            }
            let seen = observe_skill(&entry.path())?;
            match classify(repo.root(), &entry.path(), None)? {
                Existing::ManagedCopy(rec) if seen.copy_hash.as_ref() != Some(&rec) && !opts.force => {
                    let (kind, reason) = edited().expect("conflict");
                    actions.push(action(&name, kind, None, seen, reason));
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
    let index = instructions::load_rules_index(dir)?;
    let action =
        |file: &str, kind, item_id, content: Option<String>, expected: Option<String>, reason: Option<String>| Action {
            harness: hname.to_string(),
            kind,
            dest: dir.join(file),
            item: Item::Rule { file: file.to_string(), item_id, content, expected, metadata_only: false },
            reason,
        };
    let current_hash = |file: &str| rule_state(&dir.join(file));
    let foreign = || Some(format!("{NOT_A_FILE} where sk1ll placed the rule; left alone"));

    let mut desired = BTreeSet::new();
    for rule in rules {
        let (file, content) = instructions::render_rule(rule);
        let want = hash_str(&content);
        let current = current_hash(&file)?;
        let (kind, reason) = match (&current, index.get(&file)) {
            (None, _) => (ActionKind::Create, None),
            (Some(cur), _) if cur.starts_with(NOT_A_FILE) => (ActionKind::Conflict, foreign()),
            (Some(cur), Some(rec)) if *cur == want && *rec == want => {
                if displaced::placement_needs_id(
                    repo,
                    displaced::Kind::Rule,
                    &dir.join(&file),
                    &rule.path,
                    rule.id.as_ref(),
                )? {
                    let mut record = action(&file, ActionKind::Update, rule.id.clone(), Some(content), current, None);
                    if let Item::Rule { metadata_only, .. } = &mut record.item {
                        *metadata_only = true;
                    }
                    actions.push(record);
                }
                desired.insert(file);
                continue;
            }
            (Some(_), None) => (ActionKind::Conflict, Some("existing rule file not managed by sk1ll".into())),
            (Some(cur), Some(_)) if *cur == want => (ActionKind::Update, None),
            (Some(cur), Some(rec)) if cur != rec && !opts.force => {
                (ActionKind::Conflict, Some(format!("rule {EDITED}")))
            }
            (Some(_), Some(_)) => (ActionKind::Update, None),
        };
        actions.push(action(&file, kind, rule.id.clone(), Some(content), current, reason));
        desired.insert(file);
    }

    for (file, recorded) in &index {
        if desired.contains(file) {
            continue;
        }
        let current = current_hash(file)?;
        let (kind, reason) = match &current {
            Some(cur) if cur.starts_with(NOT_A_FILE) => (ActionKind::Conflict, foreign()),
            Some(cur) if cur != recorded && !opts.force => (ActionKind::Conflict, Some(format!("rule {EDITED}"))),
            _ => (ActionKind::Remove, None),
        };
        actions.push(action(file, kind, None, None, current, reason));
    }
    Ok(())
}

fn apply_rule(repo: &Repo, action: &Action) -> Result<()> {
    let Item::Rule { file, item_id, content, expected, metadata_only } = &action.item else {
        unreachable!("only Rule placements reach apply_rule");
    };
    let (content, expected) = (content.as_deref(), expected.as_deref());
    let source = repo.root().join(instructions::INSTRUCTIONS_DIR).join(file);
    if action.kind == ActionKind::Conflict {
        return Ok(());
    }
    let dest = &action.dest;
    let dir = dest.parent().ok_or_else(|| Error::Invalid(format!("{} has no parent", dest.display())))?;
    recover_rules_dir(&action.harness, dir)?;
    let current = rule_state(dest)?;
    if current.as_deref() != expected {
        return Err(changed_since_plan(dest));
    }
    if current.as_deref().is_some_and(|c| c.starts_with(NOT_A_FILE)) {
        return Err(Error::Invalid(format!("refusing to touch {}: {NOT_A_FILE}", dest.display())));
    }
    let unchanged = || -> Result<()> {
        match rule_state(dest)?.as_deref() == expected {
            true => Ok(()),
            false => Err(changed_since_plan(dest)),
        }
    };
    let mut index = instructions::load_rules_index(dir)?;
    if *metadata_only {
        if current.as_ref().is_none_or(|hash| index.get(file) != Some(hash)) {
            return Err(changed_since_plan(dest));
        }
        return displaced::record_placement(repo, displaced::Kind::Rule, dest, Some(&source), item_id.as_ref());
    }
    let displaced = match content {
        Some(content) => {
            let displace = Displace { repo, kind: displaced::Kind::Rule, harness: &action.harness, expected };
            let displaced = replace_file(dest, content, unchanged, displace)?;
            index.insert(file.to_string(), hash_str(content));
            displaced
        }
        None => {
            let displaced = match current {
                Some(_) => {
                    #[cfg(test)]
                    hook(Hook::BeforeSwap, dest);
                    remove_displacing(repo, displaced::Kind::Rule, &action.harness, dest, expected)?
                }
                None => None,
            };
            index.remove(file);
            displaced
        }
    };
    // The harness changed either way, so the index must say so before any
    // displaced edit is reported.
    instructions::save_rules_index(dir, &index)?;
    displaced::record_placement(
        repo,
        displaced::Kind::Rule,
        dest,
        content.map(|_| source.as_path()),
        item_id.as_ref(),
    )?;
    displaced.map_or(Ok(()), Err)
}

fn apply_instructions(repo: &Repo, action: &Action, content: Option<&str>, expected: Option<&str>) -> Result<()> {
    if action.kind == ActionKind::Conflict {
        return Ok(());
    }
    let dest = &action.dest;
    recover_file_siblings(&action.harness, dest)?;
    let text = instructions::read_optional(dest)?.unwrap_or_default();
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
    let displace =
        Displace { repo, kind: displaced::Kind::Instructions, harness: &action.harness, expected: Some(&whole) };
    let spliced = instructions::splice(&text, block.as_ref(), content);
    replace_file(dest, &spliced, unchanged, displace)?.map_or(Ok(()), Err)
}

fn apply_skill(repo: &Repo, action: &Action, adopted: bool, reviewed_digest: Option<&str>) -> Result<()> {
    let Item::Skill { name, mode, source, item_id, expected, metadata_only } = &action.item else {
        unreachable!("only skill placements reach apply_skill");
    };
    let (name, mode, source, expected) = (name.as_str(), *mode, source.as_deref(), expected.as_deref());
    if action.kind == ActionKind::Conflict {
        return Ok(());
    }
    let dest = &action.dest;
    let dir = dest.parent().ok_or_else(|| Error::Invalid(format!("{} has no parent", dest.display())))?;
    recover_skills_dir(&action.harness, dir)?;
    let unchanged = |at: &Path| -> Result<bool> { Ok(observe_skill(at)?.state.as_deref() == expected) };
    if !unchanged(dest)? {
        return Err(changed_since_plan(dest));
    }
    let existing = classify(repo.root(), dest, source)?;
    let managed = match &existing {
        Existing::Unmanaged(_) if adopted => true,
        Existing::Missing => false,
        Existing::ManagedLink(_) | Existing::ManagedCopy(_) => true,
        Existing::Unmanaged(why) => {
            return Err(Error::Invalid(format!("refusing to touch {}: {why}", dest.display())));
        }
    };
    let mut links = load_links_index(dir)?;
    if *metadata_only {
        if !managed || matches!(existing, Existing::Missing) {
            return Err(changed_since_plan(dest));
        }
        return displaced::record_placement(repo, displaced::Kind::Skill, dest, source, item_id.as_ref());
    }
    let displaced = match action.kind {
        ActionKind::Conflict => unreachable!(),
        ActionKind::Remove => {
            let displaced = if managed {
                #[cfg(test)]
                hook(Hook::BeforeSwap, dest);
                let displaced = remove_displacing(repo, displaced::Kind::Skill, &action.harness, dest, expected);
                remove_empty_tmp(dir);
                displaced?
            } else {
                None
            };
            links.remove(name);
            displaced
        }
        ActionKind::Create | ActionKind::Update => {
            let src = source.ok_or_else(|| Error::Invalid(format!("no source for {name}")))?;
            // Validate the entire source before replacing any harness content.
            let canonical = lexical_normalize(&src.canonicalize().at(src)?);
            if mode == LinkMode::Symlink && !canonical.starts_with(repo.root()) {
                return Err(Error::Invalid(format!("{} resolves outside the repo", src.display())));
            }
            // An exact legacy link only needs its ownership recorded.
            if mode == LinkMode::Symlink && matches!(&existing, Existing::ManagedLink(to) if *to == canonical) {
                links.insert(name.to_string(), canonical);
                save_links_index(dir, &links)?;
                return displaced::record_placement(repo, displaced::Kind::Skill, dest, Some(src), item_id.as_ref());
            }
            // Build the replacement in the skills dir's own scratch directory
            // (same filesystem, never discovered as a skill), then swap it in.
            let tmp = dir.join(TMP_DIR);
            fs::create_dir_all(&tmp).at(&tmp)?;
            let stage = tmp.join(format!("stage-{name}"));
            let built = (|| {
                if fs::symlink_metadata(&stage).is_ok() {
                    remove_entry(&stage)?; // left over from an interrupted sync
                }
                match mode {
                    LinkMode::Symlink => symlink_dir(src, &stage).at(&stage),
                    LinkMode::Copy => {
                        #[cfg(test)]
                        hook(Hook::BeforeCopy, src);
                        let skip = if import_source(src) { IMPORT_COPY_SKIP } else { COPY_SKIP };
                        copy_dir(src, &stage, skip)?;
                        let digest = copy_hash(&stage)?;
                        if reviewed_digest.is_some_and(|expected| expected != digest) {
                            return Err(Error::Changed(format!("{} changed since the sync preview", src.display())));
                        }
                        let marker = stage.join(MARKER);
                        fs::write(&marker, digest).at(&marker)
                    }
                }
            })();
            let swap = Swap { repo, harness: &action.harness, managed, expected };
            let swapped = built.and_then(|()| swap_in(&stage, dest, swap, unchanged));
            if swapped.is_err() && fs::symlink_metadata(&stage).is_ok() {
                let _ = remove_entry(&stage);
            }
            remove_empty_tmp(dir);
            let displaced = swapped?;
            match mode {
                LinkMode::Symlink => {
                    links.insert(name.to_string(), canonical);
                }
                LinkMode::Copy => {
                    links.remove(name);
                }
            }
            displaced
        }
    };
    save_links_index(dir, &links)?;
    displaced::record_placement(repo, displaced::Kind::Skill, dest, source, item_id.as_ref())?;
    displaced.map_or(Ok(()), Err)
}

/// What [`rule_state`] says of anything at a rule file's place that isn't
/// a plain file (a symlink, even to identical text, is someone else's).
const NOT_A_FILE: &str = "not a plain file";

/// A rule file's hash, as one look at the entry itself, never following a
/// symlink there; `None`: absent. Whatever isn't a plain file never matches
/// a hash sk1ll recorded, so it is never replaced or removed.
fn rule_state(path: &Path) -> Result<Option<String>> {
    use crate::config_file::{Entry, open_entry};
    use std::io::Read;
    // Opened without blocking, so a FIFO there can't stall the engine.
    let mut file = match open_entry(path)? {
        Entry::Missing => return Ok(None),
        Entry::Other(what) => return Ok(Some(format!("{NOT_A_FILE}: {what}"))),
        Entry::File(file, _) => file,
    };
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).at(path)?;
    Ok(Some(hash_str(&String::from_utf8_lossy(&bytes))))
}

/// One look at a skill destination.
struct Observed {
    /// The entry's identity (link target, or content hash and marker), the
    /// precondition `apply` checks; `None`: absent.
    state: Option<String>,
    /// For a directory, its content hash, taken in the same pass as `state`.
    copy_hash: Option<String>,
}

/// Snapshot the entry itself, never following a skill symlink into the repo.
fn observe_skill(dest: &Path) -> Result<Observed> {
    let meta = match fs::symlink_metadata(dest) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Observed { state: None, copy_hash: None }),
        Err(e) => return Err(e).at(dest),
    };
    let (state, copy_hash) = if meta.file_type().is_symlink() {
        (format!("link:{:?}", fs::read_link(dest).at(dest)?), None)
    } else if meta.is_dir() {
        let marker = instructions::read_optional(&dest.join(MARKER))?;
        let hash = copy_hash(dest)?;
        (format!("copy:{hash}:{marker:?}"), Some(hash))
    } else {
        ("unmanaged".to_string(), None)
    };
    Ok(Observed { state: Some(state), copy_hash })
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

fn remove_empty_tmp(dir: &Path) {
    let _ = fs::remove_dir(dir.join(TMP_DIR));
}

/// What [`swap_in`] needs to know about the skill it replaces.
struct Swap<'a> {
    repo: &'a Repo,
    harness: &'a str,
    /// Something sk1ll manages is at the destination (else it is absent).
    managed: bool,
    expected: Option<&'a str>,
}

/// Swaps the staged `stage` in at `dest`. `unchanged(path)` tells whether
/// the entry at `path` still matches the plan's precondition: it is checked
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
fn swap_in(stage: &Path, dest: &Path, swap: Swap, unchanged: impl Fn(&Path) -> Result<bool>) -> Result<Option<Error>> {
    #[cfg(test)]
    hook(Hook::BeforeSwap, dest);
    if !unchanged(dest)? {
        return Err(changed_since_plan(dest));
    }
    #[cfg(test)]
    hook(Hook::AfterCheck, dest);
    if !swap.managed {
        // Absent when planned (and just now); if something appeared since, it stays.
        return place_at(stage, dest).map(|()| None);
    }
    let holder = dest.parent().unwrap_or(dest);
    let mut held = displaced::Displacing::begin(
        Some(swap.repo),
        displaced::Kind::Skill,
        swap.harness,
        dest,
        swap.expected,
        holder,
    )?;
    if let Err(e) = held.take(dest) {
        held.abandon();
        return Err(e);
    }
    let intact = held.intact();
    #[cfg(test)]
    hook(Hook::AfterAside, dest);
    let swapped = match intact {
        Ok(true) => place_at(stage, dest),
        Ok(false) => Err(changed_since_plan(dest)),
        Err(e) => Err(e),
    };
    if let Err(e) = swapped {
        held.put_back()?;
        return Err(e);
    }
    #[cfg(test)]
    hook(Hook::AfterSwap, dest);
    held.finish()
}

/// Removes `dest` (a skill entry or rule file) unless it changed since the
/// plan: it is moved into the recovery area first, and put back if it no
/// longer matches `expected` there. Once removed, the moved entry is checked
/// once more, like the old entry in [`swap_in`].
fn remove_displacing(
    repo: &Repo,
    kind: displaced::Kind,
    harness: &str,
    dest: &Path,
    expected: Option<&str>,
) -> Result<Option<Error>> {
    let holder = dest.parent().unwrap_or(dest);
    let mut held = displaced::Displacing::begin(Some(repo), kind, harness, dest, expected, holder)?;
    if let Err(e) = held.take(dest) {
        held.abandon();
        return Err(e);
    }
    let intact = held.intact();
    #[cfg(test)]
    hook(Hook::AfterAside, dest);
    match intact {
        Ok(true) => {
            #[cfg(test)]
            hook(Hook::AfterSwap, dest);
            held.finish()
        }
        verdict => {
            held.put_back()?;
            Err(verdict.err().unwrap_or_else(|| changed_since_plan(dest)))
        }
    }
}

/// How [`replace_file`] records the content it replaces.
struct Displace<'a> {
    repo: &'a Repo,
    kind: displaced::Kind,
    harness: &'a str,
    /// The fingerprint the replaced content should have (`None`: there
    /// should be none, so any content found is someone else's).
    expected: Option<&'a str>,
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
) -> Result<Option<Error>> {
    let real = match fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() => fs::canonicalize(path).at(path)?,
        _ => path.to_path_buf(),
    };
    let holder = real.parent().ok_or_else(|| Error::Invalid(format!("{} has no parent", real.display())))?;
    fs::create_dir_all(holder).at(holder)?;
    let tmp = sibling(&real, "sk1ll-tmp");
    crate::config_file::write_new(&tmp, text.as_bytes())?;
    let fail = |e: Error| {
        let _ = fs::remove_file(&tmp);
        Err(e)
    };
    #[cfg(test)]
    hook(Hook::BeforeSwap, path);
    if let Err(e) = unchanged() {
        return fail(e);
    }
    #[cfg(test)]
    hook(Hook::AfterCheck, path);
    let Displace { repo, kind, harness, expected } = displace;
    let mut held = match displaced::Displacing::begin(Some(repo), kind, harness, path, expected, holder) {
        Ok(held) => held,
        Err(e) => return fail(e),
    };
    let verdict = match held.link(&real) {
        Ok(true) => held.intact().map(Some),
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
    let swapped = match held {
        // The old file is held in recovery under a second name: replacing it loses nothing.
        Some(_) => fs::rename(&tmp, &real).at(&real),
        None => place_at(&tmp, &real),
    };
    if let Err(e) = swapped {
        if let Some(held) = held {
            held.abandon();
        }
        return fail(e);
    }
    #[cfg(test)]
    hook(Hook::AfterSwap, path);
    held.map_or(Ok(None), displaced::Displacing::finish)
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
/// harness: replacements that were being staged are deleted, and old
/// content that was set aside (by older versions, next to the destination)
/// is moved into the recovery area. Recovery entries in the harnesses' own
/// directories that the state directory's index lost are listed again.
/// Returns the entries that rescued; those that differ from what sk1ll last
/// wrote are also in [`displaced::changed`]. Planning never needs this (it
/// ignores such leftovers); [`apply`] does it for the directory it works in.
///
/// Only ever run this holding the engine lock (which `_lock` proves): another
/// sync's work in progress looks just like a crash's leftovers, and this
/// deletes or moves them. Read-only paths (`sync --dry-run`, status,
/// conflicts) must never call it.
pub fn recover(_lock: &crate::EngineLock, harnesses: &BTreeMap<String, Harness>) -> Result<Vec<displaced::Entry>> {
    let mut rescued = Vec::new();
    for (hname, harness) in harnesses {
        if let Some(dir) = harness.skills_dir() {
            rescued.extend(recover_skills_dir(hname, &dir)?);
            displaced::reindex(&dir)?;
        }
        if let Some(path) = harness.instructions_file() {
            rescued.extend(match harness.instructions_format {
                InstructionsFormat::Block => recover_file_siblings(hname, &path)?,
                InstructionsFormat::Rules => recover_rules_dir(hname, &path)?,
            });
            let holder = match harness.instructions_format {
                InstructionsFormat::Block => real_file(&path).and_then(|real| real.parent().map(Path::to_path_buf)),
                InstructionsFormat::Rules => Some(path),
            };
            if let Some(holder) = holder {
                displaced::reindex(&holder)?;
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

fn recover_skills_dir(harness: &str, dir: &Path) -> Result<Vec<displaced::Entry>> {
    let mut rescued = Vec::new();
    let mut leftovers = Vec::new();
    for name in read_dir_names(dir)? {
        if name.starts_with(LEGACY_STAGE_PREFIX) {
            remove_entry(&dir.join(&name))?;
        } else if let Some(skill) = name.strip_prefix(LEGACY_OLD_PREFIX) {
            leftovers.push((dir.join(&name), skill.to_string()));
        }
    }
    let tmp = dir.join(TMP_DIR);
    for name in read_dir_names(&tmp)? {
        if name.starts_with("stage-") {
            remove_entry(&tmp.join(&name))?;
        } else if let Some(skill) = name.strip_prefix("old-") {
            leftovers.push((tmp.join(&name), skill.to_string()));
        }
    }
    for (aside, skill) in leftovers {
        // What sk1ll last wrote there: a link is as it was made; a copy
        // matches its own marker unless it was edited.
        let expected = match fs::symlink_metadata(&aside) {
            Ok(m) if m.file_type().is_symlink() => observe_skill(&aside)?.state,
            Ok(m) if m.is_dir() => instructions::read_optional(&aside.join(MARKER))?
                .map(|raw| format!("copy:{}:{:?}", raw.trim(), Some(&raw))),
            _ => None,
        };
        let kind = displaced::Kind::Skill;
        rescued.push(displaced::rescue(kind, harness, &dir.join(&skill), &aside, expected.as_deref(), dir)?);
    }
    remove_empty_tmp(dir);
    Ok(rescued)
}

fn recover_rules_dir(harness: &str, dir: &Path) -> Result<Vec<displaced::Entry>> {
    let mut rescued = Vec::new();
    let mut index = None;
    for name in read_dir_names(dir)? {
        if name == TMP_DIR {
            continue; // the recovery area, which ends in `.sk1ll-tmp` too
        }
        let path = dir.join(&name);
        let file = if let Some(file) = name.strip_prefix(LEGACY_OLD_PREFIX) {
            file
        } else if let Some(file) = name.strip_prefix('.').and_then(|n| n.strip_suffix(".sk1ll-old")) {
            file
        } else {
            if name.starts_with('.') && name.ends_with(".sk1ll-tmp") {
                remove_entry(&path)?;
            }
            continue;
        };
        if index.is_none() {
            index = Some(instructions::load_rules_index(dir)?);
        }
        let expected = index.as_ref().and_then(|i| i.get(file)).cloned();
        let kind = displaced::Kind::Rule;
        rescued.push(displaced::rescue(kind, harness, &dir.join(file), &path, expected.as_deref(), dir)?);
    }
    Ok(rescued)
}

/// Leftovers next to a block instructions file (its target, if linked).
fn recover_file_siblings(harness: &str, path: &Path) -> Result<Vec<displaced::Entry>> {
    let Some(real) = real_file(path) else {
        return Ok(Vec::new());
    };
    let tmp = sibling(&real, "sk1ll-tmp");
    if fs::symlink_metadata(&tmp).is_ok() {
        remove_entry(&tmp)?;
    }
    let old = sibling(&real, "sk1ll-old");
    if fs::symlink_metadata(&old).is_err() {
        return Ok(Vec::new());
    }
    if displaced::same_file(&old, &real) {
        // Killed before the replacement went in: it is the live file.
        fs::remove_file(&old).at(&old)?;
        return Ok(Vec::new());
    }
    let holder = real.parent().unwrap_or(&real);
    Ok(vec![displaced::rescue(displaced::Kind::Instructions, harness, path, &old, None, holder)?])
}

/// Resolves a conflict the other way round: copies the harness's content back
/// into the repo, where it becomes a draft change. Works for copied skill
/// directories (into the skill's repo directory, without the marker) and
/// `rules` files (into `instructions/<name>.md`, keeping the rule's
/// `harnesses:` key). The harness copy is then recorded as managed, so the
/// next plan has no conflict for it.
pub fn adopt(repo: &Repo, action: &Action) -> Result<()> {
    if action.kind != ActionKind::Conflict {
        return Err(Error::Invalid("only conflicts can be adopted".into()));
    }
    match &action.item {
        Item::Skill { name, .. } => adopt_skill(repo, &action.dest, name),
        Item::Rule { file, .. } => adopt_rule(repo, &action.dest, file),
        Item::Instructions { .. } => Err(Error::Invalid(
            "adopt isn't supported for block instructions; one block mixes several rules, so move the edits \
             into instructions/ by hand, or overwrite them with `sk1ll sync --force`"
                .into(),
        )),
    }
}

fn adopt_skill(repo: &Repo, src: &Path, name: &str) -> Result<()> {
    let source = repo.skills()?.into_iter().find(|s| s.name == name).map(|s| s.path);
    if matches!(classify(repo.root(), src, source.as_deref())?, Existing::Unmanaged(_)) {
        return Err(Error::Invalid(format!(
            "`{name}` is an unmanaged skill and cannot replace an existing repo skill"
        )));
    }
    let target = adopt_target(repo, displaced::Kind::Skill, Path::new(name), None)?;
    let placed_origin = displaced::skill_placement_origin(src)?.or(target.origin);
    copy_into_repo(repo, src, &target, None, placed_origin)?;
    let marker = src.join(MARKER);
    fs::write(&marker, copy_hash(src)?).at(&marker)
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

fn recovery_target_changed(id: Option<&ItemId>) -> Error {
    let id = id.map_or_else(|| "unassigned".into(), ToString::to_string);
    Error::Invalid(format!(
        "Recovery cannot identify a safe destination for Item ID `{id}`. Pick a target for the recovered content"
    ))
}

pub fn copy_skill_for_diff(src: &Path, dest: &Path, origin: Origin) -> Result<()> {
    copy_dir(src, dest, copy_skip(origin))
}

fn copy_skip(origin: Origin) -> &'static [&'static str] {
    if origin == Origin::Imported { IMPORT_COPY_SKIP } else { COPY_SKIP }
}

fn copy_into_repo(
    repo: &Repo,
    src: &Path,
    target: &AdoptTarget,
    recovered_id: Option<&ItemId>,
    placed_origin: Option<Origin>,
) -> Result<()> {
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
    let origin_before = placed_origin.unwrap_or(origin);
    let skip = if origin == Origin::Personal { COPY_SKIP } else { copy_skip(origin_before) };
    validate_copy(src, skip)?;
    #[cfg(unix)]
    {
        use crate::placement::Directory;
        let parent_path = dest.parent().unwrap();
        let parent = Directory::open(parent_path, true).at(parent_path)?;
        let name = dest.file_name().unwrap();
        if target.existing && !parent.is_directory(name) {
            return Err(Error::Invalid(format!("{} is not a skill directory", dest.display())));
        }
        let stage = crate::source::unique(parent_path);
        let stage_name = stage.file_name().unwrap();
        let aside = crate::source::unique(parent_path);
        let aside_name = aside.file_name().unwrap();
        let staged = parent.new_directory(stage_name).at(&stage)?;
        let result = (|| {
            copy_adoption_tree(src, &staged, skip, false)?;
            if let Some(id) = id {
                staged.create_file(Path::new(ID_FILE), format!("{id}\n").as_bytes(), false).at(dest.join(ID_FILE))?;
            } else {
                if !parent.still_at(parent_path).at(parent_path)? {
                    return Err(changed_since_plan(parent_path));
                }
                copy_adoption_tree(dest, &staged, skip, true)?;
            }
            if !parent.still_at(parent_path).at(parent_path)? {
                return Err(changed_since_plan(parent_path));
            }
            if target.existing {
                parent.rename_child(name, aside_name).at(dest)?;
            }
            if let Err(error) = parent.rename_child(stage_name, name).at(dest) {
                if target.existing {
                    parent.rename_child(aside_name, name).at(&aside)?;
                }
                return Err(error);
            }
            if target.existing {
                parent.remove_tree(aside_name).at(&aside)?;
            }
            Ok(())
        })();
        if result.is_err() {
            let _ = parent.remove_tree(stage_name);
        }
        result
    }
    #[cfg(not(unix))]
    {
        let _ = id;
        Err(Error::Invalid("safe repo skill adoption is unavailable on this platform".into()))
    }
}

#[cfg(unix)]
fn copy_adoption_tree(
    src: &Path,
    dest: &crate::placement::Directory,
    skip: &[&str],
    preserve_filtered: bool,
) -> Result<()> {
    let walker = WalkDir::new(src)
        .into_iter()
        .filter_entry(|e| preserve_filtered || e.depth() == 0 || !skip.iter().any(|name| e.file_name() == *name));
    for entry in walker {
        let entry = entry.map_err(std::io::Error::from).at(src)?;
        if entry.depth() == 0 {
            continue;
        }
        let rel = entry.path().strip_prefix(src).unwrap();
        if preserve_filtered && !rel.components().any(|part| skip.iter().any(|name| part.as_os_str() == *name)) {
            continue;
        }
        let kind = entry.file_type();
        if kind.is_dir() {
            dest.create_directory(rel).at(entry.path())?;
        } else if kind.is_symlink() {
            let target = fs::read_link(entry.path()).at(entry.path())?;
            dest.create_symlink(rel, target.as_os_str()).at(entry.path())?;
        } else if kind.is_file() {
            dest.copy_file(entry.path(), rel).at(entry.path())?;
        } else {
            return Err(Error::Invalid(format!("unsupported skill entry: {}", entry.path().display())));
        }
    }
    Ok(())
}

fn adopt_rule(repo: &Repo, src: &Path, file: &str) -> Result<()> {
    let Some(text) = crate::config_file::read_text(src, false)? else {
        return Err(Error::Invalid(format!("{} no longer exists; nothing to adopt", src.display())));
    };
    let target = adopt_target(repo, displaced::Kind::Rule, Path::new(file), None)?;
    rule_into_repo(repo, &target, &text, None)?;
    let dir = src.parent().ok_or_else(|| Error::Invalid(format!("{} has no parent", src.display())))?;
    let mut index = instructions::load_rules_index(dir)?;
    index.insert(file.to_string(), hash_str(&text));
    instructions::save_rules_index(dir, &index)
}

fn rule_into_repo(repo: &Repo, target: &AdoptTarget, text: &str, recovered_id: Option<&ItemId>) -> Result<()> {
    let dest = &target.path;
    let id = match target.rule.as_ref().and_then(|rule| rule.id.as_ref()).or(recovered_id) {
        Some(id) => id.clone(),
        None => repo.new_item_id()?,
    };
    repo.check_item_id(dest.strip_prefix(repo.root()).unwrap().into(), id.clone())?;
    let content = instructions::restore_metadata(target.rule.as_ref(), text, dest, Some(&id))?;
    #[cfg(unix)]
    {
        let parent = dest.parent().unwrap();
        let pinned = crate::placement::Directory::open(parent, true).at(parent)?;
        if target.existing {
            pinned.replace_text(dest.file_name().unwrap(), &content).at(dest)
        } else {
            pinned.create_file(Path::new(dest.file_name().unwrap()), content.as_bytes(), false).at(dest)
        }
    }
    #[cfg(not(unix))]
    Err(Error::Invalid("safe repo Rule adoption is unavailable on this platform".into()))
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
    //! entries together take more than [`max_size`], oldest first.

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
            let git = || crate::git::Git::new(repo.root()).env("GIT_OPTIONAL_LOCKS", "0");
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

    /// `$XDG_STATE_HOME/sk1ll/displaced` (the platform's state directory,
    /// else its data directory): the index of all entries, and entries
    /// older versions kept here.
    pub fn root() -> Result<PathBuf> {
        #[cfg(test)]
        if let Some(root) = TEST_ROOT.with_borrow(Clone::clone) {
            return Ok(root);
        }
        #[cfg(test)]
        return Ok(DEFAULT_TEST_ROOT.with(|root| root.path().to_path_buf()));
        #[cfg(not(test))]
        dirs::state_dir()
            .or_else(dirs::data_dir)
            .map(|d| d.join("sk1ll").join("displaced"))
            .ok_or_else(|| Error::Invalid("cannot determine the state directory for displaced content".into()))
    }

    fn placement_path(kind: Kind, dest: &Path) -> Result<PathBuf> {
        use sha2::{Digest, Sha256};
        let absolute = std::path::absolute(dest).at(dest)?;
        let digest = Sha256::digest(crate::git::path_bytes(&absolute));
        Ok(root()?.join(".placements").join(format!("{kind:?}-{digest:x}")))
    }

    pub(super) fn record_placement(
        repo: &Repo,
        kind: Kind,
        dest: &Path,
        source: Option<&Path>,
        id: Option<&ItemId>,
    ) -> Result<()> {
        let path = placement_path(kind, dest)?;
        if let Some(source) = source {
            make_dirs(path.parent().unwrap())?;
            let record = PlacementId { id: id.cloned(), provenance: Provenance::current(repo, Some(source))? };
            let bytes = serde_json::to_vec(&record).map_err(|e| Error::Invalid(e.to_string()))?;
            crate::config_file::replace_private(&path, &bytes)
        } else {
            match fs::remove_file(&path) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                result => result.at(path),
            }
        }
    }

    fn placed(kind: Kind, dest: &Path) -> Result<Option<PlacementId>> {
        if kind == Kind::Instructions {
            return Ok(None);
        }
        let path = placement_path(kind, dest)?;
        crate::config_file::read_text(&path, false)?
            .map(|text| {
                serde_json::from_str::<PlacementId>(&text)
                    .map_err(|error| Error::Parse { path, message: error.to_string() })
            })
            .transpose()
    }

    pub(super) fn placement_needs_id(
        repo: &Repo,
        kind: Kind,
        dest: &Path,
        source: &Path,
        id: Option<&ItemId>,
    ) -> Result<bool> {
        let placed = placed(kind, dest)?;
        let root = RepoIdentity::of(repo)?;
        let source = source.strip_prefix(repo.root()).unwrap_or(source);
        Ok(placed.is_none_or(|record| {
            record.id.as_ref() != id
                || record.provenance.repo != root
                || record.provenance.source.as_deref() != Some(source)
        }))
    }

    pub fn skill_placement_origin(dest: &Path) -> Result<Option<Origin>> {
        Ok(placed(Kind::Skill, dest)?.and_then(|record| record.provenance.origin()))
    }

    /// Where the entries of content displaced from the directory `holder` are.
    pub fn area(holder: &Path) -> PathBuf {
        holder.join(TMP_DIR).join(AREA)
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
        let root = root()?;
        match locate(&root, id).map(|dir| load(&dir)).transpose()?.flatten() {
            Some((meta, path)) => entry(id.to_string(), meta, path),
            None => Err(Error::Invalid(format!("no displaced entry `{id}`"))),
        }
    }

    /// Deletes an entry and its content for good.
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
        forget(&root, id, &dir)
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
                let out = crate::git::Git::new(repo.root()).args(["rev-parse", "--absolute-git-dir"]).run()?;
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
        let git = || crate::git::Git::new(root).env("GIT_OPTIONAL_LOCKS", "0");
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
    /// - A file pull displaced from the repo's working tree goes back to its
    ///   path as a Draft change (see [`crate::draft::adopt_displaced`]).
    ///
    /// Returns where the content went.
    pub fn adopt(repo: &Repo, id: &str, lock: &crate::EngineLock) -> Result<PathBuf> {
        let entry = get(id)?;
        let target = target(repo, &entry)?;
        match entry.kind {
            Kind::Skill if fs::symlink_metadata(&entry.path).at(&entry.path)?.file_type().is_symlink() => {
                return Err(Error::Invalid(format!(
                    "displaced entry `{id}` is a skill link, which holds no content of its own; discard it"
                )));
            }
            Kind::Skill => copy_into_repo(repo, &entry.path, &target, entry.item_id.as_ref(), target.placed_origin)?,
            Kind::Rule => rule_into_repo(repo, &target, &read_text(&entry.path)?, entry.item_id.as_ref())?,
            Kind::Instructions => restore_instructions(repo, &entry)?,
            Kind::Draft => {
                crate::draft::adopt_displaced(repo, &entry, lock)?;
                return Ok(target.path);
            }
        }
        discard(id)?;
        Ok(target.path)
    }

    fn restore_instructions(repo: &Repo, entry: &Entry) -> Result<()> {
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
        let displace = Displace { repo, kind: Kind::Instructions, harness: &entry.harness, expected: Some(&expected) };
        replace_file(dest, &text, unchanged, displace)?.map_or(Ok(()), Err)
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
                    if meta.kind == Kind::Draft || differs(&meta, &path)? {
                        total += size;
                    } else if old(meta.displaced_at) {
                        forget(&root, &id, &dir)?;
                        pruned.push(id);
                    } else {
                        total += size;
                        unchanged.push((id, dir, size));
                    }
                }
                // Begun but never filled (killed meanwhile), or not ours.
                (dir, None) => {
                    if let Some(content) = dir.as_ref().map(|dir| dir.join(CONTENT))
                        && !matches!(fs::symlink_metadata(&content), Err(e) if e.kind() == std::io::ErrorKind::NotFound)
                    {
                        total += disk_size(&content, &mut inodes);
                        continue;
                    }
                    // A prepared Draft removal can wait in a journal indefinitely.
                    if dir.as_deref().and_then(read_meta).is_some_and(|meta| meta.kind == Kind::Draft) {
                        continue;
                    }
                    let since = dir.as_deref().and_then(modified).or_else(|| modified(&pointer));
                    if since.is_some_and(old) {
                        match dir {
                            Some(dir) => forget(&root, &id, &dir)?,
                            None => remove_if_there(&pointer)?,
                        }
                        pruned.push(id);
                    }
                }
                (None, Some(_)) => unreachable!(),
            }
        }
        for (id, dir, size) in unchanged {
            if total <= max {
                break;
            }
            forget(&root, &id, &dir)?;
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
        let area = area(holder);
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
        ok.then_some(dir)
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
        Ok(true)
    }

    /// Deletes the entry `id` at `dir` and its index record, and the area
    /// once it is empty.
    fn forget(root: &Path, id: &str, dir: &Path) -> Result<()> {
        match fs::remove_dir_all(dir) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e).at(dir),
            _ => {}
        }
        remove_if_there(&root.join(INDEX).join(id))?;
        // Never the state directory itself (older versions' entries).
        let named = |p: Option<&Path>, name: &str| p.and_then(Path::file_name) == Some(name.as_ref());
        let in_area = |a: &&Path| named(Some(a), AREA) && named(a.parent(), TMP_DIR);
        if let Some(area) = dir.parent().filter(in_area) {
            tidy(area);
        }
        Ok(())
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

    fn remove_if_there(path: &Path) -> Result<()> {
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

    fn read_meta(dir: &Path) -> Option<Meta> {
        let text = instructions::read_optional(&dir.join(META)).ok().flatten()?;
        serde_json::from_str(&text).ok()
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
        Ok(meta.kept || meta.expected.is_empty() || fingerprint(meta.kind, path)?.unwrap_or_default() != meta.expected)
    }

    fn read_text(path: &Path) -> Result<String> {
        let bytes = crate::config_file::read_regular(path, false)?
            .ok_or_else(|| Error::Invalid(format!("{} no longer exists", path.display())))?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    /// What sync compares content by: a skill as [`observe_skill`] sees it,
    /// a file by the hash of its text.
    fn fingerprint(kind: Kind, path: &Path) -> Result<Option<String>> {
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
        let mut held = Displacing::begin(None, kind, harness, dest, expected, holder)?;
        if let Err(e) = held.take(aside) {
            held.abandon();
            return Err(e);
        }
        let id = held.id.clone();
        held.finish()?;
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
        pub(crate) fn begin(
            repo: Option<&Repo>,
            kind: Kind,
            harness: &str,
            dest: &Path,
            expected: Option<&str>,
            holder: &Path,
        ) -> Result<Self> {
            let recorded = placed(kind, dest)?;
            let (item_id, provenance) = match recorded {
                Some(record) => (record.id, Some(record.provenance)),
                None => (None, repo.map(|repo| Provenance::current(repo, None)).transpose()?),
            };
            let root = root()?;
            make_dirs(&root.join(INDEX))?;
            // Best effort: a stale entry that can't be pruned is no reason not to sync.
            if kind != Kind::Draft {
                let _ = prune();
            }
            let area = area(holder);
            make_dirs(&area)?;
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
                harness: harness.to_string(),
                kind,
                dest: dest.to_path_buf(),
                item_id,
                provenance,
                expected: expected.unwrap_or_default().to_string(),
                displaced_at: now(),
                kept: false,
            };
            let expected = meta.expected.clone();
            let held = Self { id, root, dir, kind, dest: dest.to_path_buf(), expected, held: None };
            if let Err(e) = held.write_meta(&meta) {
                let _ = forget(&held.root, &held.id, &held.dir);
                return Err(e);
            }
            Ok(held)
        }

        fn write_meta(&self, meta: &Meta) -> Result<()> {
            let body = serde_json::to_vec_pretty(meta).map_err(|e| Error::Invalid(e.to_string()))?;
            crate::config_file::replace_private(&self.dir.join(META), &body)
        }

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

        #[cfg(unix)]
        pub(crate) fn id(&self) -> &str {
            &self.id
        }

        #[cfg(unix)]
        pub(crate) fn filled(&self) -> bool {
            self.held.is_some()
        }

        pub(crate) fn forget_empty(id: &str) -> Result<()> {
            let root = root()?;
            let dir = locate(&root, id).ok_or_else(|| Error::Invalid(format!("no displaced entry `{id}`")))?;
            match fs::symlink_metadata(dir.join(CONTENT)) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => forget(&root, id, &dir),
                _ => Err(Error::Invalid(format!("displaced entry `{id}` still holds content"))),
            }
        }

        /// Where the displaced content is (or goes) in the entry.
        pub(crate) fn content(&self) -> PathBuf {
            self.dir.join(CONTENT)
        }

        /// Moves `src` (the destination, or a leftover aside) into the
        /// entry. It is in the same directory tree, so this is one rename;
        /// if that fails anyway (a mount point in between), nothing moved.
        pub(crate) fn take(&mut self, src: &Path) -> Result<()> {
            rename(src, &self.content()).at(src)?;
            self.held = Some(Held::Moved);
            Ok(())
        }

        /// Hard-links the file `real` into the entry, leaving it in place;
        /// `false` if there is no such file.
        pub(super) fn link(&mut self, real: &Path) -> Result<bool> {
            match hard_link(real, &self.content()) {
                Ok(()) => self.held = Some(Held::Linked),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
                Err(e) => return Err(e).at(real),
            }
            Ok(true)
        }

        /// [`take`](Self::take) for `name` in the directory `parent`,
        /// never following a symlink to it; `false` if nothing is there.
        #[cfg(unix)]
        pub(crate) fn take_at(&mut self, parent: &std::os::fd::OwnedFd, name: &std::ffi::OsStr) -> Result<bool> {
            let content = self.content();
            match rustix::fs::renameat(parent, name, rustix::fs::CWD, &content) {
                Ok(()) => self.held = Some(Held::Moved),
                Err(rustix::io::Errno::NOENT) => return Ok(false),
                Err(e) => return Err(std::io::Error::from(e)).at(&self.dest),
            }
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
            self.keep()?;
            Err(self.displaced_error())
        }

        /// Marks the entry kept: changed, never pruned.
        fn keep(&self) -> Result<()> {
            let path = self.dir.join(META);
            let text = fs::read_to_string(&path).at(&path)?;
            let parsed = serde_json::from_str::<Meta>(&text);
            let mut meta = parsed.map_err(|e| Error::Parse { path, message: e.to_string() })?;
            meta.kept = true;
            self.write_meta(&meta)
        }

        /// Drops the entry, unless it holds content that was moved here: a
        /// hard link can go (the original is still in place), that can't, so
        /// it is kept instead.
        pub(crate) fn abandon(self) {
            if self.held == Some(Held::Moved) && fs::symlink_metadata(self.content()).is_ok() {
                let _ = self.keep();
                return;
            }
            let _ = forget(&self.root, &self.id, &self.dir);
        }

        /// After the swap: checks the content one last time.
        /// `Some(Error::Displaced)` if it changed; otherwise the entry is
        /// left for [`prune`].
        pub(crate) fn finish(self) -> Result<Option<Error>> {
            Ok((!self.intact()?).then(|| self.displaced_error()))
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
    fn forced_failure(to: &Path) -> Option<std::io::Error> {
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

type LinksIndex = BTreeMap<String, PathBuf>;

fn load_links_index(dir: &Path) -> Result<LinksIndex> {
    let path = dir.join(LINK_INDEX);
    match instructions::read_optional(&path)? {
        Some(text) => toml::from_str(&text).map_err(|e| Error::Parse { path, message: e.to_string() }),
        None => Ok(LinksIndex::new()),
    }
}

fn save_links_index(dir: &Path, index: &LinksIndex) -> Result<()> {
    let path = dir.join(LINK_INDEX);
    if index.is_empty() {
        return match fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e).at(&path),
            _ => Ok(()),
        };
    }
    let body = toml::to_string(index).map_err(|e| Error::Parse { path: path.clone(), message: e.to_string() })?;
    instructions::write_file(&path, &format!("# Skill links managed by 0xSk1ll. Do not edit.\n{body}"))
}

fn classify(repo_root: &Path, dest: &Path, assigned_source: Option<&Path>) -> Result<Existing> {
    let meta = match fs::symlink_metadata(dest) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Existing::Missing),
        Err(e) => return Err(e).at(dest),
    };
    if meta.file_type().is_symlink() {
        let to = fs::read_link(dest).at(dest)?;
        let abs = match dest.parent() {
            Some(parent) if to.is_relative() => parent.join(&to),
            _ => to,
        };
        let dir = dest.parent().ok_or_else(|| Error::Invalid(format!("{} has no parent", dest.display())))?;
        let links = load_links_index(dir)?;
        let name = dest.file_name().unwrap_or_default().to_string_lossy();
        let recorded = links.get(name.as_ref());
        let canonical = match abs.canonicalize() {
            Ok(path) => Some(path),
            // A removed repo skill leaves a dangling managed link. Resolve
            // relative targets lexically so `..` and `.` still match the
            // canonical target recorded during migration, without following
            // the missing source. Unknown dangling links stay unmanaged.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                recorded.filter(|p| lexical_normalize(p) == lexical_normalize(&abs)).cloned()
            }
            Err(e) => return Err(e).at(&abs),
        };
        if let Some(canonical) = canonical {
            let owned = recorded == Some(&canonical);
            let legacy = recorded.is_none()
                && assigned_source.map(|p| p.canonicalize().at(p)).transpose()?.as_ref() == Some(&canonical);
            if canonical.starts_with(repo_root) && (owned || legacy) {
                return Ok(Existing::ManagedLink(canonical));
            }
        }
        return Ok(Existing::Unmanaged(format!("symlink to {} not managed by sk1ll", abs.display())));
    }
    if meta.is_dir() {
        return Ok(match fs::read_to_string(dest.join(MARKER)) {
            Ok(hash) => Existing::ManagedCopy(hash.trim().to_string()),
            Err(_) => Existing::Unmanaged("existing directory not managed by sk1ll".into()),
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

/// Reject unsupported entries before creating or replacing any output.
fn validate_copy(src: &Path, skip: &[&str]) -> Result<()> {
    if !fs::symlink_metadata(src).at(src)?.is_dir() {
        return Err(Error::Invalid(format!("{} is not a skill directory", src.display())));
    }
    let walker =
        WalkDir::new(src).into_iter().filter_entry(|e| e.depth() == 0 || !skip.iter().any(|s| e.file_name() == *s));
    for entry in walker {
        let entry = entry.map_err(std::io::Error::from).at(src)?;
        let ft = entry.file_type();
        if !ft.is_dir() && !ft.is_file() && !(cfg!(unix) && ft.is_symlink()) {
            return Err(Error::Invalid(format!("unsupported skill entry: {}", entry.path().display())));
        }
    }
    Ok(())
}

fn copy_dir(src: &Path, dest: &Path, skip: &[&str]) -> Result<()> {
    validate_copy(src, skip)?;
    let walker =
        WalkDir::new(src).into_iter().filter_entry(|e| e.depth() == 0 || !skip.iter().any(|s| e.file_name() == *s));
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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn adopt_test_entry(repo: &Repo, id: &str) -> Result<PathBuf> {
        crate::git::Git::new(repo.root()).args(["init", "-q"]).run()?;
        displaced::adopt(repo, id, &crate::EngineLock::acquire(repo.root())?)
    }

    fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
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
            assert_eq!(fs::read_to_string(action.dest.join(MARKER)).unwrap(), hash_dir(&action.dest).unwrap());
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
                let marker = fs::read(dest.join(MARKER)).unwrap();
                assert!(apply(&repo, &actions[0]).is_err());
                assert_eq!(hash_dir(&dest).unwrap(), before);
                assert_eq!(fs::read(dest.join(MARKER)).unwrap(), marker);
            }
        }
    }

    /// Runs `f` with `edit` hooked in at `point`, once.
    fn with_hook<T>(point: Hook, edit: impl FnOnce(&Path) + 'static, f: impl FnOnce() -> T) -> T {
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
                .filter(|n| n.starts_with(".sk1ll-"))
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
        assert!(instructions::load_rules_index(&dir).unwrap().is_empty());
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
        let links = LinksIndex::from([("pdf".into(), "personal/pdf".into())]);
        let save = || {
            Assignment::default().save(&assignment_path).unwrap();
            adopt_rule(&repo, &incoming, "new.md").unwrap();
            save_links_index(&links_dir, &links).unwrap();
        };
        save();
        let files = [assignment_path.clone(), adopted, links_dir.join(LINK_INDEX), rules_dir.join(MARKER)];
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
        let index = fs::read(dir.join(LINK_INDEX)).unwrap();
        assignment.set_skill("h", &skill_id(&repo, "pdf"), false);
        let actions = plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap();
        fs::remove_file(dir.join("pdf")).unwrap();
        // Same canonical target and ownership, but a different observed link.
        std::os::unix::fs::symlink("../repo/personal/pdf", dir.join("pdf")).unwrap();
        assert!(apply(&repo, &actions[0]).is_err());
        assert_eq!(fs::read_link(dir.join("pdf")).unwrap(), Path::new("../repo/personal/pdf"));
        assert_eq!(fs::read(dir.join(LINK_INDEX)).unwrap(), index);
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
        assert!(!tmp.path().join("skills").join(LINK_INDEX).exists());
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
        assert!(dir.join(LINK_INDEX).is_file());
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
        assert!(!dir.join(LINK_INDEX).exists());
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
        assert!(root.join("personal/pdf/extra.md").is_file() && !root.join("personal/pdf").join(MARKER).exists());
        assert_eq!(
            fs::read_to_string(root.join("instructions/git.md")).unwrap(),
            "---\nharnesses: [h, b]\nid: git-rule\ndescription: Git\n---\n\nUse CC always.\n"
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
        write(&src.join("refs").join(MARKER), "upstream\n");
        write(&repo.root().join("sk1ll.toml"), "[import.pdf]\nsource = 'local'\n");
        repo.assign_missing_ids().unwrap();
        let plan_now = || plan(&repo, &harnesses, &Assignment::default(), PlanOptions::default()).unwrap();
        for action in plan_now() {
            apply(&repo, &action).unwrap();
        }
        let copy = tmp.path().join("skills/pdf");
        assert_eq!(fs::read_to_string(copy.join(ID_FILE)).unwrap(), "vendored\n");
        assert!(!copy.join("refs").join(MARKER).exists());
        write(&copy.join("SKILL.md"), "Harness edit.\n");
        fs::remove_file(copy.join(ID_FILE)).unwrap();
        adopt(&repo, &plan_now()[0]).unwrap();
        assert_eq!(fs::read_to_string(src.join("SKILL.md")).unwrap(), "Harness edit.\n");
        assert!(!src.join(ID_FILE).exists());
        assert_eq!(fs::read_to_string(src.join("refs").join(MARKER)).unwrap(), "upstream\n");
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
                    let result = if point == Hook::AfterAside {
                        // Paused just before the replacement rename; a note is appended by path.
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
                        false => assert_eq!(fs::read_to_string(dest.join("SKILL.md")).unwrap(), "Updated source.\n"),
                        true => assert!(fs::symlink_metadata(&dest).is_err(), "{case}"),
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
                    assert!(!personal.join(MARKER).exists());
                    assert!(displaced::list().unwrap().is_empty(), "{case}");
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
            copy_dir(&repo.personal_dir().join("pdf"), &skills.join(".sk1ll-stage-pdf"), &[]).unwrap();
            fs::write(skills.join(".sk1ll-stage-pdf").join(MARKER), "sha256:x").unwrap();
            copy_dir(&repo.personal_dir().join("pdf"), &skills.join(".sk1ll-tmp/stage-pdf"), &[]).unwrap();
            copy_dir(&skills.join(".sk1ll-old-pdf"), &skills.join(".sk1ll-tmp/old-gone"), &[]).unwrap();
            let marker = fs::read_to_string(skills.join(".sk1ll-old-pdf").join(MARKER)).unwrap();
            fs::write(skills.join(".sk1ll-tmp/old-gone").join(MARKER), &marker).unwrap();
            write(&skills.join(".sk1ll-tmp/old-gone/SKILL.md"), "Edited before the crash.\n");
            fs::rename(rules.join("git.md"), rules.join(".sk1ll-old-git.md")).unwrap();
            write(&rules.join(".git.md.sk1ll-tmp"), "staged\n");

            let actions = plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap();
            let summary: Vec<_> = actions.iter().map(|a| (a.kind, a.dest.clone())).collect();
            assert_eq!(
                summary,
                [(ActionKind::Create, skills.join("pdf")), (ActionKind::Create, rules.join("git.md"))],
                "explicit={explicit}"
            );
            if explicit {
                crate::git::Git::new(repo.root()).args(["init", "--quiet"]).run().unwrap();
                let lock = crate::EngineLock::acquire(repo.root()).unwrap();
                let rescued = recover(&lock, &harnesses).unwrap();
                let got: Vec<_> = rescued.iter().map(|e| (e.dest.clone(), e.changed)).collect();
                assert_eq!(
                    got,
                    [(skills.join("pdf"), false), (skills.join("gone"), true), (rules.join("git.md"), false)],
                    "{rescued:?}"
                );
            }
            for a in &actions {
                apply(&repo, a).unwrap();
            }
            assert!(plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap().is_empty());
            assert_eq!(fs::read_to_string(skills.join("pdf/SKILL.md")).unwrap(), "Updated source.\n");
            let mut names: Vec<_> = fs::read_dir(&skills).unwrap().map(|e| e.unwrap().file_name()).collect();
            names.sort();
            assert_eq!(names, [TMP_DIR, "pdf"], "explicit={explicit}");
            assert!(sk1ll_leftovers(&skills).is_empty(), "only recovery entries are kept");
            assert!(sk1ll_leftovers(&rules).is_empty());
            let changed = displaced::changed().unwrap();
            assert_eq!(changed.len(), 1, "{changed:?}");
            assert_eq!(fs::read_to_string(changed[0].path.join("SKILL.md")).unwrap(), "Edited before the crash.\n");
            assert_eq!(displaced::list().unwrap().len(), 3);
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
    /// directory is.
    #[cfg(unix)]
    #[test]
    fn late_writes_after_sync_reach_the_recovery_copy_wherever_the_state_dir_is() {
        use std::io::Write;
        for case in ["skill update", "skill removal", "rule update", "rule removal", "block update"] {
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
            r#"{{"harness":"h","kind":"rule","dest":{:?},"expected":"","displaced_at":0}}"#,
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
        recover(&lock, &d.harnesses).unwrap();
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
}
