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

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn state() -> DaemonState {
        DaemonState {
            machine_id: "ws".into(),
            server_url: "http://127.0.0.1:9".into(),
            token: "s3cret".into(),
            repo: "/r".into(),
            revoked: false,
            record: Default::default(),
        }
    }

    #[test]
    fn credentials_are_an_extra_header_for_the_server() {
        let s = state();
        let env = merge_config_env(|_| None, &header_key(&s.server_url), &header_value(&s));
        let expected = base64::engine::general_purpose::STANDARD.encode("ws:s3cret");
        assert_eq!(
            env,
            [
                ("GIT_CONFIG_COUNT".to_string(), "1".to_string()),
                ("GIT_CONFIG_KEY_0".to_string(), "http.http://127.0.0.1:9/.extraHeader".to_string()),
                ("GIT_CONFIG_VALUE_0".to_string(), format!("Authorization: Basic {expected}")),
            ]
        );
    }

    #[test]
    fn existing_env_config_is_kept() {
        let s = state();
        let key = header_key(&s.server_url);
        let user: HashMap<&str, &str> =
            [("GIT_CONFIG_COUNT", "1"), ("GIT_CONFIG_KEY_0", "core.pager"), ("GIT_CONFIG_VALUE_0", "cat")].into();
        let env = merge_config_env(|k| user.get(k).map(|v| v.to_string()), &key, "v");
        assert_eq!(env[0], ("GIT_CONFIG_COUNT".into(), "2".into()));
        assert_eq!(env[1], ("GIT_CONFIG_KEY_1".into(), key.clone()));
        // Our own entry from an earlier start is replaced, not duplicated.
        let ours: HashMap<&str, &str> =
            [("GIT_CONFIG_COUNT", "1"), ("GIT_CONFIG_KEY_0", key.as_str()), ("GIT_CONFIG_VALUE_0", "old")].into();
        let env = merge_config_env(|k| ours.get(k).map(|v| v.to_string()), &key, "new");
        assert_eq!(env, [("GIT_CONFIG_VALUE_0".to_string(), "new".to_string())]);
    }
}
