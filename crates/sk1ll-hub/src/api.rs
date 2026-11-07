//! REST API under `/api`. Everything except `POST /join` needs
//! `Authorization: Bearer <token>` (a machine token or the admin token); any
//! joined machine may manage the whole fleet.

use std::sync::Arc;

use axum::extract::{FromRequestParts, Path, Query, State};
use axum::http::StatusCode;
use axum::http::request::Parts;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use sk1ll_proto::{
    Assignment, AssignmentChanges, AssignmentSnapshot, AssignmentUpdate, FleetView, ImportsView, JoinCode, JoinRequest, JoinResponse,
    LogCause, LogEntry, LogQuery, MachineRow, REPO_PATH, headline,
};

use crate::assign::{self, Change};
use crate::error::{ApiError, Result};
use crate::{Caller, Hub, check_name, db, hash, lock, now, random_bytes};

type HubState = State<Arc<Hub>>;

pub fn routes() -> Router<Arc<Hub>> {
    Router::new()
        .route("/join", post(join))
        .route("/join-codes", post(create_join_code))
        .route("/fleet", get(fleet))
        .route("/machines/{id}", delete(remove_machine))
        .route("/machines/{id}/assignment", get(get_assignment).put(put_assignment))
        .route("/log", get(log))
        .route("/log/{id}/undo", post(undo))
        .route("/imports", get(imports))
        .route("/ws", get(crate::ws::upgrade))
}

/// The bearer token from `Authorization`.
pub fn bearer(parts: &Parts) -> Option<&str> {
    let v = parts.headers.get(axum::http::header::AUTHORIZATION)?.to_str().ok()?;
    v.strip_prefix("Bearer ").or_else(|| v.strip_prefix("bearer ")).map(str::trim)
}

impl FromRequestParts<Arc<Hub>> for Caller {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, hub: &Arc<Hub>) -> Result<Self> {
        hub.authenticate(bearer(parts).ok_or(ApiError::Unauthorized)?).await
    }
}

/// Join codes: 10 characters from an unambiguous alphabet, shown as `XXXXX-XXXXX`.
const CODE_ALPHABET: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";

fn new_join_code() -> String {
    let mut code = String::new();
    while code.len() < 10 {
        for b in random_bytes::<16>() {
            // Reject the tail so every character is equally likely.
            if (b as usize) < 256 - 256 % CODE_ALPHABET.len() && code.len() < 10 {
                code.push(CODE_ALPHABET[b as usize % CODE_ALPHABET.len()] as char);
            }
        }
    }
    format!("{}-{}", &code[..5], &code[5..])
}

/// Codes are compared case-insensitively, ignoring dashes and spaces.
fn normalize_code(code: &str) -> String {
    code.chars().filter(|c| c.is_ascii_alphanumeric()).map(|c| c.to_ascii_uppercase()).collect()
}

async fn create_join_code(State(hub): HubState, caller: Caller) -> Result<(StatusCode, Json<JoinCode>)> {
    let code = new_join_code();
    let (code_hash, author, now) = (hash(&normalize_code(&code)), caller.author().to_string(), now());
    hub.db(move |conn| db::insert_join_code(conn, &code_hash, &author, now)).await?;
    tracing::info!("{} created a join code", caller.author());
    Ok((StatusCode::CREATED, Json(JoinCode { code })))
}

async fn join(State(hub): HubState, Json(req): Json<JoinRequest>) -> Result<(StatusCode, Json<JoinResponse>)> {
    check_name("machine id", &req.machine_id)?;
    if req.machine_id == assign::HUB_AUTHOR || req.machine_id == "admin" {
        return Err(ApiError::BadRequest(format!("`{}` is reserved", req.machine_id)));
    }
    for h in &req.harnesses {
        check_name("harness", h)?;
    }
    let token = crate::hex(&random_bytes::<32>());
    let token_hash = hash(&token);
    let req2 = req.clone();
    let (assignment, revision) = hub
        .db(move |conn| {
            let req = req2;
            let tx = conn.transaction()?;
            let mut assignment = match &req.copy_from {
                Some(src) => db::assignment(&tx, src)?
                    .ok_or_else(|| ApiError::BadRequest(format!("copy_from: no machine `{src}`")))?,
                None => Assignment::default(),
            };
            for h in &req.harnesses {
                assignment.harnesses.entry(h.clone()).or_insert_with(assign::all_off);
            }
            if db::machine(&tx, &req.machine_id)?.is_some() {
                return Err(ApiError::Conflict(format!("machine `{}` already joined", req.machine_id)));
            }
            let now = now();
            if !db::consume_join_code(&tx, &hash(&normalize_code(&req.code)), &req.machine_id, now)? {
                return Err(ApiError::Forbidden("invalid or already used join code".into()));
            }
            let revision = db::insert_machine(&tx, &req.machine_id, &token_hash, now, &assignment)?;
            tx.commit()?;
            Ok((assignment, revision))
        })
        .await?;
    tracing::info!("machine {} joined", req.machine_id);
    Ok((
        StatusCode::CREATED,
        Json(JoinResponse { machine_id: req.machine_id, token, repo_path: REPO_PATH.into(), assignment, revision }),
    ))
}

async fn fleet(State(hub): HubState, _: Caller) -> Result<Json<FleetView>> {
    let baseline = hub.head();
    let records = hub.db(|conn| db::machines(conn)).await?;
    let now = now();
    let offline_after = hub.config.offline_after.as_secs() as i64;
    let machines = records
        .into_iter()
        .map(|m| {
            let connected = hub.is_connected(&m.id);
            // By age alone: a registered but silent connection is Offline too.
            let offline = m.last_seen.is_none_or(|t| now - t > offline_after);
            MachineRow {
                status: headline(offline, m.report.as_ref(), baseline.as_deref()),
                connected,
                joined_at: m.joined_at,
                last_seen: m.last_seen,
                report: m.report,
                id: m.id,
            }
        })
        .collect();
    Ok(Json(FleetView { baseline, machines }))
}

async fn get_assignment(State(hub): HubState, _: Caller, Path(id): Path<String>) -> Result<Json<AssignmentSnapshot>> {
    let machine = id.clone();
    hub.db(move |conn| db::snapshot(conn, &machine))
        .await?
        .map(Json)
        .ok_or_else(|| ApiError::NotFound(format!("no machine `{id}`")))
}

async fn put_assignment(
    State(hub): HubState,
    caller: Caller,
    Path(id): Path<String>,
    Json(body): Json<AssignmentChanges>,
) -> Result<Json<AssignmentUpdate>> {
    let changes: Vec<Change> = body
        .changes
        .into_iter()
        .map(|c| Change { harness: c.harness, kind: c.kind, name: c.name, after: c.enabled, cause: LogCause::Change })
        .collect();
    changes.iter().try_for_each(assign::validate)?;
    let (machine, author, now, items) = (id.clone(), caller.author().to_string(), now(), hub.baseline_items());
    let applied = hub
        .db(move |conn| {
            let tx = conn.transaction()?;
            let m = db::machine(&tx, &machine)?.ok_or_else(|| ApiError::NotFound(format!("no machine `{machine}`")))?;
            let report = m.report.unwrap_or_default();
            assign::check_items(&changes, &items, &report.drafts, &report.unpushed)?;
            let applied = assign::apply(&tx, &machine, &author, changes, now, Some(&items))?;
            tx.commit()?;
            Ok(applied)
        })
        .await?;
    Ok(Json(published(&hub, &id, applied)))
}

/// Hands a changed assignment to the machine's session and builds the response.
fn published(hub: &Hub, machine: &str, applied: assign::Applied) -> AssignmentUpdate {
    if !applied.entries.is_empty() {
        hub.publish_assignment(machine, applied.snapshot());
    }
    AssignmentUpdate { assignment: applied.assignment, revision: applied.revision, entries: applied.entries }
}

async fn log(State(hub): HubState, _: Caller, Query(q): Query<LogQuery>) -> Result<Json<Vec<LogEntry>>> {
    Ok(Json(hub.db(move |conn| db::log_entries(conn, &q)).await?))
}

/// Restores an entry's `before` as a new entry. A no-op (empty `entries`) if
/// the toggle already has that value.
async fn undo(State(hub): HubState, caller: Caller, Path(id): Path<i64>) -> Result<Json<AssignmentUpdate>> {
    let (author, now, items) = (caller.author().to_string(), now(), hub.baseline_items());
    let (machine, applied) = hub
        .db(move |conn| {
            let tx = conn.transaction()?;
            let (machine, change) = assign::undo_change(&tx, id)?;
            let gone = || ApiError::Conflict(format!("machine `{machine}` is no longer in the fleet"));
            let m = db::machine(&tx, &machine)?.ok_or_else(gone)?;
            let report = m.report.unwrap_or_default();
            let change = vec![change];
            assign::check_items(&change, &items, &report.drafts, &report.unpushed)?;
            let applied = assign::apply(&tx, &machine, &author, change, now, Some(&items))?;
            tx.commit()?;
            Ok((machine, applied))
        })
        .await?;
    Ok(Json(published(&hub, &machine, applied)))
}

/// Revokes the machine's token and deletes its assignment (logged). Its files
/// are untouched; its daemon stops managing them when told `Revoked`.
async fn remove_machine(State(hub): HubState, caller: Caller, Path(id): Path<String>) -> Result<StatusCode> {
    let (machine, author, now) = (id.clone(), caller.author().to_string(), now());
    hub.db(move |conn| assign::remove_machine(conn, &machine, &author, now)).await?;
    hub.revoke(&id);
    tracing::info!("{} removed machine {id}", caller.author());
    Ok(StatusCode::NO_CONTENT)
}

async fn imports(State(hub): HubState, _: Caller) -> Json<ImportsView> {
    Json(lock(&hub.imports).clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_codes_normalize() {
        let code = new_join_code();
        assert_eq!(code.len(), 11);
        assert_eq!(normalize_code(&code.to_lowercase().replace('-', " ")), code.replace('-', ""));
        assert!(code.chars().all(|c| c == '-' || CODE_ALPHABET.contains(&(c as u8))));
    }
}
