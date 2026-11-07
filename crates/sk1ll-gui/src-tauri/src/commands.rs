//! Commands use the Daemon's response types and never retry writes.
//! Draft entries also include byte fingerprints for confirmation review.
use serde::Serialize;
use sha2::{Digest, Sha256};
use sk1ll_daemon::client::*;
use sk1ll_daemon::Joined;
use std::path::{Path, PathBuf};
use tauri::State;

#[derive(Debug, Serialize)]
pub struct CommandError {
    pub message: String,
    pub status: Option<u16>,
    pub unreachable: bool,
}

impl From<ClientError> for CommandError {
    fn from(error: ClientError) -> Self {
        Self { unreachable: error.is_unreachable(), status: error.status, message: error.message }
    }
}

type Result<T> = std::result::Result<T, CommandError>;

pub struct Bridge {
    client: Client,
}

impl Bridge {
    pub fn new(client: Client) -> Self {
        Self { client }
    }
}

// Both the IPC handler and the tests call exactly the same forwarding method.
macro_rules! command {
    ($name:ident ($($arg:ident: $ty:ty),*) -> $out:ty, $client:ident ($($value:expr),*)) => {
        impl Bridge {
            pub async fn $name(&self, $($arg: $ty),*) -> Result<$out> {
                let result = self.client.$client($($value),*).await.map_err(Into::into);
                if std::env::var_os("SK1LL_GUI_TRACE").is_some() {
                    eprintln!("sk1ll-gui: command {} {}", stringify!($name), if result.is_ok() { "ok" } else { "failed" });
                }
                result
            }
        }
        #[tauri::command]
        pub async fn $name(bridge: State<'_, Bridge>, $($arg: $ty),*) -> Result<$out> {
            bridge.$name($($arg),*).await
        }
    };
}

command!(status() -> StatusView, status());
command!(fleet() -> FleetView, fleet());
command!(items() -> Vec<ItemView>, items());
command!(diff(item: String) -> DiffView, diff(&item));
command!(promote_message(items: Vec<String>) -> PromoteMessage, promote_message(&items));
command!(promote(request: PromoteRequest) -> PromoteResponse, promote(&request));
command!(reset(request: ResetRequest) -> ResetResponse, reset(&request));
command!(pull() -> PullResult, pull());
command!(conflicts() -> Vec<ConflictView>, conflicts());
command!(recovered() -> Vec<ConflictView>, recovered());
command!(adopt(target: ConflictTarget) -> (), adopt(&target));
command!(overwrite(target: ConflictTarget) -> (), overwrite(&target));
command!(adopt_displaced(id: String) -> (), adopt_displaced(&id));
command!(discard_displaced(id: String) -> (), discard_displaced(&id));
command!(harnesses() -> Vec<HarnessView>, harnesses());
command!(resolve_import(request: ResolveImportRequest) -> ResolvedSource, resolve_import(&request));
command!(add_import(request: AddImportRequest) -> ImportUpdated, add_import(&request));
command!(imports() -> ImportsView, imports());
command!(update_import(name: String) -> ImportUpdated, update_import(&name));
command!(assignment(machine: String) -> AssignmentSnapshot, assignment(&machine));
command!(set_assignment(machine: String, changes: AssignmentChanges) -> AssignmentUpdate,
    set_assignment(&machine, &changes));
command!(log(query: LogQuery) -> Vec<LogEntry>, log(&query));
command!(undo(entry: i64) -> AssignmentUpdate, undo(entry));
command!(join(request: JoinApiRequest) -> Joined, join(&request));
command!(join_code() -> JoinCode, join_code());
command!(remove_machine(machine: String) -> (), remove_machine(&machine));

#[derive(Debug, Serialize)]
pub struct ReviewDraftEntry {
    #[serde(flatten)]
    pub entry: DraftEntry,
    pub fingerprint: String,
}

/// Textual diffs do not distinguish successive edits to binary files or symlinks.
pub(crate) fn draft_fingerprint(repo: &Path, paths: &[PathBuf]) -> std::io::Result<String> {
    let mut hash = Sha256::new();
    let mut paths = paths.to_vec();
    paths.sort();
    for relative in paths {
        let path = repo.join(&relative);
        let bytes = relative.as_os_str().as_encoded_bytes();
        hash.update(bytes.len().to_le_bytes());
        hash.update(bytes);
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_symlink() => {
                hash.update(b"symlink");
                let target = std::fs::read_link(&path)?;
                let bytes = target.as_os_str().as_encoded_bytes();
                hash.update(bytes.len().to_le_bytes());
                hash.update(bytes);
            }
            Ok(_) => {
                hash.update(b"file");
                let bytes = std::fs::read(&path)?;
                hash.update(bytes.len().to_le_bytes());
                hash.update(bytes);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => hash.update(b"missing"),
            Err(error) => return Err(error),
        }
    }
    Ok(format!("{:x}", hash.finalize()))
}

impl Bridge {
    pub async fn draft(&self) -> Result<Vec<ReviewDraftEntry>> {
        let repo = self.client.status().await?.repo;
        let entries = self.client.draft().await?;
        tokio::task::spawn_blocking(move || entries.into_iter().map(|entry| {
            let fingerprint = draft_fingerprint(&repo, &entry.paths).map_err(|error| CommandError {
                message: format!("cannot review {}: {error}", entry.item), status: None, unreachable: false,
            })?;
            Ok(ReviewDraftEntry { entry, fingerprint })
        }).collect()).await.map_err(|error| CommandError {
            message: format!("cannot review Draft: {error}"), status: None, unreachable: false,
        })?
    }
}

#[tauri::command]
pub async fn draft(bridge: State<'_, Bridge>) -> Result<Vec<ReviewDraftEntry>> {
    bridge.draft().await
}

/// Local connection metadata only; it contains no credential.
#[tauri::command]
pub fn connection_info(bridge: State<'_, Bridge>) -> String {
    bridge.client.socket().display().to_string()
}
