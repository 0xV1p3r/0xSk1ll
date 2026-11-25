//! The running test again, in a child process: for a crash that ends the
//! process ([`sk1ll_core::failpoints::Action::Exit`]), with nothing
//! unwinding, after which the parent restarts on the same directories.
//!
//! ```text
//! #[test]
//! fn a_sync_killed_mid_swap_recovers() {
//!     if let Some(dir) = child::dir() {
//!         // The child: crash where the parent asked, on the parent's files.
//!         let _exit = failpoints::install(|point, _| match point {
//!             Point::BeforeSwap => Action::Exit(child::CRASHED),
//!             _ => Action::Continue,
//!         });
//!         run_sync(&dir);
//!         return;
//!     }
//!     let fixture = Fixture::new();
//!     let out = child::run("a_sync_killed_mid_swap_recovers", fixture.root());
//!     assert_eq!(out.status.code(), Some(child::CRASHED), "{}", sk1ll_testkit::text(&out));
//!     // Restart on the same directories and check what is there.
//! }
//! ```

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// The variable that carries the parent's directory to the child.
const DIR: &str = "SK1LL_TESTKIT_CHILD_DIR";

/// An exit code for [`Action::Exit`](sk1ll_core::failpoints::Action::Exit)
/// that no test harness outcome uses (0 passes, 101 fails).
pub const CRASHED: i32 = 86;

/// In the child [`run`] started: the directory its parent passed.
/// `None` in the parent.
pub fn dir() -> Option<PathBuf> {
    std::env::var_os(DIR).map(PathBuf::from)
}

/// Runs the test named `test` (the test function's path inside its test
/// binary, as `cargo test -- --exact` takes it) again in a child process of
/// this test binary, with [`dir`] returning `dir` there; waits for it and
/// returns its output.
pub fn run(test: &str, dir: &Path) -> Output {
    command(test, dir).output().expect("the test binary runs again")
}

/// The child's command, for a test that adds environment of its own.
pub fn command(test: &str, dir: &Path) -> Command {
    let mut cmd = Command::new(std::env::current_exe().expect("the test binary's path"));
    cmd.args([test, "--exact", "--nocapture", "--test-threads=1"]).env(DIR, dir);
    cmd
}
