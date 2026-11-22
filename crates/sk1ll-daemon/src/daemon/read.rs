use super::*;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use sk1ll_core::imports;
use sk1ll_core::sync::{self};
use sk1ll_core::{ItemRef, MachineConfig, Repo, default_commit_message};

use crate::client::{ChangeKind, CommitMessage, DiffView, DraftEntry, ItemView};
use crate::error::{Error, Result};
use sk1ll_core::git::plumbing;

impl Daemon {
    /// A reader: no engine mutex.
    pub(crate) fn commit_message(&self, items: &[String]) -> Result<CommitMessage> {
        let repo = Repo::open_git_only(self.repo_root())?;
        let draft = repo.draft()?;
        let items = items.iter().map(|i| ItemRef::resolve(i, &draft)).collect::<Result<Vec<_>, _>>()?;
        let message = default_commit_message(&items, self.ident.machine_id());
        Ok(CommitMessage { items, message })
    }

    /// A read-only inventory. GIT_OPTIONAL_LOCKS=0 prevents status from refreshing
    /// the index, and no engine mutex or EngineLock is acquired here.
    pub(crate) fn items(root: &Path) -> Result<Vec<ItemView>> {
        use sk1ll_core::git::Git;
        let read = |args: &[&str]| Git::new(root).env("GIT_OPTIONAL_LOCKS", "0").args(args).run();
        let paths = |revision: Option<&str>| -> Result<Vec<String>> {
            let Some(revision) = revision else { return Ok(Vec::new()) };
            let bytes = read(&["ls-tree", "-r", "-z", "--name-only", revision])?;
            Ok(bytes
                .split(|b| *b == 0)
                .filter(|p| !p.is_empty())
                .map(|p| String::from_utf8_lossy(p).into_owned())
                .collect())
        };
        let baseline = plumbing::baseline(root)?;
        let head = plumbing::rev(root, "HEAD")?;
        let baseline_paths = paths(baseline.as_deref())?;
        let head_paths = if head == baseline { baseline_paths.clone() } else { paths(head.as_deref())? };
        let mut inventory = BTreeMap::<ItemRef, ItemView>::new();
        let entry = |item: ItemRef| ItemView {
            item,
            id: None,
            in_baseline: false,
            draft: None,
            unpushed: false,
            description: None,
        };
        for path in &baseline_paths {
            let item = ItemRef::for_path(Path::new(path));
            if matches!(item, ItemRef::Skill(_) | ItemRef::Rule(_)) {
                inventory.entry(item.clone()).or_insert_with(|| entry(item)).in_baseline = true;
            }
        }
        let repo = Repo::open_git_only(root)?;
        for draft in repo.draft()? {
            let change = match draft.change {
                sk1ll_core::Change::Added => ChangeKind::Added,
                sk1ll_core::Change::Modified => ChangeKind::Modified,
                sk1ll_core::Change::Deleted => ChangeKind::Deleted,
            };
            inventory.entry(draft.item.clone()).or_insert_with(|| entry(draft.item)).draft = Some(change);
        }
        for item in plumbing::unpushed_items(root)? {
            inventory.entry(item.clone()).or_insert_with(|| entry(item)).unpushed = true;
        }
        // Descriptions of skills without a draft come from git: one
        // `cat-file --batch` for all of them, not a process per skill.
        let mut wanted = Vec::new();
        for value in inventory.values().filter(|value| value.draft.is_none()) {
            let revision = if value.unpushed { head.as_ref() } else { baseline.as_ref() };
            let (ItemRef::Skill(name), Some(rev)) = (&value.item, revision) else { continue };
            // The batch protocol is line-based: such a name can't be asked for.
            if !name.contains('\n') {
                wanted.extend(SKILL_DIRS.iter().map(|directory| format!("{rev}:{directory}/{name}/SKILL.md")));
            }
        }
        let mut blobs = BTreeMap::new();
        let cat_file = |mode: &str, objects: &[String]| {
            let input = objects.iter().flat_map(|object| [object.as_str(), "\n"]).collect::<String>().into_bytes();
            Git::new(root).env("GIT_OPTIONAL_LOCKS", "0").args(["cat-file", mode]).stdin(input).run()
        };
        // Sizes first, so git never streams us a blob over the cap.
        if !wanted.is_empty() {
            let sizes = cat_file("--batch-check", &wanted).unwrap_or_default();
            let mut sizes = sizes.split(|b| *b == b'\n').map(|line| small_blob(line, DESCRIPTION_CAP));
            wanted.retain(|_| sizes.next().unwrap_or(false));
        }
        if !wanted.is_empty() {
            let batch = cat_file("--batch", &wanted);
            let contents = batch.map(|out| parse_batch(&out, wanted.len(), DESCRIPTION_CAP)).unwrap_or_default();
            blobs.extend(wanted.into_iter().zip(contents).filter_map(|(object, bytes)| Some((object, bytes?))));
        }
        let claims_at = |revision: Option<&str>, paths: &[String]| -> BTreeMap<_, _> {
            revision
                .map(|rev| sk1ll_core::item_id::committed_inventory(root, rev, paths).items)
                .unwrap_or_default()
                .into_iter()
                .collect()
        };
        let baseline_ids = claims_at(baseline.as_deref(), &baseline_paths);
        let head_ids = if head == baseline { baseline_ids.clone() } else { claims_at(head.as_deref(), &head_paths) };
        // Malformed metadata and duplicate Item IDs (until `repair-ids`) are
        // errors here. The report to the server stays partial instead.
        sk1ll_core::content::item_ids_on_disk(root)?;
        let on_disk: BTreeMap<_, _> = sk1ll_core::item_id::working_inventory(root).items.into_iter().collect();
        for item in on_disk.keys() {
            inventory.entry(item.clone()).or_insert_with(|| entry(item.clone()));
        }
        let baseline_item_ids: BTreeSet<_> = baseline_ids.values().filter_map(Option::as_ref).collect();
        let mut historical = Vec::new();
        for value in inventory.values_mut() {
            let old = baseline_ids.get(&value.item);
            let committed = head_ids.get(&value.item);
            let working = on_disk.get(&value.item);
            value.id = working.or(committed).or(old).cloned().flatten();
            value.in_baseline = match value.id.as_ref() {
                Some(id) => baseline_item_ids.contains(id),
                None => old.is_some_and(|id| id.is_none()),
            };
            if let (Some(Some(old)), Some(current)) = (old, working.or(committed))
                && current.as_ref() != Some(old)
            {
                let mut previous = entry(value.item.clone());
                previous.id = Some(old.clone());
                previous.in_baseline = true;
                previous.draft = (working != committed).then_some(ChangeKind::Deleted);
                previous.unpushed = committed != Some(&previous.id);
                historical.push(previous);
                if working != committed {
                    value.draft = Some(ChangeKind::Added);
                }
            }
            let ItemRef::Skill(name) = &value.item else { continue };
            for directory in SKILL_DIRS {
                let path = format!("{directory}/{name}/SKILL.md");
                let text = if value.draft.is_some() {
                    use std::io::Read;
                    let mut text = String::new();
                    std::fs::File::open(root.join(&path))
                        .ok()
                        .and_then(|file| file.take(DESCRIPTION_CAP as u64).read_to_string(&mut text).ok())
                        .map(|_| text)
                } else {
                    let revision = if value.unpushed { head.as_ref() } else { baseline.as_ref() };
                    revision
                        .and_then(|rev| blobs.remove(&format!("{rev}:{path}")))
                        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                };
                if let Some(text) = text {
                    value.description = skill_description(&text);
                    break;
                }
            }
        }
        let mut by_id = BTreeMap::new();
        for value in historical.into_iter().chain(inventory.into_values()) {
            let key = (value.id.clone(), value.id.is_none().then(|| value.item.clone()));
            match by_id.entry(key) {
                std::collections::btree_map::Entry::Occupied(mut item) => {
                    if value.draft != Some(ChangeKind::Deleted) {
                        item.insert(value);
                    }
                }
                std::collections::btree_map::Entry::Vacant(item) => {
                    item.insert(value);
                }
            }
        }
        let mut items: Vec<_> = by_id.into_values().collect();
        items.sort_by(|a, b| a.item.cmp(&b.item).then(a.id.cmp(&b.id)));
        Ok(items)
    }

    /// A reader: no engine mutex.
    pub(crate) fn draft(&self) -> Result<Vec<DraftEntry>> {
        let repo = Repo::open_git_only(self.repo_root())?;
        Ok(repo
            .draft()?
            .into_iter()
            .map(|d| DraftEntry {
                item: d.item,
                change: match d.change {
                    sk1ll_core::Change::Added => ChangeKind::Added,
                    sk1ll_core::Change::Modified => ChangeKind::Modified,
                    sk1ll_core::Change::Deleted => ChangeKind::Deleted,
                },
                paths: d.paths,
            })
            .collect())
    }

    /// A reader: no engine mutex.
    pub(crate) fn diff(&self, item: &str) -> Result<DiffView> {
        let repo = Repo::open_git_only(self.repo_root())?;
        let item = ItemRef::resolve(item, &repo.draft()?)?;
        let diff = repo.diff(&item)?;
        Ok(DiffView { item, diff, target: None })
    }

    /// A reader: no engine mutex.
    pub(crate) fn repo_content(&self) -> Result<crate::client::RepoContent> {
        let repo = Repo::open(self.repo_root())?;
        let pull_pending = repo.pull_pending()?;
        let imports = if pull_pending { Vec::new() } else { repo.import_status()? };
        Ok(crate::client::RepoContent { skills: repo.skills()?, rules: repo.rules()?, imports, pull_pending })
    }

    /// A reader: the file an editor opens for a personal Skill or Rule.
    pub(crate) fn editable(&self, item: &str) -> Result<PathBuf> {
        let repo = Repo::open(self.repo_root())?;
        let harnesses = MachineConfig::load(&self.cfg.machine_config)?.harnesses();
        Ok(sk1ll_core::content::editable_path(&repo, &harnesses, item)?)
    }

    /// A reader: no engine mutex.
    pub(crate) fn skills(&self) -> Result<Vec<sk1ll_core::content::ContentEntry>> {
        let repo = Repo::open(self.repo_root())?;
        let harnesses = MachineConfig::load(&self.cfg.machine_config)?.harnesses();
        let baseline = plumbing::baseline(repo.root())?;
        let snapshot = self.applied();
        let assignment = snapshot.as_ref().map(|s| s.assignment.clone()).unwrap_or_default();
        let mut entries = sk1ll_core::content::list(&repo, &harnesses, &assignment, baseline.as_deref())?;
        if snapshot.is_none() {
            for entry in &mut entries {
                entry.enabled_harnesses.clear();
            }
        }
        Ok(entries)
    }

    /// A reader: no engine mutex.
    pub(crate) fn skill_tree(
        &self,
        q: &sk1ll_core::content::ContentQuery,
    ) -> Result<Vec<sk1ll_core::content::FileEntry>> {
        Ok(sk1ll_core::content::tree(
            &Repo::open(self.repo_root())?,
            &MachineConfig::load(&self.cfg.machine_config)?.harnesses(),
            q,
        )?)
    }

    /// A reader: no engine mutex.
    pub(crate) fn skill_file(&self, q: &sk1ll_core::content::FileQuery) -> Result<sk1ll_core::content::FileContent> {
        Ok(sk1ll_core::content::file(
            &Repo::open(self.repo_root())?,
            &MachineConfig::load(&self.cfg.machine_config)?.harnesses(),
            q,
        )?)
    }

    /// A reader: no engine mutex.
    pub(crate) fn conflict_diff(
        &self,
        req: &crate::client::ConflictDiffRequest,
        snapshot: &Snapshot,
    ) -> Result<DiffView> {
        use sk1ll_core::Origin;
        let repo = Repo::open(self.repo_root())?;
        let (view, recovery) = if let Some(id) = &req.id {
            let entry = sync::displaced::get(id)?;
            let target = sync::displaced::target(&repo, &entry)?;
            if entry.kind == sync::displaced::Kind::Draft {
                return Self::pull_recovery_diff(&repo, entry, target.path);
            }
            let view = displaced_view(entry, repo.root());
            (view, Some(target))
        } else {
            let view = snapshot.conflict(req)?.ok_or_else(|| Error::NotFound("Conflict no longer exists".into()))?;
            (view, None)
        };
        let left_source = view.path.unwrap_or(view.dest);
        let mut item = if view.name == BLOCK_NAME { Some(ItemRef::Other(view.name.clone())) } else { None }
            .or(view.item.clone())
            .or_else(|| view.items.first().cloned())
            .unwrap_or(ItemRef::Other(view.name.clone()));
        let mut left_origin = Origin::Personal;
        let mut target = recovery.as_ref().map(|target| target.path.clone());
        let snapshot = self.applied();
        let temp = tempfile::tempdir().map_err(anyhow::Error::from)?;
        let right = temp.path().join("Draft");
        match &item {
            ItemRef::Skill(name) => {
                imports::validate_name(name)?;
                let (source, origin) = if let Some(target) = &recovery {
                    (target.path.clone(), target.origin.unwrap_or(Origin::Personal))
                } else if let Some(skill) = repo.skills()?.into_iter().find(|skill| skill.name == *name) {
                    (skill.path, skill.origin)
                } else {
                    (repo.personal_dir().join(name), Origin::Personal)
                };
                item = ItemRef::Skill(source.file_name().unwrap_or_default().to_string_lossy().into_owned());
                left_origin = match &recovery {
                    Some(target) => target.placed_origin.unwrap_or(origin),
                    None => sync::displaced::skill_placement_origin(&left_source)?.unwrap_or(origin),
                };
                if source.symlink_metadata().is_ok_and(|meta| meta.is_dir()) {
                    sync::copy_skill_for_diff(&source, &right, origin)?;
                }
                target = Some(source);
            }
            ItemRef::Rule(name) => {
                imports::validate_name(name)?;
                let path =
                    target.clone().unwrap_or_else(|| repo.root().join("instructions").join(format!("{name}.md")));
                item = ItemRef::Rule(path.file_stem().unwrap_or_default().to_string_lossy().into_owned());
                if let Some(rule) = repo.rules()?.into_iter().find(|rule| rule.path == path) {
                    let (_, content) = sk1ll_core::instructions::render_rule(&rule);
                    std::fs::write(&right, content).map_err(anyhow::Error::from)?;
                }
                target = Some(path);
            }
            _ => {
                let harnesses = MachineConfig::load(&self.cfg.machine_config)?.harnesses();
                let assignment =
                    &snapshot.as_ref().ok_or_else(|| Error::Conflict("Awaiting Assignment".into()))?.assignment;
                let files = sync::render_instructions(&repo, &harnesses, assignment, &view.harness)?;
                std::fs::write(&right, files.into_iter().map(|f| f.content).collect::<Vec<_>>().join("\n"))
                    .map_err(anyhow::Error::from)?;
            }
        }
        let mut left = left_source;
        if view.name == BLOCK_NAME && matches!(item, ItemRef::Other(_)) {
            let text = std::fs::read_to_string(&left).map_err(anyhow::Error::from)?;
            let content = sk1ll_core::instructions::managed_content(&text, &left)?;
            left = temp.path().join("Harness");
            std::fs::write(&left, content).map_err(anyhow::Error::from)?;
        } else if left.symlink_metadata().is_ok_and(|meta| meta.is_dir()) {
            let copy = temp.path().join("Harness");
            sync::copy_skill_for_diff(&left, &copy, left_origin)?;
            left = copy;
        }
        if !right.exists() {
            if left.is_dir() {
                std::fs::create_dir(&right).map_err(anyhow::Error::from)?;
            } else {
                std::fs::write(&right, "").map_err(anyhow::Error::from)?;
            }
        }
        let output = plumbing::diff_no_index(&left, &right).map_err(anyhow::Error::from)?;
        if !matches!(output.status.code(), Some(0 | 1)) {
            return Err(Error::BadRequest(String::from_utf8_lossy(&output.stderr).into_owned()));
        }
        Ok(DiffView { item, target, diff: String::from_utf8_lossy(&output.stdout).into_owned() })
    }

    /// What a pull displaced from the working tree, against what is at its
    /// path now: adopting puts it back there.
    pub(super) fn pull_recovery_diff(repo: &Repo, entry: sync::displaced::Entry, target: PathBuf) -> Result<DiffView> {
        let item = displaced_item(&entry, repo.root());
        let temp = tempfile::tempdir().map_err(anyhow::Error::from)?;
        let right = if target.symlink_metadata().is_ok() {
            target.clone()
        } else {
            let empty = temp.path().join("Draft");
            std::fs::write(&empty, "").map_err(anyhow::Error::from)?;
            empty
        };
        let output = plumbing::diff_no_index(&entry.path, &right).map_err(anyhow::Error::from)?;
        if !matches!(output.status.code(), Some(0 | 1)) {
            return Err(Error::BadRequest(String::from_utf8_lossy(&output.stderr).into_owned()));
        }
        Ok(DiffView { item, target: Some(target), diff: String::from_utf8_lossy(&output.stdout).into_owned() })
    }
}

/// Where a skill's files live in the repo, in the order they are looked up.
pub(super) const SKILL_DIRS: [&str; 2] = ["personal", "imported"];

/// A `SKILL.md` larger than this is not read for its description.
pub(super) const DESCRIPTION_CAP: usize = 16384;

/// Whether a `git cat-file --batch-check` line (`<oid> <type> <size>`)
/// describes a blob of at most `cap` bytes.
pub(super) fn small_blob(line: &[u8], cap: usize) -> bool {
    let line = String::from_utf8_lossy(line);
    let mut fields = line.rsplitn(3, ' ');
    let (size, kind) = (fields.next(), fields.next());
    kind == Some("blob") && size.and_then(|size| size.parse::<usize>().ok()).is_some_and(|size| size <= cap)
}

/// Reads `git cat-file --batch` output for `count` requested objects, in
/// request order: the content of each blob of at most `cap` bytes, `None` for
/// anything else (missing, not a blob, larger). Content is raw bytes, so it is
/// taken by the size in its header, never by looking for a line end; output
/// that ends early or doesn't parse leaves the remaining objects `None`.
pub(super) fn parse_batch(mut out: &[u8], count: usize, cap: usize) -> Vec<Option<Vec<u8>>> {
    let mut contents = vec![None; count];
    for content in &mut contents {
        let Some(end) = out.iter().position(|b| *b == b'\n') else { break };
        let header = String::from_utf8_lossy(&out[..end]);
        out = &out[end + 1..];
        // `<oid> <type> <size>`; anything else (`<name> missing`, where the
        // name may itself contain spaces) has no content after it.
        let mut fields = header.rsplitn(3, ' ');
        let (size, kind, oid) = (fields.next(), fields.next(), fields.next());
        let object = matches!(kind, Some("blob" | "tree" | "commit" | "tag"))
            && oid.is_some_and(|oid| !oid.is_empty() && oid.bytes().all(|b| b.is_ascii_hexdigit()));
        let Some(size) = size.filter(|_| object).and_then(|size| size.parse::<usize>().ok()) else { continue };
        if out.len() <= size || out[size] != b'\n' {
            break;
        }
        if kind == Some("blob") && size <= cap {
            *content = Some(out[..size].to_vec());
        }
        out = &out[size + 1..];
    }
    contents
}

pub(super) fn skill_description(text: &str) -> Option<String> {
    let mut lines = text.lines();
    if lines.next()?.trim() != "---" {
        return None;
    }
    lines
        .take_while(|line| line.trim() != "---")
        .find_map(|line| line.strip_prefix("description:").map(str::trim))
        .filter(|description| !description.is_empty() && !matches!(*description, "|" | ">"))
        .map(|description| description.trim_matches(['\"', '\'']).to_string())
}
