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

use crate::state::DaemonState;

/// Environment that lets git authenticate to `state`'s server, merged with any
/// `GIT_CONFIG_*` entries already in this process's environment (they are
/// kept; an earlier entry for the same key is replaced). Apply it to a
/// `Command` with `envs`, or to the process before any thread starts.
pub fn git_credentials_env(state: &DaemonState) -> Vec<(String, String)> {
    merge_config_env(|k| std::env::var(k).ok(), &header_key(&state.server_url), &header_value(state))
}

fn header_key(server_url: &str) -> String {
    format!("http.{}/.extraHeader", server_url.trim_end_matches('/'))
}

fn header_value(state: &DaemonState) -> String {
    let basic = base64::engine::general_purpose::STANDARD.encode(format!("{}:{}", state.machine_id, state.token));
    format!("Authorization: Basic {basic}")
}

/// Adds `key = value` to the env-scoped config that `get` reads.
fn merge_config_env(get: impl Fn(&str) -> Option<String>, key: &str, value: &str) -> Vec<(String, String)> {
    let count: usize = get("GIT_CONFIG_COUNT").and_then(|v| v.trim().parse().ok()).unwrap_or(0);
    let is_ours = |i: &usize| get(&format!("GIT_CONFIG_KEY_{i}")).is_some_and(|k| k.eq_ignore_ascii_case(key));
    let existing = (0..count).find(is_ours);
    match existing {
        Some(i) => vec![(format!("GIT_CONFIG_VALUE_{i}"), value.to_string())],
        None => vec![
            ("GIT_CONFIG_COUNT".to_string(), (count + 1).to_string()),
            (format!("GIT_CONFIG_KEY_{count}"), key.to_string()),
            (format!("GIT_CONFIG_VALUE_{count}"), value.to_string()),
        ],
    }
}

/// `text` with URL userinfo (`scheme://user:pass@`) and `Basic`/`Bearer`
/// credentials replaced by `***`, for logs and errors.
pub fn redact(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find("://") {
        let (head, tail) = rest.split_at(i + 3);
        out.push_str(head);
        let end = tail
            .find(|c: char| c == '/' || c.is_whitespace() || matches!(c, '\'' | '"' | '`' | '>' | ')'))
            .unwrap_or(tail.len());
        let authority = &tail[..end];
        match authority.rfind('@') {
            Some(at) => {
                out.push_str("***@");
                out.push_str(&authority[at + 1..]);
            }
            None => out.push_str(authority),
        }
        rest = &tail[end..];
    }
    out.push_str(rest);
    for scheme in ["Basic ", "Bearer ", "basic ", "bearer "] {
        out = redact_after(&out, scheme);
    }
    out
}

fn redact_after(text: &str, marker: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find(marker) {
        let (head, tail) = rest.split_at(i + marker.len());
        out.push_str(head);
        let end = tail.find(|c: char| c.is_whitespace() || matches!(c, '\'' | '"')).unwrap_or(tail.len());
        if end > 0 {
            out.push_str("***");
        }
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
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

    #[test]
    fn redaction() {
        assert_eq!(
            redact("`git clone http://ws:tok@h:1/git/repo.git x` failed: 'https://a@b/c'"),
            "`git clone http://***@h:1/git/repo.git x` failed: 'https://***@b/c'"
        );
        assert_eq!(redact("http://h/git/repo.git"), "http://h/git/repo.git");
        assert_eq!(redact("Authorization: Basic d3M6dG9r here"), "Authorization: Basic *** here");
        assert_eq!(redact("Bearer abc"), "Bearer ***");
    }
}
