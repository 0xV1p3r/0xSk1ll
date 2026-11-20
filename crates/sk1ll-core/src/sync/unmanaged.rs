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
            let source = skills.iter().find(|s| s.name == name).map(|s| s.path.as_path());
            if !matches!(classify(repo.root(), &path, source)?, Existing::Unmanaged(_)) {
                continue;
            }
            let canonical = path.canonicalize().at(&path)?;
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
    if !matches!(classify(repo.root(), &path, Some(&skill.path))?, Existing::ManagedCopy(_) | Existing::ManagedLink(_))
    {
        return Ok(None);
    }
    let hash = hash_dir(&skill.path)?;
    let locations: Vec<_> = harnesses
        .iter()
        .filter_map(|(harness, h)| {
            let path = h.skills_dir()?.join(name);
            let canonical = path.canonicalize().ok()?;
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
        copy_dir(&canonical, &stage, COPY_SKIP)?;
        if hash_dir(&stage)? != adoption.hash {
            return Err(Error::Invalid("skill changed while being adopted; retry".into()));
        }
        ItemId::generate().write_skill(&stage)?;
        // Persist the complete copy before making it visible in the draft.
        durable_tree(&stage)?;
        #[cfg(test)]
        hook(Hook::BeforeSwap, &dest);
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

/// Roll back only our new copy, while its bytes still match a harness source.
/// Keep concurrent edits and any extra metadata that the content hash excludes.
pub fn rollback(repo: &Repo, adoption: &Adoption) -> Result<bool> {
    let dest = repo.personal_dir().join(&adoption.name);
    if !dest.symlink_metadata().is_ok_and(|m| m.is_dir()) {
        return Ok(false);
    }
    if walkdir::WalkDir::new(&dest)
        .into_iter()
        .any(|entry| entry.map_or(true, |e| e.file_name() == ".git" || e.file_name() == MARKER))
        || hash_dir(&dest)? != adoption.hash
    {
        return Ok(false);
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
            return Ok(false);
        }
        if let Some(error) = remove_displacing(displaced::Kind::Skill, "adopt", &dest, expected.as_deref())? {
            return Err(error);
        }
    }
    Ok(unchanged_source)
}

fn durable_tree(path: &Path) -> Result<()> {
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
pub fn complete(repo: &Repo, harnesses: &BTreeMap<String, Harness>, adoption: &Adoption) -> Result<()> {
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
        if !matches!(classify(repo.root(), &dest, Some(&source))?, Existing::Unmanaged(_)) {
            continue;
        }
        let canonical = dest.canonicalize().at(&dest)?;
        if hash_dir(&canonical)? != adoption.hash {
            continue;
        }
        let expected = observe_skill(&dest)?.state;
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
                expected,
            },
            reason: None,
        };
        apply_skill(repo, &action, true, None)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (tempfile::TempDir, Repo, BTreeMap<String, Harness>) {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repo::init(&tmp.path().join("repo")).unwrap();
        let harnesses = ["a", "b", "c", "d"]
            .into_iter()
            .map(|name| {
                (
                    name.into(),
                    Harness {
                        skills: Some(tmp.path().join(name).to_string_lossy().into_owned()),
                        mode: LinkMode::Copy,
                        ..Default::default()
                    },
                )
            })
            .collect();
        (tmp, repo, harnesses)
    }
    fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    #[test]
    fn groups_identical_copies_and_excludes_managed_skills_and_rules() {
        let (tmp, repo, hs) = setup();
        for name in ["a", "b"] {
            write(&tmp.path().join(name).join("pdf/SKILL.md"), "---\ndescription: PDFs\n---\none");
        }
        write(&tmp.path().join("c/pdf/SKILL.md"), "two");
        write(&tmp.path().join("a/rule.md"), "not a skill");
        write(&tmp.path().join("a/marked/SKILL.md"), "managed");
        write(&tmp.path().join("a/marked/.sk1ll-managed"), "hash");
        let items = discover(&repo, &hs).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].variants.len(), 2);
        let identical = items[0].variants.iter().find(|v| v.locations.len() == 2).unwrap();
        assert_eq!(identical.description.as_deref(), Some("PDFs"));
        write(&repo.personal_dir().join("pdf/SKILL.md"), "repo");
        assert!(discover(&repo, &hs).unwrap()[0].clashes_with_repo);
        assert!(prepare(&repo, &hs, "a", "pdf").unwrap_err().to_string().contains("already exists"));
    }

    #[test]
    fn adopts_only_chosen_variant_preserves_originals_in_recovery_and_respects_defaults() {
        let (tmp, repo, hs) = setup();
        let _recovery = displaced::test_root(&tmp.path().join("recovery"));
        for h in ["a", "b"] {
            write(&tmp.path().join(h).join("pdf/SKILL.md"), "same");
        }
        write(&tmp.path().join("a/pdf/.sk1ll-id"), "from-elsewhere\n");
        write(&tmp.path().join("c/pdf/SKILL.md"), "different");
        let adoption = prepare(&repo, &hs, "a", "pdf").unwrap();
        assert_eq!(adoption.locations.len(), 2);
        let id = ItemId::read_skill(&repo.personal_dir().join("pdf")).unwrap();
        assert!(ulid::Ulid::from_string(&id.to_string()).is_ok());
        prepare(&repo, &hs, "a", "pdf").unwrap();
        assert_eq!(ItemId::read_skill(&repo.personal_dir().join("pdf")), Some(id));
        // The identical draft copy is a retryable Adopt, not a clash.
        assert!(!discover(&repo, &hs).unwrap()[0].clashes_with_repo);
        let mut assignment = Assignment::default();
        for h in hs.keys() {
            assignment.set_new_skills_default(h, false);
        }
        for l in &adoption.locations {
            assignment.set_skill(&l.harness, "pdf", true);
        }
        complete(&repo, &hs, &adoption).unwrap();
        for action in plan(&repo, &hs, &assignment, PlanOptions::default()).unwrap() {
            apply(&repo, &action).unwrap();
        }
        assert!(tmp.path().join("a/pdf/.sk1ll-managed").is_file());
        assert!(tmp.path().join("b/pdf/.sk1ll-managed").is_file());
        assert_eq!(fs::read_to_string(tmp.path().join("c/pdf/SKILL.md")).unwrap(), "different");
        assert!(!tmp.path().join("d/pdf").exists());
        let recovery = displaced::list().unwrap();
        assert_eq!(recovery.len(), 2);
        for entry in recovery {
            assert_eq!(fs::read_to_string(entry.path.join("SKILL.md")).unwrap(), "same");
        }
        let remaining = discover(&repo, &hs).unwrap();
        assert_eq!(remaining[0].variants[0].locations[0].harness, "c");
        assignment.set_skill("c", "pdf", true);
        assert!(
            plan(&repo, &hs, &assignment, PlanOptions::default())
                .unwrap()
                .iter()
                .any(|a| a.harness == "c" && a.kind == ActionKind::Conflict)
        );
        assert!(prepare(&repo, &hs, "a", "../outside").is_err());
    }

    #[test]
    fn only_the_variant_matching_the_personal_copy_is_resumable() {
        let (tmp, repo, hs) = setup();
        write(&tmp.path().join("a/pdf/SKILL.md"), "same");
        write(&tmp.path().join("b/pdf/SKILL.md"), "different");
        assert!(discover(&repo, &hs).unwrap()[0].variants.iter().all(|v| !v.resumable));
        // An Adopt interrupted after publishing leaves an identical personal copy.
        prepare(&repo, &hs, "a", "pdf").unwrap();
        let skill = discover(&repo, &hs).unwrap().remove(0);
        assert!(!skill.clashes_with_repo);
        let resumable: Vec<_> =
            skill.variants.iter().filter(|v| v.resumable).flat_map(|v| &v.locations).map(|l| &l.harness).collect();
        assert_eq!(resumable, ["a"]);
        assert!(select(&repo, &hs, "a", "pdf").is_ok());
        assert!(select(&repo, &hs, "b", "pdf").unwrap_err().to_string().contains("already exists"));
        // An edited personal copy is a genuine clash.
        write(&repo.personal_dir().join("pdf/SKILL.md"), "edited");
        let skill = discover(&repo, &hs).unwrap().remove(0);
        assert!(skill.clashes_with_repo && skill.variants.iter().all(|v| !v.resumable));
    }

    #[test]
    fn only_a_skill_added_in_the_draft_counts_as_adopted() {
        let (tmp, repo, hs) = setup();
        write(&tmp.path().join("a/pdf/SKILL.md"), "same");
        let adoption = prepare(&repo, &hs, "a", "pdf").unwrap();
        complete(&repo, &hs, &adoption).unwrap();
        assert!(adopted(&repo, &hs, "a", "pdf").unwrap().is_some());
        repo.commit(&[crate::ItemRef::Skill("pdf".into())], "commit pdf", Some("t <t@example.com>")).unwrap();
        assert!(adopted(&repo, &hs, "a", "pdf").unwrap().is_none());
    }

    #[test]
    fn edits_after_prepare_remain_unmanaged() {
        let (tmp, repo, hs) = setup();
        write(&tmp.path().join("a/pdf/SKILL.md"), "one");
        let adoption = prepare(&repo, &hs, "a", "pdf").unwrap();
        write(&tmp.path().join("a/pdf/SKILL.md"), "edited meanwhile");
        complete(&repo, &hs, &adoption).unwrap();
        assert!(!tmp.path().join("a/pdf/.sk1ll-managed").exists());
        assert_eq!(fs::read_to_string(tmp.path().join("a/pdf/SKILL.md")).unwrap(), "edited meanwhile");
    }

    #[test]
    fn adopting_never_overwrites_repo_content_created_while_copying() {
        let (tmp, repo, hs) = setup();
        write(&tmp.path().join("a/pdf/SKILL.md"), "unmanaged");
        let dest = repo.personal_dir().join("pdf");
        HOOK.with_borrow_mut(|hook| {
            *hook = Some(Box::new(move |point, at| {
                if point == Hook::BeforeSwap && at == dest {
                    write(&at.join("SKILL.md"), "created meanwhile");
                }
            }))
        });
        assert!(prepare(&repo, &hs, "a", "pdf").is_err());
        assert_eq!(fs::read_to_string(repo.personal_dir().join("pdf/SKILL.md")).unwrap(), "created meanwhile");
        assert_eq!(fs::read_to_string(tmp.path().join("a/pdf/SKILL.md")).unwrap(), "unmanaged");
    }

    #[cfg(unix)]
    #[test]
    fn unmanaged_directory_links_are_hashed_by_content_and_adopted_without_touching_the_target() {
        let (tmp, repo, hs) = setup();
        let _recovery = displaced::test_root(&tmp.path().join("recovery"));
        write(&tmp.path().join("external/SKILL.md"), "linked");
        fs::create_dir_all(tmp.path().join("a")).unwrap();
        std::os::unix::fs::symlink(tmp.path().join("external"), tmp.path().join("a/pdf")).unwrap();
        write(&tmp.path().join("b/pdf/SKILL.md"), "linked");
        let items = discover(&repo, &hs).unwrap();
        assert_eq!(items[0].variants.len(), 1);
        let adoption = prepare(&repo, &hs, "a", "pdf").unwrap();
        complete(&repo, &hs, &adoption).unwrap();
        assert_eq!(fs::read_to_string(tmp.path().join("external/SKILL.md")).unwrap(), "linked");
        assert!(tmp.path().join("a/pdf/.sk1ll-managed").is_file());
        assert_eq!(displaced::list().unwrap().len(), 2);
    }
}
