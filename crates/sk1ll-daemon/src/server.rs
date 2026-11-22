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
use sk1ll_core::harness_rename::{RenameIntent, RenameRequest};
use sk1ll_proto::{
    AssignmentOutcome, AssignmentSnapshot, AssignmentUpdate, AssignmentWrite, DaemonMessage, ErrorBody,
    InventoryReport, MachineReport, PROTOCOL_HEADER, PROTOCOL_VERSION, ServerMessage,
};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::{self, Message, client::IntoClientRequest};

use crate::auth::redact;
use crate::client::Event;
use crate::daemon::{Daemon, Link};
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
            Ok(ws) => {
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

async fn connect(state: &DaemonState) -> std::result::Result<Ws, ConnectError> {
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
    if let Some(intent) = d.blocking(|d, eng| d.pending_harness_rename(eng)).await? {
        let result = exchange(&mut sink, &mut stream, 1, 0, rename_write(&intent)).await;
        // Reconnect after a refusal too. The exchange consumed the
        // unsolicited initial Assignment while waiting for this reply.
        d.blocking(move |d, eng| d.finish_harness_rename(eng, intent, result)).await?;
    }
    let mut sent = None;
    let mut seq = 0;
    send_report(&mut sink, refresh_report(d).await?, &mut sent, &mut seq).await?;
    let (sender, mut writes) = mpsc::channel::<SessionWrite>(16);
    d.assignment_session.send_replace(Some(sender));
    d.set_link(Link::Connected);
    let mut next_request = 1;
    let mut pending: BTreeMap<u64, (u64, oneshot::Sender<Result<AssignmentUpdate>>)> = BTreeMap::new();
    let catch_up = d.clone();
    tokio::spawn(async move {
        if let Err(e) = catch_up.on_connected().await {
            tracing::warn!("after connecting: {e}");
        }
    });

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
                // Reports are published only at the end of a local refresh.
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

/// The rename intent for `request`: the pending one if it is for the same
/// request, else a new one from `authoritative` (the server's Assignment,
/// see [`fetch_machine_assignment`]). Only local files: callers fetch first.
pub(crate) fn prepare_rename(
    cfg: &Config,
    state: &DaemonState,
    request_body: RenameRequest,
    authoritative: Option<&sk1ll_proto::Assignment>,
) -> Result<RenameIntent> {
    let authority = state.rename_authority();
    let config = &cfg.machine_config;
    if let Some(intent) = RenameIntent::load(config, Some(&authority))? {
        if intent.request == request_body {
            return Ok(intent);
        }
        return Err(Error::Conflict("finish the pending Harness rename first".into()));
    }
    let authoritative = authoritative
        .ok_or_else(|| Error::Conflict("the pending Harness rename finished meanwhile; reload and retry".into()))?;
    Ok(RenameIntent::prepare(config, &state.repo, request_body, authoritative, Some(&authority))?)
}

pub(crate) fn rename_write(intent: &RenameIntent) -> AssignmentWrite {
    AssignmentWrite::RenameHarness(sk1ll_proto::HarnessRename {
        from: intent.request.from.clone(),
        to: intent.request.to.clone(),
        replace: intent.request.replace,
        operation_id: intent.operation_id.clone(),
    })
}

fn definitive_refusal(error: &Error) -> bool {
    matches!(error, Error::Upstream(status, _) if matches!(*status,
        StatusCode::BAD_REQUEST | StatusCode::CONFLICT | StatusCode::FORBIDDEN | StatusCode::NOT_FOUND))
}

pub(crate) fn resolve_rename(
    config: &std::path::Path,
    intent: &RenameIntent,
    result: Result<AssignmentUpdate>,
) -> Result<AssignmentUpdate> {
    match result {
        Ok(update) => {
            intent.apply_config(config)?;
            intent.finish(config)?;
            Ok(update)
        }
        Err(error) => {
            if definitive_refusal(&error) {
                intent.rollback_config(config)?;
            }
            Err(error)
        }
    }
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

pub async fn rename_harness_direct(
    cfg: &Config,
    state: &DaemonState,
    _lock: &sk1ll_core::EngineLock,
    request_body: RenameRequest,
) -> anyhow::Result<crate::client::HarnessRenameResult> {
    let http = http_client().build()?;
    let authority = state.rename_authority();
    let authoritative = match RenameIntent::load(&cfg.machine_config, Some(&authority))? {
        Some(_) => None,
        None => Some(fetch_machine_assignment(state, &http).await?.assignment),
    };
    let intent = prepare_rename(cfg, state, request_body, authoritative.as_ref())?;
    intent.apply_config(&cfg.machine_config)?;
    let (mut sink, mut stream) = direct_session(state).await?;
    let result = exchange(&mut sink, &mut stream, 1, 0, rename_write(&intent)).await;
    // Persist the new authoritative Assignment before clearing the intent.
    if let Ok(update) = &result {
        save_assignment_snapshot(cfg, update)?;
    }
    let update = resolve_rename(&cfg.machine_config, &intent, result)?;
    Ok(crate::client::HarnessRenameResult { update, routed_rules: intent.routed_rules })
}

async fn direct_session(state: &DaemonState) -> anyhow::Result<(Sink, futures_util::stream::SplitStream<Ws>)> {
    anyhow::ensure!(!state.revoked, "this machine was removed from the fleet");
    let ws = connect(state).await.map_err(|error| match error {
        ConnectError::Unauthorized => anyhow::anyhow!("the Server refused this Machine's token"),
        ConnectError::Other(message) => anyhow::anyhow!(message),
    })?;
    let (mut sink, stream) = ws.split();
    send(&mut sink, &DaemonMessage::Hello { machine_id: state.machine_id.clone() }).await?;
    Ok((sink, stream))
}

pub(crate) fn save_assignment_snapshot(cfg: &Config, update: &AssignmentUpdate) -> anyhow::Result<()> {
    use anyhow::Context;
    let snap =
        AssignmentSnapshot { revision: update.revision, assignment: update.assignment.clone(), registration: None };
    crate::state::save_snapshot(&cfg.snapshot_path(), &snap)
        .context("Server accepted the Assignment, but saving its snapshot failed")
}

/// Open a short-lived session when the daemon is stopped.
pub async fn write_assignment_direct(
    cfg: &Config,
    _lock: &sk1ll_core::EngineLock,
    write: AssignmentWrite,
) -> anyhow::Result<AssignmentUpdate> {
    use anyhow::Context;
    let state = DaemonState::load(&cfg.state)?.context("this machine hasn't joined a fleet")?;
    let (mut sink, mut stream) = direct_session(&state).await?;
    let authority = state.rename_authority();
    if let Some(intent) = RenameIntent::load(&cfg.machine_config, Some(&authority))? {
        intent.apply_config(&cfg.machine_config)?;
        let result = exchange(&mut sink, &mut stream, 1, 0, rename_write(&intent)).await;
        if let Ok(update) = &result {
            save_assignment_snapshot(cfg, update)?;
        }
        resolve_rename(&cfg.machine_config, &intent, result)?;
    }
    let harnesses = sk1ll_core::MachineConfig::load(&cfg.machine_config)?.harnesses().into_keys().collect();
    let inventory = crate::daemon::repo_inventory(&state.repo, harnesses);
    send(&mut sink, &DaemonMessage::Inventory(InventoryReport { seq: 1, ..inventory })).await?;
    let update = exchange(&mut sink, &mut stream, 2, 1, write).await?;
    save_assignment_snapshot(cfg, &update)?;
    Ok(update)
}
