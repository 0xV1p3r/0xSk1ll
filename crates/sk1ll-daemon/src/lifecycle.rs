//! What the daemon is doing, as one value ([`Lifecycle`]): whether it
//! manages a standalone repo or a Fleet member, how its server connection
//! stands, whether it has stopped managing (removed from the Fleet, or
//! halted for a restart), and where the Machine stands against the server's
//! Baseline (Blocked, a pull pending). Every change is a transition method
//! here; the matching [`StatusView`] fields are only ever derived from it
//! ([`Lifecycle::apply`]).
//!
//! The daemon keeps it behind a mutex of its own, never held across network
//! I/O, so the server session can change the connection while an engine
//! operation runs. Its persistent part ([`Lifecycle::record`]) is saved to
//! the state file on every change (see [`crate::state::PullRecord`]), so a
//! restart starts from the remembered block and server Baseline.

use sk1ll_proto::{ItemRef, headline};

use crate::client::{Connection, StatusView};
use crate::state::{PullRecord, PullState};

/// How a joined daemon's server connection stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Link {
    Connecting,
    Connected,
    /// The server is unreachable; the Machine keeps working from its cache.
    Offline,
    /// The server (or something in front of it) answers 401 to the token. The
    /// Machine keeps working and retrying; only the server's explicit
    /// `revoked` message removes it.
    AuthRejected,
    /// The server speaks another protocol version; retried like Offline.
    Incompatible,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Fleet {
    Standalone,
    /// `rejected_since`: since when (Unix seconds) the server has rejected
    /// the token; only a successful connection clears it.
    Joined {
        link: Link,
        rejected_since: Option<i64>,
    },
    /// Joined (again) and about to restart into the new credentials, or a
    /// failed join left the repo where nothing may touch it: nothing more is
    /// managed, and nothing more is written to the state file. `was`: the
    /// connection shown meanwhile.
    Halted {
        was: Connection,
        rejected_since: Option<i64>,
    },
    /// Removed from the Fleet: the daemon no longer manages this Machine.
    Revoked,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Lifecycle {
    fleet: Fleet,
    record: PullRecord,
    /// Grows whenever what the daemon manages changes (removed, halted), so
    /// an operation can tell that it is still in the world it started in
    /// (see [`crate::daemon::World`]).
    epoch: u64,
}

impl Lifecycle {
    pub(crate) fn standalone(record: PullRecord) -> Self {
        Self { fleet: Fleet::Standalone, record, epoch: 0 }
    }

    /// A joined daemon at startup: connecting, unless it was removed.
    pub(crate) fn joined(revoked: bool, record: PullRecord) -> Self {
        let fleet =
            if revoked { Fleet::Revoked } else { Fleet::Joined { link: Link::Connecting, rejected_since: None } };
        Self { fleet, record, epoch: 0 }
    }

    // -------------------------------------------------------------------
    // Transitions

    /// The session's connection changed. Only a joined, managing daemon has
    /// one; connecting clears an auth rejection. `false`: there is none (a
    /// halted daemon's session belongs to the registration it replaces).
    pub(crate) fn set_link(&mut self, next: Link) -> bool {
        let Fleet::Joined { link, rejected_since } = &mut self.fleet else { return false };
        *link = next;
        if next == Link::Connected {
            *rejected_since = None;
        }
        true
    }

    /// The first step of storing a rejoin's registration, taken in the same
    /// critical section as storing it and [`Lifecycle::halt`]: `Err` says why
    /// the registration a join was allowed to replace at `epoch` is no longer
    /// replaceable (it connected again, or something else changed it).
    pub(crate) fn check_rejoin(&self, epoch: u64) -> Result<(), &'static str> {
        if self.halted() {
            return Err("already joined again; the daemon is restarting");
        }
        if !self.can_rejoin() || self.epoch != epoch {
            return Err("this Machine's registration works again; there is nothing to join");
        }
        Ok(())
    }

    /// The server rejected the token at `now`. Returns since when it has, or
    /// `None` if this daemon has no server connection to reject.
    pub(crate) fn auth_rejected(&mut self, now: i64) -> Option<i64> {
        let Fleet::Joined { link, rejected_since } = &mut self.fleet else { return None };
        *link = Link::AuthRejected;
        Some(*rejected_since.get_or_insert(now))
    }

    /// Removed from the Fleet. A halted daemon is about to become another
    /// registration, so it stays halted.
    pub(crate) fn revoke(&mut self) {
        if matches!(self.fleet, Fleet::Joined { .. }) {
            self.fleet = Fleet::Revoked;
            self.epoch += 1;
        }
    }

    /// Stops managing until the restart (see [`Fleet::Halted`]).
    pub(crate) fn halt(&mut self) {
        let rejected_since = match &self.fleet {
            Fleet::Joined { rejected_since, .. } | Fleet::Halted { rejected_since, .. } => *rejected_since,
            _ => None,
        };
        self.fleet = Fleet::Halted { was: self.connection(), rejected_since };
        self.epoch += 1;
    }

    pub(crate) fn set_pull(&mut self, pull: PullState) {
        self.record.pull = pull;
    }

    /// The server refused a push for good (`Some`), or that no longer
    /// stands (`None`: a push went through, or `HEAD` moved on).
    pub(crate) fn set_push_refused(&mut self, refusal: Option<crate::state::PushRefusal>) {
        self.record.push_refused = refusal;
    }

    pub(crate) fn push_refused(&self) -> Option<&crate::state::PushRefusal> {
        self.record.push_refused.as_ref()
    }

    pub(crate) fn set_server_baseline(&mut self, baseline: Option<String>) {
        self.record.server_baseline = baseline;
    }

    // -------------------------------------------------------------------
    // Queries

    /// `Err`: why nothing may be changed now.
    pub(crate) fn managed(&self) -> Result<(), &'static str> {
        match self.fleet {
            Fleet::Standalone | Fleet::Joined { .. } => Ok(()),
            Fleet::Revoked => Err("this machine was removed from the fleet; the daemon no longer manages it"),
            Fleet::Halted { .. } => Err("the daemon is restarting after joining again; try again"),
        }
    }

    pub(crate) fn epoch(&self) -> u64 {
        self.epoch
    }

    pub(crate) fn idle(&self) -> bool {
        self.managed().is_err()
    }

    pub(crate) fn connected(&self) -> bool {
        matches!(self.fleet, Fleet::Joined { link: Link::Connected, .. })
    }

    pub(crate) fn revoked(&self) -> bool {
        self.fleet == Fleet::Revoked
    }

    pub(crate) fn halted(&self) -> bool {
        matches!(self.fleet, Fleet::Halted { .. })
    }

    /// A join may replace this daemon's identity: standalone, removed, or
    /// refused by the server.
    pub(crate) fn can_rejoin(&self) -> bool {
        matches!(self.fleet, Fleet::Standalone | Fleet::Revoked | Fleet::Joined { link: Link::AuthRejected, .. })
    }

    pub(crate) fn connection(&self) -> Connection {
        match &self.fleet {
            Fleet::Standalone => Connection::Standalone,
            Fleet::Joined { link: Link::Connecting, .. } => Connection::Connecting,
            Fleet::Joined { link: Link::Connected, .. } => Connection::Connected,
            Fleet::Joined { link: Link::Offline, .. } => Connection::Offline,
            Fleet::Joined { link: Link::AuthRejected, .. } => Connection::AuthRejected,
            Fleet::Joined { link: Link::Incompatible, .. } => Connection::Incompatible,
            Fleet::Halted { was, .. } => *was,
            Fleet::Revoked => Connection::Revoked,
        }
    }

    pub(crate) fn pull(&self) -> &PullState {
        &self.record.pull
    }

    /// The last pull stopped short of the server's Baseline (Blocked, or
    /// half-way): worth trying again once the Draft changes.
    pub(crate) fn pull_stopped(&self) -> bool {
        self.record.pull != PullState::Clear
    }

    /// What the state file keeps: whether the Machine was removed, and the record.
    pub(crate) fn persisted(&self) -> (bool, &PullRecord) {
        (self.revoked(), &self.record)
    }

    /// Derives the view's lifecycle fields, and the headline, which follows
    /// from them and the rest of the report.
    pub(crate) fn apply(&self, v: &mut StatusView) {
        v.connection = self.connection();
        v.revoked = self.revoked();
        v.auth_rejected_since = match &self.fleet {
            Fleet::Joined { rejected_since, .. } | Fleet::Halted { rejected_since, .. } => *rejected_since,
            _ => None,
        };
        v.server_baseline.clone_from(&self.record.server_baseline);
        let r = &mut v.report;
        let (blocked, pending): (Option<&Vec<ItemRef>>, Option<&Vec<ItemRef>>) = match &self.record.pull {
            PullState::Clear => (None, None),
            PullState::Blocked { items, .. } => (Some(items), None),
            PullState::Pending { items, .. } => (None, Some(items)),
        };
        r.blocked = blocked.is_some();
        r.clashes = blocked.cloned().unwrap_or_default();
        r.pull_pending = pending.cloned();
        r.push_refused = self.record.push_refused.as_ref().map(|refusal| refusal.reason.clone());
        r.behind = pending.is_some()
            || blocked.is_some()
            || self.record.server_baseline.as_ref().is_some_and(|h| r.baseline.as_ref() != Some(h));
        let offline = !matches!(v.connection, Connection::Connected | Connection::Standalone);
        v.headline = headline(offline, Some(&v.report), v.server_baseline.as_deref());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(s: &str) -> ItemRef {
        s.parse().unwrap()
    }

    #[test]
    fn connection_transitions_only_while_joined() {
        let mut lc = Lifecycle::joined(false, PullRecord::default());
        assert_eq!(lc.connection(), Connection::Connecting);
        assert_eq!(lc.auth_rejected(10), Some(10));
        lc.set_link(Link::Offline);
        // A network error proves nothing about the token.
        assert_eq!(lc.auth_rejected(20), Some(10));
        lc.set_link(Link::Connected);
        assert!(lc.connected());
        assert_eq!(lc.auth_rejected(30), Some(30));

        let mut standalone = Lifecycle::standalone(PullRecord::default());
        standalone.set_link(Link::Connected);
        assert_eq!(standalone.auth_rejected(1), None);
        standalone.revoke();
        assert_eq!(standalone.connection(), Connection::Standalone);
        assert!(standalone.managed().is_ok());
    }

    #[test]
    fn a_halted_daemon_stays_halted() {
        let mut lc = Lifecycle::joined(false, PullRecord::default());
        lc.set_link(Link::Connected);
        lc.halt();
        assert!(lc.idle());
        lc.revoke();
        lc.set_link(Link::Offline);
        assert!(lc.halted());
        assert_eq!(lc.connection(), Connection::Connected);
        assert!(!lc.can_rejoin());
    }

    #[test]
    fn the_view_is_derived_from_the_pull_state() {
        let mut v = crate::client::StatusView {
            machine_id: "m".into(),
            server_url: None,
            mode: crate::client::DaemonMode::Joined,
            has_origin: true,
            repo_ready: true,
            repo: "/r".into(),
            headline: sk1ll_proto::MachineStatus::Offline,
            connection: Connection::Connecting,
            last_error: None,
            revoked: false,
            server_baseline: None,
            assignment_revision: 1,
            version: String::new(),
            protocol: 1,
            server: None,
            keep_report: Vec::new(),
            awaiting_assignment: false,
            auth_rejected_since: None,
            report: sk1ll_proto::MachineReport { baseline: Some("old".into()), ..Default::default() },
        };
        let record = PullRecord {
            server_baseline: Some("new".into()),
            pull: PullState::Blocked { baseline: "new".into(), items: vec![item("skill:pdf")] },
            push_refused: None,
        };
        let mut lc = Lifecycle::joined(false, record);
        lc.set_link(Link::Connected);
        lc.apply(&mut v);
        assert!(v.report.blocked && v.report.behind);
        assert_eq!(v.report.clashes, [item("skill:pdf")]);
        assert_eq!(v.headline, sk1ll_proto::MachineStatus::Blocked);
        assert_eq!(v.server_baseline.as_deref(), Some("new"));

        lc.set_pull(PullState::Pending { baseline: String::new(), items: Vec::new() });
        lc.apply(&mut v);
        assert!(!v.report.blocked && v.report.clashes.is_empty());
        assert_eq!(v.report.pull_pending, Some(Vec::new()));

        lc.set_pull(PullState::Clear);
        lc.set_server_baseline(Some("old".into()));
        lc.apply(&mut v);
        assert!(!v.report.behind && v.report.pull_pending.is_none());
        assert_eq!(v.headline, sk1ll_proto::MachineStatus::InSync);

        lc.revoke();
        lc.apply(&mut v);
        assert!(v.revoked);
        assert_eq!(v.connection, Connection::Revoked);
        assert!(lc.persisted().0);
    }
}
