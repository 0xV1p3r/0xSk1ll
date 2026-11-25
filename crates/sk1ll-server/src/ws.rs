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
//! Frames are handled one at a time, in the order they came, so a machine's
//! own assignment write (`set_assignment`) is applied after the status it
//! sent before it: a draft item it reports can be enabled right away. Each
//! write is answered with `assignment_result`, which also echoes the last
//! report stored from this session.
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
use sk1ll_proto::{
    AssignmentOutcome, AssignmentSnapshot, AssignmentWrite, DaemonMessage, ItemRef, MachineReport, Scope, ServerMessage,
};
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

    let writer = tokio::spawn(write_loop(sink, qrx, arx, cancel.clone(), revoked, server.config.clone()));
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

    read_loop(&server, &machine, id, &token_hash, stream, &cancel).await;
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
) {
    let idle = server.config.idle_limit();
    let touch_every = std::time::Duration::from_secs(10).min(server.config.offline_after / 4);
    let mut last_touch: Option<tokio::time::Instant> = None;
    // The `seq` of the last report stored from this session.
    let mut stored = 0;
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
                        let seq = report.seq;
                        if seq <= stored {
                            tracing::warn!("{machine}: ignoring status {seq} after status {stored}");
                            continue;
                        }
                        match on_report(server, machine, id, token_hash, report, cancel).await {
                            Ok(true) => stored = seq,
                            Ok(false) => {}
                            Err(e) => return tracing::error!("{machine}: storing status: {e}; closing"),
                        }
                    }
                    Ok(DaemonMessage::SetAssignment { req, write }) => {
                        let result = if stored == 0 && !matches!(write, AssignmentWrite::RenameHarness(_)) {
                            Some(AssignmentOutcome::Refused {
                                status: 400,
                                error: "send status before changing Assignment".into(),
                            })
                        } else {
                            on_set_assignment(server, machine, id, token_hash, write, cancel).await
                        };
                        if let Some(result) = result
                            && let Some(h) =
                                crate::lock(&server.conns).get(machine).filter(|h| h.id == id && !cancel.is_cancelled())
                        {
                            h.send(machine, ServerMessage::AssignmentResult { req, seq: stored, result });
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
/// Every frame has a deadline; a stuck send ends with the session. A
/// `revoked` session (its machine was removed) sends `Revoked` last.
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
    let fields: &[&str] = match v.get("type").and_then(Value::as_str) {
        Some("status") => &["drafts", "unpushed", "clashes", "conflicts", "items"],
        _ => &[],
    };
    for &field in fields {
        let Some(Value::Array(entries)) = v.get_mut(field) else { continue };
        let before = entries.len();
        entries.retain(|e| {
            let ok = match field {
                "conflicts" => serde_json::from_value::<sk1ll_proto::ConflictRef>(e.clone()).is_ok(),
                "items" => serde_json::from_value::<sk1ll_proto::ReportedItem>(e.clone())
                    .is_ok_and(|r| matches!(r.item, ItemRef::Skill(_) | ItemRef::Rule(_))),
                _ => serde_json::from_value::<ItemRef>(e.clone()).is_ok(),
            };
            if !ok {
                tracing::warn!("status: skipping malformed `{field}` entry {e}");
            }
            ok
        });
        if field == "items" && entries.len() != before {
            v["inventory_complete"] = Value::Bool(false);
            v["inventory_error"] = Value::String("malformed Item IDs in report".into());
        }
    }
    serde_json::from_value(v).map_err(|e| e.to_string())
}

/// Stores the report if the session is still current and its token still
/// valid (`false` if not); adds harnesses the machine has no assignment for
/// (all off).
async fn on_report(
    server: &Arc<Server>,
    machine: &str,
    id: u64,
    token_hash: &str,
    report: MachineReport,
    cancel: &CancellationToken,
) -> Result<bool> {
    if cancel.is_cancelled() {
        return Ok(false);
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
            let Some(record) = db::machine(&tx, &m)? else { return Ok(None) };
            db::save_report(&tx, &m, &report, now)?;
            let added = assign::new_harness_changes(&record.assignment, &report.harnesses);
            let mut latest = None;
            if !added.is_empty() {
                latest = Some(assign::apply(&tx, &m, assign::SERVER_AUTHOR, added, now, &[], false)?.snapshot());
            }
            tx.commit()?;
            Ok(Some(latest))
        })
        .await?;
    let Some(updated) = updated else { return Ok(false) };
    if let Some(snapshot) = updated {
        server.publish_assignment(machine, snapshot);
    }
    Ok(true)
}

/// A write to the machine's own assignment, if the session is still current;
/// `None` (no answer) for a stale one.
async fn on_set_assignment(
    server: &Arc<Server>,
    machine: &str,
    id: u64,
    token_hash: &str,
    write: AssignmentWrite,
    cancel: &CancellationToken,
) -> Option<AssignmentOutcome> {
    if cancel.is_cancelled() || !server.is_current(machine, id) {
        return None;
    }
    // The scope is read again from the token as the write commits.
    let caller = Caller::Machine { id: machine.into(), scope: Scope::SelfOnly, token_hash: token_hash.into() };
    Some(match crate::api::write_assignment(server, caller, machine.into(), write, Some(id)).await {
        Ok(update) => AssignmentOutcome::Applied(update),
        Err(e) => AssignmentOutcome::Refused { status: e.status().as_u16(), error: e.message() },
    })
}
