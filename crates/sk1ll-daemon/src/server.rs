//! The daemon's side of the server protocol (`sk1ll-proto`): one WebSocket
//! session at a time, reconnected with jittered exponential backoff, plus
//! authenticated pass-through of REST calls for the local API.
//!
//! On connect: `hello`; then the local state is recomputed (with the engine
//! mutex) and sent as the first status; then the outbox of `draft_discarded`
//! is delivered, each entry removed only once acknowledged as applied; then commits queued
//! while offline are pushed and the baseline is pulled. That is one refresh
//! per connect when nothing changed: the outbox is filtered against the
//! report just computed, the pull syncs again only if it moved something, and
//! the server's initial nudge for the baseline the machine is on is only
//! noted. Afterwards a status
//! goes out whenever the report changes, always before newly queued
//! `draft_discarded`. The socket is read continuously so pings are answered;
//! server messages are handled in order by a separate worker so slow work (a
//! pull) never stalls reading.
//!
//! Servers without acknowledgement support leave entries pending; they are
//! replayed once per connection until an upgraded server confirms them. A
//! server that can't apply them yet closes with code 1013 (try again later);
//! the reconnect backoff then keeps growing instead of starting over.
//!
//! An HTTP 401 on connecting is not a revocation (a proxy may send it too):
//! the machine is marked auth-rejected, keeps being managed, and the
//! connection is retried. Only the server's `revoked` message revokes it.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::response::Response;
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use sk1ll_proto::{
    AssignmentChanges, AssignmentSnapshot, AssignmentUpdate, DaemonMessage, ErrorBody, MachineReport, PROTOCOL_HEADER,
    PROTOCOL_VERSION, ServerMessage,
};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::{self, Message, client::IntoClientRequest};

use crate::auth::redact;
use crate::client::{Connection, Event};
use crate::daemon::Daemon;
use crate::error::{Error, Result};
use crate::{Config, DaemonState};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const SEND_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;
type Sink = futures_util::stream::SplitSink<Ws, Message>;

enum ConnectError {
    /// HTTP 401 on the upgrade: the token was refused, by the server or by
    /// something in front of it.
    Unauthorized,
    Other(String),
}

/// The server closed the session with code 1013: come back later.
#[derive(Debug, thiserror::Error)]
#[error("the server asked to try again later: {0}")]
struct RetryLater(String);

pub(crate) async fn run(d: Arc<Daemon>) {
    let Ok(state) = d.ident.joined() else {
        return;
    };
    let (inbox_tx, inbox_rx) = mpsc::unbounded_channel();
    tokio::spawn(inbox(d.clone(), inbox_rx));
    let mut backoff = d.cfg.reconnect_min;
    while !d.is_revoked() {
        match connect(&d).await {
            Ok(ws) => {
                let grown = std::mem::replace(&mut backoff, d.cfg.reconnect_min);
                tracing::info!("connected to {}", state.server_url);
                if let Err(e) = session(&d, ws, &inbox_tx).await {
                    tracing::info!("server connection lost: {}", redact(&format!("{e:#}")));
                    if e.is::<RetryLater>() {
                        backoff = grown;
                    }
                }
                d.set_connection(Connection::Offline);
            }
            Err(ConnectError::Unauthorized) => d.set_auth_rejected(),
            Err(ConnectError::Other(e)) => {
                if d.view().last_error.as_deref() != Some(e.as_str()) {
                    d.set_error(e);
                }
                d.set_connection(Connection::Offline);
            }
        }
        if d.is_revoked() {
            break;
        }
        tokio::time::sleep(jitter(backoff)).await;
        backoff = (backoff * 2).min(d.cfg.reconnect_max);
    }
    tracing::info!("revoked: idle; join again with `sk1ll join` to manage this machine");
}

/// A random delay in [d/2, d].
fn jitter(d: Duration) -> Duration {
    #[cfg(test)]
    {
        for duration in [Duration::ZERO, Duration::from_nanos(1), Duration::from_secs(60)] {
            let micros =
                std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |t| t.subsec_micros());
            let value = duration / 2 + duration.mul_f64(f64::from(micros % 1000) / 2000.0);
            assert!(value >= duration / 2 && value <= duration);
        }
    }
    let micros = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |t| t.subsec_micros());
    let frac = f64::from(micros % 1000) / 1000.0;
    d / 2 + d.mul_f64(frac / 2.0)
}

async fn connect(d: &Daemon) -> std::result::Result<Ws, ConnectError> {
    let other = |e: &dyn std::fmt::Display| ConnectError::Other(redact(&format!("server unreachable (offline): {e}")));
    let state = d.ident.joined().map_err(|e| other(&e))?;
    let mut req = state.ws_url().into_client_request().map_err(|e| other(&e))?;
    let auth = HeaderValue::from_str(&format!("Bearer {}", state.token)).map_err(|_| other(&"bad token"))?;
    req.headers_mut().insert(header::AUTHORIZATION, auth);
    req.headers_mut().insert(PROTOCOL_HEADER, HeaderValue::from(PROTOCOL_VERSION));
    match tokio::time::timeout(CONNECT_TIMEOUT, tokio_tungstenite::connect_async(req)).await {
        Err(_) => Err(other(&"connection timed out")),
        Ok(Err(tungstenite::Error::Http(resp))) if resp.status() == StatusCode::UNAUTHORIZED => {
            Err(ConnectError::Unauthorized)
        }
        Ok(Err(tungstenite::Error::Http(resp))) if resp.status() == StatusCode::UPGRADE_REQUIRED => {
            let body = resp.body().as_deref().and_then(|b| serde_json::from_slice::<ErrorBody>(b).ok());
            let fallback = || sk1ll_proto::protocol_mismatch(&PROTOCOL_VERSION.to_string());
            Err(ConnectError::Other(body.map_or_else(fallback, |b| b.error)))
        }
        Ok(Err(e)) => Err(other(&e)),
        Ok(Ok((ws, _))) => Ok(ws),
    }
}

async fn send(ws: &mut Sink, msg: &DaemonMessage) -> anyhow::Result<()> {
    let text = serde_json::to_string(msg)?;
    tokio::time::timeout(SEND_TIMEOUT, ws.send(Message::text(text)))
        .await
        .map_err(|_| anyhow::anyhow!("send timed out"))??;
    Ok(())
}

/// Delivers the outbox in order after a fresh status, skipping recreated items.
///
/// `fresh`: the caller refreshed just before (on connecting), so an entry
/// already queued then needs no second refresh. An empty outbox never
/// refreshes: there is nothing to filter, and the report published last goes
/// out as it is.
async fn flush_outbox(
    d: &Arc<Daemon>,
    sink: &mut Sink,
    sent: &mut MachineReport,
    mut fresh: bool,
    pending: &mut std::collections::HashSet<u64>,
) -> anyhow::Result<()> {
    loop {
        // Capture the entry and the state used to filter it under the same
        // engine mutex, so a concurrent Restore cannot queue against an older report.
        // (A Restore refreshes before it queues; if that refresh failed, the
        // report is stale and is recomputed here whatever `fresh` says.)
        // Adoption holds the engine mutex while waiting for its published
        // draft to reach the Server. Keep sending fresh reports even if an outbox
        // refresh is waiting for that mutex.
        let mut view = d.subscribe_view();
        let already_sent = pending.clone();
        let refresh = d.blocking(move |d, eng| {
            let entry = d.outbox_front(&already_sent);
            if entry.is_some() && (!fresh || Daemon::report_is_stale(eng)) {
                d.refresh(eng)?;
            }
            Ok((d.view().report, entry))
        });
        tokio::pin!(refresh);
        let (report, entry) = loop {
            // A new receiver starts with the current report already seen.
            // Send it even if a rename is holding the engine while it waits
            // for the Server to see that draft.
            let report = view.borrow_and_update().report.clone();
            if report != *sent {
                send(sink, &DaemonMessage::Status(report.clone())).await?;
                *sent = report;
            }
            tokio::select! {
                result = &mut refresh => break result.map_err(|e| {
                    d.set_error(format!("refreshing before replaying discards: {e}"));
                    e
                })?,
                changed = view.changed() => { changed?; }
            }
        };
        if report != *sent {
            send(sink, &DaemonMessage::Status(report.clone())).await?;
            *sent = report.clone();
        }
        let Some(entry) = entry else { return Ok(()) };
        let items = entry
            .items
            .into_iter()
            .filter(|item| !report.drafts.contains(item) && !report.unpushed.contains(item))
            .collect::<Vec<_>>();
        if !items.is_empty() {
            send(sink, &DaemonMessage::DraftDiscarded { id: Some(entry.id), items }).await?;
            pending.insert(entry.id);
        } else {
            d.outbox_delivered(entry.id);
        }
        // One refresh serves every entry of this flush.
        fresh = true;
    }
}

/// No cached report or discard is safe to send after a failed refresh.
async fn refresh_report(d: &Arc<Daemon>) -> anyhow::Result<MachineReport> {
    match d
        .blocking(|d, eng| {
            d.refresh(eng)?;
            Ok(d.view().report)
        })
        .await
    {
        Ok(report) => Ok(report),
        Err(e) => {
            d.set_error(format!("refreshing before reporting to the Server: {e}"));
            Err(e.into())
        }
    }
}

async fn session(d: &Arc<Daemon>, ws: Ws, inbox: &mpsc::UnboundedSender<ServerMessage>) -> anyhow::Result<()> {
    let (mut sink, mut stream) = ws.split();
    let mut view = d.subscribe_view();
    send(&mut sink, &DaemonMessage::Hello { machine_id: d.ident.machine_id().to_string() }).await?;
    // The cached report may be stale (changes while offline): recompute it
    // before the first status and before replaying any discards.
    let mut sent = refresh_report(d).await?;
    send(&mut sink, &DaemonMessage::Status(sent.clone())).await?;
    let mut pending = std::collections::HashSet::new();
    flush_outbox(d, &mut sink, &mut sent, true, &mut pending).await?;
    d.set_connection(Connection::Connected);
    let catch_up = d.clone();
    tokio::spawn(async move {
        if let Err(e) = catch_up.blocking(|d, eng| d.on_connected(eng)).await {
            tracing::warn!("after connecting: {e}");
        }
    });

    loop {
        tokio::select! {
            msg = stream.next() => match msg {
                Some(Ok(Message::Text(text))) => match serde_json::from_str::<ServerMessage>(&text) {
                    Ok(ServerMessage::DraftDiscardApplied { id }) => {
                        if pending.remove(&id) { d.outbox_delivered(id); }
                    }
                    Ok(ServerMessage::Revoked) => {
                        d.blocking(|d, eng| d.revoke(eng, "removed from the fleet")).await?;
                        return Ok(());
                    }
                    Ok(m) => {
                        let _ = inbox.send(m);
                    }
                    Err(e) => tracing::warn!("bad server message: {e}"),
                },
                Some(Ok(Message::Close(Some(frame)))) if frame.code == CloseCode::Again => {
                    return Err(RetryLater(frame.reason.to_string()).into());
                }
                Some(Ok(Message::Close(_))) | None => return Ok(()),
                // Pings are answered by tungstenite as we keep reading.
                Some(Ok(_)) => {}
                Some(Err(e)) => return Err(e.into()),
            },
            changed = view.changed() => {
                if changed.is_err() {
                    return Ok(());
                }
                // Reports are published only at the end of a local refresh.
                // Reading them here must not wait for a pull's engine mutex.
                let report = view.borrow_and_update().report.clone();
                if report != sent {
                    send(&mut sink, &DaemonMessage::Status(report.clone())).await?;
                    sent = report;
                }
            }
            () = d.outbox_ready.notified() => {
                // The status that led to it goes first (e.g. the draft after a Restore).
                flush_outbox(d, &mut sink, &mut sent, false, &mut pending).await?;
            }
        }
    }
}

/// Handles server messages in the order they arrived.
async fn inbox(d: Arc<Daemon>, mut rx: mpsc::UnboundedReceiver<ServerMessage>) {
    while let Some(m) = rx.recv().await {
        let result = match m {
            ServerMessage::DraftDiscardApplied { .. } => Ok(()),
            ServerMessage::Assignment(snap) => d.blocking(move |d, eng| d.apply_assignment(eng, snap).map(drop)).await,
            ServerMessage::Nudge { baseline } => d.blocking(move |d, eng| d.nudge(eng, baseline)).await,
            ServerMessage::ImportsChanged { updates } => {
                d.event(Event::Imports { updates });
                Ok(())
            }
            ServerMessage::Revoked => d.blocking(|d, eng| d.revoke(eng, "removed from the fleet")).await,
        };
        if let Err(e) = result {
            d.set_error(e);
        }
    }
}

/// An HTTP client for the server: every request carries this daemon's
/// [`PROTOCOL_VERSION`], so a server that speaks another one refuses it.
pub(crate) fn http_client() -> reqwest::ClientBuilder {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(PROTOCOL_HEADER, HeaderValue::from(PROTOCOL_VERSION));
    reqwest::Client::builder().default_headers(headers)
}

/// Forwards a local API call to the server's `/api` with the machine token.
pub(crate) async fn proxy(d: &Daemon, method: Method, path_and_query: &str, body: Bytes) -> Result<Response> {
    if d.is_revoked() {
        return Err(Error::Unavailable("this machine was removed from the fleet; it can't reach the server".into()));
    }
    let resp = request(&d.http, d.ident.joined()?, method.clone(), path_and_query, body).await?;
    let status = resp.status();
    let content_type = resp.headers().get(header::CONTENT_TYPE).cloned();
    let bytes = resp.bytes().await.map_err(|e| Error::Unavailable(format!("reading the server's response: {e}")))?;
    if method != Method::GET
        && !path_and_query.split('?').next().is_some_and(|path| path.ends_with("/assignment/copy-from/preview"))
        && status.is_success()
    {
        d.event(Event::Fleet);
    }
    let mut out = Response::builder().status(status);
    if let Some(ct) = content_type {
        out = out.header(header::CONTENT_TYPE, ct);
    }
    out.body(Body::from(bytes)).map_err(|e| Error::Internal(e.into()))
}

/// The authenticated Server transport shared by the daemon and direct CLI.
async fn request(
    http: &reqwest::Client,
    state: &DaemonState,
    method: Method,
    path_and_query: &str,
    body: Bytes,
) -> Result<reqwest::Response> {
    if state.revoked {
        return Err(Error::Unavailable("this machine was removed from the fleet; it can't reach the server".into()));
    }
    let url = format!("{}/api{path_and_query}", state.server_url);
    let mut req = http.request(method, url).bearer_auth(&state.token).timeout(REQUEST_TIMEOUT);
    if !body.is_empty() {
        req = req.header(header::CONTENT_TYPE, "application/json").body(body);
    }
    req.send().await.map_err(|e| Error::Unavailable(redact(&format!("offline: the server is unreachable ({e})"))))
}

/// Changes this machine's assignment at the Server while the daemon is stopped,
/// then stores the returned authoritative snapshot. Hold the repo's engine
/// lock across this call so a daemon cannot apply an older snapshot meanwhile.
pub async fn set_assignment_direct(
    cfg: &Config,
    _lock: &sk1ll_core::EngineLock,
    changes: &AssignmentChanges,
) -> anyhow::Result<AssignmentUpdate> {
    use anyhow::Context;
    let state = DaemonState::load(&cfg.state)?.context("this machine hasn't joined a fleet")?;
    let http = http_client().build().context("building the HTTP client")?;
    let update = change_assignment(&http, &state, changes).await?;
    save_direct_update(cfg, &update)?;
    Ok(update)
}

pub async fn set_harness_assignment_direct(
    cfg: &Config,
    _lock: &sk1ll_core::EngineLock,
    harness: &str,
    enabled: bool,
) -> anyhow::Result<Option<AssignmentUpdate>> {
    use anyhow::Context;
    let state = DaemonState::load(&cfg.state)?.context("this machine hasn't joined a fleet")?;
    let http = http_client().build().context("building the HTTP client")?;
    let update = set_harness_assignment(&http, &state, harness, sk1ll_proto::SetHarnessAssignment { enabled }).await?;
    if let Some(update) = &update {
        save_direct_update(cfg, update)?;
    }
    Ok(update)
}

fn save_direct_update(cfg: &Config, update: &AssignmentUpdate) -> anyhow::Result<()> {
    use anyhow::Context;
    let snap =
        AssignmentSnapshot { revision: update.revision, assignment: update.assignment.clone(), registration: None };
    crate::state::save_snapshot(&cfg.snapshot_path(), &snap)
        .context("Server accepted the assignment, but saving its authoritative snapshot failed")
}

pub(crate) async fn set_harness_assignment(
    http: &reqwest::Client,
    state: &DaemonState,
    harness: &str,
    body: sk1ll_proto::SetHarnessAssignment,
) -> Result<Option<AssignmentUpdate>> {
    if harness.starts_with('.') {
        return Err(Error::BadRequest(format!("invalid harness `{harness}`")));
    }
    let harness = crate::client::encode(harness);
    let machine = crate::client::encode(&state.machine_id);
    let path = format!("/machines/{machine}/assignment/harness/{harness}");
    let payload = serde_json::to_vec(&body).map_err(|e| Error::Internal(e.into()))?;
    let response = request(http, state, Method::POST, &path, payload.into()).await?;
    let status = response.status();
    let body = response.bytes().await.map_err(|e| Error::Unavailable(format!("reading the Server's response: {e}")))?;
    if status.is_success() {
        return serde_json::from_slice(&body)
            .map(Some)
            .map_err(|e| Error::Unavailable(format!("reading the Server's assignment response: {e}")));
    }
    if status == StatusCode::NOT_FOUND && serde_json::from_slice::<serde_json::Value>(&body).is_err() {
        return Ok(None);
    }
    Err(Error::Upstream(
        status,
        redact(
            &serde_json::from_slice::<ErrorBody>(&body)
                .map_or_else(|_| String::from_utf8_lossy(&body).into_owned(), |error| error.error),
        ),
    ))
}

pub(crate) async fn change_assignment(
    http: &reqwest::Client,
    state: &DaemonState,
    changes: &AssignmentChanges,
) -> anyhow::Result<AssignmentUpdate> {
    use anyhow::Context;
    let path = format!("/machines/{}/assignment", state.machine_id);
    let resp = request(http, state, Method::PUT, &path, serde_json::to_vec(changes)?.into()).await?;
    let status = resp.status();
    let body = resp.bytes().await.context("reading the Server's assignment response")?;
    if !status.is_success() {
        let message = serde_json::from_slice::<sk1ll_proto::ErrorBody>(&body)
            .map_or_else(|_| String::from_utf8_lossy(&body).into_owned(), |e| e.error);
        anyhow::bail!("Server refused the assignment (HTTP {status}): {}", redact(&message));
    }
    let update: AssignmentUpdate = serde_json::from_slice(&body).context("parsing the Server's assignment response")?;
    Ok(update)
}

/// Wait for the existing status protocol to report this local draft before PUT.
/// HTTP and WebSocket requests have no ordering guarantee across transports.
pub(crate) async fn assign_adoption(
    d: &Daemon,
    adoption: &sk1ll_core::sync::unmanaged::Adoption,
) -> anyhow::Result<AssignmentUpdate> {
    let changes = AssignmentChanges {
        changes: adoption
            .locations
            .iter()
            .map(|l| sk1ll_proto::ToggleChange {
                harness: l.harness.clone(),
                kind: sk1ll_proto::ToggleKind::Skill,
                name: Some(adoption.name.clone()),
                enabled: Some(true),
            })
            .collect(),
        ..Default::default()
    };
    assign_draft_item(d, &sk1ll_proto::ItemRef::Skill(adoption.name.clone()), &changes, "Adopt").await
}

/// Changes this machine's toggles for `item` once the Server has its draft report.
pub(crate) async fn assign_draft_item(
    d: &Daemon,
    item: &sk1ll_proto::ItemRef,
    changes: &AssignmentChanges,
    action: &str,
) -> anyhow::Result<AssignmentUpdate> {
    await_draft_report(d, item, action).await?;
    change_assignment(&d.http, d.ident.joined()?, changes).await
}

/// Waits until the Server has this machine's latest report of `item`, when
/// it is a draft or unpushed item.
async fn await_draft_report(d: &Daemon, item: &sk1ll_proto::ItemRef, action: &str) -> anyhow::Result<()> {
    tokio::time::timeout(SEND_TIMEOUT, async {
        loop {
            anyhow::ensure!(d.is_connected(), "{action} needs a connection to the Server");
            let report = d.view().report;
            if !report.drafts.contains(item) && !report.unpushed.contains(item) {
                break; // An unchanged baseline item needs no new draft report.
            }
            let fleet: sk1ll_proto::FleetView =
                request(&d.http, d.ident.joined()?, Method::GET, "/fleet", Bytes::new())
                    .await?
                    .error_for_status()?
                    .json()
                    .await?;
            if fleet.machines.iter().any(|m| {
                m.id == d.ident.machine_id()
                    && m.report.as_ref().is_some_and(|r| r.drafts.contains(item) || r.unpushed.contains(item))
            }) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await
    .map_err(|_| anyhow::anyhow!("{action} timed out waiting for the Server to receive the draft; try again"))?
}

/// Why [`copy_assignment`] has no result.
#[derive(Debug)]
pub(crate) enum CopyFailure {
    /// The Server has no copy endpoint (0.1.0): nothing was copied.
    Unsupported,
    /// Nothing was copied: never sent, or refused.
    Refused(anyhow::Error),
    /// Sent, but the response was lost: the Server may have copied.
    Lost(anyhow::Error),
}

/// Has the Server copy this machine's explicit toggles of `copy.from` to
/// `copy.to`, once it has the machine's report of `copy.from`.
pub(crate) async fn copy_assignment(
    d: &Daemon,
    copy: &sk1ll_proto::AssignmentCopy,
) -> std::result::Result<sk1ll_proto::AssignmentCopied, CopyFailure> {
    use anyhow::Context;
    await_draft_report(d, &copy.from, "Rename").await.map_err(CopyFailure::Refused)?;
    let state = d.ident.joined().map_err(|e| CopyFailure::Refused(e.into()))?;
    if state.revoked {
        return Err(CopyFailure::Refused(anyhow::anyhow!("this machine was removed from the fleet")));
    }
    let url = format!("{}/api/machines/{}/assignment/copy", state.server_url, state.machine_id);
    let resp =
        d.http.post(url).bearer_auth(&state.token).timeout(REQUEST_TIMEOUT).json(copy).send().await.map_err(|e| {
            let error = anyhow::anyhow!(redact(&format!("offline: the server is unreachable ({e})")));
            // Only a request that never connected certainly changed nothing.
            if e.is_connect() { CopyFailure::Refused(error) } else { CopyFailure::Lost(error) }
        })?;
    let status = resp.status();
    let body = resp.bytes().await.context("reading the Server's response").map_err(CopyFailure::Lost)?;
    if status.is_server_error() {
        return Err(CopyFailure::Lost(anyhow::anyhow!("HTTP {status}")));
    }
    if !status.is_success() {
        let error = serde_json::from_slice::<ErrorBody>(&body);
        // Only this Server's own errors have a JSON body; an older Server's router has no route.
        if status == StatusCode::NOT_FOUND && error.is_err() {
            return Err(CopyFailure::Unsupported);
        }
        let message = error.map_or_else(|_| String::from_utf8_lossy(&body).into_owned(), |e| e.error);
        return Err(CopyFailure::Refused(anyhow::anyhow!(
            "Server refused the copy (HTTP {status}): {}",
            redact(&message)
        )));
    }
    serde_json::from_slice(&body).context("parsing the Server's response").map_err(CopyFailure::Lost)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sk1ll_core::repo::Repo;
    use sk1ll_proto::ItemRef;

    #[tokio::test]
    async fn outbox_flush_sends_an_already_published_draft_while_the_engine_is_busy() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        let repo = Repo::init(&root).unwrap();
        let mut cfg = Config::new().unwrap();
        cfg.state = dir.path().join("daemon.json");
        cfg.assignment = dir.path().join("assignment.json");
        cfg.machine_config = dir.path().join("machine.toml");
        std::fs::write(&cfg.machine_config, "").unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let d = Daemon::new(
            cfg,
            DaemonState {
                machine_id: "test".into(),
                token: "test".into(),
                server_url: format!("http://{address}"),
                repo: root,
                revoked: false,
            },
        )
        .unwrap();
        let mut sent = d.view().report;
        let (published, ready) = tokio::sync::oneshot::channel();
        let (release, held) = std::sync::mpsc::channel();
        let busy = tokio::spawn({
            let d = d.clone();
            async move {
                d.blocking(move |d, eng| {
                    repo.create_item("skill", "pdf", "PDF")?;
                    d.refresh(eng)?;
                    published.send(()).unwrap();
                    let _ = held.recv_timeout(Duration::from_secs(10));
                    Ok(())
                })
                .await
            }
        });
        let peer = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
            ws.next().await.unwrap().unwrap()
        });
        let (ws, _) = tokio_tungstenite::connect_async(format!("ws://{address}")).await.unwrap();
        let (mut sink, _stream) = ws.split();
        ready.await.unwrap();
        let report = d.view().report;
        assert!(report.drafts.contains(&ItemRef::Skill("pdf".into())), "{report:?}");
        let mut pending = Default::default();
        let flush = flush_outbox(&d, &mut sink, &mut sent, false, &mut pending);
        tokio::pin!(flush);
        let received = tokio::select! {
            message = tokio::time::timeout(Duration::from_secs(2), peer) => message,
            result = &mut flush => panic!("flush completed while the engine was busy: {result:?}"),
        };
        release.send(()).unwrap();
        busy.await.unwrap().unwrap();
        let message = received.unwrap().unwrap();
        assert_eq!(
            serde_json::from_str::<DaemonMessage>(message.to_text().unwrap()).unwrap(),
            DaemonMessage::Status(report)
        );
        flush.await.unwrap();
    }
}
