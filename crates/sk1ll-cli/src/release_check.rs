//! A notice that a newer sk1ll release exists, for `sk1ll status`. It only says so: the app
//! updates itself, and a tarball install is replaced by hand.
//!
//! It asks GitHub at most once a day, and only when standard output is a terminal, so scripts
//! and tests never reach the network. `SK1LL_NO_UPDATE_CHECK=1` turns it off. The answer is
//! cached in the cache directory; any failure just means no notice. `SK1LL_RELEASE_URL`
//! replaces GitHub's latest-release API URL: tests point it at a stub.
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;

const LATEST: &str = "https://api.github.com/repos/0xV1p3r/0xSk1ll/releases/latest";
const RELEASES: &str = "https://github.com/0xV1p3r/0xSk1ll/releases/latest";
const EVERY: Duration = Duration::from_secs(24 * 60 * 60);
const TIMEOUT: Duration = Duration::from_secs(3);

/// The line to print when a release newer than this build is out.
pub(crate) fn notice() -> Option<String> {
    use std::io::IsTerminal;
    if !std::io::stdout().is_terminal() || std::env::var_os("SK1LL_NO_UPDATE_CHECK").is_some_and(|v| !v.is_empty()) {
        return None;
    }
    let cache = sk1ll_core::paths::cache_dir().ok()?.join("latest-release.json");
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
    let latest = match cached(&cache, now) {
        Some(version) => version,
        None => {
            let version = fetch()?;
            let entry = serde_json::json!({ "checked_at": now, "version": version });
            if let Some(dir) = cache.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = std::fs::write(&cache, entry.to_string());
            version
        }
    };
    newer(&latest, env!("CARGO_PKG_VERSION"))
}

/// The cached release, unless the cache is a day old (or from the future: a changed clock).
fn cached(path: &Path, now: u64) -> Option<String> {
    let entry: Value = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    let checked = entry.get("checked_at")?.as_u64()?;
    (checked <= now && now - checked < EVERY.as_secs()).then(|| entry.get("version")?.as_str().map(str::to_string))?
}

fn fetch() -> Option<String> {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().ok()?;
    rt.block_on(async {
        let client = reqwest::Client::builder().timeout(TIMEOUT).user_agent("sk1ll-cli").build().ok()?;
        let release: Value = client
            .get(std::env::var("SK1LL_RELEASE_URL").ok().filter(|url| !url.is_empty()).as_deref().unwrap_or(LATEST))
            .header("Accept", "application/vnd.github+json")
            .send()
            .await
            .ok()?
            .error_for_status()
            .ok()?
            .json()
            .await
            .ok()?;
        release_version(&release).map(str::to_owned)
    })
}

fn newer(latest: &str, ours: &str) -> Option<String> {
    matches!(crate::release_order(latest, ours), Some(std::cmp::Ordering::Greater))
        .then(|| format!("sk1ll {latest} is out (this is {ours}): {RELEASES}"))
}

fn release_version(release: &Value) -> Option<&str> {
    Some(release.get("tag_name")?.as_str()?.trim_start_matches('v'))
}
