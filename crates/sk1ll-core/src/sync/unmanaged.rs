//! Unmanaged skills, grouped by folder name and content hash.
use serde::{Deserialize, Serialize};

use super::*;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Location {
    pub harness: String,
    pub path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Variant {
    pub hash: String,
    pub description: Option<String>,
    pub locations: Vec<Location>,
    /// Identical to the existing personal copy: an interrupted Adopt of this
    /// variant that Adopt can resume.
    #[serde(default)]
    pub resumable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnmanagedSkill {
    pub name: String,
    pub description: Option<String>,
    /// A repo skill has this name and no variant is `resumable`. When one is,
    /// only the resumable variants can be adopted.
    pub clashes_with_repo: bool,
    pub variants: Vec<Variant>,
}

/// Read-only discovery. Rules and global instructions are deliberately absent.
pub fn discover(repo: &Repo, harnesses: &BTreeMap<String, Harness>) -> Result<Vec<UnmanagedSkill>> {
    let skills = repo.skills()?;
    let mut found = BTreeMap::<String, BTreeMap<String, Variant>>::new();
    for (harness, config) in harnesses {
        let Some(dir) = config.skills_dir() else {
            continue;
        };
        if !dir.is_dir() {
            continue;
        }
        let index = load_index(&dir)?.owned(&dir)?;
        for entry in fs::read_dir(&dir).at(&dir)? {
            let entry = entry.at(&dir)?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            let path = entry.path();
            if !path.join("SKILL.md").is_file() {
                continue;
            }
            if !matches!(classify(repo.root(), &path, None, index.get(&name))?, Existing::Unmanaged(_)) {
                continue;
            }
            let canonical = path.canonicalize().at(&path)?;
            match refuse_source_marker(&canonical) {
                Ok(()) => {}
                Err(Error::Invalid(_)) => continue,
                Err(e) => return Err(e),
            }
            let hash = hash_dir(&canonical)?;
            let variant = found.entry(name).or_default().entry(hash.clone()).or_insert_with(|| Variant {
                hash,
                description: fs::read_to_string(canonical.join("SKILL.md"))
                    .ok()
                    .and_then(|t| crate::skill::frontmatter_field(&t, "description")),
                locations: Vec::new(),
                resumable: false,
            });
            variant.locations.push(Location { harness: harness.clone(), path });
        }
    }
    Ok(found
        .into_iter()
        .map(|(name, variants)| {
            let mut variants: Vec<_> = variants.into_values().collect();
            // An identical personal copy is an interrupted Adopt that can be retried.
            let dest = repo.personal_dir().join(&name);
            let copy = (dest.symlink_metadata().is_ok_and(|m| m.is_dir())
                && repo.imported_dir().join(&name).symlink_metadata().is_err()
                && !repo.manifest.import.contains_key(&name))
            .then(|| hash_dir(&dest).ok())
            .flatten();
            for variant in &mut variants {
                variant.resumable = copy.as_ref() == Some(&variant.hash);
            }
            let resumable = variants.iter().any(|v| v.resumable);
            UnmanagedSkill {
                description: variants.first().and_then(|v| v.description.clone()),
                clashes_with_repo: !resumable && skills.iter().any(|s| s.name == name),
                name,
                variants,
            }
        })
        .collect())
}

/// A selected variant and the identical harness copies to enable and replace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Adoption {
    pub name: String,
    pub hash: String,
    pub locations: Vec<Location>,
}

/// A completed adoption can be retried after losing the local API response.
/// Only a skill still added in the draft qualifies, so Adopt on an ordinary
/// in-sync skill never pins its Assignment. Hand edits to managed content
/// still use the ordinary conflict Adopt path.
pub fn adopted(
    repo: &Repo,
    harnesses: &BTreeMap<String, Harness>,
    harness: &str,
    name: &str,
) -> Result<Option<Adoption>> {
    let added = crate::ItemRef::Skill(name.into());
    if !repo.draft()?.iter().any(|d| d.item == added && d.change == crate::Change::Added) {
        return Ok(None);
    }
    let Some(skill) = repo.skills()?.into_iter().find(|s| s.name == name && s.path == repo.personal_dir().join(name))
    else {
        return Ok(None);
    };
    let Some(path) = harnesses.get(harness).and_then(|h| h.skills_dir()).map(|p| p.join(name)) else {
        return Ok(None);
    };
    if !matches!(
        classify(repo.root(), &path, None, index_entry(&path)?.as_ref())?,
        Existing::ManagedCopy(_) | Existing::ManagedLink(_)
    ) {
        return Ok(None);
    }
    let hash = hash_dir(&skill.path)?;
    let locations: Vec<_> = harnesses
        .iter()
        .filter_map(|(harness, h)| {
            let path = h.skills_dir()?.join(name);
            let canonical = path.canonicalize().ok()?;
            if refuse_source_marker(&canonical).is_err()
                && !matches!(
                    classify(repo.root(), &path, None, index_entry(&path).ok()?.as_ref()).ok()?,
                    Existing::ManagedCopy(_) | Existing::ManagedLink(_)
                )
            {
                return None;
            }
            (hash_dir(&canonical).ok()? == hash).then(|| Location { harness: harness.clone(), path })
        })
        .collect();
    Ok(locations.iter().any(|l| l.harness == harness).then(|| Adoption { name: name.into(), hash, locations }))
}

/// Selects and validates a variant without publishing or touching harness content.
/// An identical personal copy is accepted so interrupted online adoption can retry.
pub fn select(repo: &Repo, harnesses: &BTreeMap<String, Harness>, harness: &str, name: &str) -> Result<Adoption> {
    let mut components = Path::new(name).components();
    if !matches!(components.next(), Some(std::path::Component::Normal(_)))
        || components.next().is_some()
        || name.starts_with('.')
    {
        return Err(Error::Invalid("skill name must be a folder name".into()));
    }
    if let Some(dir) = harnesses.get(harness).and_then(Harness::skills_dir) {
        let path = dir.join(name);
        if matches!(classify(repo.root(), &path, None, index_entry(&path)?.as_ref())?, Existing::Unmanaged(_)) {
            refuse_source_marker(&path)?;
        }
    }
    let skill = discover(repo, harnesses)?
        .into_iter()
        .find(|s| s.name == name)
        .ok_or_else(|| Error::Invalid(format!("no unmanaged skill `{name}` in `{harness}`")))?;
    let variant = skill
        .variants
        .into_iter()
        .find(|v| v.locations.iter().any(|l| l.harness == harness))
        .ok_or_else(|| Error::Invalid(format!("no unmanaged skill `{name}` in `{harness}`")))?;
    let dest = repo.personal_dir().join(name);
    let identical = dest.symlink_metadata().is_ok_and(|m| m.is_dir()) && hash_dir(&dest)? == variant.hash;
    if repo.imported_dir().join(name).symlink_metadata().is_ok()
        || repo.manifest.import.contains_key(name)
        || (dest.symlink_metadata().is_ok() && !identical)
    {
        return Err(Error::Invalid(format!("a repo skill named `{name}` already exists")));
    }
    Ok(Adoption { name: name.into(), hash: variant.hash, locations: variant.locations })
}

/// Publish a selected adoption. Retry accepts only a
/// byte-identical existing destination; no existing content is removed.
pub fn publish(repo: &Repo, adoption: &Adoption) -> Result<()> {
    let dest = repo.personal_dir().join(&adoption.name);
    if dest.symlink_metadata().is_ok() {
        if !dest.symlink_metadata().at(&dest)?.is_symlink() && hash_dir(&dest)? == adoption.hash {
            return Ok(());
        }
        return Err(Error::Invalid("adoption destination changed; leaving its content alone".into()));
    }
    for location in &adoption.locations {
        refuse_source_marker(&location.path)?;
    }
    let canonical = adoption
        .locations
        .iter()
        .find_map(|l| {
            let path = l.path.canonicalize().ok()?;
            (hash_dir(&path).ok()? == adoption.hash).then_some(path)
        })
        .ok_or_else(|| Error::Invalid("adoption source changed; retry Adopt".into()))?;
    let parent = repo.personal_dir();
    fs::create_dir_all(&parent).at(&parent)?;
    let stage = crate::source::unique(&parent);
    fs::create_dir(&stage).at(&stage)?;
    let copied = (|| {
        refuse_source_marker(&canonical)?;
        copy_dir(&canonical, &stage, excluded_from_personal_skill)?;
        refuse_source_marker(&canonical)?;
        if hash_dir(&stage)? != adoption.hash {
            return Err(Error::Invalid("skill changed while being adopted; retry".into()));
        }
        repo.new_item_id()?.write_skill(&stage)?;
        // Persist the complete copy before making it visible in the draft.
        durable_tree(&stage)?;
        crate::failpoint!(BeforeSwap, &dest);
        rename_noreplace(&stage, &dest).at(&dest)?;
        #[cfg(unix)]
        fs::File::open(&parent).at(&parent)?.sync_all().at(&parent)?;
        Ok(())
    })();
    if stage.exists() {
        fs::remove_dir_all(&stage).at(&stage)?;
    }
    copied
}

/// What [`rollback`] did with the copy a refused Adopt published.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RolledBack {
    /// Taken back out of the Draft.
    Removed,
    /// Not there any more.
    Absent,
    /// Kept where it is, for this reason.
    Kept(&'static str),
}

/// Roll back only our new copy: while it is still an uncommitted addition
/// with the Item ID `id` that Adopt gave it, and its bytes still match a
/// harness source. Keep concurrent edits, a commit made meanwhile and any
/// extra metadata that the content hash excludes; in doubt, keep it.
pub fn rollback(repo: &Repo, adoption: &Adoption, id: &crate::ItemId) -> Result<RolledBack> {
    let dest = repo.personal_dir().join(&adoption.name);
    if !dest.symlink_metadata().is_ok_and(|m| m.is_dir()) {
        return Ok(RolledBack::Absent);
    }
    let added = crate::ItemRef::Skill(adoption.name.clone());
    if !repo.draft()?.iter().any(|d| d.item == added && d.change == crate::Change::Added) {
        return Ok(RolledBack::Kept("it was committed meanwhile"));
    }
    if crate::ItemId::read_skill(&dest).ok().flatten().as_ref() != Some(id) {
        return Ok(RolledBack::Kept("its Item ID changed meanwhile"));
    }
    if walkdir::WalkDir::new(&dest)
        .into_iter()
        .any(|entry| entry.map_or(true, |e| e.depth() > 0 && excluded_from_skill(e.depth(), e.file_name())))
        || hash_dir(&dest)? != adoption.hash
    {
        return Ok(RolledBack::Kept("it changed meanwhile"));
    }
    let unchanged_source = adoption
        .locations
        .iter()
        .any(|l| l.path.canonicalize().ok().and_then(|p| hash_dir(&p).ok()).as_ref() == Some(&adoption.hash));
    if unchanged_source {
        // Use the existing displacement check so a concurrent save, including
        // one through an open descriptor, remains recoverable after rollback.
        let expected = observe_skill(&dest)?.state;
        if hash_dir(&dest)? != adoption.hash {
            return Ok(RolledBack::Kept("it changed meanwhile"));
        }
        let displace = Displace {
            repo,
            kind: displaced::Kind::Skill,
            harness: "adopt",
            expected: expected.as_deref(),
            ownership: None,
            baseline: None,
            adopting: None,
            keep: false,
            overwritten: false,
        };
        if let Some(error) = remove_displacing(&dest, displace)? {
            return Err(error);
        }
        return Ok(RolledBack::Removed);
    }
    Ok(RolledBack::Kept("the harness copy it came from changed meanwhile"))
}

pub(super) fn durable_tree(path: &Path) -> Result<()> {
    for entry in walkdir::WalkDir::new(path).contents_first(true) {
        let entry = entry.map_err(std::io::Error::from).at(path)?;
        if entry.file_type().is_file() || (cfg!(unix) && entry.file_type().is_dir()) {
            fs::File::open(entry.path()).at(entry.path())?.sync_all().at(entry.path())?;
        }
    }
    Ok(())
}

/// Selects and publishes in one step, without Assignment or replacement.
pub fn prepare(repo: &Repo, harnesses: &BTreeMap<String, Harness>, harness: &str, name: &str) -> Result<Adoption> {
    let adoption = select(repo, harnesses, harness, name)?;
    publish(repo, &adoption)?;
    Ok(adoption)
}

/// Replaces only the copies the user adopted, using sync's normal displacement
/// mechanism. Changed variants remain unmanaged and become ordinary conflicts.
/// Refuses ([`Error::PullPending`]) while a pull is pending, like every
/// placement.
pub fn complete(repo: &Repo, harnesses: &BTreeMap<String, Harness>, adoption: &Adoption) -> Result<()> {
    repo.require_no_pending_pull()?;
    let source = repo.personal_dir().join(&adoption.name);
    if !source.join("SKILL.md").is_file() {
        return Ok(());
    }
    for location in &adoption.locations {
        let Some(config) = harnesses.get(&location.harness) else {
            continue;
        };
        let Some(dir) = config.skills_dir() else {
            continue;
        };
        let dest = dir.join(&adoption.name);
        if dest != location.path {
            continue;
        }
        let expected_index = index_entry(&dest)?;
        if !matches!(classify(repo.root(), &dest, None, expected_index.as_ref())?, Existing::Unmanaged(_)) {
            continue;
        }
        let canonical = dest.canonicalize().at(&dest)?;
        refuse_source_marker(&canonical)?;
        if hash_dir(&canonical)? != adoption.hash {
            continue;
        }
        let expected = observe_skill(&dest)?.state;
        refuse_source_marker(&canonical)?;
        if hash_dir(&canonical)? != adoption.hash {
            continue;
        }
        let action = Action {
            harness: location.harness.clone(),
            kind: ActionKind::Update,
            dest,
            item: Item::Skill {
                name: adoption.name.clone(),
                mode: config.mode,
                source: Some(source.clone()),
                item_id: repo.resolved_ids()?.get(source.strip_prefix(repo.root()).unwrap()).cloned().flatten(),
                expected,
                metadata_only: false,
                expected_index,
            },
            reason: None,
        };
        apply_skill(repo, &action, true, None)?;
    }
    Ok(())
}
