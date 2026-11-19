use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, bail};

/// Transports the import check may use (git's `GIT_ALLOW_PROTOCOL` format).
pub const IMPORT_PROTOCOLS: &str = "https:http:ssh:git";

/// Server settings, from `SK1LL_SERVER_*` environment variables.
#[derive(Debug, Clone)]
pub struct Config {
    /// Holds `repo.git` and `server.db` (`SK1LL_SERVER_DATA`, default `/data`).
    pub data_dir: PathBuf,
    /// `SK1LL_SERVER_BIND`, default `0.0.0.0:7878`.
    pub bind: SocketAddr,
    /// Bootstrap token that may create join codes and manage the fleet
    /// (`SK1LL_SERVER_ADMIN_TOKEN`).
    pub admin_token: Option<String>,
    /// A machine without a live connection is Offline once it hasn't been
    /// seen for this long (`SK1LL_SERVER_OFFLINE_AFTER`, default `2m`).
    pub offline_after: Duration,
    /// Join code lifetime (SK1LL_SERVER_JOIN_CODE_TTL, default 24h).
    pub join_code_ttl: Duration,
    /// How often to check imports for newer upstream commits
    /// (`SK1LL_SERVER_IMPORT_POLL`, default `1h`).
    pub import_poll: Duration,
    /// Transports the import check may use for upstream sources, which come
    /// from the pushed manifest. Not configurable from the environment.
    pub import_protocols: String,
    /// Whether the server checks imports' upstreams at all
    /// (`SK1LL_SERVER_IMPORT_CHECKS`, `on` or `off`, default `on`).
    pub import_checks: bool,
    /// The only hosts the import check may contact, normalized (lowercase,
    /// IDNs as punycode); `None` for any (`SK1LL_SERVER_IMPORT_HOSTS`,
    /// comma separated, e.g. `github.com,gitlab.com`).
    pub import_hosts: Option<Vec<String>>,
    /// The ssh program the import check runs while `import_hosts` is set,
    /// without the server's ssh config. Not configurable from the environment.
    pub import_ssh: String,
    /// How often the server pings each daemon (`SK1LL_SERVER_PING`, default `30s`).
    /// A connection that sends nothing, pongs included, for
    /// [`Config::idle_limit`] is closed.
    pub ping_every: Duration,
    /// How long one frame to a daemon may take to send before the daemon is
    /// disconnected as too slow (`SK1LL_SERVER_SEND_TIMEOUT`, default `10s`).
    pub send_timeout: Duration,
    /// Maximum inactivity while reading a Git request body or forwarding its
    /// response (`SK1LL_SERVER_PUSH_IDLE`, default `30s`).
    pub push_idle: Duration,
    /// Deadline for the whole Git backend exchange, including upload and
    /// response forwarding (`SK1LL_SERVER_PUSH_MAX`, default `10m`).
    pub push_max: Duration,
}

impl Config {
    /// Defaults with `data_dir`; no admin token.
    pub fn new(data_dir: impl Into<PathBuf>) -> Self {
        Self {
            data_dir: data_dir.into(),
            bind: SocketAddr::from(([0, 0, 0, 0], 7878)),
            admin_token: None,
            offline_after: Duration::from_secs(120),
            join_code_ttl: Duration::from_secs(86400),
            import_poll: Duration::from_secs(3600),
            import_protocols: IMPORT_PROTOCOLS.into(),
            import_checks: true,
            import_hosts: None,
            import_ssh: "ssh".into(),
            ping_every: Duration::from_secs(30),
            send_timeout: Duration::from_secs(10),
            push_idle: Duration::from_secs(30),
            push_max: Duration::from_secs(600),
        }
    }

    pub fn from_env() -> anyhow::Result<Self> {
        let var = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
        let mut cfg = Self::new(var("SK1LL_SERVER_DATA").unwrap_or_else(|| "/data".into()));
        if let Some(bind) = var("SK1LL_SERVER_BIND") {
            cfg.bind = bind.parse().with_context(|| format!("SK1LL_SERVER_BIND: invalid address `{bind}`"))?;
        }
        if let Some(v) = var("SK1LL_SERVER_JOIN_CODE_TTL") {
            cfg.join_code_ttl = parse_duration(&v).with_context(|| format!("SK1LL_SERVER_JOIN_CODE_TTL: `{v}`"))?;
        }
        cfg.admin_token = var("SK1LL_SERVER_ADMIN_TOKEN");
        if let Some(v) = var("SK1LL_SERVER_OFFLINE_AFTER") {
            cfg.offline_after = parse_duration(&v).with_context(|| format!("SK1LL_SERVER_OFFLINE_AFTER: `{v}`"))?;
        }
        if let Some(v) = var("SK1LL_SERVER_IMPORT_POLL") {
            cfg.import_poll = parse_duration(&v).with_context(|| format!("SK1LL_SERVER_IMPORT_POLL: `{v}`"))?;
        }
        if let Some(v) = var("SK1LL_SERVER_IMPORT_CHECKS") {
            cfg.import_checks = parse_switch(&v).with_context(|| format!("SK1LL_SERVER_IMPORT_CHECKS: `{v}`"))?;
        }
        // Unlike the others, set but empty is an error: it would otherwise
        // mean any host, the opposite of what setting it asks for.
        cfg.import_hosts = import_hosts(std::env::var_os("SK1LL_SERVER_IMPORT_HOSTS"))?;
        if let Some(v) = var("SK1LL_SERVER_PING") {
            cfg.ping_every = parse_duration(&v).with_context(|| format!("SK1LL_SERVER_PING: `{v}`"))?;
        }
        if let Some(v) = var("SK1LL_SERVER_SEND_TIMEOUT") {
            cfg.send_timeout = parse_duration(&v).with_context(|| format!("SK1LL_SERVER_SEND_TIMEOUT: `{v}`"))?;
        }
        if let Some(v) = var("SK1LL_SERVER_PUSH_IDLE") {
            cfg.push_idle = parse_duration(&v).with_context(|| format!("SK1LL_SERVER_PUSH_IDLE: `{v}`"))?;
        }
        if let Some(v) = var("SK1LL_SERVER_PUSH_MAX") {
            cfg.push_max = parse_duration(&v).with_context(|| format!("SK1LL_SERVER_PUSH_MAX: `{v}`"))?;
        }
        Ok(cfg)
    }

    /// A connection silent for this long is closed: max(2 × ping, offline-after).
    pub fn idle_limit(&self) -> Duration {
        (self.ping_every * 2).max(self.offline_after)
    }

    pub fn repo_dir(&self) -> PathBuf {
        self.data_dir.join("repo.git")
    }

    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("server.db")
    }
}

/// `on` or `off`, ignoring ASCII case.
fn parse_switch(s: &str) -> anyhow::Result<bool> {
    match s.trim().to_ascii_lowercase().as_str() {
        "on" => Ok(true),
        "off" => Ok(false),
        _ => bail!("use on or off"),
    }
}

/// `SK1LL_SERVER_IMPORT_HOSTS`: `None` only when it is unset. Set, it must
/// name at least one host in UTF-8.
fn import_hosts(value: Option<std::ffi::OsString>) -> anyhow::Result<Option<Vec<String>>> {
    let Some(value) = value else { return Ok(None) };
    let v = value
        .into_string()
        .map_err(|v| anyhow::anyhow!("SK1LL_SERVER_IMPORT_HOSTS: `{}` is not UTF-8", v.display()))?;
    Ok(Some(parse_hosts(&v).with_context(|| format!("SK1LL_SERVER_IMPORT_HOSTS: `{v}`"))?))
}

/// Comma-separated host names or IP addresses, normalized like the hosts
/// they are compared with. Ports, userinfo, paths and wildcards are refused.
pub fn parse_hosts(s: &str) -> anyhow::Result<Vec<String>> {
    let mut hosts = Vec::new();
    for entry in s.split(',').map(str::trim).filter(|e| !e.is_empty()) {
        let host = crate::imports::normalize_host(entry).map_err(anyhow::Error::msg)?;
        if !hosts.contains(&host) {
            hosts.push(host);
        }
    }
    if hosts.is_empty() {
        bail!(
            "names no host; leave it unset to allow any host, or turn checks off with SK1LL_SERVER_IMPORT_CHECKS=off"
        );
    }
    Ok(hosts)
}

/// `90`, `90s`, `2m`, `1h` or `1d`; a bare number is seconds.
pub fn parse_duration(s: &str) -> anyhow::Result<Duration> {
    let s = s.trim();
    let split = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let n: u64 = num.parse().context("expected a number with an optional s/m/h/d unit")?;
    let mult = match unit.trim() {
        "" | "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86400,
        u => bail!("unknown unit `{u}`; use s, m, h or d"),
    };
    if n == 0 {
        bail!("must be greater than zero");
    }
    Ok(Duration::from_secs(n * mult))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(parse_duration("90").unwrap(), Duration::from_secs(90));
        assert_eq!(parse_duration("2m").unwrap(), Duration::from_secs(120));
        assert_eq!(parse_duration("1h").unwrap(), Duration::from_secs(3600));
        assert!(parse_duration("0").is_err());
        assert!(parse_duration("5w").is_err());
        assert!(parse_duration("m").is_err());
    }

    #[test]
    fn switches_and_hosts() {
        assert!(parse_switch("ON").unwrap());
        assert!(!parse_switch(" off ").unwrap());
        assert!(parse_switch("no").is_err());
        assert_eq!(parse_hosts("GitHub.com, gitlab.com,,github.com").unwrap(), ["github.com", "gitlab.com"]);
        assert_eq!(parse_hosts("bücher.example").unwrap(), ["xn--bcher-kva.example"]);
        assert_eq!(parse_hosts("[::1],10.0.0.1").unwrap(), ["[::1]", "10.0.0.1"]);
        assert_eq!(import_hosts(None).unwrap(), None);
        assert_eq!(import_hosts(Some("github.com".into())).unwrap(), Some(vec!["github.com".into()]));
        for set in ["", "   ", ","] {
            let err = format!("{:#}", import_hosts(Some(set.into())).unwrap_err());
            assert!(err.contains("names no host"), "{set:?}: {err}");
        }
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            let err = import_hosts(Some(std::ffi::OsString::from_vec(b"git\xffhub.com".to_vec()))).unwrap_err();
            assert!(err.to_string().contains("not UTF-8"), "{err}");
        }
        for bad in [" , ", "github.com:443", "git@github.com", "*.github.com", "github.com/x", "a b", "git%68ub.com"] {
            assert!(parse_hosts(bad).is_err(), "{bad}");
        }
    }
}
