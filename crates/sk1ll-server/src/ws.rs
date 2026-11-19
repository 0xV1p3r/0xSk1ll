//! `GET /api/ws`: one daemon session per machine. Server messages go out as JSON
//! text frames. The server pings every `ping_every`, refreshes `last_seen` on
//! inbound frames at a bounded rate, and closes a session that has been silent for
//! [`Config::idle_limit`](crate::Config::idle_limit).
//!
//! Each session has an id (its generation) and a cancellation token. Being
//! replaced by a newer connection, removing the machine, a full queue, a
//! missed send deadline or the socket ending cancels it, which ends both its
//! reader and its writer, even a writer stuck in a send. A status report is
//! accepted only while the session is current and its token still valid.
//!
//! Discards with an id are acknowledged only after a successful commit, including
//! no-ops. Invalid or stale sessions receive no acknowledgement. If the
//! baseline's items can't be read, the session is closed with code 1013 (try
//! again later), so the daemon reconnects, backing off, and sends its
//! unacknowledged discards again; later discards on that session aren't read.
//!
//! Outbound state is bounded: the assignment is coalesced in a `watch` (only
//! the newest revision is kept), and other messages go through a small queue.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade, close_code};
use axum::extract::{FromRequestParts, State};
use axum::http::request::Parts;
use axum::response::Response;
use futures_util::stream::SplitStream;
use futures_util::{Sink, SinkExt, StreamExt};
use serde_json::Value;
use sk1ll_proto::{AssignmentSnapshot, DaemonMessage, ItemRef, MachineReport, ServerMessage};
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

use crate::error::{ApiError, Result};
use crate::{Caller, Server, assign, db, now};

/// Non-assignment messages a session may have waiting; a daemon that falls
/// this far behind is disconnected.
const QUEUE: usize = 64;

/// An authenticated daemon: its machine id and the hash of its token.
pub struct WsCaller(String, String);

impl FromRequestParts<Arc<Server>> for WsCaller {
    type Rejection = ApiError;

    /// Token from `Authorization: Bearer`; machine tokens only.
    async fn from_request_parts(parts: &mut Parts, server: &Arc<Server>) -> Result<Self> {
        let token = crate::api::bearer(parts).ok_or(ApiError::Unauthorized)?;
        match server.authenticate(token).await? {
            Caller::Machine { id, token_hash, .. } => Ok(Self(id, token_hash)),
            Caller::Admin => Err(ApiError::Forbidden("the WebSocket is for machine daemons".into())),
        }
    }
}

/// The registry's side of a session.
pub struct Handle {
    pub id: u64,
    /// The token the session authenticated with; a removal revokes only its own.
    pub token_hash: String,
    cancel: CancellationToken,
    revoked: Arc<AtomicBool>,
    assignment: Arc<watch::Sender<Option<AssignmentSnapshot>>>,
    queue: mpsc::Sender<ServerMessage>,
}

impl Handle {
    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    pub fn revoke(&self) {
        self.revoked.store(true, Ordering::SeqCst);
        self.cancel.cancel();
    }

    /// Queues `msg`; a daemon whose queue is full is disconnected.
    pub fn send(&self, machine: &str, msg: ServerMessage) {
        if let Err(mpsc::error::TrySendError::Full(_)) = self.queue.try_send(msg) {
            tracing::warn!("{machine}: not reading its messages; disconnecting");
            self.cancel.cancel();
        }
    }

    /// Replaces the pending assignment unless it is already as new.
    pub fn publish(&self, snapshot: AssignmentSnapshot) {
        publish(&self.assignment, snapshot);
    }
}

fn publish(tx: &watch::Sender<Option<AssignmentSnapshot>>, snapshot: AssignmentSnapshot) {
    tx.send_if_modified(|cur| {
        if cur.as_ref().is_some_and(|c| c.revision >= snapshot.revision) {
            return false;
        }
        *cur = Some(snapshot);
        true
    });
}

pub async fn upgrade(
    State(server): State<Arc<Server>>,
    WsCaller(machine, token_hash): WsCaller,
    ws: WebSocketUpgrade,
) -> Response {
    ws.on_upgrade(move |socket| session(server, machine, token_hash, socket))
}

async fn session(server: Arc<Server>, machine: String, token_hash: String, socket: WebSocket) {
    let (sink, stream) = socket.split();
    let cancel = CancellationToken::new();
    let revoked = Arc::new(AtomicBool::new(false));
    let (atx, arx) = watch::channel(None);
    let atx = Arc::new(atx);
    let (qtx, qrx) = mpsc::channel(QUEUE);
    let id = server.next_conn.fetch_add(1, Ordering::Relaxed);
    let handle = Handle {
        id,
        token_hash: token_hash.clone(),
        cancel: cancel.clone(),
        revoked: revoked.clone(),
        assignment: atx.clone(),
        queue: qtx.clone(),
    };

    let retry = Arc::new(AtomicBool::new(false));
    let ends = Ending { revoked, retry: retry.clone() };
    let writer = tokio::spawn(write_loop(sink, qrx, arx, cancel.clone(), ends, server.config.clone()));
    match register(&server, &machine, handle).await {
        Ok(Some(snapshot)) => {
            tracing::info!("{machine} connected (session {id})");
            publish(&atx, snapshot);
        }
        Ok(None) => {
            tracing::info!("{machine}: session {id} authenticated with a removed token; closing");
            cancel.cancel();
        }
        Err(e) => {
            tracing::error!("{machine}: registering session {id}: {e}");
            cancel.cancel();
        }
    }
    if let Some(baseline) = server.head() {
        let _ = qtx.try_send(ServerMessage::Nudge { baseline });
    }
    drop((atx, qtx));

    read_loop(&server, &machine, id, &token_hash, stream, &cancel, &retry).await;
    cancel.cancel();
    let _ = writer.await;
    server.disconnect(&machine, id);
    tracing::info!("{machine} disconnected (session {id})");
}

/// Registers `handle` as `machine`'s session if its token is still the
/// machine's, and returns the assignment to send first; `None` (and nothing
/// registered) if the token was removed. The check and the registration share
/// the DB lock with a removal, so an upgrade delayed past its machine's
/// removal can't replace the session of a machine that joined again. The
/// assignment is read after registering, so no later change can be missed.
async fn register(server: &Arc<Server>, machine: &str, handle: Handle) -> Result<Option<AssignmentSnapshot>> {
    let (h, m) = (server.clone(), machine.to_string());
    server
        .db(move |conn| {
            if db::machine_for_token(conn, &handle.token_hash)?.map(|(id, _)| id).as_deref() != Some(m.as_str()) {
                return Ok(None);
            }
            h.connect(&m, handle);
            db::snapshot(conn, &m)
        })
        .await
}

async fn read_loop(
    server: &Arc<Server>,
    machine: &str,
    id: u64,
    token_hash: &str,
    mut stream: SplitStream<WebSocket>,
    cancel: &CancellationToken,
    retry: &AtomicBool,
) {
    let idle = server.config.idle_limit();
    let touch_every = std::time::Duration::from_secs(10).min(server.config.offline_after / 4);
    let mut last_touch: Option<tokio::time::Instant> = None;
    loop {
        let next = tokio::select! {
            biased;
            () = cancel.cancelled() => return,
            next = tokio::time::timeout(idle, stream.next()) => next,
        };
        let Ok(msg) = next else {
            return tracing::info!("{machine}: nothing received for {idle:?}; closing");
        };
        let Some(Ok(msg)) = msg else { return };
        if last_touch.is_none_or(|t| t.elapsed() >= touch_every) {
            let (m, t, now) = (machine.to_string(), token_hash.to_string(), now());
            let _ = server.db(move |conn| db::touch(conn, &m, &t, now)).await;
            last_touch = Some(tokio::time::Instant::now());
        }
        match msg {
            Message::Text(text) => {
                match serde_json::from_str::<Value>(&text).map_err(|e| e.to_string()).and_then(parse) {
                    Ok(DaemonMessage::Hello { machine_id }) if machine_id != machine => {
                        return tracing::warn!("{machine}: hello claims to be `{machine_id}`; closing");
                    }
                    Ok(DaemonMessage::Hello { .. }) => {}
                    Ok(DaemonMessage::Status(report)) => {
                        // A later discard must not sweep against an older report,
                        // so drop the session; reconnecting sends a fresh one first.
                        if let Err(e) = on_status(server, machine, id, token_hash, report, cancel).await {
                            return tracing::error!("{machine}: storing status: {e}; closing");
                        }
                    }
                    Ok(DaemonMessage::DraftDiscarded { id: discard_id, items }) => {
                        match on_draft_discarded(server, machine, id, token_hash, items, cancel).await {
                            Ok(Discard::Applied) => {
                                if let Some(discard_id) = discard_id
                                    && let Some(h) = crate::lock(&server.conns)
                                        .get(machine)
                                        .filter(|h| h.id == id && !cancel.is_cancelled())
                                {
                                    h.send(machine, ServerMessage::DraftDiscardApplied { id: discard_id });
                                }
                            }
                            Ok(Discard::Stale) => {}
                            Ok(Discard::Unknown) => {
                                retry.store(true, Ordering::SeqCst);
                                return tracing::warn!(
                                    "{machine}: keeping toggles for discarded drafts: the baseline's items are \
                                     unknown; closing so the daemon sends them again later"
                                );
                            }
                            // The daemon only replays unacknowledged discards on reconnect.
                            Err(e) => return tracing::error!("{machine}: discarding draft toggles: {e}; closing"),
                        }
                    }
                    Err(e) => tracing::warn!("{machine}: bad message: {e}"),
                }
            }
            Message::Close(_) => return,
            _ => {}
        }
    }
}

/// Why a session is ending, when the daemon must be told.
#[derive(Default)]
struct Ending {
    /// The machine was removed: send `Revoked` last.
    revoked: Arc<AtomicBool>,
    /// The daemon should come back later: close with code 1013.
    retry: Arc<AtomicBool>,
}

/// Sends queued messages, the latest assignment and pings until cancelled.
/// Every frame has a deadline; a stuck send ends with the session.
async fn write_loop<S: Sink<Message> + Unpin>(
    mut sink: S,
    mut queue: mpsc::Receiver<ServerMessage>,
    mut assignment: watch::Receiver<Option<AssignmentSnapshot>>,
    cancel: CancellationToken,
    ends: Ending,
    config: crate::Config,
) {
    let Ending { revoked, retry } = ends;
    let deadline = config.send_timeout;
    let mut ping = tokio::time::interval(config.ping_every);
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    ping.tick().await;
    'frames: loop {
        let frame = tokio::select! {
            biased;
            () = cancel.cancelled() => break,
            changed = assignment.changed() => {
                if changed.is_err() {
                    break;
                }
                let Some(snapshot) = assignment.borrow_and_update().clone() else { continue };
                text(&ServerMessage::Assignment(snapshot))
            }
            msg = queue.recv() => match msg {
                Some(msg) => text(&msg),
                None => break,
            },
            _ = ping.tick() => Message::Ping(Default::default()),
        };
        tokio::select! {
            biased;
            // A removal still needs its final Revoked frame, and a retry
            // its close code. Other cancellations drop a stalled socket
            // immediately.
            () = cancel.cancelled() => {
                if revoked.load(Ordering::SeqCst) || retry.load(Ordering::SeqCst) {
                    break 'frames;
                }
                return;
            },
            sent = tokio::time::timeout(deadline, sink.send(frame)) => match sent {
                Ok(Ok(())) => {}
                Ok(Err(_)) => return cancel.cancel(),
                Err(_) => {
                    tracing::warn!("a daemon didn't take a frame within {deadline:?}; disconnecting");
                    return cancel.cancel();
                }
            },
        }
    }
    if revoked.load(Ordering::SeqCst) {
        let _ = tokio::time::timeout(deadline, sink.send(text(&ServerMessage::Revoked))).await;
    }
    let close = retry
        .load(Ordering::SeqCst)
        .then(|| CloseFrame { code: close_code::AGAIN, reason: "the baseline can't be read; try again later".into() });
    let _ = tokio::time::timeout(deadline, sink.send(Message::Close(close))).await;
}

fn text(msg: &ServerMessage) -> Message {
    Message::Text(serde_json::to_string(msg).expect("serializing plain data").into())
}

/// Parses a daemon message. In a status report, list entries that don't
/// parse (an item ref from a newer daemon, say) are skipped and logged
/// rather than dropping the whole report.
fn parse(mut v: Value) -> std::result::Result<DaemonMessage, String> {
    if v.get("type").and_then(Value::as_str) == Some("status") {
        for field in ["drafts", "unpushed", "clashes", "conflicts"] {
            let Some(Value::Array(entries)) = v.get_mut(field) else {
                continue;
            };
            entries.retain(|e| {
                let ok = if field == "conflicts" {
                    serde_json::from_value::<sk1ll_proto::ConflictRef>(e.clone()).is_ok()
                } else {
                    serde_json::from_value::<ItemRef>(e.clone()).is_ok()
                };
                if !ok {
                    tracing::warn!("status: skipping malformed `{field}` entry {e}");
                }
                ok
            });
        }
    }
    serde_json::from_value(v).map_err(|e| e.to_string())
}

/// Stores the report if the session is still current and its token still
/// valid; adds harnesses the machine has no assignment for (all off).
async fn on_status(
    server: &Arc<Server>,
    machine: &str,
    id: u64,
    token_hash: &str,
    report: MachineReport,
    cancel: &CancellationToken,
) -> Result<()> {
    if cancel.is_cancelled() {
        return Ok(());
    }
    let (h, m, t, now) = (server.clone(), machine.to_string(), token_hash.to_string(), now());
    let updated = server
        .db(move |conn| {
            // Checked under the DB lock, so a removal can't slip in between.
            if !h.is_current(&m, id)
                || db::machine_for_token(conn, &t)?.map(|(id, _)| id).as_deref() != Some(m.as_str())
            {
                tracing::info!("{m}: ignoring status from stale session {id}");
                return Ok(None);
            }
            let tx = conn.transaction()?;
            db::save_report(&tx, &m, &report, now)?;
            let Some(a) = db::assignment(&tx, &m)? else {
                return Ok(None);
            };
            let added = assign::new_harness_changes(&a, &report.harnesses);
            let mut latest = None;
            if !added.is_empty() {
                latest = Some(assign::apply(&tx, &m, assign::SERVER_AUTHOR, added, now, None)?.snapshot());
            }
            tx.commit()?;
            Ok(latest)
        })
        .await?;
    if let Some(snapshot) = updated {
        server.publish_assignment(machine, snapshot);
    }
    Ok(())
}

/// What became of a draft discard.
#[derive(Debug, PartialEq, Eq)]
enum Discard {
    /// Committed (maybe as a no-op): acknowledge it.
    Applied,
    /// The session is no longer current, or its token valid: ignore it.
    Stale,
    /// The baseline's items couldn't be read, so nothing was dropped.
    Unknown,
}

/// An explicit Restore, scoped to this machine and the named items. A status
/// snapshot alone never proves that an absent draft item was discarded.
async fn on_draft_discarded(
    server: &Arc<Server>,
    machine: &str,
    id: u64,
    token_hash: &str,
    discarded: Vec<ItemRef>,
    cancel: &CancellationToken,
) -> Result<Discard> {
    if cancel.is_cancelled() {
        return Ok(Discard::Stale);
    }
    // Serialize with pushes so a Restore can't sweep against an old baseline,
    // and read HEAD itself: the cached one lags a push whose read of it failed.
    let _push = server.push_lock.lock().await;
    let Some(items) = server.live_items().await else { return Ok(Discard::Unknown) };
    let (h, m, t, now) = (server.clone(), machine.to_string(), token_hash.to_string(), now());
    let updated = server
        .db(move |conn| {
            if !h.is_current(&m, id)
                || db::machine_for_token(conn, &t)?.map(|(id, _)| id).as_deref() != Some(m.as_str())
            {
                return Ok(None);
            }
            let tx = conn.transaction()?;
            let updated = assign::sweep(&tx, &m, &items, &discarded, now)?.map(|a| a.snapshot());
            tx.commit()?;
            Ok(Some(updated))
        })
        .await?;
    let Some(updated) = updated else { return Ok(Discard::Stale) };
    if let Some(snapshot) = updated {
        server.publish_assignment(machine, snapshot);
    }
    Ok(Discard::Applied)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn malformed_report_entries_are_skipped() {
        let v = serde_json::json!({
            "type": "status",
            "drafts": ["skill:pdf", "bogus", "manifest", 3],
            "conflicts": [{"item": "rule:git", "harness": "codex"}, {"item": "nope"}],
            "harnesses": ["claude"],
        });
        let DaemonMessage::Status(r) = parse(v).unwrap() else { panic!("expected status") };
        assert_eq!(r.drafts, [ItemRef::Skill("pdf".into()), ItemRef::Manifest]);
        assert_eq!(r.conflicts.len(), 1);
        assert_eq!(r.harnesses, ["claude"]);
    }

    #[test]
    fn malformed_unpushed_entries_are_skipped() {
        let v = serde_json::json!({
            "type": "status",
            "baseline": "abc",
            "unpushed": ["skill:wip", "bogus", "rule:git", 3, "manifest", "other:notes.txt"],
            "harnesses": ["claude"],
        });
        let DaemonMessage::Status(r) = parse(v).unwrap() else { panic!("expected status") };
        assert_eq!(
            r.unpushed,
            [
                ItemRef::Skill("wip".into()),
                ItemRef::Rule("git".into()),
                ItemRef::Manifest,
                ItemRef::Other("notes.txt".into()),
            ]
        );
        assert_eq!(r.baseline.as_deref(), Some("abc"));
        assert_eq!(r.harnesses, ["claude"]);
    }

    #[tokio::test]
    async fn assignments_coalesce_to_the_newest_revision() {
        let (tx, mut rx) = watch::channel(None);
        for revision in [3, 1, 5, 4] {
            publish(&tx, AssignmentSnapshot { revision, ..Default::default() });
        }
        assert!(rx.has_changed().unwrap());
        assert_eq!(rx.borrow_and_update().as_ref().map(|s| s.revision), Some(5));
        publish(&tx, AssignmentSnapshot { revision: 2, ..Default::default() });
        assert!(!rx.has_changed().unwrap(), "an older revision never replaces a newer one");
    }

    /// Cancel after a send has started, rather than between messages.
    struct RevokeDuringSend {
        cancel: CancellationToken,
        revoked: Arc<AtomicBool>,
        frames: Arc<std::sync::Mutex<Vec<Message>>>,
        interrupted: bool,
    }

    impl Sink<Message> for RevokeDuringSend {
        type Error = std::convert::Infallible;

        fn poll_ready(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::result::Result<(), Self::Error>> {
            if !self.interrupted {
                self.interrupted = true;
                self.revoked.store(true, Ordering::SeqCst);
                self.cancel.cancel();
                cx.waker().wake_by_ref();
                return std::task::Poll::Pending;
            }
            std::task::Poll::Ready(Ok(()))
        }

        fn start_send(self: std::pin::Pin<&mut Self>, frame: Message) -> std::result::Result<(), Self::Error> {
            self.frames.lock().unwrap().push(frame);
            Ok(())
        }

        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::result::Result<(), Self::Error>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn poll_close(
            self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::result::Result<(), Self::Error>> {
            self.poll_flush(cx)
        }
    }

    #[tokio::test]
    async fn revocation_during_a_send_still_delivers_the_revoked_message() {
        let cancel = CancellationToken::new();
        let revoked = Arc::new(AtomicBool::new(false));
        let frames = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = RevokeDuringSend {
            cancel: cancel.clone(),
            revoked: revoked.clone(),
            frames: frames.clone(),
            interrupted: false,
        };
        let (queue_tx, queue_rx) = mpsc::channel(1);
        let (_assignment_tx, assignment_rx) = watch::channel(None);
        queue_tx.send(ServerMessage::Nudge { baseline: "abc".into() }).await.unwrap();
        let ends = Ending { revoked, ..Ending::default() };
        write_loop(sink, queue_rx, assignment_rx, cancel, ends, crate::Config::new("/unused")).await;
        let frames = frames.lock().unwrap();
        assert!(
            frames.iter().any(|frame| matches!(frame, Message::Text(text)
            if serde_json::from_str::<ServerMessage>(text).ok() == Some(ServerMessage::Revoked))),
            "{frames:?}"
        );
        assert!(matches!(frames.last(), Some(Message::Close(_))), "{frames:?}");
    }

    /// A session handle for tests, and its cancellation token.
    fn handle(id: u64, token_hash: String) -> (Handle, CancellationToken) {
        let cancel = CancellationToken::new();
        let (queue, _) = mpsc::channel(QUEUE);
        let handle = Handle {
            id,
            token_hash,
            cancel: cancel.clone(),
            revoked: Arc::default(),
            assignment: Arc::new(watch::channel(None).0),
            queue,
        };
        (handle, cancel)
    }

    /// Review finding: an upgrade authenticated before its machine was
    /// removed, and delayed until the machine joined again, must not replace
    /// the new session; nor may revoking the removed token end it.
    #[tokio::test]
    async fn a_delayed_upgrade_cannot_replace_a_rejoined_machines_session() {
        let dir = tempfile::tempdir().unwrap();
        let server = Server::open(crate::Config::new(dir.path().join("data"))).await.unwrap();
        let join = |token: &'static str| {
            let server = server.clone();
            async move {
                let a = sk1ll_proto::Assignment::default();
                let scope = sk1ll_proto::Scope::Fleet;
                server.db(move |conn| db::insert_machine(conn, "m", &crate::hash(token), 0, &a, scope)).await.unwrap();
            }
        };
        join("old").await;
        let (stale, stale_cancel) = handle(1, crate::hash("old"));
        server.remove_machine(Caller::Admin, "m".into()).await.unwrap();
        join("new").await;
        let (fresh, fresh_cancel) = handle(2, crate::hash("new"));
        assert!(register(&server, "m", fresh).await.unwrap().is_some());

        assert!(register(&server, "m", stale).await.unwrap().is_none());
        assert!(!stale_cancel.is_cancelled(), "never registered, so the caller closes it");
        assert!(!fresh_cancel.is_cancelled() && server.is_current("m", 2));
        server.revoke("m", &crate::hash("old"));
        assert!(!fresh_cancel.is_cancelled() && server.is_current("m", 2));

        // Removing the machine now revokes the session it registered.
        server.remove_machine(Caller::Admin, "m".into()).await.unwrap();
        assert!(fresh_cancel.is_cancelled() && !server.is_current("m", 2));
        let (late, _) = handle(3, crate::hash("new"));
        assert!(register(&server, "m", late).await.unwrap().is_none());
        assert!(!server.is_connected("m"));
    }

    /// Review finding: after a push whose read of the new HEAD failed, a
    /// discard must not sweep against the old cached HEAD (which lacks the
    /// item the push added). While HEAD can't be read it drops nothing and
    /// closes with 1013 instead of acknowledging; once HEAD reads again, the
    /// resent discard is checked against the live baseline.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_discard_after_a_failed_head_read_uses_the_live_baseline() {
        use tokio_tungstenite::tungstenite::{self, client::IntoClientRequest};

        let dir = tempfile::tempdir().unwrap();
        let server = Server::open(crate::Config::new(dir.path().join("data"))).await.unwrap();
        let mut a = sk1ll_proto::Assignment::default();
        a.set_skill("claude", "pdf", true);
        let scope = sk1ll_proto::Scope::Fleet;
        server.db(move |conn| db::insert_machine(conn, "m", &crate::hash("t"), 0, &a, scope)).await.unwrap();
        let has_pdf = || {
            let server = server.clone();
            async move {
                server
                    .db(|conn| Ok(db::assignment(conn, "m")?.unwrap().harness("claude").skills.contains_key("pdf")))
                    .await
                    .unwrap()
            }
        };

        // A push adds pdf, and reading HEAD after it fails.
        let git = |dir: &std::path::Path, args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
                .args(args)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        };
        let work = dir.path().join("work");
        git(dir.path(), &["clone", "--quiet", &server.config.repo_dir().to_string_lossy(), &work.to_string_lossy()]);
        std::fs::create_dir_all(work.join("personal/pdf")).unwrap();
        std::fs::write(work.join("personal/pdf/SKILL.md"), "x\n").unwrap();
        git(&work, &["add", "--all"]);
        git(&work, &["commit", "--quiet", "-m", "pdf"]);
        let before = server.read_head().await.unwrap();
        git(&work, &["push", "--quiet", "origin", "HEAD:main"]);
        server.fail_head.store(true, Ordering::SeqCst);
        server.after_push(before, crate::git::Budget::default(), &Caller::Admin).await;
        assert_eq!(server.head(), None, "the cached HEAD is no longer trusted");

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let router = server.router();
        let serve = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let connect = || async move {
            let mut req = format!("ws://{address}/api/ws").into_client_request().unwrap();
            req.headers_mut().insert("authorization", "Bearer t".parse().unwrap());
            let (mut ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
            let discard = DaemonMessage::DraftDiscarded { id: Some(7), items: vec![ItemRef::Skill("pdf".into())] };
            let text = serde_json::to_string(&discard).unwrap();
            ws.send(tungstenite::Message::text(text)).await.unwrap();
            ws
        };
        let mut ws = connect().await;
        let close = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match ws.next().await {
                    Some(Ok(tungstenite::Message::Text(text))) => {
                        let msg: ServerMessage = serde_json::from_str(&text).unwrap();
                        assert!(!matches!(msg, ServerMessage::DraftDiscardApplied { .. }), "acknowledged");
                    }
                    Some(Ok(tungstenite::Message::Close(frame))) => break frame,
                    Some(Ok(_)) => {}
                    other => panic!("connection ended without a close frame: {other:?}"),
                }
            }
        })
        .await
        .expect("the session must close");
        let frame = close.expect("a close code");
        assert_eq!(u16::from(frame.code), close_code::AGAIN);
        assert!(has_pdf().await, "an unknown baseline drops nothing");

        // HEAD reads again: the resent discard sees pdf in the live baseline.
        server.fail_head.store(false, Ordering::SeqCst);
        let mut ws = connect().await;
        let applied = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(Ok(tungstenite::Message::Text(text))) = ws.next().await
                    && let Ok(ServerMessage::DraftDiscardApplied { id }) = serde_json::from_str(&text)
                {
                    break id;
                }
            }
        })
        .await
        .expect("the resent discard must be acknowledged");
        assert_eq!(applied, 7);
        assert!(has_pdf().await, "pdf is in the baseline, so its toggle stays");
        serve.abort();
    }

    #[test]
    fn idle_limit_covers_two_pings() {
        let mut c = crate::Config::new("/x");
        assert_eq!(c.idle_limit(), Duration::from_secs(120));
        c.ping_every = Duration::from_secs(90);
        assert_eq!(c.idle_limit(), Duration::from_secs(180));
    }
}
