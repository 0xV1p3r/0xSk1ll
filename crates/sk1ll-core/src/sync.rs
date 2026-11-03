//! Plan/apply sync of skills and instructions into harness targets.
//!
//! `plan` is side-effect free so a front-end can show (and confirm) the diff;
//! `apply` re-checks ownership/state before touching anything, so a stale plan
//! can never delete or overwrite something sk1ll doesn't manage.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use globset::{Glob, GlobSet, GlobSetBuilder};
use serde::Serialize;
use walkdir::WalkDir;

use crate::hash::{hash_dir, hash_str};
use crate::instructions::{self, Fragment};
use crate::manifest::{InstructionsFormat, LinkMode, Target};
use crate::skill::Skill;
use crate::{Error, IoExt, Repo, Result};

/// Written inside copied skill directories (holds the content hash) and in
/// rules directories (holds the index of managed rule files).
pub const MARKER: &str = ".sk1ll-managed";

const EDITED: &str = "edited by hand in the harness; move the edits into instructions/ or use --force";

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
    },
    Instructions {
        fragments: Vec<String>,
        content: Option<String>,
        /// Hash of the managed block as seen when planning; `apply` refuses
        /// to write if the file changed since.
        expected: Option<String>,
    },
    /// One fragment as a file in a rules directory.
    Rule {
        file: String,
        /// `None` removes the file.
        content: Option<String>,
        /// Hash of the file as seen when planning (`None`: absent).
        expected: Option<String>,
    },
}

/// A file a target would receive, for previewing.
#[derive(Debug, Clone, Serialize)]
pub struct RenderedFile {
    pub path: PathBuf,
    pub content: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Action {
    pub target: String,
    pub kind: ActionKind,
    pub dest: PathBuf,
    pub item: Item,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct PlanOptions {
    /// Overwrite hand edits inside managed instruction blocks. Never affects
    /// unmanaged skill directories.
    pub force: bool,
}

enum Existing {
    Missing,
    ManagedLink(PathBuf),
    ManagedCopy(String),
    Unmanaged(String),
}

/// What a profile selects, resolved against the repo.
struct Selection {
    skills: Vec<Skill>,
    fragments: Vec<Fragment>,
    targets: Vec<String>,
}

impl Selection {
    fn new(repo: &Repo, profile_name: &str) -> Result<Self> {
        let profile = repo.manifest.profile(profile_name)?;
        let skill_match = Matcher::new(&profile.skills, &profile.exclude_skills)?;
        let fragment_match = Matcher::new(&profile.instructions, &profile.exclude_instructions)?;
        let targets = if profile.targets.is_empty() {
            repo.manifest.targets.keys().cloned().collect()
        } else {
            profile.targets.clone()
        };
        if let Some(unknown) = targets.iter().find(|t| !repo.manifest.targets.contains_key(*t)) {
            return Err(Error::Invalid(format!(
                "profile `{profile_name}` references unknown target `{unknown}`"
            )));
        }
        Ok(Self {
            skills: repo.skills()?.into_iter().filter(|s| skill_match.is_match(&s.name)).collect(),
            fragments: repo.fragments()?.into_iter().filter(|f| fragment_match.is_match(&f.name)).collect(),
            targets,
        })
    }

    fn fragments_for(&self, target: &str) -> Vec<&Fragment> {
        self.fragments.iter().filter(|f| f.applies_to(target)).collect()
    }
}

pub fn plan(repo: &Repo, profile_name: &str, opts: PlanOptions) -> Result<Vec<Action>> {
    let sel = Selection::new(repo, profile_name)?;
    let mut actions = Vec::new();
    for tname in &sel.targets {
        let target = &repo.manifest.targets[tname];
        if let Some(dir) = target.skills_dir() {
            plan_skills(repo, tname, target, &dir, &sel.skills, &mut actions)?;
        }
        if let Some(path) = target.instructions_file() {
            let fragments = sel.fragments_for(tname);
            match target.instructions_format {
                InstructionsFormat::Block => plan_instructions(tname, &path, &fragments, opts, &mut actions)?,
                InstructionsFormat::Rules => plan_rules(tname, &path, &fragments, opts, &mut actions)?,
            }
        }
    }
    Ok(actions)
}

/// The instruction files a target would receive, for previewing.
pub fn render_instructions(repo: &Repo, profile_name: &str, target: &str) -> Result<Vec<RenderedFile>> {
    let sel = Selection::new(repo, profile_name)?;
    let t = repo
        .manifest
        .targets
        .get(target)
        .ok_or_else(|| Error::Invalid(format!("unknown target `{target}`")))?;
    let Some(path) = t.instructions_file() else {
        return Ok(Vec::new());
    };
    let fragments = sel.fragments_for(target);
    Ok(match t.instructions_format {
        InstructionsFormat::Block => instructions::render(&fragments)
            .map(|content| RenderedFile { path, content })
            .into_iter()
            .collect(),
        InstructionsFormat::Rules => fragments
            .iter()
            .map(|f| {
                let (file, content) = instructions::render_rule(f);
                RenderedFile { path: path.join(file), content }
            })
            .collect(),
    })
}

pub fn apply(repo: &Repo, action: &Action) -> Result<()> {
    match &action.item {
        Item::Skill { name, mode, source } => apply_skill(repo, action, name, *mode, source.as_deref()),
        Item::Instructions { content, expected, .. } => {
            apply_instructions(action, content.as_deref(), expected.as_deref())
        }
        Item::Rule { file, content, expected } => apply_rule(action, file, content.as_deref(), expected.as_deref()),
    }
}

fn plan_skills(
    repo: &Repo,
    tname: &str,
    target: &Target,
    dir: &Path,
    skills: &[Skill],
    actions: &mut Vec<Action>,
) -> Result<()> {
    let action = |name: &str, kind, source: Option<&Path>, reason: Option<String>| Action {
        target: tname.to_string(),
        kind,
        dest: dir.join(name),
        item: Item::Skill { name: name.to_string(), mode: target.mode, source: source.map(Path::to_path_buf) },
        reason,
    };

    let mut desired = BTreeSet::new();
    for skill in skills {
        desired.insert(skill.name.as_str());
        let kind = match classify(repo.root(), &dir.join(&skill.name))? {
            Existing::Missing => Some((ActionKind::Create, None)),
            Existing::ManagedLink(to) if target.mode == LinkMode::Symlink && to == skill.path => None,
            Existing::ManagedCopy(hash) if target.mode == LinkMode::Copy && hash == hash_dir(&skill.path)? => None,
            Existing::ManagedLink(_) | Existing::ManagedCopy(_) => Some((ActionKind::Update, None)),
            Existing::Unmanaged(why) => Some((ActionKind::Conflict, Some(why))),
        };
        if let Some((kind, reason)) = kind {
            actions.push(action(&skill.name, kind, Some(&skill.path), reason));
        }
    }

    if dir.is_dir() {
        for entry in fs::read_dir(dir).at(dir)? {
            let entry = entry.at(dir)?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if desired.contains(name.as_str()) {
                continue;
            }
            if let Existing::ManagedLink(_) | Existing::ManagedCopy(_) = classify(repo.root(), &entry.path())? {
                actions.push(action(&name, ActionKind::Remove, None, None));
            }
        }
    }
    Ok(())
}

fn plan_instructions(
    tname: &str,
    file: &Path,
    fragments: &[&Fragment],
    opts: PlanOptions,
    actions: &mut Vec<Action>,
) -> Result<()> {
    let content = instructions::render(fragments);
    let text = instructions::read_optional(file)?;
    let block = match &text {
        Some(text) => instructions::find_block(text, file)?,
        None => None,
    };
    let (kind, reason) = match (&block, &content) {
        (None, None) => return Ok(()),
        (Some(b), Some(c)) if b.inner == *c => return Ok(()),
        (Some(b), _) if b.edited() && !opts.force => (ActionKind::Conflict, Some(format!("managed block {EDITED}"))),
        (None, Some(_)) => (ActionKind::Create, None),
        (Some(_), Some(_)) => (ActionKind::Update, None),
        (Some(_), None) => (ActionKind::Remove, None),
    };
    actions.push(Action {
        target: tname.to_string(),
        kind,
        dest: file.to_path_buf(),
        item: Item::Instructions {
            fragments: fragments.iter().map(|f| f.name.clone()).collect(),
            content,
            expected: block.map(|b| hash_str(&b.inner)),
        },
        reason,
    });
    Ok(())
}

fn plan_rules(
    tname: &str,
    dir: &Path,
    fragments: &[&Fragment],
    opts: PlanOptions,
    actions: &mut Vec<Action>,
) -> Result<()> {
    let index = instructions::load_rules_index(dir)?;
    let action = |file: &str, kind, content: Option<String>, expected: Option<String>, reason: Option<String>| Action {
        target: tname.to_string(),
        kind,
        dest: dir.join(file),
        item: Item::Rule { file: file.to_string(), content, expected },
        reason,
    };
    let current_hash = |file: &str| -> Result<Option<String>> {
        Ok(instructions::read_optional(&dir.join(file))?.map(|t| hash_str(&t)))
    };

    let mut desired = BTreeSet::new();
    for fragment in fragments {
        let (file, content) = instructions::render_rule(fragment);
        let want = hash_str(&content);
        let current = current_hash(&file)?;
        let (kind, reason) = match (&current, index.get(&file)) {
            (None, _) => (ActionKind::Create, None),
            (Some(cur), Some(rec)) if *cur == want && *rec == want => {
                desired.insert(file);
                continue;
            }
            (Some(_), None) => (ActionKind::Conflict, Some("existing rule file not managed by sk1ll".into())),
            (Some(cur), Some(_)) if *cur == want => (ActionKind::Update, None),
            (Some(cur), Some(rec)) if cur != rec && !opts.force => (ActionKind::Conflict, Some(format!("rule {EDITED}"))),
            (Some(_), Some(_)) => (ActionKind::Update, None),
        };
        actions.push(action(&file, kind, Some(content), current, reason));
        desired.insert(file);
    }

    for (file, recorded) in &index {
        if desired.contains(file) {
            continue;
        }
        let current = current_hash(file)?;
        let (kind, reason) = match &current {
            Some(cur) if cur != recorded && !opts.force => (ActionKind::Conflict, Some(format!("rule {EDITED}"))),
            _ => (ActionKind::Remove, None),
        };
        actions.push(action(file, kind, None, current, reason));
    }
    Ok(())
}

fn apply_rule(action: &Action, file: &str, content: Option<&str>, expected: Option<&str>) -> Result<()> {
    if action.kind == ActionKind::Conflict {
        return Ok(());
    }
    let dest = &action.dest;
    let dir = dest.parent().ok_or_else(|| Error::Invalid(format!("{} has no parent", dest.display())))?;
    let current = instructions::read_optional(dest)?.map(|t| hash_str(&t));
    if current.as_deref() != expected {
        return Err(Error::Invalid(format!("{} changed since the plan was made; re-run sync", dest.display())));
    }
    let mut index = instructions::load_rules_index(dir)?;
    match content {
        Some(content) => {
            instructions::write_file(dest, content)?;
            index.insert(file.to_string(), hash_str(content));
        }
        None => {
            if current.is_some() {
                fs::remove_file(dest).at(dest)?;
            }
            index.remove(file);
        }
    }
    instructions::save_rules_index(dir, &index)
}

fn apply_instructions(action: &Action, content: Option<&str>, expected: Option<&str>) -> Result<()> {
    if action.kind == ActionKind::Conflict {
        return Ok(());
    }
    let dest = &action.dest;
    let text = instructions::read_optional(dest)?.unwrap_or_default();
    let block = instructions::find_block(&text, dest)?;
    if block.as_ref().map(|b| hash_str(&b.inner)).as_deref() != expected {
        return Err(Error::Invalid(format!("{} changed since the plan was made; re-run sync", dest.display())));
    }
    instructions::write_file(dest, &instructions::splice(&text, block.as_ref(), content))
}

fn apply_skill(repo: &Repo, action: &Action, name: &str, mode: LinkMode, source: Option<&Path>) -> Result<()> {
    let dest = &action.dest;
    let managed = match classify(repo.root(), dest)? {
        Existing::Missing => false,
        Existing::ManagedLink(_) | Existing::ManagedCopy(_) => true,
        Existing::Unmanaged(why) => {
            if action.kind == ActionKind::Conflict {
                return Ok(());
            }
            return Err(Error::Invalid(format!("refusing to touch {}: {why}", dest.display())));
        }
    };
    match action.kind {
        ActionKind::Conflict => Ok(()),
        ActionKind::Remove => {
            if managed {
                remove_entry(dest)?;
            }
            Ok(())
        }
        ActionKind::Create | ActionKind::Update => {
            let src = source.ok_or_else(|| Error::Invalid(format!("no source for {name}")))?;
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent).at(parent)?;
            }
            if managed {
                remove_entry(dest)?;
            }
            match mode {
                LinkMode::Symlink => symlink_dir(src, dest).at(dest),
                LinkMode::Copy => {
                    copy_dir(src, dest)?;
                    let marker = dest.join(MARKER);
                    fs::write(&marker, hash_dir(src)?).at(&marker)
                }
            }
        }
    }
}

fn classify(repo_root: &Path, dest: &Path) -> Result<Existing> {
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
        return Ok(if abs.starts_with(repo_root) {
            Existing::ManagedLink(abs)
        } else {
            Existing::Unmanaged(format!("symlink to {} not managed by sk1ll", abs.display()))
        });
    }
    if meta.is_dir() {
        return Ok(match fs::read_to_string(dest.join(MARKER)) {
            Ok(hash) => Existing::ManagedCopy(hash.trim().to_string()),
            Err(_) => Existing::Unmanaged("existing directory not managed by sk1ll".into()),
        });
    }
    Ok(Existing::Unmanaged("existing file".into()))
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

fn copy_dir(src: &Path, dest: &Path) -> Result<()> {
    for entry in WalkDir::new(src).into_iter().filter_entry(|e| e.file_name() != ".git") {
        let entry = entry.map_err(std::io::Error::from).at(src)?;
        let rel = entry.path().strip_prefix(src).unwrap_or(entry.path());
        let out = dest.join(rel);
        if entry.file_type().is_dir() {
            fs::create_dir_all(&out).at(&out)?;
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

struct Matcher {
    include: GlobSet,
    exclude: GlobSet,
}

impl Matcher {
    fn new(include: &[String], exclude: &[String]) -> Result<Self> {
        Ok(Self { include: globset(include)?, exclude: globset(exclude)? })
    }

    fn is_match(&self, name: &str) -> bool {
        self.include.is_match(name) && !self.exclude.is_match(name)
    }
}

fn globset(patterns: &[String]) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for p in patterns {
        builder.add(Glob::new(p).map_err(|e| Error::Invalid(format!("bad glob `{p}`: {e}")))?);
    }
    builder.build().map_err(|e| Error::Invalid(e.to_string()))
}
