//! Git configuration passed through the environment (`GIT_CONFIG_COUNT`,
//! `GIT_CONFIG_KEY_n`, `GIT_CONFIG_VALUE_n`), as the daemon gives git the
//! machine's credentials and keeps its readers from writing the index.

/// Adds `key = value` to the env-scoped config that `get` reads.
pub fn merge_config_env(get: impl Fn(&str) -> Option<String>, key: &str, value: &str) -> Vec<(String, String)> {
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
