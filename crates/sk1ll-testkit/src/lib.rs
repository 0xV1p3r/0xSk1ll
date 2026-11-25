//! Fixtures shared by the sk1ll crates' integration tests (`crates/*/tests`),
//! as a dev-dependency only.
//!
//! Every fixture keeps to its own temporary directory: git never reads the
//! developer's global or system config ([`git`]), spawned binaries get a
//! HOME and XDG directories of their own ([`Home`]), and `machine.toml`
//! files turn every preset off ([`MachineToml`]), so nothing finds the
//! developer's real harnesses.
//!
//! - [`git`]: isolated git commands, a fixed identity, unsigned commits.
//! - [`SkillsRepo`]: a skills repo with a seed commit, and [`Skill`] and
//!   [`Rule`] builders for its content.
//! - [`MachineToml`] and [`Harness`]: a `machine.toml` with presets off and
//!   harnesses in copy or symlink mode.
//! - [`Home`]: an isolated HOME for spawned binaries, and for in-process
//!   engines on the test's thread ([`Home::enter`]).
//! - [`Machine`]: one Machine's files without a daemon (repo, `machine.toml`,
//!   Assignment, HOME), and the `sk1ll` command line for them.
//! - [`child`]: the running test again in a child process, for crashes that
//!   end the process ([`sk1ll_core::failpoints::Action::Exit`]).
//! - `server` (feature `server`): a real sk1ll-server that can be stopped and
//!   restarted on the same port.
//! - `daemon` (feature `daemon`): a `sk1ll-daemon` process per Machine.
//! - `eventually` (with either feature): polls until a condition holds.

pub mod child;
pub mod git;
mod home;
mod machine;
mod repo;

#[cfg(feature = "daemon")]
pub mod daemon;
#[cfg(feature = "server")]
pub mod server;

pub use home::Home;
pub use machine::{Harness, Machine, MachineToml, presets_off};
pub use repo::{Rule, Skill, SkillsRepo};
pub use sk1ll_core::machine::{InstructionsFormat, LinkMode};

use std::path::Path;

/// Writes `text` to `path`, creating its parent directories.
pub fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().expect("a path with a parent")).unwrap();
    std::fs::write(path, text).unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
}

/// The text of `path`.
pub fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// Stdout and stderr of a finished command, in that order.
pub fn text(out: &std::process::Output) -> String {
    format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr))
}

/// Stdout of a command that succeeded; `Err` holds stderr then stdout.
pub fn outcome(out: std::process::Output) -> Result<String, String> {
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    if out.status.success() { Ok(stdout) } else { Err(format!("{}{stdout}", String::from_utf8_lossy(&out.stderr))) }
}

/// Polls `f` every 100 ms until it returns `Some`, for up to 30 seconds; a
/// timeout names the condition by where `eventually` was called.
#[cfg(feature = "server")]
#[track_caller]
pub fn eventually<T, F, Fut>(mut f: F) -> impl Future<Output = T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Option<T>>,
{
    let at = std::panic::Location::caller();
    async move {
        for _ in 0..300 {
            if let Some(v) = f().await {
                return v;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        panic!("the condition at {at} never held within 30s");
    }
}
