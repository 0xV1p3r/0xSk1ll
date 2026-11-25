use sk1ll_core::EngineLock;
use sk1ll_testkit::{Home, git::git};

#[test]
fn second_holder_is_refused_until_the_first_drops() {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q"]);
    let home = Home::new();
    let _entered = home.enter();

    let first = EngineLock::acquire(dir.path()).unwrap();
    assert!(EngineLock::try_acquire(dir.path()).unwrap().is_none());
    assert!(EngineLock::acquire(dir.path()).is_err());
    drop(first);
    assert!(EngineLock::try_acquire(dir.path()).unwrap().is_some());
}

/// A child forked on another thread (to run git, say) shares the lock
/// file's open description until it reaches exec. Dropping the lock must
/// release it all the same.
#[cfg(unix)]
#[test]
fn a_forked_child_does_not_keep_a_dropped_lock() {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q"]);
    let home = Home::new();
    let _entered = home.enter();
    let first = EngineLock::acquire(dir.path()).unwrap();
    // SAFETY: the child only waits for a signal, which is async-signal-safe.
    let child = unsafe { libc::fork() };
    if child == 0 {
        loop {
            // SAFETY: as above.
            unsafe { libc::pause() };
        }
    }
    assert!(child > 0, "fork failed");
    drop(first);
    let second = EngineLock::try_acquire(dir.path());
    // SAFETY: `child` is our own child, killed and reaped once.
    unsafe {
        libc::kill(child, libc::SIGKILL);
        libc::waitpid(child, std::ptr::null_mut(), 0);
    }
    assert!(second.unwrap().is_some(), "the forked child kept the lock");
}
