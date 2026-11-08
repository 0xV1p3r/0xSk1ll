use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, bail};

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
    /// How often to check imports for newer upstream commits
    /// (`SK1LL_SERVER_IMPORT_POLL`, default `1h`).
    pub import_poll: Duration,
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
            import_poll: Duration::from_secs(3600),
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
        cfg.admin_token = var("SK1LL_SERVER_ADMIN_TOKEN");
        if let Some(v) = var("SK1LL_SERVER_OFFLINE_AFTER") {
            cfg.offline_after = parse_duration(&v).with_context(|| format!("SK1LL_SERVER_OFFLINE_AFTER: `{v}`"))?;
        }
        if let Some(v) = var("SK1LL_SERVER_IMPORT_POLL") {
            cfg.import_poll = parse_duration(&v).with_context(|| format!("SK1LL_SERVER_IMPORT_POLL: `{v}`"))?;
        }
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
}
