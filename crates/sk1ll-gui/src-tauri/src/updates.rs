//! Updates of the app itself from the GitHub releases. The updater plugin checks
//! `latest.json` (see `tauri.release.conf.json`) and verifies every download against the
//! public key there; only the release `.deb` carries that config, so other builds never
//! check. The webview gets no updater permission: these commands are the whole surface.
//!
//! Installing replaces the bundled `sk1ll` too, so after the package is in place the
//! Daemon's user service is restarted into it before the app relaunches.
use std::sync::Mutex;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, Runtime};
use tauri_plugin_updater::{Update, UpdaterExt};

/// The plugin has no timeouts of its own: a stalled connection would block retries until restart.
const CHECK_TIMEOUT: Duration = Duration::from_secs(30);
/// For the whole download, so a slow connection still gets there.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// The event that carries [`Progress`] while an update installs.
pub const PROGRESS_EVENT: &str = "app-update-progress";

/// The release a check found, as the app shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AvailableUpdate {
    pub version: String,
    pub current_version: String,
    /// The release notes: this version's CHANGELOG section.
    pub notes: Option<String>,
    /// RFC 3339.
    pub date: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "stage", rename_all = "snake_case")]
pub enum Progress {
    /// `percent`: `None` while the size is unknown.
    Downloading {
        percent: Option<u8>,
    },
    Installing,
    RestartingDaemon,
    Relaunching,
}

/// The update the last check found, kept for `install_update`.
#[derive(Default)]
pub struct Found(Mutex<Option<Update>>);

/// Registers the updater plugin when this build carries its config (release builds).
pub fn register<R: Runtime>(app: &AppHandle<R>) -> tauri::Result<()> {
    if configured(app) {
        app.plugin(tauri_plugin_updater::Builder::new().build())?;
    }
    Ok(())
}

fn configured<R: Runtime>(app: &AppHandle<R>) -> bool {
    app.config().plugins.0.contains_key("updater")
}

/// This build can update itself: it was configured for the updater, and installed as the
/// package the releases carry.
pub fn available<R: Runtime>(app: &AppHandle<R>) -> bool {
    configured(app) && matches!(tauri::utils::platform::bundle_type(), Some(tauri::utils::config::BundleType::Deb))
}

pub async fn check<R: Runtime>(app: &AppHandle<R>) -> Result<Option<AvailableUpdate>, String> {
    if !available(app) {
        return Err("This build of the app can't update itself. Install the release .deb to get updates.".into());
    }
    let updater = app.updater_builder().timeout(CHECK_TIMEOUT).build().map_err(|e| e.to_string())?;
    let update = updater.check().await.map_err(|e| format!("Checking for updates failed: {e}"))?;
    let view = update.as_ref().map(|u| AvailableUpdate {
        version: u.version.clone(),
        current_version: u.current_version.clone(),
        notes: u.body.clone(),
        date: u.raw_json.get("pub_date").and_then(|d| d.as_str()).map(str::to_string),
    });
    *app.state::<Found>().0.lock().unwrap_or_else(|e| e.into_inner()) = update;
    Ok(view)
}

/// Downloads and installs the update the last check found (the package manager asks for
/// the password), restarts the Daemon into it, then relaunches the app. A Daemon that
/// can't be restarted here (not the user service) keeps its release; the relaunched app
/// says so and offers the restart.
pub async fn install<R: Runtime>(app: &AppHandle<R>, version: &str) -> Result<(), String> {
    let found = app.state::<Found>().0.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let update = found
        .filter(|u| u.version == version)
        .ok_or_else(|| format!("Check for updates again: {version} is no longer the update on offer."))?;
    let emit = |progress: Progress| {
        let _ = app.emit(PROGRESS_EVENT, progress);
    };
    let (mut received, mut last) = (0u64, None);
    emit(Progress::Downloading { percent: None });
    let bytes = tokio::time::timeout(
        DOWNLOAD_TIMEOUT,
        update.download(
            |chunk, total| {
                received += chunk as u64;
                let percent = total.filter(|t| *t > 0).map(|t| (received.min(t) * 100 / t) as u8);
                if percent != last {
                    last = percent;
                    emit(Progress::Downloading { percent });
                }
            },
            || {},
        ),
    )
    .await
    .map_err(|_| "The download timed out. Try again.".to_string())?
    .map_err(|e| format!("Downloading the update failed: {e}"))?;
    emit(Progress::Installing);
    // dpkg through pkexec blocks until the password prompt is answered.
    let install = update.clone();
    tauri::async_runtime::spawn_blocking(move || install.install(bytes))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("Installing the update failed: {e}"))?;
    emit(Progress::RestartingDaemon);
    match tauri::async_runtime::spawn_blocking(crate::service::restart).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => eprintln!("sk1ll-gui: the Daemon keeps its release until restarted: {e}"),
        Err(e) => eprintln!("sk1ll-gui: restarting the Daemon failed: {e}"),
    }
    emit(Progress::Relaunching);
    app.restart()
}
