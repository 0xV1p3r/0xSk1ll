//! How git authenticates to the server: the environment each git process
//! gets, never a credential in a URL or `.git/config`.

use sk1ll_daemon::DaemonState;
use sk1ll_daemon::auth::git_credentials_env;
use sk1ll_testkit::child;

const CASE: &str = "SK1LL_TEST_AUTH_CASE";
const KEY: &str = "http.http://127.0.0.1:9/.extraHeader";
/// `ws:s3cret` in base64.
const HEADER: &str = "Authorization: Basic d3M6czNjcmV0";

fn pairs(env: &[(&str, &str)]) -> Vec<(String, String)> {
    env.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

/// The token reaches git as a Basic `Authorization` header scoped to the
/// server's URL, after the `GIT_CONFIG_*` entries the user already has; an
/// entry of our own from an earlier start is replaced, not added again.
#[test]
fn git_gets_the_token_as_a_header_for_the_server_beside_the_users_own_config() {
    if child::dir().is_some() {
        let state = DaemonState::new("ws".into(), "http://127.0.0.1:9/".into(), "s3cret".into(), "/r".into());
        let env = git_credentials_env(&state);
        let expected = match std::env::var(CASE).unwrap().as_str() {
            "none" => pairs(&[("GIT_CONFIG_COUNT", "1"), ("GIT_CONFIG_KEY_0", KEY), ("GIT_CONFIG_VALUE_0", HEADER)]),
            "user's" => pairs(&[("GIT_CONFIG_COUNT", "2"), ("GIT_CONFIG_KEY_1", KEY), ("GIT_CONFIG_VALUE_1", HEADER)]),
            "ours" => pairs(&[("GIT_CONFIG_VALUE_0", HEADER)]),
            other => panic!("unknown case {other}"),
        };
        assert_eq!(env, expected);
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    for (case, existing) in [("none", None), ("user's", Some("core.pager")), ("ours", Some(KEY))] {
        let mut command = child::command(
            "auth::git_gets_the_token_as_a_header_for_the_server_beside_the_users_own_config",
            dir.path(),
        );
        command.env(CASE, case);
        for var in ["GIT_CONFIG_COUNT", "GIT_CONFIG_KEY_0", "GIT_CONFIG_VALUE_0", "GIT_CONFIG_KEY_1"] {
            command.env_remove(var);
        }
        if let Some(key) = existing {
            command.env("GIT_CONFIG_COUNT", "1").env("GIT_CONFIG_KEY_0", key).env("GIT_CONFIG_VALUE_0", "old");
        }
        let output = command.output().unwrap();
        assert!(output.status.success(), "{case}: {}", sk1ll_testkit::text(&output));
    }
}

/// Joining warns that the token travels in clear text over plain HTTP,
/// except to this host.
#[test]
fn only_plain_http_to_another_host_sends_the_token_in_clear_text() {
    use sk1ll_daemon::join::insecure_http;
    for url in
        ["http://localhost", "http://LOCALHOST:8080", "http://127.42.0.1", "http://[::1]:9", "https://example.com"]
    {
        assert!(!insecure_http(url), "{url}");
    }
    for url in ["http://example.com", " http://10.0.0.2:8080/ "] {
        assert!(insecure_http(url), "{url}");
    }
}
