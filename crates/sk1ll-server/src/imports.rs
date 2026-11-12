//! Periodic check of the baseline's imports against their upstream refs.

use std::sync::Arc;

use sk1ll_core::lock::LOCK_FILE;
use sk1ll_core::manifest::MANIFEST_FILE;
use sk1ll_core::source::resolve_url;
use sk1ll_core::{Lockfile, Manifest};
use sk1ll_proto::{ServerMessage, ImportError, ImportUpdate, ImportsView};

use crate::{Server, git, lock, now};

/// Checks now, then every `import_poll` and after each push.
pub async fn poll(server: Arc<Server>) {
    loop {
        let view = check(&server).await;
        let changed = {
            let mut current = lock(&server.imports);
            let changed = current.updates != view.updates;
            *current = view.clone();
            changed
        };
        if changed {
            tracing::info!("{} import(s) have upstream updates", view.updates.len());
            server.broadcast(&ServerMessage::ImportsChanged { updates: view.updates });
        }
        tokio::select! {
            () = tokio::time::sleep(server.config.import_poll) => {}
            () = server.imports_due.notified() => {}
        }
    }
}

async fn check(server: &Server) -> ImportsView {
    let mut view = ImportsView::default();
    if let Some(head) = server.head()
        && let Err(e) = check_at(server, &head, &mut view).await
    {
        view.errors.push(ImportError { name: MANIFEST_FILE.into(), message: format!("{e:#}") });
    }
    view.updates.sort();
    view.checked_at = Some(now());
    view
}

async fn check_at(server: &Server, head: &str, view: &mut ImportsView) -> anyhow::Result<()> {
    let repo = server.config.repo_dir();
    let Some(manifest) = git::show(&repo, head, MANIFEST_FILE).await? else {
        return Ok(());
    };
    let manifest: Manifest = toml::from_str(&manifest)?;
    let lockfile: Lockfile = match git::show(&repo, head, LOCK_FILE).await? {
        Some(text) => toml::from_str(&text)?,
        None => Lockfile::default(),
    };
    for (name, import) in manifest.import {
        // Not vendored yet, or pinned to a commit: nothing to compare.
        let Some(locked) = lockfile.skills.get(&name) else {
            continue;
        };
        if is_commit(&import.git_ref) {
            continue;
        }
        match git::ls_remote(&resolve_url(&import.source), &import.git_ref, &server.config.import_protocols).await {
            Ok(Some(upstream)) if upstream != locked.commit => view.updates.push(ImportUpdate {
                name,
                source: import.source,
                git_ref: import.git_ref,
                locked: locked.commit.clone(),
                upstream,
            }),
            Ok(Some(_)) => {}
            Ok(None) => view.errors.push(ImportError { name, message: format!("ref `{}` not found", import.git_ref) }),
            Err(e) => view.errors.push(ImportError { name, message: format!("{e:#}") }),
        }
    }
    Ok(())
}

fn is_commit(git_ref: &str) -> bool {
    git_ref.len() == 40 && git_ref.chars().all(|c| c.is_ascii_hexdigit())
}
