//! How git authenticates to the server without the token ever being written to a
//! remote URL or `.git/config`: the token stays in the 0600 state file, and
//! each git process gets it from environment-scoped config
//! (`GIT_CONFIG_COUNT` / `GIT_CONFIG_KEY_n` / `GIT_CONFIG_VALUE_n`) as an
//! `http.<server-url>/.extraHeader` carrying `Authorization: Basic`.
//!
//! The daemon puts this in its own process environment once, before any
//! thread starts ([`crate::prepare_process`]), so every git it or core runs
//! inherits it. The CLI does the same for its direct mode.

use base64::Engine as _;
use sk1ll_core::git::config_env::merge_config_env;
pub use sk1ll_core::redact::redact;

use crate::state::DaemonState;

/// Environment that lets git authenticate to `state`'s server, merged with any
/// `GIT_CONFIG_*` entries already in this process's environment (they are
/// kept; an earlier entry for the same key is replaced). Apply it to a
/// `Command` with `envs`, or to the process before any thread starts.
pub fn git_credentials_env(state: &DaemonState) -> Vec<(String, String)> {
    merge_config_env(|k| std::env::var(k).ok(), &header_key(&state.server_url), &header_value(state))
}

/// Environment that keeps git's read commands from writing the index: no
/// opportunistic refresh by `git status` (`GIT_OPTIONAL_LOCKS`) nor by
/// `git diff` (`diff.autoRefreshIndex`). The daemon's readers run git without
/// the engine mutex, and a reader holding `index.lock` for that refresh
/// would make a commit or pull running at that moment fail. Merged like
/// [`git_credentials_env`]; apply it to the process before any thread starts.
pub fn git_read_only_env() -> Vec<(String, String)> {
    let mut env = merge_config_env(|k| std::env::var(k).ok(), "diff.autoRefreshIndex", "false");
    env.push(("GIT_OPTIONAL_LOCKS".to_string(), "0".to_string()));
    env
}

fn header_key(server_url: &str) -> String {
    format!("http.{}/.extraHeader", server_url.trim_end_matches('/'))
}

fn header_value(state: &DaemonState) -> String {
    let basic = base64::engine::general_purpose::STANDARD.encode(format!("{}:{}", state.machine_id, state.token));
    format!("Authorization: Basic {basic}")
}
