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
    AssignmentOutcome, AssignmentSnapshot, AssignmentWrite, DaemonMessage, InventoryReport, ItemRef, MachineReport,
    Scope, ServerMessage,
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
                        match on_report(server, machine, id, token_hash, Report::Status(report), cancel).await {
                            Ok(true) => stored = seq,
                            Ok(false) => {}
                            Err(e) => return tracing::error!("{machine}: storing status: {e}; closing"),
                        }
                    }
                    Ok(DaemonMessage::Inventory(report)) => {
                        let seq = report.seq;
                        if seq <= stored {
                            continue;
                        }
                        match on_report(server, machine, id, token_hash, Report::Inventory(report), cancel).await {
                            Ok(true) => stored = seq,
                            Ok(false) => {}
                            Err(e) => return tracing::error!("{machine}: storing inventory: {e}; closing"),
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
        Some("inventory") => &["items"],
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

enum Report {
    Status(MachineReport),
    Inventory(InventoryReport),
}

/// Stores the report if the session is still current and its token still
/// valid (`false` if not); adds harnesses the machine has no assignment for
/// (all off).
async fn on_report(
    server: &Arc<Server>,
    machine: &str,
    id: u64,
    token_hash: &str,
    report: Report,
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
            let (report, harnesses) = match report {
                Report::Status(report) => {
                    let harnesses = report.harnesses.clone();
                    (report, harnesses)
                }
                Report::Inventory(update) => {
                    let mut report = record.report.unwrap_or_default();
                    report.items = update.items;
                    report.inventory_complete = update.inventory_complete;
                    report.inventory_error = update.inventory_error;
                    (report, update.harnesses)
                }
            };
            db::save_report(&tx, &m, &report, now)?;
            let added = assign::new_harness_changes(&record.assignment, &harnesses);
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
        write_loop(sink, queue_rx, assignment_rx, cancel, revoked, crate::Config::new("/unused")).await;
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

    /// A machine's own write is applied after the status sent before it,
    /// on the same session, and answered with that status's `seq`.
    #[tokio::test(flavor = "multi_thread")]
    async fn session_writes_follow_the_status_before_them() {
        use tokio_tungstenite::tungstenite::{self, client::IntoClientRequest};

        let dir = tempfile::tempdir().unwrap();
        let server = Server::open(crate::Config::new(dir.path().join("data"))).await.unwrap();
        let (a, scope) = (sk1ll_proto::Assignment::default(), Scope::SelfOnly);
        server.db(move |conn| db::insert_machine(conn, "m", &crate::hash("t"), 0, &a, scope)).await.unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let router = server.router();
        let serve = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let mut req = format!("ws://{address}/api/ws").into_client_request().unwrap();
        req.headers_mut().insert("authorization", "Bearer t".parse().unwrap());
        let (mut ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
        let send = |msg: DaemonMessage| tungstenite::Message::text(serde_json::to_string(&msg).unwrap());

        let id: sk1ll_proto::ItemId = "drafted".parse().unwrap();
        let item = sk1ll_proto::ReportedItem { item: ItemRef::Skill("new".into()), id: id.clone() };
        let report = MachineReport {
            seq: 1,
            items: vec![item],
            inventory_complete: true,
            harnesses: vec!["claude".into()],
            ..Default::default()
        };
        ws.send(send(DaemonMessage::Status(report))).await.unwrap();
        // Turning the default off right after materializes the reported item.
        let change = |kind, enabled| sk1ll_proto::ToggleChange {
            harness: "claude".into(),
            kind,
            item_id: None,
            name: None,
            enabled: Some(enabled),
        };
        let write = AssignmentWrite::Changes(sk1ll_proto::AssignmentChanges {
            changes: vec![change(sk1ll_proto::ToggleKind::NewSkillsDefault, true)],
            expected: None,
        });
        ws.send(send(DaemonMessage::SetAssignment { req: 9, write })).await.unwrap();
        let result = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(Ok(tungstenite::Message::Text(text))) = ws.next().await
                    && let Ok(ServerMessage::AssignmentResult { req, seq, result }) = serde_json::from_str(&text)
                {
                    break (req, seq, result);
                }
            }
        })
        .await
        .expect("the write must be answered");
        let (req, seq, AssignmentOutcome::Applied(update)) = result else { panic!("{result:?}") };
        assert_eq!((req, seq), (9, 1));
        let h = update.assignment.harness("claude");
        assert!(h.new_skills_default);
        assert_eq!(h.skills.get(&id), Some(&false), "the reported draft item kept its value");
        let materialized = update.entries.iter().find(|e| e.cause == sk1ll_proto::LogCause::DefaultMaterialized);
        assert_eq!(materialized.and_then(|e| e.name.as_deref()), Some("new"));
        serve.abort();
    }

    #[tokio::test]
    async fn replayed_rename_and_replaced_session_cannot_overwrite_assignment() {
        use sk1ll_proto::{AssignmentChanges, HarnessRename, ToggleChange, ToggleKind};
        use tokio_tungstenite::tungstenite::{self, client::IntoClientRequest};

        let dir = tempfile::tempdir().unwrap();
        let server = Server::open(crate::Config::new(dir.path().join("data"))).await.unwrap();
        let skill: sk1ll_proto::ItemId = "skill-id".parse().unwrap();
        let mut initial = sk1ll_proto::Assignment::default();
        initial.set_skill("old", &skill, true);
        initial.set_harness("existing", false);
        server
            .db(move |conn| db::insert_machine(conn, "m", &crate::hash("t"), 0, &initial, Scope::SelfOnly))
            .await
            .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let router = server.router();
        let serve = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let connect = || async {
            let mut req = format!("ws://{address}/api/ws").into_client_request().unwrap();
            req.headers_mut().insert("authorization", "Bearer t".parse().unwrap());
            let (mut ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
            assert!(ws.next().await.is_some());
            ws
        };
        let send = |msg: DaemonMessage| tungstenite::Message::text(serde_json::to_string(&msg).unwrap());
        let mut first = connect().await;
        let denied =
            HarnessRename { from: "old".into(), to: "existing".into(), operation_id: "deny".into(), replace: false };
        first
            .send(send(DaemonMessage::SetAssignment { req: 9, write: AssignmentWrite::RenameHarness(denied) }))
            .await
            .unwrap();
        loop {
            let tungstenite::Message::Text(text) = first.next().await.unwrap().unwrap() else { continue };
            if let Ok(ServerMessage::AssignmentResult { req: 9, result, .. }) = serde_json::from_str(&text) {
                assert!(matches!(result, AssignmentOutcome::Refused { status: 409, .. }));
                break;
            }
        }
        let rename =
            HarnessRename { from: "old".into(), to: "existing".into(), operation_id: "move".into(), replace: true };
        first
            .send(send(DaemonMessage::SetAssignment { req: 10, write: AssignmentWrite::RenameHarness(rename.clone()) }))
            .await
            .unwrap();
        // Treat the response as lost after the server commits.
        tokio::time::timeout(Duration::from_secs(5), async {
            while server
                .db(|conn| db::harness_rename(conn, &db::machine(conn, "m")?.unwrap().registration, "move"))
                .await
                .unwrap()
                .is_none()
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        let (generation, cancel) = {
            let conns = crate::lock(&server.conns);
            (conns["m"].id, conns["m"].cancel.clone())
        };
        let push = server.push_lock.lock().await;
        let token_hash = crate::hash("t");
        let mut stale = Box::pin(on_set_assignment(
            &server,
            "m",
            generation,
            &token_hash,
            AssignmentWrite::Changes(AssignmentChanges {
                expected: None,
                changes: vec![ToggleChange {
                    harness: "existing".into(),
                    kind: ToggleKind::NewRulesDefault,
                    name: None,
                    item_id: None,
                    enabled: Some(false),
                }],
            }),
            &cancel,
        ));
        // The first session passed the early generation check and is waiting to materialize defaults.
        assert!(futures_util::poll!(&mut stale).is_pending());
        let mut next = connect().await;
        drop(push);
        assert!(matches!(stale.await, Some(AssignmentOutcome::Refused { status: 409, .. })));

        // Even if a later report had recreated the source with all-off defaults,
        // replay of the completed operation leaves the populated destination alone.
        let status =
            MachineReport { seq: 1, inventory_complete: true, harnesses: vec!["old".into()], ..Default::default() };
        next.send(send(DaemonMessage::Status(status))).await.unwrap();
        next.send(send(DaemonMessage::SetAssignment { req: 11, write: AssignmentWrite::RenameHarness(rename) }))
            .await
            .unwrap();
        loop {
            let tungstenite::Message::Text(text) = next.next().await.unwrap().unwrap() else { continue };
            if let Ok(ServerMessage::AssignmentResult { req: 11, seq, result }) = serde_json::from_str(&text) {
                let AssignmentOutcome::Applied(update) = result else { panic!("{result:?}") };
                assert_eq!(seq, 1);
                assert!(update.entries.is_empty());
                assert!(update.assignment.skill_enabled("existing", &skill));
                assert!(update.assignment.harness("existing").new_rules_default);
                assert!(!update.assignment.harness("old").new_rules_default);
                break;
            }
        }
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
