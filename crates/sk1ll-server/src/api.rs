//! REST API under `/api`. Everything except `POST /join` needs
//! `Authorization: Bearer <token>` (a machine token or the admin token).
//! Every caller may read the fleet view, assignments, the log and imports.
//! Changes need a scope (ADR 0005): the admin token and machines that joined
//! with a `fleet` code may manage the whole fleet; a `self` machine may only
//! change its own assignment, undo log entries for itself and remove itself.
//! Each write checks the caller's token and scope again inside its database
//! transaction, so a request that was authenticated before its machine was
//! removed or joined again with another scope can't act on the old one.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{FromRequestParts, Path, Query, State};
use axum::http::StatusCode;
use axum::http::request::Parts;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use rusqlite::Connection;
use sk1ll_proto::{
    Assignment, AssignmentChanges, AssignmentCopied, AssignmentCopy, AssignmentCopyPreview, AssignmentSnapshot,
    AssignmentUpdate, Expected, FleetView, ImportsView, JoinCode, JoinCodeRequest, JoinRequest, JoinResponse, LogCause,
    LogEntry, LogFacets, LogQuery, MachineAssignmentCopy, MachineRow, REPO_PATH, ToggleChange, headline,
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
        .route("/machines/{id}/assignment/copy", post(copy_assignment))
        .route("/machines/{id}/assignment/harness/{harness}", post(set_harness_assignment))
        .route("/machines/{id}/assignment/copy-from", post(copy_from))
        .route("/machines/{id}/assignment/copy-from/preview", post(preview_copy_from))
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

/// Body: a [`JoinCodeRequest`], or nothing (or `null`, which older daemons
/// send) for a `fleet` code.
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
    Ok((StatusCode::CREATED, Json(JoinCode { code, expires_at: Some(expires_at), scope: Some(scope) })))
}

async fn join(State(server): ServerState, Json(req): Json<JoinRequest>) -> Result<(StatusCode, Json<JoinResponse>)> {
    check_name("machine id", &req.machine_id)?;
    if req.machine_id == assign::SERVER_AUTHOR || req.machine_id == "admin" {
        return Err(ApiError::BadRequest(format!("`{}` is reserved", req.machine_id)));
    }
    for h in &req.harnesses {
        check_name("harness", h)?;
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
                None => Assignment::default(),
            };
            for h in &req.harnesses {
                assignment.harnesses.entry(h.clone()).or_insert_with(assign::all_off);
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
                registration: Some(m.registration),
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

async fn put_assignment(
    State(server): ServerState,
    caller: Caller,
    Path(id): Path<String>,
    Json(body): Json<AssignmentChanges>,
) -> Result<Json<AssignmentUpdate>> {
    caller.require_machine(&id)?;
    let AssignmentChanges { changes, expected } = body;
    let changes: Vec<Change> = changes
        .into_iter()
        .map(|c| Change { harness: c.harness, kind: c.kind, name: c.name, after: c.enabled, cause: LogCause::Change })
        .collect();
    changes.iter().try_for_each(assign::validate)?;
    let (machine, author, now, items) = (id.clone(), caller.author().to_string(), now(), server.baseline_items());
    let applied = server
        .db(move |conn| {
            let tx = conn.transaction()?;
            authorize_assignment(&tx, &caller, &machine)?;
            let m = db::machine(&tx, &machine)?.ok_or_else(|| ApiError::NotFound(format!("no machine `{machine}`")))?;
            if let Some(expected) = &expected {
                assign::check_expected(&tx, &m, expected)?;
            }
            let report = m.report.unwrap_or_default();
            assign::check_items(&changes, &items, &report.drafts, &report.unpushed)?;
            let applied = assign::apply(&tx, &machine, &author, changes, now, Some(&items))?;
            tx.commit()?;
            Ok(applied)
        })
        .await?;
    Ok(Json(published(&server, &id, applied)))
}

async fn set_harness_assignment(
    State(server): ServerState,
    caller: Caller,
    Path((machine, harness)): Path<(String, String)>,
    Json(body): Json<sk1ll_proto::SetHarnessAssignment>,
) -> Result<Json<sk1ll_proto::HarnessAssignmentResult>> {
    caller.require_machine(&machine)?;
    check_name("harness", &harness)?;
    let push = server.push_lock.clone().lock_owned().await;
    let items = assignment_inventory(&server).await?;
    let id = machine.clone();
    let (applied, skipped) = server
        .db(move |conn| {
            let _push = push;
            let tx = conn.transaction()?;
            authorize_assignment(&tx, &caller, &machine)?;
            let m = db::machine(&tx, &machine)?.ok_or_else(|| ApiError::NotFound(format!("no machine `{machine}`")))?;
            let report = m.report.unwrap_or_default();
            let known: std::collections::BTreeSet<_> = items
                .iter()
                .cloned()
                .chain(report.drafts)
                .chain(report.unpushed)
                .filter(|item| matches!(item, sk1ll_proto::ItemRef::Skill(_) | sk1ll_proto::ItemRef::Rule(_)))
                .collect();
            let (eligible, skipped): (Vec<_>, Vec<_>) = known.into_iter().partition(|item| match item {
                sk1ll_proto::ItemRef::Skill(name) | sk1ll_proto::ItemRef::Rule(name) => {
                    check_name("item name", name).is_ok()
                }
                _ => false,
            });
            let changes = body
                .changes(&harness, eligible)
                .changes
                .into_iter()
                .map(|c| Change {
                    harness: c.harness,
                    kind: c.kind,
                    name: c.name,
                    after: c.enabled,
                    cause: LogCause::Change,
                })
                .collect();
            let applied = assign::apply(&tx, &machine, caller.author(), changes, now(), None)?;
            tx.commit()?;
            Ok((applied, skipped))
        })
        .await?;
    Ok(Json(sk1ll_proto::HarnessAssignmentResult { update: published(&server, &id, applied), skipped }))
}

async fn assignment_inventory(server: &Server) -> Result<Arc<std::collections::BTreeSet<sk1ll_proto::ItemRef>>> {
    server
        .live_items()
        .await
        .ok_or_else(|| ApiError::Unavailable("could not read the current baseline inventory; retry shortly".into()))
}

fn plan_copy(
    tx: &Connection,
    caller: &Caller,
    target: &str,
    body: &MachineAssignmentCopy,
) -> Result<(Vec<Change>, AssignmentCopyPreview)> {
    authorize_assignment(tx, caller, target)?;
    let load = |id: &str, expected: Option<&Expected>| -> Result<_> {
        let m = db::machine(tx, id)?.ok_or_else(|| ApiError::NotFound(format!("no machine `{id}`")))?;
        if let Some(expected) = expected {
            assign::check_expected(tx, &m, expected)?;
        }
        let snap = db::snapshot(tx, id)?.expect("registered machine has assignment");
        Ok((m.assignment, Expected { revision: snap.revision, registration: Some(m.registration) }))
    };
    let (destination, target_state) = load(target, body.expected.as_ref())?;
    let (source, source_state) = load(&body.source, body.source_expected.as_ref())?;
    let changes = assign::machine_copy_changes(&source, &destination, body)?;
    let preview = AssignmentCopyPreview { count: changes.len(), target: target_state, source: source_state };
    Ok((changes, preview))
}

async fn preview_copy_from(
    State(server): ServerState,
    caller: Caller,
    Path(id): Path<String>,
    Json(body): Json<MachineAssignmentCopy>,
) -> Result<Json<AssignmentCopyPreview>> {
    caller.require_machine(&id)?;
    server
        .db(move |conn| {
            let tx = conn.transaction()?;
            let (_, preview) = plan_copy(&tx, &caller, &id, &body)?;
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
    let machine = id.clone();
    let applied = server
        .db(move |conn| {
            let tx = conn.transaction()?;
            let (changes, preview) = plan_copy(&tx, &caller, &machine, &body)?;
            let registration = preview.target.registration.expect("registered Machine");
            let applied = assign::apply(&tx, &machine, caller.author(), changes, now(), None)?;
            for entry in &applied.entries {
                db::mark_machine_copy(&tx, entry.id, &registration)?;
            }
            tx.commit()?;
            Ok(applied)
        })
        .await?;
    Ok(Json(published(&server, &id, applied)))
}

/// Copies a machine's explicit toggles of one item to another, before a
/// rename reaches its Draft, in one transaction. A toggle the new name
/// already has is kept; the toggles copied are logged like any change.
async fn copy_assignment(
    State(server): ServerState,
    caller: Caller,
    Path(id): Path<String>,
    Json(copy): Json<AssignmentCopy>,
) -> Result<Json<AssignmentCopied>> {
    caller.require_machine(&id)?;
    let push = server.push_lock.clone().lock_owned().await;
    let items = assignment_inventory(&server).await?;
    let (machine, author, now) = (id.clone(), caller.author().to_string(), now());
    let (applied, registration, copied, skipped) = server
        .db(move |conn| {
            // The database task can outlive a cancelled HTTP request.
            let _push = push;
            let tx = conn.transaction()?;
            authorize_assignment(&tx, &caller, &machine)?;
            let m = db::machine(&tx, &machine)?.ok_or_else(|| ApiError::NotFound(format!("no machine `{machine}`")))?;
            let (changes, skipped) = assign::copy_changes(&m, &copy, &items)?;
            let applied = assign::apply(&tx, &machine, &author, changes.clone(), now, None)?;
            tx.commit()?;
            Ok((applied, m.registration, changes, skipped))
        })
        .await?;
    let toggle = |c: Change| ToggleChange { harness: c.harness, kind: c.kind, name: c.name, enabled: c.after };
    let update = published(&server, &id, applied);
    Ok(Json(AssignmentCopied {
        assignment: update.assignment,
        revision: update.revision,
        registration: Some(registration),
        entries: update.entries,
        applied: copied.into_iter().map(toggle).collect(),
        skipped: skipped.into_iter().map(toggle).collect(),
    }))
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
    let (author, now, items) = (caller.author().to_string(), now(), server.baseline_items());
    let (machine, applied) = server
        .db(move |conn| {
            let tx = conn.transaction()?;
            let caller = caller.current(&tx)?;
            let (machine, change) = assign::undo_change(&tx, id, |machine| caller.require_machine(machine))?;
            let gone = || ApiError::Conflict(format!("machine `{machine}` is no longer in the fleet"));
            let m = db::machine(&tx, &machine)?.ok_or_else(gone)?;
            let report = m.report.unwrap_or_default();
            let change = vec![change];
            let copy_registration = db::copy_registration(&tx, id)?;
            if copy_registration.as_ref().is_some_and(|r| *r != m.registration) {
                return Err(ApiError::Conflict(format!("machine `{machine}` joined the fleet again since this copy")));
            }
            let copy = copy_registration.is_some();
            if !copy {
                assign::check_items(&change, &items, &report.drafts, &report.unpushed)?;
            }
            let applied = assign::apply(&tx, &machine, &author, change, now, (!copy).then_some(&items))?;
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

#[cfg(test)]
mod tests {
    use super::*;

    use sk1ll_proto::{Scope, ToggleChange, ToggleKind};

    use crate::Config;

    /// Joins `id` with a fresh code of `scope`; returns its token.
    async fn join_with(server: &Arc<Server>, id: &str, scope: Scope) -> String {
        let code = new_join_code();
        let code_hash = hash(&normalize_code(&code));
        server.db(move |conn| db::insert_join_code(conn, &code_hash, "admin", now(), scope)).await.unwrap();
        let req = JoinRequest { code, machine_id: id.into(), harnesses: vec!["claude".into()], copy_from: None };
        join(State(server.clone()), Json(req)).await.unwrap().1.0.token
    }

    fn push_items(server: &Server, directory: &std::path::Path, names: &[&str]) {
        let work = directory.join("work");
        let git = |path: &std::path::Path, args: &[&str]| {
            let output = std::process::Command::new("git")
                .arg("-C")
                .arg(path)
                .args(["-c", "user.name=test", "-c", "user.email=test@test", "-c", "commit.gpgsign=false"])
                .args(args)
                .output()
                .unwrap();
            assert!(output.status.success(), "{args:?}: {}", String::from_utf8_lossy(&output.stderr));
        };
        git(directory, &["clone", server.config.repo_dir().to_str().unwrap(), "work"]);
        for name in names {
            let path = work.join("personal").join(name);
            std::fs::create_dir_all(&path).unwrap();
            std::fs::write(path.join("SKILL.md"), "skill").unwrap();
        }
        git(&work, &["add", "--all"]);
        git(&work, &["commit", "-m", "add skills"]);
        git(&work, &["push", "origin", "HEAD:main"]);
    }

    #[tokio::test]
    async fn copy_checks_baseline_membership_after_an_in_progress_push() {
        use sk1ll_proto::ItemRef;

        let dir = tempfile::tempdir().unwrap();
        let server = Server::open(Config::new(dir.path().join("data"))).await.unwrap();
        join_with(&server, "me", Scope::Fleet).await;
        let from = ItemRef::Skill("pdf".into());
        let to = ItemRef::Skill("docs".into());

        let push = server.push_lock.lock().await;
        let copy = copy_assignment(
            State(server.clone()),
            Caller::Admin,
            Path("me".into()),
            Json(AssignmentCopy { from: from.clone(), to: to.clone() }),
        );
        tokio::pin!(copy);
        assert!(futures_util::poll!(&mut copy).is_pending());

        // Publish the pushed tree before releasing the lock. Both items
        // now exist, so the established destination cannot be overwritten.
        crate::lock(&server.baseline).inventory = None;
        push_items(&server, dir.path(), &["pdf", "docs"]);
        drop(push);
        let result = copy.await;
        assert!(matches!(result, Err(ApiError::Conflict(_))), "{result:?}");
    }

    #[tokio::test]
    async fn harness_bulk_assignment_includes_unpulled_baseline_and_reported_items() {
        use sk1ll_proto::{ItemRef, MachineReport, SetHarnessAssignment};

        let dir = tempfile::tempdir().unwrap();
        let server = Server::open(Config::new(dir.path().join("data"))).await.unwrap();
        let token = join_with(&server, "me", Scope::SelfOnly).await;
        join_with(&server, "other", Scope::Fleet).await;
        let caller = server.authenticate(&token).await.unwrap();
        let body = Json(SetHarnessAssignment { enabled: false });
        let path = |id: &str| Path((id.into(), "claude".into()));
        assert!(matches!(
            set_harness_assignment(State(server.clone()), caller.clone(), path("other"), body).await,
            Err(ApiError::Forbidden(_))
        ));

        let on = set_harness_assignment(
            State(server.clone()),
            caller.clone(),
            path("me"),
            Json(SetHarnessAssignment { enabled: true }),
        )
        .await
        .unwrap()
        .0;
        assert!(!on.update.assignment.harness("claude").skills.contains_key("unpulled"));
        push_items(&server, dir.path(), &["unpulled", "team notes"]);
        server.fail_listing.store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(matches!(
            set_harness_assignment(
                State(server.clone()),
                caller.clone(),
                path("me"),
                Json(SetHarnessAssignment { enabled: false })
            )
            .await,
            Err(ApiError::Unavailable(_))
        ));
        let copy = AssignmentCopy { from: ItemRef::Skill("unpulled".into()), to: ItemRef::Skill("new".into()) };
        assert!(matches!(
            copy_assignment(State(server.clone()), caller.clone(), Path("me".into()), Json(copy.clone())).await,
            Err(ApiError::Unavailable(_))
        ));
        server.fail_listing.store(false, std::sync::atomic::Ordering::SeqCst);
        let _ = copy_assignment(State(server.clone()), caller.clone(), Path("me".into()), Json(copy)).await.unwrap();
        server
            .db(|conn| {
                db::save_report(
                    conn,
                    "me",
                    &MachineReport {
                        drafts: vec![ItemRef::Skill("draft".into()), ItemRef::Skill("draft notes".into())],
                        unpushed: vec![ItemRef::Rule("local".into())],
                        ..Default::default()
                    },
                    now(),
                )
            })
            .await
            .unwrap();
        let off = set_harness_assignment(
            State(server.clone()),
            caller,
            path("me"),
            Json(SetHarnessAssignment { enabled: false }),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(off.skipped, [ItemRef::Skill("draft notes".into()), ItemRef::Skill("team notes".into())]);
        let off = off.update;
        assert_eq!(off.revision, on.update.revision + 1);
        let harness = off.assignment.harness("claude");
        assert!(!harness.skills["unpulled"]);
        assert!(!harness.skills["draft"]);
        assert!(!harness.rules["local"]);
        assert!(!harness.new_skills_default && !harness.new_rules_default);
        assert_eq!(off.entries.len(), 5);
        assert!(off.entries.iter().all(|entry| entry.author == "me"
            && entry.machine == "me"
            && entry.before == if entry.kind.has_name() { None } else { Some(true) }
            && entry.after == Some(false)
            && entry.cause == LogCause::Change));
        let entries = server.db(|conn| db::log_entries(conn, &LogQuery::default())).await.unwrap();
        assert!(off.entries.iter().all(|entry| entries.iter().any(|stored| stored.id == entry.id)));
    }

    /// Review finding: a request authenticated as a `fleet` machine, then
    /// paused while that machine is removed and joins again as `self`, must
    /// not write with the old scope or token.
    #[tokio::test]
    async fn writes_recheck_the_callers_token_and_scope() {
        let dir = tempfile::tempdir().unwrap();
        let server = Server::open(Config::new(dir.path().join("data"))).await.unwrap();
        let token = join_with(&server, "me", Scope::Fleet).await;
        join_with(&server, "other", Scope::Fleet).await;
        let other_entry = put_assignment(
            State(server.clone()),
            Caller::Admin,
            Path("other".into()),
            Json(AssignmentChanges {
                changes: vec![ToggleChange {
                    harness: "claude".into(),
                    kind: ToggleKind::NewSkillsDefault,
                    name: None,
                    enabled: Some(true),
                }],
                expected: None,
            }),
        )
        .await
        .unwrap()
        .0
        .entries[0]
            .id;
        // Authenticated now, as each handler's extractor does ...
        let stale = server.authenticate(&token).await.unwrap();
        assert!(matches!(&stale, Caller::Machine { scope: Scope::Fleet, .. }));
        // ... then the machine is removed and joins again with a self code.
        let (now, removed) = (now(), "me".to_string());
        server.db(move |conn| assign::remove_machine(conn, &removed, "admin", now)).await.unwrap();
        let rejoined = join_with(&server, "me", Scope::SelfOnly).await;
        let fresh = server.authenticate(&rejoined).await.unwrap();

        let change = || {
            Json(AssignmentChanges {
                changes: vec![ToggleChange {
                    harness: "claude".into(),
                    kind: ToggleKind::NewRulesDefault,
                    name: None,
                    enabled: Some(true),
                }],
                expected: None,
            })
        };
        let copy = || {
            Json(AssignmentCopy {
                from: sk1ll_proto::ItemRef::Skill("pdf".into()),
                to: sk1ll_proto::ItemRef::Skill("pdf-tools".into()),
            })
        };
        let unauthorized = |r: Result<()>| assert!(matches!(r, Err(ApiError::Unauthorized)), "{r:?}");
        let forbidden = |r: Result<()>| assert!(matches!(r, Err(ApiError::Forbidden(_))), "{r:?}");
        let s = || State(server.clone());
        // The old token is gone, whatever it asks for.
        unauthorized(create_join_code(s(), stale.clone(), Bytes::new()).await.map(drop));
        unauthorized(put_assignment(s(), stale.clone(), Path("other".into()), change()).await.map(drop));
        unauthorized(copy_assignment(s(), stale.clone(), Path("other".into()), copy()).await.map(drop));
        unauthorized(undo(s(), stale.clone(), Path(other_entry)).await.map(drop));
        unauthorized(remove_machine(s(), stale.clone(), Path("other".into())).await.map(drop));
        // A fleet-scoped caller struct carrying the new token still gets the
        // new, narrower scope from the database.
        let Caller::Machine { id, token_hash, .. } = fresh else { panic!("{fresh:?}") };
        let widened = Caller::Machine { id, scope: Scope::Fleet, token_hash };
        forbidden(create_join_code(s(), widened.clone(), Bytes::new()).await.map(drop));
        forbidden(put_assignment(s(), widened.clone(), Path("other".into()), change()).await.map(drop));
        forbidden(copy_assignment(s(), widened.clone(), Path("other".into()), copy()).await.map(drop));
        forbidden(undo(s(), widened.clone(), Path(other_entry)).await.map(drop));
        forbidden(remove_machine(s(), widened.clone(), Path("other".into())).await.map(drop));
        let (other, entries) = server
            .db(|conn| {
                Ok((db::machine(conn, "other")?.map(|m| m.assignment), db::log_entries(conn, &LogQuery::default())?))
            })
            .await
            .unwrap();
        assert!(!other.unwrap().harness("claude").new_rules_default);
        assert!(entries.iter().all(|e| e.machine != "other" || e.kind != ToggleKind::NewRulesDefault), "{entries:?}");
        // Its own assignment is still its to change: the copy gets past the
        // scope check and only fails on the unknown item.
        let own = copy_assignment(s(), widened.clone(), Path("me".into()), copy()).await;
        assert!(matches!(own, Err(ApiError::Unprocessable(_))), "{own:?}");
        assert_eq!(put_assignment(s(), widened, Path("me".into()), change()).await.unwrap().entries.len(), 1);
    }

    #[test]
    fn join_codes_normalize() {
        let code = new_join_code();
        assert_eq!(code.len(), 11);
        assert_eq!(normalize_code(&code.to_lowercase().replace('-', " ")), code.replace('-', ""));
        assert!(code.chars().all(|c| c == '-' || CODE_ALPHABET.contains(&(c as u8))));
    }
}
