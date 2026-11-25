//! The server's environment format, in a child with its own environment.

use std::time::Duration;

use sk1ll_server::{Config, config::parse_duration};
use sk1ll_testkit::{child, text};

#[test]
fn durations_accept_seconds_minutes_hours_and_days() {
    for (input, seconds) in [("90", 90), ("90s", 90), (" 2m ", 120), ("1h", 3600), ("1d", 86400)] {
        assert_eq!(parse_duration(input).unwrap(), Duration::from_secs(seconds));
    }
    for input in ["0", "5w", "m"] {
        assert!(parse_duration(input).is_err(), "{input}");
    }
}

#[test]
fn the_server_reads_and_validates_its_environment() {
    const TEST: &str = "the_server_reads_and_validates_its_environment";
    if let Some(data) = child::dir() {
        match std::env::var("CHECK_CONFIG").unwrap().as_str() {
            "defaults" => {
                let config = Config::from_env().unwrap();
                assert_eq!(config.bind.to_string(), "0.0.0.0:7878");
                assert_eq!(config.data_dir, std::path::Path::new("/data"));
                assert_eq!(config.admin_token, None);
                assert_eq!(config.import_hosts, None);
                assert!(config.import_checks);
                assert_eq!(config.idle_limit(), Duration::from_secs(120));
            }
            "valid" => {
                let config = Config::from_env().unwrap();
                assert_eq!(config.bind.to_string(), "127.0.0.1:12345");
                assert_eq!(config.data_dir, data);
                assert_eq!(config.admin_token.as_deref(), Some("test-admin"));
                assert_eq!(config.join_code_ttl, Duration::from_secs(86400));
                assert_eq!(config.import_poll, Duration::from_secs(3600));
                assert_eq!(config.push_idle, Duration::from_secs(90));
                assert_eq!(config.push_max, Duration::from_secs(120));
                assert_eq!(config.send_timeout, Duration::from_secs(3));
                assert_eq!(config.offline_after, Duration::from_secs(4));
                assert_eq!(config.idle_limit(), Duration::from_secs(10));
                assert!(!config.import_checks);
                assert_eq!(config.import_hosts.unwrap(), ["github.com", "xn--bcher-kva.example", "[::1]", "10.0.0.1"]);
            }
            "enabled" => assert!(Config::from_env().unwrap().import_checks),
            variable => {
                let error = format!("{:#}", Config::from_env().unwrap_err());
                assert!(error.contains(variable), "{error}");
            }
        }
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let command = |case| {
        let mut cmd = child::command(TEST, dir.path());
        for (key, _) in std::env::vars_os().filter(|(k, _)| k.to_string_lossy().starts_with("SK1LL_SERVER_")) {
            cmd.env_remove(key);
        }
        cmd.env("CHECK_CONFIG", case);
        cmd
    };
    let run = |cmd: &mut std::process::Command| {
        let out = cmd.output().unwrap();
        assert!(out.status.success(), "{}", text(&out));
    };
    run(&mut command("defaults"));
    run(command("valid")
        .env("SK1LL_SERVER_DATA", dir.path())
        .env("SK1LL_SERVER_BIND", "127.0.0.1:12345")
        .env("SK1LL_SERVER_ADMIN_TOKEN", "test-admin")
        .env("SK1LL_SERVER_JOIN_CODE_TTL", "1d")
        .env("SK1LL_SERVER_IMPORT_POLL", "1h")
        .env("SK1LL_SERVER_IMPORT_CHECKS", " off ")
        .env("SK1LL_SERVER_IMPORT_HOSTS", "GitHub.com, bücher.example, [::1], 10.0.0.1,,github.com")
        .env("SK1LL_SERVER_PUSH_IDLE", "90")
        .env("SK1LL_SERVER_PUSH_MAX", "2m")
        .env("SK1LL_SERVER_SEND_TIMEOUT", "3s")
        .env("SK1LL_SERVER_OFFLINE_AFTER", "4s")
        .env("SK1LL_SERVER_PING", "5s"));
    run(command("enabled").env("SK1LL_SERVER_IMPORT_CHECKS", "ON"));
    for (var, value) in [
        ("SK1LL_SERVER_BIND", "missing-port"),
        ("SK1LL_SERVER_IMPORT_CHECKS", "no"),
        ("SK1LL_SERVER_IMPORT_HOSTS", ""),
        ("SK1LL_SERVER_IMPORT_HOSTS", "   "),
        ("SK1LL_SERVER_IMPORT_HOSTS", ","),
        ("SK1LL_SERVER_IMPORT_HOSTS", "github.com:443"),
        ("SK1LL_SERVER_IMPORT_HOSTS", "git@github.com"),
        ("SK1LL_SERVER_IMPORT_HOSTS", "*.github.com"),
        ("SK1LL_SERVER_IMPORT_HOSTS", "github.com/x"),
        ("SK1LL_SERVER_IMPORT_HOSTS", "a b"),
        ("SK1LL_SERVER_IMPORT_HOSTS", "git%68ub.com"),
        ("SK1LL_SERVER_JOIN_CODE_TTL", "0"),
        ("SK1LL_SERVER_OFFLINE_AFTER", "5w"),
        ("SK1LL_SERVER_IMPORT_POLL", "h"),
        ("SK1LL_SERVER_PING", "-1"),
        ("SK1LL_SERVER_SEND_TIMEOUT", "0s"),
        ("SK1LL_SERVER_PUSH_IDLE", "forever"),
        ("SK1LL_SERVER_PUSH_MAX", "15ms"),
    ] {
        run(command(var).env(var, value));
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let var = "SK1LL_SERVER_IMPORT_HOSTS";
        run(command(var).env(var, std::ffi::OsStr::from_bytes(b"git\xffhub.com")));
    }
}
