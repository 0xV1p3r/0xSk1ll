use std::path::{Path, PathBuf};

use crate::git;

/// A skills repo (`Repo::init`'s layout) on branch `main` with everything
/// committed as one seed commit, its git config carrying the test identity
/// with signing off. Removed when dropped if it made its own directory.
#[derive(Debug)]
pub struct SkillsRepo {
    root: PathBuf,
    _tmp: Option<tempfile::TempDir>,
}

impl Default for SkillsRepo {
    fn default() -> Self {
        Self::new()
    }
}

impl SkillsRepo {
    /// A seeded repo in a temporary directory of its own.
    pub fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let mut repo = Self::init(tmp.path().join("repo"));
        repo._tmp = Some(tmp);
        repo
    }

    /// A seeded repo at `root`, which is left in place when dropped.
    pub fn init(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        sk1ll_core::Repo::init(&root).unwrap();
        git::configure(&root);
        let repo = Self { root, _tmp: None };
        repo.commit_all("seed");
        repo
    }

    /// A clone of `origin` at `root`, configured as [`init`](Self::init) does.
    pub fn clone_from(origin: &Path, root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        let parent = root.parent().expect("a path with a parent");
        std::fs::create_dir_all(parent).unwrap();
        git::git(parent, &["clone", "--quiet", path(origin), path(&root)]);
        git::configure(&root);
        Self { root, _tmp: None }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The repo, opened.
    pub fn open(&self) -> sk1ll_core::Repo {
        sk1ll_core::Repo::open(&self.root).unwrap()
    }

    /// `git <args>` in the repo (see [`git::git`]).
    #[track_caller]
    pub fn git(&self, args: &[&str]) -> String {
        git::git(&self.root, args)
    }

    /// Writes `rel` in the repo; returns its path.
    pub fn write(&self, rel: &str, text: &str) -> PathBuf {
        let path = self.root.join(rel);
        crate::write(&path, text);
        path
    }

    /// The text of `rel` in the repo.
    pub fn read(&self, rel: &str) -> String {
        crate::read(&self.root.join(rel))
    }

    /// Writes `skill` into `personal/`; returns its directory.
    pub fn skill(&self, skill: Skill) -> PathBuf {
        skill.write(&self.root.join("personal"))
    }

    /// Writes `rule` into `instructions/`; returns its file.
    pub fn rule(&self, rule: Rule) -> PathBuf {
        rule.write(&self.root.join("instructions"))
    }

    /// Stages everything and commits it; returns the new `HEAD`.
    pub fn commit_all(&self, message: &str) -> String {
        self.git(&["add", "--all"]);
        self.git(&["commit", "--quiet", "--allow-empty", "-m", message]);
        self.head()
    }

    pub fn head(&self) -> String {
        self.git(&["rev-parse", "HEAD"])
    }

    /// A bare `origin` at `bare`, with `main` pushed to it and tracked.
    pub fn push_to_new_origin(&self, bare: &Path) {
        git::init_bare(bare);
        self.git(&["remote", "add", "origin", path(bare)]);
        self.git(&["push", "--quiet", "-u", "origin", "main"]);
    }
}

fn path(p: &Path) -> &str {
    p.to_str().expect("a UTF-8 path")
}

/// A Skill's directory: `SKILL.md` with front matter, an optional Item ID
/// sidecar (`.sk1ll-id`), and other files.
#[derive(Debug, Clone)]
pub struct Skill {
    name: String,
    description: Option<String>,
    body: String,
    id: Option<String>,
    files: Vec<(String, String)>,
}

impl Skill {
    /// `name` with the description `name`, an empty body and no Item ID.
    pub fn new(name: &str) -> Self {
        Self { name: name.into(), description: Some(name.into()), body: String::new(), id: None, files: Vec::new() }
    }

    /// `None`: no front matter at all.
    pub fn description(mut self, description: Option<&str>) -> Self {
        self.description = description.map(Into::into);
        self
    }

    pub fn body(mut self, body: &str) -> Self {
        self.body = body.into();
        self
    }

    /// The Item ID written to `.sk1ll-id`.
    pub fn id(mut self, id: &str) -> Self {
        self.id = Some(id.into());
        self
    }

    /// Another file, at `rel` inside the skill.
    pub fn file(mut self, rel: &str, text: &str) -> Self {
        self.files.push((rel.into(), text.into()));
        self
    }

    /// `SKILL.md` as written.
    pub fn skill_md(&self) -> String {
        match &self.description {
            Some(d) => format!("---\nname: {}\ndescription: {d:?}\n---\n{}", self.name, self.body),
            None => self.body.clone(),
        }
    }

    /// Writes the skill into the directory `parent` (a repo's `personal/`, a
    /// harness's skills directory); returns its directory.
    pub fn write(&self, parent: &Path) -> PathBuf {
        let dir = parent.join(&self.name);
        crate::write(&dir.join("SKILL.md"), &self.skill_md());
        if let Some(id) = &self.id {
            crate::write(&dir.join(".sk1ll-id"), &format!("{id}\n"));
        }
        for (rel, text) in &self.files {
            crate::write(&dir.join(rel), text);
        }
        dir
    }
}

/// A Rule: `<name>.md` with an optional `id:` in its front matter.
#[derive(Debug, Clone)]
pub struct Rule {
    name: String,
    body: String,
    id: Option<String>,
}

impl Rule {
    /// `name` with the body `name` and no Item ID.
    pub fn new(name: &str) -> Self {
        Self { name: name.into(), body: format!("{name}\n"), id: None }
    }

    pub fn body(mut self, body: &str) -> Self {
        self.body = body.into();
        self
    }

    pub fn id(mut self, id: &str) -> Self {
        self.id = Some(id.into());
        self
    }

    /// The file as written.
    pub fn text(&self) -> String {
        match &self.id {
            Some(id) => format!("---\nid: {id}\n---\n{}", self.body),
            None => self.body.clone(),
        }
    }

    /// Writes the rule into the directory `parent`; returns its file.
    pub fn write(&self, parent: &Path) -> PathBuf {
        let path = parent.join(format!("{}.md", self.name));
        crate::write(&path, &self.text());
        path
    }
}
