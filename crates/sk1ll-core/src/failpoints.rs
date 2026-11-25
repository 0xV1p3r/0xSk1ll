//! Fault injection for tests: named points in sync, draft, pull, imports and
//! the daemon where a test steps in to edit the filesystem as a concurrent
//! writer would, to stop the operation as a crash would, or to make one
//! system call fail.
//!
//! Compiled for core's own tests and with the `failpoints` cargo feature
//! (off by default). Without either, each point compiles to nothing. With
//! the feature on, a point does nothing until a test installs a callback,
//! so a binary built with it (the CLI in `cargo test --all-features`)
//! behaves as usual.
//!
//! A callback sees each [`Point`] as it is reached, with the path it is
//! about (each point's docs say which), and answers with an [`Action`]:
//!
//! - [`Action::Continue`], after any side effect it made itself (writing
//!   to a harness, say, to simulate an edit made at that moment);
//! - [`Action::Crash`], an interrupt that unwinds out of the operation as a
//!   [`Crash`] panic, which [`catch`] turns back into a value. Destructors
//!   run while it unwinds: locks are released, as a process death would
//!   release them, and staging directories that clean up after themselves
//!   are removed, which a process death would leave behind;
//! - [`Action::Exit`], a real crash: the process exits at once, without
//!   unwinding. Run the operation in a child process (the test binary run
//!   again for one test, see `sk1ll-testkit`'s `child` module) and restart
//!   in the parent;
//! - [`Action::Fail`], an injected I/O error, only at points that model a
//!   system call ([`Point::can_fail`]).
//!
//! A crash test interrupts an operation, then runs a fresh one (a new
//! [`crate::Repo`], `sync::recover` and the next plan, or a new engine) on
//! the same directories and checks what it finds. A sweep runs an operation
//! once under [`record`] to list the points it reaches, then once per hit,
//! from a fresh fixture each time, with [`crash_at`].
//!
//! Callbacks installed with [`install`] (or [`scoped`]) see only the points reached on the
//! installing thread, so parallel tests never see each other's. Points the
//! daemon reaches on its engine's threads need [`install_shared`], whose
//! callback sees every thread's points and should filter by path. While a
//! callback runs, points it reaches itself (on its own thread) are skipped.
//!
//! The module also holds the other per-thread test seams core has: extra
//! environment for the git commands a thread runs ([`git_env`]), and the
//! state and cache directories a thread uses ([`dirs`]).

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

macro_rules! points {
    ($($(#[$doc:meta])* $name:ident $(=> $fault:ident)?,)*) => {
        /// A place where a test can step in. Each variant says where it is
        /// and which path its callback gets.
        #[non_exhaustive]
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub enum Point {
            $($(#[$doc])* $name,)*
        }

        impl Point {
            /// Every point, in declaration order, for sweeps.
            pub const ALL: &'static [Point] = &[$(Point::$name,)*];

            /// The variant's name, for test labels.
            pub fn name(self) -> &'static str {
                match self {
                    $(Point::$name => stringify!($name),)*
                }
            }

            /// Whether [`Action::Fail`] is meaningful here: the point models
            /// a system call that can fail.
            pub fn can_fail(self) -> bool {
                match self {
                    $(Point::$name => points!(@fault $($fault)?),)*
                }
            }
        }
    };
    (@fault fault) => { true };
    (@fault) => { false };
}

points! {
    // Sync. The path is the harness destination unless noted.
    /// While planning a skill, after observing the destination and before
    /// hashing the source (or reusing the hash an earlier harness of the
    /// same plan took).
    PlanSource,
    /// Before a skill's source is copied. Path: the repo skill.
    BeforeCopy,
    /// For each entry of a skill being copied. Path: the source entry.
    CopyEntry,
    /// While applying, right before the destination is replaced or removed.
    BeforeSwap,
    /// After the last check of the destination, before it is moved or
    /// linked into the recovery area.
    AfterCheck,
    /// After the old content was moved (or a file hard-linked) into the
    /// recovery area, before it is checked there.
    AfterRename,
    /// After the old content was checked in the recovery area, before the
    /// replacement goes in.
    AfterAside,
    /// Right after the replacement went in (or the removal happened),
    /// before the displaced content is checked one last time.
    AfterSwap,
    /// Before a recovery entry is pruned. Path: the entry's content.
    BeforePrune,
    /// Before a managed directory's index is moved into place. Path: the
    /// index file.
    IndexBeforeMove,
    /// Moving content into the recovery area (a rename). Path: where it
    /// goes. Failing it with `CrossesDevices` acts as if the recovery area
    /// were on another file system.
    RecoveryRename => fault,
    /// Hard-linking a file into the recovery area. Path: where it goes.
    RecoveryLink => fault,

    // Hashing.
    /// Once a file's length is taken, before its bytes are read. Path: the file.
    HashMidRead,
    /// Once a placement payload matched its fingerprint, before anything of
    /// it is unlinked. Path: the payload.
    HashBeforeRemoval,

    // Config files (`machine.toml` and the like).
    /// The new text is staged; the last check is next. Path: the file.
    ConfigBeforeCheck,
    /// After the last check, right before the new text is renamed in.
    /// Path: the file.
    ConfigBeforePublish,

    // Item content (the GUI's file view).
    /// After an item's directory or file is opened, before it is read.
    /// Path: the item's root.
    ContentAccess,

    // Pull. The path is the skills repo unless noted.
    /// Unpushed commits are replayed; the last checks against the Draft
    /// are next.
    PullChecks,
    /// The branch moved back from unpushed commits that conflict; what the
    /// un-commit staged is recorded next.
    PullUncommitted,
    /// The last checks passed; the journal is written next.
    PullJournal,
    /// The journal is written; this path is next. Reached once per path,
    /// in the order the pull goes through them. Path: the file.
    PullFile,
    /// Inside a file's replacement: what was there is held in the recovery
    /// area and checked; the new content goes in next. Path: the file.
    PullSwap,
    /// Inside a removal, after the tracked leaf was checked. Path: the file.
    PullRemove,
    /// A file's replacement, addition or removal happened; its completion
    /// is read next. Path: the file.
    PullPublished,
    /// Every path is done; the index is next.
    PullIndex,
    /// The index is updated; the branch moves next.
    PullUpdateRef,
    /// The branch moved; the journal is removed next.
    PullDone,

    // Keeping the Draft's versions through a pull (`Repo::pull_keeping`).
    // The path is the skills repo unless noted.
    /// The keep's journal is written; the restore is next.
    KeepRestore,
    /// Inside the restore: this path's staging is journaled as the keep's
    /// own; its index entry is reset next. Path: the file.
    KeepOwned,
    /// Inside the restore: this file's recovery entry is journaled; the
    /// file has yet to move into it. Path: the file.
    KeepRecorded,
    /// Inside the restore: this file is in recovery, and the Baseline's
    /// version of it (if any) is not in place yet. Path: the file.
    KeepMoved,
    /// Inside the restore: a Baseline file is staged, not published yet.
    KeepStaged,
    /// Inside the restore: the Baseline's file is in. Path: the file.
    KeepPublished,
    /// The kept files are in recovery; the pull is next.
    KeepPull,
    /// The pull went through; the kept files go back next.
    KeepSettle,

    // Draft, pull's file system and first baselines.
    /// Pull's atomic swap of a directory (`RENAME_EXCHANGE`). Path: the
    /// staged directory. Failing it (with any kind but a raw OS error)
    /// reports the exchange unsupported, as on a file system without it.
    PullExchange => fault,
    /// Joining an empty clone to its first baseline: after every checkout
    /// destination was checked, before files are written. Path: the repo.
    BaselinePublish,

    // Imports.
    /// Moving a fetched import into the repo. Path: the destination.
    SourceInstall => fault,
    /// Putting the previous version of an import back after its
    /// replacement failed. Path: the destination.
    SourceRestore => fault,

    // Daemon (sk1ll-daemon's `failpoints` feature). Reached on the engine's
    // threads: install them with `install_shared`. Path: the Machine's
    // `machine.toml` unless noted.
    /// A reviewed sync: after the review matched the plan, before it applies.
    DaemonAfterSyncReview,
    /// A rejoin: the old registration was found replaceable, before the new
    /// one is stored.
    DaemonBeforeRejoinPersist,
    /// `POST /harnesses/apply`: holding the harness-change lock, before the
    /// change is made.
    DaemonApplyEntered,
    /// `POST /harnesses/take-out`: holding the harness-change lock, before
    /// its first check. A callback may block here; the daemon calls it off
    /// its async runtime.
    DaemonTakeOutEntered,
    /// A take-out: after its sync, before `machine.toml` is checked.
    DaemonBeforeTakeOutApply,
    /// A take-out: after its last check, before `machine.toml` is written.
    DaemonBeforeTakeOutWrite,
    /// The machine-local assignment store: before a write is swapped in.
    /// Path: the store file.
    DaemonStoreBeforeSwap,
    /// Joining: after the snapshot was taken, before the daemon state is
    /// saved. Path: the daemon state file. Failing it fails the join there.
    DaemonJoinAfterSnapshot => fault,
    /// Joining: before the last of the credential files is written. Path:
    /// that file.
    DaemonJoinBeforeCredentials,
    /// A Harness rename sent to the server: its answer is in, not applied
    /// yet. A callback may block here; the daemon calls it off its async
    /// runtime.
    DaemonRenameAnswered,
    /// The session's inbox: before a message from the server is handled. A
    /// callback may block here; the daemon calls it off its async runtime.
    DaemonServerMessage,
    /// Before the daemon replaces itself with a fresh copy, as it does after
    /// `POST /join`. Path: the daemon state file. A test's in-process engine
    /// answers [`Action::Crash`] here, or the test process would be replaced.
    DaemonRestart,

    // Server (sk1ll-server's `failpoints` feature).
    /// Confirming that a repo cleanup's git processes stopped. Path: the
    /// repo. Failing it reports the cleanup unconfirmed.
    ServerCleanupConfirmation => fault,
    /// A request (a push, a Machine removal, an inventory read) found the
    /// push lock held and is now in line for it: the lock hands over in
    /// that order. Path: the repo.
    ServerPushLockQueued,
    /// A Machine removal: holding the push lock, on the blocking thread of
    /// its database work, before it locks the database. Path: the repo. A
    /// callback may block here.
    ServerRemovalEntered,
}

/// What a callback has the operation do at a point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Go on.
    Continue,
    /// Unwind out of the operation with a [`Crash`] panic.
    Crash,
    /// Exit the process with this code, without unwinding.
    Exit(i32),
    /// Return an error of this kind from the system call the point models,
    /// with the message `injected at failpoint <name>`. Only where
    /// [`Point::can_fail`]; elsewhere it panics, as a mistake in the test.
    Fail(std::io::ErrorKind),
}

/// The panic payload of [`Action::Crash`]: where the operation stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Crash {
    pub point: Point,
    pub path: PathBuf,
}

/// One point reached, as [`record`] lists them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub point: Point,
    pub path: PathBuf,
}

type Local = Rc<RefCell<dyn FnMut(Point, &Path) -> Action>>;
type Shared = Arc<dyn Fn(Point, &Path) -> Action + Send + Sync>;

static NEXT_ID: AtomicU64 = AtomicU64::new(0);
static SHARED: Mutex<Vec<(u64, Shared)>> = Mutex::new(Vec::new());
static SHARED_COUNT: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    static LOCAL: RefCell<Vec<(u64, Local)>> = const { RefCell::new(Vec::new()) };
    static IN_CALLBACK: Cell<bool> = const { Cell::new(false) };
    static GIT_ENV: RefCell<Vec<(String, String)>> = const { RefCell::new(Vec::new()) };
    static DIRS: RefCell<Option<crate::paths::Dirs>> = const { RefCell::new(None) };
}

/// Removes what it was returned for when dropped.
#[must_use = "the callback is removed when the guard is dropped"]
pub struct Guard {
    remove: Option<Box<dyn FnOnce()>>,
}

impl Guard {
    fn new(remove: impl FnOnce() + 'static) -> Self {
        Self { remove: Some(Box::new(remove)) }
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        if let Some(remove) = self.remove.take() {
            remove();
        }
    }
}

impl std::fmt::Debug for Guard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Guard")
    }
}

/// Calls `f` at every point this thread reaches, until the guard drops.
/// Callbacks run in the order they were installed; the first answer other
/// than [`Action::Continue`] is taken and the rest are skipped.
pub fn install(f: impl FnMut(Point, &Path) -> Action + 'static) -> Guard {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    LOCAL.with_borrow_mut(|local| local.push((id, Rc::new(RefCell::new(f)))));
    Guard::new(move || {
        // A guard dropped during thread teardown finds the list gone already.
        let _ = LOCAL.try_with(|local| local.borrow_mut().retain(|(i, _)| *i != id));
    })
}

/// Runs `op` with `f` called at every point this thread reaches, as
/// [`install`] would, but for a callback that borrows from the caller's
/// stack: `f` is removed when `op` returns or unwinds.
pub fn scoped<R>(f: impl FnMut(Point, &Path) -> Action, op: impl FnOnce() -> R) -> R {
    let mut f = f;
    let f: &mut dyn FnMut(Point, &Path) -> Action = &mut f;
    // SAFETY: only the lifetime is erased. The guard removes the callback
    // from this thread's list before `f` goes out of scope, also when `op`
    // unwinds, and `hit` holds a callback only while calling it, which
    // happens inside `op`.
    let f: &'static mut dyn FnMut(Point, &Path) -> Action = unsafe { std::mem::transmute(f) };
    let _guard = install(f);
    op()
}

/// Calls `f` at every point any thread reaches, until the guard drops: for
/// points reached off the test's own thread (the daemon's engine). Other
/// tests run in parallel, so `f` should act only on its own paths. Runs
/// after this thread's own callbacks. Several threads may call it at once,
/// and it may block (a rendezvous with the test) without holding up the
/// others.
pub fn install_shared(f: impl Fn(Point, &Path) -> Action + Send + Sync + 'static) -> Guard {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    lock_shared().push((id, Arc::new(f)));
    SHARED_COUNT.fetch_add(1, Ordering::SeqCst);
    Guard::new(move || {
        let mut shared = lock_shared();
        let before = shared.len();
        shared.retain(|(i, _)| *i != id);
        SHARED_COUNT.fetch_sub(before - shared.len(), Ordering::SeqCst);
    })
}

/// Calls `f` at every point this thread reaches, for its side effects: the
/// operation always continues.
pub fn observe(mut f: impl FnMut(Point, &Path) + 'static) -> Guard {
    install(move |point, path| {
        f(point, path);
        Action::Continue
    })
}

/// Calls `f` once, at the first time this thread reaches `point`.
pub fn once(point: Point, f: impl FnOnce(&Path) + 'static) -> Guard {
    let mut f = Some(f);
    install(move |at, path| {
        if at == point
            && let Some(f) = f.take()
        {
            f(path);
        }
        Action::Continue
    })
}

/// Calls `f` once, the first time any thread reaches `point` about `path`
/// (exactly that path): for points the daemon reaches on its engine's
/// threads. `f` may block, to hold the operation there until the test
/// lets it go on.
pub fn once_shared(point: Point, path: &Path, f: impl FnOnce(&Path) + Send + 'static) -> Guard {
    let (path, f) = (path.to_path_buf(), Mutex::new(Some(f)));
    install_shared(move |at, about| {
        if at == point
            && about == path
            && let Some(f) = f.lock().unwrap_or_else(|e| e.into_inner()).take()
        {
            f(about);
        }
        Action::Continue
    })
}

/// Fails the system call at `point` with `kind`, every time this thread
/// reaches it.
pub fn fail(point: Point, kind: std::io::ErrorKind) -> Guard {
    assert!(point.can_fail(), "{point:?} cannot fail");
    install(move |at, _| if at == point { Action::Fail(kind) } else { Action::Continue })
}

/// Fails the system call at `point` with `kind` the first time this thread
/// reaches it.
pub fn fail_once(point: Point, kind: std::io::ErrorKind) -> Guard {
    assert!(point.can_fail(), "{point:?} cannot fail");
    let mut armed = true;
    install(move |at, _| if at == point && std::mem::take(&mut armed) { Action::Fail(kind) } else { Action::Continue })
}

/// Crashes ([`Action::Crash`]) at this thread's `n`th hit (from 0) of any
/// point, as [`record`] numbers them.
pub fn crash_at(n: usize) -> Guard {
    let mut seen = 0;
    install(move |_, _| {
        seen += 1;
        if seen == n + 1 { Action::Crash } else { Action::Continue }
    })
}

/// The points this thread reaches while the recording lives, in order.
#[must_use = "the recording stops when it is dropped"]
#[derive(Debug)]
pub struct Recording {
    hits: Rc<RefCell<Vec<Hit>>>,
    _guard: Guard,
}

impl Recording {
    /// The hits so far.
    pub fn hits(&self) -> Vec<Hit> {
        self.hits.borrow().clone()
    }
}

/// Records every point this thread reaches, for a sweep to enumerate.
pub fn record() -> Recording {
    let hits = Rc::new(RefCell::new(Vec::new()));
    let sink = hits.clone();
    let guard = install(move |point, path| {
        sink.borrow_mut().push(Hit { point, path: path.to_path_buf() });
        Action::Continue
    });
    Recording { hits, _guard: guard }
}

/// Runs `f`, turning an [`Action::Crash`] in it into `Err`. Other panics
/// propagate.
pub fn catch<T>(f: impl FnOnce() -> T) -> Result<T, Crash> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(out) => Ok(out),
        Err(payload) => match payload.downcast::<Crash>() {
            Ok(crash) => Err(*crash),
            Err(payload) => std::panic::resume_unwind(payload),
        },
    }
}

/// Reaches `point`: runs the callbacks and does what they answer. For the
/// sk1ll crates' own points; tests install callbacks instead. Returns the
/// error to inject at a point that [can fail](Point::can_fail).
#[doc(hidden)]
pub fn hit(point: Point, path: &Path) -> Option<std::io::Error> {
    if IN_CALLBACK.get() {
        return None;
    }
    let local: Vec<Local> = LOCAL.with_borrow(|local| local.iter().map(|(_, f)| f.clone()).collect());
    let shared: Vec<Shared> = if SHARED_COUNT.load(Ordering::SeqCst) == 0 {
        Vec::new()
    } else {
        lock_shared().iter().map(|(_, f)| f.clone()).collect()
    };
    if local.is_empty() && shared.is_empty() {
        return None;
    }
    let action = {
        IN_CALLBACK.set(true);
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                IN_CALLBACK.set(false);
            }
        }
        let _reset = Reset;
        local
            .iter()
            .map(|f| (f.borrow_mut())(point, path))
            .chain(shared.iter().map(|f| f(point, path)))
            .find(|action| *action != Action::Continue)
            .unwrap_or(Action::Continue)
    };
    match action {
        Action::Continue => None,
        Action::Crash => std::panic::resume_unwind(Box::new(Crash { point, path: path.to_path_buf() })),
        Action::Exit(code) => std::process::exit(code),
        Action::Fail(kind) if point.can_fail() => {
            Some(std::io::Error::new(kind, format!("injected at failpoint {}", point.name())))
        }
        Action::Fail(_) => panic!("failpoint {point:?} cannot fail; crash there instead"),
    }
}

fn lock_shared() -> std::sync::MutexGuard<'static, Vec<(u64, Shared)>> {
    SHARED.lock().unwrap_or_else(|e| e.into_inner())
}

/// Adds `vars` to the environment of every git command this thread runs,
/// until the guard drops. Unlike `std::env::set_var`, it can't leak into
/// tests running in parallel.
pub fn git_env(vars: &[(&str, &str)]) -> Guard {
    let added: Vec<(String, String)> = vars.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    let count = added.len();
    GIT_ENV.with_borrow_mut(|env| env.extend(added));
    Guard::new(move || {
        let _ = GIT_ENV.try_with(|env| {
            let mut env = env.borrow_mut();
            let keep = env.len().saturating_sub(count);
            env.truncate(keep);
        });
    })
}

/// Makes `dirs` this thread's state and cache directories (see
/// [`crate::paths`]) until the guard drops, ahead of what the process was
/// configured with.
pub fn dirs(dirs: crate::paths::Dirs) -> Guard {
    let before = DIRS.with_borrow_mut(|d| d.replace(dirs));
    Guard::new(move || {
        let _ = DIRS.try_with(|d| *d.borrow_mut() = before);
    })
}

pub(crate) fn thread_git_env(cmd: &mut std::process::Command) {
    GIT_ENV.with_borrow(|vars| cmd.envs(vars.iter().map(|(k, v)| (k, v))));
}

pub(crate) fn thread_dirs() -> Option<crate::paths::Dirs> {
    DIRS.with_borrow(Clone::clone)
}
