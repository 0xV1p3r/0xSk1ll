//! Commands use the Daemon's response types and never retry writes.
//! Draft entries also include byte fingerprints for confirmation review.
use serde::Serialize;
use sha2::{Digest, Sha256};
use sk1ll_daemon::Joined;
use sk1ll_daemon::client::*;
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
command!(skills() -> Vec<ContentEntry>, skills());
command!(skill_tree(query: ContentQuery) -> Vec<FileEntry>, skill_tree(&query));
command!(skill_file(query: FileQuery) -> FileContent, skill_file(&query));
command!(adopt_unmanaged(target: ConflictTarget) -> (), adopt_unmanaged(&target));
command!(fork(name: String) -> (), fork(&name));
command!(diff(item: String) -> DiffView, diff(&item));
command!(commit_message(items: Vec<String>) -> CommitMessage, commit_message(&items));
command!(commit(request: CommitRequest) -> CommitResponse, commit(&request));
command!(restore(request: RestoreRequest) -> RestoreResponse, restore(&request));
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
command!(
    set_assignment(machine: String, changes: AssignmentChanges) -> AssignmentUpdate,
    set_assignment(&machine, &changes)
);
command!(log(query: LogQuery) -> Vec<LogEntry>, log(&query));
command!(log_facets() -> LogFacets, log_facets());
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
        tokio::task::spawn_blocking(move || {
            entries
                .into_iter()
                .map(|entry| {
                    let fingerprint = draft_fingerprint(&repo, &entry.paths).map_err(|error| CommandError {
                        message: format!("cannot review {}: {error}", entry.item),
                        status: None,
                        unreachable: false,
                    })?;
                    Ok(ReviewDraftEntry { entry, fingerprint })
                })
                .collect()
        })
        .await
        .map_err(|error| CommandError {
            message: format!("cannot review Draft: {error}"),
            status: None,
            unreachable: false,
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

fn editor_error(message: impl Into<String>) -> CommandError {
    CommandError { message: message.into(), status: None, unreachable: false }
}

impl Bridge {
    /// Resolve the editor destination from the same inventory used by Skills.
    pub(crate) async fn editor_target(&self, item: &str) -> Result<crate::editors::Target> {
        let (kind, name) = item.split_once(':').ok_or_else(|| editor_error("Expected skill:<name> or rule:<name>"))?;
        if name.is_empty()
            || name.contains(['/', '\0'])
            || (cfg!(windows) && name.contains('\\'))
            || matches!(name, "." | "..")
        {
            return Err(editor_error("Invalid item name"));
        }
        if !matches!(kind, "skill" | "rule") {
            return Err(editor_error("Adopt this Unmanaged skill before opening it"));
        }
        let entry = self
            .client
            .skills()
            .await?
            .into_iter()
            .find(|entry| entry.item.as_ref().is_some_and(|reference| reference.to_string() == item))
            .ok_or_else(|| editor_error("Item is no longer in Skills"))?;
        match entry.kind {
            ContentType::Import => return Err(editor_error("Fork this Import before opening it in an editor")),
            ContentType::Unmanaged => return Err(editor_error("Adopt this Unmanaged skill before opening it")),
            _ => {}
        }
        let path = entry.path.ok_or_else(|| editor_error("Item has no repo path"))?;
        let repo = self.client.status().await?.repo;
        crate::editors::validate_target(&repo, &path, entry.kind == ContentType::Rule).map_err(editor_error)
    }
}

fn preference_path<R: tauri::Runtime>(app: &tauri::AppHandle<R>) -> Result<PathBuf> {
    use tauri::Manager;
    Ok(app.path().app_config_dir().map_err(|e| editor_error(e.to_string()))?.join("editors.json"))
}

#[tauri::command]
pub fn editors(
    app: tauri::AppHandle,
    cache: State<'_, crate::editors::EditorCache>,
    refresh: Option<bool>,
) -> Result<crate::editors::EditorsView> {
    let mut guard = cache.0.lock().map_err(|_| editor_error("Editor cache unavailable"))?;
    if refresh.unwrap_or(false) || guard.is_none() {
        *guard = Some(crate::editors::Discovery::environment().detect());
    }
    let editors = guard.as_ref().unwrap();
    let preference = std::fs::read(preference_path(&app)?)
        .ok()
        .and_then(|b| serde_json::from_slice::<crate::editors::Preferences>(&b).ok())
        .and_then(|p| p.preferred_editor);
    let preferred_editor =
        preference.filter(|id| editors.iter().any(|e| &e.view.id == id)).unwrap_or_else(|| editors[0].view.id.clone());
    Ok(crate::editors::EditorsView { editors: editors.iter().map(|e| e.view.clone()).collect(), preferred_editor })
}

#[tauri::command]
pub async fn open_in_editor(
    app: tauri::AppHandle,
    bridge: State<'_, Bridge>,
    cache: State<'_, crate::editors::EditorCache>,
    editor_id: String,
    item: String,
) -> Result<()> {
    let editor = {
        let mut guard = cache.0.lock().map_err(|_| editor_error("Editor cache unavailable"))?;
        let editors = guard.get_or_insert_with(|| crate::editors::Discovery::environment().detect());
        editors
            .iter()
            .find(|e| e.view.id == editor_id)
            .cloned()
            .ok_or_else(|| editor_error("Editor is not available; refresh the editor list"))?
    };
    let target = bridge.editor_target(&item).await?;
    let path = preference_path(&app)?;
    std::fs::create_dir_all(path.parent().unwrap()).map_err(|e| editor_error(e.to_string()))?;
    crate::editors::launch(&editor, &target).map_err(editor_error)?;
    let bytes = serde_json::to_vec(&crate::editors::Preferences { preferred_editor: Some(editor_id) })
        .map_err(|e| editor_error(e.to_string()))?;
    std::fs::write(path, bytes)
        .map_err(|e| editor_error(format!("Editor opened, but preference could not be saved: {e}")))?;
    Ok(())
}

command!(push() -> PushResult, push());
command!(create_item(request: CreateItemRequest) -> (), create_item(&request));
command!(delete_item(request: DeleteItemRequest) -> (), delete_item(&request));
command!(rename_item(request: RenameItemRequest) -> RenameItemResponse, rename_item(&request));
command!(check_imports() -> ImportsView, check_imports());
command!(remove_import(name: String) -> (), remove_import(&name));
command!(conflict_diff(request: ConflictDiffRequest) -> DiffView, conflict_diff(&request));

#[tauri::command]
pub fn set_preferred_editor(
    app: tauri::AppHandle,
    cache: State<'_, crate::editors::EditorCache>,
    editor_id: String,
) -> Result<()> {
    let mut guard = cache.0.lock().map_err(|_| editor_error("Editor cache unavailable"))?;
    let editors = guard.get_or_insert_with(|| crate::editors::Discovery::environment().detect());
    if !editors.iter().any(|e| e.view.id == editor_id) {
        return Err(editor_error("Editor is not available; refresh the editor list"));
    }
    let path = preference_path(&app)?;
    std::fs::create_dir_all(path.parent().unwrap()).map_err(|e| editor_error(e.to_string()))?;
    let bytes = serde_json::to_vec(&crate::editors::Preferences { preferred_editor: Some(editor_id) })
        .map_err(|e| editor_error(e.to_string()))?;
    std::fs::write(path, bytes).map_err(|e| editor_error(e.to_string()))
}
