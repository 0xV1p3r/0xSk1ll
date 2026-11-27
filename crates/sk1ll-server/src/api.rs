//! REST API under `/api`. Everything except `POST /join` needs
//! `Authorization: Bearer <token>` (a machine token or the admin token).
//! Every caller may read the fleet view, assignments, the log and imports.
//! Changes need a scope (ADR 0005): the admin token and machines that joined
//! with a `fleet` code may manage the whole fleet; a `self` machine may only
//! change its own assignment, undo log entries for itself and remove itself.
//! Each write checks the caller's token and scope again inside its database
//! transaction, so a request that was authenticated before its machine was
//! removed or joined again with another scope can't act on the old one.

use std::collections::BTreeSet;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{FromRequestParts, Path, Query, State};
use axum::http::StatusCode;
use axum::http::request::Parts;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use rusqlite::Connection;
use sk1ll_proto::{
    Assignment, AssignmentChanges, AssignmentCopyPreview, AssignmentSnapshot, AssignmentUpdate, AssignmentWrite,
    CopyChange, Expected, FleetView, HarnessAssignment, HarnessRename, ImportsView, ItemId, JoinCode, JoinCodeRequest,
    JoinRequest, JoinResponse, LogCause, LogEntry, LogFacets, LogQuery, MachineAssignmentCopy, MachineRow, Orphan,
    OrphansPruned, OrphansView, PruneOrphans, REPO_PATH, SetHarnessAssignment, headline,
};

use crate::assign::{self, Change};
use crate::error::{ApiError, Result};
use crate::{Caller, Server, check_name, db, hash, lock, now, random_bytes};

type ServerState = State<Arc<Server>>;

pub fn routes() -> Router<Arc<Server>> {
    Router::new()
        .route("/join", post(join))
        .route("/join-codes", post(create_join_code))
        .route("/fleet", get(fleet))
        .route("/machines/{id}", delete(remove_machine))
        .route("/machines/{id}/assignment", get(get_assignment).put(put_assignment))
        .route("/machines/{id}/assignment/harness/{harness}", post(set_harness_assignment))
        .route("/machines/{id}/assignment/rename-harness", post(rename_harness))
        .route("/machines/{id}/assignment/copy-from", post(copy_from))
        .route("/machines/{id}/assignment/copy-from/preview", post(preview_copy_from))
        .route("/assignment/orphans", get(orphans))
        .route("/assignment/orphans/prune", post(prune_orphans))
        .route("/log", get(log))
        .route("/log/facets", get(log_facets))
        .route("/log/{id}/undo", post(undo))
        .route("/imports", get(imports))
        .route("/ws", get(crate::ws::upgrade))
}

/// The bearer token from `Authorization`.
pub fn bearer(parts: &Parts) -> Option<&str> {
    let v = parts.headers.get(axum::http::header::AUTHORIZATION)?.to_str().ok()?;
    v.strip_prefix("Bearer ").or_else(|| v.strip_prefix("bearer ")).map(str::trim)
}

impl FromRequestParts<Arc<Server>> for Caller {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, server: &Arc<Server>) -> Result<Self> {
        server.authenticate(bearer(parts).ok_or(ApiError::Unauthorized)?).await
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

/// The code lifetime in seconds.
fn join_code_ttl(server: &Server) -> i64 {
    server.config.join_code_ttl.as_secs().min(i64::MAX as u64) as i64
}

/// Body: a [`JoinCodeRequest`], or nothing (or `null`) for a `fleet` code.
async fn create_join_code(
    State(server): ServerState,
    caller: Caller,
    body: Bytes,
) -> Result<(StatusCode, Json<JoinCode>)> {
    caller.require_fleet("create join codes")?;
    let req: JoinCodeRequest = match serde_json::from_slice::<Option<JoinCodeRequest>>(&body) {
        _ if body.iter().all(u8::is_ascii_whitespace) => JoinCodeRequest::default(),
        Ok(req) => req.unwrap_or_default(),
        Err(e) => return Err(ApiError::BadRequest(format!("invalid join code request: {e}"))),
    };
    let code = new_join_code();
    let (code_hash, author, now, scope) = (hash(&normalize_code(&code)), caller.author().to_string(), now(), req.scope);
    let check = caller.clone();
    server
        .db(move |conn| {
            check.current(conn)?.require_fleet("create join codes")?;
            db::insert_join_code(conn, &code_hash, &author, now, scope)
        })
        .await?;
    tracing::info!("{} created a join code with scope {}", caller.author(), scope.as_str());
    let expires_at = now.saturating_add(join_code_ttl(&server));
    Ok((StatusCode::CREATED, Json(JoinCode { code, expires_at, scope })))
}

async fn join(State(server): ServerState, Json(req): Json<JoinRequest>) -> Result<(StatusCode, Json<JoinResponse>)> {
    check_name("machine id", &req.machine_id)?;
    if req.machine_id == assign::SERVER_AUTHOR || req.machine_id == "admin" {
        return Err(ApiError::BadRequest(format!("`{}` is reserved", req.machine_id)));
    }
    for h in &req.harnesses {
        check_name("harness", &h.name)?;
    }
    let token = crate::hex(&random_bytes::<32>());
    let token_hash = hash(&token);
    let ttl = join_code_ttl(&server);
    let req2 = req.clone();
    let (assignment, revision, scope) = server
        .db(move |conn| {
            let req = req2;
            let tx = conn.transaction()?;
            // The code first, so callers without one learn nothing about machine ids;
            // any later refusal rolls its consumption back. A rejoined machine
            // (removed, then joined again) gets the new code's scope.
            let now = now();
            let Some(scope) = db::consume_join_code(&tx, &hash(&normalize_code(&req.code)), &req.machine_id, now, ttl)?
            else {
                return Err(ApiError::Forbidden("invalid, expired or already used join code".into()));
            };
            let mut assignment = match &req.copy_from {
                Some(src) => db::assignment(&tx, src)?
                    .ok_or_else(|| ApiError::BadRequest(format!("copy_from: no machine `{src}`")))?,
                None => Assignment::fleet(),
            };
            for h in &req.harnesses {
                assignment.harnesses.entry(h.name.clone()).or_insert_with(HarnessAssignment::all_off);
            }
            if db::machine(&tx, &req.machine_id)?.is_some() {
                return Err(ApiError::Conflict(format!("machine `{}` already joined", req.machine_id)));
            }
            let revision = db::insert_machine(&tx, &req.machine_id, &token_hash, now, &assignment, scope)?;
            tx.commit()?;
            Ok((assignment, revision, scope))
        })
        .await?;
    tracing::info!("machine {} joined with scope {}", req.machine_id, scope.as_str());
    Ok((
        StatusCode::CREATED,
        Json(JoinResponse { machine_id: req.machine_id, token, repo_path: REPO_PATH.into(), assignment, revision }),
    ))
}

async fn fleet(State(server): ServerState, _: Caller) -> Result<Json<FleetView>> {
    let baseline = server.head();
    let records = server.db(|conn| db::machines(conn)).await?;
    let now = now();
    let offline_after = server.config.offline_after.as_secs() as i64;
    let machines = records
        .into_iter()
        .map(|m| {
            let connected = server.is_connected(&m.id);
            // By age alone: a registered but silent connection is Offline too.
            let offline = m.last_seen.is_none_or(|t| now - t > offline_after);
            MachineRow {
                status: headline(offline, m.report.as_ref(), baseline.as_deref()),
                connected,
                joined_at: m.joined_at,
                last_seen: m.last_seen,
                report: m.report,
                scope: m.scope,
                registration: m.registration,
                id: m.id,
            }
        })
        .collect();
    Ok(Json(FleetView { baseline, machines }))
}

async fn get_assignment(
    State(server): ServerState,
    _: Caller,
    Path(id): Path<String>,
) -> Result<Json<AssignmentSnapshot>> {
    let machine = id.clone();
    server
        .db(move |conn| {
            let registration = db::machine(conn, &machine)?.map(|m| m.registration);
            Ok(db::snapshot(conn, &machine)?.map(|s| AssignmentSnapshot { registration, ..s }))
        })
        .await?
        .map(Json)
        .ok_or_else(|| ApiError::NotFound(format!("no machine `{id}`")))
}

/// Whether `caller`, as the database has it now, may change `machine`'s
/// assignment (ADR 0005): the admin and `fleet` machines any, a `self`
/// machine only its own. Every assignment write calls it inside the
/// transaction that writes, so a token removed or a machine joined again
/// with another scope meanwhile can't use the old rights.
fn authorize_assignment(conn: &Connection, caller: &Caller, machine: &str) -> Result<()> {
    caller.current(conn)?.require_machine(machine)
}

/// Applies `write` to `machine`'s assignment for `caller`, in one
/// transaction that checks the caller's token and scope as they are now,
/// and hands the new assignment to the machine's session. Both the REST
/// endpoints and the machine's own session write through it.
pub(crate) async fn write_assignment(
    server: &Arc<Server>,
    caller: Caller,
    machine: String,
    write: AssignmentWrite,
    session: Option<u64>,
) -> Result<AssignmentUpdate> {
    caller.require_machine(&machine)?;
    let materializes =
        matches!(&write, AssignmentWrite::Changes(changes) if changes.changes.iter().any(|c| !c.kind.has_item()));
    let (push, baseline) = if materializes {
        let push = server.inventory_lock().await?;
        (Some(push), server.live_items().await?)
    } else if matches!(&write, AssignmentWrite::Changes(changes) if changes.changes.iter().any(|c| c.item_id.is_some() && c.name.is_none()))
    {
        (None, server.live_items().await.unwrap_or_else(|_| server.baseline_items()))
    } else {
        (None, server.baseline_items())
    };
    let (id, author, now, current) = (machine.clone(), caller.author().to_string(), now(), server.clone());
    let applied = server
        .db(move |conn| {
            let _push = push;
            if session.is_some_and(|generation| !current.is_current(&machine, generation)) {
                return Err(ApiError::Conflict("this session was replaced".into()));
            }
            let tx = conn.transaction()?;
            authorize_assignment(&tx, &caller, &machine)?;
            let m = db::machine(&tx, &machine)?.ok_or_else(|| ApiError::NotFound(format!("no machine `{machine}`")))?;
            if materializes && m.report.as_ref().is_some_and(|report| !report.inventory_complete) {
                return Err(ApiError::Unavailable("the Machine's inventory is incomplete".into()));
            }
            let applied = match write {
                AssignmentWrite::Changes(AssignmentChanges { changes, expected }) => {
                    let changes: Vec<Change> = changes.into_iter().map(Change::from_toggle).collect();
                    changes.iter().try_for_each(assign::validate)?;
                    if let Some(expected) = &expected {
                        assign::check_expected(&tx, &m, expected)?;
                    }
                    assign::apply(&tx, &machine, &author, changes, now, &baseline, true)?
                }
                AssignmentWrite::Harness { harness, enabled } => {
                    check_name("harness", &harness)?;
                    let changes = assign::harness_changes(&m.assignment, &harness, enabled);
                    assign::apply(&tx, &machine, &author, changes, now, &baseline, false)?
                }
                AssignmentWrite::RenameHarness(rename) => assign::rename_harness(&tx, &m, &author, rename, now)?,
            };
            tx.commit()?;
            Ok(applied)
        })
        .await?;
    Ok(published(server, &id, applied))
}

async fn put_assignment(
    State(server): ServerState,
    caller: Caller,
    Path(id): Path<String>,
    Json(body): Json<AssignmentChanges>,
) -> Result<Json<AssignmentUpdate>> {
    Ok(Json(write_assignment(&server, caller, id, AssignmentWrite::Changes(body), None).await?))
}

/// Sets both defaults and every toggle of one harness: every item, pulled
/// or not, gets that value there.
async fn set_harness_assignment(
    State(server): ServerState,
    caller: Caller,
    Path((machine, harness)): Path<(String, String)>,
    Json(body): Json<SetHarnessAssignment>,
) -> Result<Json<AssignmentUpdate>> {
    let write = AssignmentWrite::Harness { harness, enabled: body.enabled };
    Ok(Json(write_assignment(&server, caller, machine, write, None).await?))
}

async fn rename_harness(
    State(server): ServerState,
    caller: Caller,
    Path(machine): Path<String>,
    Json(body): Json<HarnessRename>,
) -> Result<Json<AssignmentUpdate>> {
    Ok(Json(write_assignment(&server, caller, machine, AssignmentWrite::RenameHarness(body), None).await?))
}

/// The changes copying `body.source`'s Assignment to `target` makes, and
/// their preview: each with its value before and after, and the item's name
/// from `baseline`, the two Machines' reports or the log.
fn plan_copy(
    tx: &Connection,
    caller: &Caller,
    target: &str,
    body: &MachineAssignmentCopy,
    baseline: &assign::Items,
) -> Result<(Vec<Change>, AssignmentCopyPreview)> {
    authorize_assignment(tx, caller, target)?;
    let mut reported = Vec::new();
    let mut load = |id: &str, expected: Option<&Expected>| -> Result<_> {
        let m = db::machine(tx, id)?.ok_or_else(|| ApiError::NotFound(format!("no machine `{id}`")))?;
        if let Some(expected) = expected {
            assign::check_expected(tx, &m, expected)?;
        }
        let snap = db::snapshot(tx, id)?.expect("registered machine has assignment");
        reported.extend(m.report.into_iter().flat_map(|r| r.items).map(|r| (r.item, r.id)));
        Ok((m.assignment, Expected { revision: snap.revision, registration: Some(m.registration) }))
    };
    let (destination, target_state) = load(target, body.expected.as_ref())?;
    let (source, source_state) = load(&body.source, body.source_expected.as_ref())?;
    let changes = assign::machine_copy_changes(&source, &destination, body)?;
    let name = |id: &ItemId| -> Result<Option<String>> {
        match baseline.iter().chain(&reported).find(|(_, known)| known == id) {
            Some((item, _)) => Ok(Some(item.name().to_string())),
            None => db::last_name(tx, id),
        }
    };
    let described = changes
        .iter()
        .map(|c| {
            Ok(CopyChange {
                harness: c.harness.clone(),
                kind: c.kind,
                item_id: c.item_id.clone(),
                name: c.item_id.as_ref().map(name).transpose()?.flatten(),
                before: assign::get(&destination, &c.harness, c.kind, c.item_id.as_ref()),
                after: c.after,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let preview =
        AssignmentCopyPreview { count: changes.len(), changes: described, target: target_state, source: source_state };
    Ok((changes, preview))
}

async fn preview_copy_from(
    State(server): ServerState,
    caller: Caller,
    Path(id): Path<String>,
    Json(body): Json<MachineAssignmentCopy>,
) -> Result<Json<AssignmentCopyPreview>> {
    caller.require_machine(&id)?;
    let baseline = server.baseline_items();
    server
        .db(move |conn| {
            let tx = conn.transaction()?;
            let (_, preview) = plan_copy(&tx, &caller, &id, &body, &baseline)?;
            Ok(Json(preview))
        })
        .await
}

async fn copy_from(
    State(server): ServerState,
    caller: Caller,
    Path(id): Path<String>,
    Json(body): Json<MachineAssignmentCopy>,
) -> Result<Json<AssignmentUpdate>> {
    caller.require_machine(&id)?;
    let (machine, baseline) = (id.clone(), server.baseline_items());
    let applied = server
        .db(move |conn| {
            let tx = conn.transaction()?;
            let (changes, preview) = plan_copy(&tx, &caller, &machine, &body, &baseline)?;
            let registration = preview.target.registration.expect("registered Machine");
            let applied = assign::apply(&tx, &machine, caller.author(), changes, now(), &baseline, false)?;
            for entry in &applied.entries {
                db::mark_machine_copy(&tx, entry.id, &registration)?;
            }
            tx.commit()?;
            Ok(applied)
        })
        .await?;
    Ok(Json(published(&server, &id, applied)))
}

/// Partial reports cannot be used to tell whether an ID is absent.
fn check_machine_inventories(machines: &[db::MachineRecord]) -> Result<()> {
    if let Some(machine) = machines.iter().find(|m| m.report.as_ref().is_some_and(|r| !r.inventory_complete)) {
        return Err(ApiError::Unavailable(format!("inventory for `{}` is incomplete", machine.id)));
    }
    Ok(())
}

/// The Baseline's IDs and every Machine's reported IDs.
fn known_ids(baseline: &[(sk1ll_proto::ItemRef, ItemId)], machines: &[db::MachineRecord]) -> BTreeSet<ItemId> {
    let reported = machines.iter().flat_map(|m| m.report.iter().flat_map(|r| r.items.iter().map(|i| i.id.clone())));
    baseline.iter().map(|(_, id)| id.clone()).chain(reported).collect()
}

/// Toggles for items in neither the baseline (read again, not cached) nor
/// any machine's last report.
async fn orphans(State(server): ServerState, _: Caller) -> Result<Json<OrphansView>> {
    let baseline = server.live_items().await?;
    let orphans = server
        .db(move |conn| {
            let machines = db::machines(conn)?;
            check_machine_inventories(&machines)?;
            let known = known_ids(&baseline, &machines);
            let known = known.iter().collect();
            let mut orphans = Vec::new();
            for m in &machines {
                for (harness, kind, item_id) in assign::orphans(&m.assignment, &known) {
                    orphans.push(Orphan {
                        machine: m.id.clone(),
                        harness: harness.into(),
                        kind,
                        item_id: item_id.clone(),
                        name: db::last_name(conn, item_id)?,
                    });
                }
            }
            Ok(orphans)
        })
        .await?;
    Ok(Json(OrphansView { orphans }))
}

/// Removes the orphans (logged): every machine's for the admin or a `fleet`
/// machine, only its own for a `self` machine, or one machine's. Holds the
/// push lock, so a push can't add an item between reading the baseline and
/// deciding what is orphaned.
async fn prune_orphans(State(server): ServerState, caller: Caller, body: Bytes) -> Result<Json<OrphansPruned>> {
    let body: PruneOrphans = match serde_json::from_slice::<Option<PruneOrphans>>(&body) {
        _ if body.iter().all(u8::is_ascii_whitespace) => PruneOrphans::default(),
        Ok(body) => body.unwrap_or_default(),
        Err(e) => return Err(ApiError::BadRequest(format!("invalid prune request: {e}"))),
    };
    let target = move |caller: &Caller| -> Result<Option<String>> {
        match (&body.machine, caller) {
            (Some(machine), _) => caller.require_machine(machine).map(|()| Some(machine.clone())),
            (None, Caller::Machine { id, scope: sk1ll_proto::Scope::SelfOnly, .. }) => Ok(Some(id.clone())),
            (None, _) => Ok(None),
        }
    };
    target.clone()(&caller)?;
    let push = server.inventory_lock().await?;
    let baseline = server.live_items().await?;
    let now = now();
    let (entries, updated) = server
        .db(move |conn| {
            let _push = push;
            let tx = conn.transaction()?;
            let caller = caller.current(&tx)?;
            let target = target(&caller)?;
            let machines = db::machines(&tx)?;
            check_machine_inventories(&machines)?;
            if let Some(t) = &target
                && !machines.iter().any(|m| m.id == *t)
            {
                return Err(ApiError::NotFound(format!("no machine `{t}`")));
            }
            let known = known_ids(&baseline, &machines);
            let known = known.iter().collect();
            let (mut entries, mut updated) = (Vec::new(), Vec::new());
            for m in machines.iter().filter(|m| target.as_ref().is_none_or(|t| *t == m.id)) {
                let mut changes = Vec::new();
                for (harness, kind, item_id) in assign::orphans(&m.assignment, &known) {
                    changes.push(Change {
                        harness: harness.into(),
                        kind,
                        item_id: Some(item_id.clone()),
                        name: db::last_name(&tx, item_id)?,
                        after: None,
                        cause: LogCause::OrphanPruned,
                    });
                }
                if changes.is_empty() {
                    continue;
                }
                let applied = assign::apply(&tx, &m.id, caller.author(), changes, now, &baseline, false)?;
                updated.push((m.id.clone(), applied.snapshot()));
                entries.extend(applied.entries);
            }
            tx.commit()?;
            Ok((entries, updated))
        })
        .await?;
    for (machine, snapshot) in updated {
        server.publish_assignment(&machine, snapshot);
    }
    Ok(Json(OrphansPruned { entries }))
}

/// Hands a changed assignment to the machine's session and builds the response.
fn published(server: &Server, machine: &str, applied: assign::Applied) -> AssignmentUpdate {
    if !applied.entries.is_empty() {
        server.publish_assignment(machine, applied.snapshot());
    }
    AssignmentUpdate {
        assignment: applied.assignment,
        revision: applied.revision,
        entries: applied.entries,
        errors: Vec::new(),
    }
}

async fn log(State(server): ServerState, _: Caller, Query(q): Query<LogQuery>) -> Result<Json<Vec<LogEntry>>> {
    Ok(Json(server.db(move |conn| db::log_entries(conn, &q)).await?))
}

async fn log_facets(State(server): ServerState, _: Caller) -> Result<Json<LogFacets>> {
    Ok(Json(server.db(|conn| db::log_facets(conn)).await?))
}

/// Restores an entry's `before` as a new entry. A no-op (empty `entries`) if
/// the toggle already has that value.
async fn undo(State(server): ServerState, caller: Caller, Path(id): Path<i64>) -> Result<Json<AssignmentUpdate>> {
    let materializes = server
        .db(move |conn| {
            let entry = db::log_entry(conn, id)?.ok_or_else(|| ApiError::NotFound(format!("no log entry {id}")))?;
            Ok(!entry.kind.has_item() && db::copy_registration(conn, id)?.is_none())
        })
        .await?;
    let (push, baseline) = if materializes {
        let push = server.inventory_lock().await?;
        (Some(push), server.live_items().await?)
    } else {
        (None, server.baseline_items())
    };
    let (author, now) = (caller.author().to_string(), now());
    let (machine, applied) = server
        .db(move |conn| {
            let _push = push;
            let tx = conn.transaction()?;
            let caller = caller.current(&tx)?;
            let (machine, change) = assign::undo_change(&tx, id, |machine| caller.require_machine(machine))?;
            let gone = || ApiError::Conflict(format!("machine `{machine}` is no longer in the fleet"));
            let m = db::machine(&tx, &machine)?.ok_or_else(gone)?;
            if materializes && m.report.as_ref().is_some_and(|report| !report.inventory_complete) {
                return Err(ApiError::Unavailable("the Machine's inventory is incomplete".into()));
            }
            let copy_registration = db::copy_registration(&tx, id)?;
            if copy_registration.as_ref().is_some_and(|r| *r != m.registration) {
                return Err(ApiError::Conflict(format!("machine `{machine}` joined the fleet again since this copy")));
            }
            let copy = copy_registration.is_some();
            let applied = assign::apply(&tx, &machine, &author, vec![change], now, &baseline, !copy)?;
            if copy {
                for entry in &applied.entries {
                    db::mark_machine_copy(&tx, entry.id, &m.registration)?;
                }
            }
            tx.commit()?;
            Ok((machine, applied))
        })
        .await?;
    Ok(Json(published(&server, &machine, applied)))
}

/// Revokes the machine's token and deletes its assignment (logged). Its files
/// are untouched; its daemon stops managing them when told `Revoked`.
/// A `self` machine may remove itself: leaving only touches its own
/// assignment and token, and coming back needs a code from a `fleet` caller.
async fn remove_machine(State(server): ServerState, caller: Caller, Path(id): Path<String>) -> Result<StatusCode> {
    caller.require_machine(&id)?;
    server.remove_machine(caller.clone(), id.clone()).await?;
    tracing::info!("{} removed machine {id}", caller.author());
    Ok(StatusCode::NO_CONTENT)
}

async fn imports(State(server): ServerState, _: Caller) -> Json<ImportsView> {
    Json(lock(&server.imports).clone())
}
