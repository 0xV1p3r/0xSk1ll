//! Git commands that never read the developer's global or system config,
//! never prompt, commit as a fixed identity and never sign.

use std::path::Path;
use std::process::{Command, Output};

/// The environment of an isolated git command: no global or system config,
/// no prompts.
pub const ENV: &[(&str, &str)] = &[
    ("GIT_CONFIG_GLOBAL", if cfg!(windows) { "NUL" } else { "/dev/null" }),
    ("GIT_CONFIG_NOSYSTEM", "1"),
    ("GIT_TERMINAL_PROMPT", "0"),
];

/// The identity commits are made as.
pub const NAME: &str = "Test";
pub const EMAIL: &str = "test@example.com";

/// Configuration every isolated git command gets with `-c`: [`NAME`] and
/// [`EMAIL`], unsigned commits and tags.
const CONFIG: &[&str] = &[
    "user.name=Test",
    "user.email=test@example.com",
    "commit.gpgsign=false",
    "tag.gpgsign=false",
    "init.defaultBranch=main",
];

/// Applies [`ENV`] to `cmd` (any program that runs git: `sk1ll`, the daemon).
pub fn isolate(cmd: &mut Command) -> &mut Command {
    cmd.envs(ENV.iter().copied())
}

/// `git -C <dir>`, isolated, with the identity and unsigned commits.
pub fn command(dir: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(dir);
    for config in CONFIG {
        cmd.args(["-c", config]);
    }
    isolate(&mut cmd);
    cmd
}

/// Runs `git -C <dir> <args>` (see [`command`]), whatever its outcome.
pub fn output(dir: &Path, args: &[&str]) -> Output {
    command(dir).args(args).output().expect("git runs")
}

/// Runs `git -C <dir> <args>` (see [`command`]) and returns its trimmed
/// stdout; panics with its stderr if it fails.
#[track_caller]
pub fn git(dir: &Path, args: &[&str]) -> String {
    let out = output(dir, args);
    assert!(out.status.success(), "git {args:?} in {}: {}", dir.display(), String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Writes the identity and unsigned commits into `repo`'s own config, for
/// the git commands sk1ll runs there itself.
pub fn configure(repo: &Path) {
    for (key, value) in
        [("user.name", NAME), ("user.email", EMAIL), ("commit.gpgsign", "false"), ("tag.gpgsign", "false")]
    {
        git(repo, &["config", key, value]);
    }
}

/// A bare repo at `path` on branch `main`.
pub fn init_bare(path: &Path) {
    std::fs::create_dir_all(path).unwrap();
    git(path, &["init", "--quiet", "--bare", "-b", "main"]);
}
