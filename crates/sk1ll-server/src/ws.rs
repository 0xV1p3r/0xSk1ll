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
//! no-ops. Invalid or stale sessions receive no acknowledgement.
//!
//! Outbound state is bounded: the assignment is coalesced in a `watch` (only
//! the newest revision is kept), and other messages go through a small queue.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{FromRequestParts, State};
use axum::http::request::Parts;
use axum::response::Response;
use futures_util::stream::SplitStream;
use futures_util::{Sink, SinkExt, StreamExt};
use serde_json::Value;
use sk1ll_proto::{AssignmentSnapshot, DaemonMessage, ServerMessage, ItemRef, MachineReport};
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

use crate::error::{ApiError, Result};
use crate::{Caller, Server, assign, db, hash, now};

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
            Caller::Machine(id) => Ok(Self(id, hash(token))),
            Caller::Admin => Err(ApiError::Forbidden("the WebSocket is for machine daemons".into())),
        }
    }
}

/// The registry's side of a session.
pub struct Handle {
    pub id: u64,
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
    let handle =
        Handle { id, cancel: cancel.clone(), revoked: revoked.clone(), assignment: atx.clone(), queue: qtx.clone() };
    server.connect(&machine, handle);
    tracing::info!("{machine} connected (session {id})");

    let writer = tokio::spawn(write_loop(sink, qrx, arx, cancel.clone(), revoked, server.config.clone()));
    // Registered first, so no change committed after this read can be missed.
    let (m, t) = (machine.clone(), token_hash.clone());
    match server
        .db(move |conn| {
            if db::machine_for_token(conn, &t)?.as_deref() != Some(m.as_str()) {
                return Ok(None);
            }
            db::snapshot(conn, &m)
        })
        .await
    {
        Ok(Some(snapshot)) => publish(&atx, snapshot),
        Ok(None) => cancel.cancel(),
        Err(e) => tracing::error!("{machine}: loading assignment: {e}"),
    }
    if let Some(baseline) = server.head() {
        let _ = qtx.try_send(ServerMessage::Nudge { baseline });
    }
    drop((atx, qtx));

    read_loop(&server, &machine, id, &token_hash, stream, &cancel).await;
    cancel.cancel();
    let _ = writer.await;
    server.disconnect(&machine, id);
    tracing::info!("{machine} disconnected (session {id})");
}

async fn read_loop(
    server: &Arc<Server>,
    machine: &str,
    id: u64,
    token_hash: &str,
    mut stream: SplitStream<WebSocket>,
    cancel: &CancellationToken,
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
                        if let Err(e) = on_status(server, machine, id, token_hash, report, cancel).await {
                            tracing::error!("{machine}: storing status: {e}");
                        }
                    }
                    Ok(DaemonMessage::DraftDiscarded { id: discard_id, items }) => {
                        match on_draft_discarded(server, machine, id, token_hash, items, cancel).await {
                            Ok(true) => {
                                if let Some(discard_id) = discard_id
                                    && let Some(h) = server.conns.lock().unwrap().get(machine).filter(|h| h.id == id && !cancel.is_cancelled()) { h.send(machine, ServerMessage::DraftDiscardApplied { id: discard_id }); }
                            }
                            Ok(false) => {}
                            Err(e) => tracing::error!("{machine}: discarding draft toggles: {e}"),
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

/// Sends queued messages, the latest assignment and pings until cancelled.
/// Every frame has a deadline; a stuck send ends with the session.
async fn write_loop<S: Sink<Message> + Unpin>(
    mut sink: S,
    mut queue: mpsc::Receiver<ServerMessage>,
    mut assignment: watch::Receiver<Option<AssignmentSnapshot>>,
    cancel: CancellationToken,
    revoked: Arc<AtomicBool>,
    config: crate::Config,
) {
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
            // A removal still needs its final Revoked frame. Other
            // cancellations drop a stalled socket immediately.
            () = cancel.cancelled() => {
                if revoked.load(Ordering::SeqCst) {
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
    let _ = tokio::time::timeout(deadline, sink.send(Message::Close(None))).await;
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
            if !h.is_current(&m, id) || db::machine_for_token(conn, &t)?.as_deref() != Some(m.as_str()) {
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

/// An explicit Restore, scoped to this machine and the named items. A status
/// snapshot alone never proves that an absent draft item was discarded.
async fn on_draft_discarded(
    server: &Arc<Server>,
    machine: &str,
    id: u64,
    token_hash: &str,
    discarded: Vec<ItemRef>,
    cancel: &CancellationToken,
) -> Result<bool> {
    if cancel.is_cancelled() {
        return Ok(false);
    }
    // Serialize with pushes so a Restore can't sweep against an old baseline.
    let _push = server.push_lock.lock().await;
    let items = server.baseline_items();
    let (h, m, t, now) = (server.clone(), machine.to_string(), token_hash.to_string(), now());
    let updated = server
        .db(move |conn| {
            if !h.is_current(&m, id) || db::machine_for_token(conn, &t)?.as_deref() != Some(m.as_str()) {
                return Ok(None);
            }
            let tx = conn.transaction()?;
            let updated = assign::sweep(&tx, &m, &items, Some(&discarded), now)?.map(|a| a.snapshot());
            tx.commit()?;
            Ok(Some(updated))
        })
        .await?;
    let Some(updated) = updated else { return Ok(false) };
    if let Some(snapshot) = updated {
        server.publish_assignment(machine, snapshot);
    }
    Ok(true)
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
            mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>,
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
            self: std::pin::Pin<&mut Self>, _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::result::Result<(), Self::Error>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn poll_close(
            self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>,
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
            cancel: cancel.clone(), revoked: revoked.clone(), frames: frames.clone(), interrupted: false,
        };
        let (queue_tx, queue_rx) = mpsc::channel(1);
        let (_assignment_tx, assignment_rx) = watch::channel(None);
        queue_tx.send(ServerMessage::Nudge { baseline: "abc".into() }).await.unwrap();
        write_loop(sink, queue_rx, assignment_rx, cancel, revoked, crate::Config::new("/unused")).await;
        let frames = frames.lock().unwrap();
        assert!(frames.iter().any(|frame| matches!(frame, Message::Text(text)
            if serde_json::from_str::<ServerMessage>(text).ok() == Some(ServerMessage::Revoked))), "{frames:?}");
        assert!(matches!(frames.last(), Some(Message::Close(_))), "{frames:?}");
    }

    #[test]
    fn idle_limit_covers_two_pings() {
        let mut c = crate::Config::new("/x");
        assert_eq!(c.idle_limit(), Duration::from_secs(120));
        c.ping_every = Duration::from_secs(90);
        assert_eq!(c.idle_limit(), Duration::from_secs(180));
    }
}
