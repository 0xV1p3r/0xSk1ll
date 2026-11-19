//! Desktop alerts owned by the daemon, independent of the local API clients.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sk1ll_core::machine::MachineConfig;
use tokio::sync::watch;

use crate::client::{Connection, StatusView};
use crate::daemon::Daemon;
use crate::state::write_private;

type Items = BTreeSet<String>;

#[derive(Debug, Clone, Copy, Ord, PartialOrd, Eq, PartialEq, Serialize, Deserialize)]
enum Kind {
    Blocked,
    Conflict,
    Revoked,
    AuthRejected,
    Imports,
}

impl Kind {
    fn title(self) -> &'static str {
        match self {
            Self::Blocked => "Baseline pull blocked",
            Self::Conflict => "New harness Conflicts",
            Self::Revoked => "Machine removed from the Fleet",
            Self::AuthRejected => "Server authentication rejected",
            Self::Imports => "Import checks failed",
        }
    }
}

#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Alert {
    active: Items,
    pending: bool,
    sent_at: Option<i64>,
    #[serde(default)]
    retry_at: Option<i64>,
    #[serde(default)]
    failures: usize,
}

type History = BTreeMap<Kind, Alert>;

impl Alert {
    fn observe(&mut self, kind: Kind, items: Items, now: i64) -> bool {
        let grew = !self.active.is_empty() && items.len() > self.active.len();
        let new_failure = kind == Kind::Imports && !items.is_subset(&self.active);
        if items.is_empty() {
            self.pending = false;
            self.retry_at = None;
            self.failures = 0;
        } else if self.active.is_empty() || grew || new_failure {
            self.pending = true;
        }
        self.active = items;
        // A recurrence inside the cooldown is queued while the issue is active.
        // A larger active set can be reported immediately.
        let past_cooldown = self.sent_at.is_none_or(|sent| now.saturating_sub(sent) >= 600);
        self.pending
            && (past_cooldown || grew || self.retry_at.is_some())
            && self.retry_at.is_none_or(|retry| now >= retry)
    }

    fn delivered(&mut self, now: i64, success: bool) {
        if success {
            self.pending = false;
            self.sent_at = Some(now);
            self.retry_at = None;
            self.failures = 0;
        } else {
            let delay = [60, 300, 600][self.failures.min(2)];
            self.failures = (self.failures + 1).min(2);
            self.retry_at = Some(now.saturating_add(delay));
        }
    }
}

fn load(path: &Path) -> History {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|err| {
            tracing::debug!(%err, "could not parse desktop notification history");
            History::default()
        }),
        Err(err) => {
            if err.kind() != std::io::ErrorKind::NotFound {
                tracing::debug!(%err, "could not read desktop notification history");
            }
            History::default()
        }
    }
}

fn save(path: &Path, history: &History) -> anyhow::Result<()> {
    write_private(path, &serde_json::to_vec(history)?)
}

fn enabled(d: &Daemon) -> bool {
    std::env::var_os("SK1LL_NOTIFICATIONS").as_deref() != Some("0".as_ref())
        && MachineConfig::load(&d.cfg.machine_config).is_ok_and(|config| config.notifications.enabled)
}

pub(crate) async fn run(d: Arc<Daemon>) {
    let (imports, mut failures) = watch::channel(None);
    tokio::join!(poll_imports(&d, imports), async {
        let path = d.cfg.state.with_extension("notifications.json");
        let mut history = load(&path);
        let mut status = d.subscribe_view();
        let mut rejected_since = None;
        let mut tick = tokio::time::interval(Duration::from_secs(5));
        loop {
            tokio::select! {
                _ = tick.tick() => {}
                _ = status.changed() => {}
                _ = failures.changed() => {}
            }
            let view = status.borrow_and_update().clone();
            let auth = auth_items(&view, &mut rejected_since, Instant::now());
            if !enabled(&d) {
                continue;
            }
            let import_failures = failures.borrow_and_update().clone();
            let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() as i64;
            let machine = || Items::from([view.machine_id.clone()]);
            let blocked = if !view.report.blocked {
                Items::new()
            } else if view.report.clashes.is_empty() {
                machine()
            } else {
                view.report.clashes.iter().map(ToString::to_string).collect()
            };
            let observations = [
                // Before the first pull, an empty report cannot prove a Blocked state cleared.
                (
                    Kind::Blocked,
                    (view.report.blocked || d.pull_cleared_blocked.load(Ordering::Relaxed)).then_some(blocked),
                ),
                (
                    Kind::Conflict,
                    (view.repo_ready && !view.awaiting_assignment && !view.revoked).then(|| {
                        view.report.conflicts.iter().map(|c| format!("{} in {}", c.item, c.harness)).collect()
                    }),
                ),
                (Kind::Revoked, Some(if view.revoked { machine() } else { Items::new() })),
                (Kind::AuthRejected, auth),
                (Kind::Imports, import_failures),
            ];
            let mut changed = false;
            for (kind, items) in observations {
                let Some(items) = items else { continue };
                let alert = history.entry(kind).or_default();
                let before = alert.clone();
                if alert.observe(kind, items, now) {
                    let result = deliver(kind.title(), &body(&alert.active)).await;
                    alert.delivered(now, result.is_ok());
                    if let Err(err) = result {
                        tracing::debug!(%err, "could not send desktop notification");
                    }
                }
                changed |= before != *alert;
            }
            if changed && let Err(err) = save(&path, &history) {
                tracing::debug!(%err, "could not save desktop notification history");
            }
        }
    });
}

fn auth_items(view: &StatusView, since: &mut Option<Instant>, now: Instant) -> Option<Items> {
    if view.connection == Connection::AuthRejected {
        let started = *since.get_or_insert(now);
        return (now.duration_since(started) >= Duration::from_secs(120))
            .then(|| Items::from([view.machine_id.clone()]));
    }
    *since = None;
    // Preserve history when the daemon has yet to authenticate after a restart.
    matches!(view.connection, Connection::Connected | Connection::Standalone | Connection::Revoked).then(Items::new)
}

async fn poll_imports(d: &Arc<Daemon>, failures: watch::Sender<Option<Items>>) {
    let mut tick = tokio::time::interval(Duration::from_secs(30));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last_local = None::<tokio::time::Instant>;
    let mut checked_at = None;
    loop {
        tick.tick().await;
        let view = d.view();
        if !enabled(d) || view.revoked || !view.repo_ready {
            continue;
        }
        let result = if d.is_standalone() && last_local.is_none_or(|last| last.elapsed() >= Duration::from_secs(3600)) {
            last_local = Some(tokio::time::Instant::now());
            d.local_imports(true).await
        } else {
            d.local_imports(false).await
        };
        let result = match result {
            Ok(imports) if !imports.imports.is_empty() && view.connection == Connection::Connected => {
                Ok(crate::api::import_checks(d, imports).await)
            }
            other => other,
        };
        match result {
            Ok(view) if view.imports.is_empty() || (view.checked_at.is_some() && view.checked_at >= checked_at) => {
                checked_at = if view.imports.is_empty() { None } else { view.checked_at };
                failures.send_if_modified(|previous| {
                    let current = Some(view.errors.into_iter().map(|err| err.name).collect());
                    if *previous == current {
                        return false;
                    }
                    *previous = current;
                    true
                });
            }
            Ok(_) => {}
            Err(err) => tracing::debug!(%err, "could not read Import check results"),
        }
    }
}

fn body(items: &Items) -> String {
    let mut names = items.iter().take(3).map(|item| item.chars().take(80).collect::<String>()).collect::<Vec<_>>();
    if items.len() > names.len() {
        names.push(format!("and {} more", items.len() - names.len()));
    }
    format!("{}. Open 0xSk1ll to resolve.", names.join(", "))
}

#[cfg(any(test, target_os = "macos"))]
fn applescript(title: &str, body: &str) -> String {
    fn escape(text: &str) -> String {
        text.replace('\\', "\\\\").replace('"', "\\\"").replace('\r', "\\r").replace('\n', "\\n")
    }
    format!("display notification \"{}\" with title \"{}\"", escape(body), escape(title))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn deliver(title: &str, body: &str) -> anyhow::Result<()> {
    use tokio::process::Command;

    #[cfg(target_os = "linux")]
    let mut command = {
        let mut command = Command::new("notify-send");
        let body = body.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
        command.args(["--app-name=0xSk1ll", "--", title, &body]);
        command
    };
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut command = Command::new("/usr/bin/osascript");
        command.args(["-e", &applescript(title, body)]);
        command
    };
    let output = tokio::time::timeout(Duration::from_secs(5), command.kill_on_drop(true).output()).await??;
    anyhow::ensure!(output.status.success(), "desktop notification command exited with {}", output.status);
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
async fn deliver(_: &str, _: &str) -> anyhow::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn items(names: &[&str]) -> Items {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    #[test]
    fn conflict_count_growth_bypasses_cooldown_and_recurrence_waits() {
        let mut alert = Alert::default();
        let mut calls = Vec::new();
        let mut observe = |now, names: &[&str]| {
            if alert.observe(Kind::Conflict, items(names), now) {
                calls.push((now, alert.active.clone()));
                alert.delivered(now, true);
            }
        };
        observe(0, &["a"]);
        observe(10, &["b"]);
        observe(20, &["b", "c"]);
        observe(30, &["b"]);
        observe(40, &[]);
        observe(50, &["b"]);
        observe(619, &["b"]);
        observe(620, &["b"]);
        observe(1800, &["b"]);
        assert_eq!(calls, [(0, items(&["a"])), (20, items(&["b", "c"])), (620, items(&["b"]))]);
    }

    #[test]
    fn new_import_failure_is_reported_after_cooldown_even_at_same_count() {
        let mut alert = Alert::default();
        assert!(alert.observe(Kind::Imports, items(&["a"]), 0));
        alert.delivered(0, true);
        assert!(!alert.observe(Kind::Imports, items(&["b"]), 1));
        assert!(alert.observe(Kind::Imports, items(&["b"]), 600));
    }

    #[test]
    fn unchanged_alert_survives_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.notifications.json");
        let mut alert = Alert::default();
        assert!(alert.observe(Kind::Revoked, items(&["machine"]), 0));
        alert.delivered(0, true);
        save(&path, &History::from([(Kind::Revoked, alert)])).unwrap();
        assert!(!load(&path).get_mut(&Kind::Revoked).unwrap().observe(Kind::Revoked, items(&["machine"]), 1200));
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(path.metadata().unwrap().permissions().mode() & 0o777, 0o600);
    }

    #[test]
    fn unchanged_conflict_retries_after_delivery_failure() {
        let mut alert = Alert::default();
        assert!(alert.observe(Kind::Conflict, items(&["conflict"]), 0));
        alert.delivered(0, false);
        assert_eq!(alert.sent_at, None);
        assert!(!alert.observe(Kind::Conflict, items(&["conflict"]), 59));
        assert!(alert.observe(Kind::Conflict, items(&["conflict"]), 60));
        alert.delivered(60, true);
        assert_eq!(alert.sent_at, Some(60));
        assert!(!alert.observe(Kind::Conflict, items(&["conflict"]), 660));
    }

    #[test]
    fn escapes_applescript_string_literals() {
        assert_eq!(
            applescript("a\\b", "say \"hi\"\r\nnext"),
            r#"display notification "say \"hi\"\r\nnext" with title "a\\b""#,
        );
    }
}
