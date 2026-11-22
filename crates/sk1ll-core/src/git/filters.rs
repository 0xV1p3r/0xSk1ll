//! The skills repo doesn't support git filter drivers (`filter.<driver>.smudge`,
//! `.clean`, `.process`: Git LFS, say). The server is plain git over HTTP,
//! with no LFS endpoint, so a driver has nothing to talk to. Pull refuses a
//! Baseline that writes a file through one (see [`driver_paths`]), and every
//! git command that pull (or the clone and first Baseline it starts from)
//! runs has every configured driver turned off ([`Git::without_filter_drivers`]),
//! so no driver process runs even for a pull that started before its
//! attributes named one. Conversions without a driver (`text`, `eol`,
//! `ident`, `working-tree-encoding`) still apply.

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

impl Git {
    /// Turns each of `drivers` into a no-op for this command: its `smudge`,
    /// `clean` and `process` empty (git then converts nothing for it) and
    /// not `required` (so that is no error either). Set on the command line,
    /// after every other scope, `-c` and `GIT_CONFIG_PARAMETERS` included,
    /// so nothing overrides it; `--config-env` because a driver's name may
    /// hold `=` (git splits there at the last `=`).
    pub(crate) fn without_filter_drivers(self, drivers: &BTreeSet<String>) -> Self {
        if drivers.is_empty() {
            return self;
        }
        let mut args = Vec::<OsString>::new();
        for name in drivers {
            for (key, env) in
                [("smudge", EMPTY_ENV), ("clean", EMPTY_ENV), ("process", EMPTY_ENV), ("required", FALSE_ENV)]
            {
                args.push(format!("--config-env=filter.{name}.{key}={env}").into());
            }
        }
        self.env(EMPTY_ENV, "").env(FALSE_ENV, "false").args(args)
    }
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
    // `<path>\0<attribute>\0<value>\0`, per path.
    let fields: Vec<&[u8]> = out.split(|b| *b == 0).collect();
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
    Ok(found.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        Git::new(dir.path()).args(["init", "--quiet"]).run().unwrap();
        dir
    }

    fn config(dir: &Path, key: &str, value: &str) {
        Git::new(dir).args(["config", key, value]).run().unwrap();
    }

    #[test]
    fn drivers_are_read_from_every_key_with_dots_and_case_kept() {
        let dir = repo();
        config(dir.path(), "filter.lfs.smudge", "x");
        config(dir.path(), "filter.a.b.clean", "x");
        config(dir.path(), "filter.Mixed=Name.process", "x");
        config(dir.path(), "filter.req.required", "true");
        let drivers = configured_drivers(Git::new(dir.path()).args(["-c", "filter.cli.smudge=x"])).unwrap();
        let want: BTreeSet<String> = ["Mixed=Name", "a.b", "cli", "lfs"].map(String::from).into();
        assert_eq!(drivers, want);
    }

    #[test]
    fn set_unset_and_unspecified_count_only_as_configured_drivers() {
        let dir = repo();
        std::fs::write(
            dir.path().join(".gitattributes"),
            "a filter=lfs\nb filter\nc -filter\nd filter=set\ne filter=unset\n",
        )
        .unwrap();
        let paths: Vec<&Path> = ["a", "b", "c", "d", "e", "f"].map(Path::new).into();
        let none = BTreeSet::new();
        assert_eq!(driver_paths(Git::new(dir.path()), None, &paths, &none).unwrap(), [PathBuf::from("a")]);
        let set: BTreeSet<String> = ["set".to_string()].into();
        // `b filter` is ambiguous with `filter=set` here: counted, to be safe.
        let found = driver_paths(Git::new(dir.path()), None, &paths, &set).unwrap();
        assert_eq!(found, ["a", "b", "d"].map(PathBuf::from));
        let unset: BTreeSet<String> = ["unset".to_string()].into();
        let found = driver_paths(Git::new(dir.path()), None, &paths, &unset).unwrap();
        assert_eq!(found, ["a", "c", "e"].map(PathBuf::from));
    }

    /// A driver configured in every way git takes one, `-c` included, never
    /// runs, and its absence is no error even when it is `required`.
    #[test]
    fn neutralized_drivers_never_run() {
        let dir = repo();
        let marker = dir.path().join("ran");
        let cmd = format!("touch '{}'; cat", marker.display());
        config(dir.path(), "filter.x=y.smudge", &cmd);
        config(dir.path(), "filter.x=y.clean", &cmd);
        config(dir.path(), "filter.x=y.required", "true");
        std::fs::write(dir.path().join(".gitattributes"), "*.txt filter=x=y\n*.md filter=cli\n").unwrap();
        let git = || {
            Git::new(dir.path()).args(["-c", &format!("filter.cli.process={cmd}"), "-c", "filter.cli.required=true"])
        };
        let drivers = configured_drivers(git()).unwrap();
        for path in ["--path=a.txt", "--path=a.md"] {
            let blob = git()
                .without_filter_drivers(&drivers)
                .args(["hash-object", "-w", "--stdin", path])
                .stdin(b"hi\n".to_vec())
                .text()
                .unwrap();
            let out = git().without_filter_drivers(&drivers).args(["cat-file", "--filters", path, &blob]).run();
            assert_eq!(out.unwrap(), b"hi\n");
        }
        assert!(!marker.exists(), "a filter driver ran");
        // Without, they do.
        let _ = git().args(["hash-object", "--stdin", "--path=a.md"]).stdin(b"hi\n".to_vec()).output();
        assert!(marker.exists(), "the test's driver is not configured as git reads it");
    }
}
