//! The daemon's WebSocket session and authenticated fleet REST requests.
//!
//! The socket sends a fresh status before assignment writes. It handles write
//! replies itself, so a write's caller waits for its reply here, without the
//! engine mutex. A separate worker handles unsolicited snapshots and nudges
//! in order.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::response::Response;
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use sk1ll_proto::{
    AssignmentOutcome, AssignmentSnapshot, AssignmentUpdate, AssignmentWrite, DaemonMessage, ErrorBody, MachineReport,
    PROTOCOL_HEADER, PROTOCOL_VERSION, ServerMessage, VERSION_HEADER,
};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::{self, Message, client::IntoClientRequest};

use crate::DaemonState;
use crate::auth::redact;
use crate::client::{Connection, Event, ServerVersion};
use crate::daemon::{Daemon, Link, rename_write};
use crate::error::{Error, Result};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const SEND_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;
type Sink = futures_util::stream::SplitSink<Ws, Message>;

enum ConnectError {
    /// HTTP 401 on the upgrade: the token was refused, by the server or by
    /// something in front of it.
    Unauthorized,
    /// HTTP 426: the server speaks another protocol version.
    Incompatible(String, ServerVersion),
    Other(String),
}

/// Which side to upgrade, when the server named its protocol: protocol
/// versions only grow, so the lower one is the older side.
fn incompatible(server: &ServerVersion) -> Option<String> {
    let theirs = server.protocol?;
    let release = server.version.as_deref().map_or_else(String::new, |v| format!("sk1ll {v}, "));
    let ours = env!("CARGO_PKG_VERSION");
    let upgrade = if theirs < PROTOCOL_VERSION {
        "upgrade the Server to this Machine's release"
    } else {
        "upgrade sk1ll on this Machine to the Server's release"
    };
    Some(format!(
        "the Server runs {release}protocol {theirs}; this Machine runs sk1ll {ours}, protocol {PROTOCOL_VERSION}: {upgrade}"
    ))
}

/// The versions a server response names in its headers.
fn server_version(headers: &axum::http::HeaderMap) -> ServerVersion {
    let get = |name| headers.get(name).and_then(|v| v.to_str().ok()).map(|v| v.trim().to_string());
    ServerVersion {
        version: get(VERSION_HEADER).filter(|v| !v.is_empty()),
        protocol: get(PROTOCOL_HEADER).and_then(|v| v.parse().ok()),
    }
}

pub(crate) struct SessionWrite {
    pub(crate) write: AssignmentWrite,
    pub(crate) result: oneshot::Sender<Result<AssignmentUpdate>>,
}

pub(crate) async fn write_assignment(d: &Daemon, write: AssignmentWrite) -> Result<AssignmentUpdate> {
    let sender = d.assignment_session.borrow().clone().ok_or_else(offline)?;
    let (result, reply) = oneshot::channel();
    tokio::time::timeout(REQUEST_TIMEOUT, async {
        sender.send(SessionWrite { write, result }).await.map_err(|_| offline())?;
        reply
            .await
            .map_err(|_| Error::Unavailable("the Server disconnected before confirming the Assignment".into()))?
    })
    .await
    .map_err(|_| Error::Unavailable("the Server did not confirm the Assignment in time".into()))?
}

/// Waits for the server session. A daemon that connects on demand (an
/// engine in a CLI process) starts it for the first request that needs it
/// and waits until it connected or failed to; any other fails at once
/// while it is offline.
pub(crate) async fn connected(d: &Arc<Daemon>) -> Result<()> {
    if d.assignment_session.borrow().is_some() && d.is_connected() {
        return Ok(());
    }
    if !d.on_demand() || d.ident.joined().is_err() {
        return Err(offline());
    }
    d.session_started.call_once(|| {
        tokio::spawn(run(d.clone()));
    });
    let mut view = d.subscribe_view();
    let wait = async {
        loop {
            let status = view.borrow_and_update().clone();
            match status.connection {
                Connection::Connected if d.assignment_session.borrow().is_some() => return Ok(()),
                Connection::Offline | Connection::AuthRejected | Connection::Incompatible | Connection::Revoked => {
                    return Err(Error::Unavailable(status.last_error.unwrap_or_else(|| offline().to_string())));
                }
                _ => {}
            }
            if view.changed().await.is_err() {
                return Err(offline());
            }
        }
    };
    tokio::time::timeout(CONNECT_TIMEOUT + SEND_TIMEOUT + REQUEST_TIMEOUT, wait)
        .await
        .map_err(|_| Error::Unavailable("the Server did not accept the connection in time".into()))?
}

fn offline() -> Error {
    Error::Unavailable("offline: Assignment changes need a connection to the Server".into())
}

fn assignment_result(outcome: AssignmentOutcome) -> Result<AssignmentUpdate> {
    match outcome {
        AssignmentOutcome::Applied(update) => Ok(update),
        AssignmentOutcome::Refused { status, error } => {
            Err(Error::Upstream(StatusCode::from_u16(status).map_err(|e| Error::Internal(e.into()))?, redact(&error)))
        }
    }
}

pub(crate) async fn run(d: Arc<Daemon>) {
    let Ok(state) = d.ident.joined() else {
        return;
    };
    let (inbox_tx, inbox_rx) = mpsc::unbounded_channel();
    tokio::spawn(inbox(d.clone(), inbox_rx));
    let mut backoff = d.cfg.reconnect_min;
    while !d.is_revoked() {
        match connect(state).await {
            Ok((ws, server)) => {
                d.update(|v| v.server = Some(server));
                backoff = d.cfg.reconnect_min;
                tracing::info!("connected to {}", state.server_url);
                if let Err(e) = session(&d, ws, &inbox_tx).await {
                    tracing::info!("server connection lost: {}", redact(&format!("{e:#}")));
                }
                d.assignment_session.send_replace(None);
                if !d.is_revoked() {
                    d.set_link(Link::Offline);
                }
            }
            Err(ConnectError::Unauthorized) => d.set_auth_rejected(),
            Err(ConnectError::Incompatible(e, server)) => {
                d.update(|v| v.server = Some(server));
                if d.view().last_error.as_deref() != Some(e.as_str()) {
                    d.set_error(e);
                }
                d.set_link(Link::Incompatible);
            }
            Err(ConnectError::Other(e)) => {
                if d.view().last_error.as_deref() != Some(e.as_str()) {
                    d.set_error(e);
                }
                d.set_link(Link::Offline);
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
    let micros = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |t| t.subsec_micros());
    let frac = f64::from(micros % 1000) / 1000.0;
    d / 2 + d.mul_f64(frac / 2.0)
}

async fn connect(state: &DaemonState) -> std::result::Result<(Ws, ServerVersion), ConnectError> {
    let other = |e: &dyn std::fmt::Display| ConnectError::Other(redact(&format!("server unreachable (offline): {e}")));
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
            let server = server_version(resp.headers());
            let error = incompatible(&server).unwrap_or_else(|| body.map_or_else(fallback, |b| b.error));
            Err(ConnectError::Incompatible(error, server))
        }
        Ok(Err(e)) => Err(other(&e)),
        Ok(Ok((ws, resp))) => Ok((ws, server_version(resp.headers()))),
    }
}

async fn send(ws: &mut Sink, msg: &DaemonMessage) -> anyhow::Result<()> {
    let text = serde_json::to_string(msg)?;
    tokio::time::timeout(SEND_TIMEOUT, ws.send(Message::text(text)))
        .await
        .map_err(|_| anyhow::anyhow!("send timed out"))??;
    Ok(())
}

async fn reconciled_report(d: &Arc<Daemon>) -> anyhow::Result<MachineReport> {
    match d
        .blocking(|d, eng| {
            d.reconcile(eng)?;
            Ok(d.view().report)
        })
        .await
    {
        Ok(report) => Ok(report),
        Err(e) => {
            d.set_error(format!("reconciling before reporting to the Server: {e}"));
            Err(e.into())
        }
    }
}

async fn session(d: &Arc<Daemon>, ws: Ws, inbox: &mpsc::UnboundedSender<ServerMessage>) -> anyhow::Result<()> {
    let (mut sink, mut stream) = ws.split();
    let mut view = d.subscribe_view();
    send(&mut sink, &DaemonMessage::Hello { machine_id: d.ident.machine_id().to_string() }).await?;
    if let Some((intent, world)) = d.blocking(|d, eng| d.pending_harness_rename(eng)).await? {
        let result = exchange(&mut sink, &mut stream, 1, 0, rename_write(&intent)).await;
        // Reconnect after a refusal too. The exchange consumed the
        // unsolicited initial Assignment while waiting for this reply.
        d.blocking(move |d, eng| d.finish_harness_rename(eng, world, intent, result)).await?;
    }
    let mut sent = None;
    let mut seq = 0;
    send_report(&mut sink, reconciled_report(d).await?, &mut sent, &mut seq).await?;
    let (sender, mut writes) = mpsc::channel::<SessionWrite>(16);
    d.assignment_session.send_replace(Some(sender));
    if !d.set_link(Link::Connected) {
        // Halted for a rejoin meanwhile: this session's registration is
        // being replaced.
        d.assignment_session.send_replace(None);
        return Ok(());
    }
    let mut next_request = 1;
    let mut pending: BTreeMap<u64, (u64, oneshot::Sender<Result<AssignmentUpdate>>)> = BTreeMap::new();
    // An engine in a CLI process serves the one request it connected for.
    if !d.on_demand() {
        let catch_up = d.clone();
        tokio::spawn(async move {
            if let Err(e) = catch_up.on_connected().await {
                tracing::warn!("after connecting: {e}");
            }
        });
    }

    loop {
        tokio::select! {
            msg = stream.next() => match msg {
                Some(Ok(Message::Text(text))) => match serde_json::from_str::<ServerMessage>(&text) {
                    Ok(ServerMessage::AssignmentResult { req, seq, result }) => {
                        if let Some((expected, reply)) = pending.remove(&req) {
                            let result = if seq == expected {
                                assignment_result(result)
                            } else {
                                Err(Error::Unavailable(format!(
                                    "the Server confirmed Assignment after report {seq}, expected {expected}"
                                )))
                            };
                            let _ = reply.send(result);
                        }
                    }
                    Ok(ServerMessage::Revoked) => {
                        d.assignment_session.send_replace(None);
                        drop(writes);
                        drop(pending);
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
                // Reports are published only at the end of a reconcile.
                // Reading them here must not wait for a pull's engine mutex.
                let report = view.borrow_and_update().report.clone();
                send_report(&mut sink, report, &mut sent, &mut seq).await?;
            }
            () = d.restart_assignment_session.notified() => return Ok(()),
            Some(request) = writes.recv() => {
                pending.retain(|_, (_, reply)| !reply.is_closed());
                if request.result.is_closed() {
                    continue;
                }
                if !matches!(request.write, AssignmentWrite::RenameHarness(_)) {
                    let report = view.borrow_and_update().report.clone();
                    send_report(&mut sink, report, &mut sent, &mut seq).await?;
                }
                next_request += 1;
                send(&mut sink, &DaemonMessage::SetAssignment { req: next_request, write: request.write }).await?;
                pending.insert(next_request, (seq, request.result));
            }
        }
    }
}

/// The published report keeps seq at zero. Only the session numbers frames.
async fn send_report(
    sink: &mut Sink,
    report: MachineReport,
    sent: &mut Option<MachineReport>,
    seq: &mut u64,
) -> anyhow::Result<()> {
    if sent.as_ref() != Some(&report) {
        *seq += 1;
        send(sink, &DaemonMessage::Status(MachineReport { seq: *seq, ..report.clone() })).await?;
        *sent = Some(report);
    }
    Ok(())
}

/// Handles server messages in the order they arrived.
async fn inbox(d: Arc<Daemon>, mut rx: mpsc::UnboundedReceiver<ServerMessage>) {
    while let Some(m) = rx.recv().await {
        // Off the runtime: a test's callback may hold the message here.
        #[cfg(feature = "failpoints")]
        {
            let config = d.cfg.machine_config.clone();
            let _ = tokio::task::spawn_blocking(move || sk1ll_core::failpoint!(DaemonServerMessage, &config)).await;
        }
        let result = match m {
            ServerMessage::AssignmentResult { .. } => Ok(()),
            ServerMessage::Assignment(snap) => d.blocking(move |d, eng| d.apply_assignment(eng, snap).map(drop)).await,
            ServerMessage::Nudge { baseline } => d.nudge(baseline).await,
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

/// The authenticated Server transport.
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

/// `POST /machines/{id}/assignment/copy-from` for this Machine itself, as
/// the server confirms it; its error as the server words it.
pub(crate) async fn copy_own_assignment(
    d: &Daemon,
    copy: &sk1ll_proto::MachineAssignmentCopy,
) -> Result<AssignmentUpdate> {
    if d.is_revoked() {
        return Err(Error::Unavailable("this machine was removed from the fleet; it can't reach the server".into()));
    }
    let state = d.ident.joined()?;
    let path = format!("/machines/{}/assignment/copy-from", crate::client::encode(&state.machine_id));
    let body = Bytes::from(serde_json::to_vec(copy).map_err(|e| Error::Internal(e.into()))?);
    let response = request(&d.http, state, Method::POST, &path, body).await?;
    let status = StatusCode::from_u16(response.status().as_u16()).map_err(|e| Error::Internal(e.into()))?;
    if !status.is_success() {
        let error = match response.json::<sk1ll_proto::ErrorBody>().await {
            Ok(body) => body.error,
            Err(_) => status.to_string(),
        };
        return Err(Error::Upstream(status, redact(&error)));
    }
    response.json().await.map_err(|e| Error::Unavailable(redact(&format!("reading the server's response: {e}"))))
}

/// This Machine's Assignment as the server has it now.
pub(crate) async fn fetch_machine_assignment(
    state: &DaemonState,
    http: &reqwest::Client,
) -> Result<AssignmentSnapshot> {
    let path = format!("/machines/{}/assignment", crate::client::encode(&state.machine_id));
    let response = request(http, state, Method::GET, &path, Bytes::new()).await?;
    response
        .error_for_status()
        .map_err(|e| Error::Unavailable(redact(&e.to_string())))?
        .json()
        .await
        .map_err(|e| Error::Unavailable(redact(&e.to_string())))
}

async fn exchange(
    sink: &mut Sink,
    stream: &mut futures_util::stream::SplitStream<Ws>,
    req: u64,
    expected_seq: u64,
    write: AssignmentWrite,
) -> Result<AssignmentUpdate> {
    send(sink, &DaemonMessage::SetAssignment { req, write })
        .await
        .map_err(|e| Error::Unavailable(redact(&e.to_string())))?;
    tokio::time::timeout(REQUEST_TIMEOUT, async {
        while let Some(frame) = stream.next().await {
            let frame = frame.map_err(|e| Error::Unavailable(redact(&e.to_string())))?;
            if let Message::Text(text) = frame
                && let Ok(ServerMessage::AssignmentResult { req: id, seq, result }) = serde_json::from_str(&text)
                && id == req
            {
                if seq != expected_seq {
                    return Err(Error::Unavailable(
                        "the Server confirmed Assignment after an unexpected status".into(),
                    ));
                }
                return assignment_result(result);
            }
        }
        Err(Error::Unavailable("the Server disconnected before confirming the Assignment".into()))
    })
    .await
    .map_err(|_| Error::Unavailable("the Server did not confirm the Assignment in time".into()))?
}
