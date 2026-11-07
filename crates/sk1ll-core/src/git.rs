//! Thin wrapper around the `git` binary. Shelling out (rather than linking a
//! git library) keeps auth (SSH agent, credential helpers), hooks and config
//! behaving exactly like the user's own git.
//!
//! Every invocation runs under an execution deadline, so a hung remote, ssh
//! prompt or hook can't hold the caller (and any lock it holds) forever:
//!
//! - Network commands (`fetch`, `push`, `clone`, `ls-remote`, `pull`):
//!   `SK1LL_GIT_TIMEOUT`, default 10 minutes.
//! - Everything else (local commands, which may still run hooks):
//!   `SK1LL_GIT_LOCAL_TIMEOUT`, default 30 minutes.
//!
//! Values are seconds, or a number with `ms`, `s`, `m` or `h`; `0`, `none`
//! or `off` disables the deadline. On Unix git runs in its own process group;
//! when the deadline passes the group gets SIGTERM, then SIGKILL after a
//! short grace period, and the command fails with "timed out after ...".

use std::ffi::{OsStr, OsString};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use crate::{Error, Result};

/// One git invocation, built up like [`Command`] and run under the deadline
/// described in the module docs. Front-ends (the daemon) use it so that none
/// of their git commands can hang either.
pub struct Git {
    cmd: Command,
    args: Vec<OsString>,
    stdin: Option<Vec<u8>>,
    /// `Some(..)`: overrides the default deadline for this command.
    timeout: Option<Option<Duration>>,
}

impl Git {
    /// `git -C <dir>`.
    pub fn new(dir: &Path) -> Self {
        let mut cmd = Command::new("git");
        cmd.arg("-C").arg(dir);
        if cfg!(test) {
            // Unit tests must not depend on the developer's own git config.
            let null = if cfg!(windows) { "NUL" } else { "/dev/null" };
            cmd.env("GIT_CONFIG_GLOBAL", null).env("GIT_CONFIG_NOSYSTEM", "1");
            #[cfg(test)]
            TEST_ENV.with_borrow(|vars| cmd.envs(vars.iter().map(|(k, v)| (k, v))));
        }
        Self { cmd, args: Vec::new(), stdin: None, timeout: None }
    }

    /// Takes pathspecs literally, so file names are never globs. Only for
    /// commands given file paths: it also changes git's internal pathspecs
    /// (e.g. `stash -u` would no longer remove untracked files).
    pub(crate) fn literal(self) -> Self {
        self.env("GIT_LITERAL_PATHSPECS", "1")
    }

    pub fn arg(mut self, arg: impl AsRef<OsStr>) -> Self {
        self.args.push(arg.as_ref().to_os_string());
        self
    }

    pub fn args<I: IntoIterator<Item = S>, S: AsRef<OsStr>>(mut self, args: I) -> Self {
        self.args.extend(args.into_iter().map(|a| a.as_ref().to_os_string()));
        self
    }

    pub fn env(mut self, key: &str, value: impl AsRef<OsStr>) -> Self {
        self.cmd.env(key, value);
        self
    }

    pub(crate) fn envs<'a>(mut self, vars: impl IntoIterator<Item = &'a (String, String)>) -> Self {
        for (k, v) in vars {
            self.cmd.env(k, v);
        }
        self
    }

    pub fn stdin(mut self, input: Vec<u8>) -> Self {
        self.stdin = Some(input);
        self
    }

    /// Overrides the execution deadline (`None`: no deadline).
    pub fn timeout(mut self, timeout: Option<Duration>) -> Self {
        self.timeout = Some(timeout);
        self
    }

    fn describe(&self) -> String {
        self.args.iter().map(|a| a.to_string_lossy()).collect::<Vec<_>>().join(" ")
    }

    /// Runs git and returns its output whatever the exit status. Running out
    /// of time is an [`Error::Git`].
    pub fn output(mut self) -> Result<Output> {
        let describe = self.describe();
        let timeout = match self.timeout {
            Some(t) => t,
            None => default_timeout(subcommand(&self.args).as_deref().is_some_and(is_network)),
        };
        let spawn_err = |e: std::io::Error| Error::Git { args: describe.clone(), stderr: e.to_string() };
        self.cmd.args(&self.args).stdout(Stdio::piped()).stderr(Stdio::piped());
        self.cmd.stdin(if self.stdin.is_some() { Stdio::piped() } else { Stdio::null() });
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut self.cmd, 0);
        let mut child = self.cmd.spawn().map_err(spawn_err)?;

        // Feed stdin and drain stdout/stderr on threads, so a child that
        // stops reading or writing can't block us past the deadline.
        let writer = match (self.stdin.take(), child.stdin.take()) {
            (Some(input), Some(mut pipe)) => Some(thread::spawn(move || pipe.write_all(&input))),
            _ => None,
        };
        let (tx, rx) = mpsc::channel();
        for (stream, pipe) in [
            (0, child.stdout.take().map(|p| Box::new(p) as Box<dyn Read + Send>)),
            (1, child.stderr.take().map(|p| Box::new(p) as Box<dyn Read + Send>)),
        ] {
            let tx = tx.clone();
            thread::spawn(move || {
                let mut buf = Vec::new();
                let res = pipe.map_or(Ok(0), |mut p| p.read_to_end(&mut buf));
                let _ = tx.send((stream, res.map(|_| buf)));
            });
        }
        drop(tx);

        let deadline = timeout.map(|t| Instant::now() + t);
        let status = wait_until(&mut child, deadline).map_err(spawn_err)?;
        let timed_out = status.is_none();
        let status = match status {
            Some(status) => status,
            None => terminate(&mut child).map_err(spawn_err)?,
        };
        // The pipes close once every process holding them is gone; a
        // straggler (a backgrounded grandchild) gets killed at the deadline.
        let mut streams: [Option<Vec<u8>>; 2] = [None, None];
        let mut killed_stragglers = false;
        while streams.iter().any(Option::is_none) {
            let got = match deadline {
                _ if timed_out || killed_stragglers => rx.recv_timeout(GRACE).map_err(|_| ()),
                Some(d) => rx.recv_timeout(d.saturating_duration_since(Instant::now())).map_err(|_| ()),
                None => rx.recv().map_err(|_| ()),
            };
            match got {
                Ok((i, res)) => streams[i] = Some(res.map_err(spawn_err)?),
                Err(()) if !killed_stragglers && !timed_out => {
                    kill_group(&mut child);
                    killed_stragglers = true;
                }
                Err(()) => break,
            }
        }
        let [stdout, stderr] = streams.map(Option::unwrap_or_default);
        let finished = !timed_out && !killed_stragglers;
        // Only join the writer once git is done with its input; after a
        // timeout it's left to finish (or fail) on its own. Git not reading
        // all of its input is not an error in itself.
        if finished
            && let Some(writer) = writer
            && let Ok(Err(e)) = writer.join()
            && e.kind() != std::io::ErrorKind::BrokenPipe
        {
            return Err(spawn_err(e));
        }
        if !finished {
            let limit = timeout.map_or_else(String::new, format_duration);
            let err = String::from_utf8_lossy(&stderr).trim().to_string();
            let mut msg = format!("timed out after {limit} (see SK1LL_GIT_TIMEOUT / SK1LL_GIT_LOCAL_TIMEOUT)");
            if !err.is_empty() {
                msg.push_str(": ");
                msg.push_str(&err);
            }
            return Err(Error::Git { args: describe, stderr: msg });
        }
        Ok(Output { status, stdout, stderr })
    }

    /// Runs git; a non-zero exit is an [`Error::Git`]. Returns raw stdout.
    pub fn run(self) -> Result<Vec<u8>> {
        let describe = self.describe();
        let out = self.output()?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            let stderr =
                if stderr.is_empty() { String::from_utf8_lossy(&out.stdout).trim().to_string() } else { stderr };
            return Err(Error::Git { args: describe, stderr });
        }
        Ok(out.stdout)
    }

    /// Like [`run`](Self::run), with stdout as trimmed text.
    pub fn text(self) -> Result<String> {
        Ok(String::from_utf8_lossy(&self.run()?).trim().to_string())
    }

    /// Whether git exits successfully; failing to spawn git is still an error.
    pub(crate) fn succeeds(self) -> Result<bool> {
        Ok(self.output()?.status.success())
    }
}

/// How long a timed-out git gets between SIGTERM and SIGKILL, and how long
/// its pipes get to close after that.
const GRACE: Duration = Duration::from_secs(2);

const NETWORK_COMMANDS: [&str; 5] = ["fetch", "push", "clone", "ls-remote", "pull"];
const DEFAULT_NETWORK_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const DEFAULT_LOCAL_TIMEOUT: Duration = Duration::from_secs(30 * 60);

fn is_network(subcommand: &str) -> bool {
    NETWORK_COMMANDS.contains(&subcommand)
}

/// The deadline from the environment (see the module docs). An unparsable
/// value falls back to the default rather than disabling the deadline.
fn default_timeout(network: bool) -> Option<Duration> {
    let (var, default) = if network {
        ("SK1LL_GIT_TIMEOUT", DEFAULT_NETWORK_TIMEOUT)
    } else {
        ("SK1LL_GIT_LOCAL_TIMEOUT", DEFAULT_LOCAL_TIMEOUT)
    };
    std::env::var(var).ok().and_then(|v| parse_timeout(&v)).unwrap_or(Some(default))
}

/// `90`, `90s`, `500ms`, `10m`, `1h`; `0`/`none`/`off` is `Some(None)`
/// (no deadline). `None` if it doesn't parse.
fn parse_timeout(value: &str) -> Option<Option<Duration>> {
    let v = value.trim().to_ascii_lowercase();
    if matches!(v.as_str(), "none" | "off") {
        return Some(None);
    }
    let split = v.find(|c: char| !c.is_ascii_digit()).unwrap_or(v.len());
    let (n, unit) = v.split_at(split);
    let n: u64 = n.parse().ok()?;
    let d = match unit.trim() {
        "" | "s" => Duration::from_secs(n),
        "ms" => Duration::from_millis(n),
        "m" => Duration::from_secs(n.checked_mul(60)?),
        "h" => Duration::from_secs(n.checked_mul(3600)?),
        _ => return None,
    };
    Some((!d.is_zero()).then_some(d))
}

fn format_duration(d: Duration) -> String {
    let secs = d.as_secs();
    match secs {
        0 => format!("{}ms", d.as_millis()),
        s if s % 3600 == 0 => format!("{}h", s / 3600),
        s if s % 60 == 0 => format!("{}m", s / 60),
        s => format!("{s}s"),
    }
}

/// The git subcommand among `args`, skipping global options (and the value
/// of those that take one).
fn subcommand(args: &[OsString]) -> Option<String> {
    let mut args = args.iter().map(|a| a.to_string_lossy());
    while let Some(arg) = args.next() {
        match arg.as_ref() {
            "-c" | "-C" | "--git-dir" | "--work-tree" | "--namespace" | "--exec-path" | "--config-env" => {
                args.next();
            }
            a if a.starts_with('-') => {}
            a => return Some(a.to_string()),
        }
    }
    None
}

/// Waits for `child` to exit; `None` if `deadline` passes first.
fn wait_until(child: &mut Child, deadline: Option<Instant>) -> std::io::Result<Option<ExitStatus>> {
    let Some(deadline) = deadline else {
        return child.wait().map(Some);
    };
    let mut pause = Duration::from_millis(1);
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        let now = Instant::now();
        if now >= deadline {
            return Ok(None);
        }
        thread::sleep(pause.min(deadline - now));
        pause = (pause * 2).min(Duration::from_millis(50));
    }
}

/// Asks git (and everything it started) to stop, then makes it; returns once
/// git is reaped.
fn terminate(child: &mut Child) -> std::io::Result<ExitStatus> {
    #[cfg(unix)]
    {
        signal_group(child, libc::SIGTERM);
        if let Some(status) = wait_until(child, Some(Instant::now() + GRACE))? {
            // Git is gone; take anything it left behind in its group with it.
            signal_group(child, libc::SIGKILL);
            return Ok(status);
        }
    }
    kill_group(child);
    child.wait()
}

/// SIGKILLs git's process group (just git where there are no groups).
fn kill_group(child: &mut Child) {
    #[cfg(unix)]
    signal_group(child, libc::SIGKILL);
    #[cfg(not(unix))]
    let _ = child.kill();
}

#[cfg(unix)]
fn signal_group(child: &Child, signal: libc::c_int) {
    // The child leads its own group (`process_group(0)`), so its pid is the
    // group id. A missing group (ESRCH) just means everyone is gone already.
    if let Ok(pgid) = libc::pid_t::try_from(child.id()) {
        // SAFETY: kill(2) has no memory-safety preconditions.
        unsafe {
            libc::kill(-pgid, signal);
        }
    }
}

#[cfg(test)]
thread_local! {
    static TEST_ENV: std::cell::RefCell<Vec<(String, String)>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Sets extra environment for every git command this test thread runs (each
/// test has its own thread), until the guard is dropped. Unlike
/// `std::env::set_var`, this can't leak into tests running in parallel.
#[cfg(test)]
pub(crate) fn test_env(vars: &[(&str, &str)]) -> impl Drop + use<> {
    struct Guard;
    impl Drop for Guard {
        fn drop(&mut self) {
            TEST_ENV.with_borrow_mut(Vec::clear);
        }
    }
    TEST_ENV.with_borrow_mut(|env| env.extend(vars.iter().map(|(k, v)| (k.to_string(), v.to_string()))));
    Guard
}

/// NUL-separated paths, as `--stdin -z` expects.
pub(crate) fn nul_list<P: AsRef<Path>>(paths: &[P]) -> Vec<u8> {
    let mut out = Vec::new();
    for p in paths {
        out.extend_from_slice(&path_bytes(p.as_ref()));
        out.push(0);
    }
    out
}

/// A path as git spells it. On Unix that is the path's exact bytes (a `\` is
/// an ordinary file-name character there); only Windows separators become `/`.
pub(crate) fn path_bytes(path: &Path) -> Vec<u8> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        path.as_os_str().as_bytes().to_vec()
    }
    #[cfg(not(unix))]
    {
        path.to_string_lossy().replace('\\', "/").into_bytes()
    }
}

/// The inverse of [`path_bytes`]: a path from git's `-z` output.
pub(crate) fn path_from_bytes(raw: &[u8]) -> PathBuf {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        PathBuf::from(OsStr::from_bytes(raw))
    }
    #[cfg(not(unix))]
    {
        // Git for Windows writes paths as UTF-8.
        PathBuf::from(String::from_utf8_lossy(raw).as_ref())
    }
}

/// Paths from git's NUL-separated (`-z`) output.
pub(crate) fn split_paths(out: &[u8]) -> impl Iterator<Item = PathBuf> + '_ {
    out.split(|b| *b == 0).filter(|p| !p.is_empty()).map(path_from_bytes)
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;

    #[cfg(unix)]
    fn alive(pid: i32) -> bool {
        // Signal 0 checks for existence; a zombie still counts, so look at /proc too.
        let exists = unsafe { libc::kill(pid, 0) } == 0;
        exists
            && std::fs::read_to_string(format!("/proc/{pid}/stat"))
                .map_or(true, |s| s.rsplit(')').next().is_some_and(|rest| !rest.trim_start().starts_with('Z')))
    }

    #[cfg(unix)]
    #[test]
    fn hung_network_command_times_out_and_is_killed() {
        let tmp = tempfile::tempdir().unwrap();
        let pidfile = tmp.path().join("ssh.pid");
        // A fake ssh that hangs (and records its pid) instead of connecting.
        let ssh = format!("sleep 30 & echo $! > '{}'; wait; :", pidfile.display());
        let _env = test_env(&[("GIT_SSH_COMMAND", &ssh), ("GIT_SSH_VARIANT", "simple")]);
        let start = Instant::now();
        let result = Git::new(tmp.path())
            .args(["ls-remote", "ssh://sk1ll.invalid/repo.git"])
            .timeout(Some(Duration::from_millis(300)))
            .run();
        let elapsed = start.elapsed();
        match result {
            Err(Error::Git { args, stderr }) => {
                assert!(args.starts_with("ls-remote"), "{args}");
                assert!(stderr.contains("timed out after"), "{stderr}");
            }
            other => panic!("expected a timeout, got {other:?}"),
        }
        assert!(elapsed < Duration::from_secs(10), "took {elapsed:?}");
        let pid: i32 = std::fs::read_to_string(&pidfile).unwrap().trim().parse().unwrap();
        let gone = Instant::now() + Duration::from_secs(5);
        while alive(pid) && Instant::now() < gone {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!alive(pid), "the hung ssh ({pid}) survived the timeout");
    }

    #[test]
    fn stdin_and_output_still_flow_under_a_deadline() {
        let tmp = tempfile::tempdir().unwrap();
        Git::new(tmp.path()).args(["init", "-q"]).run().unwrap();
        // Larger than a pipe buffer both ways, so neither side can deadlock.
        let input: Vec<u8> = (0..4 << 20).map(|i| (i % 251) as u8).collect();
        let oid = Git::new(tmp.path())
            .args(["hash-object", "-w", "--stdin"])
            .stdin(input.clone())
            .timeout(Some(Duration::from_secs(60)))
            .text()
            .unwrap();
        let out = Git::new(tmp.path()).args(["cat-file", "blob", &oid]).timeout(Some(Duration::from_secs(60))).run();
        assert_eq!(out.unwrap(), input);
    }

    #[test]
    fn deadlines_parse_and_pick_network_commands() {
        assert_eq!(parse_timeout("90"), Some(Some(Duration::from_secs(90))));
        assert_eq!(parse_timeout("2m"), Some(Some(Duration::from_secs(120))));
        assert_eq!(parse_timeout("1h"), Some(Some(Duration::from_secs(3600))));
        assert_eq!(parse_timeout("500ms"), Some(Some(Duration::from_millis(500))));
        assert_eq!(parse_timeout("0"), Some(None));
        assert_eq!(parse_timeout("none"), Some(None));
        assert_eq!(parse_timeout("soon"), None);
        let sub = |args: &[&str]| subcommand(&args.iter().map(OsString::from).collect::<Vec<_>>());
        assert_eq!(sub(&["-c", "user.name=t", "push", "origin"]).as_deref(), Some("push"));
        assert!(is_network(&sub(&["fetch", "--quiet", "origin"]).unwrap()));
        assert!(!is_network(&sub(&["--no-pager", "commit-tree", "x"]).unwrap()));
    }
}
