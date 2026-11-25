//! The skills repo doesn't support git filter drivers (`filter.<driver>.smudge`,
//! `.clean`, `.process`: Git LFS, say). The server is plain git over HTTP,
//! with no LFS endpoint, so a driver has nothing to talk to. Pull and the
//! clone refuse a Baseline that writes a file through one (see
//! [`driver_paths`]), and every git command on the skills repo runs with
//! every configured driver turned off ([`Git::without_filter_drivers`]): it
//! is built by [`RepoGit`] (or, for pull, [`Git::for_pull`]), which reads the
//! drivers from git's configuration for that repo, its conditional includes
//! (`includeIf.gitdir`) and all. So no driver process runs, not under a pull
//! that started before its attributes named one, and not under `git status`,
//! a restore, a diff or a commit's `add`. Conversions without a driver
//! (`text`, `eol`, `ident`, `working-tree-encoding`) still apply. Imports
//! (third-party sources, in their own caches) keep their own handling.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use super::{Git, path_bytes, path_from_bytes};
use crate::{Error, Result};

/// Holds the empty value [`Git::without_filter_drivers`] sets a driver's
/// commands to.
const EMPTY_ENV: &str = "SK1LL_NO_FILTER_DRIVER";
/// Holds `false`, for `filter.<driver>.required`.
const FALSE_ENV: &str = "SK1LL_NO_FILTER_DRIVER_REQUIRED";

/// The filter drivers git's configuration for `dir` gives a command to
/// (`smudge`, `clean` or `process`), as git itself reads it: every scope,
/// includes and `-c`/`GIT_CONFIG_*` from the environment. A driver's name is
/// what the attribute names, dots and all.
pub(crate) fn configured_drivers(git: Git) -> Result<BTreeSet<String>> {
    let pattern = r"^filter\..*\.(smudge|clean|process)$";
    let out = git.args(["config", "-z", "--get-regexp", pattern]).output()?;
    match out.status.code() {
        Some(0) => {}
        // No such key.
        Some(1) => return Ok(BTreeSet::new()),
        _ => {
            return Err(Error::Git {
                args: "config --get-regexp".into(),
                stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
            });
        }
    }
    // `<key>\n<value>\0`, or `<key>\0` without a value. git lowercases the
    // section and the variable, not the subsection: the driver's name.
    let mut drivers = BTreeSet::new();
    for entry in out.stdout.split(|b| *b == 0).filter(|e| !e.is_empty()) {
        let key = entry.split(|b| *b == b'\n').next().unwrap_or_default();
        let key = String::from_utf8_lossy(key);
        let name = key.strip_prefix("filter.").and_then(|rest| rest.rsplit_once('.')).map(|(name, _)| name);
        if let Some(name) = name {
            drivers.insert(name.to_string());
        }
    }
    Ok(drivers)
}

/// How every git command on the skills repo (its working tree, index or
/// git directory) is built: `git -C <dir>` with each filter driver git's
/// configuration for `dir` names turned off. The drivers are read when it
/// is made, so make one for a series of commands, not for a long life.
#[derive(Debug, Clone)]
pub struct RepoGit {
    dir: PathBuf,
    env: Vec<(String, String)>,
    drivers: BTreeSet<String>,
}

impl RepoGit {
    /// For the repo at (or the directory) `dir`.
    pub fn open(dir: &Path) -> Result<Self> {
        Self::with_env(dir, &[])
    }

    /// With `env` set for every command, discovery included (git's
    /// authentication, see [`super::config_env`]).
    pub fn with_env(dir: &Path, env: &[(String, String)]) -> Result<Self> {
        let env = env.to_vec();
        let drivers = configured_drivers(Git::new(dir).envs(&env))?;
        Ok(Self { dir: dir.to_path_buf(), env, drivers })
    }

    /// One command.
    pub fn git(&self) -> Git {
        Git::new(&self.dir).envs(&self.env).without_filter_drivers(&self.drivers)
    }

    /// The drivers it turns off.
    pub(crate) fn drivers(&self) -> &BTreeSet<String> {
        &self.drivers
    }

    /// The arguments and environment that turn the drivers off, for a
    /// command that is not a [`Git`] (a streamed read): `git -C <dir>`, then
    /// these, then its own arguments.
    pub(crate) fn neutralizing(&self) -> (Vec<OsString>, Vec<(&'static str, &'static str)>) {
        neutralizing(&self.drivers)
    }
}

impl Git {
    /// `git -C <dir>` for one command on the skills repo, with its filter
    /// drivers turned off (see [`RepoGit`]).
    pub fn repo(dir: &Path) -> Result<Self> {
        Ok(RepoGit::open(dir)?.git())
    }

    /// Turns each of `drivers` into a no-op for this command: its `smudge`,
    /// `clean` and `process` empty (git then converts nothing for it) and
    /// not `required` (so that is no error either). Set on the command line,
    /// after every other scope, `-c` and `GIT_CONFIG_PARAMETERS` included,
    /// so nothing overrides it; `--config-env` because a driver's name may
    /// hold `=` (git splits there at the last `=`).
    pub(crate) fn without_filter_drivers(self, drivers: &BTreeSet<String>) -> Self {
        let (args, env) = neutralizing(drivers);
        env.into_iter().fold(self, |git, (k, v)| git.env(k, v)).args(args)
    }
}

fn neutralizing(drivers: &BTreeSet<String>) -> (Vec<OsString>, Vec<(&'static str, &'static str)>) {
    if drivers.is_empty() {
        return (Vec::new(), Vec::new());
    }
    let mut args = Vec::<OsString>::new();
    for name in drivers {
        for (key, env) in [("smudge", EMPTY_ENV), ("clean", EMPTY_ENV), ("process", EMPTY_ENV), ("required", FALSE_ENV)]
        {
            args.push(format!("--config-env=filter.{name}.{key}={env}").into());
        }
    }
    (args, vec![(EMPTY_ENV, ""), (FALSE_ENV, "false")])
}

/// Which of `paths` git would write through a filter driver: their `filter`
/// attribute names one, with `.gitattributes` read where the caller's
/// conversion reads them: from the working tree, or the commit `source`.
/// `configured`: what [`configured_drivers`] found.
///
/// A driver needn't be configured here to count: the Baseline is the same
/// for every Machine, and a driver one has (git-lfs) another may not.
/// `git check-attr` prints `set`, `unset` and `unspecified` both for the
/// states (`filter`, `-filter`, no `filter`: no driver) and for a driver of
/// that name (`filter=set`); git itself tells them apart. A driver of such a
/// name counts only when it is configured: unconfigured, git converts
/// nothing either way.
pub(crate) fn driver_paths(
    git: Git,
    source: Option<&str>,
    paths: &[&Path],
    configured: &BTreeSet<String>,
) -> Result<Vec<PathBuf>> {
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    let mut stdin = Vec::new();
    for path in paths {
        stdin.extend_from_slice(&path_bytes(path));
        stdin.push(0);
    }
    let mut git = git.arg("check-attr");
    if let Some(source) = source {
        git = git.arg(format!("--source={source}"));
    }
    let out = git.args(["-z", "--stdin", "filter"]).stdin(stdin).run()?;
    Ok(filtered_paths(&out, configured))
}

/// The paths `git check-attr -z ... filter` output (`<path>\0filter\0<value>\0`
/// per path) gives a filter driver, by the rules of [`driver_paths`]: what
/// pull, the clone, a commit and the server's push check all decide by.
pub fn filtered_paths(check_attr: &[u8], configured: &BTreeSet<String>) -> Vec<PathBuf> {
    let fields: Vec<&[u8]> = check_attr.split(|b| *b == 0).collect();
    let mut found = BTreeSet::new();
    for f in fields.chunks(3).filter(|f| f.len() == 3) {
        let value = String::from_utf8_lossy(f[2]);
        let driver = match value.as_ref() {
            "set" | "unset" | "unspecified" => configured.contains(value.as_ref()),
            _ => true,
        };
        if driver {
            found.insert(path_from_bytes(f[0]));
        }
    }
    found.into_iter().collect()
}
