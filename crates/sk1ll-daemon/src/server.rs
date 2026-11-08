//! The daemon's side of the server protocol (`sk1ll-proto`): one WebSocket
//! session at a time, reconnected with jittered exponential backoff, plus
//! authenticated pass-through of REST calls for the local API.
//!
//! On connect: `hello`; then the local state is recomputed (with the engine
//! mutex) and sent as the first status; then the outbox of `draft_discarded`
//! is delivered, each entry removed only once sent; then commits queued
//! while offline are pushed and the baseline is pulled. Afterwards a status
//! goes out whenever the report changes, always before newly queued
//! `draft_discarded`. The socket is read continuously so pings are answered;
//! server messages are handled in order by a separate worker so slow work (a
//! pull) never stalls reading.
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
use sk1ll_proto::{AssignmentChanges, AssignmentSnapshot, AssignmentUpdate, DaemonMessage, ServerMessage, MachineReport};
use tokio::sync::mpsc;
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

pub(crate) async fn run(d: Arc<Daemon>) {
    let (inbox_tx, inbox_rx) = mpsc::unbounded_channel();
    tokio::spawn(inbox(d.clone(), inbox_rx));
    let mut backoff = d.cfg.reconnect_min;
    while !d.is_revoked() {
        match connect(&d).await {
            Ok(ws) => {
                backoff = d.cfg.reconnect_min;
                tracing::info!("connected to {}", d.ident.server_url);
                if let Err(e) = session(&d, ws, &inbox_tx).await {
                    tracing::info!("server connection lost: {}", redact(&format!("{e:#}")));
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
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |t| t.subsec_nanos());
    let frac = f64::from(nanos % 1000) / 1000.0;
    d / 2 + d.mul_f64(frac / 2.0)
}

async fn connect(d: &Daemon) -> std::result::Result<Ws, ConnectError> {
    let other = |e: &dyn std::fmt::Display| ConnectError::Other(redact(&format!("server unreachable (offline): {e}")));
    let mut req = d.ident.ws_url().into_client_request().map_err(|e| other(&e))?;
    let auth = HeaderValue::from_str(&format!("Bearer {}", d.ident.token)).map_err(|_| other(&"bad token"))?;
    req.headers_mut().insert(header::AUTHORIZATION, auth);
    match tokio::time::timeout(CONNECT_TIMEOUT, tokio_tungstenite::connect_async(req)).await {
        Err(_) => Err(other(&"connection timed out")),
        Ok(Err(tungstenite::Error::Http(resp))) if resp.status() == StatusCode::UNAUTHORIZED => {
            Err(ConnectError::Unauthorized)
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
async fn flush_outbox(d: &Arc<Daemon>, sink: &mut Sink, sent: &mut MachineReport) -> anyhow::Result<()> {
    loop {
        // Capture the entry and the state used to filter it under the same
        // engine mutex, so a concurrent Restore cannot queue against an older report.
        // Adoption holds the engine mutex while waiting for its published
        // draft to reach the Server. Keep sending fresh reports even if an outbox
        // refresh is waiting for that mutex.
        let mut view = d.subscribe_view();
        let refresh = d.blocking(|d, eng| {
            d.refresh(eng)?;
            Ok((d.view().report, d.outbox_front()))
        });
        tokio::pin!(refresh);
        let (report, entry) = loop {
            tokio::select! {
                result = &mut refresh => break result.map_err(|e| {
                    d.set_error(format!("refreshing before replaying discards: {e}"));
                    e
                })?,
                changed = view.changed() => {
                    changed?;
                    let report = view.borrow_and_update().report.clone();
                    if report != *sent {
                        send(sink, &DaemonMessage::Status(report.clone())).await?;
                        *sent = report;
                    }
                }
            }
        };
        if report != *sent {
            send(sink, &DaemonMessage::Status(report.clone())).await?;
            *sent = report.clone();
        }
        let Some(entry) = entry else { return Ok(()) };
        let items = entry.items.into_iter()
            .filter(|item| !report.drafts.contains(item) && !report.unpushed.contains(item))
            .collect::<Vec<_>>();
        if !items.is_empty() {
            send(sink, &DaemonMessage::DraftDiscarded { items }).await?;
        }
        d.outbox_delivered(entry.id);
    }
}

/// No cached report or discard is safe to send after a failed refresh.
async fn refresh_report(d: &Arc<Daemon>) -> anyhow::Result<MachineReport> {
    match d.blocking(|d, eng| {
        d.refresh(eng)?;
        Ok(d.view().report)
    }).await {
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
    send(&mut sink, &DaemonMessage::Hello { machine_id: d.ident.machine_id.clone() }).await?;
    // The cached report may be stale (changes while offline): recompute it
    // before the first status and before replaying any discards.
    let mut sent = refresh_report(d).await?;
    send(&mut sink, &DaemonMessage::Status(sent.clone())).await?;
    flush_outbox(d, &mut sink, &mut sent).await?;
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
                    Ok(ServerMessage::Revoked) => {
                        d.blocking(|d, eng| d.revoke(eng, "removed from the fleet")).await?;
                        return Ok(());
                    }
                    Ok(m) => {
                        let _ = inbox.send(m);
                    }
                    Err(e) => tracing::warn!("bad server message: {e}"),
                },
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
                flush_outbox(d, &mut sink, &mut sent).await?;
            }
        }
    }
}

/// Handles server messages in the order they arrived.
async fn inbox(d: Arc<Daemon>, mut rx: mpsc::UnboundedReceiver<ServerMessage>) {
    while let Some(m) = rx.recv().await {
        let result = match m {
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

/// Forwards a local API call to the server's `/api` with the machine token.
pub(crate) async fn proxy(d: &Daemon, method: Method, path_and_query: &str, body: Bytes) -> Result<Response> {
    if d.is_revoked() {
        return Err(Error::Unavailable("this machine was removed from the fleet; it can't reach the server".into()));
    }
    let resp = request(&d.http, &d.ident, method.clone(), path_and_query, body).await?;
    let status = resp.status();
    let content_type = resp.headers().get(header::CONTENT_TYPE).cloned();
    let bytes = resp.bytes().await.map_err(|e| Error::Unavailable(format!("reading the server's response: {e}")))?;
    if method != Method::GET && status.is_success() {
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
    http: &reqwest::Client, state: &DaemonState, method: Method, path_and_query: &str, body: Bytes,
) -> Result<reqwest::Response> {
    if state.revoked {
        return Err(Error::Unavailable("this machine was removed from the fleet; it can't reach the server".into()));
    }
    let url = format!("{}/api{path_and_query}", state.server_url);
    let mut req = http.request(method, url).bearer_auth(&state.token).timeout(REQUEST_TIMEOUT);
    if !body.is_empty() {
        req = req.header(header::CONTENT_TYPE, "application/json").body(body);
    }
    req
        .send()
        .await
        .map_err(|e| Error::Unavailable(redact(&format!("offline: the server is unreachable ({e})"))))
}

/// Changes this machine's assignment at the Server while the daemon is stopped,
/// then stores the returned authoritative snapshot. Hold the repo's engine
/// lock across this call so a daemon cannot apply an older snapshot meanwhile.
pub async fn set_assignment_direct(
    cfg: &Config, _lock: &sk1ll_core::EngineLock, changes: &AssignmentChanges,
) -> anyhow::Result<AssignmentUpdate> {
    use anyhow::Context;
    let state = DaemonState::load(&cfg.state)?.context("this machine hasn't joined a fleet")?;
    let update = change_assignment(&reqwest::Client::new(), &state, changes).await?;
    let snap = AssignmentSnapshot { revision: update.revision, assignment: update.assignment.clone() };
    crate::state::save_snapshot(&cfg.snapshot_path(), &snap)
        .context("Server accepted the assignment, but saving its authoritative snapshot failed")?;
    Ok(update)
}

async fn change_assignment(http: &reqwest::Client, state: &DaemonState, changes: &AssignmentChanges) -> anyhow::Result<AssignmentUpdate> {
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
pub(crate) async fn assign_adoption(d: &Daemon, adoption: &sk1ll_core::sync::unmanaged::Adoption) -> anyhow::Result<AssignmentUpdate> {
    let item = sk1ll_proto::ItemRef::Skill(adoption.name.clone());
    tokio::time::timeout(SEND_TIMEOUT, async {
        loop {
            anyhow::ensure!(d.is_connected(), "Adopt needs a connection to the Server");
            let report = d.view().report;
            if !report.drafts.contains(&item) && !report.unpushed.contains(&item) {
                break; // An unchanged baseline item needs no new draft report.
            }
            let fleet: sk1ll_proto::FleetView = request(&d.http, &d.ident, Method::GET, "/fleet", Bytes::new())
                .await?.error_for_status()?.json().await?;
            if fleet.machines.iter().any(|m| m.id == d.ident.machine_id && m.report.as_ref().is_some_and(|r| {
                r.drafts.contains(&item) || r.unpushed.contains(&item)
            })) { break; }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        Ok::<_, anyhow::Error>(())
    }).await.map_err(|_| anyhow::anyhow!("Adopt timed out waiting for the Server to receive the draft; retry Adopt"))??;
    let changes = AssignmentChanges { changes: adoption.locations.iter().map(|l| sk1ll_proto::ToggleChange {
        harness: l.harness.clone(), kind: sk1ll_proto::ToggleKind::Skill,
        name: Some(adoption.name.clone()), enabled: Some(true),
    }).collect() };
    change_assignment(&d.http, &d.ident, &changes).await
}
