//! SQLite storage: machines (token hash, last report, assignment as JSON),
//! join codes (hashed, single use) and the append-only assignment log.

use std::path::Path;

use anyhow::Context;
use rusqlite::{Connection, OptionalExtension, Row, params};
use sk1ll_proto::{Assignment, AssignmentSnapshot, LogCause, LogEntry, LogQuery, MachineReport, ToggleKind};

use crate::error::Result;

/// Each entry upgrades the schema by one `user_version`. Append only.
const MIGRATIONS: &[&str] = &[
    "
    CREATE TABLE machines (
        id          TEXT PRIMARY KEY,
        token_hash  TEXT NOT NULL UNIQUE,
        joined_at   INTEGER NOT NULL,
        last_seen   INTEGER,
        report      TEXT,
        assignment  TEXT NOT NULL
    );
    CREATE TABLE join_codes (
        code_hash   TEXT PRIMARY KEY,
        created_by  TEXT NOT NULL,
        created_at  INTEGER NOT NULL,
        used_by     TEXT,
        used_at     INTEGER
    );
    CREATE TABLE log (
        id       INTEGER PRIMARY KEY AUTOINCREMENT,
        at       INTEGER NOT NULL,
        author   TEXT NOT NULL,
        machine  TEXT NOT NULL,
        harness  TEXT NOT NULL,
        kind     TEXT NOT NULL,
        name     TEXT,
        before   INTEGER,
        after    INTEGER,
        cause    TEXT NOT NULL
    );
    CREATE INDEX log_machine ON log (machine, id);
",
    "
    ALTER TABLE machines ADD COLUMN revision INTEGER NOT NULL DEFAULT 0;
    CREATE TABLE counters (
        name   TEXT PRIMARY KEY,
        value  INTEGER NOT NULL
    );
    INSERT INTO counters (name, value) VALUES ('revision', 0);
",
];

pub fn open(path: &Path) -> anyhow::Result<Connection> {
    let mut conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    migrate(&mut conn)?;
    Ok(conn)
}

fn migrate(conn: &mut Connection) -> rusqlite::Result<()> {
    let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    for (i, sql) in MIGRATIONS.iter().enumerate().skip(version.max(0) as usize) {
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", i as i64 + 1)?;
        tx.commit()?;
    }
    Ok(())
}

pub struct MachineRecord {
    pub id: String,
    pub joined_at: i64,
    pub last_seen: Option<i64>,
    pub report: Option<MachineReport>,
    pub assignment: Assignment,
}

fn json<T: serde::de::DeserializeOwned>(text: &str, what: &str) -> Result<T> {
    Ok(serde_json::from_str(text).with_context(|| format!("corrupt {what} in hub.db"))?)
}

fn to_json<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_string(value).expect("serializing plain data")
}

const MACHINE_COLS: &str = "id, joined_at, last_seen, report, assignment";

type MachineRow = (String, i64, Option<i64>, Option<String>, String);

fn machine_row(row: &Row) -> rusqlite::Result<MachineRow> {
    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?))
}

fn machine_record((id, joined_at, last_seen, report, assignment): MachineRow) -> Result<MachineRecord> {
    Ok(MachineRecord {
        report: report.map(|r| json(&r, "report")).transpose()?,
        assignment: json(&assignment, "assignment")?,
        id,
        joined_at,
        last_seen,
    })
}

pub fn machines(conn: &Connection) -> Result<Vec<MachineRecord>> {
    let mut stmt = conn.prepare(&format!("SELECT {MACHINE_COLS} FROM machines ORDER BY id"))?;
    let rows = stmt.query_map([], machine_row)?.collect::<rusqlite::Result<Vec<_>>>()?;
    rows.into_iter().map(machine_record).collect()
}

pub fn machine(conn: &Connection, id: &str) -> Result<Option<MachineRecord>> {
    let row =
        conn.query_row(&format!("SELECT {MACHINE_COLS} FROM machines WHERE id = ?1"), [id], machine_row).optional()?;
    row.map(machine_record).transpose()
}

pub fn machine_for_token(conn: &Connection, token_hash: &str) -> Result<Option<String>> {
    Ok(conn.query_row("SELECT id FROM machines WHERE token_hash = ?1", [token_hash], |r| r.get(0)).optional()?)
}

/// Inserts a machine; returns its assignment's revision.
pub fn insert_machine(conn: &Connection, id: &str, token_hash: &str, now: i64, a: &Assignment) -> Result<u64> {
    let revision = next_revision(conn)?;
    conn.execute(
        "INSERT INTO machines (id, token_hash, joined_at, assignment, revision) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![id, token_hash, now, to_json(a), revision as i64],
    )?;
    Ok(revision)
}

/// The next value of the hub-wide assignment revision sequence. Hub-wide
/// rather than per machine so that a re-joined machine never goes back.
fn next_revision(conn: &Connection) -> Result<u64> {
    let v: i64 =
        conn.query_row("UPDATE counters SET value = value + 1 WHERE name = 'revision' RETURNING value", [], |r| {
            r.get(0)
        })?;
    Ok(v as u64)
}

pub fn assignment(conn: &Connection, id: &str) -> Result<Option<Assignment>> {
    Ok(snapshot(conn, id)?.map(|s| s.assignment))
}

pub fn snapshot(conn: &Connection, id: &str) -> Result<Option<AssignmentSnapshot>> {
    let row: Option<(String, i64)> = conn
        .query_row("SELECT assignment, revision FROM machines WHERE id = ?1", [id], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?;
    row.map(|(t, revision)| {
        Ok(AssignmentSnapshot { revision: revision.max(0) as u64, assignment: json(&t, "assignment")? })
    })
    .transpose()
}

/// Stores `a` under a new revision and returns it.
pub fn save_assignment(conn: &Connection, id: &str, a: &Assignment) -> Result<u64> {
    let revision = next_revision(conn)?;
    conn.execute(
        "UPDATE machines SET assignment = ?2, revision = ?3 WHERE id = ?1",
        params![id, to_json(a), revision as i64],
    )?;
    Ok(revision)
}

pub fn save_report(conn: &Connection, id: &str, report: &MachineReport, now: i64) -> Result<()> {
    conn.execute("UPDATE machines SET report = ?2, last_seen = ?3 WHERE id = ?1", params![id, to_json(report), now])?;
    Ok(())
}

/// Sets `last_seen`, but only while `token_hash` is still the machine's
/// token, so a revoked connection can't touch a re-joined machine.
pub fn touch(conn: &Connection, id: &str, token_hash: &str, now: i64) -> Result<()> {
    conn.execute("UPDATE machines SET last_seen = ?3 WHERE id = ?1 AND token_hash = ?2", params![id, token_hash, now])?;
    Ok(())
}

pub fn delete_machine(conn: &Connection, id: &str) -> Result<()> {
    conn.execute("DELETE FROM machines WHERE id = ?1", [id])?;
    Ok(())
}

pub fn insert_join_code(conn: &Connection, code_hash: &str, created_by: &str, now: i64) -> Result<()> {
    conn.execute(
        "INSERT INTO join_codes (code_hash, created_by, created_at) VALUES (?1, ?2, ?3)",
        params![code_hash, created_by, now],
    )?;
    Ok(())
}

/// Marks an unused code as used by `machine`; false if it doesn't exist or was used.
pub fn consume_join_code(conn: &Connection, code_hash: &str, machine: &str, now: i64) -> Result<bool> {
    let n = conn.execute(
        "UPDATE join_codes SET used_by = ?2, used_at = ?3 WHERE code_hash = ?1 AND used_at IS NULL",
        params![code_hash, machine, now],
    )?;
    Ok(n == 1)
}

fn kind_str(kind: ToggleKind) -> &'static str {
    match kind {
        ToggleKind::Skill => "skill",
        ToggleKind::Fragment => "fragment",
        ToggleKind::NewSkillsDefault => "new_skills_default",
        ToggleKind::NewFragmentsDefault => "new_fragments_default",
    }
}

fn parse_kind(s: &str) -> Result<ToggleKind> {
    Ok(serde_json::from_value(serde_json::Value::String(s.into())).context("corrupt log kind in hub.db")?)
}

/// Appends `entry` (its `id` is ignored) and sets its new id.
pub fn append_log(conn: &Connection, entry: &mut LogEntry) -> Result<()> {
    conn.execute(
        "INSERT INTO log (at, author, machine, harness, kind, name, before, after, cause)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            entry.at,
            entry.author,
            entry.machine,
            entry.harness,
            kind_str(entry.kind),
            entry.name,
            entry.before,
            entry.after,
            to_json(&entry.cause),
        ],
    )?;
    entry.id = conn.last_insert_rowid();
    Ok(())
}

const LOG_COLS: &str = "id, at, author, machine, harness, kind, name, before, after, cause";

type LogRow = (i64, i64, String, String, String, String, Option<String>, Option<bool>, Option<bool>, String);

fn log_row(r: &Row) -> rusqlite::Result<LogRow> {
    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?, r.get(9)?))
}

fn log_entry_from((id, at, author, machine, harness, kind, name, before, after, cause): LogRow) -> Result<LogEntry> {
    Ok(LogEntry {
        id,
        at,
        author,
        machine,
        harness,
        kind: parse_kind(&kind)?,
        name,
        before,
        after,
        cause: json::<LogCause>(&cause, "log cause")?,
    })
}

/// Newest first.
pub fn log_entries(conn: &Connection, q: &LogQuery) -> Result<Vec<LogEntry>> {
    let limit = q.limit.unwrap_or(100).clamp(1, 1000);
    let mut stmt = conn.prepare(&format!(
        "SELECT {LOG_COLS} FROM log
         WHERE (?1 IS NULL OR machine = ?1) AND (?2 IS NULL OR id < ?2)
         ORDER BY id DESC LIMIT ?3"
    ))?;
    let rows = stmt.query_map(params![q.machine, q.before, limit], log_row)?.collect::<rusqlite::Result<Vec<_>>>()?;
    rows.into_iter().map(log_entry_from).collect()
}

pub fn log_entry(conn: &Connection, id: i64) -> Result<Option<LogEntry>> {
    let row = conn.query_row(&format!("SELECT {LOG_COLS} FROM log WHERE id = ?1"), [id], log_row).optional()?;
    row.map(log_entry_from).transpose()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrates_once_and_round_trips_log() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hub.db");
        drop(open(&path).unwrap());
        let conn = open(&path).unwrap();
        let v: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0)).unwrap();
        assert_eq!(v, MIGRATIONS.len() as i64);

        let mut e = LogEntry {
            id: 0,
            at: 1,
            author: "a".into(),
            machine: "m".into(),
            harness: "claude".into(),
            kind: ToggleKind::NewSkillsDefault,
            name: None,
            before: Some(true),
            after: None,
            cause: LogCause::Undo { entry: 3 },
        };
        append_log(&conn, &mut e).unwrap();
        assert_eq!(log_entry(&conn, e.id).unwrap(), Some(e.clone()));
        assert_eq!(log_entries(&conn, &LogQuery { machine: Some("other".into()), ..Default::default() }).unwrap(), []);
        assert_eq!(log_entries(&conn, &LogQuery::default()).unwrap(), [e]);
    }

    #[test]
    fn revisions_grow_across_machines_and_rejoins() {
        let dir = tempfile::tempdir().unwrap();
        let conn = open(&dir.path().join("hub.db")).unwrap();
        let a = Assignment::default();
        let r1 = insert_machine(&conn, "m", "t1", 0, &a).unwrap();
        let r2 = save_assignment(&conn, "m", &a).unwrap();
        assert!(r2 > r1);
        assert_eq!(snapshot(&conn, "m").unwrap().unwrap().revision, r2);
        delete_machine(&conn, "m").unwrap();
        let r3 = insert_machine(&conn, "m", "t2", 0, &a).unwrap();
        assert!(r3 > r2, "a re-joined machine never goes back");
    }
}
